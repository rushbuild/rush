// Rush Build - CustomRule placeholder
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Graph-only handling for Starlark-defined custom rules.
//!
//! TF `.bzl` files use `rule(implementation = _impl)` to define custom
//! build rules. Rush's Starlark evaluator currently doesn't evaluate
//! the `implementation` function (it needs a `ctx` API surface with
//! `ctx.actions.run_shell`, `ctx.actions.declare_file`, etc.). Until
//! that lands, we register the target in the dep graph so consumer
//! edges resolve, but no artifacts are produced.
//!
//! This is not a lie: if a C++ consumer later fails because an
//! expected artifact is missing, that's the symptom of the unfinished
//! rule() evaluator and the fix is to build it, not to paper over the
//! gap with fake content. See UNIMPLEMENTED.md §5j and memory/
//! feedback_no_fake_stubs.md. Tracked in issue #183.

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use rush_rules::context::BuildContext;
use rush_rules::providers::ProviderSet;
use rush_rules::{Artifact, ArtifactType, BuildRule, RuleInstance, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Graph-only placeholder for Starlark rule()-defined custom rules.
/// See module docs.
pub struct CustomRuleStub;

impl BuildRule for CustomRuleStub {
    fn name(&self) -> &str {
        "CustomRule"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        match string_attr(attrs, "__rush_custom_rule_kind").as_deref() {
            Some("mojo_library") => {
                return Ok(Box::new(MojoLibraryInstance::parse(attrs)?));
            }
            Some("mojo_binary") | Some("mojo_test") => {
                return Ok(Box::new(MojoBinaryInstance::parse(attrs)?));
            }
            Some("mojo_import") => {
                return Ok(Box::new(MojoImportInstance::parse(attrs)?));
            }
            Some("modular_versioned_expand_template") => {
                return Ok(Box::new(
                    ModularVersionedExpandTemplateInstance::parse(attrs)?,
                ));
            }
            Some("uv_lock") => {
                return Ok(Box::new(UvLockInstance::parse(attrs)?));
            }
            // rules_python's Starlark rule() identities converge on the
            // same implementations as the native py rules.
            Some("py_binary") => {
                return rush_rules::rules_python::PyBinaryRule.parse_attributes(attrs);
            }
            Some("py_test") => {
                return rush_rules::rules_python::PyTestRule.parse_attributes(attrs);
            }
            Some("py_library") => {
                return rush_rules::rules_python::PyLibraryRule.parse_attributes(attrs);
            }
            Some("pycross_wheel_library") => {
                return Ok(Box::new(PycrossWheelLibraryInstance::parse(attrs)?));
            }
            _ => {}
        }

        if let Some(instance) = parse_tensorflow_mlir_custom_rule(attrs)? {
            return Ok(instance);
        }

        // Extract name attribute if available
        let name = attrs
            .get("name")
            .and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "custom_rule_instance".to_string());

        // #39 graph-RSS: extract ONLY the deps now instead of retaining the
        // whole `attrs` map per instance. The core:lib analysis OOM (54GB) is
        // dominated by per-RuleInstance attr payloads held in the graph across
        // the full transitive closure; this rule cloned the entire attr map
        // (including any large `format!("{:?}", dict)` attr strings) for every
        // graph-only custom-rule target yet only ever reads `deps`. build() and
        // providers() are empty (rule() evaluator unimplemented, issue #183),
        // so the remaining attrs are dead weight. Behavior is identical.
        let deps = extract_deps(attrs);

        Ok(Box::new(CustomRuleInstance { name, deps }))
    }

    fn documentation(&self) -> &str {
        "Graph-only placeholder for Starlark-defined custom rules \
         (via rule() in .bzl files). See UNIMPLEMENTED.md §5j."
    }
}

