use anyhow::{anyhow, Result};
use async_trait::async_trait;
use rush_rules::{
    context::BuildContext, extract_bool, extract_string, extract_string_list,
    providers::ProviderSet, Artifact, ArtifactType, BuildRule, RuleInstance, Value,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

// Framework search roots are represented in CcInfo, but this native consumer
// has no framework compiler implementation. Never silently omit this bucket.
fn reject_framework_include_roots<'a>(
    providers: impl IntoIterator<Item = &'a ProviderSet>,
) -> Result<()> {
    for provider in providers {
        if let Some(cc) = provider.get_cc_info() {
            anyhow::ensure!(
                cc.compilation
                    .framework_include_dirs
                    .iter()
                    .next()
                    .is_none(),
                "native C++ compilation does not support nonempty typed framework include roots"
            );
        }
    }
    Ok(())
}

const RESPONSE_FILE_LINK_UNSUPPORTED: &str =
    "tf_cc_test response-file links are not scheduler-modeled";
const EMPTY_LINK_UNSUPPORTED: &str = "empty tf_cc_test link shape is not scheduler-verified";

/// Package portion of a Bazel label: "//a/b:c" -> "a/b"; ":c" -> consumer_pkg;
/// "@r//a/b:c" -> "a/b" (external-repo root resolution is the caller's ws_root).
fn label_package(label: &str, consumer_pkg: &str) -> String {
    if let Some(rest) = label.strip_prefix("//") {
        rest.split(':').next().unwrap_or("").to_string()
    } else if let Some(rest) = label.strip_prefix('@') {
        rest.split_once("//")
            .map(|(_, p)| p.split(':').next().unwrap_or("").to_string())
            .unwrap_or_default()
    } else if label.starts_with(':') {
        consumer_pkg.to_string()
    } else {
        consumer_pkg.to_string()
    }
}

/// Expand filegroup-label `srcs` to ABSOLUTE member paths.
fn expand_test_srcs(
    srcs: &[String],
    ws_root: &Path,
    consumer_pkg: &str,
    lookup: impl Fn(&str) -> Option<Vec<PathBuf>>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(srcs.len());
    for s in srcs {
        let label = if s.starts_with("//") || s.starts_with(':') || s.starts_with('@') {
            Some(s.clone())
        } else if Path::new(s).is_absolute() {
            None
        } else {
            Some(format!(":{s}"))
        };
        if let Some(label) = label {
            if let Some(files) = lookup(&label) {
                if !files.is_empty() {
                    let pkg = label_package(&label, consumer_pkg);
                    for f in &files {
                        let abs = if f.is_absolute() {
                            f.clone()
                        } else if pkg.is_empty() {
                            ws_root.join(f)
                        } else {
                            ws_root.join(&pkg).join(f)
                        };
                        out.push(abs.to_string_lossy().into_owned());
                    }
                    continue;
                }
            }
        }
        out.push(s.clone());
    }
    out
}

/// Resolve a source/header entry exactly as the runtime and legacy paths do,
/// while keeping already-expanded filegroup outputs absolute.
fn resolve_test_input_path(ws_root: &Path, package: &str, raw: &str) -> PathBuf {
    // Parse Bazel labels before consulting Path: on Unix, `//pkg:file` is
    // otherwise classified as an absolute filesystem path and kept verbatim.
    if let Some(rest) = raw.strip_prefix("//") {
        if let Some((label_package, name)) = rest.split_once(':') {
            return ws_root.join(label_package).join(name);
        }
        return ws_root.join(rest);
    }
    if let Some(name) = raw.strip_prefix(':') {
        return ws_root.join(package).join(name);
    }
    if raw.starts_with('@') {
        let name = rush_rules::cc_command_spec::source_file_component(raw);
        return ws_root.join(package).join(name);
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        path.to_path_buf()
    } else if package.is_empty() {
        ws_root.join(path)
    } else {
        ws_root.join(package).join(path)
    }
}

/// Every source or generated header exported by a CcInfo dependency is a
/// compile input. Sort by path after first-seen ID dedup so HashMap-backed
/// provider construction cannot perturb the declared-action shape.
fn collect_dep_header_inputs(
    registry: &rush_action::ids::ArtifactRegistry,
    dep_cc_infos: &[std::sync::Arc<rush_rules::providers::CcInfo>],
) -> Vec<rush_action::ids::ArtifactId> {
    let mut seen = HashSet::new();
    let mut headers = Vec::new();
    let roots = dep_cc_infos
        .iter()
        .flat_map(|dep_cc| {
            [
                std::sync::Arc::clone(&dep_cc.compilation.headers),
                std::sync::Arc::clone(&dep_cc.compilation.textual_headers),
            ]
        })
        .collect();
    let transitive_headers = rush_rules::nested_set::NestedSet::new(Vec::new(), roots);
    for aid in transitive_headers.iter() {
        if seen.insert(aid) {
            headers.push(aid);
        }
    }
    headers.sort_by(|left, right| {
        registry
            .artifact_path(*left)
            .cmp(registry.artifact_path(*right))
    });
    headers
}

/// Runtime response-file buckets for dependency-backed tf_cc_test links.
///
/// This deliberately preserves the legacy renderer's special treatment of
/// raw archive flags: alwayslink inputs form one whole-archive section, every
/// other archive path forms one normal group, and remaining fragments follow.
/// The scheduler cannot materialize this file yet, so analysis uses the common
/// typed link plan for input ownership but marks this runtime shape incomplete.
#[derive(Debug, Default, PartialEq, Eq)]
struct TfCcTestResponsePlan {
    whole_archives: Vec<String>,
    normal_archives: Vec<String>,
    fragments: Vec<String>,
}

impl TfCcTestResponsePlan {
    fn render(&self) -> String {
        let mut response = String::new();
        if !self.whole_archives.is_empty() {
            response.push_str("-Wl,--whole-archive\n");
            for archive in &self.whole_archives {
                response.push_str(archive);
                response.push('\n');
            }
            response.push_str("-Wl,--no-whole-archive\n");
        }
        if !self.normal_archives.is_empty() {
            response.push_str("-Wl,--start-group\n");
            for archive in &self.normal_archives {
                response.push_str(archive);
                response.push('\n');
            }
            response.push_str("-Wl,--end-group\n");
        }
        for fragment in &self.fragments {
            response.push_str(fragment);
            response.push('\n');
        }
        response
    }
}

fn assemble_tf_cc_test_response_plan(
    cc_infos: &[std::sync::Arc<rush_rules::providers::CcInfo>],
    reg: &rush_action::ids::ArtifactRegistry,
) -> TfCcTestResponsePlan {
    use rush_rules::providers::LinkFragment;

    let mut plan = TfCcTestResponsePlan::default();
    for cc in cc_infos {
        for lid in cc.linking.linker_inputs.iter() {
            let li = reg.linker_input(lid);
            let path = reg.artifact_path(li.artifact).display().to_string();
            if li.alwayslink {
                if !plan.whole_archives.contains(&path) {
                    plan.whole_archives.push(path);
                }
            } else if !plan.normal_archives.contains(&path) {
                plan.normal_archives.push(path);
            }
        }
        for frag in cc.linking.link_fragments.iter() {
            match frag {
                LinkFragment::Flag(sid) => {
                    let value = reg.strings().str(sid).to_string();
                    if value.ends_with(".a") || value.ends_with(".so") {
                        if !plan.normal_archives.contains(&value) {
                            plan.normal_archives.push(value);
                        }
                    } else if value.contains("--start-group")
                        || value.contains("--end-group")
                        || value.contains("--whole-archive")
                    {
                        // The response renderer owns archive grouping.
                    } else if !plan.fragments.contains(&value) {
                        plan.fragments.push(value);
                    }
                }
                LinkFragment::SearchDir(pid) => {
                    let value = format!("-L{}", reg.paths().path(pid).display());
                    if !plan.fragments.contains(&value) {
                        plan.fragments.push(value);
                    }
                }
                LinkFragment::Library(lid) => {
                    let li = reg.linker_input(lid);
                    let path = reg.artifact_path(li.artifact).display().to_string();
                    if !plan.normal_archives.contains(&path) {
                        plan.normal_archives.push(path);
                    }
                }
                LinkFragment::Group(lids) => {
                    for lid in lids.iter() {
                        let li = reg.linker_input(*lid);
                        let path = reg.artifact_path(li.artifact).display().to_string();
                        if !plan.normal_archives.contains(&path) {
                            plan.normal_archives.push(path);
                        }
                    }
                }
            }
        }
    }
    plan.normal_archives
        .retain(|path| !plan.whole_archives.contains(path));
    plan
}

