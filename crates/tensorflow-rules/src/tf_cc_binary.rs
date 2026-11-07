use anyhow::{anyhow, Result};
use async_trait::async_trait;
use rush_rules::{
    context::BuildContext, extract_bool, extract_string, extract_string_list,
    providers::ProviderSet, Artifact, ArtifactType, BuildRule, RuleInstance, Value,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

const EMPTY_LINK_UNSUPPORTED: &str = "empty tf_cc_binary link shape is not scheduler-verified";

/// Resolve a source/header entry against the package that owns this binary.
/// This mirrors rules_cc's resolver, including cross-package `//pkg:file`
/// source labels and already-expanded absolute filegroup members.
fn resolve_tf_cc_input_path(workspace_root: &Path, package: &str, raw: &str) -> PathBuf {
    // Parse Bazel labels before consulting Path: on Unix, `//pkg:file` is
    // otherwise classified as an absolute filesystem path and kept verbatim.
    if let Some(rest) = raw.strip_prefix("//") {
        if let Some((pkg, name)) = rest.split_once(':') {
            return workspace_root.join(pkg).join(name);
        }
        return workspace_root.join(rest);
    }
    if let Some(name) = raw.strip_prefix(':') {
        return workspace_root.join(package).join(name);
    }
    if raw.starts_with('@') {
        let name = rush_rules::cc_command_spec::source_file_component(raw);
        return workspace_root.join(package).join(name);
    }
    let raw_path = Path::new(raw);
    if raw_path.is_absolute() {
        return raw_path.to_path_buf();
    }
    workspace_root.join(package).join(raw)
}

/// Candidate path for a generated source/header named directly in `srcs`.
/// Generated filegroup members are normally expanded to their absolute path;
/// this fallback covers same-package genrule outputs named as plain files.
fn generated_tf_cc_input_path(output_dir: &Path, package: &str, raw: &str) -> Option<PathBuf> {
    if raw.starts_with('@') {
        return None;
    }
    let (owner_package, name) = if let Some(rest) = raw.strip_prefix("//") {
        rest.split_once(':').unwrap_or((rest, ""))
    } else if let Some(name) = raw.strip_prefix(':') {
        (package, name)
    } else if Path::new(raw).is_absolute() {
        return None;
    } else {
        (package, raw)
    };
    if name.is_empty() {
        return None;
    }
    Some(output_dir.join("genfiles").join(owner_package).join(name))
}

/// Expand explicit target labels and the implicit `:<file>` dependencies
/// created for bare srcs. Only absolute FilesInfo members are safe to splice;
/// relative cross-package source-file providers retain their original label.
fn expand_tf_cc_srcs(
    srcs: &[String],
    lookup: impl Fn(&str) -> Option<Vec<PathBuf>>,
) -> Vec<String> {
    let mut out = Vec::with_capacity(srcs.len());
    for src in srcs {
        let label = if src.starts_with(':') || src.starts_with("//") || src.starts_with('@') {
            Some(src.clone())
        } else if Path::new(src).is_absolute() {
            None
        } else {
            Some(format!(":{src}"))
        };
        if let Some(label) = label {
            if let Some(files) = lookup(&label) {
                if !files.is_empty() && files.iter().all(|path| path.is_absolute()) {
                    out.extend(
                        files
                            .into_iter()
                            .map(|path| path.to_string_lossy().into_owned()),
                    );
                    continue;
                }
            }
        }
        out.push(src.clone());
    }
    out
}

/// Resolve an owned input during analysis. Checked-in inputs are registered at
/// their absolute package-qualified paths. An artifact already registered at
/// either the resolved source path or the genfiles path is reused verbatim so
/// generated sources/headers retain their producer identity.
fn resolve_analysis_owned_input(
    ctx: &mut rush_rules::analysis_context::AnalysisContext<'_>,
    raw: &str,
    kind: rush_action::ids::ArtifactKind,
) -> Option<(rush_action::ids::ArtifactId, PathBuf)> {
    let source_path = resolve_tf_cc_input_path(ctx.workspace_root(), ctx.package(), raw);
    if let Some(aid) = ctx
        .registry()
        .find_artifact_by_path(&source_path, ctx.config_id())
    {
        return Some((aid, source_path));
    }
    if source_path.is_file() {
        let aid = ctx.register_source_artifact(&source_path, kind);
        return Some((aid, source_path));
    }

    let generated_path = generated_tf_cc_input_path(ctx.output_dir(), ctx.package(), raw)?;
    let aid = ctx
        .registry()
        .find_artifact_by_path(&generated_path, ctx.config_id())?;
    Some((aid, generated_path))
}

/// Build/legacy counterpart of `resolve_analysis_owned_input`. By these phases
/// generated actions may already have materialized their outputs, but prefer
/// registry identity when available so both paths choose the same file.
fn resolve_build_owned_input(
    registry: Option<&rush_action::ids::ArtifactRegistry>,
    workspace_root: &Path,
    output_dir: &Path,
    package: &str,
    raw: &str,
) -> Option<PathBuf> {
    let source_path = resolve_tf_cc_input_path(workspace_root, package, raw);
    if let Some(aid) = registry.and_then(|reg| {
        reg.find_artifact_by_path(&source_path, rush_action::ids::ConfigId::DEFAULT)
    }) {
        return registry.map(|reg| reg.artifact_path(aid).to_path_buf());
    }
    if source_path.is_file() {
        return Some(source_path);
    }

    let generated_path = generated_tf_cc_input_path(output_dir, package, raw)?;
    if let Some(aid) = registry.and_then(|reg| {
        reg.find_artifact_by_path(&generated_path, rush_action::ids::ConfigId::DEFAULT)
    }) {
        return registry.map(|reg| reg.artifact_path(aid).to_path_buf());
    }
    generated_path.is_file().then_some(generated_path)
}

/// Partition expanded `srcs` into real translation units and owned headers.
/// Header paths are sorted/deduped because they feed every compile action.
fn collect_build_owned_inputs(
    registry: Option<&rush_action::ids::ArtifactRegistry>,
    workspace_root: &Path,
    output_dir: &Path,
    package: &str,
    expanded_srcs: &[String],
) -> (Vec<(String, PathBuf)>, Vec<PathBuf>) {
    let mut sources = Vec::new();
    let mut seen_sources = HashSet::new();
    let mut headers = Vec::new();
    for raw in expanded_srcs {
        if crate::is_tf_cc_compilable_source(raw) {
            if let Some(path) =
                resolve_build_owned_input(registry, workspace_root, output_dir, package, raw)
            {
                if seen_sources.insert(path.clone()) {
                    sources.push((raw.clone(), path));
                }
            }
        } else if crate::is_tf_cc_header_input(raw) {
            if let Some(path) =
                resolve_build_owned_input(registry, workspace_root, output_dir, package, raw)
            {
                headers.push(path);
            }
        }
    }
    headers.sort();
    headers.dedup();
    (sources, headers)
}

/// All ordinary and textual headers reachable from dependency CcInfos are
/// compile inputs, regardless of whether they are checked in or generated.
fn collect_dep_header_inputs(
    registry: &rush_action::ids::ArtifactRegistry,
    cc_infos: &[std::sync::Arc<rush_rules::providers::CcInfo>],
) -> Vec<rush_action::ids::ArtifactId> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for cc in cc_infos {
        for aid in cc
            .compilation
            .headers
            .iter()
            .chain(cc.compilation.textual_headers.iter())
        {
            if seen.insert(aid) {
                out.push(aid);
            }
        }
    }
    out.sort_by(|left, right| {
        registry
            .artifact_path(*left)
            .cmp(registry.artifact_path(*right))
    });
    out
}