/// Extract the `deps` attribute as a list of label strings. Shared by
/// `parse_attributes` so the instance can keep ONLY the deps rather than
/// the entire attr map (#39 graph-RSS reduction).
fn extract_deps(attrs: &HashMap<String, Value>) -> Vec<String> {
    attrs
        .get("deps")
        .and_then(|v| match v {
            Value::List(items) => Some(
                items
                    .iter()
                    .filter_map(|item| match item {
                        Value::String(s) => Some(s.clone()),
                        _ => None,
                    })
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

fn string_attr(attrs: &HashMap<String, Value>, key: &str) -> Option<String> {
    match attrs.get(key) {
        Some(Value::String(value)) => Some(value.clone()),
        _ => None,
    }
}

fn bool_attr(attrs: &HashMap<String, Value>, key: &str) -> Option<bool> {
    match attrs.get(key) {
        Some(Value::Bool(value)) => Some(*value),
        _ => None,
    }
}

fn int_attr(attrs: &HashMap<String, Value>, key: &str) -> Option<i64> {
    match attrs.get(key) {
        Some(Value::Int(value)) => Some(*value),
        _ => None,
    }
}

fn string_list_attr(attrs: &HashMap<String, Value>, key: &str) -> Vec<String> {
    match attrs.get(key) {
        Some(Value::List(values)) => values
            .iter()
            .filter_map(|value| match value {
                Value::String(value) => Some(value.clone()),
                Value::Int(value) => Some(value.to_string()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// rules_pycross `pycross_wheel_library`: install a fetched wheel into a
/// per-target directory whose `site-packages` subdir is the PYTHONPATH
/// contract (Bazel's WheelInstall action does the same via pypa/installer:
/// zip-extract plus relocation of the `.data/{purelib,platlib,scripts,
/// headers,data}` scheme dirs). The install runs under the hermetic
/// rules_python interpreter with a stdlib-only script.
pub struct PycrossWheelLibraryInstance {
    name: String,
    wheel: String,
    deps: Vec<String>,
}

impl PycrossWheelLibraryInstance {
    fn parse(attrs: &HashMap<String, Value>) -> Result<Self> {
        let name = string_attr(attrs, "name")
            .ok_or_else(|| anyhow!("pycross_wheel_library requires a name"))?;
        let wheel = string_attr(attrs, "wheel").ok_or_else(|| {
            anyhow!("pycross_wheel_library {} requires a wheel", name)
        })?;
        for patch_attr in ["patches", "post_install_patches"] {
            if let Some(Value::List(items)) = attrs.get(patch_attr) {
                if !items.is_empty() {
                    return Err(anyhow!(
                        "pycross_wheel_library {}: {} is not supported yet",
                        name,
                        patch_attr
                    ));
                }
            }
        }
        Ok(Self {
            name,
            wheel,
            deps: string_list_attr(attrs, "deps"),
        })
    }
}

#[async_trait]
impl RuleInstance for PycrossWheelLibraryInstance {
    fn rule_type(&self) -> &str {
        // Was the trait default "unknown". The prune planner's admission
        // and its census gate on this string; an unnamed WRITER hiding in
        // the "unknown" bucket would ride along with any future
        // admission of that bucket. Named so gating can be per-kind.
        "pycross_wheel_library"
    }

    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        let mut deps = vec![self.wheel.clone()];
        deps.extend(self.deps.iter().cloned());
        deps
    }

    fn analyze(
        &self,
        ctx: &mut rush_rules::analysis_context::AnalysisContext,
        dep_providers: &[std::sync::Arc<ProviderSet>],
    ) -> ProviderSet {
        let rel = ctx.target_output_rel(&self.name, "site-packages");
        let site = ctx.predicted_output_path(Path::new(&rel));
        let mut imports = Vec::new();
        let mut site_roots = vec![site.display().to_string()];
        for provs in dep_providers {
            if let Some(info) = provs.get_custom("PyInfo") {
                for key in ["imports", "site_roots"] {
                    if let Some(joined) = info.data.get(key) {
                        for root in joined.split(':').filter(|s| !s.is_empty()) {
                            let target = if key == "imports" {
                                &mut imports
                            } else {
                                &mut site_roots
                            };
                            if !target.iter().any(|r: &String| r == root) {
                                target.push(root.to_string());
                            }
                        }
                    }
                }
            }
        }
        ProviderSet::empty().with_custom(
            rush_rules::rules_python::py_info(&imports, &site_roots),
        )
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        let wheel_file = context
            .iter_transitive_artifacts(&self.wheel)
            .and_then(|mut artifacts| {
                artifacts
                    .find(|a| {
                        a.path.extension().and_then(|e| e.to_str()) == Some("whl")
                    })
                    .map(|a| a.path.clone())
            })
            .ok_or_else(|| {
                anyhow!(
                    "pycross_wheel_library {}: wheel {} resolved no .whl file",
                    self.name,
                    self.wheel
                )
            })?;

        let package = context.package_path.as_deref().unwrap_or(Path::new(""));
        let out_root = context
            .output_dir
            .join(package)
            .join(&self.name);
        let site = out_root.join("site-packages");
        if out_root.exists() {
            std::fs::remove_dir_all(&out_root)?;
        }
        std::fs::create_dir_all(&site)?;

        let python = rush_rules::python_runtime::find_python3_lenient(
            &context.main_workspace_root,
            &rush_rules::rules_python::default_py_version(),
        )?;
        // The effective subset of pypa/installer for a plain
        // --enable-implicit-namespace-pkgs install: extract, then relocate
        // the wheel's .data scheme dirs (purelib/platlib join site-packages,
        // scripts -> bin, headers -> include, data -> data).
        const INSTALL_SNIPPET: &str = r#"
import shutil, sys, zipfile
from pathlib import Path
wheel, root = sys.argv[1], Path(sys.argv[2])
site = root / "site-packages"
with zipfile.ZipFile(wheel) as zf:
    zf.extractall(site)
for data_dir in list(site.glob("*.data")):
    for scheme in list(data_dir.iterdir()):
        dest = {
            "purelib": site,
            "platlib": site,
            "scripts": root / "bin",
            "headers": root / "include",
            "data": root / "data",
        }.get(scheme.name)
        if dest is None:
            raise SystemExit(f"unknown wheel data scheme: {scheme}")
        dest.mkdir(parents=True, exist_ok=True)
        for item in list(scheme.iterdir()):
            shutil.move(str(item), dest / item.name)
        if scheme.name == "scripts":
            for f in dest.iterdir():
                f.chmod(0o755)
    shutil.rmtree(data_dir)
"#;
        let output = std::process::Command::new(&python)
            .arg("-c")
            .arg(INSTALL_SNIPPET)
            .arg(&wheel_file)
            .arg(&out_root)
            .env("SOURCE_DATE_EPOCH", "315532800")
            .env("PYTHONHASHSEED", "0")
            .output()
            .with_context(|| {
                format!("pycross_wheel_library {}: spawn installer", self.name)
            })?;
        if !output.status.success() {
            return Err(anyhow!(
                "pycross_wheel_library {}: wheel install failed: {}",
                self.name,
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        // Emit the site root, then re-emit dep site roots so consumers
        // reading artifact shape (not providers) still see the closure.
        let mut artifacts = vec![Artifact {
            path: site,
            artifact_type: ArtifactType::Generated,
        }];
        for dep in &self.deps {
            if let Some(dep_artifacts) = context.direct_artifacts(dep) {
                for a in dep_artifacts {
                    if a.path
                        .file_name()
                        .map(|n| n == "site-packages")
                        .unwrap_or(false)
                        && !artifacts.iter().any(|x| x.path == a.path)
                    {
                        artifacts.push(a.clone());
                    }
                }
            }
        }
        Ok(artifacts)
    }

    fn providers(&self) -> ProviderSet {
        ProviderSet::empty()
    }
}

/// Modular's `uv_lock` rule (bazel/pip/requirements/uv.bzl): expand the
/// repo's own uv-lock.sh template with the same substitutions the rule
/// makes, then run it — resolving Python package versions with the fetched
/// `uv` binary and the hermetic Python interpreter. The action is
/// `requires-network` in Bazel and runs unsandboxed there too.
pub struct UvLockInstance {
    name: String,
    existing_lockfile: String,
    pyproject: String,
    py3_runtime: String,
}

const UV_TOOL_LABEL: &str = "//bazel/internal:uv";

impl UvLockInstance {
    fn parse(attrs: &HashMap<String, Value>) -> Result<Self> {
        let name = string_attr(attrs, "name")
            .ok_or_else(|| anyhow!("uv_lock requires a string name"))?;
        let existing_lockfile =
            string_attr(attrs, "existing_lockfile").ok_or_else(|| {
                anyhow!("uv_lock {} requires existing_lockfile", name)
            })?;
        let pyproject = string_attr(attrs, "pyproject")
            .ok_or_else(|| anyhow!("uv_lock {} requires pyproject", name))?;
        let py3_runtime = string_attr(attrs, "py3_runtime")
            .ok_or_else(|| anyhow!("uv_lock {} requires py3_runtime", name))?;
        Ok(Self {
            name,
            existing_lockfile,
            pyproject,
            py3_runtime,
        })
    }

    /// Locate the hermetic interpreter for the runtime label via the
    /// shared rules_python discovery (RUSH_PY3 override, then the
    /// materialized platform repos in known output bases).
    fn find_python3(&self, context: &BuildContext) -> Result<PathBuf> {
        rush_rules::python_runtime::find_python3(
            &context.main_workspace_root,
            &self.py3_runtime,
        )
        .with_context(|| format!("uv_lock {}", self.name))
    }
}

/// A `../..`-style path from `base` to `target` (both absolute), for
/// templates that prefix substitutions with `$PWD/`.
fn relative_path_from(base: &Path, target: &Path) -> PathBuf {
    use std::path::Component;
    let base_comps: Vec<Component> = base.components().collect();
    let target_comps: Vec<Component> = target.components().collect();
    let common = base_comps
        .iter()
        .zip(target_comps.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut out = PathBuf::new();
    for _ in common..base_comps.len() {
        out.push("..");
    }
    for comp in &target_comps[common..] {
        out.push(comp.as_os_str());
    }
    out
}

#[async_trait]
impl RuleInstance for UvLockInstance {
    fn rule_type(&self) -> &str {
        // Was the trait default "unknown". The prune planner's admission
        // and its census gate on this string; an unnamed WRITER hiding in
        // the "unknown" bucket would ride along with any future
        // admission of that bucket. Named so gating can be per-kind.
        "uv_lock"
    }

    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        vec![UV_TOOL_LABEL.to_string()]
    }

    fn providers(&self) -> ProviderSet {
        ProviderSet::empty()
    }

    fn analyze(
        &self,
        ctx: &mut rush_rules::analysis_context::AnalysisContext,
        _dep_providers: &[std::sync::Arc<rush_rules::providers::ProviderSet>],
    ) -> ProviderSet {
        use rush_rules::providers::FilesInfo;
        // DefaultInfo(files = [lockfile]) — FilesInfo is what genrule's
        // get_dependency_files() reads for `srcs = [":uv_lock"]`.
        let rel = ctx
            .target_output_rel(&self.name, &format!("{}-new-uv.lock", self.name));
        let path = ctx.predicted_output_path(std::path::Path::new(&rel));
        ProviderSet::empty().with_files(FilesInfo::new(vec![path]))
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        let template = context.source_path("uv-lock.sh");
        let template_src =
            std::fs::read_to_string(&template).with_context(|| {
                format!("uv_lock {}: read template {}", self.name, template.display())
            })?;
        let package = context.package_path.as_deref().ok_or_else(|| {
            anyhow!("uv_lock {} has no package build context", self.name)
        })?;

        let uv = context
            .iter_transitive_artifacts(UV_TOOL_LABEL)
            .and_then(|mut artifacts| artifacts.next().map(|a| a.path.clone()))
            .ok_or_else(|| {
                anyhow!(
                    "uv_lock {}: {} resolved no uv binary",
                    self.name,
                    UV_TOOL_LABEL
                )
            })?;
        let python = self.find_python3(context)?;

        // Layout must match analyze()'s target_output_rel prediction:
        // <pkg>/<name>/<name>-new-uv.lock.
        let output = context
            .output_dir
            .join(package)
            .join(&self.name)
            .join(format!("{}-new-uv.lock", self.name));
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let ws = &context.workspace_root;
        let abs = |p: &Path| -> PathBuf {
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                ws.join(p)
            }
        };
        // The template prefixes {{python}}/{{pyproject}}/{{existing_lockfile}}
        // with `$PWD/`, so those substitutions must be cwd-relative; the
        // script runs with cwd = workspace root, matching Bazel's execroot.
        let script = template_src
            .replace(
                "{{existing_lockfile}}",
                &format!("{}/{}", package.display(), self.existing_lockfile),
            )
            .replace(
                "{{pyproject}}",
                &format!("{}/{}", package.display(), self.pyproject),
            )
            .replace(
                "{{python}}",
                &relative_path_from(ws, &abs(&python)).to_string_lossy(),
            )
            .replace("{{uv}}", &abs(&uv).to_string_lossy())
            .replace("{{lockfile_output}}", &abs(&output).to_string_lossy());
        // The script must not collide with the <name>/ output directory.
        let script_path = context
            .output_dir
            .join(package)
            .join(format!("{}.lockgen.sh", self.name));
        std::fs::write(&script_path, script)?;
        let mut perms = std::fs::metadata(&script_path)?.permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&script_path, perms)?;

        let status = std::process::Command::new("bash")
            .arg(abs(&script_path))
            .current_dir(ws)
            .output()
            .with_context(|| format!("uv_lock {}: spawn script", self.name))?;
        if !status.status.success() {
            return Err(anyhow!(
                "uv_lock {} failed: {}",
                self.name,
                String::from_utf8_lossy(&status.stderr)
            ));
        }
        if !abs(&output).is_file() {
            return Err(anyhow!(
                "uv_lock {} produced no lockfile at {}",
                self.name,
                output.display()
            ));
        }
        Ok(vec![Artifact {
            path: output,
            artifact_type: ArtifactType::Generated,
        }])
    }
}

/// Modular's `modular_versioned_expand_template`: expand a template with
/// user substitutions plus the version keys derived from the six root
/// string_flag build settings, exactly as
/// bazel/internal/modular_versioned_expand_template.bzl computes them.
pub struct ModularVersionedExpandTemplateInstance {
    name: String,
    template: String,
    out: String,
    substitutions: Vec<(String, String)>,
    data: Vec<String>,
}

impl ModularVersionedExpandTemplateInstance {
    fn parse(attrs: &HashMap<String, Value>) -> Result<Self> {
        let name = string_attr(attrs, "name").ok_or_else(|| {
            anyhow!("modular_versioned_expand_template requires a name")
        })?;
        let template = string_attr(attrs, "template").ok_or_else(|| {
            anyhow!(
                "modular_versioned_expand_template {} requires a template",
                name
            )
        })?;
        let out = string_attr(attrs, "out").ok_or_else(|| {
            anyhow!("modular_versioned_expand_template {} requires out", name)
        })?;
        let substitutions = match attrs.get("substitutions") {
            Some(Value::Dict(map)) => map
                .iter()
                .filter_map(|(k, v)| match v {
                    Value::String(s) => Some((k.clone(), s.clone())),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        Ok(Self {
            name,
            template,
            out,
            substitutions,
            data: string_list_attr(attrs, "data"),
        })
    }

    fn version_setting(context: &BuildContext, label: &str) -> Result<String> {
        let scope = context.workspace_root.to_string_lossy();
        rush_starlark::config_defines::build_setting_value_in_scope(&scope, label).ok_or_else(|| {
            anyhow!(
                "modular_versioned_expand_template: build setting {} was \
                 never registered (root BUILD not evaluated?)",
                label
            )
        })
    }
}

#[async_trait]
impl RuleInstance for ModularVersionedExpandTemplateInstance {
    fn rule_type(&self) -> &str {
        // Was the trait default "unknown". The prune planner's admission
        // and its census gate on this string; an unnamed WRITER hiding in
        // the "unknown" bucket would ride along with any future
        // admission of that bucket. Named so gating can be per-kind.
        "modular_versioned_expand_template"
    }

    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        self.data.clone()
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        let template_rel = self
            .template
            .trim_start_matches(':')
            .trim_start_matches("//");
        let template_path = context.source_path(
            template_rel
                .rsplit_once(':')
                .map(|(_, t)| t)
                .unwrap_or(template_rel),
        );
        let content =
            std::fs::read_to_string(&template_path).with_context(|| {
                format!(
                    "modular_versioned_expand_template {}: read template {}",
                    self.name,
                    template_path.display()
                )
            })?;

        let max_base = Self::version_setting(context, "//:max_base_version")?;
        let max_label = Self::version_setting(context, "//:max_version_label")?;
        let mojo_base = Self::version_setting(context, "//:mojo_base_version")?;
        let mojo_label = Self::version_setting(context, "//:mojo_version_label")?;
        let release_type = Self::version_setting(context, "//:release_type")?;
        let sha = Self::version_setting(context, "//:modular_version_sha")?;
        let split3 = |v: &str, what: &str| -> Result<(String, String, String)> {
            let mut it = v.splitn(3, '.');
            match (it.next(), it.next(), it.next()) {
                (Some(a), Some(b), Some(c)) => {
                    Ok((a.to_string(), b.to_string(), c.to_string()))
                }
                _ => Err(anyhow!(
                    "modular_versioned_expand_template {}: {} '{}' is not \
                     MAJOR.MINOR.PATCH",
                    self.name,
                    what,
                    v
                )),
            }
        };
        let (max_major, max_minor, max_patch) = split3(&max_base, "max_base_version")?;
        let (mojo_major, mojo_minor, mojo_patch) =
            split3(&mojo_base, "mojo_base_version")?;

        let mut expanded = content;
        // User substitutions first (version keys win on collision, matching
        // the rule's `expanded_substitutions | {...}` dict merge).
        for (key, value) in &self.substitutions {
            let value = expand_location_vars(
                "modular_versioned_expand_template",
                &self.name,
                value,
                context,
            )?;
            expanded = expanded.replace(key, &value);
        }
        let version_map: [(&str, String); 17] = [
            ("@MAX_VERSION_MAJOR@", max_major.clone()),
            ("@MAX_VERSION_MINOR@", max_minor.clone()),
            ("@MAX_VERSION_PATCH@", max_patch.clone()),
            ("@MAX_VERSION_LABEL@", max_label.clone()),
            ("@MAX_VERSION_STRING@", format!("{}{}", max_base, max_label)),
            ("@MODULAR_VERSION_REVISION@", sha),
            ("@MODULAR_BUILD_TYPE_LOWER@", release_type),
            ("@MOJO_VERSION_MAJOR@", mojo_major),
            ("@MOJO_VERSION_MINOR@", mojo_minor),
            ("@MOJO_VERSION_PATCH@", mojo_patch),
            ("@MOJO_VERSION_LABEL@", mojo_label.clone()),
            (
                "@MOJO_VERSION_STRING@",
                format!("{}{}", mojo_base, mojo_label),
            ),
            ("@MODULAR_VERSION_MAJOR@", max_major),
            ("@MODULAR_VERSION_MINOR@", max_minor),
            ("@MODULAR_VERSION_PATCH@", max_patch),
            ("@MODULAR_VERSION_LABEL@", max_label.clone()),
            (
                "@MODULAR_VERSION_STRING@",
                format!("{}{}", max_base, max_label),
            ),
        ];
        for (key, value) in &version_map {
            expanded = expanded.replace(key, value);
        }

        let package = context.package_path.as_deref().unwrap_or(Path::new(""));
        let output = context.output_dir.join(package).join(&self.out);
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("create template output directory {}", parent.display())
            })?;
        }
        std::fs::write(&output, expanded).with_context(|| {
            format!("write expanded template {}", output.display())
        })?;
        Ok(vec![Artifact {
            path: output,
            artifact_type: ArtifactType::Generated,
        }])
    }

    fn providers(&self) -> ProviderSet {
        ProviderSet::empty()
    }
}

/// rules_mojo `mojo_import`: expose prebuilt Mojo packages (wheel-provided
/// `.mojoc`/`.mojopkg` files) as artifacts. Consumers derive `-I` as the
/// artifact's parent directory, which equals rules_mojo's
/// `format_import(package.dirname + ".")` for the always-"." import_path
/// these targets use. Modular's `@modular_wheel` aliases route closed-source
/// deps (matmul_rs, _hal, ...) here.
pub struct MojoImportInstance {
    name: String,
    mojodeps: Vec<String>,
    deps: Vec<String>,
}

impl MojoImportInstance {
    fn parse(attrs: &HashMap<String, Value>) -> Result<Self> {
        let name = string_attr(attrs, "name")
            .ok_or_else(|| anyhow!("mojo_import requires a string name"))?;
        Ok(Self {
            name,
            mojodeps: string_list_attr(attrs, "mojodeps"),
            deps: string_list_attr(attrs, "deps"),
        })
    }
}

#[async_trait]
impl RuleInstance for MojoImportInstance {
    fn rule_type(&self) -> &str {
        // Was the trait default "unknown". The prune planner's admission
        // and its census gate on this string; an unnamed WRITER hiding in
        // the "unknown" bucket would ride along with any future
        // admission of that bucket. Named so gating can be per-kind.
        "mojo_import"
    }

    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        self.deps.clone()
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        let mut artifacts = Vec::new();
        for rel in &self.mojodeps {
            let path = context.source_path(rel);
            if !path.is_file() {
                return Err(anyhow!(
                    "mojo_import {}: prebuilt package {} is missing at {}",
                    self.name,
                    rel,
                    path.display()
                ));
            }
            artifacts.push(Artifact {
                path,
                artifact_type: ArtifactType::Generated,
            });
        }
        if artifacts.is_empty() {
            return Err(anyhow!(
                "mojo_import {} declares no mojodeps",
                self.name
            ));
        }
        Ok(artifacts)
    }

    fn providers(&self) -> ProviderSet {
        ProviderSet::empty()
    }
}

/// Bazel `ctx.expand_location` parity for mojo rules: expand
/// $(location <lbl>), $(locations <lbl>), $(execpath[s] <lbl>) inside one
/// argument. Labels resolve through the rule's declared dependency
/// artifacts (additional_compiler_inputs / data flow through
/// dependencies(), so BuildContext keys them by the verbatim label).
/// Unresolvable labels hard-error exactly like Bazel — never pass the
/// unexpanded variable through to the compiler.
fn expand_location_vars(
    rule: &str,
    name: &str,
    value: &str,
    context: &BuildContext,
) -> Result<String> {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("$(") {
        out.push_str(&rest[..start]);
        let tail = &rest[start + 2..];
        let end = tail.find(')').ok_or_else(|| {
            anyhow!("{} {}: unterminated make variable in {:?}", rule, name, value)
        })?;
        let expr = &tail[..end];
        let consumed = start + 2 + end + 1;
        match expr.split_once(' ') {
            Some((kw @ ("location" | "locations" | "execpath" | "execpaths"), label)) => {
                let label = label.trim();
                let files: Vec<String> = context
                    .iter_transitive_artifacts(label)
                    .ok_or_else(|| {
                        anyhow!(
                            "{} {}: $({} {}) label is not a declared dependency \
                             (Bazel requires it in additional_compiler_inputs/data)",
                            rule,
                            name,
                            kw,
                            label
                        )
                    })?
                    .map(|a| a.path.display().to_string())
                    .collect();
                if files.is_empty() {
                    return Err(anyhow!(
                        "{} {}: $({} {}) resolved no files",
                        rule,
                        name,
                        kw,
                        label
                    ));
                }
                if matches!(kw, "location" | "execpath") && files.len() != 1 {
                    return Err(anyhow!(
                        "{} {}: $({} {}) matched {} files; use the plural form",
                        rule,
                        name,
                        kw,
                        label,
                        files.len()
                    ));
                }
                out.push_str(&files.join(" "));
            }
            // Non-location make var (none expected in mojo copts): verbatim.
            _ => out.push_str(&rest[start..consumed]),
        }
        rest = &rest[consumed..];
    }
    out.push_str(rest);
    Ok(out)
}

fn collect_mojo_imports(
    context: &BuildContext,
    deps: &[String],
) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
    let mut import_dirs = Vec::new();
    let mut inputs = Vec::new();
    let mut seen_dirs = HashSet::new();
    let mut seen_inputs = HashSet::new();

    for dep in deps {
        let artifacts = context
            .iter_transitive_artifacts(dep)
            .ok_or_else(|| anyhow!("cannot resolve Mojo dependency {}", dep))?;
        for artifact in artifacts {
            if artifact.path.extension().and_then(|ext| ext.to_str()) != Some("mojoc") {
                continue;
            }
            if seen_inputs.insert(artifact.path.clone()) {
                inputs.push(artifact.path.clone());
            }
            if let Some(parent) = artifact.path.parent() {
                let parent = parent.to_path_buf();
                if seen_dirs.insert(parent.clone()) {
                    import_dirs.push(parent);
                }
            }
        }
    }

    // Keep import dirs in dependency-declaration order (Bazel's depset
    // order): mojoc resolution — and at least one compiler crash
    // (test_rms_norm_fused_residual) — is sensitive to -I order, and the
    // sorted order diverged from Bazel's command line. Inputs stay sorted
    // for stable action hashing.
    inputs.sort();
    Ok((import_dirs, inputs))
}

fn parse_tensorflow_mlir_custom_rule(
    attrs: &HashMap<String, Value>,
) -> Result<Option<Box<dyn RuleInstance>>> {
    let Some(name) = string_attr(attrs, "name") else {
        return Ok(None);
    };

    if let (Some(template), Some(element_type), Some(output_type), Some(platform), Some(out)) = (
        string_attr(attrs, "template"),
        string_attr(attrs, "type"),
        string_attr(attrs, "output_type"),
        string_attr(attrs, "platform"),
        string_attr(attrs, "out"),
    ) {
        if template.ends_with(".mlir.tmpl") && out.ends_with(".mlir") {
            return Ok(Some(Box::new(MlirOpGenerationInstance {
                name,
                template,
                element_type,
                output_type,
                platform,
                out,
            })));
        }
    }

    if name.ends_with("_kernel_generator") {
        if let (Some(mlir_op), Some(data_type), Some(tile_size), Some(host_triple)) = (
            string_attr(attrs, "mlir_op"),
            string_attr(attrs, "data_type"),
            string_attr(attrs, "tile_size"),
            string_attr(attrs, "host_triple"),
        ) {
            return Ok(Some(Box::new(MlirGpuKernelInstance {
                name,
                mlir_op,
                data_type,
                tile_size,
                unroll_factors: string_attr(attrs, "unroll_factors")
                    .filter(|value| value != "None"),
                max_supported_rank: int_attr(attrs, "max_supported_rank").unwrap_or(5),
                host_triple,
                gpu_archs: string_list_attr(attrs, "gpu_archs"),
                jit: bool_attr(attrs, "jit").unwrap_or(false),
                jit_i64_indexed_for_large_tensors: bool_attr(
                    attrs,
                    "jit_i64_indexed_for_large_tensors",
                )
                .unwrap_or(false),
                extra_args: string_list_attr(attrs, "extra_args"),
            })));
        }
    }

    Ok(None)
}

/// Concrete support for the upstream rules_mojo `mojo_library` rule.
///
/// Rush does not yet expose a general Starlark `ctx.actions` API, so known
/// custom rules are specialized here. This mirrors the single
/// `MojoPrecompile` action emitted by rules_mojo without pretending the rest
/// of the custom-rule surface is implemented.
/// Make `stage_root` hold exactly one symlink per source, touching only what
/// differs.
///
/// The previous approach removed the whole stage tree and recreated every
/// symlink on each build. Reconciling costs one readlink per unchanged
/// source instead of an unlink plus a symlink, and an unchanged target
/// mutates nothing.
///
/// Returns the number of filesystem mutations so a no-change build's cost is
/// observable rather than assumed.
fn stage_mojo_sources(
    stage_root: &Path,
    desired_pairs: &[(PathBuf, PathBuf)],
) -> Result<usize> {
    // staged path -> the absolute source it must point at. Built by the
    // caller, which owns the workspace-relative and absolutize helpers.
    let desired: HashMap<&PathBuf, &PathBuf> =
        desired_pairs.iter().map(|(k, v)| (k, v)).collect();

    // Existing state: every non-directory entry under the stage root.
    let mut existing: Vec<PathBuf> = Vec::new();
    let mut stack = vec![stage_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(path),
                Ok(_) => existing.push(path),
                Err(_) => {}
            }
        }
    }

    let mut mutations = 0usize;

    // Remove anything not wanted, or pointing at the wrong source. Doing this
    // is what keeps a removed src from lingering in the farm.
    for path in &existing {
        let keep = match desired.get(path) {
            Some(want) => std::fs::read_link(path)
                .map(|actual| actual == **want)
                .unwrap_or(false),
            None => false,
        };
        if !keep {
            let _ = std::fs::remove_file(path);
            mutations += 1;
        }
    }

    // Create what is missing; an entry that already points at the right
    // source is left alone.
    for (staged, want) in desired_pairs {
        if std::fs::read_link(staged).map(|a| a == *want).unwrap_or(false) {
            continue;
        }
        if let Some(parent) = staged.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("create Mojo stage directory {}", parent.display())
            })?;
        }
        let _ = std::fs::remove_file(staged);
        std::os::unix::fs::symlink(want, staged).with_context(|| {
            format!("stage Mojo source {} -> {}", want.display(), staged.display())
        })?;
        mutations += 1;
    }

    Ok(mutations)
}

#[derive(Debug)]
struct MojoLibraryInstance {
    name: String,
    srcs: Vec<String>,
    deps: Vec<String>,
    data: Vec<String>,
    additional_compiler_inputs: Vec<String>,
    copts: Vec<String>,
}

impl MojoLibraryInstance {
    fn parse(attrs: &HashMap<String, Value>) -> Result<Self> {
        let name = string_attr(attrs, "name")
            .ok_or_else(|| anyhow!("mojo_library requires a string name"))?;
        Ok(Self {
            name,
            srcs: string_list_attr(attrs, "srcs"),
            deps: string_list_attr(attrs, "deps"),
            data: string_list_attr(attrs, "data"),
            additional_compiler_inputs: string_list_attr(attrs, "additional_compiler_inputs"),
            copts: string_list_attr(attrs, "copts"),
        })
    }

    fn is_label(value: &str) -> bool {
        value.starts_with(':') || value.starts_with("//") || value.starts_with('@')
    }

    fn collect_dependency_labels(&self) -> Vec<String> {
        let mut labels = Vec::new();
        let mut seen = HashSet::new();
        for value in self
            .srcs
            .iter()
            .chain(&self.deps)
            .chain(&self.data)
            .chain(&self.additional_compiler_inputs)
        {
            if Self::is_label(value) && seen.insert(value.clone()) {
                labels.push(value.clone());
            }
        }
        labels
    }

    fn collect_sources(&self, context: &BuildContext) -> Result<Vec<PathBuf>> {
        let mut sources = Vec::new();
        let mut seen = HashSet::new();

        for src in &self.srcs {
            if Self::is_label(src) {
                let artifacts = context.iter_transitive_artifacts(src).ok_or_else(|| {
                    anyhow!(
                        "mojo_library {} cannot resolve srcs dependency {}",
                        self.name,
                        src
                    )
                })?;
                for artifact in artifacts {
                    if artifact.path.extension().and_then(|ext| ext.to_str()) == Some("mojo")
                        && seen.insert(artifact.path.clone())
                    {
                        sources.push(artifact.path.clone());
                    }
                }
            } else {
                let path = context.source_path(src);
                if seen.insert(path.clone()) {
                    sources.push(path);
                }
            }
        }

        if sources.is_empty() {
            return Err(anyhow!(
                "mojo_library {} resolved no .mojo source files",
                self.name
            ));
        }
        Ok(sources)
    }

    fn collect_additional_inputs(&self, context: &BuildContext) -> Vec<PathBuf> {
        let mut inputs = Vec::new();
        let mut seen = HashSet::new();
        for value in self.data.iter().chain(&self.additional_compiler_inputs) {
            if Self::is_label(value) {
                if let Some(artifacts) = context.iter_transitive_artifacts(value) {
                    for artifact in artifacts {
                        if seen.insert(artifact.path.clone()) {
                            inputs.push(artifact.path.clone());
                        }
                    }
                }
            } else {
                let path = context.source_path(value);
                if seen.insert(path.clone()) {
                    inputs.push(path);
                }
            }
        }
        inputs
    }

    fn find_mojo_compiler_with_roots(
        workspace_root: &Path,
        compiler_override: Option<PathBuf>,
        cache_root_override: Option<PathBuf>,
    ) -> Result<PathBuf> {
        if let Some(path) = compiler_override {
            if path.is_file() {
                return Ok(path);
            }
            return Err(anyhow!(
                "RUSH_MOJO points to missing compiler {}",
                path.display()
            ));
        }

        let cache_root = cache_root_override
            .unwrap_or_else(|| workspace_root.join(".cache").join("rush"));
        let external = cache_root.join("bazel_output_base").join("external");
        let preferred = external
            .join("rules_mojo++mojo+mojo_toolchain_linux_x86_64")
            .join("bin")
            .join("mojo");
        if preferred.is_file() {
            return Ok(preferred);
        }

        if let Ok(entries) = std::fs::read_dir(&external) {
            let mut candidates = entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .contains("mojo_toolchain_")
                })
                .map(|entry| entry.path().join("bin").join("mojo"))
                .filter(|path| path.is_file())
                .collect::<Vec<_>>();
            candidates.sort();
            if let Some(path) = candidates.into_iter().next() {
                return Ok(path);
            }
        }

        Err(anyhow!(
            "no fetched Mojo compiler found; set RUSH_MOJO or populate the rules_mojo toolchain repository"
        ))
    }

    fn find_mojo_compiler(workspace_root: &Path) -> Result<PathBuf> {
        Self::find_mojo_compiler_with_roots(
            workspace_root,
            std::env::var_os("RUSH_MOJO").map(PathBuf::from),
            std::env::var_os("RUSH_WORKSPACE_CACHE_ROOT").map(PathBuf::from),
        )
    }

    fn collect_toolchain_inputs(compiler: &Path) -> Vec<PathBuf> {
        let Some(root) = compiler.parent().and_then(Path::parent) else {
            return vec![compiler.to_path_buf()];
        };
        let mut inputs = Vec::new();
        for directory in [root.join("bin"), root.join("lib")] {
            if let Ok(entries) = std::fs::read_dir(directory) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_file() {
                        inputs.push(path);
                    }
                }
            }
        }
        inputs.sort();
        inputs.dedup();
        if !inputs.iter().any(|path| path == compiler) {
            inputs.push(compiler.to_path_buf());
        }
        inputs
    }

}

