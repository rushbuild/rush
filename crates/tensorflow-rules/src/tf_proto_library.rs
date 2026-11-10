// TensorFlow protocol buffer library rule (tf_proto_library)
//
// Declared-tool generation is an explicit uncached native bootstrap. The
// generation owner alone writes .pb files; CcImpl consumes those declared
// outputs. The historical unlabelled direct-factory literal path remains below.
// The real Bazel macro creates multiple targets:
//  - {name}: proto_library
//  - {name}_proto_srcs: filegroup
//  - {name}_cc: cc_proto_library
//  - {name}_cc_headers_only: alias to {name}_cc
//
// The declared-tool branch intentionally supplies no reusable compiler/read
// certificate and does not instantiate separate target/exec configurations.

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use rush_action::{Action, Artifact as ActionArtifact, Command, ExecutionInfo};
use rush_rules::{
    extract_string, Artifact, ArtifactType, BuildRule, RuleInstance, Value,
};
use std::collections::HashMap;
use std::path::PathBuf;

fn reject_framework_include_roots<'a>(
    providers: impl IntoIterator<Item = &'a rush_rules::providers::ProviderSet>,
) -> Result<()> {
    for provider in providers {
        if let Some(cc) = provider.get_cc_info() {
            anyhow::ensure!(
                cc.compilation.framework_include_dirs.iter().next().is_none(),
                "native C++ compilation does not support nonempty typed framework include roots"
            );
        }
    }
    Ok(())
}

