// Python build rules.
//
// Real py_library / py_binary / py_test execution: hermetic rules_python
// interpreters, Bazel `imports` semantics, and pip dependencies provided by
// pycross wheel libraries. The launcher a py_binary produces is the
// build-phase contract (what genrule `$(location <tool>)` invokes and what
// `bazel run` would exec); test *running* is a separate phase.
//
// PyInfo is carried as a CustomProvider named "PyInfo":
//   data["imports"]    = ':'-joined import roots: main-workspace providers
//                        use workspace-relative paths, while external providers
//                        use absolute repository roots so consumers cannot
//                        reinterpret them relative to another repository
//   data["site_roots"] = ':'-joined ABSOLUTE site-packages roots from
//                        pycross wheel libraries (transitive)

use crate::context::BuildContext;
use crate::providers::{CustomProvider, ProviderSet};
use crate::{
    extract_string, extract_string_list, Artifact, ArtifactType, BuildRule, RuleInstance, Value,
};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Workspace-relative import roots for a target's `imports` attribute.
/// Bazel semantics: each entry is package-relative; "." is the package dir
/// itself, ".." climbs toward the repo root.
pub fn py_import_roots(package: &str, imports: &[String]) -> Vec<String> {
    let mut roots = Vec::new();
    for entry in imports {
        let mut parts: Vec<&str> = if package.is_empty() {
            Vec::new()
        } else {
            package.split('/').collect()
        };
        for comp in entry.split('/') {
            match comp {
                "" | "." => {}
                ".." => {
                    parts.pop();
                }
                other => parts.push(other),
            }
        }
        let root = parts.join("/");
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots
}

fn analysis_import_roots(
    ctx: &crate::analysis_context::AnalysisContext<'_>,
    imports: &[String],
) -> Vec<String> {
    let roots = py_import_roots(ctx.package(), imports);
    if ctx.workspace().is_some() {
        roots
            .into_iter()
            .map(|root| {
                if root.is_empty() {
                    ctx.workspace_root().display().to_string()
                } else {
                    ctx.workspace_root().join(root).display().to_string()
                }
            })
            .collect()
    } else {
        roots
    }
}

/// Encode a PyInfo custom provider.
pub fn py_info(imports: &[String], site_roots: &[String]) -> CustomProvider {
    let mut data = HashMap::new();
    data.insert("imports".to_string(), imports.join(":"));
    data.insert("site_roots".to_string(), site_roots.join(":"));
    CustomProvider::new("PyInfo".to_string(), data)
}

fn decode_list(provider: &CustomProvider, key: &str) -> Vec<String> {
    provider
        .data
        .get(key)
        .map(|joined| {
            joined
                .split(':')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Union dep PyInfo providers into (imports, site_roots).
fn union_dep_py_info(
    dep_providers: &[std::sync::Arc<ProviderSet>],
    imports: &mut Vec<String>,
    site_roots: &mut Vec<String>,
) {
    for provs in dep_providers {
        if let Some(info) = provs.get_custom("PyInfo") {
            for root in decode_list(info, "imports") {
                if !imports.contains(&root) {
                    imports.push(root);
                }
            }
            for root in decode_list(info, "site_roots") {
                if !site_roots.contains(&root) {
                    site_roots.push(root);
                }
            }
        }
    }
}

fn is_label(s: &str) -> bool {
    s.starts_with(':') || s.starts_with("//") || s.starts_with('@')
}

/// srcs may mix package-relative filenames and labels (filegroups,
/// generated files). Labels become graph dependencies.
fn label_srcs(srcs: &[String]) -> Vec<String> {
    srcs.iter().filter(|s| is_label(s)).cloned().collect()
}

/// Resolve every source file for a target: plain srcs from the source tree,
/// label srcs through their built artifacts.
fn resolve_source_files(
    context: &BuildContext,
    srcs: &[String],
) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for src in srcs {
        if is_label(src) {
            let artifacts = context.iter_transitive_artifacts(src).ok_or_else(|| {
                anyhow!("py rule: cannot resolve src label {}", src)
            })?;
            for artifact in artifacts {
                if !files.contains(&artifact.path) {
                    files.push(artifact.path.clone());
                }
            }
        } else {
            let path = context.source_path(src);
            if !files.contains(&path) {
                files.push(path);
            }
        }
    }
    Ok(files)
}

/// PYTHONPATH assembly shared by py_binary and py_test, mirroring the
/// rules_python bootstrap stub's ordering: runfiles-root analogue first,
/// then import roots, then pip site roots, then external-repo roots
/// derived from dep artifacts (the import_all analogue).
fn assemble_pythonpath(
    context: &BuildContext,
    own_imports: &[String],
    deps: &[String],
) -> Vec<String> {
    let ws = &context.workspace_root;
    let mut entries: Vec<String> = vec![ws.display().to_string()];
    let mut push = |e: String, entries: &mut Vec<String>| {
        if !entries.contains(&e) {
            entries.push(e);
        }
    };

    for root in own_imports {
        push(ws.join(root).display().to_string(), &mut entries);
    }

    for dep in deps {
        if let Some(provs) = context.get_dependency_providers(dep) {
            if let Some(info) = provs.get_custom("PyInfo") {
                for root in decode_list(info, "imports") {
                    push(ws.join(&root).display().to_string(), &mut entries);
                }
                for root in decode_list(info, "site_roots") {
                    push(root, &mut entries);
                }
                continue;
            }
        }
        // No PyInfo (external py repos like absl_py, or providers lost
        // through wrapper rules): derive roots from artifact shape. A
        // site-packages ancestor wins; otherwise strip the dep label's
        // package depth to reach the external-repo import root.
        if let Some(artifacts) = context.direct_artifacts(dep) {
            let pkg_depth = dep
                .split("//")
                .nth(1)
                .map(|after| after.split(':').next().unwrap_or(after))
                .map(|pkg| {
                    if pkg.is_empty() {
                        0
                    } else {
                        pkg.matches('/').count() + 1
                    }
                })
                .unwrap_or(0);
            for artifact in artifacts {
                if let Some(site) = artifact
                    .path
                    .ancestors()
                    .find(|a| a.file_name().map(|n| n == "site-packages").unwrap_or(false))
                {
                    push(site.display().to_string(), &mut entries);
                    continue;
                }
                if artifact.path.extension().and_then(|e| e.to_str()) != Some("py") {
                    continue;
                }
                if let Some(parent) = artifact.path.parent() {
                    let mut root = parent;
                    for _ in 0..pkg_depth {
                        match root.parent() {
                            Some(p) => root = p,
                            None => break,
                        }
                    }
                    push(root.display().to_string(), &mut entries);
                    if dep.starts_with('@') {
                        if let Some(external_root) = root.parent() {
                            push(external_root.display().to_string(), &mut entries);
                        }
                    }
                }
            }
        }
    }
    entries
}

fn write_launcher(
    context: &BuildContext,
    name: &str,
    main_path: &Path,
    pythonpath: &[String],
) -> Result<PathBuf> {
    let package = context
        .package_path
        .as_deref()
        .unwrap_or_else(|| Path::new(""));
    let out_dir = context.output_dir.join(package);
    let wrapper_path = out_dir.join(name);
    // Target names may be slash-qualified (py_test name "tests/test_x").
    if let Some(parent) = wrapper_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Launchers execute ABI-tagged extension modules from selected wheels.
    // Never substitute another materialized Python minor version here: a
    // wrapper that combines cp313 wheels with Python 3.12 is a false green.
    let python = crate::python_runtime::find_python3(
        &context.main_workspace_root,
        &default_py_version(),
    )?;

    // PREPEND hermetic entries so a polluted host PYTHONPATH cannot shadow
    // hermetic deps (the rules_python stub puts runfiles entries first).
    let wrapper_content = format!(
        r#"#!/bin/bash
# rush py launcher for {name}
export PYTHONPATH="{pp}${{PYTHONPATH:+:$PYTHONPATH}}"
exec "{python}" "{main}" "$@"
"#,
        name = name,
        pp = pythonpath.join(":"),
        python = python.display(),
        main = main_path.display(),
    );
    std::fs::write(&wrapper_path, wrapper_content)?;
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(&wrapper_path)?.permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&wrapper_path, perms)?;
    Ok(wrapper_path)
}

/// Default hermetic Python version (underscored form). Bazel's default
/// comes from `python.defaults` in the module graph; rush reads
/// RUSH_PY_DEFAULT_VERSION (accepts "3.13" or "3_13") with a 3_13
/// fallback matching Modular's DEFAULT_PYTHON_VERSION.
pub fn default_py_version() -> String {
    std::env::var("RUSH_PY_DEFAULT_VERSION")
        .map(|v| v.replace('.', "_"))
        .unwrap_or_else(|_| "3_13".to_string())
}

// ============================================================================
// py_library
// ============================================================================

pub struct PyLibraryRule;

impl BuildRule for PyLibraryRule {
    fn name(&self) -> &str {
        "py_library"
    }

    fn parse_attributes(
        &self,
        attrs: &HashMap<String, crate::Value>,
    ) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let srcs = extract_string_list(attrs, "srcs").unwrap_or_default();
        let deps = extract_string_list(attrs, "deps").unwrap_or_default();
        let imports = extract_string_list(attrs, "imports").unwrap_or_default();
        let data = extract_string_list(attrs, "data").unwrap_or_default();

        Ok(Box::new(PyLibraryInstance {
            name,
            srcs,
            deps,
            imports,
            data,
        }))
    }
}

pub struct PyLibraryInstance {
    name: String,
    srcs: Vec<String>,
    deps: Vec<String>,
    imports: Vec<String>,
    /// Runtime files and targets. Every entry is a Bazel label attribute,
    /// including bare same-package spellings such as `extension.so`.
    data: Vec<String>,
}

#[async_trait]
impl RuleInstance for PyLibraryInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn rule_type(&self) -> &str {
        "py_library"
    }

    fn dependencies(&self) -> Vec<String> {
        let mut deps = self.deps.clone();
        for dep in label_srcs(&self.srcs)
            .into_iter()
            .chain(self.data.iter().cloned())
        {
            if !deps.contains(&dep) {
                deps.push(dep);
            }
        }
        deps
    }

    fn analyze(
        &self,
        ctx: &mut crate::analysis_context::AnalysisContext,
        _dep_providers: &[std::sync::Arc<ProviderSet>],
    ) -> ProviderSet {
        let mut imports = analysis_import_roots(ctx, &self.imports);
        let mut site_roots = Vec::new();
        // `data` creates build-order edges but does not export PyInfo into this
        // library. Select only providers named by the actual `deps`
        // attribute; the flat dep_providers slice also contains label-valued
        // srcs and data entries.
        let py_deps: Vec<_> = self
            .deps
            .iter()
            .filter_map(|dep| ctx.get_dependency_provider(dep).cloned())
            .collect();
        union_dep_py_info(&py_deps, &mut imports, &mut site_roots);
        ProviderSet::empty().with_custom(py_info(&imports, &site_roots))
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        Ok(resolve_source_files(context, &self.srcs)?
            .into_iter()
            .map(|path| Artifact {
                path,
                artifact_type: ArtifactType::Source,
            })
            .collect())
    }

    fn providers(&self) -> ProviderSet {
        ProviderSet::empty()
    }
}