#[async_trait]
impl RuleInstance for MojoLibraryInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        self.collect_dependency_labels()
    }

    /// Declare the MojoPrecompile action so the build knows which targets a
    /// source edit affects.
    ///
    /// The PASS-B prune index is built from actions DECLARED in the
    /// ArtifactRegistry: it maps every declared input path to the targets
    /// that consume it, and an incremental build uses that map to walk only
    /// the affected subgraph. Mojo rules built their action inside `build()`
    /// and declared nothing, so on the Modular corpus the index held 51
    /// paths for 4,175 targets - all of them genrule outputs. Editing any
    /// .mojo file mapped to no seed target, pruning declined, and PASS-B
    /// re-walked the entire graph: measured at 32-37s for a one-line
    /// comment, against Bazel's 13s.
    ///
    /// Declared INCOMPLETE on purpose. Execution stays in `build()` (the
    /// scheduler must not try to run this), and the label-shaped srcs and
    /// dependency imports are not resolvable here - only `build()` has the
    /// providers for those. That subset is SAFE for pruning because the
    /// pruner requires EVERY dirty path to map to a seed before it prunes:
    /// an edit to something still undeclared simply falls back to the full
    /// walk, exactly as today.
    fn analyze(
        &self,
        ctx: &mut rush_rules::analysis_context::AnalysisContext,
        dep_providers: &[std::sync::Arc<rush_rules::providers::ProviderSet>],
    ) -> rush_rules::providers::ProviderSet {
        use rush_action::ids::ArtifactKind;

        let package = ctx.package().to_string();
        let workspace_root = ctx.workspace_root().to_path_buf();

        // Plain-file srcs resolve to <workspace>/<package>/<src>, the same
        // path build() derives via context.source_path().
        let mut inputs = Vec::new();
        for src in &self.srcs {
            if Self::is_label(src) {
                continue;
            }
            let abs = workspace_root.join(&package).join(src);
            inputs.push(ctx.register_source_artifact(&abs, ArtifactKind::Source));
        }
        for extra in &self.additional_compiler_inputs {
            if Self::is_label(extra) {
                continue;
            }
            let abs = workspace_root.join(&package).join(extra);
            inputs.push(ctx.register_source_artifact(&abs, ArtifactKind::Source));
        }

        // Label-shaped srcs are the common case here, and the one that
        // matters most: //mojo/stdlib/std:std takes its sources from a
        // filegroup of glob(["**/*.mojo"]), so declaring only plain-file
        // srcs left every stdlib source unmapped - including the file this
        // was measured on. A filegroup publishes its members as FilesInfo,
        // and the orchestrator has already built the deps' providers by the
        // time analyze() runs, so resolve them here.
        //
        // A dependency's own sources landing in this input set is a slight
        // over-approximation, and a safe one: extra seeds can only widen the
        // active set, never narrow it.
        for provider in dep_providers {
            let Some(files) = provider.files.as_ref() else {
                continue;
            };
            for file in &files.files {
                if file.extension().and_then(|e| e.to_str()) == Some("mojo") {
                    inputs.push(ctx.register_source_artifact(file, ArtifactKind::Source));
                }
            }
        }

        if inputs.is_empty() {
            return rush_rules::providers::ProviderSet::empty();
        }

        // build() writes <output_dir>/<package>/<name>.mojoc.
        let rel = std::path::Path::new(&package).join(format!("{}.mojoc", self.name));
        let output = ctx.register_generated_artifact(&rel, ArtifactKind::Generated, None);

        let _ = ctx.declare_incomplete_action(
            "MojoPrecompile",
            inputs,
            vec![output],
            "label srcs and transitive mojo imports resolve only in build()",
        );

        rush_rules::providers::ProviderSet::empty()
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        let sources = self.collect_sources(context)?;
        let (import_dirs, dependency_inputs) = collect_mojo_imports(context, &self.deps)?;
        let compiler = Self::find_mojo_compiler(&context.main_workspace_root)?;
        let package = context.package_path.as_deref().ok_or_else(|| {
            anyhow!("mojo_library {} has no package build context", self.name)
        })?;
        let output = context
            .output_dir
            .join(package)
            .join(format!("{}.mojoc", self.name));
        let output_parent = output
            .parent()
            .ok_or_else(|| anyhow!("invalid Mojo output path {}", output.display()))?;
        std::fs::create_dir_all(output_parent).with_context(|| {
            format!("create Mojo output directory {}", output_parent.display())
        })?;

        // Bazel-sandbox parity for MojoPrecompile (byte-exact validated):
        // mojoc resolves imports path-relatively from the source dirs, so
        // compiling in the live workspace lets undeclared SIBLING source
        // packages shadow/augment the declared precompiled imports and skew
        // the output (max/kernels/src/comm gained ~944B vs Bazel). It also
        // embeds the positional root argument VERBATIM (not rewritten by
        // -strip-file-prefix), so the root must be workspace-relative with
        // the process cwd at a staged source root that contains only the
        // declared srcs — exactly what Bazel's sandbox provides.
        let workspace_relative = |path: &Path| -> PathBuf {
            path.strip_prefix(&context.workspace_root)
                .unwrap_or(path)
                .to_path_buf()
        };
        let absolutize = |path: &Path| -> PathBuf {
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                context.workspace_root.join(path)
            }
        };
        let stage_root = absolutize(
            &context
                .output_dir
                .join(".mojo-lib-stage")
                .join(package)
                .join(&self.name),
        );
        // Reconcile the stage farm instead of rebuilding it.
        //
        // This used to remove_dir_all() the whole stage root and recreate a
        // symlink per source on EVERY build - cache hit or not.
        // //mojo/stdlib/std:std alone stages 271 sources and the corpus has
        // 117 mojo targets, so an unchanged tree still paid thousands of
        // unlink and symlink syscalls per build. It also hid inside
        // "cached=true 0.000s": the action timer starts after this runs.
        //
        // Correctness is preserved by reconciling in BOTH directions - a
        // symlink that no longer matches a declared source is removed, so
        // dropping a file from srcs cannot leave mojoc compiling it.
        let staged_pairs: Vec<(PathBuf, PathBuf)> = sources
            .iter()
            .map(|s| (stage_root.join(workspace_relative(s)), absolutize(s)))
            .collect();
        let stage_mutations = stage_mojo_sources(&stage_root, &staged_pairs)?;
        if std::env::var("RUSH_MOJO_TIMING").as_deref() == Ok("1") {
            // Countable, and immune to host contention: how many symlinks
            // this build actually had to touch. The old code's answer was
            // always sources.len(), on every build.
            eprintln!(
                "[stage] {} sources={} mutations={}",
                self.name,
                staged_pairs.len(),
                stage_mutations
            );
        }

        let mut args = vec![
            "precompile".to_string(),
            "-strip-file-prefix=.".to_string(),
            "-o".to_string(),
            absolutize(&output).to_string_lossy().into_owned(),
        ];
        for copt in &self.copts {
            args.push(expand_location_vars(
                "mojo_library",
                &self.name,
                copt,
                context,
            )?);
        }
        // Bazel parity (rules_mojo mojo_library.bzl _format_root): the
        // MojoPrecompile positional root is ctx.files.srcs[0].dirname, NOT
        // the package dir — packages like op_utils glob srcs from a
        // subdirectory, and mojoc requires the directory that actually
        // holds __init__.mojo.
        let root_dir = sources
            .first()
            .and_then(|s| s.parent())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| context.workspace_root.join(package));
        // Srcs outside the root each add their own -I (rules_mojo
        // _format_include), BEFORE the dependency import dirs. Relative
        // paths resolve under the staged cwd.
        for source in &sources {
            if let Some(dir) = source.parent() {
                if !dir.starts_with(&root_dir) {
                    args.push("-I".to_string());
                    args.push(
                        workspace_relative(dir).to_string_lossy().into_owned(),
                    );
                }
            }
        }
        // Dependency imports are precompiled .mojoc dirs OUTSIDE the stage:
        // pass them absolute (path shape is byte-irrelevant, proven).
        for import_dir in import_dirs {
            args.push("-I".to_string());
            args.push(absolutize(&import_dir).to_string_lossy().into_owned());
        }
        args.push(
            workspace_relative(&root_dir).to_string_lossy().into_owned(),
        );

        let command = rush_action::Command {
            executable: compiler.clone(),
            args,
            working_dir: Some(stage_root.clone()),
            execution_info: rush_action::ExecutionInfo::default(),
        };
        let mut input_paths = sources;
        input_paths.extend(dependency_inputs);
        input_paths.extend(self.collect_additional_inputs(context));
        input_paths.extend(Self::collect_toolchain_inputs(&compiler));
        input_paths.sort();
        input_paths.dedup();
        let inputs = input_paths
            .into_iter()
            .map(rush_action::Artifact::Source)
            .collect::<Vec<_>>();
        let outputs = vec![rush_action::Artifact::Source(output.clone())];
        let env = HashMap::from([
            (
                "MODULAR_CRASH_REPORTING_ENABLED".to_string(),
                "false".to_string(),
            ),
            (
                "MODULAR_TELEMETRY_ENABLED".to_string(),
                "false".to_string(),
            ),
            ("PATH".to_string(), "/dev/null".to_string()),
            ("TEST_TMPDIR".to_string(), ".".to_string()),
        ]);
        let args_for_log: Vec<String> = command.args.clone();
        let id = rush_action::Action::compute_id(&inputs, &command, &env)?;
        let action = rush_action::Action {
            id,
            command,
            env,
            inputs,
            outputs,
            mnemonic: "MojoPrecompile".to_string(),
        };
        // RUSH_MOJO_TIMING=1: the changed Mojo action's own wall time, plus
        // the exact child command. Aggregate phase time is not critical-path
        // time: if this ONE action is most of the build, no amount of
        // scheduler pruning can win, and that has to be measured rather than
        // inferred from a phase total.
        let mojo_timing = std::env::var("RUSH_MOJO_TIMING").as_deref() == Ok("1");
        let t_action = std::time::Instant::now();
        let result = context.execute_action(action).await?;
        if mojo_timing {
            eprintln!(
                "[mojo] {} {:.3}s cached={} exit={} argv: {} {}",
                self.name,
                t_action.elapsed().as_secs_f64(),
                result.cached,
                result.exit_code,
                compiler.display(),
                args_for_log.join(" "),
            );
        }
        if result.exit_code != 0 {
            return Err(anyhow!(
                "Mojo precompile failed for {}: {}",
                self.name,
                String::from_utf8_lossy(&result.stderr)
            ));
        }
        // Phase 4: a virtual cache hit legitimately leaves the output
        // CAS-backed (stamp on disk, bytes in the CAS registry).
        if !rush_action::virtual_artifacts::present_or_virtual(&output) {
            return Err(anyhow!(
                "Mojo precompile reported success but did not produce {}",
                output.display()
            ));
        }

        Ok(vec![Artifact {
            path: output,
            artifact_type: ArtifactType::Generated,
        }])
    }

    fn providers(&self) -> rush_rules::providers::ProviderSet {
        rush_rules::providers::ProviderSet::empty()
    }

    fn rule_type(&self) -> &str {
        "mojo_library"
    }
}