/// TensorFlow cc_test rule
pub struct TfCcTestRule;

#[async_trait]
impl BuildRule for TfCcTestRule {
    fn name(&self) -> &str {
        "tf_cc_test"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let srcs = extract_string_list(attrs, "srcs").unwrap_or_default();
        let deps = extract_string_list(attrs, "deps").unwrap_or_default();
        let mut copts = extract_string_list(attrs, "copts").unwrap_or_default();
        copts.extend(extract_string_list(attrs, "extra_copts").unwrap_or_default());
        let linkopts = extract_string_list(attrs, "linkopts").unwrap_or_default();
        let defines = extract_string_list(attrs, "defines").unwrap_or_default();
        let data = extract_string_list(attrs, "data").unwrap_or_default();

        // TensorFlow-specific attributes
        let gpu = extract_bool(attrs, "gpu")?
            .unwrap_or_else(|| name.ends_with("_gpu") || name.ends_with("_2gpu"));
        let xla = extract_bool(attrs, "xla")?.unwrap_or(false);

        Ok(Box::new(TfCcTestInstance {
            name,
            srcs,
            deps,
            copts,
            linkopts,
            defines,
            data,
            gpu,
            xla,
        }))
    }

    fn documentation(&self) -> &str {
        "TensorFlow C++ test rule"
    }
}

struct TfCcTestInstance {
    name: String,
    srcs: Vec<String>,
    deps: Vec<String>,
    copts: Vec<String>,
    linkopts: Vec<String>,
    defines: Vec<String>,
    /// `data` — runtime deps republished into runfiles + built via deps
    /// (e.g. distributed_runtime TestCluster tests pull grpc_testlib_server
    /// through a library's `data`).
    data: Vec<String>,
    gpu: bool,
    xla: bool,
}