// ============================================================================
// py_binary / py_test — shared executable core
// ============================================================================

fn parse_py_executable(
    kind: &'static str,
    attrs: &HashMap<String, Value>,
) -> Result<PyExecutableInstance> {
    let name = extract_string(attrs, "name")?;
    let srcs = extract_string_list(attrs, "srcs").unwrap_or_default();
    let deps = extract_string_list(attrs, "deps").unwrap_or_default();
    let imports = extract_string_list(attrs, "imports").unwrap_or_default();
    let data = extract_string_list(attrs, "data").unwrap_or_default();
    let main = extract_string(attrs, "main").ok().filter(|m| m != "None");

    Ok(PyExecutableInstance {
        kind,
        name,
        srcs,
        deps,
        imports,
        data,
        main,
    })
}

pub struct PyExecutableInstance {
    kind: &'static str,
    name: String,
    srcs: Vec<String>,
    deps: Vec<String>,
    imports: Vec<String>,
    data: Vec<String>,
    main: Option<String>,
}

impl PyExecutableInstance {
    fn find_main(&self, source_files: &[PathBuf]) -> Result<PathBuf> {
        let main_file_name = self
            .main
            .as_ref()
            .map(|m| {
                // main may itself be a label or path; match by file name.
                m.rsplit(&[':', '/'][..]).next().unwrap_or(m).to_string()
            })
            .unwrap_or_else(|| format!("{}.py", self.name));
        source_files
            .iter()
            .find(|p| {
                p.file_name().map(|n| n.to_string_lossy() == main_file_name.as_str())
                    == Some(true)
            })
            .or_else(|| source_files.first())
            .cloned()
            .ok_or_else(|| {
                anyhow!("py executable {} has no sources", self.name)
            })
    }
}