#[derive(Debug)]
struct MojoBinaryInstance {
    name: String,
    srcs: Vec<String>,
    main: Option<String>,
    deps: Vec<String>,
    data: Vec<String>,
    additional_compiler_inputs: Vec<String>,
    copts: Vec<String>,
    linkopts: Vec<String>,
    enable_assertions: bool,
    requires_gpu: bool,
}

impl MojoBinaryInstance {
    fn parse(attrs: &HashMap<String, Value>) -> Result<Self> {
        let name = string_attr(attrs, "name")
            .ok_or_else(|| anyhow!("mojo_binary requires a string name"))?;
        let target_compatible_with = string_list_attr(attrs, "target_compatible_with");
        Ok(Self {
            name,
            srcs: string_list_attr(attrs, "srcs"),
            // BUILD evaluation stringifies an explicit Starlark `None` into
            // the string "None" (macros like mojo_filecheck_test pass
            // `main = None` through); treat it as unset like Bazel does.
            main: string_attr(attrs, "main").filter(|v| v != "None"),
            deps: string_list_attr(attrs, "deps"),
            data: string_list_attr(attrs, "data"),
            additional_compiler_inputs: string_list_attr(attrs, "additional_compiler_inputs"),
            copts: string_list_attr(attrs, "copts"),
            linkopts: string_list_attr(attrs, "linkopts"),
            enable_assertions: bool_attr(attrs, "enable_assertions").unwrap_or(true),
            requires_gpu: target_compatible_with
                .iter()
                .any(|constraint| constraint.ends_with(":has_gpu")),
        })
    }