#[async_trait]
impl RuleInstance for TfCcTestInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        let mut deps = self.deps.clone();

        for s in &self.srcs {
            if (s.starts_with(':') || s.starts_with("//") || s.starts_with('@'))
                && !deps.contains(s)
            {
                deps.push(s.clone());
            }
        }

        // Bare srcs entries are Bazel file labels too. Add an implicit
        // same-package edge so a clean-tree generated .cc/header producer is
        // analyzed before FilesInfo expansion; checked-in files use the
        // orchestrator's normal source-file fallback.
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

        // `data` labels are dependencies too — Bazel builds them and
        // stages outputs into the test's runfiles. Register label-shaped
        // entries so they get built and reach analyze() for runfiles.
        for d in &self.data {
            if (d.starts_with(':') || d.starts_with("//") || d.starts_with('@'))
                && !deps.contains(d)
            {
                deps.push(d.clone());
            }
        }

        // Add test framework dependencies
        deps.push("//testing:gtest_main".to_string());

        // Add GPU dependencies if enabled
        if self.gpu {
            deps.push("//tensorflow/core:gpu_runtime".to_string());
        }

        // Add XLA dependencies if enabled
        if self.xla {
            deps.push("//tensorflow/compiler/xla:xla".to_string());
        }

        deps
    }

    fn validate_analysis_dependencies(
        &self,
        ctx: &rush_rules::analysis_context::AnalysisContext,
    ) -> Result<()> {
        reject_framework_include_roots(
            ctx.labeled_dep_providers().iter().map(|(_, provider)| provider.as_ref()),
        )
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        reject_framework_include_roots(
            context.dependency_providers_ordered.iter().map(|(_, provider)| provider).chain(context.dependency_providers.values()),
        )?;
        let mut artifacts = Vec::new();

        // Compile flags
        let mut compile_flags = context.toolchain.cxxflags.clone();
        compile_flags.extend(self.copts.iter().cloned());

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

        // Consume only include roots exported through CcInfo. Inferring one
        // `-I` from every transitive header parent can make dependency headers
        // shadow host headers (for example BoringSSL's openssl/time.h shadows
        // libc's <time.h> when .../include/openssl is added directly).
        let mut include_paths = Vec::new();
        // tf_cc_test.rs::build() otherwise has a WEAKER linker than cc_test — it collected
        // only linker_inputs (no alwayslink whole-archive, no link_fragments, dropping the pinned
        // absl `link_fragments` group + the framework/protobuf/gtest archives), leaving ~438
        // protobuf/absl/gtest/nsync/benchmark undefineds for the GPU test. Mirror CcTestInstance's
        // collection (rules_cc.rs:2336): split alwayslink→whole-archive vs normal, and consume
        // link_fragments for every test with dependencies.
        let rocm_enabled = std::env::var("RUSH_ENABLE_ROCM").as_deref() == Ok("1");
        let structured_link = !context.dependency_providers_ordered.is_empty();
        let mut response_plan = TfCcTestResponsePlan::default();
        if let Some(reg_arc) = context.artifact_registry.as_ref() {
            let reg = reg_arc.lock().await;
            let ordered_cc_infos: Vec<_> = context
                .dependency_providers_ordered
                .iter()
                .filter_map(|(_, providers)| providers.cc_info.clone())
                .collect();
            for cc_info in &ordered_cc_infos {
                for path_id in cc_info.compilation.include_dirs.iter() {
                    include_paths.push(reg.paths().path(path_id).to_path_buf());
                }
            }
            if structured_link {
                response_plan = assemble_tf_cc_test_response_plan(&ordered_cc_infos, &reg);
            }
        }

        // Deduplicate include paths
        include_paths.sort();
        include_paths.dedup();

        for include_path in &include_paths {
            compile_flags.push(format!("-I{}", include_path.display()));
        }

        // Bazel compiles TensorFlow tests from an execution root where the
        // workspace, generated files, XLA, and TSL package roots are visible.
        // These cannot be reconstructed safely from individual header parents.
        for root in [
            context.workspace_root.clone(),
            context.output_path("genfiles"),
            context.workspace_root.join("third_party/xla"),
            context
                .workspace_root
                .join("third_party/xla/third_party/tsl"),
        ] {
            let flag = format!("-I{}", root.display());
            if !compile_flags.contains(&flag) {
                compile_flags.push(flag);
            }
        }

        // ROCm host-wide defines (tf_cc_test bypasses global copt injection).
        if std::env::var("RUSH_ENABLE_ROCM").as_deref() == Ok("1") {
            for f in [
                "-DTENSORFLOW_USE_ROCM=1",
                "-D__HIP_PLATFORM_AMD__",
                "-DEIGEN_USE_HIP",
                "-I/opt/rocm/include",
            ] {
                compile_flags.push(f.to_string());
            }
        }

        // Compile each source file (expand cross-package filegroup labels).
        let bc_pkg = context
            .package_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let expanded_srcs =
            expand_test_srcs(&self.srcs, &context.workspace_root, &bc_pkg, |label| {
                context.get_dependency_files(label)
            });
        let target_output_dir = context.target_output_dir(&self.name);
        let mut seen_source_paths = HashSet::new();
        for src in &expanded_srcs {
            if !crate::is_tf_cc_compilable_source(src) {
                continue;
            }
            let src_path = resolve_test_input_path(&context.workspace_root, &bc_pkg, src);
            if !seen_source_paths.insert(src_path.clone()) {
                continue;
            }
            let obj_name = crate::tf_cc_obj_name_for_resolved_path(
                &src_path,
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
            cmd.arg("-c").arg(&src_path).arg("-o").arg(&obj_path);

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

        // Link test binary
        let test_path = target_output_dir.join("test").join(&self.name);

        // Create test directory
        if let Some(parent) = test_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Build link command
        let mut cmd = tokio::process::Command::new(&context.toolchain.cxx);
        // #39 link-speed: parallel linker flag at index 0 (matches the
        // analyze/legacy CcLink argv ordering).
        if let Some(f) = rush_rules::rules_cc::fast_linker_driver_flag() {
            cmd.arg(f);
        }
        cmd.arg("-o").arg(&test_path);

        // Add object files
        for artifact in &artifacts {
            if matches!(artifact.artifact_type, ArtifactType::Object) {
                cmd.arg(&artifact.path);
            }
        }

        // Add libraries.
        if structured_link {
            // Full closure (cc_test parity): alwayslink archives whole-archived, the rest in a
            // --start-group so back-references resolve regardless of order (clears the ~438
            // undefineds), then the link_fragment flags (-L dirs, raw flags). The transitive .a
            // closure for a TF GPU test is enormous (thousands of archives) → passing it directly
            // overflows ARG_MAX (E2BIG); write it to a linker RESPONSE FILE and pass @file (gcc
            // expands it). (Codex 019eebc6.)
            let rsp = response_plan.render();
            let rsp_path = target_output_dir
                .join("test")
                .join(format!("{}.link.rsp", self.name));
            if let Some(parent) = rsp_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(&rsp_path, rsp).await?;
            cmd.arg(format!("@{}", rsp_path.display()));
            // The GPU test's .cu.cc kernels + the ROCm StreamExecutor reference the ROCm runtime
            // (libamdhip64, librocblas, …). The @local_config_rocm//rocm:hip/:rocblas linkopts
            // don't reliably flow into the test's link_fragments, so link the runtime explicitly
            // (-rpath lets the binary find sonames at run time). -l comes
            // after the archive closure so the static refs resolve. (Codex 019eebc6.)
            if rocm_enabled {
                cmd.arg("-L/opt/rocm/lib")
                    .arg("-Wl,-rpath,/opt/rocm/lib")
                    .arg("-lamdhip64")
                    .arg("-lrocblas");
            }
        }

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
            path: test_path,
            artifact_type: ArtifactType::Executable,
        });

        Ok(artifacts)
    }

    /// Declare one CppCompile per translation unit and one final CcLink.
    /// Typed dependency link inputs remain declared even when the response-file
    /// runtime shape forces legacy execution.
    fn analyze(
        &self,
        ctx: &mut rush_rules::analysis_context::AnalysisContext,
        dep_providers: &[std::sync::Arc<rush_rules::providers::ProviderSet>],
    ) -> ProviderSet {
        if let Err(error) = reject_framework_include_roots(
            dep_providers.iter().map(|provider| provider.as_ref()),
        ) {
            ctx.fail_native_namespace_analysis(error.to_string());
            return ProviderSet::empty();
        }
        use rush_action::ids::{ArtifactKind, ExecutableAction, StringId};
        use std::path::Path;
        {
            let dep_cc_infos = rush_rules::providers::extract_cc_infos(dep_providers);

            // Include search roots and header action inputs are separate:
            // CcInfo.include_dirs controls compiler lookup, while the complete
            // header set below remains declared for scheduling and caching.
            let mut dep_include_dirs: Vec<std::path::PathBuf> = Vec::new();
            {
                let reg = ctx.registry();
                for dep_cc in dep_cc_infos.iter() {
                    for path_id in dep_cc.compilation.include_dirs.iter() {
                        let path = reg.paths().path(path_id).to_path_buf();
                        if !dep_include_dirs.contains(&path) {
                            dep_include_dirs.push(path);
                        }
                    }
                }
            }
            dep_include_dirs.sort();
            dep_include_dirs.dedup();

            let cxx_pathbuf = ctx.toolchain().cxx.clone();
            let cc_pathbuf = ctx.toolchain().cc.clone();

            let ws_root_pb = ctx.workspace_root().to_path_buf();
            let consumer_pkg = ctx.package().to_string();
            let expanded_srcs = expand_test_srcs(&self.srcs, &ws_root_pb, &consumer_pkg, |label| {
                ctx.get_dependency_provider(label)
                    .and_then(|p| p.get_files())
                    .map(|fi| fi.files.clone())
            });
            // tf_cc_test has no separate hdrs/textual_hdrs attributes. Bazel
            // permits header-like entries in srcs. Register those at their
            // absolute, package-qualified path without treating other non-TU
            // entries as headers. If an entry is a generated filegroup or genrule
            // output, path-keyed interning returns the producer's existing
            // generated ArtifactId instead of reclassifying it as a source.
            let mut compile_header_inputs = Vec::new();
            for header in expanded_srcs
                .iter()
                .filter(|src| crate::is_tf_cc_header_input(src))
            {
                let path = resolve_test_input_path(&ws_root_pb, &consumer_pkg, header);
                compile_header_inputs
                    .push(ctx.register_source_artifact(&path, ArtifactKind::Header));
            }
            compile_header_inputs.extend(collect_dep_header_inputs(ctx.registry(), &dep_cc_infos));
            let mut seen_headers = HashSet::new();
            compile_header_inputs.retain(|aid| seen_headers.insert(*aid));
            compile_header_inputs.sort_by(|left, right| {
                ctx.registry()
                    .artifact_path(*left)
                    .cmp(ctx.registry().artifact_path(*right))
            });

            let mut obj_artifacts: Vec<_> = Vec::with_capacity(expanded_srcs.len());
            let mut seen_sources = HashSet::new();
            for src in &expanded_srcs {
                if !crate::is_tf_cc_compilable_source(src) {
                    continue;
                }
                let src_path_abs = resolve_test_input_path(&ws_root_pb, &consumer_pkg, src);
                // Path-keyed interning also preserves the producer edge for a
                // generated translation unit expanded from a labeled src.
                let src_aid = ctx.register_source_artifact(&src_path_abs, ArtifactKind::Source);
                if !seen_sources.insert(src_aid) {
                    continue;
                }
                let obj_name = crate::tf_cc_obj_name_for_resolved_path(
                    &src_path_abs,
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
                let mut compile_inputs = vec![src_aid];
                compile_inputs.extend(
                    compile_header_inputs
                        .iter()
                        .copied()
                        .filter(|aid| *aid != src_aid),
                );
                let compile_id = ctx
                    .declare_action("CppCompile", compile_inputs, vec![obj_aid])
                    .expect("tf_cc_test CppCompile output must have one producer");
                {
                    let obj_path_str = ctx.registry().artifact_path(obj_aid).display().to_string();

                    // tf_cc_test compile argv (matches build()):
                    //   cxx -c <src> -o <obj> <copts> <-D defines>
                    //   [-DGOOGLE_CUDA=1] [-DTENSORFLOW_USE_XLA=1]
                    //   <-I dep CcInfo include roots>
                    let mut argv_strs: Vec<String> = Vec::new();
                    argv_strs.push("-c".to_string());
                    argv_strs.push(src_path_abs.display().to_string());
                    argv_strs.push("-o".to_string());
                    argv_strs.push(obj_path_str);
                    if src.ends_with(".cc") || src.ends_with(".cpp") {
                        argv_strs.extend(ctx.toolchain().cxxflags.iter().cloned());
                    } else {
                        argv_strs.extend(ctx.toolchain().cflags.iter().cloned());
                    }
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
                    for root in [
                        ctx.workspace_root().to_path_buf(),
                        ctx.predicted_output_path(Path::new("genfiles")),
                        ctx.workspace_root().join("third_party/xla"),
                        ctx.workspace_root().join("third_party/xla/third_party/tsl"),
                    ] {
                        let flag = format!("-I{}", root.display());
                        if !argv_strs.contains(&flag) {
                            argv_strs.push(flag);
                        }
                    }
                    // ROCm host-wide defines (parity with the runtime path).
                    if std::env::var("RUSH_ENABLE_ROCM").as_deref() == Ok("1") {
                        for f in [
                            "-DTENSORFLOW_USE_ROCM=1",
                            "-D__HIP_PLATFORM_AMD__",
                            "-DEIGEN_USE_HIP",
                            "-I/opt/rocm/include",
                        ] {
                            argv_strs.push(f.to_string());
                        }
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
            let unsupported_reason = (!dep_providers.is_empty())
                .then_some(RESPONSE_FILE_LINK_UNSUPPORTED)
                .or(link_plan.unsupported_reason)
                .or_else(|| crate::tf_cc_link::unsupported_raw_link_args(&self.linkopts))
                .or_else(|| {
                    (obj_artifacts.is_empty() && !link_plan.has_typed_link_input())
                        .then_some(EMPTY_LINK_UNSUPPORTED)
                });

            let mut link_inputs: Vec<rush_action::ids::ArtifactId> = obj_artifacts.clone();
            link_inputs.extend(link_plan.declared_inputs.iter().copied());
            let bin_rel = ctx.target_output_rel(&self.name, &format!("test/{}", self.name));
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
            .expect("tf_cc_test CcLink output must have one producer");
            if unsupported_reason.is_none() {
                let obj_path_strs: Vec<String> = obj_artifacts
                    .iter()
                    .map(|aid| ctx.registry().artifact_path(*aid).display().to_string())
                    .collect();
                let bin_path_str = ctx.registry().artifact_path(bin_aid).display().to_string();

                // The scheduler-supported no-provider shape links inline:
                //   cxx -o <test> <objs...> <typed link plan>
                //   <linkopts...> -lpthread -ldl -lm
                let mut argv_strs: Vec<String> = Vec::new();
                // #39 link-speed: drive a parallel linker (mold/lld) for this
                // monolithic-static test link, matching the cc_binary path.
                // Honors RUSH_LINKER; None (bfd) leaves argv unchanged. Pushed
                // at index 0 so this analyze argv and the legacy CcLink spec
                // below stay byte-identical for the PASS-B verifier.
                if let Some(f) = rush_rules::rules_cc::fast_linker_driver_flag() {
                    argv_strs.push(f);
                }
                argv_strs.push("-o".to_string());
                argv_strs.push(bin_path_str);
                for o in &obj_path_strs {
                    argv_strs.push(o.clone());
                }
                argv_strs.extend(link_plan.argv.iter().cloned());
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

            ProviderSet::empty().with_default_info(rush_rules::providers::DefaultInfo {
                files: vec![bin_aid],
            })
        }
    }

    fn rule_type(&self) -> &str {
        "tf_cc_test"
    }

    /// Phase 5.C.4 commit 30: legacy specs for the verifier.
    /// Mirrors build()'s argv emission. Differs from tf_cc_binary
    /// (commit 29) in link-argv ordering: `-o <test>` appears
    /// BEFORE objs (build() does cmd.arg("-o").arg(&test_path)
    /// before adding objs).
    async fn build_legacy_command_specs(
        &self,
        context: &BuildContext,
    ) -> Vec<(
        rush_rules::SchedulerActionKey,
        rush_action::command_spec::NormalizedCommandSpec,
    )> {
        if let Err(error) = reject_framework_include_roots(
            context.dependency_providers_ordered.iter().map(|(_, provider)| provider).chain(context.dependency_providers.values()),
        ) {
            eprintln!("native C++ command specification refused: {error:#}");
            return Vec::new();
        }
        use rush_action::command_spec::NormalizedCommandSpec;
        use rush_rules::SchedulerActionKey;
        let mut specs = Vec::new();

        let mut dep_include_dirs: Vec<std::path::PathBuf> = Vec::new();
        let mut dep_header_paths: Vec<std::path::PathBuf> = Vec::new();
        let mut link_plan = crate::tf_cc_link::TfCcLinkPlan::default();
        let mut link_input_paths = Vec::new();
        let mut response_plan = TfCcTestResponsePlan::default();
        let structured_link = !context.dependency_providers_ordered.is_empty();
        if let Some(reg_arc) = context.artifact_registry.as_ref() {
            let reg = reg_arc.lock().await;
            // Resolve the same complete dependency-header set as analyze().
            let cc_infos: Vec<std::sync::Arc<rush_rules::providers::CcInfo>> = context
                .dependency_providers_ordered
                .iter()
                .filter_map(|(_, providers)| providers.cc_info.clone())
                .collect();
            dep_header_paths = collect_dep_header_inputs(&reg, &cc_infos)
                .into_iter()
                .map(|aid| reg.artifact_path(aid).to_path_buf())
                .collect();
            link_plan = crate::tf_cc_link::assemble_tf_cc_link_plan(&cc_infos, &reg);
            link_input_paths = link_plan
                .declared_inputs
                .iter()
                .map(|artifact| reg.artifact_path(*artifact).to_path_buf())
                .collect();
            if structured_link {
                response_plan = assemble_tf_cc_test_response_plan(&cc_infos, &reg);
            }
            for cc_info in &cc_infos {
                for path_id in cc_info.compilation.include_dirs.iter() {
                    dep_include_dirs.push(reg.paths().path(path_id).to_path_buf());
                }
            }
        }
        dep_include_dirs.sort();
        dep_include_dirs.dedup();

        let target_output_dir = context.target_output_dir(&self.name);
        let mut object_paths: Vec<std::path::PathBuf> = Vec::new();
        let mut seen_source_paths = HashSet::new();
        let bc_pkg_legacy = context
            .package_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let expanded_srcs = expand_test_srcs(
            &self.srcs,
            &context.workspace_root,
            &bc_pkg_legacy,
            |label| context.get_dependency_files(label),
        );
        let mut compile_header_paths: Vec<PathBuf> = expanded_srcs
            .iter()
            .filter(|src| crate::is_tf_cc_header_input(src))
            .map(|header| resolve_test_input_path(&context.workspace_root, &bc_pkg_legacy, header))
            .collect();
        compile_header_paths.extend(dep_header_paths);
        compile_header_paths.sort();
        compile_header_paths.dedup();

        for src in &expanded_srcs {
            if !crate::is_tf_cc_compilable_source(src) {
                continue;
            }
            let src_path = resolve_test_input_path(&context.workspace_root, &bc_pkg_legacy, src);
            if !seen_source_paths.insert(src_path.clone()) {
                continue;
            }
            let obj_name = crate::tf_cc_obj_name_for_resolved_path(
                &src_path,
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
            if src.ends_with(".cc") || src.ends_with(".cpp") {
                argv.extend(context.toolchain.cxxflags.iter().cloned());
            } else {
                argv.extend(context.toolchain.cflags.iter().cloned());
            }
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
            for root in [
                context.workspace_root.clone(),
                context.output_path("genfiles"),
                context.workspace_root.join("third_party/xla"),
                context
                    .workspace_root
                    .join("third_party/xla/third_party/tsl"),
            ] {
                let flag = format!("-I{}", root.display());
                if !argv.contains(&flag) {
                    argv.push(flag);
                }
            }
            // ROCm host-wide defines (legacy-spec parity with runtime/analyze).
            if std::env::var("RUSH_ENABLE_ROCM").as_deref() == Ok("1") {
                for f in [
                    "-DTENSORFLOW_USE_ROCM=1",
                    "-D__HIP_PLATFORM_AMD__",
                    "-DEIGEN_USE_HIP",
                    "-I/opt/rocm/include",
                ] {
                    argv.push(f.to_string());
                }
            }

            let mut declared_inputs = vec![src_path.clone()];
            declared_inputs.extend(
                compile_header_paths
                    .iter()
                    .filter(|path| *path != &src_path)
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

        // The legacy spec records the actual transport argv. Dependency-backed
        // tests use the same response body as build(); the typed action remains
        // incomplete because ExecutableAction cannot materialize that file.
        let bin_path = target_output_dir.join("test").join(&self.name);
        let response_path = target_output_dir
            .join("test")
            .join(format!("{}.link.rsp", self.name));
        let mut link_argv: Vec<String> = Vec::new();
        if let Some(f) = rush_rules::rules_cc::fast_linker_driver_flag() {
            link_argv.push(f);
        }
        link_argv.push("-o".to_string());
        link_argv.push(bin_path.display().to_string());
        for o in &object_paths {
            link_argv.push(o.display().to_string());
        }
        let param_files = if structured_link {
            let response = response_plan.render();
            link_argv.push(format!("@{}", response_path.display()));
            if std::env::var("RUSH_ENABLE_ROCM").as_deref() == Ok("1") {
                link_argv.extend([
                    "-L/opt/rocm/lib".to_string(),
                    "-Wl,-rpath,/opt/rocm/lib".to_string(),
                    "-lamdhip64".to_string(),
                    "-lrocblas".to_string(),
                ]);
            }
            vec![(
                response_path,
                blake3::hash(response.as_bytes()).to_hex().to_string(),
            )]
        } else {
            link_argv.extend(link_plan.argv.iter().cloned());
            Vec::new()
        };
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
            param_files,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use rush_action::ids::{
        ActionDeclarationStatus, ArtifactId, ArtifactKey, ArtifactKind, ArtifactRegistry, ConfigId,
        DeclaredActionId, LinkerInputData, LinkerInputExtras, LinkerInputId, TargetId,
    };
    use rush_rules::analysis_context::{AnalysisContext, StubIncludeResolver};
    use rush_rules::context::Toolchain;
    use rush_rules::providers::{CcInfo, DirectCcInfo, FilesInfo, LinkFragment};
    use std::sync::Arc;

    fn register_artifact(
        registry: &mut ArtifactRegistry,
        path: &Path,
        kind: ArtifactKind,
        is_source: bool,
        owner: TargetId,
    ) -> ArtifactId {
        let exec_path = registry.intern_path(path);
        registry.intern_artifact(
            ArtifactKey {
                config: ConfigId::DEFAULT,
                exec_path,
            },
            kind,
            is_source,
            owner,
            None,
        )
    }

    fn no_op_toolchain() -> Toolchain {
        let mut toolchain = Toolchain::default();
        toolchain.cc = PathBuf::from("true");
        toolchain.cxx = PathBuf::from("true");
        toolchain.ar = PathBuf::from("true");
        toolchain
    }

    fn register_linker_input(
        registry: &mut ArtifactRegistry,
        artifact: ArtifactId,
        alwayslink: bool,
        extras: Option<Arc<LinkerInputExtras>>,
    ) -> LinkerInputId {
        registry.intern_linker_input(LinkerInputData {
            owner: TargetId(1),
            artifact,
            alwayslink,
            extras,
        })
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
    fn response_plan_preserves_runtime_bucket_order() {
        let mut registry = ArtifactRegistry::new();
        let whole = register_artifact(
            &mut registry,
            Path::new("/lib/whole.a"),
            ArtifactKind::StaticLibrary,
            false,
            TargetId(1),
        );
        let normal_z = register_artifact(
            &mut registry,
            Path::new("/lib/z_normal.a"),
            ArtifactKind::StaticLibrary,
            false,
            TargetId(1),
        );
        let typed = register_artifact(
            &mut registry,
            Path::new("/lib/typed.a"),
            ArtifactKind::StaticLibrary,
            false,
            TargetId(1),
        );
        let grouped = register_artifact(
            &mut registry,
            Path::new("/lib/grouped.a"),
            ArtifactKind::StaticLibrary,
            false,
            TargetId(1),
        );
        let normal_a = register_artifact(
            &mut registry,
            Path::new("/lib/a_normal.a"),
            ArtifactKind::StaticLibrary,
            false,
            TargetId(1),
        );
        let whole_input = register_linker_input(&mut registry, whole, true, None);
        let normal_z_input = register_linker_input(&mut registry, normal_z, false, None);
        let typed_input = register_linker_input(&mut registry, typed, false, None);
        let grouped_input = register_linker_input(&mut registry, grouped, false, None);
        let normal_a_input = register_linker_input(&mut registry, normal_a, false, None);
        let raw_archive = registry.intern_string("/raw/libraw.a");
        let redundant_group = registry.intern_string("-Wl,--start-group");
        let pthread = registry.intern_string("-pthread");
        let search = registry.intern_path(Path::new("/search"));

        let first = CcInfo::from_direct_and_deps(
            DirectCcInfo {
                linker_inputs: vec![normal_z_input, whole_input],
                link_fragments: vec![
                    LinkFragment::Flag(raw_archive),
                    LinkFragment::Flag(redundant_group),
                    LinkFragment::Library(typed_input),
                    LinkFragment::Group(vec![grouped_input, grouped_input].into_boxed_slice()),
                    LinkFragment::Flag(pthread),
                    LinkFragment::SearchDir(search),
                ],
                ..DirectCcInfo::default()
            },
            std::iter::empty(),
        );
        let second = CcInfo::from_direct_and_deps(
            DirectCcInfo {
                linker_inputs: vec![normal_a_input, whole_input],
                ..DirectCcInfo::default()
            },
            std::iter::empty(),
        );

        let plan = assemble_tf_cc_test_response_plan(&[first, second], &registry);

        assert_eq!(
            plan.render(),
            "-Wl,--whole-archive\n\
/lib/whole.a\n\
-Wl,--no-whole-archive\n\
-Wl,--start-group\n\
/lib/z_normal.a\n\
/raw/libraw.a\n\
/lib/typed.a\n\
/lib/grouped.a\n\
/lib/a_normal.a\n\
-Wl,--end-group\n\
-pthread\n\
-L/search\n"
        );
    }

    #[test]
    fn test_parse_tf_cc_test() {
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("test".to_string()));
        attrs.insert(
            "srcs".to_string(),
            Value::List(vec![Value::String("test.cc".to_string())]),
        );

        let rule = TfCcTestRule;
        let instance = rule.parse_attributes(&attrs).unwrap();

        assert_eq!(instance.target_name(), "test");
        assert!(instance.dependencies().contains(&":test.cc".to_string()));
        // Should have gtest_main added automatically
        assert!(instance
            .dependencies()
            .contains(&"//testing:gtest_main".to_string()));
    }

    #[tokio::test]
    async fn declared_output_paths_match_and_rsp_is_target_qualified() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("ws");
        let output = temp.path().join("out");
        let package = "pkg";
        for directory in ["foo", "bar"] {
            let source_dir = workspace.join(package).join(directory);
            std::fs::create_dir_all(&source_dir).unwrap();
            std::fs::write(source_dir.join("same.cc"), "int f() { return 0; }\n").unwrap();
        }

        let rule = TfCcTestInstance {
            name: "same_test".to_string(),
            srcs: vec![
                "foo/same.cc".to_string(),
                "bar/same.cc".to_string(),
                "foo/same.cc".to_string(),
            ],
            deps: vec!["//dep:stub".to_string()],
            copts: Vec::new(),
            linkopts: Vec::new(),
            defines: Vec::new(),
            data: Vec::new(),
            gpu: false,
            xla: false,
        };
        let resolver = StubIncludeResolver::new(workspace.clone());
        let toolchain = no_op_toolchain();
        let dep_provider = ProviderSet::empty().with_cc_info(CcInfo::empty());
        let dep_providers = vec![Arc::new(dep_provider.clone())];
        let mut registry = ArtifactRegistry::new();
        let target = TargetId(1);
        {
            let mut context = AnalysisContext::new(
                target,
                ConfigId::DEFAULT,
                package,
                None,
                &output,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace,
                &[],
            );
            let _ = rule.analyze(&mut context, &dep_providers);
        }
        let link_ids = action_ids_with_mnemonic(&registry, target, "CcLink");
        assert_eq!(link_ids.len(), 1);
        let link_id = link_ids[0];
        assert!(matches!(
            &registry.declared_action(link_id).status,
            ActionDeclarationStatus::Incomplete { reason }
                if *reason == RESPONSE_FILE_LINK_UNSUPPORTED
        ));
        assert!(registry.executable_action(link_id).is_none());
        let mut analyzed_outputs: Vec<PathBuf> = registry
            .actions_of(target)
            .iter()
            .flat_map(|id| registry.declared_action(*id).outputs.iter())
            .map(|id| registry.artifact_path(*id).to_path_buf())
            .collect();
        analyzed_outputs.sort();
        assert_eq!(analyzed_outputs.len(), 3);

        let registry = Arc::new(tokio::sync::Mutex::new(registry));
        let context = BuildContext::new(workspace, output.clone())
            .with_package(PathBuf::from(package))
            .with_toolchain(toolchain)
            .with_artifact_registry(Arc::clone(&registry))
            .with_dependencies(
                HashMap::new(),
                vec![("//dep:stub".to_string(), dep_provider)],
            );
        let mut runtime_outputs: Vec<PathBuf> = rule
            .build(&context)
            .await
            .unwrap()
            .into_iter()
            .map(|artifact| artifact.path)
            .collect();
        runtime_outputs.sort();
        assert_eq!(runtime_outputs, analyzed_outputs);
        let target_dir = output.join(package).join("same_test");
        let response_path = target_dir.join("test/same_test.link.rsp");
        assert!(response_path.is_file());
        assert_eq!(std::fs::read_to_string(&response_path).unwrap(), "");
        assert!(!output.join("test/same_test.link.rsp").exists());

        let legacy_specs = rule.build_legacy_command_specs(&context).await;
        let legacy_link = legacy_specs
            .iter()
            .find_map(|(_, spec)| (spec.mnemonic == "CcLink").then_some(spec))
            .expect("legacy response-file link spec");
        assert!(legacy_link
            .argv
            .contains(&format!("@{}", response_path.display())));
        assert_eq!(
            legacy_link.param_files,
            vec![(response_path, blake3::hash(b"").to_hex().to_string())]
        );
        let mut legacy_outputs: Vec<PathBuf> = legacy_specs
            .into_iter()
            .flat_map(|(_, spec)| spec.declared_outputs)
            .collect();
        legacy_outputs.sort();
        assert_eq!(legacy_outputs, analyzed_outputs);
        assert_eq!(
            analyzed_outputs
                .iter()
                .filter(|path| path.starts_with(target_dir.join("obj")))
                .count(),
            2,
        );
        assert!(analyzed_outputs.contains(&target_dir.join("test/same_test")));
    }

    #[tokio::test]
    async fn fragment_only_response_link_declares_inputs_and_matches_legacy() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("ws");
        let output = temp.path().join("out");
        let package = "pkg";
        let resolver = StubIncludeResolver::new(workspace.clone());
        let toolchain = no_op_toolchain();
        let mut registry = ArtifactRegistry::new();

        let (library, linkstamp, linker_input) = {
            let mut producer = AnalysisContext::new(
                TargetId(1),
                ConfigId::DEFAULT,
                "dep",
                None,
                &output,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace,
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
                .declare_action("GenerateLinkInputs", Vec::new(), vec![library, linkstamp])
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
        let cc_info = CcInfo::from_direct_and_deps(
            DirectCcInfo {
                link_fragments: vec![LinkFragment::Library(linker_input)],
                ..DirectCcInfo::default()
            },
            std::iter::empty(),
        );
        let dep_provider = ProviderSet::empty().with_cc_info(cc_info);
        let dep_providers = vec![Arc::new(dep_provider.clone())];
        let rule = TfCcTestInstance {
            name: "test".to_string(),
            srcs: Vec::new(),
            deps: vec!["//dep:fragment".to_string()],
            copts: Vec::new(),
            linkopts: Vec::new(),
            defines: Vec::new(),
            data: Vec::new(),
            gpu: false,
            xla: false,
        };
        let target = TargetId(2);
        let providers = {
            let mut consumer = AnalysisContext::new(
                target,
                ConfigId::DEFAULT,
                package,
                None,
                &output,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace,
                &[],
            );
            rule.analyze(&mut consumer, &dep_providers)
        };

        let link_ids = action_ids_with_mnemonic(&registry, target, "CcLink");
        assert_eq!(link_ids.len(), 1);
        let link_id = link_ids[0];
        let link_action = registry.declared_action(link_id);
        assert_eq!(link_action.inputs.as_ref(), &[library, linkstamp]);
        assert!(matches!(
            &link_action.status,
            ActionDeclarationStatus::Incomplete { reason }
                if *reason == RESPONSE_FILE_LINK_UNSUPPORTED
        ));
        assert!(registry.executable_action(link_id).is_none());
        assert_eq!(
            providers.get_default_info().unwrap().files.as_slice(),
            link_action.outputs.as_ref()
        );

        let library_path = registry.artifact_path(library).to_path_buf();
        let linkstamp_path = registry.artifact_path(linkstamp).to_path_buf();
        let registry = Arc::new(tokio::sync::Mutex::new(registry));
        let context = BuildContext::new(workspace, output.clone())
            .with_package(PathBuf::from(package))
            .with_toolchain(toolchain)
            .with_dependencies(
                HashMap::new(),
                vec![("//dep:fragment".to_string(), dep_provider)],
            )
            .with_artifact_registry(Arc::clone(&registry));
        let legacy_specs = rule.build_legacy_command_specs(&context).await;
        let legacy_link = legacy_specs
            .iter()
            .find_map(|(_, spec)| (spec.mnemonic == "CcLink").then_some(spec))
            .expect("legacy link spec");
        let mut expected_inputs = vec![library_path.clone(), linkstamp_path.clone()];
        expected_inputs.sort();
        assert_eq!(legacy_link.declared_inputs, expected_inputs);
        assert!(!legacy_link
            .argv
            .contains(&linkstamp_path.display().to_string()));

        let target_dir = output.join(package).join("test");
        let binary_path = target_dir.join("test/test");
        let response_path = target_dir.join("test/test.link.rsp");
        let response = format!(
            "-Wl,--start-group\n{}\n-Wl,--end-group\n",
            library_path.display()
        );
        let mut expected_argv = Vec::new();
        if let Some(flag) = rush_rules::rules_cc::fast_linker_driver_flag() {
            expected_argv.push(flag);
        }
        expected_argv.extend([
            "-o".to_string(),
            binary_path.display().to_string(),
            format!("@{}", response_path.display()),
            "-lpthread".to_string(),
            "-ldl".to_string(),
            "-lm".to_string(),
        ]);
        assert_eq!(legacy_link.argv, expected_argv);
        assert_eq!(
            legacy_link.param_files,
            vec![(
                response_path.clone(),
                blake3::hash(response.as_bytes()).to_hex().to_string()
            )]
        );

        let runtime_outputs = rule.build(&context).await.unwrap();
        assert_eq!(runtime_outputs.len(), 1);
        assert_eq!(runtime_outputs[0].path, binary_path);
        assert_eq!(std::fs::read_to_string(response_path).unwrap(), response);
    }

    #[tokio::test]
    async fn empty_test_declares_incomplete_link_and_legacy_spec() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("ws");
        let output = temp.path().join("out");
        let resolver = StubIncludeResolver::new(workspace.clone());
        let toolchain = no_op_toolchain();
        let mut registry = ArtifactRegistry::new();
        let rule = TfCcTestInstance {
            name: "empty".to_string(),
            srcs: Vec::new(),
            deps: Vec::new(),
            copts: Vec::new(),
            linkopts: Vec::new(),
            defines: Vec::new(),
            data: Vec::new(),
            gpu: false,
            xla: false,
        };
        let target = TargetId(1);
        let providers = {
            let mut context = AnalysisContext::new(
                target,
                ConfigId::DEFAULT,
                "pkg",
                None,
                &output,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace,
                &[],
            );
            rule.analyze(&mut context, &[])
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
        let context = BuildContext::new(workspace, output)
            .with_package(PathBuf::from("pkg"))
            .with_toolchain(toolchain)
            .with_artifact_registry(registry);
        let legacy_links: Vec<_> = rule
            .build_legacy_command_specs(&context)
            .await
            .into_iter()
            .filter(|(_, spec)| spec.mnemonic == "CcLink")
            .collect();
        assert_eq!(legacy_links.len(), 1);
        assert!(legacy_links[0].1.declared_inputs.is_empty());
        assert!(legacy_links[0].1.param_files.is_empty());
    }

    #[tokio::test]
    async fn inline_no_provider_link_matches_legacy() {
        let workspace = PathBuf::from("/ws");
        let output = PathBuf::from("/out");
        let resolver = StubIncludeResolver::new(workspace.clone());
        let toolchain = Toolchain::default();
        let mut registry = ArtifactRegistry::new();
        let rule = TfCcTestInstance {
            name: "test".to_string(),
            srcs: vec!["test.cc".to_string()],
            deps: Vec::new(),
            copts: Vec::new(),
            linkopts: Vec::new(),
            defines: Vec::new(),
            data: Vec::new(),
            gpu: false,
            xla: false,
        };
        let target = TargetId(1);
        {
            let mut context = AnalysisContext::new(
                target,
                ConfigId::DEFAULT,
                "pkg",
                None,
                &output,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace,
                &[],
            );
            rule.analyze(&mut context, &[]);
        }

        let link_ids = action_ids_with_mnemonic(&registry, target, "CcLink");
        assert_eq!(link_ids.len(), 1);
        let link_id = link_ids[0];
        assert!(matches!(
            &registry.declared_action(link_id).status,
            ActionDeclarationStatus::Complete
        ));
        let scheduler_spec =
            rush_action::command_spec::NormalizedCommandSpec::from_registry_action(
                &registry, link_id,
            )
            .expect("inline link has an executable payload");
        assert!(scheduler_spec.param_files.is_empty());

        let registry = Arc::new(tokio::sync::Mutex::new(registry));
        let context = BuildContext::new(workspace, output)
            .with_package(PathBuf::from("pkg"))
            .with_toolchain(toolchain)
            .with_artifact_registry(registry);
        let legacy_link = rule
            .build_legacy_command_specs(&context)
            .await
            .into_iter()
            .find_map(|(_, spec)| (spec.mnemonic == "CcLink").then_some(spec))
            .expect("legacy inline link spec");
        assert_eq!(legacy_link, scheduler_spec);
    }

    #[test]
    fn raw_direct_linkopt_declares_incomplete_link() {
        let workspace = PathBuf::from("/ws");
        let output = PathBuf::from("/out");
        let resolver = StubIncludeResolver::new(workspace.clone());
        let toolchain = Toolchain::default();
        let mut registry = ArtifactRegistry::new();
        let rule = TfCcTestInstance {
            name: "test".to_string(),
            srcs: vec!["test.cc".to_string()],
            deps: Vec::new(),
            copts: Vec::new(),
            linkopts: vec!["/opaque/libdirect.a".to_string()],
            defines: Vec::new(),
            data: Vec::new(),
            gpu: false,
            xla: false,
        };
        let target = TargetId(1);
        {
            let mut context = AnalysisContext::new(
                target,
                ConfigId::DEFAULT,
                "pkg",
                None,
                &output,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace,
                &[],
            );
            rule.analyze(&mut context, &[]);
        }

        let link_ids = action_ids_with_mnemonic(&registry, target, "CcLink");
        assert_eq!(link_ids.len(), 1);
        let link_id = link_ids[0];
        assert!(matches!(
            &registry.declared_action(link_id).status,
            ActionDeclarationStatus::Incomplete { reason }
                if *reason == crate::tf_cc_link::UNTYPED_ARTIFACT_REFERENCE_UNSUPPORTED
        ));
        assert!(registry.executable_action(link_id).is_none());
    }

    #[test]
    fn analyze_package_qualifies_same_basename_sources() {
        let ws = PathBuf::from("/ws");
        let out = PathBuf::from("/out");
        assert_eq!(
            resolve_test_input_path(&ws, "consumer", "//shared/tools:test.cc"),
            PathBuf::from("/ws/shared/tools/test.cc"),
            "Bazel // labels must not be mistaken for absolute paths",
        );
        let resolver = StubIncludeResolver::new(ws.clone());
        let toolchain = Toolchain::default();
        let extra = Vec::new();
        let mut registry = ArtifactRegistry::new();

        for (target, package) in [(TargetId(1), "pkg_a"), (TargetId(2), "pkg_b")] {
            let mut ctx = AnalysisContext::new(
                target,
                ConfigId::DEFAULT,
                package,
                None,
                &out,
                &mut registry,
                &resolver,
                &toolchain,
                &ws,
                &extra,
            );
            let rule = TfCcTestInstance {
                name: "test".to_string(),
                srcs: vec!["test.cc".to_string(), "test.cc".to_string()],
                deps: Vec::new(),
                copts: Vec::new(),
                linkopts: Vec::new(),
                defines: Vec::new(),
                data: Vec::new(),
                gpu: false,
                xla: false,
            };
            let _ = rule.analyze(&mut ctx, &[]);
        }

        let first = registry
            .find_artifact_by_path(Path::new("/ws/pkg_a/test.cc"), ConfigId::DEFAULT)
            .expect("first package source is registered");
        let second = registry
            .find_artifact_by_path(Path::new("/ws/pkg_b/test.cc"), ConfigId::DEFAULT)
            .expect("second package source is registered");
        assert_ne!(first, second);
        assert!(registry
            .find_artifact_by_path(Path::new("test.cc"), ConfigId::DEFAULT)
            .is_none());

        for (target, package) in [(TargetId(1), "pkg_a"), (TargetId(2), "pkg_b")] {
            let action_ids = registry.actions_of(target);
            assert_eq!(action_ids.len(), 2, "compile and link must both declare");
            let target_dir = out.join(package).join("test");
            for action_id in action_ids {
                let action = registry.declared_action(*action_id);
                assert_eq!(action.owner_target, target);
                for output_id in action.outputs.iter() {
                    assert!(registry.artifact_path(*output_id).starts_with(&target_dir));
                }
            }
            assert!(registry
                .find_artifact_by_path(&target_dir.join("test/test"), ConfigId::DEFAULT)
                .is_some());
        }
    }

    #[tokio::test]
    async fn compile_inputs_cover_own_and_transitive_headers_with_legacy_parity() {
        let ws = PathBuf::from("/ws");
        let out = PathBuf::from("/out");
        let package = "pkg";
        let resolver = StubIncludeResolver::new(ws.clone());
        let toolchain = Toolchain::default();
        let extra = Vec::new();
        let mut registry = ArtifactRegistry::new();

        let own_generated_header_path = PathBuf::from("/out/genfiles/pkg/generated.inc");
        let own_generated_header = register_artifact(
            &mut registry,
            &own_generated_header_path,
            ArtifactKind::Generated,
            false,
            TargetId(2),
        );
        let dep_source_header = register_artifact(
            &mut registry,
            Path::new("/deps/public.h"),
            ArtifactKind::Header,
            true,
            TargetId(3),
        );
        let dep_generated_header = register_artifact(
            &mut registry,
            Path::new("/out/genfiles/dep/generated.h"),
            ArtifactKind::Header,
            false,
            TargetId(3),
        );
        let dep_source_textual = register_artifact(
            &mut registry,
            Path::new("/deps/detail.inc"),
            ArtifactKind::Header,
            true,
            TargetId(3),
        );
        let dep_generated_textual = register_artifact(
            &mut registry,
            Path::new("/out/genfiles/dep/detail.inc"),
            ArtifactKind::Header,
            false,
            TargetId(3),
        );

        let file_provider = Arc::new(
            ProviderSet::empty()
                .with_files(FilesInfo::new(vec![own_generated_header_path.clone()])),
        );
        let cc_info = CcInfo::from_direct_and_deps(
            DirectCcInfo {
                headers: vec![dep_source_header, dep_generated_header, dep_source_header],
                textual_headers: vec![
                    dep_source_textual,
                    dep_generated_textual,
                    dep_generated_header,
                ],
                ..DirectCcInfo::default()
            },
            std::iter::empty(),
        );
        let cc_provider = Arc::new(ProviderSet::empty().with_cc_info(cc_info));
        let dep_providers = vec![Arc::clone(&file_provider), Arc::clone(&cc_provider)];
        let labeled_dep_providers = vec![
            (
                "//pkg:generated_headers".to_string(),
                Arc::clone(&file_provider),
            ),
            ("//dep:headers".to_string(), Arc::clone(&cc_provider)),
        ];
        let rule = TfCcTestInstance {
            name: "test".to_string(),
            srcs: vec![
                "test.cc".to_string(),
                "own.h".to_string(),
                ":generated_headers".to_string(),
                "libprebuilt.a".to_string(),
                ":runtime_data".to_string(),
            ],
            deps: vec!["//dep:headers".to_string()],
            copts: Vec::new(),
            linkopts: Vec::new(),
            defines: Vec::new(),
            data: Vec::new(),
            gpu: false,
            xla: false,
        };

        {
            let mut ctx = AnalysisContext::new(
                TargetId(1),
                ConfigId::DEFAULT,
                package,
                None,
                &out,
                &mut registry,
                &resolver,
                &toolchain,
                &ws,
                &extra,
            )
            .with_labeled_dep_providers(&labeled_dep_providers);
            let _ = rule.analyze(&mut ctx, &dep_providers);
        }

        let (compile_id, compile_action) = registry
            .declared_actions()
            .find(|(_, action)| registry.strings().str(action.mnemonic) == "CppCompile")
            .expect("tf_cc_test declares one compile action");
        let input_ids: Vec<ArtifactId> = compile_action.inputs.iter().copied().collect();
        let input_paths: Vec<PathBuf> = input_ids
            .iter()
            .map(|aid| registry.artifact_path(*aid).to_path_buf())
            .collect();
        assert_eq!(
            input_paths,
            vec![
                PathBuf::from("/ws/pkg/test.cc"),
                PathBuf::from("/deps/detail.inc"),
                PathBuf::from("/deps/public.h"),
                PathBuf::from("/out/genfiles/dep/detail.inc"),
                PathBuf::from("/out/genfiles/dep/generated.h"),
                own_generated_header_path.clone(),
                PathBuf::from("/ws/pkg/own.h"),
            ]
        );
        assert_eq!(
            input_ids.iter().copied().collect::<HashSet<_>>().len(),
            input_ids.len(),
            "duplicate headers across headers/textual_headers are declared once"
        );
        assert!(input_ids.contains(&own_generated_header));
        assert!(!registry.artifact(own_generated_header).is_source);
        let own_source_header = registry
            .find_artifact_by_path(Path::new("/ws/pkg/own.h"), ConfigId::DEFAULT)
            .expect("own source header is registered at its package-qualified path");
        assert!(registry.artifact(own_source_header).is_source);
        assert!(registry
            .find_artifact_by_path(Path::new("own.h"), ConfigId::DEFAULT)
            .is_none());
        assert!(registry
            .find_artifact_by_path(Path::new("/ws/pkg/libprebuilt.a"), ConfigId::DEFAULT)
            .is_none());
        assert!(registry
            .find_artifact_by_path(Path::new("/ws/pkg/:runtime_data"), ConfigId::DEFAULT)
            .is_none());

        let scheduler_spec =
            rush_action::command_spec::NormalizedCommandSpec::from_registry_action(
                &registry, compile_id,
            )
            .expect("compile action has an executable payload");
        let shared_registry = Arc::new(tokio::sync::Mutex::new(registry));
        let ordered_providers = labeled_dep_providers
            .iter()
            .map(|(label, providers)| (label.clone(), providers.as_ref().clone()))
            .collect();
        let build_context = BuildContext::new(ws, out)
            .with_package(PathBuf::from(package))
            .with_toolchain(toolchain)
            .with_dependencies(HashMap::new(), ordered_providers)
            .with_artifact_registry(shared_registry);
        let legacy_specs = rule.build_legacy_command_specs(&build_context).await;
        let legacy_compile = legacy_specs
            .iter()
            .find_map(|(_, spec)| (spec.mnemonic == "CppCompile").then_some(spec))
            .expect("legacy path emits the matching compile spec");
        assert_eq!(&scheduler_spec, legacy_compile);
    }
}

#[cfg(test)]
mod framework_projection_tests {
    use super::*;
    use rush_action::ids::{ArtifactRegistry, ConfigId, TargetId};
    use rush_rules::analysis_context::{AnalysisContext, StubIncludeResolver};
    use rush_rules::context::Toolchain;
    use rush_rules::providers::{CcInfo, DirectCcInfo};
    use std::sync::Arc;

    #[tokio::test]
    async fn typed_framework_dependency_refuses_before_actions_or_writes() {
        let temp = tempfile::TempDir::new().unwrap();
        let workspace = temp.path().join("source");
        let output = temp.path().join("output");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&output).unwrap();
        std::fs::write(workspace.join("main.cc"), "#error must never compile\n").unwrap();
        let old_output = output.join("old-output");
        std::fs::write(&old_output, b"retained").unwrap();
        let mut registry = ArtifactRegistry::new();
        let framework = registry.intern_path(Path::new("/frameworks"));
        let provider = ProviderSet::empty().with_cc_info(CcInfo::from_declared_direct_and_deps(
            DirectCcInfo {
                framework_include_dirs: vec![framework],
                ..Default::default()
            },
            std::iter::empty(),
        ));
        let labeled = vec![("//dep:framework".to_owned(), Arc::new(provider.clone()))];
        let attrs = HashMap::from([
            ("name".to_owned(), Value::String("consumer".into())),
            (
                "srcs".to_owned(),
                Value::List(vec![Value::String("main.cc".into())]),
            ),
            (
                "deps".to_owned(),
                Value::List(vec![Value::String("//dep:framework".into())]),
            ),
        ]);
        let rule = TfCcTestRule.parse_attributes(&attrs).unwrap();
        let resolver = StubIncludeResolver::new(workspace.clone());
        let toolchain = Toolchain::default();
        {
            let mut ctx = AnalysisContext::new(
                TargetId(1),
                ConfigId::DEFAULT,
                "",
                None,
                &output,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace,
                &[],
            )
            .with_labeled_dep_providers(&labeled);
            assert!(rule
                .validate_analysis_dependencies(&ctx)
                .unwrap_err()
                .to_string()
                .contains("framework"));
            let providers = rule.analyze(&mut ctx, &[Arc::clone(&labeled[0].1)]);
            assert!(providers.get_cc_info().is_none());
            assert!(ctx
                .native_namespace_analysis_error()
                .unwrap()
                .contains("framework"));
        }
        assert_eq!(registry.declared_actions().count(), 0);
        let mut context = BuildContext::new(workspace, output.clone())
            .with_dependencies(HashMap::new(), vec![("//dep:framework".into(), provider)]);
        assert!(rule.build_legacy_command_specs(&context).await.is_empty());
        assert!(rule
            .build(&context)
            .await
            .unwrap_err()
            .to_string()
            .contains("framework"));
        context.dependency_providers_ordered.clear();
        assert!(rule.build_legacy_command_specs(&context).await.is_empty());
        assert!(rule
            .build(&context)
            .await
            .unwrap_err()
            .to_string()
            .contains("framework"));
        assert_eq!(std::fs::read(&old_output).unwrap(), b"retained");
        assert_eq!(std::fs::read_dir(output).unwrap().count(), 1);
    }
}