/// Collect dependency include roots for a tf_cc_binary compile. Ordinary
/// header parents preserve the rule's flat include-search behavior, while
/// `CcInfo.compilation.include_dirs` carries directory-only roots such as an
/// include-prefix virtual tree. Textual and owned headers remain action inputs
/// without widening the include search path.
fn collect_dep_compile_include_dirs(
    registry: &rush_action::ids::ArtifactRegistry,
    cc_infos: &[std::sync::Arc<rush_rules::providers::CcInfo>],
) -> Vec<PathBuf> {
    let mut include_dirs = sorted_header_parent_dirs(cc_infos.iter().flat_map(|cc| {
        cc.compilation
            .headers
            .iter()
            .map(|aid| registry.artifact_path(aid).to_path_buf())
    }));
    for cc_info in cc_infos {
        for path_id in cc_info.compilation.include_dirs.iter() {
            include_dirs.push(registry.paths().path(path_id).to_path_buf());
        }
    }
    include_dirs.sort();
    include_dirs.dedup();
    include_dirs
}

fn sorted_header_parent_dirs(header_paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut parents: Vec<PathBuf> = header_paths
        .into_iter()
        .filter_map(|path| path.parent().map(Path::to_path_buf))
        .filter(|path| !path.as_os_str().is_empty())
        .collect();
    parents.sort();
    parents.dedup();
    parents
}

/// TensorFlow cc_binary rule
pub struct TfCcBinaryRule;

#[async_trait]
impl BuildRule for TfCcBinaryRule {
    fn name(&self) -> &str {
        "tf_cc_binary"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let srcs = extract_string_list(attrs, "srcs").unwrap_or_default();
        let deps = extract_string_list(attrs, "deps").unwrap_or_default();
        let copts = extract_string_list(attrs, "copts").unwrap_or_default();
        let linkopts = extract_string_list(attrs, "linkopts").unwrap_or_default();
        let defines = extract_string_list(attrs, "defines").unwrap_or_default();

        // TensorFlow-specific attributes
        let gpu = extract_bool(attrs, "gpu")?.unwrap_or(false);
        let xla = extract_bool(attrs, "xla")?.unwrap_or(false);

        Ok(Box::new(TfCcBinaryInstance {
            name,
            srcs,
            deps,
            copts,
            linkopts,
            defines,
            gpu,
            xla,
        }))
    }

    fn documentation(&self) -> &str {
        "TensorFlow C++ binary rule"
    }
}

struct TfCcBinaryInstance {
    name: String,
    srcs: Vec<String>,
    deps: Vec<String>,
    copts: Vec<String>,
    linkopts: Vec<String>,
    defines: Vec<String>,
    gpu: bool,
    xla: bool,
}