    fn dependency_labels(&self) -> Vec<String> {
        let mut labels = Vec::new();
        let mut seen = HashSet::new();
        for value in self
            .srcs
            .iter()
            .chain(&self.deps)
            .chain(&self.data)
            .chain(&self.additional_compiler_inputs)
        {
            if MojoLibraryInstance::is_label(value) && seen.insert(value.clone()) {
                labels.push(value.clone());
            }
        }
        labels
    }

    fn collect_sources(&self, context: &BuildContext) -> Result<Vec<PathBuf>> {
        let mut sources = Vec::new();
        let mut seen = HashSet::new();
        for src in &self.srcs {
            if MojoLibraryInstance::is_label(src) {
                let artifacts = context.iter_transitive_artifacts(src).ok_or_else(|| {
                    anyhow!("mojo_binary {} cannot resolve src {}", self.name, src)
                })?;
                for artifact in artifacts {
                    if artifact.path.extension().and_then(|ext| ext.to_str()) == Some("mojo")
                        && seen.insert(artifact.path.clone())
                    {
                        sources.push(artifact.path.clone());
                    }
                }
            } else {
                let path = context.source_path(src);
                if seen.insert(path.clone()) {
                    sources.push(path);
                }
            }
        }
        if sources.is_empty() {
            return Err(anyhow!(
                "mojo_binary {} resolved no .mojo source files",
                self.name
            ));
        }
        Ok(sources)
    }

    fn select_main<'a>(&self, sources: &'a [PathBuf]) -> Result<&'a Path> {
        if let Some(main) = self.main.as_deref() {
            let main_name = Path::new(main).file_name();
            if let Some(source) = sources.iter().find(|source| source.file_name() == main_name) {
                return Ok(source);
            }
            return Err(anyhow!(
                "mojo_binary {} main {} is not in srcs",
                self.name,
                main
            ));
        }
        if sources.len() == 1 {
            return Ok(&sources[0]);
        }
        sources
            .iter()
            .find(|source| {
                source.file_stem().and_then(|stem| stem.to_str()) == Some(self.name.as_str())
                    || source.file_stem().and_then(|stem| stem.to_str()) == Some("main")
            })
            .map(PathBuf::as_path)
            .ok_or_else(|| anyhow!("mojo_binary {} requires a main source", self.name))
    }

    fn collect_additional_inputs(&self, context: &BuildContext) -> Vec<PathBuf> {
        let mut inputs = Vec::new();
        let mut seen = HashSet::new();
        for value in self.data.iter().chain(&self.additional_compiler_inputs) {
            if MojoLibraryInstance::is_label(value) {
                if let Some(artifacts) = context.iter_transitive_artifacts(value) {
                    for artifact in artifacts {
                        if seen.insert(artifact.path.clone()) {
                            inputs.push(artifact.path.clone());
                        }
                    }
                }
            } else {
                let path = context.source_path(value);
                if seen.insert(path.clone()) {
                    inputs.push(path);
                }
            }
        }
        inputs
    }

    fn find_mojo_cxx(toolchain_root: &Path) -> Result<PathBuf> {
        if let Some(path) = std::env::var_os("RUSH_MOJO_CXX") {
            let path = PathBuf::from(path);
            if path.is_file() {
                return Ok(path);
            }
            return Err(anyhow!(
                "RUSH_MOJO_CXX points to missing compiler {}",
                path.display()
            ));
        }

        let external_root = toolchain_root.parent().ok_or_else(|| {
            anyhow!(
                "cannot derive Bazel external root from Mojo toolchain {}",
                toolchain_root.display()
            )
        })?;
        let arch = match std::env::consts::ARCH {
            "x86_64" => "x86_64",
            "aarch64" => "aarch64",
            other => {
                return Err(anyhow!(
                    "unsupported Mojo host architecture {}; set RUSH_MOJO_CXX",
                    other
                ));
            }
        };
        let candidate = external_root
            .join(format!("+http_archive+clang-linux-{}", arch))
            .join("bin")
            .join("clang++");
        if candidate.is_file() {
            Ok(candidate)
        } else {
            Err(anyhow!(
                "fetched Mojo C++ linker is missing at {}; set RUSH_MOJO_CXX",
                candidate.display()
            ))
        }
    }

    fn find_clang_resource_dir(cxx: &Path) -> Option<PathBuf> {
        if let Some(path) = std::env::var_os("RUSH_MOJO_CLANG_RESOURCE_DIR") {
            return Some(PathBuf::from(path));
        }
        let root = cxx.parent()?.parent()?.join("lib").join("clang");
        let mut versions = std::fs::read_dir(root)
            .ok()?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect::<Vec<_>>();
        versions.sort();
        versions.pop()
    }

    fn find_mojo_sysroot(toolchain_root: &Path) -> Option<PathBuf> {
        if let Some(path) = std::env::var_os("RUSH_MOJO_SYSROOT") {
            return Some(PathBuf::from(path));
        }
        let arch = match std::env::consts::ARCH {
            "x86_64" => "x86_64",
            "aarch64" => "aarch64",
            _ => return None,
        };
        let candidate = toolchain_root
            .parent()?
            .join(format!("sysroot-jammy-{}+", arch))
            .join("sysroot");
        candidate.is_dir().then_some(candidate)
    }
}

