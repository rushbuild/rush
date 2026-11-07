use anyhow::{anyhow, Result};
use async_trait::async_trait;
use rush_rules::{
    context::BuildContext, extract_bool, extract_string, extract_string_list, extract_string_opt,
    providers::ProviderSet, Artifact, ArtifactType, BuildRule, RuleInstance, Value,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Resolve a tf_cc_library source/header attribute against the target's
/// package. Artifact paths must be package-qualified: the registry interns by
/// `(config, path)`, so registering a bare `foo.h` or `foo.cc` aliases every
/// package that happens to use the same basename.
fn resolve_tf_cc_input_path(workspace_root: &Path, package: &str, raw: &str) -> PathBuf {
    // Parse Bazel labels before consulting Path: on Unix, `//pkg:file` is
    // otherwise classified as an absolute filesystem path and kept verbatim.
    if let Some(rest) = raw.strip_prefix("//") {
        if let Some((label_package, name)) = rest.split_once(':') {
            return workspace_root.join(label_package).join(name);
        }
        return workspace_root.join(rest);
    }
    if let Some(name) = raw.strip_prefix(':') {
        return workspace_root.join(package).join(name);
    }
    if raw.starts_with('@') {
        // A FilesInfo-providing external dependency is expanded to an absolute
        // path before reaching this fallback. Keep the historical same-repo
        // behavior for an unexpanded external label, but at least retain its
        // filename component rather than embedding label punctuation in a path.
        let name = rush_rules::cc_command_spec::source_file_component(raw);
        return workspace_root.join(package).join(name);
    }
    let raw_path = Path::new(raw);
    if raw_path.is_absolute() {
        return raw_path.to_path_buf();
    }
    workspace_root.join(package).join(raw)
}

fn package_string(context: &BuildContext) -> String {
    context
        .package_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

fn resolve_tf_cc_build_input_path(context: &BuildContext, raw: &str) -> PathBuf {
    let package = package_string(context);
    let source = resolve_tf_cc_input_path(&context.workspace_root, &package, raw);
    if source.exists()
        || Path::new(raw).is_absolute()
        || raw.starts_with("//")
        || raw.starts_with('@')
        || raw.starts_with(':')
    {
        return source;
    }
    let generated = context.output_dir.join("genfiles").join(&package).join(raw);
    if generated.exists() {
        generated
    } else {
        source
    }
}

/// PASS-B runs before generated outputs necessarily exist. Prefer the
/// analysis registry's exact artifact path when available, and use filesystem
/// existence only as the runtime fallback. Rush is single-config today, which
/// is also the only configuration BuildContext represents.
fn resolve_tf_cc_registered_input_path(
    context: &BuildContext,
    registry: &rush_action::ids::ArtifactRegistry,
    raw: &str,
) -> PathBuf {
    let package = package_string(context);
    let source = resolve_tf_cc_input_path(&context.workspace_root, &package, raw);
    if let Some(aid) = registry.find_artifact_by_path(&source, rush_action::ids::ConfigId::DEFAULT)
    {
        return registry.artifact_path(aid).to_path_buf();
    }
    if source.exists() {
        return source;
    }
    if !Path::new(raw).is_absolute()
        && !raw.starts_with("//")
        && !raw.starts_with('@')
        && !raw.starts_with(':')
    {
        let generated = context.output_dir.join("genfiles").join(&package).join(raw);
        if let Some(aid) =
            registry.find_artifact_by_path(&generated, rush_action::ids::ConfigId::DEFAULT)
        {
            return registry.artifact_path(aid).to_path_buf();
        }
    }
    source
}

/// Expand explicit target labels and implicit bare file labels in declaration
/// order. Only absolute FilesInfo paths are spliced: a relative FilesInfo from
/// a cross-package source-file target would otherwise lose its package.
fn expand_tf_cc_inputs(
    inputs: &[String],
    lookup: impl Fn(&str) -> Option<Vec<PathBuf>>,
) -> Vec<String> {
    let mut out = Vec::with_capacity(inputs.len());
    for input in inputs {
        let label = if input.starts_with(':') || input.starts_with("//") || input.starts_with('@') {
            Some(input.clone())
        } else if Path::new(input).is_absolute() {
            None
        } else {
            Some(format!(":{input}"))
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
        out.push(input.clone());
    }
    out
}

/// Analysis-side expansion. Absolute FilesInfo paths also let a generated
/// input retain the ArtifactId registered by its producer when interned.
fn expand_analysis_inputs(
    ctx: &rush_rules::analysis_context::AnalysisContext,
    inputs: &[String],
) -> Vec<String> {
    expand_tf_cc_inputs(inputs, |label| {
        ctx.get_dependency_provider(label)
            .and_then(|p| p.get_files())
            .map(|files| files.files.clone())
    })
}

fn expand_build_inputs(context: &BuildContext, inputs: &[String]) -> Vec<String> {
    expand_tf_cc_inputs(inputs, |label| context.get_dependency_files(label))
}

/// Register one direct source/header input without reclassifying an existing
/// generated artifact. Plain generated files are found under the canonical
/// `genfiles/<package>/...` projection; label/filegroup outputs arrive here as
/// absolute FilesInfo paths and match on the first lookup.
fn register_tf_cc_input(
    ctx: &mut rush_rules::analysis_context::AnalysisContext,
    raw: &str,
    kind: rush_action::ids::ArtifactKind,
) -> rush_action::ids::ArtifactId {
    let resolved = resolve_tf_cc_input_path(ctx.workspace_root(), ctx.package(), raw);
    if let Some(aid) = ctx
        .registry()
        .find_artifact_by_path(&resolved, ctx.config_id())
    {
        return aid;
    }
    if resolved.exists() {
        return ctx.register_source_artifact(&resolved, kind);
    }

    if !Path::new(raw).is_absolute()
        && !raw.starts_with("//")
        && !raw.starts_with('@')
        && !raw.starts_with(':')
    {
        let generated =
            ctx.predicted_output_path(&Path::new("genfiles").join(ctx.package()).join(raw));
        if let Some(aid) = ctx
            .registry()
            .find_artifact_by_path(&generated, ctx.config_id())
        {
            return aid;
        }
    }

    ctx.register_source_artifact(&resolved, kind)
}

fn push_unique<T: Eq + std::hash::Hash + Copy>(
    values: &mut Vec<T>,
    seen: &mut HashSet<T>,
    value: T,
) {
    if seen.insert(value) {
        values.push(value);
    }
}

/// TensorFlow cc_library rule with TensorFlow-specific features
pub struct TfCcLibraryRule;

#[async_trait]
impl BuildRule for TfCcLibraryRule {
    fn name(&self) -> &str {
        "tf_cc_library"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let srcs = extract_string_list(attrs, "srcs").unwrap_or_default();
        let hdrs = extract_string_list(attrs, "hdrs").unwrap_or_default();
        let textual_hdrs = extract_string_list(attrs, "textual_hdrs").unwrap_or_default();
        let deps = extract_string_list(attrs, "deps").unwrap_or_default();
        let copts = extract_string_list(attrs, "copts").unwrap_or_default();
        let defines = extract_string_list(attrs, "defines").unwrap_or_default();
        let includes = extract_string_list(attrs, "includes").unwrap_or_default();
        let linkopts = extract_string_list(attrs, "linkopts").unwrap_or_default();

        // TensorFlow-specific attributes
        let gpu = extract_bool(attrs, "gpu")?.unwrap_or(false);
        let xla = extract_bool(attrs, "xla")?.unwrap_or(false);
        let tf_core = extract_bool(attrs, "tf_core")?.unwrap_or(false);
        let alwayslink = extract_bool(attrs, "alwayslink")?.unwrap_or(false);
        let visibility = extract_string_opt(attrs, "visibility");

        Ok(Box::new(TfCcLibraryInstance {
            name,
            srcs,
            hdrs,
            textual_hdrs,
            deps,
            copts,
            defines,
            includes,
            linkopts,
            gpu,
            xla,
            tf_core,
            alwayslink,
            visibility,
        }))
    }

    fn documentation(&self) -> &str {
        "TensorFlow C++ library rule with GPU, XLA, and TensorFlow core support"
    }
}

struct TfCcLibraryInstance {
    name: String,
    srcs: Vec<String>,
    hdrs: Vec<String>,
    textual_hdrs: Vec<String>,
    deps: Vec<String>,
    copts: Vec<String>,
    defines: Vec<String>,
    includes: Vec<String>,
    linkopts: Vec<String>,
    gpu: bool,
    xla: bool,
    tf_core: bool,
    alwayslink: bool,
    #[allow(dead_code)]
    visibility: Option<String>,
}

#[async_trait]
impl RuleInstance for TfCcLibraryInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        let mut deps = self.deps.clone();

        // Bazel treats label-shaped src/header attrs as dependencies. This is
        // what makes FilesInfo available to expand filegroups and, critically,
        // preserves a generated header's producer-owned ArtifactId.
        for input in self
            .srcs
            .iter()
            .chain(self.hdrs.iter())
            .chain(self.textual_hdrs.iter())
        {
            if (input.starts_with(':') || input.starts_with("//") || input.starts_with('@'))
                && !deps.contains(input)
            {
                deps.push(input.clone());
            }
        }
        // Every bare src/header is a Bazel file label. It may be checked in or
        // a same-package generated output exposed through an implicit
        // `:<path>` target. The graph resolver falls back to an on-disk source
        // file when no sibling producer exists; adding the candidate edge is
        // what makes clean-tree generated .cc/.cxx files reachable too.
        for input in self
            .srcs
            .iter()
            .chain(self.hdrs.iter())
            .chain(self.textual_hdrs.iter())
        {
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

        // Add TensorFlow core dependencies if this is a TensorFlow core library
        if self.tf_core {
            deps.push("//tensorflow/core:framework".to_string());
            deps.push("//tensorflow/core:lib".to_string());
        }

        // Add GPU dependencies if GPU is enabled
        if self.gpu {
            deps.push("//tensorflow/core:gpu_runtime".to_string());
        }

        // Add XLA dependencies if XLA is enabled
        if self.xla {
            deps.push("//tensorflow/compiler/xla:xla".to_string());
        }

        deps
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        let mut artifacts = Vec::new();

        // Compile flags
        let mut compile_flags = self.copts.clone();

        // Add TensorFlow-specific defines
        for define in &self.defines {
            compile_flags.push(format!("-D{}", define));
        }

        // Add GPU defines
        if self.gpu {
            compile_flags.push("-DGOOGLE_CUDA=1".to_string());
            compile_flags.push("-D__CUDA_NO_HALF_OPERATORS__".to_string());
            compile_flags.push("-D__CUDA_NO_HALF_CONVERSIONS__".to_string());
        }

        // Add XLA defines
        if self.xla {
            compile_flags.push("-DTENSORFLOW_USE_XLA=1".to_string());
        }

        // Add TensorFlow core defines
        if self.tf_core {
            compile_flags.push("-DTENSORFLOW_FRAMEWORK".to_string());
        }

        // Add include directories
        for include in &self.includes {
            compile_flags.push(format!("-I{}", include));
        }

        // Phase 5.C.4 commit 25: modernized from
        // iter_transitive_artifacts(dep) NestedSet walk to typed
        // CcInfo.compilation.headers walk. Pro's commit-25
        // directive: "decide whether to modernize legacy build
        // path or constrain scheduler analyze() path." Chose
        // Option B (modernize legacy), openly:
        //
        //   The legacy NestedSet walk was a pre-typed-providers
        //   leftover. cc_library's NestedSet doesn't expose
        //   .h artifacts (its build() returns [.o, .a]), so
        //   the NestedSet walk yielded ZERO header parent dirs
        //   for cc_library deps — leaving tf_cc_library compiles
        //   to rely on gcc's source-dir-lookup default. That
        //   worked empirically when src and header lived in
        //   the same dir (hello_cpp's tf_dep_consumer +
        //   hello_lib.h), but it's brittle: any TF-scale
        //   workload where the dep's headers live elsewhere
        //   would compile-fail. Typed CcInfo.compilation.headers
        //   IS the authoritative dep header source going
        //   forward.
        //
        //   This change makes legacy build() and scheduler
        //   analyze() walk the same typed source, eliminating
        //   the divergence the verifier flagged on
        //   tf_dep_consumer (commit 8). Open acknowledgment
        //   per Pro's anti-fake-mustache rule: the spec
        //   description and runtime behavior change in the
        //   same direction.
        let mut include_paths = Vec::new();
        if let Some(reg_arc) = context.artifact_registry.as_ref() {
            let reg = reg_arc.lock().await;
            for (_dep_key, providers) in &context.dependency_providers {
                if let Some(cc_info) = providers.get_cc_info() {
                    for h_aid in cc_info.compilation.headers.iter() {
                        let p = reg.artifact_path(h_aid).to_path_buf();
                        if let Some(parent) = p.parent() {
                            // Filter empty parents: a bare-filename
                            // header (e.g. cc_library registering
                            // "hello_lib.h" without a directory
                            // segment) has parent="" — emitting
                            // -I "" produces a malformed g++ argv
                            // (`g++: error: missing path after '-I'`).
                            // For bare filenames, gcc finds the
                            // header via source-dir lookup; no -I
                            // needed.
                            if parent.as_os_str().is_empty() {
                                continue;
                            }
                            include_paths.push(parent.to_path_buf());
                        }
                    }
                    // Also honor the dep's CcInfo.compilation.include_dirs.
                    // These are dir-only `-I` roots that carry NO enumerated
                    // header artifact — most importantly the include_prefix
                    // virtual-include farm root (Bazel `include_prefix`, e.g.
                    // @farmhash_gpu_archive exposes
                    // third_party/farmhash_gpu/src/farmhash_gpu.h through a
                    // _virtual_includes/ farm whose root lives here, not as a
                    // header parent dir) — plus dir-only `includes`/`-I` roots
                    // (eigen_archive, the clone-cache root). The headers-parent
                    // loop above structurally misses them, so a consumer's
                    // prefixed `#include` fails. Mirrors cc_library
                    // (rules_cc.rs) and tf_cc_test's transitive sweep.
                    for path_id in cc_info.compilation.include_dirs.iter() {
                        include_paths.push(reg.paths().path(path_id).to_path_buf());
                    }
                }
            }
        }

        // Deduplicate include paths
        include_paths.sort();
        include_paths.dedup();

        for include_path in &include_paths {
            compile_flags.push(format!("-I{}", include_path.display()));
        }

        // Expand source-producing labels/filegroups and compile only real
        // translation units. Header-like srcs remain private compile inputs.
        let expanded_srcs = expand_build_inputs(context, &self.srcs);
        let target_output_dir = context.target_output_dir(&self.name);
        let mut seen_source_paths = HashSet::new();
        for src in expanded_srcs
            .iter()
            .filter(|src| crate::is_tf_cc_compilable_source(src))
        {
            let src_path = resolve_tf_cc_build_input_path(context, src);
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

            // Determine compiler based on file extension
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
                path: obj_path,
                artifact_type: ArtifactType::Object,
            });
        }

        // Create static library only when a downstream link (or the requested
        // root itself) needs the archive. Provider-only library builds keep the
        // object/header artifacts and avoid the archive tax.
        if !artifacts.is_empty() && context.materialize_static_libraries() {
            let lib_name = format!("lib{}.a", self.name.replace('/', "_"));
            let lib_path = target_output_dir.join("lib").join(lib_name);

            // Create library directory
            if let Some(parent) = lib_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }

            // The old reuse predicate compared only member paths and a global
            // symbol-name set. It could not prove symbol-to-member ownership,
            // so a changed object could leave the archive index stale. Always
            // rebuild until a complete equivalence contract exists.
            let _ = tokio::fs::remove_file(&lib_path).await;

            let mut cmd = tokio::process::Command::new(&context.toolchain.ar);
            cmd.arg("rcs").arg(&lib_path).arg("--thin");
            for artifact in &artifacts {
                cmd.arg(&artifact.path);
            }

            let output = cmd.output().await?;
            if !output.status.success() {
                return Err(anyhow!(
                    "Archive creation failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                ));
            }

            artifacts.push(Artifact {
                path: lib_path,
                artifact_type: ArtifactType::StaticLibrary,
            });
        }

        // Publish ordinary headers through the legacy flat artifact channel.
        // Textual headers remain typed CcInfo compile inputs, but must not be
        // flattened as ArtifactType::Header: legacy C++ sinks interpret every
        // flat Header parent as an `-I` root, which would let textual headers
        // shadow ordinary/system headers and diverge from the typed argv.
        let direct_headers = expand_build_inputs(context, &self.hdrs);
        for hdr in &direct_headers {
            let hdr_path = resolve_tf_cc_build_input_path(context, hdr);
            artifacts.push(Artifact {
                path: hdr_path,
                artifact_type: ArtifactType::Header,
            });
        }

        Ok(artifacts)
    }

    /// Phase 5.B.3.e + 5.C.4: analyze() is the canonical entry
    /// point. Populates ExecutableAction so the scheduler can
    /// dispatch tf_cc_library's compile/archive actions. argv
    /// mirrors tf_cc_library's `build()` exactly: argv order is
    /// `-c src -o obj <flags...>` (different from cc_library's
    /// `<flags...> -o obj src`), TF-conditional defines for
    /// gpu/xla/tf_core, raw self.includes (no resolution), no
    /// -fPIC/-std=c++17/-D_GNU_SOURCE/Linux extras/workspace_root
    /// -I/extras_proto -I/genfiles -I.
    fn analyze(
        &self,
        ctx: &mut rush_rules::analysis_context::AnalysisContext,
        dep_providers: &[std::sync::Arc<rush_rules::providers::ProviderSet>],
    ) -> ProviderSet {
        use rush_action::ids::ArtifactKind;
        use rush_rules::providers::{CcInfo, DirectCcInfo, LinkFragment};
        use std::path::Path;

        let expanded_srcs = expand_analysis_inputs(ctx, &self.srcs);
        let expanded_headers = expand_analysis_inputs(ctx, &self.hdrs);
        let expanded_textual_headers = expand_analysis_inputs(ctx, &self.textual_hdrs);
        let direct_headers: Vec<_> = expanded_headers
            .iter()
            .map(|h| register_tf_cc_input(ctx, h, ArtifactKind::Header))
            .collect();
        let direct_textual_headers: Vec<_> = expanded_textual_headers
            .iter()
            .map(|h| register_tf_cc_input(ctx, h, ArtifactKind::Header))
            .collect();
        let mut direct_include_dirs: Vec<_> = vec![ctx.resolve_package_include(Path::new("."))];
        for include in &self.includes {
            direct_include_dirs.push(ctx.resolve_package_include(Path::new(include)));
        }
        let mut all_define_strs: Vec<String> = self.defines.clone();
        if self.gpu {
            all_define_strs.push("GOOGLE_CUDA=1".to_string());
        }
        if self.xla {
            all_define_strs.push("TENSORFLOW_USE_XLA=1".to_string());
        }
        if self.tf_core {
            all_define_strs.push("TENSORFLOW_FRAMEWORK".to_string());
        }
        let direct_defines: Vec<_> = all_define_strs
            .iter()
            .map(|d| ctx.intern_string(d))
            .collect();
        let mut direct_linker_inputs = Vec::new();
        let mut public_lib_aid: Option<rush_action::ids::ArtifactId> = None;

        let compilable_srcs: Vec<_> = expanded_srcs
            .iter()
            .filter(|src| crate::is_tf_cc_compilable_source(src))
            .collect();
        if !compilable_srcs.is_empty() {
            let dep_cc_infos = rush_rules::providers::extract_cc_infos(dep_providers);

            // Every compile consumes the target's own ordinary/textual headers
            // plus the full transitive header closure of its deps. Source
            // headers are freshness-bearing action inputs just like generated
            // headers; generated IDs remain producer-owned because we carry the
            // provider's exact ArtifactId instead of re-registering by kind.
            let mut compile_header_inputs = Vec::new();
            let mut seen_headers = HashSet::new();
            for aid in direct_headers
                .iter()
                .chain(direct_textual_headers.iter())
                .copied()
            {
                push_unique(&mut compile_header_inputs, &mut seen_headers, aid);
            }
            for header in expanded_srcs
                .iter()
                .filter(|src| crate::is_tf_cc_header_input(src))
            {
                let aid = register_tf_cc_input(ctx, header, ArtifactKind::Header);
                push_unique(&mut compile_header_inputs, &mut seen_headers, aid);
            }
            for dep_cc in &dep_cc_infos {
                for aid in dep_cc
                    .compilation
                    .headers
                    .iter()
                    .chain(dep_cc.compilation.textual_headers.iter())
                {
                    push_unique(&mut compile_header_inputs, &mut seen_headers, aid);
                }
            }

            // Phase 5.C.4: pre-collect immutable strings for the
            // borrow-collect pattern. tf_cc_library uses self
            // attrs (copts/defines/includes) + transitive header
            // parent dirs from dep CcInfo as -I.
            //
            // The transitive header walk legacy does via
            // iter_transitive_artifacts(dep) is the NestedSet
            // path. analyze() can't use that (NestedSet lives at
            // build time only). Instead we use dep CcInfo's
            // compilation.headers transitive — same logical
            // header set when deps expose CcInfo correctly. For
            // tf_cc_library deps with no CcInfo (system stubs,
            // unmigrated rules), the verifier surfaces the
            // divergence on the corresponding workload.
            let mut dep_header_parent_dirs: Vec<std::path::PathBuf> = Vec::new();
            {
                let reg = ctx.registry();
                for dep_cc in dep_cc_infos.iter() {
                    for h_aid in dep_cc.compilation.headers.iter() {
                        let p = reg.artifact_path(h_aid).to_path_buf();
                        if let Some(parent) = p.parent() {
                            // Phase 5.C.4 commit 25: filter empty
                            // parents. cc_library registers .h files
                            // via Path::new(h) — for bare-filename
                            // attrs ("hello_lib.h"), the registry path
                            // is the relative bare filename and
                            // parent() returns "". Emitting -I "" as
                            // a single argv element produces a
                            // malformed g++ command (`missing path
                            // after '-I'`). The legacy build() loop
                            // (above) and build_legacy_command_specs
                            // apply the same filter so all three
                            // walks agree.
                            if parent.as_os_str().is_empty() {
                                continue;
                            }
                            let parent = parent.to_path_buf();
                            if !dep_header_parent_dirs.contains(&parent) {
                                dep_header_parent_dirs.push(parent);
                            }
                        }
                    }
                    // Mirror build(): also honor the dep's
                    // CcInfo.compilation.include_dirs — dir-only `-I`
                    // roots with no enumerated header, including the
                    // include_prefix virtual-include farm root
                    // (@farmhash_gpu_archive's third_party/farmhash_gpu
                    // prefix) and dir-only `includes` roots
                    // (eigen_archive, the clone-cache root). Keeps
                    // analyze() (scheduler) byte-parity with build().
                    for path_id in dep_cc.compilation.include_dirs.iter() {
                        let dir = reg.paths().path(path_id).to_path_buf();
                        if !dep_header_parent_dirs.contains(&dir) {
                            dep_header_parent_dirs.push(dir);
                        }
                    }
                }
            }
            // Legacy dedups header parent dirs via sort+dedup.
            // Mirror that here.
            dep_header_parent_dirs.sort();
            dep_header_parent_dirs.dedup();

            let cxx_pathbuf = ctx.toolchain().cxx.clone();
            let cc_pathbuf = ctx.toolchain().cc.clone();

            let mut obj_artifacts: Vec<_> = Vec::with_capacity(compilable_srcs.len());
            let mut seen_sources = HashSet::new();
            for src in compilable_srcs {
                let src_aid = register_tf_cc_input(ctx, src, ArtifactKind::Source);
                if !seen_sources.insert(src_aid) {
                    continue;
                }
                let obj_name = crate::tf_cc_obj_name_for_resolved_path(
                    ctx.registry().artifact_path(src_aid),
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
                compile_inputs.extend(compile_header_inputs.iter().copied());
                let compile_id = ctx
                    .declare_action("CppCompile", compile_inputs, vec![obj_aid])
                    .expect("tf_cc_library CppCompile output must have one producer");
                {
                    let src_path_abs = ctx.registry().artifact_path(src_aid).to_path_buf();
                    let obj_path_str = ctx.registry().artifact_path(obj_aid).display().to_string();

                    // tf_cc_library compile argv (matches build()):
                    //   -c src -o obj <copts> <defines> <gpu/xla/tf_core>
                    //   <-I self.includes> <-I header parent dirs>
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
                        argv_strs.push("-D__CUDA_NO_HALF_OPERATORS__".to_string());
                        argv_strs.push("-D__CUDA_NO_HALF_CONVERSIONS__".to_string());
                    }
                    if self.xla {
                        argv_strs.push("-DTENSORFLOW_USE_XLA=1".to_string());
                    }
                    if self.tf_core {
                        argv_strs.push("-DTENSORFLOW_FRAMEWORK".to_string());
                    }
                    for inc in &self.includes {
                        argv_strs.push(format!("-I{}", inc));
                    }
                    for p in &dep_header_parent_dirs {
                        argv_strs.push(format!("-I{}", p.display()));
                    }

                    // Compiler choice: cxx for .cc/.cpp, cc otherwise.
                    let exe_pathbuf = if src.ends_with(".cc") || src.ends_with(".cpp") {
                        &cxx_pathbuf
                    } else {
                        &cc_pathbuf
                    };
                    let exe_pid = ctx.intern_path(exe_pathbuf);
                    let argv: Vec<rush_action::ids::StringId> =
                        argv_strs.iter().map(|s| ctx.intern_string(s)).collect();
                    ctx.set_executable_action(
                        compile_id,
                        rush_action::ids::ExecutableAction {
                            executable: exe_pid,
                            argv: argv.into_boxed_slice(),
                            env: Box::new([]),
                            working_dir: None,
                            env_policy: rush_action::ids::ActionEnvPolicy::InheritHost,
                        },
                    );
                }
            }
            let lib_path_str = ctx.target_output_rel(
                &self.name,
                &format!("lib/lib{}.a", self.name.replace('/', "_")),
            );
            let lib_artifact = ctx.register_generated_artifact(
                Path::new(&lib_path_str),
                ArtifactKind::StaticLibrary,
                None,
            );

            // CcArchive: same shape as cc_library: ar rcs lib --thin obj1 obj2 ...
            let archive_inputs = obj_artifacts.clone();
            let lib_path_abs = ctx
                .registry()
                .artifact_path(lib_artifact)
                .display()
                .to_string();
            let obj_path_strs: Vec<String> = obj_artifacts
                .iter()
                .map(|aid| ctx.registry().artifact_path(*aid).display().to_string())
                .collect();
            let ar_pathbuf = ctx.toolchain().ar.clone();
            let archive_id = ctx
                .declare_action("CcArchive", archive_inputs, vec![lib_artifact])
                .expect("tf_cc_library CcArchive output must have one producer");
            {
                let ar_pid = ctx.intern_path(&ar_pathbuf);
                let mut argv: Vec<rush_action::ids::StringId> =
                    Vec::with_capacity(3 + obj_path_strs.len());
                argv.push(ctx.intern_string("rcs"));
                argv.push(ctx.intern_string(&lib_path_abs));
                argv.push(ctx.intern_string("--thin"));
                for s in &obj_path_strs {
                    argv.push(ctx.intern_string(s));
                }
                ctx.set_executable_action(
                    archive_id,
                    rush_action::ids::ExecutableAction {
                        executable: ar_pid,
                        argv: argv.into_boxed_slice(),
                        env: Box::new([]),
                        working_dir: None,
                        env_policy: rush_action::ids::ActionEnvPolicy::InheritHost,
                    },
                );
            }
            let lid = ctx.register_linker_input(lib_artifact, self.alwayslink);
            direct_linker_inputs.push(lid);
            public_lib_aid = Some(lib_artifact);
        }
        let direct_link_fragments: Vec<LinkFragment> = self
            .linkopts
            .iter()
            .map(|f| LinkFragment::Flag(ctx.intern_string(f)))
            .collect();
        let direct = DirectCcInfo {
            headers: direct_headers,
            textual_headers: direct_textual_headers,
            include_dirs: direct_include_dirs,
            defines: direct_defines,
            linker_inputs: direct_linker_inputs,
            link_fragments: direct_link_fragments,
            ..DirectCcInfo::default()
        };
        let cc_info = CcInfo::from_direct_and_deps(
            direct,
            rush_rules::providers::extract_cc_infos(dep_providers).into_iter(),
        );
        let mut providers = ProviderSet::empty().with_cc_info(cc_info);
        if let Some(aid) = public_lib_aid {
            providers = providers
                .with_default_info(rush_rules::providers::DefaultInfo { files: vec![aid] });
        }
        providers
    }

    fn rule_type(&self) -> &str {
        "tf_cc_library"
    }

    /// Phase 5.C.4 commit 4 + commit 8: legacy spec builder
    /// mirrors tf_cc_library::build() exactly. Pro's call on
    /// Q A: the legacy spec MUST reflect actual legacy behavior
    /// (NestedSet artifact walk) rather than the typed CcInfo
    /// path the scheduler uses. Verifier divergence between the
    /// two sources is real and the verifier should surface it
    /// as a known divergence the next commit decides how to
    /// resolve.
    ///
    /// Walks `context.iter_transitive_artifacts(dep)` for each
    /// dep label and filters by ArtifactType::Header — exactly
    /// what tf_cc_library::build()'s compile_flags loop does.
    /// If scheduler analyze()'s typed CcInfo walk produces a
    /// different set, the verifier will flag it.
    async fn build_legacy_command_specs(
        &self,
        context: &rush_rules::context::BuildContext,
    ) -> Vec<(
        rush_rules::SchedulerActionKey,
        rush_action::command_spec::NormalizedCommandSpec,
    )> {
        use rush_action::command_spec::NormalizedCommandSpec;
        use rush_rules::SchedulerActionKey;
        let mut specs = Vec::new();
        let expanded_srcs = expand_build_inputs(context, &self.srcs);
        let compilable_srcs: Vec<_> = expanded_srcs
            .iter()
            .filter(|src| crate::is_tf_cc_compilable_source(src))
            .collect();
        if compilable_srcs.is_empty() {
            return specs;
        }

        // Phase 5.C.4 commit 25: walk dep CcInfo.compilation.headers
        // (typed) — same source as scheduler analyze() and the
        // modernized legacy build() runtime (above). Replaces the
        // commit-8 NestedSet walk that produced empty results for
        // cc_library deps and triggered the verifier divergence
        // on tf_dep_consumer.
        //
        // Pro's anti-fake-mustache rule: legacy build() runtime AND
        // this spec builder change in the same direction (both
        // walk typed CcInfo). The legacy compile_proto_source
        // pattern of "use whatever the dep's NestedSet exposes"
        // is replaced by "use whatever the dep's typed CcInfo
        // exposes" — typed CcInfo is the authoritative source
        // post-5.B.3 and going forward.
        let mut dep_header_parent_dirs: Vec<std::path::PathBuf> = Vec::new();
        let mut compile_header_paths: Vec<std::path::PathBuf> = Vec::new();
        let mut registered_src_paths: HashMap<String, PathBuf> = HashMap::new();

        // Direct ordinary and textual headers are compile inputs too. Expand
        // FilesInfo labels so generated headers use the producer's physical
        // output path, then package-qualify checked-in paths.
        let direct_headers = expand_build_inputs(context, &self.hdrs);
        let direct_textual_headers = expand_build_inputs(context, &self.textual_hdrs);

        if let Some(reg_arc) = context.artifact_registry.as_ref() {
            let reg = reg_arc.lock().await;
            for header in direct_headers.iter().chain(direct_textual_headers.iter()) {
                let path = resolve_tf_cc_registered_input_path(context, &reg, header);
                if !compile_header_paths.contains(&path) {
                    compile_header_paths.push(path);
                }
            }
            for header in expanded_srcs
                .iter()
                .filter(|src| crate::is_tf_cc_header_input(src))
            {
                let path = resolve_tf_cc_registered_input_path(context, &reg, header);
                if !compile_header_paths.contains(&path) {
                    compile_header_paths.push(path);
                }
            }
            for src in &expanded_srcs {
                registered_src_paths.insert(
                    src.clone(),
                    resolve_tf_cc_registered_input_path(context, &reg, src),
                );
            }

            // Header-parent-dir derivation intentionally remains limited to
            // ordinary headers. Textual headers are declared inputs, but must
            // not introduce new `-I` roots (which can shadow host headers).
            for (_dep_key, providers) in &context.dependency_providers {
                if let Some(cc_info) = providers.get_cc_info() {
                    for h_aid in cc_info.compilation.headers.iter() {
                        let p = reg.artifact_path(h_aid).to_path_buf();
                        if !compile_header_paths.contains(&p) {
                            compile_header_paths.push(p.clone());
                        }
                        if let Some(parent) = p.parent() {
                            if parent.as_os_str().is_empty() {
                                continue;
                            }
                            dep_header_parent_dirs.push(parent.to_path_buf());
                        }
                    }
                    for h_aid in cc_info.compilation.textual_headers.iter() {
                        let p = reg.artifact_path(h_aid).to_path_buf();
                        if !compile_header_paths.contains(&p) {
                            compile_header_paths.push(p);
                        }
                    }
                    // Mirror build()/analyze(): honor the dep's
                    // CcInfo.compilation.include_dirs (include_prefix
                    // virtual-include farm root + dir-only `includes`
                    // roots) so the legacy command specs match.
                    for path_id in cc_info.compilation.include_dirs.iter() {
                        dep_header_parent_dirs.push(reg.paths().path(path_id).to_path_buf());
                    }
                }
            }
        } else {
            for header in direct_headers.iter().chain(direct_textual_headers.iter()) {
                let path = resolve_tf_cc_build_input_path(context, header);
                if !compile_header_paths.contains(&path) {
                    compile_header_paths.push(path);
                }
            }
            for header in expanded_srcs
                .iter()
                .filter(|src| crate::is_tf_cc_header_input(src))
            {
                let path = resolve_tf_cc_build_input_path(context, header);
                if !compile_header_paths.contains(&path) {
                    compile_header_paths.push(path);
                }
            }
        }
        dep_header_parent_dirs.sort();
        dep_header_parent_dirs.dedup();

        let target_output_dir = context.target_output_dir(&self.name);
        let mut object_paths: Vec<std::path::PathBuf> = Vec::new();
        let mut seen_source_paths = HashSet::new();
        for src in compilable_srcs {
            let src_path = registered_src_paths
                .get(src)
                .cloned()
                .unwrap_or_else(|| resolve_tf_cc_build_input_path(context, src));
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
            argv.extend(self.copts.iter().cloned());
            for d in &self.defines {
                argv.push(format!("-D{}", d));
            }
            if self.gpu {
                argv.push("-DGOOGLE_CUDA=1".to_string());
                argv.push("-D__CUDA_NO_HALF_OPERATORS__".to_string());
                argv.push("-D__CUDA_NO_HALF_CONVERSIONS__".to_string());
            }
            if self.xla {
                argv.push("-DTENSORFLOW_USE_XLA=1".to_string());
            }
            if self.tf_core {
                argv.push("-DTENSORFLOW_FRAMEWORK".to_string());
            }
            for inc in &self.includes {
                argv.push(format!("-I{}", inc));
            }
            for p in &dep_header_parent_dirs {
                argv.push(format!("-I{}", p.display()));
            }

            let mut spec = NormalizedCommandSpec {
                mnemonic: "CppCompile".to_string(),
                executable,
                cwd: None,
                argv,
                env: Vec::new(),
                // Scheduler parity: own and transitive ordinary/textual
                // headers, both source and generated, are declared inputs.
                declared_inputs: {
                    let mut inputs = vec![src_path.clone()];
                    inputs.extend(compile_header_paths.iter().cloned());
                    inputs
                },
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

        // CcArchive spec: ar rcs lib --thin obj1 obj2 ...
        let lib_name = format!("lib{}.a", self.name.replace('/', "_"));
        let lib_path = target_output_dir.join("lib").join(lib_name);
        let mut archive_argv: Vec<String> = Vec::with_capacity(3 + object_paths.len());
        archive_argv.push("rcs".to_string());
        archive_argv.push(lib_path.display().to_string());
        archive_argv.push("--thin".to_string());
        for o in &object_paths {
            archive_argv.push(o.display().to_string());
        }
        let mut archive_spec = NormalizedCommandSpec {
            mnemonic: "CcArchive".to_string(),
            executable: context.toolchain.ar.clone(),
            cwd: None,
            argv: archive_argv,
            env: Vec::new(),
            declared_inputs: object_paths.clone(),
            declared_outputs: vec![lib_path.clone()],
            toolchain_identity: None,
            param_files: Vec::new(),
            env_policy: rush_action::ids::ActionEnvPolicy::InheritHost,
        };
        archive_spec.normalize();
        specs.push((
            SchedulerActionKey {
                mnemonic: "CcArchive".to_string(),
                primary_output_path: lib_path,
            },
            archive_spec,
        ));

        specs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rush_action::ids::{
        ArtifactId, ArtifactKey, ArtifactKind, ArtifactRegistry, ConfigId, TargetId,
    };
    use rush_rules::analysis_context::{AnalysisContext, StubIncludeResolver};
    use rush_rules::context::Toolchain;
    use rush_rules::providers::{CcInfo, DirectCcInfo, FilesInfo};
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn instance(
        name: &str,
        srcs: &[&str],
        hdrs: &[&str],
        textual_hdrs: &[&str],
    ) -> TfCcLibraryInstance {
        TfCcLibraryInstance {
            name: name.to_string(),
            srcs: srcs.iter().map(|s| (*s).to_string()).collect(),
            hdrs: hdrs.iter().map(|s| (*s).to_string()).collect(),
            textual_hdrs: textual_hdrs.iter().map(|s| (*s).to_string()).collect(),
            deps: Vec::new(),
            copts: Vec::new(),
            defines: Vec::new(),
            includes: Vec::new(),
            linkopts: Vec::new(),
            gpu: false,
            xla: false,
            tf_core: false,
            alwayslink: false,
            visibility: None,
        }
    }

    fn fake_artifact(
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

    fn compile_action_inputs(registry: &ArtifactRegistry) -> Vec<ArtifactId> {
        registry
            .declared_actions()
            .find_map(|(_, action)| {
                (registry.strings().str(action.mnemonic) == "CppCompile")
                    .then(|| action.inputs.to_vec())
            })
            .expect("tf_cc_library must declare a CppCompile action")
    }

    fn compile_action_argv(registry: &ArtifactRegistry) -> Vec<String> {
        let (id, _) = registry
            .declared_actions()
            .find(|(_, action)| registry.strings().str(action.mnemonic) == "CppCompile")
            .expect("tf_cc_library must declare a CppCompile action");
        registry
            .executable_action(id)
            .expect("CppCompile must carry an executable payload")
            .argv
            .iter()
            .map(|sid| registry.strings().str(*sid).to_string())
            .collect()
    }

    fn no_op_toolchain() -> Toolchain {
        let mut toolchain = Toolchain::default();
        toolchain.cc = PathBuf::from("true");
        toolchain.cxx = PathBuf::from("true");
        toolchain.ar = PathBuf::from("true");
        toolchain
    }

    #[test]
    fn test_parse_basic_tf_cc_library() {
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("test_lib".to_string()));
        attrs.insert(
            "srcs".to_string(),
            Value::List(vec![Value::String("test.cc".to_string())]),
        );
        attrs.insert(
            "hdrs".to_string(),
            Value::List(vec![Value::String("test.h".to_string())]),
        );
        attrs.insert(
            "textual_hdrs".to_string(),
            Value::List(vec![Value::String("test.inc".to_string())]),
        );
        attrs.insert("gpu".to_string(), Value::Bool(true));

        let rule = TfCcLibraryRule;
        let instance = rule.parse_attributes(&attrs).unwrap();

        assert_eq!(instance.target_name(), "test_lib");
        let deps = instance.dependencies();
        assert!(deps.contains(&":test.cc".to_string()));
        assert!(deps.contains(&":test.h".to_string()));
        assert!(deps.contains(&":test.inc".to_string()));
        assert!(deps.contains(&"//tensorflow/core:gpu_runtime".to_string()));
    }

    #[test]
    fn test_tf_core_dependencies() {
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("tf_core_lib".to_string()));
        attrs.insert("tf_core".to_string(), Value::Bool(true));

        let rule = TfCcLibraryRule;
        let instance = rule.parse_attributes(&attrs).unwrap();

        let deps = instance.dependencies();
        assert!(deps.contains(&"//tensorflow/core:framework".to_string()));
        assert!(deps.contains(&"//tensorflow/core:lib".to_string()));
    }

    #[tokio::test]
    async fn output_paths_match_across_analysis_runtime_and_legacy() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("ws");
        let output = temp.path().join("out");
        let package = "pkg";
        for directory in ["foo", "bar"] {
            let source_dir = workspace.join(package).join(directory);
            std::fs::create_dir_all(&source_dir).unwrap();
            std::fs::write(source_dir.join("same.cc"), "int f() { return 0; }\n").unwrap();
        }

        let rule = instance(
            "same_lib",
            &["foo/same.cc", "bar/same.cc", "foo/same.cc"],
            &[],
            &[],
        );
        let resolver = StubIncludeResolver::new(workspace.clone());
        let toolchain = no_op_toolchain();
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
            let _ = rule.analyze(&mut context, &[]);
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
        let context = BuildContext::new(workspace, output.clone())
            .with_package(PathBuf::from(package))
            .with_toolchain(toolchain)
            .with_artifact_registry(Arc::clone(&registry));
        let mut runtime_outputs: Vec<PathBuf> = rule
            .build(&context)
            .await
            .unwrap()
            .into_iter()
            .map(|artifact| artifact.path)
            .collect();
        runtime_outputs.sort();
        assert_eq!(runtime_outputs, analyzed_outputs);

        let mut legacy_outputs: Vec<PathBuf> = rule
            .build_legacy_command_specs(&context)
            .await
            .into_iter()
            .flat_map(|(_, spec)| spec.declared_outputs)
            .collect();
        legacy_outputs.sort();
        assert_eq!(legacy_outputs, analyzed_outputs);
        let target_dir = output.join(package).join("same_lib");
        assert_eq!(
            analyzed_outputs
                .iter()
                .filter(|path| path.starts_with(target_dir.join("obj")))
                .count(),
            2,
        );
        assert!(analyzed_outputs.contains(&target_dir.join("lib/libsame_lib.a")));
    }

    #[test]
    fn source_and_header_ids_are_package_qualified() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("ws");
        let output = temp.path().join("out");
        assert_eq!(
            resolve_tf_cc_input_path(&workspace, "consumer", "//shared/tools:main.cc"),
            workspace.join("shared/tools/main.cc"),
            "Bazel // labels must not be mistaken for absolute paths",
        );
        for package in ["pkg_a", "pkg_b"] {
            let dir = workspace.join(package);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("same.cc"), "int f() { return 0; }").unwrap();
            std::fs::write(dir.join("same.h"), "#pragma once\n").unwrap();
        }

        let resolver = StubIncludeResolver::new(workspace.clone());
        let toolchain = Toolchain::default();
        let extra = Vec::<PathBuf>::new();
        let mut registry = ArtifactRegistry::new();
        let rule = instance("same_lib", &["same.cc", "same.cc"], &["same.h"], &[]);

        for (target, package) in [(TargetId(1), "pkg_a"), (TargetId(2), "pkg_b")] {
            let mut ctx = AnalysisContext::new(
                target,
                ConfigId::DEFAULT,
                package,
                None,
                &output,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace,
                &extra,
            );
            let _ = rule.analyze(&mut ctx, &[]);
        }

        let a_src = registry
            .find_artifact_by_path(&workspace.join("pkg_a/same.cc"), ConfigId::DEFAULT)
            .expect("package A source registered");
        let b_src = registry
            .find_artifact_by_path(&workspace.join("pkg_b/same.cc"), ConfigId::DEFAULT)
            .expect("package B source registered");
        let a_hdr = registry
            .find_artifact_by_path(&workspace.join("pkg_a/same.h"), ConfigId::DEFAULT)
            .expect("package A header registered");
        let b_hdr = registry
            .find_artifact_by_path(&workspace.join("pkg_b/same.h"), ConfigId::DEFAULT)
            .expect("package B header registered");

        assert_ne!(a_src, b_src, "same-basename sources must not alias");
        assert_ne!(a_hdr, b_hdr, "same-basename headers must not alias");
        assert!(registry
            .find_artifact_by_path(Path::new("same.cc"), ConfigId::DEFAULT)
            .is_none());
        assert!(registry
            .find_artifact_by_path(Path::new("same.h"), ConfigId::DEFAULT)
            .is_none());

        for (target, package) in [(TargetId(1), "pkg_a"), (TargetId(2), "pkg_b")] {
            let action_ids = registry.actions_of(target);
            assert_eq!(action_ids.len(), 2, "compile and archive must both declare");
            let target_dir = output.join(package).join("same_lib");
            for action_id in action_ids {
                let action = registry.declared_action(*action_id);
                assert_eq!(action.owner_target, target);
                for output_id in action.outputs.iter() {
                    assert!(registry.artifact_path(*output_id).starts_with(&target_dir));
                }
            }
            assert!(registry
                .find_artifact_by_path(&target_dir.join("lib/libsame_lib.a"), ConfigId::DEFAULT,)
                .is_some());
        }
    }

    #[tokio::test]
    async fn compile_inputs_cover_own_and_transitive_header_kinds() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("ws");
        let output = temp.path().join("out");
        let package = "consumer";
        let package_dir = workspace.join(package);
        std::fs::create_dir_all(&package_dir).unwrap();
        std::fs::write(package_dir.join("main.cc"), "int main() { return 0; }").unwrap();
        std::fs::write(package_dir.join("own.h"), "#pragma once\n").unwrap();
        std::fs::write(package_dir.join("own.inc"), "// textual\n").unwrap();

        // Generated outputs deliberately do not exist: analysis and PASS-B
        // must use producer-registered paths on a clean tree.
        let own_gen = output.join("genfiles/consumer/own_generated.h");
        let own_text_gen = output.join("genfiles/consumer/own_generated.inc");
        let dep_source = workspace.join("dep/dep.h");
        let dep_text_source = workspace.join("textual_only/dep.inc");
        let dep_gen = output.join("genfiles/dep/dep_generated.h");
        let dep_text_gen = output.join("genfiles/textual_only/dep_generated.inc");

        let mut registry = ArtifactRegistry::new();
        let own_gen_id = fake_artifact(
            &mut registry,
            &own_gen,
            ArtifactKind::Header,
            false,
            TargetId(10),
        );
        let own_text_gen_id = fake_artifact(
            &mut registry,
            &own_text_gen,
            ArtifactKind::Header,
            false,
            TargetId(11),
        );
        let dep_source_id = fake_artifact(
            &mut registry,
            &dep_source,
            ArtifactKind::Header,
            true,
            TargetId(20),
        );
        let dep_text_source_id = fake_artifact(
            &mut registry,
            &dep_text_source,
            ArtifactKind::Header,
            true,
            TargetId(20),
        );
        let dep_gen_id = fake_artifact(
            &mut registry,
            &dep_gen,
            ArtifactKind::Header,
            false,
            TargetId(21),
        );
        let dep_text_gen_id = fake_artifact(
            &mut registry,
            &dep_text_gen,
            ArtifactKind::Header,
            false,
            TargetId(22),
        );

        let leaf = CcInfo::from_direct_and_deps(
            DirectCcInfo {
                headers: vec![dep_source_id, dep_gen_id],
                textual_headers: vec![dep_text_source_id, dep_text_gen_id],
                ..DirectCcInfo::default()
            },
            std::iter::empty(),
        );
        // Put the headers one level below the direct dep to prove the consumer
        // walks the transitive NestedSet closure, not only direct slices.
        let wrapper = CcInfo::from_direct_and_deps(DirectCcInfo::default(), std::iter::once(leaf));
        let cc_dep = Arc::new(ProviderSet::empty().with_cc_info(wrapper));
        let own_gen_dep =
            Arc::new(ProviderSet::empty().with_files(FilesInfo::new(vec![own_gen.clone()])));
        let own_text_gen_dep =
            Arc::new(ProviderSet::empty().with_files(FilesInfo::new(vec![own_text_gen.clone()])));
        let labeled = vec![
            ("//dep:wrapper".to_string(), Arc::clone(&cc_dep)),
            (":own_generated".to_string(), Arc::clone(&own_gen_dep)),
            (
                ":own_generated_text".to_string(),
                Arc::clone(&own_text_gen_dep),
            ),
        ];
        let rule = instance(
            "consumer_lib",
            &["main.cc"],
            &["own.h", ":own_generated"],
            &["own.inc", ":own_generated_text"],
        );

        let resolver = StubIncludeResolver::new(workspace.clone());
        let toolchain = Toolchain::default();
        let extra = Vec::<PathBuf>::new();
        let providers = {
            let mut ctx = AnalysisContext::new(
                TargetId(1),
                ConfigId::DEFAULT,
                package,
                None,
                &output,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace,
                &extra,
            )
            .with_labeled_dep_providers(&labeled);
            rule.analyze(
                &mut ctx,
                &[
                    Arc::clone(&cc_dep),
                    Arc::clone(&own_gen_dep),
                    Arc::clone(&own_text_gen_dep),
                ],
            )
        };

        let main_id = registry
            .find_artifact_by_path(&package_dir.join("main.cc"), ConfigId::DEFAULT)
            .unwrap();
        let own_id = registry
            .find_artifact_by_path(&package_dir.join("own.h"), ConfigId::DEFAULT)
            .unwrap();
        let own_text_id = registry
            .find_artifact_by_path(&package_dir.join("own.inc"), ConfigId::DEFAULT)
            .unwrap();
        let expected: HashSet<_> = [
            main_id,
            own_id,
            own_gen_id,
            own_text_id,
            own_text_gen_id,
            dep_source_id,
            dep_gen_id,
            dep_text_source_id,
            dep_text_gen_id,
        ]
        .into_iter()
        .collect();
        let actual = compile_action_inputs(&registry);
        assert_eq!(actual.len(), expected.len(), "inputs must be deduplicated");
        assert_eq!(actual.into_iter().collect::<HashSet<_>>(), expected);
        let analyze_argv = compile_action_argv(&registry);
        assert!(analyze_argv.contains(&format!("-I{}", workspace.join("dep").display())));
        assert!(analyze_argv.contains(&format!("-I{}", output.join("genfiles/dep").display())));
        assert!(!analyze_argv.contains(&format!("-I{}", workspace.join("textual_only").display())));
        assert!(!analyze_argv.contains(&format!(
            "-I{}",
            output.join("genfiles/textual_only").display()
        )));

        let cc = providers.get_cc_info().unwrap();
        assert_eq!(cc.compilation.headers.direct(), &[own_id, own_gen_id]);
        assert_eq!(
            cc.compilation.textual_headers.direct(),
            &[own_text_id, own_text_gen_id]
        );
        assert!(!registry.artifact(own_gen_id).is_source);
        assert_eq!(registry.artifact(own_gen_id).owner_target, TargetId(10));
        assert!(!own_gen.exists());
        assert!(!dep_gen.exists());

        // The legacy verifier must see the identical physical input paths even
        // though the generated files have not yet materialized.
        let registry = Arc::new(tokio::sync::Mutex::new(registry));
        let mut build_context = BuildContext::new(workspace.clone(), output.clone());
        build_context.package_path = Some(PathBuf::from(package));
        build_context.artifact_registry = Some(Arc::clone(&registry));
        build_context
            .dependency_providers
            .insert("//dep:wrapper".to_string(), cc_dep.as_ref().clone());
        build_context.dependency_providers_ordered = labeled
            .iter()
            .map(|(label, provider)| (label.clone(), provider.as_ref().clone()))
            .collect();
        let specs = rule.build_legacy_command_specs(&build_context).await;
        let compile = specs
            .iter()
            .find(|(key, _)| key.mnemonic == "CppCompile")
            .map(|(_, spec)| spec)
            .expect("legacy CppCompile spec");
        let declared: HashSet<_> = compile.declared_inputs.iter().cloned().collect();
        for path in [
            package_dir.join("main.cc"),
            package_dir.join("own.h"),
            own_gen,
            package_dir.join("own.inc"),
            own_text_gen,
            dep_source,
            dep_gen,
            dep_text_source,
            dep_text_gen,
        ] {
            assert!(
                declared.contains(&path),
                "missing legacy input {}",
                path.display()
            );
        }
        assert_eq!(declared.len(), 9);
        assert!(compile
            .argv
            .contains(&format!("-I{}", workspace.join("dep").display())));
        assert!(!compile
            .argv
            .contains(&format!("-I{}", workspace.join("textual_only").display())));
        assert!(!compile.argv.contains(&format!(
            "-I{}",
            output.join("genfiles/textual_only").display()
        )));
    }

    #[tokio::test]
    async fn generated_source_label_expands_and_keeps_producer_ownership() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("ws");
        let output = temp.path().join("out");
        let package = "pkg";
        std::fs::create_dir_all(workspace.join(package)).unwrap();
        let generated = output.join("genfiles/pkg/ops/generated.cc");
        let mut registry = ArtifactRegistry::new();
        let generated_id = fake_artifact(
            &mut registry,
            &generated,
            ArtifactKind::Generated,
            false,
            TargetId(2),
        );
        let resolver = StubIncludeResolver::new(workspace.clone());
        let toolchain = Toolchain::default();
        let extra = Vec::<PathBuf>::new();
        let file_provider =
            Arc::new(ProviderSet::empty().with_files(FilesInfo::new(vec![generated.clone()])));
        let labeled = vec![(":ops/generated.cc".to_string(), Arc::clone(&file_provider))];
        let rule = instance("generated_lib", &[":ops/generated.cc"], &[], &[]);
        assert!(rule
            .dependencies()
            .contains(&":ops/generated.cc".to_string()));
        assert!(
            instance("bare_generated_lib", &["ops/generated.cc"], &[], &[])
                .dependencies()
                .contains(&":ops/generated.cc".to_string())
        );
        {
            let mut ctx = AnalysisContext::new(
                TargetId(1),
                ConfigId::DEFAULT,
                package,
                None,
                &output,
                &mut registry,
                &resolver,
                &toolchain,
                &workspace,
                &extra,
            )
            .with_labeled_dep_providers(&labeled);
            let _ = rule.analyze(&mut ctx, &[Arc::clone(&file_provider)]);
        }
        assert!(!generated.exists());
        assert_eq!(compile_action_inputs(&registry)[0], generated_id);

        let registry = Arc::new(tokio::sync::Mutex::new(registry));
        let mut build_context = BuildContext::new(workspace, output);
        build_context.package_path = Some(PathBuf::from(package));
        build_context.artifact_registry = Some(registry);
        build_context.dependency_providers_ordered = vec![(
            ":ops/generated.cc".to_string(),
            file_provider.as_ref().clone(),
        )];
        let specs = rule.build_legacy_command_specs(&build_context).await;
        let compile = specs
            .iter()
            .find(|(key, _)| key.mnemonic == "CppCompile")
            .map(|(_, spec)| spec)
            .unwrap();
        assert_eq!(compile.declared_inputs, vec![generated.clone()]);
        assert_eq!(compile.argv.get(1), Some(&generated.display().to_string()));
    }

    #[tokio::test]
    async fn textual_headers_do_not_widen_legacy_flat_include_roots() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("ws");
        let output = temp.path().join("out");
        std::fs::create_dir_all(workspace.join("pkg")).unwrap();
        std::fs::write(workspace.join("pkg/public.h"), "#pragma once\n").unwrap();
        std::fs::write(workspace.join("pkg/private.inc"), "// textual\n").unwrap();

        let rule = instance("headers", &[], &["public.h"], &["private.inc"]);
        let context =
            BuildContext::new(workspace.clone(), output).with_package(PathBuf::from("pkg"));
        let artifacts = rule.build(&context).await.unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].path, workspace.join("pkg/public.h"));
        assert_eq!(artifacts[0].artifact_type, ArtifactType::Header);
    }
}