#[async_trait]
impl RuleInstance for TfCcBinaryInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        let mut deps = self.deps.clone();

        // A filegroup or generated-file label in srcs must be analyzed before
        // this consumer so expand_filegroup_srcs can expose its actual files.
        for src in &self.srcs {
            if (src.starts_with(':') || src.starts_with("//") || src.starts_with('@'))
                && !deps.contains(src)
            {
                deps.push(src.clone());
            }
        }
        // Every bare srcs entry is a Bazel file label. Register the implicit
        // same-package label so clean-tree generated translation units as well
        // as headers reach their producer; checked-in files use the graph
        // resolver's existing source-file fallback.
        for input in &self.srcs {
            if input.starts_with(':')
                || input.starts_with("//")
                || input.starts_with('@')
                || Path::new(input).is_absolute()
            {
                continue;
            }
            let implicit = format!(":{input}");
            if !deps.contains(&implicit) {
                deps.push(implicit);
            }
        }

        // Add GPU dependencies if enabled
        if self.gpu {
            deps.push("//tensorflow/core:gpu_runtime".to_string());
        }

        if self.xla {
            deps.push("//tensorflow/compiler/xla:xla".to_string());
        }

        deps
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        let mut artifacts = Vec::new();

        // Compile flags
        let mut compile_flags = self.copts.clone();

        // Add defines
        for define in &self.defines {
            compile_flags.push(format!("-D{}", define));
        }

        // Add GPU defines
        if self.gpu {
            compile_flags.push("-DGOOGLE_CUDA=1".to_string());
        }

        // Add XLA defines
        if self.xla {
            compile_flags.push("-DTENSORFLOW_USE_XLA=1".to_string());
        }

        let package = context
            .package_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let expanded_srcs =
            expand_tf_cc_srcs(&self.srcs, |label| context.get_dependency_files(label));
        let target_output_dir = context.target_output_dir(&self.name);

        let (resolved_sources, include_paths, link_plan) =
            if let Some(reg_arc) = context.artifact_registry.as_ref() {
                let reg = reg_arc.lock().await;
                let ordered_cc_infos: Vec<std::sync::Arc<rush_rules::providers::CcInfo>> = context
                    .dependency_providers_ordered
                    .iter()
                    .filter_map(|(_, ps)| ps.cc_info.clone())
                    .collect();
                let (sources, _own_headers) = collect_build_owned_inputs(
                    Some(&reg),
                    &context.workspace_root,
                    &context.output_dir,
                    &package,
                    &expanded_srcs,
                );
                (
                    sources,
                    collect_dep_compile_include_dirs(&reg, &ordered_cc_infos),
                    crate::tf_cc_link::assemble_tf_cc_link_plan(&ordered_cc_infos, &reg),
                )
            } else {
                let (sources, _own_headers) = collect_build_owned_inputs(
                    None,
                    &context.workspace_root,
                    &context.output_dir,
                    &package,
                    &expanded_srcs,
                );
                (
                    sources,
                    Vec::new(),
                    crate::tf_cc_link::TfCcLinkPlan::default(),
                )
            };

        for include_path in &include_paths {
            compile_flags.push(format!("-I{}", include_path.display()));
        }

        // Compile each real translation unit. Header-like srcs entries are
        // declared inputs above and are intentionally not compiled.
        for (src, src_path) in &resolved_sources {
            let obj_name = crate::tf_cc_obj_name_for_resolved_path(
                src_path,
                &context.workspace_root,
                &context.output_dir,
            );
            let obj_path = target_output_dir.join("obj").join(obj_name);

            // Create output directory
            if let Some(parent) = obj_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }

            // Determine compiler
            let compiler = if src.ends_with(".cc") || src.ends_with(".cpp") {
                &context.toolchain.cxx
            } else {
                &context.toolchain.cc
            };

            // Build compile command
            let mut cmd = tokio::process::Command::new(compiler);
            cmd.arg("-c").arg(src_path).arg("-o").arg(&obj_path);

            for flag in &compile_flags {
                cmd.arg(flag);
            }

            // Execute compilation
            let output = cmd.output().await?;
            if !output.status.success() {
                return Err(anyhow!(
                    "Compilation failed for {}: {}",
                    src,
                    String::from_utf8_lossy(&output.stderr)
                ));
            }

            artifacts.push(Artifact {
                path: obj_path.clone(),
                artifact_type: ArtifactType::Object,
            });
        }

        // Link binary
        let bin_path = target_output_dir.join("bin").join(&self.name);

        // Create binary directory
        if let Some(parent) = bin_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Build link command
        let mut cmd = tokio::process::Command::new(&context.toolchain.cxx);

        // Add object files first
        for artifact in &artifacts {
            if matches!(artifact.artifact_type, ArtifactType::Object) {
                cmd.arg(&artifact.path);
            }
        }

        // Match the canonical cc_binary path: honor RUSH_LINKER and otherwise
        // select the fastest available GNU-compatible linker. TensorFlow's
        // generated op tools are deps-only binaries with large archive groups,
        // so leaving them on single-threaded bfd dominates clean-build tail.
        if let Some(flag) = rush_rules::rules_cc::fast_linker_driver_flag() {
            cmd.arg(flag);
        }

        for arg in &link_plan.argv {
            cmd.arg(arg);
        }

        // Output file must come after inputs
        cmd.arg("-o").arg(&bin_path);

        // Add link options
        for linkopt in &self.linkopts {
            cmd.arg(linkopt);
        }

        // Standard libraries
        cmd.arg("-lpthread").arg("-ldl").arg("-lm");

        // Execute linking
        let output = cmd.output().await?;
        if !output.status.success() {
            return Err(anyhow!(
                "Linking failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        artifacts.push(Artifact {
            path: bin_path,
            artifact_type: ArtifactType::Executable,
        });

        Ok(artifacts)
    }

    /// Phase 5.C0: declare CppCompile per source plus one final CcLink.
    /// Cross-target libraries and auxiliary inputs come from typed CcInfo
    /// linker inputs and fragments.
    fn analyze(
        &self,
        ctx: &mut rush_rules::analysis_context::AnalysisContext,
        dep_providers: &[std::sync::Arc<rush_rules::providers::ProviderSet>],
    ) -> ProviderSet {
        use rush_action::ids::{ArtifactKind, ExecutableAction, StringId};
        use std::path::Path;
        // A deps-only tf_cc_binary can link from typed dependency libraries,
        // as TF's op-wrapper generators do. Every shape declares the final
        // link; shapes without typed link material fail closed as Incomplete.
        {
            let dep_cc_infos = rush_rules::providers::extract_cc_infos(dep_providers);
            let dep_header_inputs = collect_dep_header_inputs(ctx.registry(), &dep_cc_infos);

            let expanded_srcs = expand_tf_cc_srcs(&self.srcs, |label| {
                ctx.get_dependency_provider(label)
                    .and_then(|providers| providers.get_files())
                    .map(|files| files.files.clone())
            });

            // Resolve all owned inputs before declaring actions. Header-like
            // srcs are registered and fed to every compile, but never compiled.
            let mut own_header_inputs = Vec::new();
            let mut resolved_sources = Vec::new();
            let mut seen_sources = HashSet::new();
            for src in &expanded_srcs {
                if crate::is_tf_cc_compilable_source(src) {
                    if let Some((aid, path)) =
                        resolve_analysis_owned_input(ctx, src, ArtifactKind::Source)
                    {
                        if seen_sources.insert(aid) {
                            resolved_sources.push((src.clone(), aid, path));
                        }
                    }
                } else if crate::is_tf_cc_header_input(src) {
                    if let Some((aid, _)) =
                        resolve_analysis_owned_input(ctx, src, ArtifactKind::Header)
                    {
                        own_header_inputs.push(aid);
                    }
                }
            }

            let mut header_inputs = dep_header_inputs;
            header_inputs.extend(own_header_inputs);
            header_inputs.sort_by(|left, right| {
                ctx.registry()
                    .artifact_path(*left)
                    .cmp(ctx.registry().artifact_path(*right))
            });
            header_inputs.dedup();
            let dep_include_dirs = collect_dep_compile_include_dirs(ctx.registry(), &dep_cc_infos);

            let cxx_pathbuf = ctx.toolchain().cxx.clone();
            let cc_pathbuf = ctx.toolchain().cc.clone();

            let mut obj_artifacts: Vec<_> = Vec::with_capacity(resolved_sources.len());
            for (src, src_aid, src_path_abs) in &resolved_sources {
                let obj_name = crate::tf_cc_obj_name_for_resolved_path(
                    src_path_abs,
                    ctx.workspace_root(),
                    ctx.output_dir(),
                );
                let obj_rel = ctx.target_output_rel(&self.name, &format!("obj/{obj_name}"));
                let obj_aid = ctx.register_generated_artifact(
                    Path::new(&obj_rel),
                    ArtifactKind::Object,
                    None,
                );
                obj_artifacts.push(obj_aid);
                let mut compile_inputs = vec![*src_aid];
                compile_inputs.extend(header_inputs.iter().copied().filter(|aid| aid != src_aid));
                let compile_id = ctx
                    .declare_action("CppCompile", compile_inputs, vec![obj_aid])
                    .expect("tf_cc_binary CppCompile output must have one producer");
                {
                    let obj_path_str = ctx.registry().artifact_path(obj_aid).display().to_string();

                    // tf_cc_binary compile argv (matches build()):
                    //   -c src -o obj <copts> <-D defines> <gpu/xla>
                    //   <-I dependency include dirs>
                    let mut argv_strs: Vec<String> = Vec::new();
                    argv_strs.push("-c".to_string());
                    argv_strs.push(src_path_abs.display().to_string());
                    argv_strs.push("-o".to_string());
                    argv_strs.push(obj_path_str);
                    argv_strs.extend(self.copts.iter().cloned());
                    for d in &self.defines {
                        argv_strs.push(format!("-D{}", d));
                    }
                    if self.gpu {
                        argv_strs.push("-DGOOGLE_CUDA=1".to_string());
                    }
                    if self.xla {
                        argv_strs.push("-DTENSORFLOW_USE_XLA=1".to_string());
                    }
                    for p in &dep_include_dirs {
                        argv_strs.push(format!("-I{}", p.display()));
                    }

                    let exe_pathbuf = if src.ends_with(".cc") || src.ends_with(".cpp") {
                        &cxx_pathbuf
                    } else {
                        &cc_pathbuf
                    };
                    let exe_pid = ctx.intern_path(exe_pathbuf);
                    let argv: Vec<StringId> =
                        argv_strs.iter().map(|s| ctx.intern_string(s)).collect();
                    ctx.set_executable_action(
                        compile_id,
                        ExecutableAction {
                            executable: exe_pid,
                            argv: argv.into_boxed_slice(),
                            env: Box::new([]),
                            working_dir: None,
                            env_policy: rush_action::ids::ActionEnvPolicy::InheritHost,
                        },
                    );
                }
            }

            let link_plan =
                crate::tf_cc_link::assemble_tf_cc_link_plan(&dep_cc_infos, ctx.registry());
            let unsupported_reason = link_plan
                .unsupported_reason
                .or_else(|| crate::tf_cc_link::unsupported_raw_link_args(&self.linkopts))
                .or_else(|| {
                    (obj_artifacts.is_empty() && !link_plan.has_typed_link_input())
                        .then_some(EMPTY_LINK_UNSUPPORTED)
                });

            let mut link_inputs: Vec<rush_action::ids::ArtifactId> = obj_artifacts.clone();
            link_inputs.extend(link_plan.declared_inputs.iter().copied());
            let bin_rel = ctx.target_output_rel(&self.name, &format!("bin/{}", self.name));
            let bin_aid = ctx.register_generated_artifact(
                Path::new(&bin_rel),
                ArtifactKind::Executable,
                None,
            );
            let link_id = if let Some(reason) = unsupported_reason {
                ctx.declare_incomplete_action("CcLink", link_inputs, vec![bin_aid], reason)
            } else {
                ctx.declare_action("CcLink", link_inputs, vec![bin_aid])
            }
            .expect("tf_cc_binary CcLink output must have one producer");
            if unsupported_reason.is_none() {
                let obj_path_strs: Vec<String> = obj_artifacts
                    .iter()
                    .map(|aid| ctx.registry().artifact_path(*aid).display().to_string())
                    .collect();
                let bin_path_str = ctx.registry().artifact_path(bin_aid).display().to_string();

                // tf_cc_binary link argv (matches build()):
                //   <objs...> <fast-linker flag, when available>
                //   <typed dependency link plan> -o <bin>
                //   <linkopts...> -lpthread -ldl -lm
                let mut argv_strs: Vec<String> = Vec::new();
                for o in &obj_path_strs {
                    argv_strs.push(o.clone());
                }
                if let Some(flag) = rush_rules::rules_cc::fast_linker_driver_flag() {
                    argv_strs.push(flag);
                }
                argv_strs.extend(link_plan.argv.iter().cloned());
                argv_strs.push("-o".to_string());
                argv_strs.push(bin_path_str);
                argv_strs.extend(self.linkopts.iter().cloned());
                argv_strs.push("-lpthread".to_string());
                argv_strs.push("-ldl".to_string());
                argv_strs.push("-lm".to_string());

                let exe_pid = ctx.intern_path(&cxx_pathbuf);
                let argv: Vec<StringId> = argv_strs.iter().map(|s| ctx.intern_string(s)).collect();
                ctx.set_executable_action(
                    link_id,
                    ExecutableAction {
                        executable: exe_pid,
                        argv: argv.into_boxed_slice(),
                        env: Box::new([]),
                        working_dir: None,
                        env_policy: rush_action::ids::ActionEnvPolicy::InheritHost,
                    },
                );
            }

            // DefaultInfo.files = [.exe only] — commit 9 invariant:
            // public output is the binary; .o files stay internal.
            ProviderSet::empty().with_default_info(rush_rules::providers::DefaultInfo {
                files: vec![bin_aid],
            })
        }
    }

    /// Phase 5.C.4 commit 29: legacy specs for the verifier.
    /// Mirrors what build()'s argv emission does at runtime
    /// (post-commit-29 modernization to typed CcInfo). One
    /// CppCompile spec per src + one CcLink spec.
    async fn build_legacy_command_specs(
        &self,
        context: &BuildContext,
    ) -> Vec<(
        rush_rules::SchedulerActionKey,
        rush_action::command_spec::NormalizedCommandSpec,
    )> {
        use rush_action::command_spec::NormalizedCommandSpec;
        use rush_rules::SchedulerActionKey;
        let mut specs = Vec::new();

        let package = context
            .package_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let expanded_srcs =
            expand_tf_cc_srcs(&self.srcs, |label| context.get_dependency_files(label));

        let (resolved_sources, mut header_paths, dep_include_dirs, link_plan, link_input_paths) =
            if let Some(reg_arc) = context.artifact_registry.as_ref() {
                let reg = reg_arc.lock().await;
                let ordered_cc_infos: Vec<std::sync::Arc<rush_rules::providers::CcInfo>> = context
                    .dependency_providers_ordered
                    .iter()
                    .filter_map(|(_, ps)| ps.cc_info.clone())
                    .collect();
                let dep_header_inputs = collect_dep_header_inputs(&reg, &ordered_cc_infos);
                let (sources, own_headers) = collect_build_owned_inputs(
                    Some(&reg),
                    &context.workspace_root,
                    &context.output_dir,
                    &package,
                    &expanded_srcs,
                );
                let mut header_paths = own_headers;
                header_paths.extend(
                    dep_header_inputs
                        .iter()
                        .map(|aid| reg.artifact_path(*aid).to_path_buf()),
                );
                let link_plan =
                    crate::tf_cc_link::assemble_tf_cc_link_plan(&ordered_cc_infos, &reg);
                let link_input_paths = link_plan
                    .declared_inputs
                    .iter()
                    .map(|artifact| reg.artifact_path(*artifact).to_path_buf())
                    .collect();
                (
                    sources,
                    header_paths,
                    collect_dep_compile_include_dirs(&reg, &ordered_cc_infos),
                    link_plan,
                    link_input_paths,
                )
            } else {
                let (sources, own_headers) = collect_build_owned_inputs(
                    None,
                    &context.workspace_root,
                    &context.output_dir,
                    &package,
                    &expanded_srcs,
                );
                (
                    sources,
                    own_headers,
                    Vec::new(),
                    crate::tf_cc_link::TfCcLinkPlan::default(),
                    Vec::new(),
                )
            };
        header_paths.sort();
        header_paths.dedup();

        let target_output_dir = context.target_output_dir(&self.name);
        let mut object_paths: Vec<std::path::PathBuf> = Vec::new();
        for (src, src_path) in &resolved_sources {
            let obj_name = crate::tf_cc_obj_name_for_resolved_path(
                src_path,
                &context.workspace_root,
                &context.output_dir,
            );
            let obj_path = target_output_dir.join("obj").join(obj_name);
            object_paths.push(obj_path.clone());

            let executable = if src.ends_with(".cc") || src.ends_with(".cpp") {
                context.toolchain.cxx.clone()
            } else {
                context.toolchain.cc.clone()
            };

            let mut argv: Vec<String> = Vec::new();
            argv.push("-c".to_string());
            argv.push(src_path.display().to_string());
            argv.push("-o".to_string());
            argv.push(obj_path.display().to_string());
            argv.extend(self.copts.iter().cloned());
            for d in &self.defines {
                argv.push(format!("-D{}", d));
            }
            if self.gpu {
                argv.push("-DGOOGLE_CUDA=1".to_string());
            }
            if self.xla {
                argv.push("-DTENSORFLOW_USE_XLA=1".to_string());
            }
            for p in &dep_include_dirs {
                argv.push(format!("-I{}", p.display()));
            }

            let mut declared_inputs = vec![src_path.clone()];
            declared_inputs.extend(
                header_paths
                    .iter()
                    .filter(|path| *path != src_path)
                    .cloned(),
            );

            let mut spec = NormalizedCommandSpec {
                mnemonic: "CppCompile".to_string(),
                executable,
                cwd: None,
                argv,
                env: Vec::new(),
                declared_inputs,
                declared_outputs: vec![obj_path.clone()],
                toolchain_identity: None,
                param_files: Vec::new(),
                env_policy: rush_action::ids::ActionEnvPolicy::InheritHost,
            };
            spec.normalize();
            specs.push((
                SchedulerActionKey {
                    mnemonic: "CppCompile".to_string(),
                    primary_output_path: obj_path,
                },
                spec,
            ));
        }

        let bin_path = target_output_dir.join("bin").join(&self.name);
        let mut link_argv: Vec<String> = Vec::new();
        for o in &object_paths {
            link_argv.push(o.display().to_string());
        }
        if let Some(flag) = rush_rules::rules_cc::fast_linker_driver_flag() {
            link_argv.push(flag);
        }
        link_argv.extend(link_plan.argv.iter().cloned());
        link_argv.push("-o".to_string());
        link_argv.push(bin_path.display().to_string());
        link_argv.extend(self.linkopts.iter().cloned());
        link_argv.push("-lpthread".to_string());
        link_argv.push("-ldl".to_string());
        link_argv.push("-lm".to_string());

        let mut link_declared_inputs: Vec<std::path::PathBuf> = object_paths.clone();
        link_declared_inputs.extend(link_input_paths);

        let mut link_spec = NormalizedCommandSpec {
            mnemonic: "CcLink".to_string(),
            executable: context.toolchain.cxx.clone(),
            cwd: None,
            argv: link_argv,
            env: Vec::new(),
            declared_inputs: link_declared_inputs,
            declared_outputs: vec![bin_path.clone()],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: rush_action::ids::ActionEnvPolicy::InheritHost,
        };
        link_spec.normalize();
        specs.push((
            SchedulerActionKey {
                mnemonic: "CcLink".to_string(),
                primary_output_path: bin_path,
            },
            link_spec,
        ));

        specs
    }

    fn rule_type(&self) -> &str {
        "tf_cc_binary"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rush_action::ids::{
        ActionDeclarationStatus, ArtifactKind, ArtifactRegistry, ConfigId, DeclaredActionId,
        LinkerInputExtras, TargetId,
    };
    use rush_rules::analysis_context::{AnalysisContext, StubIncludeResolver};
    use rush_rules::context::Toolchain;
    use rush_rules::providers::{CcInfo, DirectCcInfo, LinkFragment};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn test_instance(srcs: Vec<&str>) -> TfCcBinaryInstance {
        TfCcBinaryInstance {
            name: "test_bin".to_string(),
            srcs: srcs.into_iter().map(str::to_string).collect(),
            deps: vec!["//dep:headers".to_string()],
            copts: Vec::new(),
            linkopts: Vec::new(),
            defines: Vec::new(),
            gpu: false,
            xla: false,
        }
    }

    fn no_op_toolchain() -> Toolchain {
        let mut toolchain = Toolchain::default();
        toolchain.cc = PathBuf::from("true");
        toolchain.cxx = PathBuf::from("true");
        toolchain.ar = PathBuf::from("true");
        toolchain
    }

    fn action_ids_with_mnemonic(
        registry: &ArtifactRegistry,
        target: TargetId,
        mnemonic: &str,
    ) -> Vec<DeclaredActionId> {
        registry
            .actions_of(target)
            .iter()
            .copied()
            .filter(|id| {
                registry
                    .strings()
                    .str(registry.declared_action(*id).mnemonic)
                    == mnemonic
            })
            .collect()
    }

    #[test]
    fn test_parse_tf_cc_binary() {
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("test_bin".to_string()));
        attrs.insert(
            "srcs".to_string(),
            Value::List(vec![Value::String("main.cc".to_string())]),
        );
        attrs.insert("gpu".to_string(), Value::Bool(true));

        let rule = TfCcBinaryRule;
        let instance = rule.parse_attributes(&attrs).unwrap();

        assert_eq!(instance.target_name(), "test_bin");
        assert!(instance.dependencies().contains(&":main.cc".to_string()));
        assert!(instance
            .dependencies()
            .contains(&"//tensorflow/core:gpu_runtime".to_string()));
    }

    #[test]
    fn package_resolver_distinguishes_same_basename_sources() {
        let root = Path::new("/workspace");
        assert_eq!(
            resolve_tf_cc_input_path(root, "pkg/a", "main.cc"),
            Path::new("/workspace/pkg/a/main.cc")
        );
        assert_eq!(
            resolve_tf_cc_input_path(root, "pkg/b", "main.cc"),
            Path::new("/workspace/pkg/b/main.cc")
        );
        assert_eq!(
            resolve_tf_cc_input_path(root, "consumer", "//shared/tools:main.cc"),
            Path::new("/workspace/shared/tools/main.cc")
        );
    }

    #[tokio::test]
    async fn output_paths_match_across_analysis_runtime_and_legacy() {
        let temp = TempDir::new().unwrap();
        let workspace_root = temp.path().join("workspace");
        let output_dir = temp.path().join("out");
        let package = "pkg";
        for directory in ["foo", "bar"] {
            let source_dir = workspace_root.join(package).join(directory);
            std::fs::create_dir_all(&source_dir).unwrap();
            std::fs::write(source_dir.join("same.cc"), "int f() { return 0; }\n").unwrap();
        }

        let instance = test_instance(vec!["foo/same.cc", "bar/same.cc", "foo/same.cc"]);
        let resolver = StubIncludeResolver::new(workspace_root.clone());
        let toolchain = no_op_toolchain();
        let mut registry = ArtifactRegistry::default();
        let target = TargetId(1);
        {
            let mut context = AnalysisContext::new(
                target,
                ConfigId::DEFAULT,
                package,
                None,
                &output_dir,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace_root,
                &[],
            );
            let _ = instance.analyze(&mut context, &[]);
        }
        let mut analyzed_outputs: Vec<PathBuf> = registry
            .actions_of(target)
            .iter()
            .flat_map(|id| registry.declared_action(*id).outputs.iter())
            .map(|id| registry.artifact_path(*id).to_path_buf())
            .collect();
        analyzed_outputs.sort();
        assert_eq!(analyzed_outputs.len(), 3);

        let registry = Arc::new(tokio::sync::Mutex::new(registry));
        let context = BuildContext::new(workspace_root, output_dir.clone())
            .with_package(PathBuf::from(package))
            .with_toolchain(toolchain)
            .with_artifact_registry(Arc::clone(&registry));
        let mut runtime_outputs: Vec<PathBuf> = instance
            .build(&context)
            .await
            .unwrap()
            .into_iter()
            .map(|artifact| artifact.path)
            .collect();
        runtime_outputs.sort();
        assert_eq!(runtime_outputs, analyzed_outputs);

        let mut legacy_outputs: Vec<PathBuf> = instance
            .build_legacy_command_specs(&context)
            .await
            .into_iter()
            .flat_map(|(_, spec)| spec.declared_outputs)
            .collect();
        legacy_outputs.sort();
        assert_eq!(legacy_outputs, analyzed_outputs);
        let target_dir = output_dir.join(package).join("test_bin");
        assert_eq!(
            analyzed_outputs
                .iter()
                .filter(|path| path.starts_with(target_dir.join("obj")))
                .count(),
            2,
        );
        assert!(analyzed_outputs.contains(&target_dir.join("bin/test_bin")));
    }

    #[test]
    fn analyze_qualifies_outputs_by_package_and_target() {
        let temp = TempDir::new().unwrap();
        let workspace_root = temp.path().join("workspace");
        let output_dir = temp.path().join("out");
        for package in ["", "pkg/a", "pkg/b"] {
            let package_dir = workspace_root.join(package);
            std::fs::create_dir_all(&package_dir).unwrap();
            std::fs::write(package_dir.join("main.cc"), "int main() { return 0; }\n").unwrap();
        }
        let resolver = StubIncludeResolver::new(workspace_root.clone());
        let toolchain = Toolchain::default();
        let mut registry = ArtifactRegistry::default();
        let instance = test_instance(vec!["main.cc"]);

        for (target, package) in [
            (TargetId(1), ""),
            (TargetId(2), "pkg/a"),
            (TargetId(3), "pkg/b"),
        ] {
            let mut context = AnalysisContext::new(
                target,
                ConfigId::DEFAULT,
                package,
                None,
                &output_dir,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace_root,
                &[],
            );
            let _ = instance.analyze(&mut context, &[]);
        }

        for (target, package) in [
            (TargetId(1), ""),
            (TargetId(2), "pkg/a"),
            (TargetId(3), "pkg/b"),
        ] {
            let action_ids = registry.actions_of(target);
            assert_eq!(action_ids.len(), 2, "compile and link must both declare");
            let target_dir = output_dir.join(package).join("test_bin");
            for action_id in action_ids {
                let action = registry.declared_action(*action_id);
                assert_eq!(action.owner_target, target);
                for output in action.outputs.iter() {
                    assert!(registry.artifact_path(*output).starts_with(&target_dir));
                }
            }
            assert!(registry
                .find_artifact_by_path(&target_dir.join("bin/test_bin"), ConfigId::DEFAULT)
                .is_some());
        }
    }

    #[tokio::test]
    async fn compile_inputs_cover_owned_and_transitive_headers_with_legacy_parity() {
        let temp = TempDir::new().unwrap();
        let workspace_root = temp.path().join("workspace");
        let output_dir = temp.path().join("out");
        let package_dir = workspace_root.join("pkg");
        let dep_dir = workspace_root.join("dep");
        let shared_tools_dir = workspace_root.join("shared/tools");
        std::fs::create_dir_all(&package_dir).unwrap();
        std::fs::create_dir_all(&dep_dir).unwrap();
        std::fs::create_dir_all(&shared_tools_dir).unwrap();

        let main_path = package_dir.join("main.cc");
        let cross_package_source_path = shared_tools_dir.join("main.cc");
        let own_header_path = package_dir.join("own.h");
        let cuda_source_path = package_dir.join("kernel.cu");
        let cuda_header_path = package_dir.join("detail.cuh");
        let dep_header_path = dep_dir.join("dep.h");
        std::fs::write(&main_path, "int main() { return 0; }\n").unwrap();
        std::fs::write(&cross_package_source_path, "int tool() { return 1; }\n").unwrap();
        std::fs::write(&own_header_path, "#pragma once\n").unwrap();
        std::fs::write(&cuda_source_path, "__global__ void kernel() {}\n").unwrap();
        std::fs::write(&cuda_header_path, "#pragma once\n").unwrap();
        std::fs::write(&dep_header_path, "#pragma once\n").unwrap();

        let resolver = StubIncludeResolver::new(workspace_root.clone());
        let toolchain = Toolchain::default();
        let mut registry = ArtifactRegistry::default();

        let own_generated_header_path = output_dir.join("genfiles/pkg/own_generated.h");
        let generated_source_path = output_dir.join("genfiles/pkg/generated.cc");
        let dep_textual_header_path = output_dir.join("genfiles/dep/dep_generated.inc");
        let (own_generated_header, generated_source, dep_textual_header) = {
            let mut producer = AnalysisContext::new(
                TargetId(1),
                ConfigId::DEFAULT,
                "generator",
                None,
                &output_dir,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace_root,
                &[],
            );
            let own_generated_header = producer.register_generated_artifact(
                Path::new("genfiles/pkg/own_generated.h"),
                ArtifactKind::Header,
                None,
            );
            let generated_source = producer.register_generated_artifact(
                Path::new("genfiles/pkg/generated.cc"),
                ArtifactKind::Generated,
                None,
            );
            let dep_textual_header = producer.register_generated_artifact(
                Path::new("genfiles/dep/dep_generated.inc"),
                ArtifactKind::Header,
                None,
            );
            producer
                .declare_action(
                    "GenerateCcInputs",
                    Vec::new(),
                    vec![own_generated_header, generated_source, dep_textual_header],
                )
                .unwrap();
            (own_generated_header, generated_source, dep_textual_header)
        };

        let dep_header = {
            let mut dep = AnalysisContext::new(
                TargetId(2),
                ConfigId::DEFAULT,
                "dep",
                None,
                &output_dir,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace_root,
                &[],
            );
            dep.register_source_artifact(&dep_header_path, ArtifactKind::Header)
        };
        let dep_cc = CcInfo::from_direct_and_deps(
            DirectCcInfo {
                headers: vec![dep_header],
                // Include a duplicate through textual_headers to exercise the
                // deterministic ArtifactId dedup as well as a generated input.
                textual_headers: vec![dep_textual_header, dep_header],
                ..DirectCcInfo::default()
            },
            Vec::<Arc<CcInfo>>::new(),
        );
        let dep_provider = ProviderSet::empty().with_cc_info(dep_cc);
        let dep_providers = vec![Arc::new(dep_provider.clone())];

        let instance = test_instance(vec![
            "main.cc",
            "main.cc",
            "own.h",
            "own.h",
            "own_generated.h",
            "generated.cc",
            "kernel.cu",
            "detail.cuh",
            "//shared/tools:main.cc",
        ]);
        let dependencies = instance.dependencies();
        assert!(dependencies.contains(&":own.h".to_string()));
        assert!(dependencies.contains(&":own_generated.h".to_string()));
        assert!(dependencies.contains(&":generated.cc".to_string()));
        assert!(dependencies.contains(&":kernel.cu".to_string()));
        assert!(dependencies.contains(&":detail.cuh".to_string()));
        assert!(dependencies.contains(&"//shared/tools:main.cc".to_string()));

        let consumer_target = TargetId(3);
        {
            let mut consumer = AnalysisContext::new(
                consumer_target,
                ConfigId::DEFAULT,
                "pkg",
                None,
                &output_dir,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace_root,
                &[],
            );
            instance.analyze(&mut consumer, &dep_providers);
        }

        let main_source = registry
            .find_artifact_by_path(&main_path, ConfigId::DEFAULT)
            .expect("main source registered at its package-qualified path");
        let own_header = registry
            .find_artifact_by_path(&own_header_path, ConfigId::DEFAULT)
            .expect("own source header registered at its package-qualified path");
        let cross_package_source = registry
            .find_artifact_by_path(&cross_package_source_path, ConfigId::DEFAULT)
            .expect("label source registered at its expanded absolute path");
        let cuda_source = registry
            .find_artifact_by_path(&cuda_source_path, ConfigId::DEFAULT)
            .expect("CUDA source registered and compiled");
        let cuda_header = registry
            .find_artifact_by_path(&cuda_header_path, ConfigId::DEFAULT)
            .expect("CUDA header registered as a private input");
        assert!(registry.artifact(main_source).is_source);
        assert!(registry.artifact(own_header).is_source);
        assert!(registry.artifact(cross_package_source).is_source);
        assert!(registry.artifact(cuda_source).is_source);
        assert!(registry.artifact(cuda_header).is_source);
        assert_eq!(
            registry.find_artifact_by_path(Path::new("main.cc"), ConfigId::DEFAULT),
            None
        );
        assert_eq!(
            registry.find_artifact_by_path(Path::new("own.h"), ConfigId::DEFAULT),
            None
        );
        assert_eq!(
            registry.artifact_path(own_generated_header),
            own_generated_header_path
        );
        assert_eq!(
            registry.artifact_path(generated_source),
            generated_source_path
        );
        assert_eq!(
            registry.artifact_path(dep_textual_header),
            dep_textual_header_path
        );

        let compile_ids: Vec<_> = registry
            .actions_of(consumer_target)
            .iter()
            .copied()
            .filter(|id| {
                registry
                    .strings()
                    .str(registry.declared_action(*id).mnemonic)
                    == "CppCompile"
            })
            .collect();
        assert_eq!(
            compile_ids.len(),
            4,
            "headers must not become compile actions"
        );

        let mut expected_headers = vec![
            own_header,
            own_generated_header,
            cuda_header,
            dep_header,
            dep_textual_header,
        ];
        expected_headers.sort_by(|left, right| {
            registry
                .artifact_path(*left)
                .cmp(registry.artifact_path(*right))
        });
        expected_headers.dedup();

        for id in &compile_ids {
            let action = registry.declared_action(*id);
            let source = action.inputs[0];
            assert!(
                source == main_source
                    || source == generated_source
                    || source == cuda_source
                    || source == cross_package_source
            );
            let mut expected = vec![source];
            expected.extend(
                expected_headers
                    .iter()
                    .copied()
                    .filter(|aid| *aid != source),
            );
            assert_eq!(action.inputs.as_ref(), expected.as_slice());

            let argv = registry
                .executable_action(*id)
                .expect("compile executable payload")
                .materialize(&registry)
                .argv;
            assert!(argv.contains(&format!("-I{}", dep_dir.display())));
            assert!(!argv.contains(&format!("-I{}", package_dir.display())));
            assert!(!argv.contains(&format!(
                "-I{}",
                own_generated_header_path.parent().unwrap().display()
            )));
            assert!(!argv.contains(&format!(
                "-I{}",
                dep_textual_header_path.parent().unwrap().display()
            )));
        }

        let scheduler_compile_specs: Vec<_> = compile_ids
            .iter()
            .map(|id| {
                rush_action::command_spec::NormalizedCommandSpec::from_registry_action(
                    &registry, *id,
                )
                .unwrap()
            })
            .collect();
        let registry = Arc::new(tokio::sync::Mutex::new(registry));
        let build_context = BuildContext::new(workspace_root.clone(), output_dir.clone())
            .with_package(PathBuf::from("pkg"))
            .with_dependencies(
                HashMap::new(),
                vec![("//dep:headers".to_string(), dep_provider)],
            )
            .with_artifact_registry(Arc::clone(&registry));
        let legacy_specs = instance.build_legacy_command_specs(&build_context).await;
        let legacy_compile_specs: Vec<_> = legacy_specs
            .into_iter()
            .map(|(_, spec)| spec)
            .filter(|spec| spec.mnemonic == "CppCompile")
            .collect();
        assert_eq!(legacy_compile_specs.len(), scheduler_compile_specs.len());
        for scheduler in &scheduler_compile_specs {
            let legacy = legacy_compile_specs
                .iter()
                .find(|spec| spec.declared_outputs == scheduler.declared_outputs)
                .expect("legacy compile spec for scheduler output");
            assert_eq!(legacy, scheduler);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn include_only_cc_info_reaches_runtime_and_matches_scheduler_legacy() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        let workspace_root = temp.path().join("workspace");
        let output_dir = temp.path().join("out");
        let package_dir = workspace_root.join("pkg");
        let dep_dir = workspace_root.join("dep");
        std::fs::create_dir_all(&package_dir).unwrap();
        std::fs::create_dir_all(&dep_dir).unwrap();

        let main_path = package_dir.join("main.cc");
        let dep_header_path = dep_dir.join("dep.h");
        std::fs::write(&main_path, "int main() { return 0; }\n").unwrap();
        std::fs::write(&dep_header_path, "#pragma once\n").unwrap();

        let arg_log = temp.path().join("compiler-args.log");
        let compiler = temp.path().join("record-compiler.sh");
        std::fs::write(
            &compiler,
            format!(
                "#!/bin/sh\nprintf '%s\\n' __invocation__ \"$@\" >> '{}'\n",
                arg_log.display()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&compiler).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&compiler, permissions).unwrap();

        let resolver = StubIncludeResolver::new(workspace_root.clone());
        let mut toolchain = Toolchain::default();
        toolchain.cc = compiler.clone();
        toolchain.cxx = compiler;
        let mut registry = ArtifactRegistry::default();

        let dep_header = {
            let mut producer = AnalysisContext::new(
                TargetId(1),
                ConfigId::DEFAULT,
                "dep",
                None,
                &output_dir,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace_root,
                &[],
            );
            producer.register_source_artifact(&dep_header_path, ArtifactKind::Header)
        };

        let virtual_include_root = output_dir.join("_virtual_includes/include_only");
        let virtual_include_id = registry.intern_path(&virtual_include_root);
        let duplicate_header_parent_id = registry.intern_path(&dep_dir);
        let header_cc = CcInfo::from_direct_and_deps(
            DirectCcInfo {
                headers: vec![dep_header],
                ..DirectCcInfo::default()
            },
            std::iter::empty(),
        );
        let include_only_cc = CcInfo::from_direct_and_deps(
            DirectCcInfo {
                include_dirs: vec![
                    virtual_include_id,
                    duplicate_header_parent_id,
                    virtual_include_id,
                ],
                ..DirectCcInfo::default()
            },
            std::iter::empty(),
        );
        let header_provider = ProviderSet::empty().with_cc_info(header_cc);
        let include_only_provider = ProviderSet::empty().with_cc_info(include_only_cc);
        let dep_providers = vec![
            Arc::new(header_provider.clone()),
            Arc::new(include_only_provider.clone()),
        ];

        let mut instance = test_instance(vec!["main.cc"]);
        instance.deps = vec![
            "//dep:headers".to_string(),
            "//dep:include_only".to_string(),
        ];
        let consumer_target = TargetId(2);
        {
            let mut consumer = AnalysisContext::new(
                consumer_target,
                ConfigId::DEFAULT,
                "pkg",
                None,
                &output_dir,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace_root,
                &[],
            );
            instance.analyze(&mut consumer, &dep_providers);
        }

        let main_source = registry
            .find_artifact_by_path(&main_path, ConfigId::DEFAULT)
            .expect("main source artifact");
        let compile_ids = action_ids_with_mnemonic(&registry, consumer_target, "CppCompile");
        assert_eq!(compile_ids.len(), 1);
        assert_eq!(
            registry.declared_action(compile_ids[0]).inputs.as_ref(),
            &[main_source, dep_header],
            "directory-only include roots must not replace header action inputs",
        );

        let scheduler_spec =
            rush_action::command_spec::NormalizedCommandSpec::from_registry_action(
                &registry,
                compile_ids[0],
            )
            .unwrap();
        let mut expected_include_dirs = vec![dep_dir.clone(), virtual_include_root.clone()];
        expected_include_dirs.sort();
        let expected_include_flags: Vec<_> = expected_include_dirs
            .iter()
            .map(|path| format!("-I{}", path.display()))
            .collect();
        let scheduler_include_flags: Vec<_> = scheduler_spec
            .argv
            .iter()
            .filter(|arg| arg.starts_with("-I"))
            .cloned()
            .collect();
        assert_eq!(scheduler_include_flags, expected_include_flags);

        let registry = Arc::new(tokio::sync::Mutex::new(registry));
        let build_context = BuildContext::new(workspace_root, output_dir)
            .with_package(PathBuf::from("pkg"))
            .with_toolchain(toolchain)
            .with_dependencies(
                HashMap::new(),
                vec![
                    ("//dep:headers".to_string(), header_provider),
                    ("//dep:include_only".to_string(), include_only_provider),
                ],
            )
            .with_artifact_registry(registry);
        let legacy_spec = instance
            .build_legacy_command_specs(&build_context)
            .await
            .into_iter()
            .map(|(_, spec)| spec)
            .find(|spec| spec.mnemonic == "CppCompile")
            .expect("legacy compile spec");
        assert_eq!(legacy_spec, scheduler_spec);

        instance.build(&build_context).await.unwrap();
        let runtime_args = std::fs::read_to_string(arg_log).unwrap();
        let runtime_include_flags: Vec<_> = runtime_args
            .lines()
            .filter(|arg| arg.starts_with("-I"))
            .map(str::to_string)
            .collect();
        assert_eq!(runtime_include_flags, expected_include_flags);
    }

    #[tokio::test]
    async fn fragment_only_link_declares_input_and_matches_legacy() {
        let temp = TempDir::new().unwrap();
        let workspace_root = temp.path().join("workspace");
        let output_dir = temp.path().join("out");
        let resolver = StubIncludeResolver::new(workspace_root.clone());
        let toolchain = no_op_toolchain();
        let mut registry = ArtifactRegistry::default();

        let (library, linkstamp, linker_input) = {
            let mut producer = AnalysisContext::new(
                TargetId(1),
                ConfigId::DEFAULT,
                "dep",
                None,
                &output_dir,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace_root,
                &[],
            );
            let library = producer.register_generated_artifact(
                Path::new("dep/libfragment.a"),
                ArtifactKind::StaticLibrary,
                None,
            );
            let linkstamp = producer.register_generated_artifact(
                Path::new("dep/linkstamp.o"),
                ArtifactKind::Object,
                None,
            );
            producer
                .declare_action("CppArchive", Vec::new(), vec![library, linkstamp])
                .unwrap();
            let linker_input = producer.register_linker_input_with_extras(
                library,
                false,
                Arc::new(LinkerInputExtras {
                    additional_inputs: vec![linkstamp].into_boxed_slice(),
                    per_input_link_flags: Vec::new().into_boxed_slice(),
                }),
            );
            (library, linkstamp, linker_input)
        };
        let dep_cc = CcInfo::from_direct_and_deps(
            DirectCcInfo {
                link_fragments: vec![LinkFragment::Library(linker_input)],
                ..DirectCcInfo::default()
            },
            std::iter::empty(),
        );
        let dep_provider = ProviderSet::empty().with_cc_info(dep_cc);
        let dep_providers = vec![Arc::new(dep_provider.clone())];
        let instance = test_instance(Vec::new());
        let consumer_target = TargetId(2);
        let providers = {
            let mut consumer = AnalysisContext::new(
                consumer_target,
                ConfigId::DEFAULT,
                "pkg",
                None,
                &output_dir,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace_root,
                &[],
            );
            instance.analyze(&mut consumer, &dep_providers)
        };

        let link_ids = action_ids_with_mnemonic(&registry, consumer_target, "CcLink");
        assert_eq!(link_ids.len(), 1);
        let link_id = link_ids[0];
        let link_action = registry.declared_action(link_id);
        assert!(matches!(
            &link_action.status,
            ActionDeclarationStatus::Complete
        ));
        assert_eq!(link_action.inputs.as_ref(), &[library, linkstamp]);
        assert!(registry.artifact(library).owner_action_declared.is_some());
        assert!(registry.artifact(linkstamp).owner_action_declared.is_some());
        assert_eq!(
            providers.get_default_info().unwrap().files.as_slice(),
            link_action.outputs.as_ref()
        );

        let scheduler_spec =
            rush_action::command_spec::NormalizedCommandSpec::from_registry_action(
                &registry, link_id,
            )
            .unwrap();
        let library_path = registry.artifact_path(library).display().to_string();
        let linkstamp_path = registry.artifact_path(linkstamp).to_path_buf();
        let bin_path = output_dir.join("pkg/test_bin/bin/test_bin");
        let mut expected_argv = Vec::new();
        if let Some(flag) = rush_rules::rules_cc::fast_linker_driver_flag() {
            expected_argv.push(flag);
        }
        expected_argv.extend([
            library_path,
            "-o".to_string(),
            bin_path.display().to_string(),
            "-lpthread".to_string(),
            "-ldl".to_string(),
            "-lm".to_string(),
        ]);
        assert_eq!(scheduler_spec.argv, expected_argv);
        assert!(!scheduler_spec
            .argv
            .contains(&linkstamp_path.display().to_string()));
        let mut expected_inputs = vec![
            registry.artifact_path(library).to_path_buf(),
            linkstamp_path,
        ];
        expected_inputs.sort();
        assert_eq!(scheduler_spec.declared_inputs, expected_inputs);

        let registry = Arc::new(tokio::sync::Mutex::new(registry));
        let build_context = BuildContext::new(workspace_root, output_dir.clone())
            .with_package(PathBuf::from("pkg"))
            .with_toolchain(toolchain)
            .with_dependencies(
                HashMap::new(),
                vec![("//dep:headers".to_string(), dep_provider)],
            )
            .with_artifact_registry(Arc::clone(&registry));
        let legacy_specs = instance.build_legacy_command_specs(&build_context).await;
        let legacy_link_specs: Vec<_> = legacy_specs
            .into_iter()
            .map(|(_, spec)| spec)
            .filter(|spec| spec.mnemonic == "CcLink")
            .collect();
        assert_eq!(legacy_link_specs, vec![scheduler_spec]);

        let runtime_outputs = instance.build(&build_context).await.unwrap();
        assert_eq!(runtime_outputs.len(), 1);
        assert_eq!(runtime_outputs[0].path, bin_path);
    }

    #[tokio::test]
    async fn empty_binary_declares_incomplete_link_without_payload() {
        let temp = TempDir::new().unwrap();
        let workspace_root = temp.path().join("workspace");
        let output_dir = temp.path().join("out");
        let resolver = StubIncludeResolver::new(workspace_root.clone());
        let toolchain = no_op_toolchain();
        let mut registry = ArtifactRegistry::default();
        let mut instance = test_instance(Vec::new());
        instance.deps.clear();
        let target = TargetId(1);
        let providers = {
            let mut context = AnalysisContext::new(
                target,
                ConfigId::DEFAULT,
                "pkg",
                None,
                &output_dir,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace_root,
                &[],
            );
            instance.analyze(&mut context, &[])
        };

        let link_ids = action_ids_with_mnemonic(&registry, target, "CcLink");
        assert_eq!(link_ids.len(), 1);
        let link_id = link_ids[0];
        let link_action = registry.declared_action(link_id);
        assert!(matches!(
            &link_action.status,
            ActionDeclarationStatus::Incomplete { reason }
                if *reason == EMPTY_LINK_UNSUPPORTED
        ));
        assert!(link_action.inputs.is_empty());
        assert!(registry.executable_action(link_id).is_none());
        assert_eq!(
            providers.get_default_info().unwrap().files.as_slice(),
            link_action.outputs.as_ref()
        );

        let registry = Arc::new(tokio::sync::Mutex::new(registry));
        let build_context = BuildContext::new(workspace_root, output_dir)
            .with_package(PathBuf::from("pkg"))
            .with_toolchain(toolchain)
            .with_artifact_registry(registry);
        let legacy_link_count = instance
            .build_legacy_command_specs(&build_context)
            .await
            .into_iter()
            .filter(|(_, spec)| spec.mnemonic == "CcLink")
            .count();
        assert_eq!(legacy_link_count, 1);
    }

    #[test]
    fn artifact_bearing_direct_linkopt_declares_incomplete_link() {
        let temp = TempDir::new().unwrap();
        let workspace_root = temp.path().join("workspace");
        let output_dir = temp.path().join("out");
        let resolver = StubIncludeResolver::new(workspace_root.clone());
        let toolchain = no_op_toolchain();
        let mut registry = ArtifactRegistry::default();
        let mut instance = test_instance(Vec::new());
        instance.deps.clear();
        instance.linkopts = vec!["/opaque/libdirect.a".to_string()];
        let target = TargetId(1);
        {
            let mut context = AnalysisContext::new(
                target,
                ConfigId::DEFAULT,
                "pkg",
                None,
                &output_dir,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace_root,
                &[],
            );
            instance.analyze(&mut context, &[]);
        }

        let link_ids = action_ids_with_mnemonic(&registry, target, "CcLink");
        assert_eq!(link_ids.len(), 1);
        let link_id = link_ids[0];
        let link_action = registry.declared_action(link_id);
        assert!(link_action.inputs.is_empty());
        match &link_action.status {
            ActionDeclarationStatus::Incomplete { reason } => {
                assert_eq!(
                    *reason,
                    crate::tf_cc_link::UNTYPED_ARTIFACT_REFERENCE_UNSUPPORTED
                );
            }
            ActionDeclarationStatus::Complete => panic!("raw artifact linkopt must fail closed"),
        }
        assert!(registry.executable_action(link_id).is_none());
    }

    #[test]
    fn flag_only_linkopts_do_not_make_an_empty_binary_complete() {
        let temp = TempDir::new().unwrap();
        let workspace_root = temp.path().join("workspace");
        let output_dir = temp.path().join("out");
        let resolver = StubIncludeResolver::new(workspace_root.clone());
        let toolchain = no_op_toolchain();
        let mut registry = ArtifactRegistry::default();
        let mut instance = test_instance(Vec::new());
        instance.deps.clear();
        instance.linkopts = vec!["-s".to_string()];
        let target = TargetId(1);
        {
            let mut context = AnalysisContext::new(
                target,
                ConfigId::DEFAULT,
                "pkg",
                None,
                &output_dir,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace_root,
                &[],
            );
            instance.analyze(&mut context, &[]);
        }

        let link_ids = action_ids_with_mnemonic(&registry, target, "CcLink");
        assert_eq!(link_ids.len(), 1);
        let link_id = link_ids[0];
        let link_action = registry.declared_action(link_id);
        assert!(link_action.inputs.is_empty());
        assert!(matches!(
            &link_action.status,
            ActionDeclarationStatus::Incomplete { reason }
                if *reason == EMPTY_LINK_UNSUPPORTED
        ));
        assert!(registry.executable_action(link_id).is_none());
    }
}