#[async_trait]
impl RuleInstance for MojoBinaryInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        self.dependency_labels()
    }

    /// Declare this binary's compile action, for the same reason
    /// [`MojoLibraryInstance::analyze`] does: the PASS-B prune index is
    /// built from DECLARED actions, and an undeclared target makes every
    /// edit fall back to walking the whole graph.
    ///
    /// Declaring the outputs matters as much as the inputs here. After a
    /// pruned rebuild the ledger is refreshed per target from
    /// `output_paths_by_target`, so a target whose outputs are unknown
    /// cannot safely take part in a pruned slice.
    fn analyze(
        &self,
        ctx: &mut rush_rules::analysis_context::AnalysisContext,
        dep_providers: &[std::sync::Arc<rush_rules::providers::ProviderSet>],
    ) -> rush_rules::providers::ProviderSet {
        use rush_action::ids::ArtifactKind;

        let package = ctx.package().to_string();
        let workspace_root = ctx.workspace_root().to_path_buf();

        let mut inputs = Vec::new();
        let plain = self
            .srcs
            .iter()
            .chain(self.main.iter())
            .chain(self.additional_compiler_inputs.iter());
        for src in plain {
            if MojoLibraryInstance::is_label(src) {
                continue;
            }
            let abs = workspace_root.join(&package).join(src);
            inputs.push(ctx.register_source_artifact(&abs, ArtifactKind::Source));
        }
        for provider in dep_providers {
            let Some(files) = provider.files.as_ref() else {
                continue;
            };
            for file in &files.files {
                if file.extension().and_then(|e| e.to_str()) == Some("mojo") {
                    inputs.push(ctx.register_source_artifact(file, ArtifactKind::Source));
                }
            }
        }
        if inputs.is_empty() {
            return rush_rules::providers::ProviderSet::empty();
        }

        // build() writes <output_dir>/<package>/<name> and, beside it, the
        // object as "<that path>.lo" - appended, not an extension swap,
        // because mojoc derives its module namespace from the -o stem.
        let rel_bin = std::path::Path::new(&package).join(&self.name);
        let rel_obj = std::path::Path::new(&package).join(format!("{}.lo", self.name));
        let outputs = vec![
            ctx.register_generated_artifact(&rel_bin, ArtifactKind::Generated, None),
            ctx.register_generated_artifact(&rel_obj, ArtifactKind::Generated, None),
        ];

        let _ = ctx.declare_incomplete_action(
            "MojoCompileBinary",
            inputs,
            outputs,
            "label srcs and transitive mojo imports resolve only in build()",
        );

        rush_rules::providers::ProviderSet::empty()
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        let sources = self.collect_sources(context)?;
        let main = self.select_main(&sources)?.to_path_buf();
        let (import_dirs, dependency_inputs) = collect_mojo_imports(context, &self.deps)?;
        let compiler = MojoLibraryInstance::find_mojo_compiler(
            &context.main_workspace_root,
        )?;
        let toolchain_root = compiler
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| anyhow!("invalid Mojo compiler path {}", compiler.display()))?;
        let lld = toolchain_root.join("bin").join("lld");
        if !lld.is_file() {
            return Err(anyhow!("Mojo linker is missing at {}", lld.display()));
        }

        let package = context.package_path.as_deref().ok_or_else(|| {
            anyhow!("mojo_binary {} has no package build context", self.name)
        })?;
        let output = context.output_dir.join(package).join(&self.name);
        let output_parent = output
            .parent()
            .ok_or_else(|| anyhow!("invalid Mojo output path {}", output.display()))?;
        std::fs::create_dir_all(output_parent).with_context(|| {
            format!("create Mojo output directory {}", output_parent.display())
        })?;
        // Bazel names the object `<target>.lo` by APPENDING (e.g.
        // `foo.mojo.test.lo`); with_extension would swallow the final
        // component (`foo.mojo.lo`). mojoc derives the module namespace
        // from the -o stem, so the exact name is semantic.
        let object = PathBuf::from(format!("{}.lo", output.display()));
        let target_triple = std::env::var("RUSH_MOJO_TARGET_TRIPLE")
            .unwrap_or_else(|_| "x86_64-unknown-linux-gnu".to_string());

        // Stage the declared sources into a per-target directory before
        // compiling. mojoc resolves imports path-relatively from the source
        // file's directory, so compiling in the workspace lets undeclared
        // sibling SOURCE packages shadow the precompiled .mojoc dependency
        // imports (Bazel never sees this because its sandbox stages only the
        // declared inputs). The staged layout mirrors the workspace-relative
        // paths and -strip-file-prefix removes the stage prefix, so embedded
        // source paths match Bazel's sandboxed compile.
        let workspace_relative = |path: &Path| -> PathBuf {
            path.strip_prefix(&context.workspace_root)
                .unwrap_or(path)
                .to_path_buf()
        };
        let absolutize = |path: &Path| -> PathBuf {
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                context.workspace_root.join(path)
            }
        };
        let stage_root = absolutize(
            &context
                .output_dir
                .join(".mojo-stage")
                .join(package)
                .join(&self.name),
        );
        if stage_root.exists() {
            std::fs::remove_dir_all(&stage_root).with_context(|| {
                format!("clear Mojo stage directory {}", stage_root.display())
            })?;
        }
        for source in &sources {
            let staged = stage_root.join(workspace_relative(source));
            if let Some(parent) = staged.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("create Mojo stage directory {}", parent.display())
                })?;
            }
            std::os::unix::fs::symlink(absolutize(source), &staged).with_context(
                || {
                    format!(
                        "stage Mojo source {} -> {}",
                        source.display(),
                        staged.display()
                    )
                },
            )?;
        }

        // Run from the stage root with a RELATIVE source path, exactly like
        // Bazel's sandboxed execution: mojoc treats an absolute source path
        // differently during declaration resolution, and at least one real
        // input (test_rms_norm_fused_residual.mojo) CRASHES the compiler in
        // the absolute form while compiling cleanly in the relative form.
        let mut args = vec![
            "build".to_string(),
            "-strip-file-prefix=.".to_string(),
            "--emit".to_string(),
            "object".to_string(),
            "-o".to_string(),
            absolutize(&object).to_string_lossy().into_owned(),
            "--lld-path".to_string(),
            lld.to_string_lossy().into_owned(),
            workspace_relative(&main).to_string_lossy().into_owned(),
        ];
        for source in &sources {
            if source != &main {
                let rel = workspace_relative(source);
                if let Some(parent) = rel.parent() {
                    args.push("-I".to_string());
                    args.push(parent.to_string_lossy().into_owned());
                }
            }
        }
        for import_dir in import_dirs {
            args.push("-I".to_string());
            args.push(absolutize(&import_dir).to_string_lossy().into_owned());
        }
        if let Some(accelerator) = std::env::var_os("RUSH_MOJO_TARGET_ACCELERATOR") {
            args.push(format!(
                "--target-accelerator={}",
                accelerator.to_string_lossy()
            ));
        } else if self.requires_gpu {
            return Err(anyhow!(
                "mojo_binary {} requires a GPU; set RUSH_MOJO_TARGET_ACCELERATOR (for example nvidia:sm_90a)",
                self.name
            ));
        }
        args.push(format!(
            "-target-triple={}",
            target_triple
        ));
        args.push(format!(
            "--target-cpu={}",
            std::env::var("RUSH_MOJO_TARGET_CPU").unwrap_or_else(|_| "x86-64-v3".to_string())
        ));
        for copt in &self.copts {
            args.push(expand_location_vars(
                "mojo_binary",
                &self.name,
                copt,
                context,
            )?);
        }
        if self.enable_assertions {
            args.push("-D".to_string());
            args.push("ASSERT=all".to_string());
        }

        let command = rush_action::Command {
            executable: compiler.clone(),
            args,
            working_dir: Some(stage_root.clone()),
            execution_info: rush_action::ExecutionInfo::default(),
        };
        let mut input_paths = sources;
        input_paths.extend(dependency_inputs);
        input_paths.extend(self.collect_additional_inputs(context));
        input_paths.extend(MojoLibraryInstance::collect_toolchain_inputs(&compiler));
        input_paths.sort();
        input_paths.dedup();
        let inputs = input_paths
            .into_iter()
            .map(rush_action::Artifact::Source)
            .collect::<Vec<_>>();
        let outputs = vec![rush_action::Artifact::Source(object.clone())];
        let env = HashMap::from([
            ("LC_CTYPE".to_string(), "C.UTF-8".to_string()),
            (
                "MODULAR_CRASH_REPORTING_ENABLED".to_string(),
                "false".to_string(),
            ),
            (
                "MODULAR_MOJO_MAX_COMPILERRT_PATH".to_string(),
                "/dev/null".to_string(),
            ),
            (
                "MODULAR_MOJO_MAX_LINKER_DRIVER".to_string(),
                "/dev/null".to_string(),
            ),
            (
                "MODULAR_MOJO_MAX_LLD_PATH".to_string(),
                "/dev/null".to_string(),
            ),
            (
                "MODULAR_TELEMETRY_ENABLED".to_string(),
                "false".to_string(),
            ),
            ("PATH".to_string(), "/dev/null".to_string()),
            ("TEST_TMPDIR".to_string(), ".".to_string()),
        ]);
        let id = rush_action::Action::compute_id(&inputs, &command, &env)?;
        let action = rush_action::Action {
            id,
            command,
            env,
            inputs,
            outputs,
            mnemonic: "MojoCompile".to_string(),
        };
        let result = context.execute_action(action).await?;
        if result.exit_code != 0 {
            return Err(anyhow!(
                "Mojo object compilation failed for {}: {}",
                self.name,
                String::from_utf8_lossy(&result.stderr)
            ));
        }
        // Phase 4: a virtual cache hit legitimately leaves the object
        // CAS-backed; the link action JIT-materializes it on a MISS.
        if !rush_action::virtual_artifacts::present_or_virtual(&object) {
            return Err(anyhow!(
                "Mojo compilation reported success but did not produce {}",
                object.display()
            ));
        }

        let cxx = Self::find_mojo_cxx(toolchain_root)?;
        let runtime_dir = toolchain_root.join("lib");
        let runtime_names = [
            "AsyncRTMojoBindings",
            "AsyncRTRuntimeGlobals",
            "KGENCompilerRTShared",
            "MSupportGlobals",
            "NVPTX",
        ];
        let runtime_paths = runtime_names
            .iter()
            .map(|name| runtime_dir.join(format!("lib{}.so", name)))
            .collect::<Vec<_>>();
        if let Some(missing) = runtime_paths.iter().find(|path| !path.is_file()) {
            return Err(anyhow!(
                "Mojo runtime library is missing at {}",
                missing.display()
            ));
        }

        let mut link_args = Vec::new();
        if let Some(sysroot) = Self::find_mojo_sysroot(toolchain_root) {
            link_args.push(format!("--sysroot={}", sysroot.display()));
        }
        if let Some(resource_dir) = Self::find_clang_resource_dir(&cxx) {
            link_args.push(format!("-resource-dir={}", resource_dir.display()));
        }
        link_args.extend([
            "-no-canonical-prefixes".to_string(),
            format!("--target={}", target_triple),
            "-gz".to_string(),
            "-fuse-ld=lld".to_string(),
            "-lm".to_string(),
            "-lstdc++".to_string(),
            "-Wl,--build-id=md5".to_string(),
            "-Wl,--hash-style=gnu".to_string(),
            "-Wl,-z,relro,-z,now".to_string(),
            "-Wl,--disable-new-dtags".to_string(),
            "-Wl,-z,nodelete".to_string(),
            "-Wl,-z,defs".to_string(),
            "-Wl,-pie".to_string(),
            "-Wl,-whole-archive".to_string(),
            object.to_string_lossy().into_owned(),
            "-Wl,-no-whole-archive".to_string(),
        ]);
        for name in runtime_names {
            link_args.push(format!("-l{}", name));
        }
        link_args.push(format!("-L{}", runtime_dir.display()));
        link_args.push(format!("-Wl,-rpath,{}", runtime_dir.display()));
        link_args.extend(self.linkopts.iter().cloned());
        link_args.push("-o".to_string());
        link_args.push(output.to_string_lossy().into_owned());

        let link_command = rush_action::Command {
            executable: cxx.clone(),
            args: link_args,
            working_dir: None,
            execution_info: rush_action::ExecutionInfo::default(),
        };
        let mut link_input_paths = vec![object.clone(), cxx.clone()];
        let clang_lld = cxx
            .parent()
            .map(|parent| parent.join("ld.lld"))
            .filter(|path| path.is_file());
        if let Some(path) = clang_lld {
            link_input_paths.push(path);
        }
        link_input_paths.extend(runtime_paths);
        link_input_paths.sort();
        link_input_paths.dedup();
        let link_inputs = link_input_paths
            .into_iter()
            .map(rush_action::Artifact::Source)
            .collect::<Vec<_>>();
        let link_outputs = vec![rush_action::Artifact::Source(output.clone())];
        let link_env = HashMap::from([
            ("LC_CTYPE".to_string(), "C.UTF-8".to_string()),
            (
                "MODULAR_CRASH_REPORTING_ENABLED".to_string(),
                "false".to_string(),
            ),
            (
                "MODULAR_TELEMETRY_ENABLED".to_string(),
                "false".to_string(),
            ),
            (
                "PATH".to_string(),
                "/bin:/usr/bin:/sbin:/usr/sbin".to_string(),
            ),
            ("ZERO_AR_DATE".to_string(), "1".to_string()),
        ]);
        let link_id =
            rush_action::Action::compute_id(&link_inputs, &link_command, &link_env)?;
        let link_action = rush_action::Action {
            id: link_id,
            command: link_command,
            env: link_env,
            inputs: link_inputs,
            outputs: link_outputs,
            mnemonic: "CppLink".to_string(),
        };
        let link_result = context.execute_action(link_action).await?;
        if link_result.exit_code != 0 {
            return Err(anyhow!(
                "Mojo link failed for {}: {}",
                self.name,
                String::from_utf8_lossy(&link_result.stderr)
            ));
        }
        // Phase 4: a virtual cache hit legitimately leaves the binary
        // CAS-backed; requested targets materialize at end of build.
        if !rush_action::virtual_artifacts::present_or_virtual(&output) {
            return Err(anyhow!(
                "Mojo link reported success but did not produce {}",
                output.display()
            ));
        }

        Ok(vec![Artifact {
            path: output,
            artifact_type: ArtifactType::Executable,
        }])
    }

    fn providers(&self) -> rush_rules::providers::ProviderSet {
        rush_rules::providers::ProviderSet::empty()
    }

    fn rule_type(&self) -> &str {
        "mojo_binary"
    }
}