#[async_trait]
impl RuleInstance for PyExecutableInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn rule_type(&self) -> &str {
        self.kind
    }

    fn dependencies(&self) -> Vec<String> {
        let mut deps = self.deps.clone();
        deps.extend(label_srcs(&self.srcs));
        deps.extend(self.data.iter().cloned());
        deps
    }

    fn analyze(
        &self,
        ctx: &mut crate::analysis_context::AnalysisContext,
        dep_providers: &[std::sync::Arc<ProviderSet>],
    ) -> ProviderSet {
        let mut imports = analysis_import_roots(ctx, &self.imports);
        let mut site_roots = Vec::new();
        union_dep_py_info(dep_providers, &mut imports, &mut site_roots);
        ProviderSet::empty().with_custom(py_info(&imports, &site_roots))
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        let source_files = resolve_source_files(context, &self.srcs)?;
        let main_path = self.find_main(&source_files)?;

        let package = context
            .package_path
            .as_deref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let own_imports = py_import_roots(&package, &self.imports);
        let pythonpath = assemble_pythonpath(context, &own_imports, &self.deps);

        let wrapper_path =
            write_launcher(context, &self.name, &main_path, &pythonpath)?;

        let mut artifacts = vec![Artifact {
            path: wrapper_path,
            artifact_type: ArtifactType::Executable,
        }];
        for path in source_files {
            artifacts.push(Artifact {
                path,
                artifact_type: ArtifactType::Source,
            });
        }
        Ok(artifacts)
    }

    fn providers(&self) -> ProviderSet {
        ProviderSet::empty()
    }
}