/// Phase 5.C.4 commit 28: locate the `grpc_cpp_plugin` binary
/// for tf_proto_library's grpc-codegen path.
///
/// Lookup order:
///   1. `RUSH_GRPC_CPP_PLUGIN` env var (absolute path to the
///      binary). Validates the file exists and is executable
///      before returning. Pro's preferred non-sudo unblock for
///      hosts where the plugin lives in a non-standard location
///      (rattler/conda/pixi caches, custom builds, etc.).
///   2. `$PATH` lookup.
///   3. `/usr/bin/grpc_cpp_plugin`.
///   4. `/usr/local/bin/grpc_cpp_plugin`.
///
/// Returns `None` if the plugin can't be found anywhere. The
/// caller is responsible for surfacing the "no plugin found"
/// error with the list of searched locations — see
/// [`grpc_cpp_plugin_searched_paths`] for the diagnostic
/// helper.
///
/// Pro's commit-28 directive: "Add `RUSH_GRPC_CPP_PLUGIN`
/// override now. Do not leave it hardcoded-only. ... Do not
/// add cache hierarchy scanning. Rattler/conda/pixi caches are
/// environment manager internals, not a stable public
/// contract." This function honors that boundary.
fn probe_grpc_cpp_plugin() -> Option<PathBuf> {
    // 1. RUSH_GRPC_CPP_PLUGIN env override.
    if let Ok(override_path) = std::env::var("RUSH_GRPC_CPP_PLUGIN") {
        if !override_path.is_empty() {
            let p = PathBuf::from(&override_path);
            if is_executable_file(&p) {
                return Some(p);
            }
            // If the env var is set but invalid (file missing
            // or not executable), DO NOT fall through to
            // PATH probes — that would silently mask a
            // configuration error. The caller will surface
            // the empty-Some result with a diagnostic listing
            // the override path that failed validation.
            // Returning None here lets the caller report
            // "RUSH_GRPC_CPP_PLUGIN points to <path> but it
            // doesn't exist / isn't executable."
            //
            // Tradeoff: a typo in RUSH_GRPC_CPP_PLUGIN
            // becomes a hard failure rather than a
            // silently-overridden fallback. That's the
            // honest behavior — if the user set the env var,
            // they meant it.
            return None;
        }
    }

    // 2. PATH lookup.
    if let Ok(path_env) = std::env::var("PATH") {
        for dir in path_env.split(':') {
            if dir.is_empty() {
                continue;
            }
            let candidate = PathBuf::from(dir).join("grpc_cpp_plugin");
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    // 3-4. Distro fallbacks.
    for p in ["/usr/bin/grpc_cpp_plugin", "/usr/local/bin/grpc_cpp_plugin"] {
        let pb = PathBuf::from(p);
        if is_executable_file(&pb) {
            return Some(pb);
        }
    }
    None
}

/// Phase 5.C.4 commit 28: cross-platform "is this an executable
/// file" check. On Unix, `is_file() && (mode & 0o111 != 0)` —
/// at least one of user/group/other has execute permission.
/// On other platforms, just `is_file()` — Windows handles
/// executability differently and we don't claim cross-platform
/// rigor here.
fn is_executable_file(path: &std::path::Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(path) {
            Ok(meta) => meta.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Phase 5.C.4 commit 31b: detect "grpc_cpp_plugin found but
/// cannot execute" failures via stderr-pattern inspection.
/// Returns `Some(diagnostic_message)` when the protoc invocation
/// failed because the grpc plugin's dynamic-linker dependencies
/// aren't resolvable (rattler/conda ABI mismatch class). Returns
/// `None` for other failure modes — the caller falls through to
/// the generic protoc-stderr surface.
///
/// Pro's commit-31b directive: "When the plugin is found but
/// fails at execution time, report it as a toolchain/probe
/// failure, not as a generic protoc failure."
///
/// Detection patterns target Linux ld.so error messages. protoc
/// runs the plugin as a subprocess; when ld.so fails to load the
/// plugin (e.g. libprotoc.so.31.1.0 missing because the plugin
/// was built against rattler's protobuf but the host has Debian's
/// 3.21), the plugin process exits before producing output, and
/// protoc surfaces the ld.so error in its own stderr.
///
/// Patterns:
///   - "cannot open shared object file" — ld.so's primary
///     "missing library" message.
///   - "libprotoc.so" with version number — strongly suggests
///     ABI mismatch between plugin and host's protobuf runtime.
///   - "while loading shared libraries" — the ld.so wrapper
///     phrase that precedes the specific library name.
///
/// On match, the function returns a multi-line diagnostic per
/// Pro's spec, naming the plugin path and listing three
/// resolution options.
fn grpc_plugin_runtime_failure_diagnostic(
    plugin_path: &std::path::Path,
    stderr: &str,
) -> Option<String> {
    let looks_like_ldso_error = stderr.contains("cannot open shared object file")
        || stderr.contains("while loading shared libraries")
        || stderr.contains("libprotoc.so");
    if !looks_like_ldso_error {
        return None;
    }
    Some(format!(
        "grpc_cpp_plugin found but could not execute:\n  \
         path: {}\n  \
         protoc stderr: {}\n\nFix:\n  \
         1. Install the distro package that provides a compatible \
         grpc_cpp_plugin (Debian: `sudo apt install protobuf-compiler-grpc`).\n  \
         2. Put a compatible `grpc_cpp_plugin` on `$PATH` (its runtime libs \
         must also be resolvable by ld.so — `LD_LIBRARY_PATH` or `RPATH`).\n  \
         3. Set `RUSH_GRPC_CPP_PLUGIN=/absolute/path/to/grpc_cpp_plugin` \
         to a compatible executable. ABI mismatch is a common pitfall when \
         pointing at a binary from an isolated environment (rattler / \
         conda / pixi) — the plugin's libprotoc.so version must match the \
         host's runtime linker resolution.",
        plugin_path.display(),
        stderr.trim(),
    ))
}

/// Phase 5.C.4 commit 28: enumerate the locations
/// `probe_grpc_cpp_plugin` would search, for diagnostic
/// purposes. Mirrors the order in `probe_grpc_cpp_plugin` so
/// the failure message tells the user exactly where rush
/// looked. RUSH_GRPC_CPP_PLUGIN, when set, appears as the
/// first entry; otherwise the message says "RUSH_GRPC_CPP_PLUGIN
/// not set" so the user knows the env-var override path is
/// available.
fn grpc_cpp_plugin_searched_paths() -> Vec<String> {
    let mut entries: Vec<String> = Vec::new();
    match std::env::var("RUSH_GRPC_CPP_PLUGIN") {
        Ok(p) if !p.is_empty() => {
            entries.push(format!("RUSH_GRPC_CPP_PLUGIN={}", p));
        }
        _ => {
            entries.push("RUSH_GRPC_CPP_PLUGIN (not set)".to_string());
        }
    }
    if let Ok(path_env) = std::env::var("PATH") {
        for dir in path_env.split(':') {
            if dir.is_empty() {
                continue;
            }
            entries.push(format!("$PATH: {}/grpc_cpp_plugin", dir));
        }
    }
    entries.push("/usr/bin/grpc_cpp_plugin".to_string());
    entries.push("/usr/local/bin/grpc_cpp_plugin".to_string());
    entries
}

/// Role of a tf_proto_library instance in rush's header-only proto split.
///
/// Bazel's `make_default_target_header_only=True` produces a header-only
/// default `{name}` (alias to a header-only cc_library) PLUS a real
/// `{name}_cc_impl` that carries the compiled generated `.pb.cc` objects.
/// rush models the message targets as `tf_proto_library` instances distinguished by this
/// role (set via the hidden `_rush_tf_proto_role` attr by the macro expander):
///   - `Default`: owns the ProtocCompile action; for header-only targets it
///     exposes headers/includes only (no object/archive), exactly as before.
///   - `CcImpl`: does NOT run protoc (reuses the Default's package-qualified
///     generated `.pb.{h,cc}` ArtifactIds as compile inputs); compiles them and
///     archives into a PACKAGE-QUALIFIED `<pkg>/<name>_cc_impl/lib<name>_cc_impl.a`
///     (package-qualified so two same-named impls in different packages don't
///     collide on the ArtifactRegistry — the BD-1 DuplicateProducer hazard).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TfProtoRole {
    Default,
    CcImpl,
    // A real macro sibling whose plugin/runtime route is not implemented.
    // Keeping it separate lets ordinary message consumers remain usable.
    GrpcSibling,
}

const UNSUPPORTED_GRPC_SIBLING: &str = "tf_proto_library C++ gRPC sibling requires its genuine declared plugin/runtime route; native execution is not implemented";

pub struct TfProtoLibraryRule;

fn proto_string_list(attrs: &HashMap<String, Value>, name: &str) -> Result<Vec<String>> {
    match attrs.get(name) {
        None => Ok(Vec::new()),
        Some(Value::List(values)) => values
            .iter()
            .map(|value| match value {
                Value::String(label) => Ok(label.clone()),
                _ => Err(anyhow!("tf_proto_library {name} elements must be strings")),
            })
            .collect(),
        Some(_) => Err(anyhow!(
            "tf_proto_library {name} must be a resolved string list"
        )),
    }
}

fn proto_bool(attrs: &HashMap<String, Value>, name: &str) -> Result<bool> {
    match attrs.get(name) {
        None | Some(Value::None) => Ok(false),
        Some(Value::Bool(value)) => Ok(*value),
        Some(Value::Int(0)) => Ok(false),
        Some(Value::Int(1)) => Ok(true),
        Some(_) => Err(anyhow!("tf_proto_library {name} must be a boolean or 0/1")),
    }
}

fn proto_optional_label(attrs: &HashMap<String, Value>, name: &str) -> Result<Option<String>> {
    match attrs.get(name) {
        None => Ok(None),
        Some(Value::String(label)) if !label.is_empty() => Ok(Some(label.clone())),
        Some(_) => Err(anyhow!(
            "tf_proto_library {name} must be a nonempty declared label"
        )),
    }
}

impl BuildRule for TfProtoLibraryRule {
    fn name(&self) -> &str {
        "tf_proto_library"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let srcs = proto_string_list(attrs, "srcs")?;
        let protodeps = proto_string_list(attrs, "protodeps")?;
        let cc_api_version = attrs
            .get("cc_api_version")
            .and_then(|v| v.as_int())
            .unwrap_or(2);
        let make_default_target_header_only = proto_bool(attrs, "make_default_target_header_only")?;
        // Preserve the historical direct-factory grpc contract. The TSL
        // macro separately projects language/sibling options before parsing
        // its ordinary C++ message declarations.
        let has_services = proto_bool(attrs, "has_services")?;
        let create_grpc_library = proto_bool(attrs, "create_grpc_library")?;
        // TF's build_config.bzl forwards `use_grpc_namespace=True` into a
        // `services_namespace=grpc` flag for grpc_cpp_plugin. Combined with
        // the proto file's `package tensorflow;` declaration, the emitted
        // service classes end up in `tensorflow::grpc::*` (which is what
        // TF's own code — e.g. profiler_client.cc —
        // references).
        let use_grpc_namespace = proto_bool(attrs, "use_grpc_namespace")?;
        let grpc_version_requested = match attrs.get("cc_grpc_version") {
            None | Some(Value::None) => false,
            Some(Value::Bool(value)) => *value,
            Some(Value::Int(value)) => *value != 0,
            Some(Value::String(value)) => !value.is_empty(),
            Some(_) => {
                return Err(anyhow!(
                    "tf_proto_library cc_grpc_version must be a scalar version"
                ))
            }
        };

        // Hidden role attr set by the tf_proto_library macro expander
        // (evaluator.rs) for the synthesized `{name}_cc_impl` impl-mode target.
        let role = match attrs.get("_rush_tf_proto_role") {
            None => TfProtoRole::Default,
            Some(Value::String(role)) if role == "cc_impl" => TfProtoRole::CcImpl,
            Some(Value::String(role)) if role == "grpc_sibling" => TfProtoRole::GrpcSibling,
            Some(_) => return Err(anyhow!("invalid tf_proto_library generation role")),
        };

        Ok(Box::new(TfProtoLibraryInstance {
            name,
            srcs,
            protodeps,
            cc_api_version,
            make_default_target_header_only,
            has_services,
            create_grpc_library,
            grpc_version_requested,
            use_grpc_namespace,
            role,
            cc_libs: proto_string_list(attrs, "cc_libs")?,
            protoc: proto_optional_label(attrs, "protoc")?,
            generation_owner: proto_optional_label(attrs, "_rush_tf_proto_generation_owner")?,
        }))
    }
}

struct TfProtoLibraryInstance {
    name: String,
    srcs: Vec<String>,
    protodeps: Vec<String>,
    #[allow(dead_code)]
    cc_api_version: i64,
    #[allow(dead_code)]
    make_default_target_header_only: bool,
    has_services: bool,
    create_grpc_library: bool,
    grpc_version_requested: bool,
    use_grpc_namespace: bool,
    role: TfProtoRole,
    cc_libs: Vec<String>,
    protoc: Option<String>,
    generation_owner: Option<String>,
}

impl TfProtoLibraryInstance {
    fn generates_grpc(&self) -> bool {
        self.has_services || self.create_grpc_library || self.grpc_version_requested
    }
}

/// Resolve the ordered include roots carried by proto dependencies into owned
/// paths for command construction.  A TensorFlow proto may publicly import a
/// proto from another repository; its generated `.pb.cc` then includes that
/// dependency by logical path (for example `tsl/protobuf/error_codes.pb.h`).
/// Declaring the generated header as an action input is necessary but not
/// sufficient: the compiler also needs the dependency repository's genfiles
/// root on its include search path.
fn collect_dependency_include_dirs(
    registry: &rush_action::ids::ArtifactRegistry,
    cc_infos: &[std::sync::Arc<rush_rules::providers::CcInfo>],
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for cc_info in cc_infos {
        for path_id in cc_info.compilation.include_dirs.iter() {
            paths.push(registry.paths().path(path_id).to_path_buf());
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

#[derive(Clone)]
struct ProtoLayout {
    source: PathBuf,
    header: PathBuf,
    generated: PathBuf,
    object: PathBuf,
}

fn utf8_path(path: &std::path::Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("proto command path is not UTF-8: {}", path.display()))
}

fn exact_provider<'a>(
    providers: impl Iterator<Item = (&'a str, &'a rush_rules::providers::ProviderSet)>,
    label: &str,
    package: &str,
    repository: Option<&str>,
) -> Result<&'a rush_rules::providers::ProviderSet> {
    let matches: Vec<_> = providers
        .filter(|(stored, _)| {
            rush_rules::providers::dep_label_matches_repository_exact(
                stored, label, package, repository,
            )
        })
        .collect();
    anyhow::ensure!(matches.len() == 1, "declared proto dependency {label} requires exactly one repository-qualified provider (found {})", matches.len());
    Ok(matches[0].1)
}

fn executable_artifact(
    registry: &rush_action::ids::ArtifactRegistry,
    provider: &rush_rules::providers::ProviderSet,
    config: rush_action::ids::ConfigId,
) -> Result<rush_action::ids::ArtifactId> {
    use rush_action::ids::ArtifactKind;
    let files = &provider
        .get_default_info()
        .context("declared protoc has no DefaultInfo")?
        .files;
    let executables: Vec<_> = files
        .iter()
        .copied()
        .filter(|id| registry.artifact(*id).kind == ArtifactKind::Executable)
        .collect();
    anyhow::ensure!(
        executables.len() == 1,
        "declared protoc requires exactly one Executable artifact"
    );
    let id = executables[0];
    let data = registry.artifact(id);
    let producer = data
        .owner_action_declared
        .context("declared protoc executable has no declared producer")?;
    anyhow::ensure!(
        !data.is_source
            && registry.declared_action(producer).owner_target == data.owner_target
            && registry.declared_action(producer).outputs.contains(&id)
            && registry.find_artifact_by_path(registry.artifact_path(id), config) == Some(id),
        "declared protoc executable has mismatched producer/configuration"
    );
    Ok(id)
}

fn set_proto_payload(
    ctx: &mut rush_rules::analysis_context::AnalysisContext,
    action: rush_action::ids::DeclaredActionId,
    executable: &std::path::Path,
    argv: &[String],
) {
    let executable = ctx.intern_path(executable);
    let argv = argv.iter().map(|arg| ctx.intern_string(arg)).collect();
    let cwd = ctx.native_execution_root().map(std::path::Path::to_owned);
    let working_dir = cwd.as_deref().map(|path| ctx.intern_path(path));
    ctx.set_executable_action(
        action,
        rush_action::ids::ExecutableAction {
            executable,
            argv,
            env: Box::new([]),
            working_dir,
            env_policy: rush_action::ids::ActionEnvPolicy::InheritHost,
        },
    );
}

impl TfProtoLibraryInstance {
    fn declared_mode(&self, mode: rush_rules::context::NativeCompileMode) -> Result<()> {
        anyhow::ensure!(
            self.role != TfProtoRole::GrpcSibling,
            UNSUPPORTED_GRPC_SIBLING
        );
        if self.srcs.is_empty() {
            return Ok(());
        }
        anyhow::ensure!(mode == rush_rules::context::NativeCompileMode::UncachedDiagnosticV1,
            "declared tf_proto_library execution currently requires RUSH_CPP_DISCOVERY=uncached-diagnostic-v1; configured exec/target and reusable proto certificates are not implemented");
        anyhow::ensure!(
            !self.generates_grpc(),
            "declared diagnostic tf_proto_library does not yet admit GRPC/plugin generation"
        );
        if self.role == TfProtoRole::Default {
            anyhow::ensure!(self.protoc.is_some(), "diagnostic tf_proto_library requires its declared protoc tool; environment/bootstrap/system fallback is forbidden");
        } else {
            anyhow::ensure!(
                self.generation_owner.is_some(),
                "CcImpl requires its explicit generation-owner dependency"
            );
        }
        Ok(())
    }

    fn layouts(
        &self,
        workspace: &std::path::Path,
        output: &std::path::Path,
        package: &str,
    ) -> Result<Vec<ProtoLayout>> {
        use std::path::{Component, Path};
        let package_root = workspace.join(package);
        let mut layouts = Vec::new();
        for src in &self.srcs {
            let input = Path::new(src);
            anyhow::ensure!(
                !src.contains(':')
                    && !input
                        .components()
                        .any(|part| matches!(part, Component::ParentDir)),
                "proto source must be an admitted package file path: {src}"
            );
            let source = package_root.join(input);
            let relative = source
                .strip_prefix(workspace)
                .context("proto source is outside its source repository")?;
            let package_relative = source
                .strip_prefix(&package_root)
                .context("proto source is outside its declaring package")?;
            anyhow::ensure!(
                source
                    .extension()
                    .is_some_and(|extension| extension == "proto"),
                "proto source must end in .proto: {src}"
            );
            let layout = ProtoLayout {
                header: output
                    .join("genfiles")
                    .join(relative.with_extension("pb.h")),
                generated: output
                    .join("genfiles")
                    .join(relative.with_extension("pb.cc")),
                object: output
                    .join(package)
                    .join(&self.name)
                    .join(package_relative.with_extension("pb.o")),
                source,
            };
            anyhow::ensure!(
                !layouts
                    .iter()
                    .any(|prior: &ProtoLayout| prior.header == layout.header),
                "duplicate proto generated output: {}",
                layout.header.display()
            );
            layouts.push(layout);
        }
        Ok(layouts)
    }

    fn analyze_declared_proto(
        &self,
        ctx: &mut rush_rules::analysis_context::AnalysisContext,
        deps: &[std::sync::Arc<rush_rules::providers::ProviderSet>],
    ) -> rush_rules::providers::ProviderSet {
        match self.analyze_declared_proto_result(ctx, deps) {
            Ok(provider) => provider,
            Err(error) => {
                ctx.fail_native_namespace_analysis(format!(
                    "tf_proto_library {}: {error:#}",
                    self.name
                ));
                rush_rules::providers::ProviderSet::empty()
            }
        }
    }

    fn analyze_declared_proto_result(
        &self,
        ctx: &mut rush_rules::analysis_context::AnalysisContext,
        deps: &[std::sync::Arc<rush_rules::providers::ProviderSet>],
    ) -> Result<rush_rules::providers::ProviderSet> {
        use rush_action::ids::{ArtifactKind, ConfigId};
        use rush_rules::providers::{
            CcInfo, DefaultInfo, DirectCcInfo, ProtoFilesInfo, ProviderSet,
        };
        self.declared_mode(ctx.native_compile_mode())?;
        anyhow::ensure!(
            ctx.config_id() == ConfigId::DEFAULT,
            "declared proto bootstrap only admits the explicit legacy configuration"
        );
        let layouts = self.layouts(ctx.workspace_root(), ctx.output_dir(), ctx.package())?;
        let proto_files = ProtoFilesInfo::from_direct_and_deps(
            layouts
                .iter()
                .map(|layout| ctx.register_source_artifact(&layout.source, ArtifactKind::Source))
                .collect(),
            deps.iter()
                .filter_map(|provider| provider.get_proto_files().cloned()),
        );
        let cc_deps = rush_rules::providers::extract_cc_infos(deps);
        let mut headers = Vec::new();
        let mut generated = Vec::new();
        if self.role == TfProtoRole::CcImpl {
            let owner = exact_provider(
                ctx.labeled_dep_providers()
                    .iter()
                    .map(|(label, provider)| (label.as_str(), provider.as_ref())),
                self.generation_owner
                    .as_deref()
                    .context("missing generation owner")?,
                ctx.package(),
                ctx.workspace(),
            )?;
            let owner_files = &owner
                .get_default_info()
                .context("generation owner has no generated file provider")?
                .files;
            for layout in &layouts {
                let header = ctx
                    .registry()
                    .find_artifact_by_path(&layout.header, ctx.config_id())
                    .context("generation owner did not declare expected .pb.h")?;
                let source = ctx
                    .registry()
                    .find_artifact_by_path(&layout.generated, ctx.config_id())
                    .context("generation owner did not declare expected .pb.cc")?;
                for id in [header, source] {
                    let data = ctx.registry().artifact(id);
                    let producer = ctx
                        .registry()
                        .producer(id)
                        .context("generation owner output has no producer")?;
                    let action = ctx.registry().declared_action(producer);
                    anyhow::ensure!(
                        owner_files.contains(&id)
                            && !data.is_source
                            && action.owner_target == data.owner_target
                            && ctx.registry().strings().str(action.mnemonic) == "ProtocCompile"
                            && action.outputs.contains(&header)
                            && action.outputs.contains(&source),
                        "CcImpl expected output belongs to a different generation producer"
                    );
                }
                headers.push(header);
                generated.push(source);
            }
        } else {
            let tool = if layouts.is_empty() {
                None
            } else {
                let provider = exact_provider(
                    ctx.labeled_exec_dep_providers()
                        .iter()
                        .map(|(label, provider)| (label.as_str(), provider.as_ref())),
                    self.protoc.as_deref().context("missing declared protoc")?,
                    ctx.package(),
                    ctx.workspace(),
                )?;
                Some(executable_artifact(
                    ctx.registry(),
                    provider,
                    ctx.config_id(),
                )?)
            };
            for layout in &layouts {
                headers.push(ctx.register_generated_artifact(
                    &layout.header,
                    ArtifactKind::Header,
                    None,
                ));
                generated.push(ctx.register_generated_artifact(
                    &layout.generated,
                    ArtifactKind::Generated,
                    None,
                ));
            }
            for (index, layout) in layouts.iter().enumerate() {
                let tool = tool.context("missing declared protoc artifact")?;
                let mut inputs = vec![proto_files.direct_sources[index]];
                for id in proto_files
                    .transitive_sources
                    .iter()
                    .chain(std::iter::once(tool))
                {
                    if !inputs.contains(&id) {
                        inputs.push(id);
                    }
                }
                let action = ctx.declare_action(
                    "ProtocCompile",
                    inputs,
                    vec![headers[index], generated[index]],
                )?;
                let executable = ctx.registry().artifact_path(tool).to_owned();
                let mut argv = vec![
                    "--experimental_allow_proto3_optional".to_string(),
                    format!(
                        "--cpp_out={}",
                        utf8_path(&ctx.output_dir().join("genfiles"))?
                    ),
                    format!("--proto_path={}", utf8_path(ctx.workspace_root())?),
                ];
                for root in ctx.extra_proto_paths() {
                    argv.push(format!("--proto_path={}", utf8_path(root)?));
                }
                argv.push(utf8_path(&layout.source)?);
                set_proto_payload(ctx, action, &executable, &argv);
            }
        }
        let mut objects = Vec::new();
        if !self.make_default_target_header_only || self.role == TfProtoRole::CcImpl {
            for (index, layout) in layouts.iter().enumerate() {
                let object =
                    ctx.register_generated_artifact(&layout.object, ArtifactKind::Object, None);
                let mut inputs = vec![generated[index]];
                for id in &headers {
                    if !inputs.contains(id) {
                        inputs.push(*id);
                    }
                }
                let mut argv = vec![
                    "-c".to_string(),
                    "-fPIC".to_string(),
                    "-std=c++17".to_string(),
                ];
                argv.extend(ctx.toolchain().cxxflags.clone());
                argv.push(format!(
                    "-I{}",
                    utf8_path(
                        layout
                            .generated
                            .parent()
                            .context("generated source has no parent")?
                    )?
                ));
                argv.push(format!(
                    "-I{}",
                    utf8_path(&ctx.output_dir().join("genfiles"))?
                ));
                argv.push(format!("-I{}", utf8_path(ctx.workspace_root())?));
                for cc in &cc_deps {
                    for id in cc
                        .compilation
                        .headers
                        .iter()
                        .chain(cc.compilation.textual_headers.iter())
                    {
                        if !inputs.contains(&id) {
                            inputs.push(id);
                        }
                    }
                    for (prefix, paths) in [
                        ("-I", &cc.compilation.include_dirs),
                        ("-iquote", &cc.compilation.quote_include_dirs),
                        ("-isystem", &cc.compilation.system_include_dirs),
                    ] {
                        for id in paths.iter() {
                            argv.push(format!(
                                "{}{}",
                                prefix,
                                utf8_path(ctx.registry().paths().path(id))?
                            ));
                        }
                    }
                    for id in cc.compilation.defines.iter() {
                        argv.push(format!("-D{}", ctx.registry().strings().str(id)));
                    }
                }
                argv.extend([
                    "-o".to_string(),
                    utf8_path(&layout.object)?,
                    utf8_path(&layout.generated)?,
                ]);
                let action = ctx.declare_incomplete_action(
                    "CppCompile",
                    inputs,
                    vec![object],
                    "pending compiler-discovered generated proto inputs",
                )?;
                ctx.record_pending_compile(action, generated[index], object)?;
                let executable = ctx.toolchain().cxx.clone();
                set_proto_payload(ctx, action, &executable, &argv);
                objects.push(object);
            }
        }
        let mut libraries = Vec::new();
        let mut default_files = headers.clone();
        default_files.extend(generated);
        if !objects.is_empty() {
            let library = ctx.register_generated_artifact(
                &ctx.output_dir()
                    .join(ctx.package())
                    .join(&self.name)
                    .join(format!("lib{}.a", self.name)),
                ArtifactKind::StaticLibrary,
                None,
            );
            let action = ctx.declare_action("CcArchive", objects.clone(), vec![library])?;
            let mut argv = vec![
                "rcs".to_string(),
                utf8_path(ctx.registry().artifact_path(library))?,
                "--thin".to_string(),
            ];
            for object in objects {
                argv.push(utf8_path(ctx.registry().artifact_path(object))?);
            }
            let executable = ctx.toolchain().ar.clone();
            set_proto_payload(ctx, action, &executable, &argv);
            libraries.push(ctx.register_linker_input(library, self.role == TfProtoRole::CcImpl));
            default_files = vec![library];
        }
        let mut direct = DirectCcInfo {
            headers,
            linker_inputs: libraries,
            ..DirectCcInfo::default()
        };
        if !layouts.is_empty() {
            direct
                .include_dirs
                .push(ctx.intern_path(&ctx.output_dir().join("genfiles")));
        }
        Ok(ProviderSet::empty()
            .with_proto_files(proto_files)
            .with_cc_info(CcInfo::from_native_direct_and_deps(direct, cc_deps))
            .with_default_info(DefaultInfo {
                files: default_files,
            }))
    }

    async fn declared_proto_specs(
        &self,
        context: &rush_rules::context::BuildContext,
    ) -> Result<
        Vec<(
            rush_rules::SchedulerActionKey,
            rush_action::command_spec::NormalizedCommandSpec,
        )>,
    > {
        use rush_action::ids::{ArtifactKind, ConfigId};
        self.declared_mode(context.native_compile_mode())?;
        let layouts = self.layouts(
            &context.workspace_root,
            &context.output_dir,
            &context
                .package_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        )?;
        if layouts.is_empty() {
            return Ok(Vec::new());
        }
        let registry = context
            .artifact_registry
            .as_ref()
            .context("declared proto build has no analyzed artifact registry")?
            .lock()
            .await;
        let primary = if self.make_default_target_header_only && self.role == TfProtoRole::Default {
            &layouts[0].header
        } else {
            &layouts[0].object
        };
        let owner = registry
            .artifact(
                registry
                    .find_artifact_by_path(primary, ConfigId::DEFAULT)
                    .context("proto target has no analyzed output")?,
            )
            .owner_target;
        let tool = if self.role == TfProtoRole::Default {
            let provider = exact_provider(
                context
                    .dependency_providers
                    .iter()
                    .map(|(label, provider)| (label.as_str(), provider)),
                self.protoc.as_deref().context("missing declared protoc")?,
                &context
                    .package_path
                    .as_ref()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                context.repository.as_deref(),
            )?;
            Some(executable_artifact(&registry, provider, ConfigId::DEFAULT)?)
        } else {
            None
        };
        if let Some(selected) = context.native_action_selection() {
            anyhow::ensure!(!selected.is_empty(), "empty proto execution demand");
            for &action_id in selected {
                anyhow::ensure!(registry.actions_of(owner).contains(&action_id),
                    "proto execution demand names another owner");
                let action = registry.declared_action(action_id);
                anyhow::ensure!(registry.strings().str(action.mnemonic) == "ProtocCompile",
                    "partial proto execution supports only real ProtocCompile actions");
            }
        }
        let mut specs = Vec::new();
        for &action_id in registry.actions_of(owner) {
            if context.native_action_selection().is_some_and(|selected| !selected.contains(&action_id)) {
                continue;
            }
            let action = registry.declared_action(action_id);
            let mnemonic = registry.strings().str(action.mnemonic);
            anyhow::ensure!(
                matches!(mnemonic, "ProtocCompile" | "CppCompile" | "CcArchive"),
                "unexpected declared proto action {mnemonic}"
            );
            let executable = registry
                .executable_action(action_id)
                .context("declared proto action has no executable payload")?;
            let executable_path = registry.paths().path(executable.executable).to_owned();
            if mnemonic == "ProtocCompile" {
                let tool = tool.context("CcImpl must not own ProtocCompile")?;
                anyhow::ensure!(
                    action.inputs.contains(&tool)
                        && executable_path == registry.artifact_path(tool),
                    "protoc payload does not name its declared produced executable"
                );
            }
            let outputs: Vec<_> = action
                .outputs
                .iter()
                .map(|id| registry.artifact_path(*id).to_owned())
                .collect();
            let mut spec = rush_action::command_spec::NormalizedCommandSpec {
                mnemonic: mnemonic.to_string(),
                executable: executable_path,
                cwd: executable
                    .working_dir
                    .map(|id| registry.paths().path(id).to_owned()),
                argv: executable
                    .argv
                    .iter()
                    .map(|id| registry.strings().str(*id).to_owned())
                    .collect(),
                env: executable
                    .env
                    .iter()
                    .map(|(key, value)| {
                        (
                            registry.strings().str(*key).to_owned(),
                            registry.strings().str(*value).to_owned(),
                        )
                    })
                    .collect(),
                declared_inputs: action
                    .inputs
                    .iter()
                    .map(|id| registry.artifact_path(*id).to_owned())
                    .collect(),
                declared_outputs: outputs.clone(),
                toolchain_identity: None,
                param_files: Vec::new(),
                env_policy: executable.env_policy,
            };
            spec.normalize();
            specs.push((
                rush_rules::SchedulerActionKey {
                    mnemonic: mnemonic.to_string(),
                    primary_output_path: outputs
                        .first()
                        .context("declared proto action has no output")?
                        .clone(),
                },
                spec,
            ));
        }
        anyhow::ensure!(
            !specs.is_empty(),
            "declared proto target has no analyzed action chain"
        );
        for layout in &layouts {
            let header = registry
                .find_artifact_by_path(&layout.header, ConfigId::DEFAULT)
                .context("missing declared proto header")?;
            let source = registry
                .find_artifact_by_path(&layout.generated, ConfigId::DEFAULT)
                .context("missing declared proto source")?;
            anyhow::ensure!(
                registry.artifact(header).kind == ArtifactKind::Header
                    && registry.artifact(source).kind == ArtifactKind::Generated
                    && registry.producer(header).is_some()
                    && registry.producer(header) == registry.producer(source),
                "missing/mismatched proto generation producer"
            );
        }
        Ok(specs)
    }

    async fn build_declared_proto(
        &self,
        context: &rush_rules::context::BuildContext,
    ) -> Result<Vec<Artifact>> {
        let specs = self.declared_proto_specs(context).await?;
        let mut artifacts = Vec::new();
        for (_, spec) in specs {
            if spec.mnemonic == "ProtocCompile" {
                anyhow::ensure!(
                    is_executable_file(&spec.executable),
                    "declared produced protoc is missing or not executable: {}",
                    spec.executable.display()
                );
            }
            for output in &spec.declared_outputs {
                if let Some(parent) = output.parent() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            let inputs = spec
                .declared_inputs
                .iter()
                .cloned()
                .map(ActionArtifact::Source)
                .collect::<Vec<_>>();
            let command = Command {
                executable: spec.executable,
                args: spec.argv,
                working_dir: spec.cwd,
                execution_info: ExecutionInfo::default(),
            };
            let env = spec.env.into_iter().collect();
            let id = Action::compute_id(&inputs, &command, &env)?;
            let outputs = spec
                .declared_outputs
                .iter()
                .cloned()
                .map(ActionArtifact::Source)
                .collect();
            let result = context
                .execute_action(Action {
                    id,
                    command,
                    env,
                    inputs,
                    outputs,
                    mnemonic: spec.mnemonic.clone(),
                })
                .await?;
            anyhow::ensure!(
                result.exit_code == 0,
                "{} failed for {}: {}",
                spec.mnemonic,
                self.name,
                String::from_utf8_lossy(&result.stderr)
            );
            for output in spec.declared_outputs {
                anyhow::ensure!(
                    std::fs::metadata(&output).is_ok_and(|metadata| metadata.is_file()),
                    "{} did not produce {}",
                    spec.mnemonic,
                    output.display()
                );
                let artifact_type = if output.extension().is_some_and(|extension| extension == "h")
                {
                    ArtifactType::Header
                } else if output
                    .extension()
                    .is_some_and(|extension| extension == "cc")
                {
                    ArtifactType::Generated
                } else if output.extension().is_some_and(|extension| extension == "o") {
                    ArtifactType::Object
                } else {
                    ArtifactType::StaticLibrary
                };
                artifacts.push(Artifact {
                    path: output,
                    artifact_type,
                });
            }
        }
        Ok(artifacts)
    }
}
#[async_trait]
impl RuleInstance for TfProtoLibraryInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        let mut dependencies = self.protodeps.clone();
        for runtime in &self.cc_libs {
            if !dependencies.contains(runtime) {
                dependencies.push(runtime.clone());
            }
        }
        if self.role == TfProtoRole::Default && !self.srcs.is_empty() {
            if let Some(tool) = &self.protoc {
                if !dependencies.contains(tool) {
                    dependencies.push(tool.clone());
                }
            }
        }
        dependencies
    }

    fn exec_only_dependencies(&self) -> Vec<String> {
        if self.role == TfProtoRole::Default && !self.srcs.is_empty() {
            self.protoc.iter().cloned().collect()
        } else {
            Vec::new()
        }
    }

    fn validate_analysis_dependencies(&self, ctx: &rush_rules::analysis_context::AnalysisContext) -> Result<()> {
        reject_framework_include_roots(
            ctx.labeled_dep_providers().iter().map(|(_, provider)| provider.as_ref()),
        )?;
        if self.protoc.is_some() || self.role != TfProtoRole::Default
            || ctx.native_compile_mode() == rush_rules::context::NativeCompileMode::UncachedDiagnosticV1 {
            self.declared_mode(ctx.native_compile_mode())?;
        }
        Ok(())
    }

    async fn build(&self, context: &rush_rules::context::BuildContext) -> Result<Vec<Artifact>> {
        reject_framework_include_roots(
            context.dependency_providers_ordered.iter().map(|(_, provider)| provider).chain(context.dependency_providers.values()),
        )?;
        if self.protoc.is_some() || self.role != TfProtoRole::Default
            || context.native_compile_mode() == rush_rules::context::NativeCompileMode::UncachedDiagnosticV1 {
            return self.build_declared_proto(context).await;
        }
        // Real implementation: Run protoc to generate .pb.h and .pb.cc files
        // Then compile them to .o and .a
        let mut artifacts = Vec::new();

        if self.srcs.is_empty() {
            return Ok(artifacts);
        }

        // Create genfiles directory for generated proto code
        let genfiles_dir = context.output_dir.join("genfiles");
        std::fs::create_dir_all(&genfiles_dir)?;

        // Create output directory for object files.
        // #37: the CcImpl role's archive must land at the PACKAGE-QUALIFIED path
        // <pkg>/<name>/lib<name>.a — matching the analyze pass's declared output
        // (the obj_rel `<pkg>/<self.name>/<stem>.pb.o` layout) that the
        // //tensorflow:tensorflow_cc cc_shared_library link reads. grpc protos
        // are UnsupportedShape for the scheduler so they take THIS legacy build()
        // path, which previously emitted a FLAT <name>/ dir — the final link then
        // failed: open input .../tsl/profiler/protobuf/profiler_analysis_proto_cc_impl/
        // lib....a No such file.
        // #39 grpc-archiving collision fix (2026-06-05, Codex 019e9969): the
        // Default-role flat <name>/ layout COLLIDES when two `tf_proto_library`
        // targets share a name across packages — e.g. //tensorflow/core/protobuf:
        // worker_proto (non-grpc, 21KB proto) and //tensorflow/core/data/service:
        // worker_proto (has_services, 2.5KB proto) both wrote rush-out/worker_proto/
        // {worker.pb.o,libworker_proto.a}. Last writer wins: the big non-grpc one
        // sealed the flat archive, clobbering the grpc target's worker.pb.o
        // (messages) and orphaning worker.grpc.pb.o (WorkerService vtables) → those
        // symbols stayed undefined in libtensorflow_cc.so. Package-qualify the
        // has_services targets too (like CcImpl) so the grpc target owns a distinct
        // <pkg>/<name>/ dir. Non-grpc Default targets keep the flat layout (their
        // scheduler shape is verifier-zero-diff and must not move).
        // #39 collision fix (2026-06-15): ALL roles package-qualify the archive/
        // object dir when the target has a package. Two same-named non-grpc
        // Default-role tf_proto_library targets in different packages (e.g.
        // //tensorflow/core/protobuf/tpu:topology_proto → tensorflow::tpu::
        // TopologyProto and //tensorflow/core/profiler/protobuf:topology_proto →
        // tensorflow::profiler::Topology) both wrote the FLAT
        // rush-out/topology_proto/{topology.pb.o,libtopology_proto.a}. Last
        // writer wins: the profiler archive clobbered the tpu one, so every
        // consumer of tensorflow::tpu::TopologyProto (eager/MLIR cc_tests) saw
        // the wrong symbols and failed to link (undefined TopologyProto ctor/
        // dtor). Package-qualifying the Default role too gives each target a
        // distinct <pkg>/<name>/ dir, exactly like the CcImpl/grpc roles already
        // do. analyze() (obj_rel/lib_rel) and build_legacy_command_specs() are
        // updated to match byte-for-byte so the scheduler verifier sees no diff.
        let target_output_dir = match context.package_path.as_ref() {
            Some(p) if !p.as_os_str().is_empty() => {
                context.output_dir.join(p).join(&self.name)
            }
            _ => context.output_dir.join(&self.name),
        };
        std::fs::create_dir_all(&target_output_dir)?;

        // Get the package path for proto_path
        let _package_path = context
            .package_path
            .as_ref()
            .ok_or_else(|| anyhow!("Package path not set in build context"))?;

        let mut object_files = Vec::new();
        // Two-phase: (1) run protoc on ALL srcs so every .pb.h exists, THEN
        // (2) compile each .pb.cc. Proto files often cross-import each other
        // (e.g. config.proto imports cluster.proto), and the generated .pb.cc
        // expects its cross-imports as .pb.h files to already be on disk.
        let mut generated_cc_files: Vec<PathBuf> = Vec::new();

        let mut dep_generated_header_paths: Vec<PathBuf> = Vec::new();
        let mut dep_include_dirs: Vec<PathBuf> = Vec::new();
        if let Some(reg_arc) = context.artifact_registry.as_ref() {
            let reg = reg_arc.lock().await;
            let cc_infos: Vec<std::sync::Arc<rush_rules::providers::CcInfo>> = context
                .dependency_providers
                .values()
                .filter_map(|p| p.get_cc_info().cloned())
                .collect();
            dep_generated_header_paths =
                rush_rules::providers::collect_generated_header_paths(&reg, &cc_infos);
            dep_include_dirs = collect_dependency_include_dirs(&reg, &cc_infos);
        }

        // Phase 1: run protoc for each .proto source
        for src in &self.srcs {
            let src_path = context.source_path(src);

            if !src_path.exists() {
                return Err(anyhow!("Proto file does not exist: {}", src_path.display()));
            }

            let mut args = Vec::new();
            // proto3 optional fields need this flag on protoc < 3.15 (e.g. Ubuntu 22.04's apt
            // protoc 3.12); newer protoc defaults it on and accepts the flag harmlessly. Bazel's
            // cc_proto rules pass it unconditionally — match that so rush builds TF's proto3-optional
            // protos (stablehlo quantization_config.proto, xla memory_space_assignment.proto, …)
            // on any protoc version.
            args.push("--experimental_allow_proto3_optional".to_string());
            args.push(format!("--cpp_out={}", genfiles_dir.display()));
            args.push(format!("--proto_path={}", context.workspace_root.display()));
            for extra in &context.extra_proto_paths {
                args.push(format!("--proto_path={}", extra.display()));
            }
            args.push(src_path.to_string_lossy().to_string());

            let command = Command {
                // The compiler rush resolves (RUSH_PROTOC / PROTOC / the pinned
                // bootstrap protoc / PATH) — NOT a bare "protoc". A bare name
                // resolves through PATH at exec time, so generation could run a
                // different protobuf than the headers these sources are then
                // compiled against, producing "generated by a newer version of
                // protoc" on an unrelated-looking target.
                executable: rush_rules::rules_system::protobuf_protoc_path(),
                args,
                working_dir: None,
                execution_info: ExecutionInfo::default(),
            };

            let proto_rel_path = src_path
                .strip_prefix(&context.workspace_root)
                .unwrap_or(&src_path);
            let proto_base = proto_rel_path.with_extension("");
            let proto_name = proto_base.to_string_lossy();

            let generated_cc = genfiles_dir.join(format!("{}.pb.cc", proto_name));
            let generated_h = genfiles_dir.join(format!("{}.pb.h", proto_name));

            let inputs = vec![ActionArtifact::Source(src_path.to_path_buf())];
            let outputs = vec![
                ActionArtifact::Source(generated_cc.clone()),
                ActionArtifact::Source(generated_h.clone()),
            ];

            let action_id = Action::compute_id(&inputs, &command, &HashMap::new())?;

            let action = Action {
                id: action_id,
                command,
                env: HashMap::new(),
                inputs,
                outputs,
                mnemonic: "GenProto".to_string(),
            };

            let result = context
                .execute_action(action)
                .await
                .with_context(|| {
                    format!(
                        "execute_action(protoc) failed for src={} cpp_out={} proto_path={}",
                        src_path.display(),
                        genfiles_dir.display(),
                        context.workspace_root.display()
                    )
                })?;

            if result.exit_code != 0 {
                return Err(anyhow!(
                    "protoc failed for {}: {}",
                    src_path.display(),
                    String::from_utf8_lossy(&result.stderr)
                ));
            }

            if !generated_cc.exists() {
                return Err(anyhow!(
                    "protoc did not generate expected file: {} (proto: {})",
                    generated_cc.display(),
                    src_path.display()
                ));
            }

            artifacts.push(Artifact {
                path: generated_cc.clone(),
                artifact_type: ArtifactType::Generated,
            });
            artifacts.push(Artifact {
                path: generated_h,
                artifact_type: ArtifactType::Header,
            });

            generated_cc_files.push(generated_cc);
        }

        // Phase 1b: if this tf_proto_library declared services, run protoc
        // again with the grpc plugin to emit `.grpc.pb.h`/.cc siblings so
        // consumers that #include "<pkg>/<base>.grpc.pb.h" resolve against
        // the same genfiles tree the .pb.h files land in.
        if self.generates_grpc() {
            // Locate grpc_cpp_plugin: probe PATH and a couple known
            // distro paths. No silent skip — if the declared services
            // require grpc codegen but the plugin isn't installed,
            // fail hard with a specific message instead of producing
            // an incomplete library whose consumers fail to find
            // `<pkg>/<base>.grpc.pb.h`.
            let grpc_plugin = probe_grpc_cpp_plugin().ok_or_else(|| {
                let searched = grpc_cpp_plugin_searched_paths().join("\n  - ");
                anyhow!(
                    "tf_proto_library `{}` declares services (has_services={}, \
                     create_grpc_library={}) so needs grpc_cpp_plugin, but no \
                     usable plugin was found.\n\nSearched (in order):\n  - {}\n\n\
                     To unblock, do ONE of:\n  \
                     1. Set `RUSH_GRPC_CPP_PLUGIN=/absolute/path/to/grpc_cpp_plugin` \
                     to an existing executable file (commit 28 env override).\n  \
                     2. Put `grpc_cpp_plugin` on your `$PATH`.\n  \
                     3. Install the distro package that provides it \
                     (Debian: `protobuf-compiler-grpc`).",
                    self.name,
                    self.has_services,
                    self.create_grpc_library,
                    searched
                )
            })?;
            {
                for src in &self.srcs {
                    let src_path = context.source_path(src);
                    if !src_path.exists() {
                        continue;
                    }
                    let proto_rel_path = src_path
                        .strip_prefix(&context.workspace_root)
                        .unwrap_or(&src_path);
                    let proto_base = proto_rel_path.with_extension("");
                    let proto_name = proto_base.to_string_lossy();
                    let generated_grpc_cc =
                        genfiles_dir.join(format!("{}.grpc.pb.cc", proto_name));
                    let generated_grpc_h =
                        genfiles_dir.join(format!("{}.grpc.pb.h", proto_name));

                    let mut args = Vec::new();
                    args.push(format!("--grpc_out={}", genfiles_dir.display()));
                    args.push(format!(
                        "--plugin=protoc-gen-grpc={}",
                        grpc_plugin.display()
                    ));
                    // services_namespace decision matrix (matches TF's
                    // third_party/xla/.../tsl/platform/default/build_config.bzl):
                    //
                    //   create_grpc_library=True    → grpc (cc_grpc_library
                    //                                  default for its
                    //                                  service_namespace
                    //                                  attr)
                    //   use_grpc_namespace=True     → grpc (explicit)
                    //   has_services=True only      → "" (services land
                    //                                  in the proto's
                    //                                  natural package
                    //                                  namespace; consumer
                    //                                  code's unqualified
                    //                                  `grpc::` references
                    //                                  must resolve to
                    //                                  the global grpc
                    //                                  namespace)
                    //
                    // Forcing the wrong choice here produces
                    // `'X' is not a member of '<pkg>::grpc'` (when too
                    // strict) or `'<pkg>::Service' has not been declared`
                    // (when too lax) — both observed during TF rung 11.
                    if self.create_grpc_library || self.use_grpc_namespace {
                        args.push("--grpc_opt=services_namespace=grpc".to_string());
                    }
                    args.push(format!("--proto_path={}", context.workspace_root.display()));
                    for extra in &context.extra_proto_paths {
                        args.push(format!("--proto_path={}", extra.display()));
                    }
                    args.push(src_path.to_string_lossy().to_string());

                    let command = Command {
                        // Resolved protoc — see the note above.
                        executable: rush_rules::rules_system::protobuf_protoc_path(),
                        args,
                        working_dir: None,
                        execution_info: ExecutionInfo::default(),
                    };

                    let inputs = vec![ActionArtifact::Source(src_path.to_path_buf())];
                    let outputs = vec![
                        ActionArtifact::Source(generated_grpc_cc.clone()),
                        ActionArtifact::Source(generated_grpc_h.clone()),
                    ];
                    let action_id = Action::compute_id(&inputs, &command, &HashMap::new())?;
                    let action = Action {
                        id: action_id,
                        command,
                        env: HashMap::new(),
                        inputs,
                        outputs,
                        mnemonic: "GenGrpcProto".to_string(),
                    };

                    let result = context.execute_action(action).await.with_context(|| {
                        format!(
                            "execute_action(protoc --grpc_out) failed for src={}",
                            src_path.display()
                        )
                    })?;
                    if result.exit_code != 0 {
                        let stderr = String::from_utf8_lossy(&result.stderr);
                        // Phase 5.C.4 commit 31b: detect "found but
                        // can't execute" — the rattler ABI mismatch
                        // class. ld.so emits "cannot open shared
                        // object file" / "libprotoc.so.<N>" when the
                        // plugin's runtime deps aren't resolvable.
                        // Surface a structured diagnostic instead of
                        // the raw protoc stderr — the user shouldn't
                        // have to know that protoc spawns the plugin
                        // as a subprocess to interpret the failure.
                        if let Some(diag) = grpc_plugin_runtime_failure_diagnostic(
                            &grpc_plugin,
                            &stderr,
                        ) {
                            return Err(anyhow!(
                                "tf_proto_library `{}`: {}",
                                self.name,
                                diag
                            ));
                        }
                        return Err(anyhow!(
                            "protoc grpc codegen failed for {}: {}",
                            src_path.display(),
                            stderr
                        ));
                    }
                    if !generated_grpc_cc.exists() {
                        // Proto may have been imported without actual
                        // service definitions — in that case grpc_cpp_plugin
                        // produces no .grpc.pb.cc. Skip silently.
                        continue;
                    }
                    artifacts.push(Artifact {
                        path: generated_grpc_cc.clone(),
                        artifact_type: ArtifactType::Generated,
                    });
                    artifacts.push(Artifact {
                        path: generated_grpc_h,
                        artifact_type: ArtifactType::Header,
                    });
                    generated_cc_files.push(generated_grpc_cc);
                }
            }
        }

        // Phase 2: compile every generated .pb.cc now that all .pb.h files exist
        for generated_cc in &generated_cc_files {
            let obj_name = generated_cc
                .file_stem()
                .ok_or_else(|| anyhow!("Invalid generated file"))?
                .to_string_lossy()
                .to_string();
            let obj_path = target_output_dir.join(format!("{}.o", obj_name));

            self.compile_proto_source(
                context,
                generated_cc,
                &obj_path,
                &dep_generated_header_paths,
                &dep_include_dirs,
            )
                .await?;

            object_files.push(obj_path.clone());
            artifacts.push(Artifact {
                path: obj_path,
                artifact_type: ArtifactType::Object,
            });
        }

        // Create static library only when this archive can reach a link action
        // or the target is requested directly.
        if !object_files.is_empty() && context.materialize_static_libraries() {
            let lib_path = target_output_dir.join(format!("lib{}.a", self.name));
            self.create_static_library(context, &object_files, &lib_path)
                .await?;
            artifacts.push(Artifact {
                path: lib_path,
                artifact_type: ArtifactType::StaticLibrary,
            });
        }

        Ok(artifacts)
    }

    /// Phase 5.B.3.e + 5.C0 + 5.C.4 commit 22: declare the full
    /// Protoc → CppCompile → CcArchive chain for each .proto and
    /// populate `ExecutableAction` on each so the scheduler can
    /// dispatch.
    ///
    /// Per .proto:
    ///   - ProtocCompile: src_proto → (pb_h, pb_cc) [+ .grpc.pb.{h,cc}
    ///       when has_services/create_grpc_library]
    ///       executable: `protoc` (PATH-resolved at exec time)
    ///       argv: --cpp_out=<output_dir>/genfiles
    ///             --proto_path=<workspace_root>
    ///             [--proto_path=<extra>...]
    ///             <abs_proto>
    ///   - CppCompile: (pb_cc, pb_h) → obj
    ///       executable: toolchain.cxx
    ///       argv: -c -fPIC -std=c++17 [cxxflags...]
    ///             -I<parent_of_src> -I<genfiles> -I<workspace_root>
    ///             -I/usr/include -o <obj> <src>
    ///       Both pb_cc AND pb_h declared as inputs (commit 14
    ///       correctness directive — pb_cc #include's pb_h).
    ///
    /// Plus once per target with at least one obj:
    ///   - CcArchive: [obj1, obj2, ...] → lib<self.name>.a
    ///       executable: toolchain.ar
    ///       argv: rcs <lib_path> <obj1> <obj2> ...
    ///
    /// Providers exposed:
    ///   - DefaultInfo.files = [.a only] — commit 9 invariant.
    ///   - CcInfo.compilation.headers = [.pb.h per .proto].
    ///   - CcInfo.compilation.include_dirs = [<output_dir>/genfiles]
    ///     so downstream `#include "<pkg>/<stem>.pb.h"` resolves.
    ///   - CcInfo.linking.linker_inputs = [.a as LinkerInput,
    ///     alwayslink=false].
    ///   - CcInfo.linking.link_fragments = [-lprotobuf, ...]
    ///     (existing tf_proto behavior — preserved).
    ///
    /// Commit 22 scope: NON-GRPC only. When has_services or
    /// create_grpc_library is set, ExecutableAction is NOT
    /// populated on any of the three mnemonics — the eligibility
    /// predicate reports `ActionGraphIncomplete` and the target
    /// falls through to legacy `build()`. The grpc plugin path
    /// (-I<grpc_plugin>, --grpc_out, services_namespace handling)
    /// is commit 23.
    ///
    /// `-I/usr/include` for system protobuf is the host-system
    /// shortcut for hello_cpp-scale workloads (mirrors legacy
    /// compile_proto_source). Hermetic / TF-scale resolution is
    /// a separate follow-up; per Pro: "not cache/hermetic-ready."
    fn analyze(
        &self,
        ctx: &mut rush_rules::analysis_context::AnalysisContext,
        dep_providers: &[std::sync::Arc<rush_rules::providers::ProviderSet>],
    ) -> rush_rules::providers::ProviderSet {
        if let Err(error) = reject_framework_include_roots(
            dep_providers.iter().map(|provider| provider.as_ref()),
        ) {
            ctx.fail_native_namespace_analysis(error.to_string());
            return rush_rules::providers::ProviderSet::empty();
        }
        if self.srcs.is_empty() || self.protoc.is_some() || self.role != TfProtoRole::Default
            || ctx.native_compile_mode() == rush_rules::context::NativeCompileMode::UncachedDiagnosticV1 {
            return self.analyze_declared_proto(ctx, dep_providers);
        }
        use rush_action::ids::{ArtifactId, ArtifactKind, ExecutableAction, StringId};
        use rush_rules::providers::{
            CcInfo, DefaultInfo, DirectCcInfo, LinkFragment, ProviderSet,
        };
        use std::path::Path;

        let pkg = ctx.package().to_string();
        // grpc cases defer to commit 23 — leave them in legacy.
        // The action graph is still declared so producer
        // validation works, but ExecutableAction stays unset and
        // the eligibility predicate routes through legacy.
        let grpc_path = self.generates_grpc();
        // ROOT-1b: the CcImpl role reuses the Default's package-qualified
        // generated .pb.{h,cc} (so it must NOT re-declare ProtocCompile —
        // that would duplicate the producer) and instead compiles+archives
        // them into a PACKAGE-QUALIFIED archive so same-named impls across
        // packages don't collide on the registry.
        let is_cc_impl = matches!(self.role, TfProtoRole::CcImpl);

        let dep_cc_infos = rush_rules::providers::extract_cc_infos(dep_providers);
        let dep_generated_header_inputs =
            rush_rules::providers::collect_generated_header_inputs(
                ctx.registry(),
                &dep_cc_infos,
            );
        let dep_include_dirs = collect_dependency_include_dirs(ctx.registry(), &dep_cc_infos);

        let mut direct_headers: Vec<ArtifactId> = Vec::new();
        let mut direct_include_dirs: Vec<rush_action::ids::PathId> = Vec::new();
        let mut obj_aids: Vec<ArtifactId> = Vec::new();
        let mut genfiles_dir_interned: bool = false;

        for src in &self.srcs {
            let src_aid =
                ctx.register_source_artifact(Path::new(src), ArtifactKind::Source);
            let stem = Path::new(src)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("proto")
                .to_string();
            // Match legacy build()'s genfiles output layout: protoc
            // --cpp_out=<genfiles_dir> places .pb.* under genfiles
            // replicating the proto's relative path. For root-pkg
            // targets (pkg == ""), proto_rel_path is just the
            // bare proto file, so generated lives directly under
            // genfiles/. Empty pkg requires omitting the
            // "<pkg>/" segment entirely — `format!("genfiles/{}/...", "")`
            // would produce "genfiles//<stem>" with a double slash
            // which then breaks the verifier's path comparison.
            let pb_h_rel = if pkg.is_empty() {
                format!("genfiles/{}.pb.h", stem)
            } else {
                format!("genfiles/{}/{}.pb.h", pkg, stem)
            };
            let pb_cc_rel = if pkg.is_empty() {
                format!("genfiles/{}.pb.cc", stem)
            } else {
                format!("genfiles/{}/{}.pb.cc", pkg, stem)
            };
            let pb_h_aid = ctx.register_generated_artifact(
                Path::new(&pb_h_rel),
                ArtifactKind::Header,
                None,
            );
            let pb_cc_aid = ctx.register_generated_artifact(
                Path::new(&pb_cc_rel),
                ArtifactKind::Generated,
                None,
            );
            direct_headers.push(pb_h_aid);

            // Object path: <output_dir>/<self.name>/<stem>.pb.o.
            // legacy build() uses target_output_dir.join(format!("{}.o",
            // generated_cc.file_stem())) — file_stem of "<stem>.pb.cc"
            // is "<stem>.pb", so resulting obj name is "<stem>.pb.o".
            //
            // BD-1 fix (2026-05-29): a `make_default_target_header_only`
            // proto target produces NO compiled object/archive — it is a
            // pure public-import reexport wrapper (e.g.
            // //tensorflow/core/protobuf:error_codes_proto_impl, whose
            // .proto only `import public`s the TSL proto). Both the object
            // and archive paths are derived from self.name ONLY, so two such
            // wrappers sharing a name across packages collide on the
            // ArtifactRegistry's single-owner outputs. Honoring the
            // (previously captured-but-ignored) attribute makes the wrapper
            // header-only: it still runs protoc (the .pb.h/.pb.cc are
            // package-qualified, no collision) and reexports the imported
            // target's CcInfo via the protodep merge below, but declares no
            // own .o/.a. This removes the collision at its source rather than
            // suppressing it in the verifier (which would be unsound — see
            // docs/analysis/bd1-fix-design-codex-2026-05-29.md).
            let obj_aid_opt = if self.make_default_target_header_only && !is_cc_impl {
                None
            } else {
                // CcImpl uses a PACKAGE-QUALIFIED obj path so two same-named
                // `_cc_impl` targets in different packages don't collide on the
                // registry's single-owner outputs (the BD-1 hazard). The
                // has_services (grpc) Default targets join CcImpl here for the
                // SAME reason (#39 collision fix, see build()'s target_output_dir):
                // two same-named worker_proto targets in different packages were
                // clobbering each other's flat outputs. Non-grpc Default targets
                // keep their self.name-only layout unchanged to bound blast radius
                // on the ~40 leaf protos (their scheduler shape is verifier-zero-diff).
                // #39 collision fix (2026-06-15): package-qualify ALL roles (see
                // build()'s target_output_dir comment). The flat Default layout
                // collided for same-named protos across packages (tpu vs profiler
                // topology_proto). Matches build()/legacy-spec byte-for-byte.
                let obj_rel = if pkg.is_empty() {
                    format!("{}/{}.pb.o", self.name, stem)
                } else {
                    format!("{}/{}/{}.pb.o", pkg, self.name, stem)
                };
                let obj_aid = ctx.register_generated_artifact(
                    Path::new(&obj_rel),
                    ArtifactKind::Object,
                    None,
                );
                obj_aids.push(obj_aid);
                Some(obj_aid)
            };

            // Compute include_dir (genfiles_dir absolute) once
            // — downstream consumers need this to resolve
            // `#include "<pkg>/<stem>.pb.h"`.
            let genfiles_dir = ctx.output_dir().join("genfiles");
            if !genfiles_dir_interned {
                let pid = ctx.intern_path(&genfiles_dir);
                direct_include_dirs.push(pid);
                genfiles_dir_interned = true;
            }

            // Set up grpc outputs even on the non-grpc path —
            // declared-output graph stays honest about what
            // legacy build() would produce.
            let mut protoc_outputs = vec![pb_h_aid, pb_cc_aid];
            if grpc_path {
                let grpc_h_rel = if pkg.is_empty() {
                    format!("genfiles/{}.grpc.pb.h", stem)
                } else {
                    format!("genfiles/{}/{}.grpc.pb.h", pkg, stem)
                };
                let grpc_cc_rel = if pkg.is_empty() {
                    format!("genfiles/{}.grpc.pb.cc", stem)
                } else {
                    format!("genfiles/{}/{}.grpc.pb.cc", pkg, stem)
                };
                let grpc_h_aid = ctx.register_generated_artifact(
                    Path::new(&grpc_h_rel),
                    ArtifactKind::Header,
                    None,
                );
                let grpc_cc_aid = ctx.register_generated_artifact(
                    Path::new(&grpc_cc_rel),
                    ArtifactKind::Generated,
                    None,
                );
                protoc_outputs.push(grpc_h_aid);
                protoc_outputs.push(grpc_cc_aid);
                direct_headers.push(grpc_h_aid);

                // #39 grpc-archiving (2026-06-05, Codex 019e9969): declare the
                // grpc TU's object + its CppCompile so the declared CcArchive
                // includes <stem>.grpc.pb.o — matching what legacy build()
                // actually archives. Without this the grpc object is invisible to
                // the declared graph / DefaultInfo / up-to-date check, so the
                // WorkerService vtable+typeinfo symbols it defines stayed
                // undefined in libtensorflow_cc.so. Package-qualified to match
                // obj_rel. Executable stays UNSET (grpc_path) — the target still
                // routes to legacy via UnsupportedShape; legacy build() runs the
                // real protoc --grpc_out + compile + archive.
                if obj_aid_opt.is_some() {
                    let grpc_obj_rel = if pkg.is_empty() {
                        format!("{}/{}.grpc.pb.o", self.name, stem)
                    } else {
                        format!("{}/{}/{}.grpc.pb.o", pkg, self.name, stem)
                    };
                    let grpc_obj_aid = ctx.register_generated_artifact(
                        Path::new(&grpc_obj_rel),
                        ArtifactKind::Object,
                        None,
                    );
                    // grpc .cc #include's its own .grpc.pb.h AND the message .pb.h.
                    let mut grpc_compile_inputs = vec![grpc_cc_aid, grpc_h_aid, pb_h_aid];
                    for h in &dep_generated_header_inputs {
                        if !grpc_compile_inputs.contains(h) {
                            grpc_compile_inputs.push(*h);
                        }
                    }
                    let _ = ctx.declare_action(
                        "CppCompile",
                        grpc_compile_inputs,
                        vec![grpc_obj_aid],
                    );
                    obj_aids.push(grpc_obj_aid);
                }
            }

            // ----- ProtocCompile declare + populate -----
            // CcImpl does NOT own protoc — the Default ({name}) target
            // declares it and produces the package-qualified .pb.{h,cc};
            // declaring it here too would register a duplicate producer.
            // CcImpl still registered the same pb_h/pb_cc ArtifactIds above
            // (register_generated_artifact is keyed by path), so they resolve
            // to the Default's outputs and feed the CppCompile below.
            if !is_cc_impl {
            if let Ok(protoc_id) =
                ctx.declare_action("ProtocCompile", vec![src_aid], protoc_outputs)
            {
                if !grpc_path {
                    // Resolve src_path to absolute (legacy build()
                    // gets absolute via context.source_path).
                    let abs_proto = if Path::new(src).is_absolute() {
                        std::path::PathBuf::from(src)
                    } else if pkg.is_empty() {
                        ctx.workspace_root().join(src)
                    } else {
                        ctx.workspace_root().join(&pkg).join(src)
                    };
                    let genfiles_dir = ctx.output_dir().join("genfiles");
                    let extra_proto_paths: Vec<std::path::PathBuf> =
                        ctx.extra_proto_paths().to_vec();

                    let mut argv_strs: Vec<String> = Vec::new();
                    argv_strs.push("--experimental_allow_proto3_optional".to_string());
                    argv_strs.push(format!("--cpp_out={}", genfiles_dir.display()));
                    argv_strs.push(format!(
                        "--proto_path={}",
                        ctx.workspace_root().display()
                    ));
                    for extra in &extra_proto_paths {
                        argv_strs.push(format!("--proto_path={}", extra.display()));
                    }
                    argv_strs.push(abs_proto.display().to_string());

                    // Resolved protoc — see the note above. Interning the
                    // resolved path also makes the action key depend on WHICH
                    // protoc ran, so switching compilers invalidates cached
                    // generated sources instead of reusing mismatched ones.
                    let resolved_protoc = rush_rules::rules_system::protobuf_protoc_path();
                    let exe_pid = ctx.intern_path(&resolved_protoc);
                    let argv: Vec<StringId> = argv_strs
                        .iter()
                        .map(|s| ctx.intern_string(s))
                        .collect();
                    ctx.set_executable_action(
                        protoc_id,
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
            } // end if !is_cc_impl (protoc owned by Default role)

            // ----- CppCompile on the generated .pb.cc -----
            // declared_inputs = [pb_cc, pb_h, dep_generated_headers]
            // correctness directive. Skipped for header-only wrappers
            // (BD-1): no object artifact was registered, so there is
            // nothing to compile — the loop iteration ends here.
            let obj_aid = match obj_aid_opt {
                Some(aid) => aid,
                None => continue,
            };
            let pb_cc_abs = ctx.registry().artifact_path(pb_cc_aid).to_path_buf();
            let obj_abs = ctx.registry().artifact_path(obj_aid).to_path_buf();
            let pb_cc_parent = pb_cc_abs
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| std::path::PathBuf::from(""));
            let cxxflags: Vec<String> = ctx.toolchain().cxxflags.clone();
            let cxx_pathbuf = ctx.toolchain().cxx.clone();
            let genfiles_for_compile = ctx.output_dir().join("genfiles");
            let workspace_root_for_compile = ctx.workspace_root().to_path_buf();

            let mut compile_inputs = vec![pb_cc_aid, pb_h_aid];
            for h in &dep_generated_header_inputs {
                if !compile_inputs.contains(h) {
                    compile_inputs.push(*h);
                }
            }
            if let Ok(compile_id) =
                ctx.declare_action("CppCompile", compile_inputs, vec![obj_aid])
            {
                if !grpc_path {
                    // Mirror compile_proto_source argv exactly:
                    //   -c -fPIC -std=c++17 [cxxflags...]
                    //   -I<parent_of_pb_cc> -I<genfiles> -I<workspace>
                    //   -I/usr/include -o <obj> <src>
                    let mut argv_strs: Vec<String> = Vec::new();
                    argv_strs.push("-c".to_string());
                    argv_strs.push("-fPIC".to_string());
                    argv_strs.push("-std=c++17".to_string());
                    argv_strs.extend(cxxflags.iter().cloned());
                    argv_strs.push(format!("-I{}", pb_cc_parent.display()));
                    argv_strs.push(format!("-I{}", genfiles_for_compile.display()));
                    argv_strs
                        .push(format!("-I{}", workspace_root_for_compile.display()));
                    // #32: pinned protobuf 3.21.9 headers when materialized.
                    argv_strs.push(format!(
                        "-I{}",
                        rush_rules::rules_system::protobuf_include_dir()
                    ));
                    for include_dir in &dep_include_dirs {
                        let flag = format!("-I{}", include_dir.display());
                        if !argv_strs.contains(&flag) {
                            argv_strs.push(flag);
                        }
                    }
                    // #39 absl-namespace fix (mirror compile_proto_source): drop
                    // -I/usr/include + inject the pinned Abseil -I so proto .pb.cc
                    // bind vendored lts_20230125, not the host debian7 absl.
                    rush_rules::cc_command_spec::demote_host_system_includes(&mut argv_strs);
                    argv_strs.push("-o".to_string());
                    argv_strs.push(obj_abs.display().to_string());
                    argv_strs.push(pb_cc_abs.display().to_string());

                    let exe_pid = ctx.intern_path(&cxx_pathbuf);
                    let argv: Vec<StringId> = argv_strs
                        .iter()
                        .map(|s| ctx.intern_string(s))
                        .collect();
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
        }

        // ----- CcArchive over the obj set -----
        let mut public_lib_aid: Option<ArtifactId> = None;
        let mut direct_linker_inputs: Vec<rush_action::ids::LinkerInputId> =
            Vec::new();
        if !obj_aids.is_empty() {
            // Match legacy: lib_path = target_output_dir.join("lib{name}.a")
            // with NO safe_name replacement (cc_proto_library uses
            // safe_name for "/" handling; tf_proto's name is a
            // single segment in BUILD).
            // grpc (has_services) joins CcImpl in package-qualifying the archive
            // path (#39 collision fix): the linker_input + DefaultInfo below point
            // at lib_aid, so this is what //tensorflow:tensorflow_cc links. A flat
            // path collided with the same-named non-grpc worker_proto.
            // #39 collision fix (2026-06-15): package-qualify ALL roles (see
            // build()'s target_output_dir comment). Two same-named Default-role
            // protos across packages (tpu vs profiler topology_proto) clobbered
            // the flat rush-out/<name>/lib<name>.a. Matches build()/legacy-spec.
            let lib_rel = if pkg.is_empty() {
                format!("{}/lib{}.a", self.name, self.name)
            } else {
                format!("{}/{}/lib{}.a", pkg, self.name, self.name)
            };
            let lib_aid = ctx.register_generated_artifact(
                Path::new(&lib_rel),
                ArtifactKind::StaticLibrary,
                None,
            );
            let lib_path_str =
                ctx.registry().artifact_path(lib_aid).display().to_string();
            let obj_path_strs: Vec<String> = obj_aids
                .iter()
                .map(|aid| ctx.registry().artifact_path(*aid).display().to_string())
                .collect();
            let ar_pathbuf = ctx.toolchain().ar.clone();

            if let Ok(archive_id) =
                ctx.declare_action("CcArchive", obj_aids.clone(), vec![lib_aid])
            {
                if !grpc_path {
                    let ar_path = ctx.intern_path(&ar_pathbuf);
                    let mut argv: Vec<StringId> =
                        Vec::with_capacity(3 + obj_path_strs.len());
                    argv.push(ctx.intern_string("rcs"));
                    argv.push(ctx.intern_string(&lib_path_str));
                    argv.push(ctx.intern_string("--thin"));
                    for s in &obj_path_strs {
                        argv.push(ctx.intern_string(s));
                    }
                    ctx.set_executable_action(
                        archive_id,
                        ExecutableAction {
                            executable: ar_path,
                            argv: argv.into_boxed_slice(),
                            env: Box::new([]),
                            working_dir: None,
                            env_policy: rush_action::ids::ActionEnvPolicy::InheritHost,
                        },
                    );
                }
            }

            // #39 producer-side proto whole-archive: a `_cc_impl` archive holds
            // generated proto TUs whose message symbols + static descriptor
            // registration must travel as a UNIT with whichever shared library
            // owns the proto (Bazel marks `cc_proto_library` impls
            // `alwayslink=1`). Linking it demand-loaded lets GNU ld drop the
            // members nothing in the owning .so references, so the owner
            // under-exports (e.g. libtensorflow_framework.so pulled some
            // xla_data.proto members but not the `ShapeProto` one → cc.so's
            // `xla::ShapeProto` stayed undefined at load). Marking the impl
            // `alwayslink` makes the owning cc_shared_library whole-archive it
            // (`--whole-archive`), exporting ALL its proto symbols; the consumer
            // then prunes it (symbol-aware dynamic-dep prune) and resolves at
            // load — no double-link, no descriptor double-registration. Only the
            // impl carries this; the header-only Default/`_cc` role does not.
            let lid = ctx.register_linker_input(lib_aid, /*alwayslink*/ is_cc_impl);
            direct_linker_inputs.push(lid);
            public_lib_aid = Some(lib_aid);
        }

        // ----- CcInfo with typed provider exposure -----
        let direct_link_fragments: Vec<LinkFragment> =
            compute_proto_link_flags(self.generates_grpc())
                .iter()
                .map(|f| LinkFragment::Flag(ctx.intern_string(f)))
                .collect();
        let direct = DirectCcInfo {
            headers: direct_headers,
            include_dirs: direct_include_dirs,
            linker_inputs: direct_linker_inputs,
            link_fragments: direct_link_fragments,
            ..DirectCcInfo::default()
        };
        let cc_info = CcInfo::from_native_direct_and_deps(
            direct,
            rush_rules::providers::extract_cc_infos(dep_providers).into_iter(),
        );
        let mut providers = ProviderSet::empty().with_cc_info(cc_info);
        if let Some(aid) = public_lib_aid {
            providers = providers.with_default_info(DefaultInfo {
                files: vec![aid],
            });
        }
        providers
    }

    fn rule_type(&self) -> &str {
        "tf_proto_library"
    }

    fn configuration_error(&self) -> Option<&str> {
        (self.role == TfProtoRole::GrpcSibling).then_some(UNSUPPORTED_GRPC_SIBLING)
    }

    /// Phase 5.C.4 commit 31a: shape-gate the grpc path.
    /// tf_proto_library is on SCHEDULER_RULE_ALLOWLIST_REF
    /// (commit 22), but only the non-grpc shape has been
    /// verifier-zero-diff. Targets with `has_services` or
    /// `create_grpc_library` set carry the grpc-codegen
    /// outputs (.grpc.pb.h/.cc) and an extra protoc invocation
    /// path; commit 31d will land that migration.
    ///
    /// Pro's commit-31a directive: "rule-kind allowlist alone is
    /// too blunt. ... Add a shape predicate before any grpc
    /// target can accidentally go scheduler path."
    ///
    /// The eligibility predicate (build_orchestrator_ext.rs)
    /// consults this method and routes Some(...) returns to
    /// `SchedulerEligibilityReason::UnsupportedShape`, which
    /// blocks dispatch and routes the target to legacy.
    fn scheduler_shape_unsupported(&self) -> Option<&'static str> {
        if self.role == TfProtoRole::GrpcSibling {
            return Some(UNSUPPORTED_GRPC_SIBLING);
        }
        if self.generates_grpc() {
            Some(
                "tf_proto_library GRPC/plugin generation is not supported by \
                 the native scheduler (has_services, create_grpc_library or \
                 cc_grpc_version requests the plugin)",
            )
        } else {
            None
        }
    }

    /// Phase 5.C.4 commit 55: tf_proto_library always needs
    /// `protoc`. When `has_services` or `create_grpc_library`
    /// is set, the grpc shape additionally needs
    /// `grpc_cpp_plugin`. Today the grpc shape is gated to
    /// legacy via `scheduler_shape_unsupported` above, but
    /// declaring the tool requirement here means future
    /// preflight surfaces (and the tool probe cache) report
    /// the grpc plugin as a required-but-missing tool the
    /// instant grpc lands as scheduler-eligible — no
    /// retrofit needed.
    fn scheduler_required_tools(&self) -> Vec<rush_rules::RequiredTool> {
        if self.role == TfProtoRole::GrpcSibling {
            return Vec::new();
        }
        let mut tools = if self.protoc.is_some() || self.role != TfProtoRole::Default || self.srcs.is_empty() {
            Vec::new()
        } else {
            vec![rush_rules::RequiredTool::Protoc]
        };
        if self.generates_grpc() {
            tools.push(rush_rules::RequiredTool::GrpcCppPlugin);
        }
        tools
    }

    /// Phase 5.C.4 commit 22: legacy specs for the full
    /// Protoc → CppCompile → CcArchive chain. Mirrors what
    /// build()'s phase-1/phase-2/static-library code emits.
    /// Skipped entirely on the grpc path (commit 23).
    async fn build_legacy_command_specs(
        &self,
        context: &rush_rules::context::BuildContext,
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
        if self.protoc.is_some() || self.role != TfProtoRole::Default
            || context.native_compile_mode() == rush_rules::context::NativeCompileMode::UncachedDiagnosticV1 {
            return self.declared_proto_specs(context).await.unwrap_or_default();
        }
        use rush_action::command_spec::NormalizedCommandSpec;
        use rush_rules::cc_command_spec::{CcArchiveCommandSpec, CcCompileCommandSpec, IncludeLayout};
        use rush_rules::SchedulerActionKey;
        use std::path::PathBuf;

        let mut specs = Vec::new();
        if self.srcs.is_empty() {
            return specs;
        }
        // grpc path: scheduler dispatch deferred to commit 23.
        // Returning empty here is allowed because the eligibility
        // predicate (ActionGraphIncomplete) keeps grpc targets
        // off the scheduler; verifier never reaches them.
        if self.generates_grpc() {
            return specs;
        }

        let pkg = context
            .package_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let is_cc_impl = matches!(self.role, TfProtoRole::CcImpl);
        let genfiles_dir = context.output_dir.join("genfiles");
        // CcImpl package-qualifies its obj/archive dir so same-named impls in
        // different packages don't collide; must match analyze()'s registry
        // paths byte-for-byte or the verifier diverges.
        // #39 collision fix (2026-06-15): package-qualify ALL roles to match
        // analyze()/build() (see build()'s target_output_dir comment). Was
        // CcImpl-only; the flat Default layout collided across same-named protos.
        let target_output_dir = if !pkg.is_empty() {
            context.output_dir.join(&pkg).join(&self.name)
        } else {
            context.output_dir.join(&self.name)
        };
        let workspace_root = context.workspace_root.clone();
        let cxxflags = context.toolchain.cxxflags.clone();
        let cxx = context.toolchain.cxx.clone();
        let ar = context.toolchain.ar.clone();
        let extra_proto_paths: Vec<PathBuf> = context.extra_proto_paths.clone();

        let mut dep_generated_header_paths: Vec<PathBuf> = Vec::new();
        let mut dep_include_dirs: Vec<PathBuf> = Vec::new();
        if let Some(reg_arc) = context.artifact_registry.as_ref() {
            let reg = reg_arc.lock().await;
            let cc_infos: Vec<std::sync::Arc<rush_rules::providers::CcInfo>> = context
                .dependency_providers
                .values()
                .filter_map(|p| p.get_cc_info().cloned())
                .collect();
            dep_generated_header_paths =
                rush_rules::providers::collect_generated_header_paths(&reg, &cc_infos);
            dep_include_dirs = collect_dependency_include_dirs(&reg, &cc_infos);
        }

        let mut obj_paths: Vec<PathBuf> = Vec::new();

        for src in &self.srcs {
            let src_path = context.source_path(src);
            if !src_path.exists() {
                continue;
            }
            let stem = std::path::Path::new(src)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("proto");
            // Same empty-pkg handling as analyze() — bare
            // PathBuf::join with an absolute-prefixed string
            // would replace the genfiles_dir prefix entirely.
            let pb_h = if pkg.is_empty() {
                genfiles_dir.join(format!("{}.pb.h", stem))
            } else {
                genfiles_dir.join(format!("{}/{}.pb.h", pkg, stem))
            };
            let pb_cc = if pkg.is_empty() {
                genfiles_dir.join(format!("{}.pb.cc", stem))
            } else {
                genfiles_dir.join(format!("{}/{}.pb.cc", pkg, stem))
            };
            // ----- ProtocCompile spec -----
            let mut argv: Vec<String> = Vec::new();
            argv.push("--experimental_allow_proto3_optional".to_string());
            argv.push(format!("--cpp_out={}", genfiles_dir.display()));
            argv.push(format!("--proto_path={}", workspace_root.display()));
            for extra in &extra_proto_paths {
                argv.push(format!("--proto_path={}", extra.display()));
            }
            argv.push(src_path.display().to_string());

            // CcImpl does NOT own protoc (the Default target does), so it
            // emits no ProtocCompile spec — matching analyze()'s skip. Emitting
            // it here would make PASS-B carry a ProtocCompile that PASS-A lacks.
            if !is_cc_impl {
                let mut protoc_spec = NormalizedCommandSpec {
                    mnemonic: "ProtocCompile".to_string(),
                    // Resolved protoc — see the note above.
                    executable: rush_rules::rules_system::protobuf_protoc_path(),
                    cwd: None,
                    argv,
                    env: Vec::new(),
                    declared_inputs: vec![src_path.clone()],
                    declared_outputs: vec![pb_h.clone(), pb_cc.clone()],
                    toolchain_identity: None,
                    param_files: Vec::new(),
                    env_policy: rush_action::ids::ActionEnvPolicy::InheritHost,
                };
                protoc_spec.normalize();
                specs.push((
                    SchedulerActionKey {
                        mnemonic: "ProtocCompile".to_string(),
                        primary_output_path: pb_h.clone(),
                    },
                    protoc_spec,
                ));
            }

            // BD-1: a header-only wrapper declares no compiled object or
            // archive in analyze(), so the legacy spec must match — else
            // the verifier sees legacy-only CppCompile/CcArchive actions
            // and diverges. ProtocCompile (headers) still runs above; the
            // object/archive are skipped exactly as in PASS-A. The CcImpl
            // role is the exception: it DOES compile+archive (that's its whole
            // purpose), so it does not take the header-only skip.
            if self.make_default_target_header_only && !is_cc_impl {
                continue;
            }
            let obj_path = target_output_dir.join(format!("{}.pb.o", stem));
            obj_paths.push(obj_path.clone());

            // ----- CppCompile spec on .pb.cc -----
            // tf_proto's compile shape needs four direct includes:
            // [parent_of_pb_cc, genfiles, workspace_root, /usr/include]
            // — different from cc_proto_library's compile (which
            // is [parent, /usr/include] only). Build the spec
            // inline with IncludeLayout::Standalone.
            let pb_cc_parent = pb_cc
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from(""));
            let compile = CcCompileCommandSpec {
                mnemonic: "CppCompile".to_string(),
                executable: cxx.clone(),
                cwd: None,
                is_c_source: false,
                // Generated proto .pb.cc TUs have no @llvm-project dep.
                wants_no_rtti: false,
                src_path: pb_cc.clone(),
                obj_path: obj_path.clone(),
                copts: Vec::new(),
                cxxflags: cxxflags.clone(),
                cflags: Vec::new(),
                defines: Vec::new(),
                // Generated proto .pb.cc TUs inherit no transitive defines.
                dep_defines: Vec::new(),
                direct_include_paths: vec![
                    pb_cc_parent,
                    genfiles_dir.clone(),
                    workspace_root.clone(),
                    // #32: pinned protobuf 3.21.9 headers when materialized.
                    PathBuf::from(rush_rules::rules_system::protobuf_include_dir()),
                ],
                direct_quote_include_paths: Vec::new(),
                dep_include_paths: dep_include_dirs.clone(),
                include_layout: IncludeLayout::Standalone,
                declared_input_paths: {
                    let mut inputs = vec![pb_cc.clone(), pb_h.clone()];
                    inputs.extend(dep_generated_header_paths.iter().cloned());
                    inputs
                },
                declared_output_paths: vec![obj_path.clone()],
                toolchain_identity: None,
            };
            specs.push((
                SchedulerActionKey {
                    mnemonic: "CppCompile".to_string(),
                    primary_output_path: obj_path,
                },
                compile.to_normalized_command_spec(),
            ));
        }

        // ----- CcArchive over the obj set -----
        if !obj_paths.is_empty() {
            // tf_proto's lib name has no safe_name munging.
            let lib_path = target_output_dir.join(format!("lib{}.a", self.name));
            let archive = CcArchiveCommandSpec {
                mnemonic: "CcArchive".to_string(),
                executable: ar.clone(),
                cwd: None,
                lib_path: lib_path.clone(),
                object_paths: obj_paths.clone(),
                declared_input_paths: obj_paths.clone(),
                declared_output_paths: vec![lib_path.clone()],
                toolchain_identity: None,
            };
            specs.push((
                SchedulerActionKey {
                    mnemonic: "CcArchive".to_string(),
                    primary_output_path: lib_path,
                },
                archive.to_normalized_command_spec(),
            ));
        }

        specs
    }
}

/// Compute the link flags tf_proto_library exposes to dependents.
/// Shared between providers() (legacy) and analyze() (typed) so the
/// flag list stays in sync during the migration window.
fn compute_proto_link_flags(has_services: bool) -> Vec<String> {
    // #32: prefer TF's pinned protobuf 3.21.9 static archive when materialized
    // (system protobuf 4.25 breaks op-gen TextFormat parsing); else -lprotobuf.
    let mut link_flags = rush_rules::rules_system::protobuf_runtime_link_flags();
    if has_services {
        // grpc service codegen needs grpc runtime symbols at link
        // time. libgrpc++ depends transitively on libgrpc and
        // absl_synchronization; list them explicitly so g++ finds
        // them regardless of linker default resolution order.
        link_flags.push("-lgrpc++".to_string());
        link_flags.push("-lgrpc".to_string());
        link_flags.push("-lgpr".to_string());
        // System grpc++ headers include system absl headers, so the
        // generated .grpc.pb.o references absl::Mutex et al directly.
        // ld.bfd refuses to resolve those through libgrpc++'s DT_NEEDED
        // ("DSO missing from command line") — the absl DSO must be
        // named explicitly.
        link_flags.push("-labsl_synchronization".to_string());
    }
    link_flags
}

impl TfProtoLibraryInstance {
    async fn compile_proto_source(
        &self,
        context: &rush_rules::context::BuildContext,
        src_path: &std::path::Path,
        obj_path: &std::path::Path,
        dependency_generated_headers: &[PathBuf],
        dependency_include_dirs: &[PathBuf],
    ) -> Result<()> {
        let mut args = Vec::new();
        args.push("-c".to_string());
        args.push("-fPIC".to_string());
        args.push("-std=c++17".to_string());

        for flag in &context.toolchain.cxxflags {
            args.push(flag.clone());
        }

        if let Some(parent) = src_path.parent() {
            args.push(format!("-I{}", parent.display()));
        }

        // Add genfiles directory to includes - generated proto files use paths like "lib/child.pb.h"
        // so we need the genfiles root in the include path
        let genfiles_dir = context.output_dir.join("genfiles");
        args.push(format!("-I{}", genfiles_dir.display()));

        // Add workspace root to includes
        args.push(format!("-I{}", context.workspace_root.display()));

        // Add protobuf include path (#32: pinned 3.21.9 when materialized).
        args.push(format!(
            "-I{}",
            rush_rules::rules_system::protobuf_include_dir()
        ));

        for include_dir in dependency_include_dirs {
            let flag = format!("-I{}", include_dir.display());
            if !args.contains(&flag) {
                args.push(flag);
            }
        }

        // #39 absl-namespace fix: proto .pb.cc TUs `#include` Abseil (protobuf
        // 3.21 uses absl) but this hand-built argv never added the pinned-absl
        // `-I`, so GCC's default `/usr/include/absl` (Debian `debian7` ns)
        // resolved them — leaving e.g. `libprotocol_proto.a` referencing
        // `absl::debian7::Mutex::Dtor` (undefined; TF vendors `lts_20230125`).
        // Mirror the normal C++ compile path: drop `-I/usr/include` and inject
        // the pinned Abseil `-I` so proto objects bind the single vendored ABI.
        rush_rules::cc_command_spec::demote_host_system_includes(&mut args);

        args.push("-o".to_string());
        args.push(obj_path.to_string_lossy().to_string());
        args.push(src_path.to_string_lossy().to_string());

        let command = Command {
            executable: context.toolchain.cxx.clone(),
            args,
            working_dir: None,
            execution_info: ExecutionInfo::default(),
        };

        let mut inputs = Vec::with_capacity(1 + dependency_generated_headers.len());
        inputs.push(ActionArtifact::Source(src_path.to_path_buf()));
        for header in dependency_generated_headers {
            let artifact = ActionArtifact::Source(header.clone());
            if !inputs.contains(&artifact) {
                inputs.push(artifact);
            }
        }
        let outputs = vec![ActionArtifact::Source(obj_path.to_path_buf())];

        let action_id = Action::compute_id(&inputs, &command, &HashMap::new())?;

        let action = Action {
            id: action_id,
            command,
            env: HashMap::new(),
            inputs,
            outputs,
            mnemonic: "CppCompile".to_string(),
        };

        let result = context.execute_action(action).await.with_context(|| {
            format!(
                "execute_action(g++ proto compile) failed for src={} obj={}",
                src_path.display(),
                obj_path.display()
            )
        })?;

        // A virtual cache hit legitimately leaves the object CAS-backed;
        // absent AND unregistered is still a producer bug.
        if !rush_action::virtual_artifacts::present_or_virtual(&obj_path) {
            return Err(anyhow!(
                "tf_proto_library: g++ exit_code={} cached={} for {} but obj `{}` \
                 was not created (stdout: {} | stderr: {})",
                result.exit_code,
                result.cached,
                src_path.display(),
                obj_path.display(),
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr),
            ));
        }

        if result.exit_code != 0 {
            return Err(anyhow!(
                "Compilation failed for {}: {}",
                src_path.display(),
                String::from_utf8_lossy(&result.stderr)
            ));
        }

        Ok(())
    }

    async fn create_static_library(
        &self,
        context: &rush_rules::context::BuildContext,
        object_files: &[PathBuf],
        lib_path: &std::path::Path,
    ) -> Result<()> {
        // #39 grpc-archiving: `ar rcs` only ADDS/replaces named members, never
        // drops stale ones, and RushActionManager treats an existing output as a
        // cache hit. Remove any pre-existing archive so it is rebuilt fresh from
        // exactly this object set (mirrors the scheduler CcArchive pre-remove and
        // commit 285's lib.rs / rules_cc / tf_cc_library fix — this path was missed).
        // The pre-remove was moved into the executor (LocalStrategy::execute)
        // so it only fires on a cache MISS; doing it eagerly here defeated
        // RushActionManager's cache-hit gate (it re-checks that the output
        // exists on disk), forcing every proto _genproto_owner archive to
        // re-run on every build (152 redundant `ar` calls on a TF no-op).
        let mut args = vec![
            "rcs".to_string(),
            lib_path.to_string_lossy().to_string(),
            "--thin".to_string(),
        ];
        for obj in object_files {
            args.push(obj.to_string_lossy().to_string());
        }

        let command = Command {
            executable: PathBuf::from("ar"),
            args,
            working_dir: None,
            execution_info: ExecutionInfo::default(),
        };

        let inputs: Vec<_> = object_files
            .iter()
            .map(|p| ActionArtifact::Source(p.clone()))
            .collect();
        let outputs = vec![ActionArtifact::Source(lib_path.to_path_buf())];

        let action_id = Action::compute_id(&inputs, &command, &HashMap::new())?;

        let action = Action {
            id: action_id,
            command,
            env: HashMap::new(),
            inputs,
            outputs,
            mnemonic: "CppArchive".to_string(),
        };

        let result = context.execute_action(action).await?;

        if result.exit_code != 0 {
            return Err(anyhow!(
                "Archive creation failed: {}",
                String::from_utf8_lossy(&result.stderr)
            ));
        }

        Ok(())
    }
}

/// Alias instance for proto library implicit targets (_cc, _cc_impl, etc.)
/// These aliases point back to the base proto target for dependency resolution
pub struct ProtoAliasInstance {
    alias_name: String,
    actual_target: String,
}

impl ProtoAliasInstance {
    pub fn new(alias_name: String, actual_target: String) -> Box<dyn RuleInstance> {
        Box::new(ProtoAliasInstance {
            alias_name,
            actual_target,
        })
    }
}

#[async_trait]
impl RuleInstance for ProtoAliasInstance {
    fn target_name(&self) -> &str {
        &self.alias_name
    }

    fn dependencies(&self) -> Vec<String> {
        // Alias depends on the actual target
        vec![self.actual_target.clone()]
    }

    fn identity_artifact_forwarder(&self) -> Option<&str> {
        Some(&self.actual_target)
    }

    async fn build(&self, _context: &rush_rules::context::BuildContext) -> Result<Vec<Artifact>> {
        // This instance produces nothing. A driver that admits its identity
        // forwarding request must skip this method AND scheduler dispatch,
        // after independently validating the singleton actual identity.
        Ok(Vec::new())
    }

    fn providers(&self) -> rush_rules::providers::ProviderSet {
        rush_rules::providers::ProviderSet::empty()
    }

    fn rule_type(&self) -> &str {
        "proto_alias"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rush_action::ids::{ArtifactRegistry, ConfigId, TargetId};
    use rush_rules::analysis_context::{AnalysisContext, StubIncludeResolver};
    use rush_rules::context::Toolchain;
    use rush_rules::providers::{CcInfo, DirectCcInfo, ProviderSet};
    use std::sync::Arc;
    use tempfile::TempDir;

    #[tokio::test]
    async fn proto_alias_identity_request_survives_metadata_without_own_work() -> Result<()> {
        use rush_rules::context::{BuildContext, NativeCompileMode, PendingCompileDeclarations};
        use rush_rules::{with_visibility, BuildRule};
        use std::path::{Path, PathBuf};

        let temp = TempDir::new()?;
        let source = temp.path().join("source");
        let output = temp.path().join("out");
        std::fs::create_dir_all(source.join("pkg"))?;
        let header = source.join("pkg/public.h");
        std::fs::write(&header, "#pragma once\n#define PUBLIC_VALUE 42\n")?;
        let config = ConfigId(7);
        let mut registry = ArtifactRegistry::new();
        let resolver = StubIncludeResolver::new(source.clone());
        let toolchain = Toolchain::default();
        // A genuine native header-library analysis supplies real nonempty
        // dependency metadata. The alias must not recreate or claim any of it.
        let actual = rush_rules::rules_cc::CcLibraryRule.parse_attributes(&HashMap::from([
            ("name".into(), Value::String("messages".into())),
            (
                "hdrs".into(),
                Value::List(vec![Value::String("public.h".into())]),
            ),
        ]))?;
        assert_eq!(actual.identity_artifact_forwarder(), None);
        let actual_provider = Arc::new({
            let mut ctx = AnalysisContext::new(
                TargetId(1),
                config,
                "pkg",
                Some("actual_owner"),
                &output,
                &mut registry,
                &resolver,
                &toolchain,
                &source,
                &[],
            );
            actual.analyze(&mut ctx, &[])
        });
        let actual_cc = actual_provider
            .get_cc_info()
            .context("actual library lost CcInfo")?;
        let header_id = actual_cc
            .compilation
            .headers
            .iter()
            .next()
            .context("actual library lost header")?;
        assert_eq!(
            registry.find_artifact_by_path(&header, config),
            Some(header_id)
        );
        assert_eq!(
            registry.find_artifact_by_path(&header, ConfigId::DEFAULT),
            None
        );
        let actual_label = "@@actual_owner//pkg:messages";
        let visibility = vec!["//visibility:public".to_owned()];
        let alias = with_visibility(
            ProtoAliasInstance::new("messages_cc".into(), actual_label.into()),
            Some(visibility.clone()),
        );
        assert_eq!(alias.target_name(), "messages_cc");
        assert_eq!(alias.visibility(), Some(visibility.as_slice()));
        assert_eq!(alias.identity_artifact_forwarder(), Some(actual_label));
        assert_eq!(alias.dependencies(), vec![actual_label.to_owned()]);
        assert!(alias.declared_outputs().is_empty());
        assert!(alias.providers().is_empty());
        let labeled = vec![(actual_label.to_owned(), actual_provider.clone())];
        for mode in [
            NativeCompileMode::Literal,
            NativeCompileMode::UncachedDiagnosticV1,
        ] {
            let artifact_count = registry.artifact_count();
            let action_count = registry.declared_action_count();
            let mut pending = PendingCompileDeclarations::default();
            {
                let mut ctx = AnalysisContext::new(
                    TargetId(2),
                    config,
                    "pkg",
                    Some("actual_owner"),
                    &output,
                    &mut registry,
                    &resolver,
                    &toolchain,
                    &source,
                    &[],
                )
                .with_labeled_dep_providers(&labeled)
                .with_native_compile_discovery(mode, &mut pending);
                alias.validate_analysis_dependencies(&ctx)?;
                assert!(alias
                    .analyze(&mut ctx, &[actual_provider.clone()])
                    .is_empty());
                assert!(ctx.native_namespace_analysis_error().is_none());
            }
            assert_eq!(registry.artifact_count(), artifact_count);
            assert_eq!(registry.declared_action_count(), action_count);
            assert!(registry.actions_of(TargetId(2)).is_empty());
            assert_eq!(pending.iter().count(), 0);
            let context = BuildContext::new(source.clone(), output.clone())
                .with_package(PathBuf::from("pkg"))
                .with_dependencies(
                    HashMap::new(),
                    vec![(actual_label.into(), actual_provider.as_ref().clone())],
                )
                .with_native_compile_discovery(mode, Arc::new(pending));
            assert!(alias.build_legacy_command_specs(&context).await.is_empty());
            assert!(alias.build(&context).await?.is_empty());
            assert_eq!(
                registry.find_artifact_by_path(Path::new(&header), config),
                Some(header_id)
            );
        }
        // The ordinary alias implementation, even with the same target name
        // and an unrecognized declaration-looking attribute, cannot grant this
        // separate native hook through a name/kwargs convention.
        let ordinary = crate::alias::AliasRule.parse_attributes(&HashMap::from([
            ("name".into(), Value::String("messages_cc".into())),
            ("actual".into(), Value::String(actual_label.into())),
            (
                "identity_artifact_forwarder".into(),
                Value::String(actual_label.into()),
            ),
        ]))?;
        assert_eq!(ordinary.identity_artifact_forwarder(), None);
        assert_eq!(
            with_visibility(ordinary, Some(visibility)).identity_artifact_forwarder(),
            None
        );
        Ok(())
    }

    #[test]
    fn declared_proto_rejects_malformed_inputs_instead_of_empty_aggregation() -> Result<()> {
        let base = HashMap::from([
            ("name".into(), Value::String("messages".into())),
            (
                "srcs".into(),
                Value::List(vec![Value::String("message.proto".into())]),
            ),
            ("protoc".into(), Value::String("@@tools//:protoc".into())),
        ]);
        for attribute in ["srcs", "protodeps", "cc_libs"] {
            for invalid in [
                Value::List(vec![Value::Int(1)]),
                Value::Dict(HashMap::new()),
                Value::String("select(unresolved)".into()),
                Value::None,
            ] {
                let mut attrs = base.clone();
                attrs.insert(attribute.into(), invalid);
                let error = TfProtoLibraryRule
                    .parse_attributes(&attrs)
                    .err()
                    .context("malformed declared proto collection was silently dropped")?;
                assert!(error.to_string().contains(attribute), "{error:#}");
            }
        }
        for (attribute, invalid) in [
            ("_rush_tf_proto_role", Value::String("unknown".into())),
            ("_rush_tf_proto_generation_owner", Value::None),
            ("protoc", Value::String(String::new())),
            ("has_services", Value::List(Vec::new())),
            ("has_services", Value::String("true".into())),
            ("make_default_target_header_only", Value::Int(2)),
            (
                "make_default_target_header_only",
                Value::String("true".into()),
            ),
            ("create_grpc_library", Value::Dict(HashMap::new())),
        ] {
            let mut attrs = base.clone();
            attrs.insert(attribute.into(), invalid);
            assert!(TfProtoLibraryRule.parse_attributes(&attrs).is_err());
        }
        let aggregate = TfProtoLibraryRule.parse_attributes(&HashMap::from([
            ("name".into(), Value::String("aggregate".into())),
            ("srcs".into(), Value::List(Vec::new())),
            (
                "protodeps".into(),
                Value::List(vec![Value::String(":real_dependency".into())]),
            ),
            ("has_services".into(), Value::None),
        ]))?;
        assert_eq!(aggregate.dependencies(), vec![":real_dependency"]);
        Ok(())
    }

    #[tokio::test]
    async fn declared_proto_grpc_sibling_refuses_even_with_old_outputs_and_empty_sources(
    ) -> Result<()> {
        use rush_rules::context::{BuildContext, NativeCompileMode};
        let temp = TempDir::new()?;
        let output = temp.path().join("output");
        std::fs::create_dir_all(output.join("genfiles/pkg"))?;
        let old_header = output.join("genfiles/pkg/message.grpc.pb.h");
        std::fs::write(&old_header, "unchanged old output sentinel\n")?;
        for sources in [Vec::new(), vec![Value::String(":messages_proto".into())]] {
            let rule = TfProtoLibraryRule.parse_attributes(&HashMap::from([
                (
                    "name".into(),
                    Value::String("messages_cc_grpc_proto".into()),
                ),
                (
                    "_rush_tf_proto_role".into(),
                    Value::String("grpc_sibling".into()),
                ),
                ("srcs".into(), Value::List(sources)),
                (
                    "protodeps".into(),
                    Value::List(vec![Value::String(":messages_proto_cc".into())]),
                ),
            ]))?;
            assert_eq!(rule.configuration_error(), Some(UNSUPPORTED_GRPC_SIBLING));
            assert_eq!(rule.dependencies(), [":messages_proto_cc"]);
            assert!(rule.exec_only_dependencies().is_empty());
            for mode in [
                NativeCompileMode::Literal,
                NativeCompileMode::UncachedDiagnosticV1,
            ] {
                let resolver = StubIncludeResolver::new(temp.path().into());
                let toolchain = Toolchain::default();
                let mut registry = ArtifactRegistry::new();
                let mut pending = rush_rules::context::PendingCompileDeclarations::default();
                let mut analysis = AnalysisContext::new(
                    TargetId(1),
                    ConfigId::DEFAULT,
                    "pkg",
                    None,
                    &output,
                    &mut registry,
                    &resolver,
                    &toolchain,
                    temp.path(),
                    &[],
                )
                .with_native_compile_discovery(mode, &mut pending);
                assert!(rule.validate_analysis_dependencies(&analysis).is_err());
                rule.analyze(&mut analysis, &[]);
                assert!(analysis.native_namespace_analysis_error().is_some());
                assert!(analysis.registry().actions_of(TargetId(1)).is_empty());
                let context = BuildContext::new(temp.path().into(), output.clone())
                    .with_native_compile_discovery(mode, Arc::new(Default::default()));
                let error = rule.build(&context).await.unwrap_err();
                assert!(
                    format!("{error:#}").contains(UNSUPPORTED_GRPC_SIBLING),
                    "{error:#}"
                );
                assert_eq!(
                    std::fs::read_to_string(&old_header)?,
                    "unchanged old output sentinel\n"
                );
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn declared_proto_grpc_version_refuses_before_output_creation() -> Result<()> {
        let temp = TempDir::new()?;
        let output = temp.path().join("must-not-exist");
        for version in [
            Value::String("1.0".into()),
            Value::Bool(true),
            Value::Int(1),
        ] {
            let rule = TfProtoLibraryRule.parse_attributes(&HashMap::from([
                ("name".into(), Value::String("messages".into())),
                (
                    "srcs".into(),
                    Value::List(vec![Value::String("message.proto".into())]),
                ),
                ("protoc".into(), Value::String("@@tools//:protoc".into())),
                ("has_services".into(), Value::None),
                ("cc_grpc_version".into(), version),
            ]))?;
            assert!(rule.scheduler_shape_unsupported().is_some());
            let context =
                rush_rules::context::BuildContext::new(temp.path().into(), output.clone())
                    .with_native_compile_discovery(
                        rush_rules::context::NativeCompileMode::UncachedDiagnosticV1,
                        Arc::new(rush_rules::context::PendingCompileDeclarations::default()),
                    );
            let error = rule.build(&context).await.unwrap_err();
            assert!(format!("{error:#}").contains("GRPC/plugin"), "{error:#}");
            assert!(
                !output.exists(),
                "unsupported plugin must refuse before any write"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn declared_proto_owner_and_impl_share_outputs_and_pending_producers() -> Result<()> {
        use rush_action::ids::ArtifactKind;
        use rush_rules::context::{BuildContext, NativeCompileMode, PendingCompileDeclarations};
        use rush_rules::providers::DefaultInfo;
        let temp = TempDir::new()?;
        let root = temp.path().join("source");
        let output = temp.path().join("output");
        let execution_root = temp.path().join("main-execution");
        let resolver = StubIncludeResolver::new(root.clone());
        let toolchain = Toolchain::default();
        let mut registry = ArtifactRegistry::new();
        let mut pending = PendingCompileDeclarations::default();
        let tool;
        {
            let mut ctx = AnalysisContext::new(
                TargetId(1),
                ConfigId::DEFAULT,
                "",
                Some("protobuf"),
                &output,
                &mut registry,
                &resolver,
                &toolchain,
                &root,
                &[],
            );
            tool = ctx.register_generated_artifact(
                &output.join("protoc"),
                ArtifactKind::Executable,
                None,
            );
            ctx.declare_action("CppLink", Vec::new(), vec![tool])?;
        }
        let tool_path = registry.artifact_path(tool).to_owned();
        let tool_providers = vec![(
            "@protobuf//:protoc".to_string(),
            Arc::new(ProviderSet::empty().with_default_info(DefaultInfo { files: vec![tool] })),
        )];
        let mut owner = make_tf_proto("messages_genproto_owner", false, false, false);
        owner.srcs = vec!["nested/value.proto".into(), "other/value.proto".into()];
        owner.make_default_target_header_only = true;
        owner.protoc = Some("@@protobuf//:protoc".into());
        let owner_provider;
        {
            let mut ctx = AnalysisContext::new(
                TargetId(2),
                ConfigId::DEFAULT,
                "pkg",
                Some("owner"),
                &output,
                &mut registry,
                &resolver,
                &toolchain,
                &root,
                &[],
            )
            .with_labeled_exec_dep_providers(&tool_providers)
            .with_native_execution_root(Some(&execution_root))
            .with_native_compile_discovery(NativeCompileMode::UncachedDiagnosticV1, &mut pending);
            owner_provider = owner.analyze_declared_proto_result(&mut ctx, &[])?;
            assert!(ctx.native_namespace_analysis_error().is_none());
        }
        assert!(
            pending.iter().next().is_none(),
            "generation owner must not compile"
        );
        assert_eq!(registry.actions_of(TargetId(2)).len(), 2);
        assert!(registry
            .find_artifact_by_path(
                &output.join("genfiles/pkg/nested/value.pb.cc"),
                ConfigId::DEFAULT
            )
            .is_some());
        assert!(registry
            .find_artifact_by_path(
                &output.join("genfiles/pkg/other/value.pb.cc"),
                ConfigId::DEFAULT
            )
            .is_some());
        let mut implementation = make_tf_proto("messages_cc_impl", false, false, false);
        implementation.srcs = owner.srcs.clone();
        implementation.role = TfProtoRole::CcImpl;
        implementation.generation_owner = Some(":messages_genproto_owner".into());
        implementation.protodeps = vec![":messages_genproto_owner".into()];
        let labeled = vec![(
            "@owner//pkg:messages_genproto_owner".into(),
            Arc::new(owner_provider),
        )];
        {
            let mut ctx = AnalysisContext::new(
                TargetId(3),
                ConfigId::DEFAULT,
                "pkg",
                Some("owner"),
                &output,
                &mut registry,
                &resolver,
                &toolchain,
                &root,
                &[],
            )
            .with_labeled_dep_providers(&labeled)
            .with_native_execution_root(Some(&execution_root))
            .with_native_compile_discovery(NativeCompileMode::UncachedDiagnosticV1, &mut pending);
            implementation.analyze_declared_proto_result(&mut ctx, &[labeled[0].1.clone()])?;
        }
        assert_eq!(pending.iter().count(), 2);
        for record in pending.iter() {
            assert_eq!(
                record.required_producers().len(),
                2,
                "both real generated header producers must precede each compile"
            );
        }
        assert!(registry.actions_of(TargetId(3)).iter().all(|id| registry
            .strings()
            .str(registry.declared_action(*id).mnemonic)
            != "ProtocCompile"));
        // A non-header-only generation owner also has its own compile/archive.
        // Header demand must select only generation, not those sibling actions.
        let mut combined = make_tf_proto("combined_proto", false, false, false);
        combined.srcs = vec!["subset/value.proto".into()];
        combined.make_default_target_header_only = false;
        combined.protoc = Some("@@protobuf//:protoc".into());
        {
            let mut ctx = AnalysisContext::new(
                TargetId(4), ConfigId::DEFAULT, "pkg", Some("owner"), &output,
                &mut registry, &resolver, &toolchain, &root, &[],
            ).with_labeled_exec_dep_providers(&tool_providers)
                .with_native_execution_root(Some(&execution_root))
                .with_native_compile_discovery(NativeCompileMode::UncachedDiagnosticV1, &mut pending);
            combined.analyze_declared_proto_result(&mut ctx, &[])?;
        }
        assert_eq!(registry.actions_of(TargetId(4)).len(), 3);
        let combined_action = |mnemonic| registry.actions_of(TargetId(4)).iter().copied().find(|id|
            registry.strings().str(registry.declared_action(*id).mnemonic) == mnemonic).unwrap();
        let combined_generator = combined_action("ProtocCompile");
        let combined_compile = combined_action("CppCompile");
        let registry = Arc::new(tokio::sync::Mutex::new(registry));
        let mut owner_context = BuildContext::new(root.clone(), output.clone())
            .with_package("pkg".into())
            .with_repository(Some("owner".into()))
            .with_dependencies(
                HashMap::new(),
                tool_providers
                    .iter()
                    .map(|(label, provider)| (label.clone(), provider.as_ref().clone()))
                    .collect(),
            )
            .with_artifact_registry(registry.clone())
            .with_native_execution_root(Some(execution_root.clone()))
            .with_native_compile_discovery(
                NativeCompileMode::UncachedDiagnosticV1,
                Arc::new(pending),
            );
        let specs = owner.declared_proto_specs(&owner_context).await?;
        assert_eq!(specs.len(), 2);
        for (_, spec) in specs {
            assert_eq!(spec.executable, tool_path);
            assert_eq!(spec.cwd.as_ref(), Some(&execution_root));
            assert!(spec.declared_inputs.contains(&tool_path));
            assert_eq!(spec.mnemonic, "ProtocCompile");
        }
        let (generation, other_owner_compile, foreign) = {
            let registry = registry.lock().await;
            (registry.actions_of(TargetId(2))[0], registry.actions_of(TargetId(3))[0],
                registry.actions_of(TargetId(1))[0])
        };
        owner_context = owner_context.with_native_action_selection(Some(Arc::new(
            std::collections::HashSet::from([generation]),
        )));
        let subset = owner.declared_proto_specs(&owner_context).await?;
        assert_eq!(subset.len(), 1);
        assert_eq!(subset[0].1.mnemonic, "ProtocCompile");
        assert!(subset[0].1.declared_outputs.contains(&output.join("genfiles/pkg/nested/value.pb.h")));
        assert!(!subset[0].1.declared_outputs.contains(&output.join("genfiles/pkg/other/value.pb.h")));
        for selection in [
            std::collections::HashSet::new(),
            std::collections::HashSet::from([other_owner_compile]),
            std::collections::HashSet::from([foreign]),
            std::collections::HashSet::from([rush_action::ids::DeclaredActionId(u32::MAX)]),
        ] {
            owner_context = owner_context.with_native_action_selection(Some(Arc::new(selection)));
            assert!(owner.declared_proto_specs(&owner_context).await.is_err(),
                "empty, foreign and non-protoc execution demand must refuse before writing");
            assert!(!output.exists());
        }
        owner_context = owner_context.with_native_action_selection(None);
        assert_eq!(combined.declared_proto_specs(&owner_context).await?.len(), 3);
        owner_context = owner_context.with_native_action_selection(Some(Arc::new(
            std::collections::HashSet::from([combined_generator]),
        )));
        let selected = combined.declared_proto_specs(&owner_context).await?;
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].1.mnemonic, "ProtocCompile");
        owner_context = owner_context.with_native_action_selection(Some(Arc::new(
            std::collections::HashSet::from([combined_compile]),
        )));
        let error = combined.declared_proto_specs(&owner_context).await.unwrap_err();
        assert!(format!("{error:#}").contains("only real ProtocCompile"), "{error:#}");
        assert!(!output.exists(), "action selection must not execute sibling compiles or create outputs");
        let missing = BuildContext::new(root, output)
            .with_package("pkg".into())
            .with_repository(Some("owner".into()))
            .with_artifact_registry(registry);
        assert!(
            owner.build_declared_proto(&missing).await.is_err(),
            "literal mode must not silently replace the declared tool"
        );
        Ok(())
    }

    #[test]
    fn declared_proto_requires_exact_generated_executable_and_exec_only_edge() -> Result<()> {
        use rush_action::ids::ArtifactKind;
        use rush_rules::providers::DefaultInfo;
        let mut registry = ArtifactRegistry::new();
        let root = PathBuf::from("/source");
        let output = PathBuf::from("/output");
        let resolver = StubIncludeResolver::new(root.clone());
        let toolchain = Toolchain::default();
        let mut ctx = AnalysisContext::new(
            TargetId(1),
            ConfigId::DEFAULT,
            "",
            None,
            &output,
            &mut registry,
            &resolver,
            &toolchain,
            &root,
            &[],
        );
        let source_tool = ctx.register_source_artifact(
            std::path::Path::new("/existing-protoc"),
            ArtifactKind::Executable,
        );
        let provider = ProviderSet::empty().with_default_info(DefaultInfo {
            files: vec![source_tool],
        });
        assert!(
            executable_artifact(ctx.registry(), &provider, ConfigId::DEFAULT).is_err(),
            "a resident binary without a declared producer is not the tool"
        );
        let mut rule = make_tf_proto("messages", false, false, false);
        rule.protoc = Some("@@protobuf//:protoc".into());
        rule.cc_libs = vec!["@@protobuf//:protobuf".into()];
        assert_eq!(rule.exec_only_dependencies(), vec!["@@protobuf//:protoc"]);
        assert!(rule
            .dependencies()
            .contains(&"@@protobuf//:protobuf".to_string()));
        assert!(
            rule.scheduler_required_tools().is_empty(),
            "declared tool must not trigger global protoc probing"
        );
        let foreign = vec![("@foreign//:protoc".to_string(), Arc::new(provider))];
        assert!(exact_provider(
            foreign
                .iter()
                .map(|(label, value)| (label.as_str(), value.as_ref())),
            "@@protobuf//:protoc",
            "",
            None
        )
        .is_err());
        Ok(())
    }
    /// Real rule-command execution only: the orchestrator's existing owned
    /// diagnostic-manager test separately covers depfile/cancellation policy.
    /// The staged binary is actual protoc, not a fabricated generator.
    #[tokio::test]
    #[ignore = "requires RUSH_PROTOC=/absolute/real/protoc, matching /usr/include protobuf headers, C++ compiler and ar; run explicitly"]
    async fn declared_proto_service_message_cpp_generation_and_sibling_refusal() -> Result<()> {
        use rush_action::ids::ArtifactKind;
        use rush_action::{ActionResult, Artifact as Input, Command, ExecutionInfo};
        use rush_rules::context::{
            ActionManager, BuildContext, NativeCompileMode, PendingCompileDeclaration,
            PendingCompileDeclarations,
        };
        use rush_rules::providers::DefaultInfo;
        use std::sync::Mutex;
        use std::time::Instant;

        struct ExecuteReal {
            actions: Mutex<Vec<Action>>,
            pending: Mutex<Vec<PendingCompileDeclaration>>,
        }
        #[async_trait]
        impl ActionManager for ExecuteReal {
            async fn execute(&self, action: Action) -> Result<ActionResult> {
                assert_eq!(
                    action.id,
                    Action::compute_id(&action.inputs, &action.command, &action.env)?
                );
                let started = Instant::now();
                eprintln!(
                    "real proto fixture command: cwd={:?} executable={:?} argv={:?}",
                    action.command.working_dir, action.command.executable, action.command.args
                );
                let output = tokio::time::timeout(
                    action.command.execution_info.timeout,
                    tokio::process::Command::new(&action.command.executable)
                        .args(&action.command.args)
                        .envs(&action.env)
                        .current_dir(
                            action
                                .command
                                .working_dir
                                .as_ref()
                                .context("fixture needs an explicit cwd")?,
                        )
                        .kill_on_drop(true)
                        .output(),
                )
                .await??;
                let mut records = Vec::new();
                if output.status.success() {
                    for artifact in &action.outputs {
                        let Input::Source(path) = artifact else {
                            anyhow::bail!("unexpected derived fixture output")
                        };
                        records.push(rush_action::artifact_value::record_output(path, None)?);
                    }
                }
                self.actions.lock().unwrap().push(action.clone());
                Ok(ActionResult {
                    action_id: action.id,
                    exit_code: output.status.code().unwrap_or(-1),
                    stdout: output.stdout,
                    stderr: output.stderr,
                    outputs: action.outputs,
                    output_records: records,
                    duration: started.elapsed(),
                    cached: false,
                })
            }
            async fn execute_pending_compile(
                &self,
                action: Action,
                pending: PendingCompileDeclaration,
            ) -> Result<ActionResult> {
                assert!(action
                    .inputs
                    .contains(&Input::Source(pending.source_path().into())));
                assert!(action
                    .outputs
                    .contains(&Input::Source(pending.object_path().into())));
                assert!(!pending.required_producers().is_empty());
                self.pending.lock().unwrap().push(pending);
                self.execute(action).await
            }
        }

        let protoc = PathBuf::from(
            std::env::var_os("RUSH_PROTOC").context("set RUSH_PROTOC to actual matching protoc")?,
        );
        anyhow::ensure!(
            protoc.is_absolute() && protoc.is_file(),
            "real absolute protoc is required"
        );
        anyhow::ensure!(
            std::path::Path::new("/usr/include/google/protobuf/message.h").is_file(),
            "matching protobuf development headers are required"
        );
        let temp = TempDir::new()?;
        let root = temp.path().join("source");
        let output = temp.path().join("output");
        std::fs::create_dir_all(root.join("pkg"))?;
        std::fs::create_dir_all(&output)?;
        std::fs::write(
            root.join("pkg/message.proto"),
            r#"syntax = "proto3";
package role_fixture;
message Request { string text = 1; }
message Reply { string text = 1; }
service Echo { rpc Call(Request) returns (Reply); }
"#,
        )?;
        let executor = Arc::new(ExecuteReal {
            actions: Mutex::new(Vec::new()),
            pending: Mutex::new(Vec::new()),
        });
        let tool_path = output.join("declared-protoc");
        let resolver = StubIncludeResolver::new(root.clone());
        let toolchain = Toolchain::default();
        let mut registry = ArtifactRegistry::new();
        let tool;
        {
            let mut ctx = AnalysisContext::new(
                TargetId(1),
                ConfigId::DEFAULT,
                "",
                Some("tools"),
                &output,
                &mut registry,
                &resolver,
                &toolchain,
                &root,
                &[],
            );
            let source = ctx.register_source_artifact(&protoc, ArtifactKind::Source);
            tool = ctx.register_generated_artifact(&tool_path, ArtifactKind::Executable, None);
            ctx.declare_action("GenRule", vec![source], vec![tool])?;
        }
        let command = Command {
            executable: "/bin/cp".into(),
            args: vec![utf8_path(&protoc)?, utf8_path(&tool_path)?],
            working_dir: Some(root.clone()),
            execution_info: ExecutionInfo::default(),
        };
        let inputs = vec![Input::Source(protoc)];
        let env = HashMap::new();
        let staged = executor
            .execute(Action {
                id: Action::compute_id(&inputs, &command, &env)?,
                command,
                env,
                inputs,
                outputs: vec![Input::Source(tool_path.clone())],
                mnemonic: "GenRule".into(),
            })
            .await?;
        anyhow::ensure!(
            staged.exit_code == 0,
            "real protoc staging failed: {}",
            String::from_utf8_lossy(&staged.stderr)
        );
        let tool_providers = vec![(
            "@tools//:protoc".into(),
            Arc::new(ProviderSet::empty().with_default_info(DefaultInfo { files: vec![tool] })),
        )];
        // This fixture consumes the selected compiler's installed protobuf
        // headers. Declare that actual runtime input without moving the SDK's
        // /usr/include ahead of libstdc++ via an explicit -isystem override.
        let runtime_header = {
            let mut ctx = AnalysisContext::new(
                TargetId(3),
                ConfigId::DEFAULT,
                "",
                None,
                &output,
                &mut registry,
                &resolver,
                &toolchain,
                &root,
                &[],
            );
            ctx.register_source_artifact(
                std::path::Path::new("/usr/include/google/protobuf/message.h"),
                ArtifactKind::Header,
            )
        };
        let runtime = Arc::new(
            ProviderSet::empty().with_cc_info(CcInfo::from_direct_and_deps(
                DirectCcInfo {
                    headers: vec![runtime_header],
                    ..Default::default()
                },
                Vec::<Arc<CcInfo>>::new(),
            )),
        );
        // The macro projection is tested in rush-starlark. Its ordinary CC
        // declaration has no has_services/create_grpc_library plugin flag.
        let attrs = HashMap::from([
            ("name".into(), Value::String("messages_proto".into())),
            (
                "srcs".into(),
                Value::List(vec![Value::String("message.proto".into())]),
            ),
            ("protoc".into(), Value::String("@@tools//:protoc".into())),
            (
                "cc_libs".into(),
                Value::List(vec![Value::String(":actual_runtime".into())]),
            ),
        ]);
        let ordinary = TfProtoLibraryRule.parse_attributes(&attrs)?;
        assert!(ordinary.configuration_error().is_none());
        let mut pending = PendingCompileDeclarations::default();
        {
            let mut ctx = AnalysisContext::new(
                TargetId(2),
                ConfigId::DEFAULT,
                "pkg",
                None,
                &output,
                &mut registry,
                &resolver,
                &toolchain,
                &root,
                &[],
            )
            .with_labeled_exec_dep_providers(&tool_providers)
            .with_native_execution_root(Some(&root))
            .with_native_compile_discovery(NativeCompileMode::UncachedDiagnosticV1, &mut pending);
            ordinary.validate_analysis_dependencies(&ctx)?;
            ordinary.analyze(&mut ctx, &[runtime.clone()]);
            assert!(
                ctx.native_namespace_analysis_error().is_none(),
                "{:?}",
                ctx.native_namespace_analysis_error()
            );
        }
        assert_eq!(pending.iter().count(), 1);
        let context = BuildContext::new(root, output.clone())
            .with_package("pkg".into())
            .with_toolchain(toolchain)
            .with_dependencies(
                HashMap::new(),
                vec![
                    (
                        "@tools//:protoc".into(),
                        tool_providers[0].1.as_ref().clone(),
                    ),
                    ("//pkg:actual_runtime".into(), runtime.as_ref().clone()),
                ],
            )
            .with_artifact_registry(Arc::new(tokio::sync::Mutex::new(registry)))
            .with_native_execution_root(Some(temp.path().join("source")))
            .with_native_compile_discovery(
                NativeCompileMode::UncachedDiagnosticV1,
                Arc::new(pending),
            )
            .with_action_manager(executor.clone());
        let specs = ordinary.build_legacy_command_specs(&context).await;
        assert_eq!(
            specs
                .iter()
                .filter(|(_, s)| s.mnemonic == "ProtocCompile")
                .count(),
            1
        );
        let compile_spec = specs
            .iter()
            .find(|(_, spec)| spec.mnemonic == "CppCompile")
            .context("real fixture must declare the generated C++ compile")?
            .1
            .clone();
        assert!(compile_spec
            .declared_inputs
            .contains(&PathBuf::from("/usr/include/google/protobuf/message.h")));
        assert!(!compile_spec
            .argv
            .iter()
            .any(|arg| arg == "-I/usr/include" || arg == "-isystem/usr/include"));
        ordinary.build(&context).await?;
        let header = output.join("genfiles/pkg/message.pb.h");
        let generated = output.join("genfiles/pkg/message.pb.cc");
        let archive = output.join("pkg/messages_proto/libmessages_proto.a");
        assert!(std::fs::read_to_string(&header)?.contains("class Request"));
        assert!(generated.is_file() && archive.is_file());
        assert!(!output.join("genfiles/pkg/message.grpc.pb.h").exists());
        assert!(!output.join("genfiles/pkg/message.grpc.pb.cc").exists());
        let actions = executor.actions.lock().unwrap();
        for (name, count) in [
            ("GenRule", 1),
            ("ProtocCompile", 1),
            ("CppCompile", 1),
            ("CcArchive", 1),
        ] {
            assert_eq!(
                actions.iter().filter(|a| a.mnemonic == name).count(),
                count,
                "{name}"
            );
        }
        let compile = actions
            .iter()
            .find(|action| action.mnemonic == "CppCompile")
            .unwrap();
        assert_eq!(compile.command.executable, compile_spec.executable);
        assert_eq!(compile.command.args, compile_spec.argv);
        assert_eq!(compile.command.working_dir, compile_spec.cwd);
        assert!(compile.inputs.contains(&Input::Source(PathBuf::from(
            "/usr/include/google/protobuf/message.h"
        ))));
        let codegen = actions
            .iter()
            .find(|a| a.mnemonic == "ProtocCompile")
            .unwrap();
        assert_eq!(codegen.command.executable, tool_path);
        assert!(codegen
            .command
            .args
            .iter()
            .any(|arg| arg.starts_with("--cpp_out=")));
        assert!(!codegen
            .command
            .args
            .iter()
            .any(|arg| arg.starts_with("--grpc_") || arg.starts_with("--plugin")));
        let count = actions.len();
        drop(actions);
        let original = std::fs::read(&generated)?;
        // Actual plugin selections still refuse in front of the same manager,
        // despite already-present pb outputs from a successful message build.
        for extra in [
            ("_rush_tf_proto_role", Value::String("grpc_sibling".into())),
            ("cc_grpc_version", Value::String("1.0".into())),
        ] {
            let mut unsupported = attrs.clone();
            unsupported.insert(extra.0.into(), extra.1);
            let rule = TfProtoLibraryRule.parse_attributes(&unsupported)?;
            assert!(rule.build(&context).await.is_err());
            assert_eq!(executor.actions.lock().unwrap().len(), count);
            assert_eq!(std::fs::read(&generated)?, original);
        }
        Ok(())
    }

    #[test]
    fn analysis_compile_uses_dependency_proto_include_root() {
        let temp = TempDir::new().unwrap();
        let workspace_root = temp.path().join("workspace");
        let output_dir = temp.path().join("out");
        let external_genfiles = output_dir
            .join("external/repo-local_tsl/genfiles");
        let mut registry = ArtifactRegistry::new();
        let external_genfiles_id = registry.intern_path(&external_genfiles);
        let dep_cc = CcInfo::from_direct_and_deps(
            DirectCcInfo {
                // Repeat the path to pin command-side deduplication as well as
                // propagation from the dependency provider.
                include_dirs: vec![external_genfiles_id, external_genfiles_id],
                ..DirectCcInfo::default()
            },
            Vec::<Arc<CcInfo>>::new(),
        );
        let dep_provider = Arc::new(ProviderSet::empty().with_cc_info(dep_cc));
        let dep_providers = vec![dep_provider];
        let resolver = StubIncludeResolver::new(workspace_root.clone());
        let toolchain = Toolchain::default();
        let target = TargetId(1);
        let rule = make_tf_proto("consumer", false, false, false);

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
            let _ = rule.analyze(&mut context, &dep_providers);
        }

        let compile_id = *registry
            .actions_of(target)
            .iter()
            .find(|action_id| {
                let action = registry.declared_action(**action_id);
                registry.strings().str(action.mnemonic) == "CppCompile"
            })
            .expect("tf_proto_library must declare CppCompile");
        let executable = registry
            .executable_action(compile_id)
            .expect("non-grpc compile must be scheduler executable");
        let argv: Vec<&str> = executable
            .argv
            .iter()
            .map(|id| registry.strings().str(*id))
            .collect();
        let expected = format!("-I{}", external_genfiles.display());
        assert_eq!(
            argv.iter().filter(|arg| **arg == expected).count(),
            1,
            "dependency genfiles root missing or duplicated: {argv:?}"
        );
    }

    /// Phase 5.C.4 commit 28 helper: write a temp executable
    /// file and return its path. Cleanup is best-effort —
    /// /tmp gets swept eventually anyway.
    fn temp_executable_file(content: &str, suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("rush-grpc-test-{}-{}", std::process::id(), suffix));
        std::fs::write(&path, content).expect("write temp file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        path
    }

    fn temp_non_executable_file(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "rush-grpc-test-noexec-{}-{}",
            std::process::id(),
            suffix
        ));
        std::fs::write(&path, "not executable").expect("write temp file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o644);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        path
    }

    #[test]
    fn is_executable_file_accepts_executable() {
        let p = temp_executable_file("#!/bin/sh\necho ok\n", "exec_yes");
        assert!(is_executable_file(&p));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn is_executable_file_rejects_non_executable() {
        let p = temp_non_executable_file("exec_no");
        // On unix the file exists but mode is 0o644 — no exec
        // bit. is_executable_file should reject.
        #[cfg(unix)]
        assert!(!is_executable_file(&p));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn is_executable_file_rejects_missing() {
        let p = std::env::temp_dir().join("rush-grpc-definitely-does-not-exist-zzz");
        let _ = std::fs::remove_file(&p);
        assert!(!is_executable_file(&p));
    }

    // grpc_cpp_plugin_searched_paths smoke: the returned list
    // includes the env-var status line and the two distro
    // fallbacks. PATH segments depend on the host; assert on
    // the bookend entries.

    #[test]
    fn searched_paths_includes_env_var_status_and_distro_fallbacks() {
        // Save and clear env to get deterministic "(not set)"
        // line. Restore at end.
        let prev = std::env::var("RUSH_GRPC_CPP_PLUGIN").ok();
        std::env::remove_var("RUSH_GRPC_CPP_PLUGIN");

        let entries = grpc_cpp_plugin_searched_paths();
        assert!(
            entries.iter().any(|e| e.contains("RUSH_GRPC_CPP_PLUGIN (not set)")),
            "entries={:?}",
            entries
        );
        assert!(
            entries.iter().any(|e| e == "/usr/bin/grpc_cpp_plugin"),
            "entries={:?}",
            entries
        );
        assert!(
            entries.iter().any(|e| e == "/usr/local/bin/grpc_cpp_plugin"),
            "entries={:?}",
            entries
        );

        if let Some(v) = prev {
            std::env::set_var("RUSH_GRPC_CPP_PLUGIN", v);
        }
    }

    #[test]
    fn searched_paths_lists_env_value_when_set() {
        let prev = std::env::var("RUSH_GRPC_CPP_PLUGIN").ok();
        std::env::set_var("RUSH_GRPC_CPP_PLUGIN", "/some/explicit/path");

        let entries = grpc_cpp_plugin_searched_paths();
        assert!(
            entries
                .iter()
                .any(|e| e == "RUSH_GRPC_CPP_PLUGIN=/some/explicit/path"),
            "entries={:?}",
            entries
        );

        match prev {
            Some(v) => std::env::set_var("RUSH_GRPC_CPP_PLUGIN", v),
            None => std::env::remove_var("RUSH_GRPC_CPP_PLUGIN"),
        }
    }

    // Phase 5.C.4 commit 31a: tf_proto_library shape predicate
    // tests. The override on scheduler_shape_unsupported gates
    // grpc-bearing targets out of scheduler dispatch — Pro's
    // explicit "do not over-certify rule families" correction.

    fn make_tf_proto(
        name: &str,
        has_services: bool,
        create_grpc_library: bool,
        use_grpc_namespace: bool,
    ) -> TfProtoLibraryInstance {
        TfProtoLibraryInstance {
            name: name.to_string(),
            srcs: vec!["foo.proto".to_string()],
            protodeps: Vec::new(),
            cc_api_version: 2,
            make_default_target_header_only: false,
            has_services,
            create_grpc_library,
            grpc_version_requested: false,
            use_grpc_namespace,
            role: TfProtoRole::Default,
            cc_libs: Vec::new(),
            protoc: None,
            generation_owner: None,
        }
    }

    #[test]
    fn shape_unsupported_returns_none_for_non_grpc() {
        // Plain non-grpc tf_proto_library — has_services and
        // create_grpc_library both false. Commit 22 verified
        // this shape end-to-end; predicate returns None
        // (= "no shape problem, dispatch is fine").
        let rule = make_tf_proto("p", false, false, false);
        assert!(rule.scheduler_shape_unsupported().is_none());
    }

    #[test]
    fn scheduler_required_tools_non_grpc_only_protoc() {
        // Phase 5.C.4 commit 55: rule-side tool declaration
        // replaces the orchestrator's hardcoded map. Non-grpc
        // tf_proto_library declares only protoc.
        let rule = make_tf_proto("p", false, false, false);
        let tools = rule.scheduler_required_tools();
        assert_eq!(tools, vec![rush_rules::RequiredTool::Protoc]);
    }

    #[test]
    fn scheduler_required_tools_grpc_includes_grpc_plugin() {
        // has_services=True → declares Protoc + GrpcCppPlugin.
        let rule = make_tf_proto("p", true, false, false);
        let tools = rule.scheduler_required_tools();
        assert_eq!(
            tools,
            vec![
                rush_rules::RequiredTool::Protoc,
                rush_rules::RequiredTool::GrpcCppPlugin
            ]
        );
    }

    #[test]
    fn scheduler_required_tools_create_grpc_library_includes_grpc_plugin() {
        // create_grpc_library=True is the other grpc trigger.
        let rule = make_tf_proto("p", false, true, false);
        let tools = rule.scheduler_required_tools();
        assert!(tools.contains(&rush_rules::RequiredTool::Protoc));
        assert!(tools.contains(&rush_rules::RequiredTool::GrpcCppPlugin));
    }

    #[test]
    fn parsed_service_attributes_share_the_scheduler_tool_and_shape_policy() {
        for has_services in [false, true] {
            for create_grpc_library in [false, true] {
                let attributes = HashMap::from([
                    ("name".to_string(), Value::String("messages".to_string())),
                    ("has_services".to_string(), Value::Bool(has_services)),
                    ("create_grpc_library".to_string(), Value::Bool(create_grpc_library)),
                    ("use_grpc_namespace".to_string(), Value::Bool(true)),
                ]);
                let rule = TfProtoLibraryRule.parse_attributes(&attributes).unwrap();
                let requested = has_services || create_grpc_library;
                assert_eq!(rule.scheduler_shape_unsupported().is_some(), requested);
                assert_eq!(
                    rule.scheduler_required_tools().contains(&rush_rules::RequiredTool::GrpcCppPlugin),
                    requested,
                );
            }
        }
    }

    #[test]
    fn shape_unsupported_fires_when_has_services_set() {
        // has_services = True triggers the grpc codegen path
        // (.grpc.pb.h/.cc + protoc plugin invocation). Not yet
        // verified — must return Some(reason) so the
        // eligibility predicate routes to UnsupportedShape.
        let rule = make_tf_proto("p", true, false, false);
        let reason = rule.scheduler_shape_unsupported();
        assert!(reason.is_some(), "has_services=True must gate dispatch");
        let s = reason.unwrap();
        assert!(
            s.contains("grpc"),
            "reason should mention grpc, got {:?}",
            s
        );
    }

    #[test]
    fn shape_unsupported_fires_when_create_grpc_library_set() {
        // create_grpc_library = True is the other grpc trigger
        // (TF's build_config.bzl uses it as the explicit
        // "this proto needs cc_grpc_library" signal).
        let rule = make_tf_proto("p", false, true, false);
        let reason = rule.scheduler_shape_unsupported();
        assert!(
            reason.is_some(),
            "create_grpc_library=True must gate dispatch"
        );
    }

    #[test]
    fn shape_unsupported_fires_when_both_grpc_flags_set() {
        // Belt-and-suspenders: both flags set is also gated.
        // (The override checks `||` so this case is implied,
        // but the test pins the behavior.)
        let rule = make_tf_proto("p", true, true, true);
        let reason = rule.scheduler_shape_unsupported();
        assert!(reason.is_some());
    }

    // Phase 5.C.4 commit 31b: grpc_plugin_runtime_failure_diagnostic
    // tests. Pro's spec: detect the ld.so / "cannot open shared
    // object" failure mode; surface a structured fix-list.

    #[test]
    fn diagnostic_fires_on_cannot_open_shared_object() {
        let stderr = "/path/to/grpc_cpp_plugin: error while loading shared \
                      libraries: libprotoc.so.31.1.0: cannot open shared \
                      object file: No such file or directory";
        let p = std::path::Path::new("/path/to/grpc_cpp_plugin");
        let diag = grpc_plugin_runtime_failure_diagnostic(p, stderr);
        assert!(diag.is_some(), "ld.so error pattern should match");
        let msg = diag.unwrap();
        assert!(msg.contains("grpc_cpp_plugin found but could not execute"));
        assert!(msg.contains("/path/to/grpc_cpp_plugin"));
        assert!(msg.contains("libprotoc.so.31.1.0"));
        assert!(msg.contains("protobuf-compiler-grpc"));
        assert!(msg.contains("RUSH_GRPC_CPP_PLUGIN"));
    }

    #[test]
    fn diagnostic_fires_on_libprotoc_alone() {
        // Some ld.so flavors phrase it differently; the plugin
        // name + ".so." substring is the secondary heuristic.
        let stderr = "Error: libprotoc.so.32.0.0 not found in the search path";
        let p = std::path::Path::new("/usr/local/bin/grpc_cpp_plugin");
        assert!(grpc_plugin_runtime_failure_diagnostic(p, stderr).is_some());
    }

    #[test]
    fn diagnostic_fires_on_while_loading_shared_libraries() {
        let stderr = "while loading shared libraries: something.so";
        let p = std::path::Path::new("/x/grpc_cpp_plugin");
        assert!(grpc_plugin_runtime_failure_diagnostic(p, stderr).is_some());
    }

    #[test]
    fn diagnostic_returns_none_for_unrelated_protoc_failures() {
        // Real protoc failures (syntax errors, missing imports,
        // unknown options) should NOT trip the ld.so detector
        // — fall through to the raw protoc stderr.
        let stderr = "foo.proto:5:1: Expected top-level statement \
                      (e.g. \"message\").";
        let p = std::path::Path::new("/x/grpc_cpp_plugin");
        assert!(grpc_plugin_runtime_failure_diagnostic(p, stderr).is_none());
    }

    #[test]
    fn diagnostic_returns_none_for_empty_stderr() {
        let p = std::path::Path::new("/x/grpc_cpp_plugin");
        assert!(grpc_plugin_runtime_failure_diagnostic(p, "").is_none());
    }

    #[test]
    fn shape_unsupported_does_not_fire_for_use_grpc_namespace_alone() {
        // use_grpc_namespace controls namespace handling for
        // grpc-emitted services but doesn't ITSELF imply grpc
        // codegen. Pro's status doc framing: only has_services
        // and create_grpc_library trigger the grpc path. If
        // a target sets only use_grpc_namespace without one of
        // the other two, no .grpc.pb.{h,cc} are produced —
        // dispatch is safe.
        let rule = make_tf_proto("p", false, false, true);
        assert!(rule.scheduler_shape_unsupported().is_none());
    }
}