#[derive(Debug)]
struct MlirOpGenerationInstance {
    name: String,
    template: String,
    element_type: String,
    output_type: String,
    platform: String,
    out: String,
}

impl MlirOpGenerationInstance {
    fn command(
        &self,
        input: &std::path::Path,
        output: &std::path::Path,
    ) -> (std::path::PathBuf, Vec<String>) {
        const SCRIPT: &str = r#"from pathlib import Path
import sys
src, dst, platform, elem_dtype, elem_type, output_dtype, output_type, scalar_type = sys.argv[1:]
text = Path(src).read_text(encoding="utf-8")
for old, new in (
    ("platform", platform),
    ("_elem_type", elem_dtype),
    ("elem_type", elem_type),
    ("_output_type", output_dtype),
    ("output_type", output_type),
    ("scalar_type", scalar_type),
):
    text = text.replace(old, new)
out = Path(dst)
out.parent.mkdir(parents=True, exist_ok=True)
out.write_text(text, encoding="utf-8")
"#;

        let element_dtype = tensorflow_dtype(&self.element_type)
            .expect("TensorFlow MLIR element type was validated by legacy build");
        let output_dtype = tensorflow_dtype(&self.output_type)
            .expect("TensorFlow MLIR output type was validated by legacy build");
        (
            std::path::PathBuf::from("python3"),
            vec![
                "-c".to_string(),
                SCRIPT.to_string(),
                input.display().to_string(),
                output.display().to_string(),
                self.platform.to_ascii_uppercase(),
                format!("_{element_dtype}"),
                mlir_type(&self.element_type).to_string(),
                format!("_{output_dtype}"),
                mlir_type(&self.output_type).to_string(),
                scalar_type(&self.element_type).to_string(),
            ],
        )
    }
}

fn mlir_type(value: &str) -> &str {
    match value {
        "c64" => "complex<f32>",
        "c128" => "complex<f64>",
        value => value,
    }
}

fn tensorflow_dtype(value: &str) -> Option<&'static str> {
    match value {
        "i1" => Some("DT_BOOL"),
        "i8" => Some("DT_INT8"),
        "i16" => Some("DT_INT16"),
        "i32" => Some("DT_INT32"),
        "i64" => Some("DT_INT64"),
        "ui8" => Some("DT_UINT8"),
        "ui16" => Some("DT_UINT16"),
        "ui32" => Some("DT_UINT32"),
        "ui64" => Some("DT_UINT64"),
        "f16" => Some("DT_HALF"),
        "f32" => Some("DT_FLOAT"),
        "f64" => Some("DT_DOUBLE"),
        "c64" => Some("DT_COMPLEX64"),
        "c128" => Some("DT_COMPLEX128"),
        _ => None,
    }
}

fn scalar_type(value: &str) -> &str {
    match value {
        "c64" => "f32",
        "c128" => "f64",
        value => value,
    }
}

#[async_trait]
impl RuleInstance for MlirOpGenerationInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        Vec::new()
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        let input = context.source_path(&self.template);
        let output = context.target_output_dir(&self.name).join(&self.out);
        if let Some(parent) = output.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let element_dtype = tensorflow_dtype(&self.element_type).ok_or_else(|| {
            anyhow::anyhow!("unsupported TensorFlow MLIR element type {}", self.element_type)
        })?;
        let output_dtype = tensorflow_dtype(&self.output_type).ok_or_else(|| {
            anyhow::anyhow!("unsupported TensorFlow MLIR output type {}", self.output_type)
        })?;
        let template = tokio::fs::read_to_string(&input).await?;
        let generated = template
            .replace("platform", &self.platform.to_ascii_uppercase())
            .replace("_elem_type", &format!("_{element_dtype}"))
            .replace("elem_type", mlir_type(&self.element_type))
            .replace("_output_type", &format!("_{output_dtype}"))
            .replace("output_type", mlir_type(&self.output_type))
            .replace("scalar_type", scalar_type(&self.element_type));
        tokio::fs::write(&output, generated).await?;

        Ok(vec![Artifact {
            path: output,
            artifact_type: rush_rules::ArtifactType::Generated,
        }])
    }

    fn analyze(
        &self,
        ctx: &mut rush_rules::analysis_context::AnalysisContext,
        _dep_providers: &[std::sync::Arc<rush_rules::providers::ProviderSet>],
    ) -> rush_rules::providers::ProviderSet {
        use rush_action::ids::{ActionEnvPolicy, ArtifactKind, ExecutableAction};
        use rush_rules::providers::{DefaultInfo, FilesInfo, ProviderSet};

        let input_path = {
            let input_id =
                ctx.resolve_package_include(std::path::Path::new(&self.template));
            ctx.registry().paths().path(input_id).to_path_buf()
        };
        let input = ctx.register_source_artifact(&input_path, ArtifactKind::Source);
        let relative = ctx.target_output_rel(&self.name, &self.out);
        let output = ctx.register_generated_artifact(
            std::path::Path::new(&relative),
            ArtifactKind::Generated,
            None,
        );
        let output_path = ctx.predicted_output_path(std::path::Path::new(&relative));
        let (executable, argv_strings) = self.command(&input_path, &output_path);
        let action = ctx
            .declare_action("TensorFlowMlirOp", vec![input], vec![output])
            .expect("declare TensorFlow MLIR template action");
        let executable = ctx.intern_path(&executable);
        let argv = argv_strings
            .iter()
            .map(|arg| ctx.intern_string(arg))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        ctx.set_executable_action(
            action,
            ExecutableAction {
                executable,
                argv,
                env: Box::new([]),
                working_dir: None,
                env_policy: ActionEnvPolicy::InheritHost,
            },
        );

        ProviderSet::empty()
            .with_files(FilesInfo::new(vec![output_path]))
            .with_default_info(DefaultInfo {
                files: vec![output],
            })
    }

    async fn build_legacy_command_specs(
        &self,
        context: &BuildContext,
    ) -> Vec<(
        rush_rules::SchedulerActionKey,
        rush_action::command_spec::NormalizedCommandSpec,
    )> {
        let input = context.source_path(&self.template);
        let output = context.target_output_dir(&self.name).join(&self.out);
        let (executable, argv) = self.command(&input, &output);
        let mut spec = rush_action::command_spec::NormalizedCommandSpec {
            mnemonic: "TensorFlowMlirOp".to_string(),
            executable,
            cwd: None,
            argv,
            env: Vec::new(),
            declared_inputs: vec![input],
            declared_outputs: vec![output.clone()],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: rush_action::ids::ActionEnvPolicy::InheritHost,
        };
        spec.normalize();
        vec![(
            rush_rules::SchedulerActionKey {
                mnemonic: "TensorFlowMlirOp".to_string(),
                primary_output_path: output,
            },
            spec,
        )]
    }

    fn rule_type(&self) -> &str {
        "tensorflow_mlir_op"
    }
}

const HLO_TO_KERNEL: &str = "//tensorflow/compiler/mlir/tools/kernel_gen:hlo_to_kernel";
const TENSORFLOW_FRAMEWORK: &str = "//tensorflow:libtensorflow_framework.so.2";

#[derive(Debug)]
struct MlirGpuKernelInstance {
    name: String,
    mlir_op: String,
    data_type: String,
    tile_size: String,
    unroll_factors: Option<String>,
    max_supported_rank: i64,
    host_triple: String,
    gpu_archs: Vec<String>,
    jit: bool,
    jit_i64_indexed_for_large_tensors: bool,
    extra_args: Vec<String>,
}

impl MlirGpuKernelInstance {
    fn mlir_target(&self) -> String {
        let file = self.mlir_op.rsplit(':').next().unwrap_or(&self.mlir_op);
        format!(
            ":generate_{}_mlir",
            file.strip_suffix(".mlir").unwrap_or(file)
        )
    }

    fn output_name(&self) -> String {
        format!("{}_kernel.o", self.name)
    }