pub struct PyBinaryRule;

impl BuildRule for PyBinaryRule {
    fn name(&self) -> &str {
        "py_binary"
    }

    fn parse_attributes(
        &self,
        attrs: &HashMap<String, crate::Value>,
    ) -> Result<Box<dyn RuleInstance>> {
        Ok(Box::new(parse_py_executable("py_binary", attrs)?))
    }
}

pub struct PyTestRule;

impl BuildRule for PyTestRule {
    fn name(&self) -> &str {
        "py_test"
    }

    fn parse_attributes(
        &self,
        attrs: &HashMap<String, crate::Value>,
    ) -> Result<Box<dyn RuleInstance>> {
        Ok(Box::new(parse_py_executable("py_test", attrs)?))
    }
}

// Backwards-compatible aliases: earlier code paths referenced these names.
pub type PyBinaryInstance = PyExecutableInstance;
pub type PyTestInstance = PyExecutableInstance;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis_context::{AnalysisContext, StubIncludeResolver};
    use crate::context::Toolchain;
    use rush_action::ids::{ArtifactRegistry, ConfigId, TargetId};
    use std::sync::Arc;

    fn string_list(values: &[&str]) -> Value {
        Value::List(
            values
                .iter()
                .map(|value| Value::String((*value).to_string()))
                .collect(),
        )
    }

    #[test]
    fn py_library_data_entries_become_graph_dependencies() {
        let attrs = HashMap::from([
            ("name".to_string(), Value::String("binding".to_string())),
            ("srcs".to_string(), string_list(&[":generated.py", "api.py"])),
            ("deps".to_string(), string_list(&[":runtime", ":generated.py"])),
            (
                "data".to_string(),
                string_list(&["binding.so", "binding.pyi"]),
            ),
        ]);

        let instance = PyLibraryRule.parse_attributes(&attrs).unwrap();

        assert_eq!(
            instance.dependencies(),
            vec![
                ":runtime".to_string(),
                ":generated.py".to_string(),
                "binding.so".to_string(),
                "binding.pyi".to_string(),
            ]
        );
    }

    #[test]
    fn py_library_data_does_not_contribute_py_info() {
        let attrs = HashMap::from([
            ("name".to_string(), Value::String("binding".to_string())),
            ("deps".to_string(), string_list(&[":runtime"])),
            ("data".to_string(), string_list(&["binding.so"])),
        ]);
        let instance = PyLibraryRule.parse_attributes(&attrs).unwrap();
        let runtime = Arc::new(
            ProviderSet::empty().with_custom(py_info(
                &["runtime_import".to_string()],
                &["/runtime/site".to_string()],
            )),
        );
        let data = Arc::new(
            ProviderSet::empty().with_custom(py_info(
                &["data_must_not_leak".to_string()],
                &["/data/must/not/leak".to_string()],
            )),
        );
        let labeled = vec![
            ("//pkg:runtime".to_string(), runtime.clone()),
            ("//pkg:binding.so".to_string(), data.clone()),
        ];

        let workspace_root = PathBuf::from("/workspace");
        let output_dir = PathBuf::from("/output");
        let resolver = StubIncludeResolver::new(workspace_root.clone());
        let toolchain = Toolchain::default();
        let extra_paths = Vec::new();
        let mut registry = ArtifactRegistry::new();
        let mut ctx = AnalysisContext::new(
            TargetId(1),
            ConfigId::DEFAULT,
            "pkg",
            None,
            &output_dir,
            &mut registry,
            &resolver,
            &toolchain,
            &workspace_root,
            &extra_paths,
        )
        .with_labeled_dep_providers(&labeled);

        let providers = instance.analyze(&mut ctx, &[runtime, data]);
        let info = providers.get_custom("PyInfo").unwrap();

        assert_eq!(
            decode_list(info, "imports"),
            vec!["runtime_import".to_string()]
        );
        assert_eq!(
            decode_list(info, "site_roots"),
            vec!["/runtime/site".to_string()]
        );
    }

    #[test]
    fn external_py_library_exports_repository_stable_import_root() {
        let attrs = HashMap::from([
            ("name".to_string(), Value::String("binding".to_string())),
            ("imports".to_string(), string_list(&["."])),
        ]);
        let instance = PyLibraryRule.parse_attributes(&attrs).unwrap();

        let workspace_root = PathBuf::from("/external/module_platlib");
        let output_dir = PathBuf::from("/output/external/module_platlib");
        let resolver = StubIncludeResolver::new(workspace_root.clone());
        let toolchain = Toolchain::default();
        let extra_paths = Vec::new();
        let mut registry = ArtifactRegistry::new();
        let mut ctx = AnalysisContext::new(
            TargetId(1),
            ConfigId::DEFAULT,
            "",
            Some("module_platlib"),
            &output_dir,
            &mut registry,
            &resolver,
            &toolchain,
            &workspace_root,
            &extra_paths,
        );

        let providers = instance.analyze(&mut ctx, &[]);
        let info = providers.get_custom("PyInfo").unwrap();
        assert_eq!(
            decode_list(info, "imports"),
            vec!["/external/module_platlib".to_string()]
        );
    }
}