    fn command(
        &self,
        tool: &std::path::Path,
        input: &std::path::Path,
        output: &std::path::Path,
    ) -> (std::path::PathBuf, Vec<String>) {
        let mut argv = self.extra_args.clone();
        if let Some(unroll_factors) = self
            .unroll_factors
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            argv.push(format!("--unroll_factors={unroll_factors}"));
        }
        argv.extend([
            format!("--tile_sizes={}", self.tile_size),
            format!("--max-supported-rank={}", self.max_supported_rank),
            format!("--host-triple={}", self.host_triple),
            format!("--arch={}", self.gpu_archs.join(",")),
            format!("--input={}", input.display()),
            format!("--output={}", output.display()),
            format!("--enable_ftz={}", self.data_type == "f32"),
            format!(
                "--jit_i64_indexed_for_large_tensors={}",
                self.jit_i64_indexed_for_large_tensors
            ),
            format!("--jit={}", self.jit),
        ]);
        (tool.to_path_buf(), argv)
    }

    fn library_path(framework: &std::path::Path) -> Option<std::ffi::OsString> {
        let mut library_path = framework.parent()?.as_os_str().to_owned();
        if let Some(existing) = std::env::var_os("LD_LIBRARY_PATH") {
            library_path.push(":");
            library_path.push(existing);
        }
        Some(library_path)
    }
}

#[async_trait]
impl RuleInstance for MlirGpuKernelInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        vec![
            self.mlir_target(),
            HLO_TO_KERNEL.to_owned(),
            TENSORFLOW_FRAMEWORK.to_owned(),
        ]
    }

    fn exec_only_dependencies(&self) -> Vec<String> {
        vec![HLO_TO_KERNEL.to_owned(), TENSORFLOW_FRAMEWORK.to_owned()]
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        let mlir_target = self.mlir_target();
        let input = context
            .direct_artifacts(&mlir_target)
            .and_then(|artifacts| {
                artifacts.iter().find(|artifact| {
                    artifact.path.extension().and_then(|ext| ext.to_str()) == Some("mlir")
                })
            })
            .map(|artifact| artifact.path.clone())
            .ok_or_else(|| anyhow::anyhow!("{} produced no MLIR input", mlir_target))?;
        let tool = context
            .direct_artifacts(HLO_TO_KERNEL)
            .and_then(|artifacts| {
                artifacts.iter().find(|artifact| {
                    artifact.artifact_type == rush_rules::ArtifactType::Executable
                })
            })
            .map(|artifact| artifact.path.clone())
            .ok_or_else(|| anyhow::anyhow!("{HLO_TO_KERNEL} produced no executable"))?;
        let output = context
            .target_output_dir(&self.name)
            .join(self.output_name());
        if let Some(parent) = output.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let (executable, argv) = self.command(&tool, &input, &output);
        let mut command = tokio::process::Command::new(executable);
        command.args(argv);

        if let Some(framework) = context
            .direct_artifacts(TENSORFLOW_FRAMEWORK)
            .and_then(|artifacts| artifacts.first())
            .map(|artifact| artifact.path.as_path())
        {
            if let Some(library_path) = Self::library_path(framework) {
                command.env("LD_LIBRARY_PATH", library_path);
            }
        }

        let result = command.output().await?;
        if !result.status.success() {
            anyhow::bail!(
                "hlo_to_kernel failed for {}:\nstdout:\n{}\nstderr:\n{}",
                self.name,
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
        }

        Ok(vec![Artifact {
            path: output,
            artifact_type: rush_rules::ArtifactType::Object,
        }])
    }

    fn analyze(
        &self,
        ctx: &mut rush_rules::analysis_context::AnalysisContext,
        dep_providers: &[std::sync::Arc<rush_rules::providers::ProviderSet>],
    ) -> rush_rules::providers::ProviderSet {
        use rush_action::ids::{ActionEnvPolicy, ArtifactKind, ExecutableAction};

        let input = dep_providers
            .iter()
            .filter_map(|providers| providers.get_default_info())
            .flat_map(|info| info.files.iter().copied())
            .find(|artifact| {
                ctx.registry()
                    .artifact_path(*artifact)
                    .extension()
                    .and_then(|extension| extension.to_str())
                    == Some("mlir")
            })
            .expect("TensorFlow MLIR GPU kernel input");
        let input_path = ctx.registry().artifact_path(input).to_path_buf();
        let tool_relative = std::path::Path::new("hlo_to_kernel");
        let tool = ctx.register_generated_artifact(
            tool_relative,
            ArtifactKind::Executable,
            None,
        );
        let tool_path = ctx.predicted_output_path(tool_relative);
        let framework_relative =
            std::path::Path::new("tensorflow_framework/libtensorflow_framework.so.2");
        let framework = ctx.register_generated_artifact(
            framework_relative,
            ArtifactKind::SharedLibrary,
            None,
        );
        let framework_path = ctx.predicted_output_path(framework_relative);
        let output_name = self.output_name();
        let relative = ctx.target_output_rel(&self.name, &output_name);
        let output = ctx.register_generated_artifact(
            std::path::Path::new(&relative),
            ArtifactKind::Object,
            None,
        );
        let output_path = ctx.predicted_output_path(std::path::Path::new(&relative));
        let linker_input = ctx.register_linker_input(output, false);
        let (executable, argv_strings) = self.command(&tool_path, &input_path, &output_path);
        let action = ctx
            .declare_action(
                "TensorFlowMlirGpuKernel",
                vec![input, tool, framework],
                vec![output],
            )
            .expect("declare TensorFlow MLIR GPU kernel action");
        let executable = ctx.intern_path(&executable);
        let argv = argv_strings
            .iter()
            .map(|arg| ctx.intern_string(arg))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let env = Self::library_path(&framework_path)
            .map(|library_path| {
                let name = ctx.intern_string("LD_LIBRARY_PATH");
                let value = ctx.intern_string(&library_path.to_string_lossy());
                vec![(name, value)].into_boxed_slice()
            })
            .unwrap_or_default();
        ctx.set_executable_action(
            action,
            ExecutableAction {
                executable,
                argv,
                env,
                working_dir: None,
                env_policy: ActionEnvPolicy::InheritHost,
            },
        );

        let direct = rush_rules::providers::DirectCcInfo {
            linker_inputs: vec![linker_input],
            ..Default::default()
        };
        let cc_info = rush_rules::providers::CcInfo::from_direct_and_deps(
            direct,
            rush_rules::providers::extract_cc_infos(dep_providers).into_iter(),
        );
        rush_rules::providers::ProviderSet::empty().with_cc_info(cc_info)
    }

    async fn build_legacy_command_specs(
        &self,
        context: &BuildContext,
    ) -> Vec<(
        rush_rules::SchedulerActionKey,
        rush_action::command_spec::NormalizedCommandSpec,
    )> {
        let mlir_target = self.mlir_target();
        let Some(input) = context
            .direct_artifacts(&mlir_target)
            .and_then(|artifacts| {
                artifacts.iter().find(|artifact| {
                    artifact.path.extension().and_then(|extension| extension.to_str())
                        == Some("mlir")
                })
            })
            .map(|artifact| artifact.path.clone())
        else {
            return Vec::new();
        };
        let Some(tool) = context
            .direct_artifacts(HLO_TO_KERNEL)
            .and_then(|artifacts| {
                artifacts.iter().find(|artifact| {
                    artifact.artifact_type == rush_rules::ArtifactType::Executable
                })
            })
            .map(|artifact| artifact.path.clone())
        else {
            return Vec::new();
        };
        let Some(framework) = context
            .direct_artifacts(TENSORFLOW_FRAMEWORK)
            .and_then(|artifacts| artifacts.first())
            .map(|artifact| artifact.path.clone())
        else {
            return Vec::new();
        };
        let output = context
            .target_output_dir(&self.name)
            .join(self.output_name());
        let (executable, argv) = self.command(&tool, &input, &output);
        let env = Self::library_path(&framework)
            .map(|library_path| {
                vec![(
                    "LD_LIBRARY_PATH".to_string(),
                    library_path.to_string_lossy().into_owned(),
                )]
            })
            .unwrap_or_default();
        let mut spec = rush_action::command_spec::NormalizedCommandSpec {
            mnemonic: "TensorFlowMlirGpuKernel".to_string(),
            executable,
            cwd: None,
            argv,
            env,
            declared_inputs: vec![input, tool, framework],
            declared_outputs: vec![output.clone()],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: rush_action::ids::ActionEnvPolicy::InheritHost,
        };
        spec.normalize();
        vec![(
            rush_rules::SchedulerActionKey {
                mnemonic: "TensorFlowMlirGpuKernel".to_string(),
                primary_output_path: output,
            },
            spec,
        )]
    }

    fn rule_type(&self) -> &str {
        "tensorflow_mlir_gpu_kernel"
    }
}

pub struct CustomRuleInstance {
    name: String,
    /// Precomputed dependency labels — the only attr this graph-only stub
    /// consumes. See `extract_deps` / the #39 note in `parse_attributes`.
    deps: Vec<String>,
}

#[async_trait]
impl RuleInstance for CustomRuleInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        self.deps.clone()
    }

    async fn build(&self, _context: &BuildContext) -> Result<Vec<Artifact>> {
        // This is correct behavior: rush doesn't evaluate Starlark
        // `rule(implementation=...)` yet, so this handler tracks the
        // target in the dep graph without producing artifacts. If a
        // consumer fails to find an expected output, the fix is to
        // build the rule() evaluator (issue #183) — not to fake the
        // missing artifact. See UNIMPLEMENTED.md §5j.
        Ok(Vec::new())
    }

    fn providers(&self) -> rush_rules::providers::ProviderSet {
        rush_rules::providers::ProviderSet::empty()
    }

    fn rule_type(&self) -> &str {
        "custom_rule_stub"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mojo_compiler_defaults_to_workspace_cache() {
        let temp = tempfile::tempdir().unwrap();
        let compiler = temp
            .path()
            .join(".cache/rush/bazel_output_base/external")
            .join("rules_mojo++mojo+mojo_toolchain_linux_x86_64")
            .join("bin/mojo");
        std::fs::create_dir_all(compiler.parent().unwrap()).unwrap();
        std::fs::write(&compiler, b"compiler").unwrap();

        assert_eq!(
            MojoLibraryInstance::find_mojo_compiler_with_roots(
                temp.path(),
                None,
                None,
            )
            .unwrap(),
            compiler
        );
    }

    #[test]
    fn parse_attributes_keeps_only_deps_not_full_attrs() {
        // #39 graph-RSS: parse_attributes must precompute deps and NOT retain
        // the whole attr map. Verify deps are extracted correctly and that a
        // large unrelated attr (the kind that used to bloat every instance)
        // does not affect behavior.
        let mut attrs: HashMap<String, Value> = HashMap::new();
        attrs.insert("name".to_string(), Value::String("widget".to_string()));
        attrs.insert(
            "deps".to_string(),
            Value::List(vec![
                Value::String("//a:b".to_string()),
                Value::String("//c:d".to_string()),
            ]),
        );
        // A big dead-weight attr (e.g. a stringified dict) — must not matter.
        attrs.insert(
            "big".to_string(),
            Value::String("x".repeat(100_000)),
        );
        let inst = CustomRuleStub.parse_attributes(&attrs).unwrap();
        assert_eq!(inst.target_name(), "widget");
        assert_eq!(inst.dependencies(), vec!["//a:b".to_string(), "//c:d".to_string()]);
    }

    #[test]
    fn parse_attributes_no_deps_is_empty() {
        let mut attrs: HashMap<String, Value> = HashMap::new();
        attrs.insert("name".to_string(), Value::String("n".to_string()));
        let inst = CustomRuleStub.parse_attributes(&attrs).unwrap();
        assert!(inst.dependencies().is_empty());
    }
}
