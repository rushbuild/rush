//! Per-target analysis context for typed-provider construction.
//!
//! Phase 5.B.3.prep — types only, no `BuildRule` trait wiring yet.
//! Phase 5.B.3 (the migration) threads `&mut AnalysisContext`
//! through the rule's `providers()` (renamed `analyze()`) entry
//! and migrates one rule at a time.
//!
//! ## What this exists to fix
//!
//! ChatGPT review Q5 + Q6 of `docs/item-5-phase-b3-pre-review.md`:
//!
//! - Q5 (`ConfigId`): every `ArtifactRegistry::intern_artifact`
//!   site needs a `ConfigId`. Hardcoding `ConfigId::DEFAULT` at
//!   call sites bakes in a multi-config audit later. Threading
//!   `ctx.config_id()` now means there's one place to change when
//!   real configurations land — the orchestrator side, not the
//!   rules.
//!
//! - Q6 (relative include resolution): the previous orchestrator
//!   `resolve_relative_include` lambda walked a *merged* CcInfo
//!   and rewrote include paths after rule analysis. That's exactly
//!   the orchestrator-side post-processing that ChatGPT flagged
//!   as the next memory-cliff failure mode (rebuilding transitive
//!   nested sets to swap PathBufs). Instead, a rule's `analyze()`
//!   resolves only its OWN direct `includes = [...]` attrs into
//!   absolute / exec-root-relative `PathId`s before it calls
//!   `CcInfo::from_direct_and_deps()`. Transitive dep
//!   `include_dirs` flow through unchanged — they were resolved
//!   when the dep was analyzed.
//!
//! ## What this does NOT do
//!
//! - No `BuildRule` trait change. That's part of 5.B.3.
//! - No orchestrator-side `IncludeResolver` impl. That's part of
//!   5.B.3.b (when the orchestrator constructs the
//!   `AnalysisContext` it hands to `cc_library` for the first
//!   real migration).
//! - No `register_action` / producer registry plumbing — that's
//!   phase 5.C0.

use rush_action::ids::{
    ArtifactId, ArtifactKey, ArtifactKind, ArtifactRegistry, ConfigId, DeclaredAction,
    DeclaredActionId, LinkerInputData, LinkerInputId, PathId, ProducerError, StringId, TargetId,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Resolves relative include attributes to absolute exec-root
/// paths for a rule's package. The orchestrator owns the
/// implementation (workspace map + external repo paths); rules
/// call it through this trait so they don't reach into
/// orchestrator state.
///
/// `package` is the rule's package path (e.g. `tensorflow/core`).
/// `workspace` is `None` for the main workspace, `Some("local_xla")`
/// for an external repo. `raw` is the verbatim BUILD-file value
/// (`"."`, `"include"`, `"../absolute/path"`, etc.).
///
/// Implementations should resolve to a path that's stable across
/// builds — exec-root-relative is preferred over absolute host
/// paths because absolute host paths poison action keys (cache
/// misses every time the build dir moves) and break
/// relocatability. Where the underlying file is in an external
/// workspace, callers may need an absolute path; the
/// implementation makes that policy choice.
pub trait IncludeResolver: Send + Sync {
    fn resolve(&self, raw: &Path, package: &str, workspace: Option<&str>) -> PathBuf;
}

/// Stub resolver for tests and bootstrap. Joins
/// `workspace_root.join(package).join(raw)`, treating `"."` as
/// the package directory itself. Production builds use the
/// orchestrator's resolver, which knows about external repos.
#[derive(Debug)]
pub struct StubIncludeResolver {
    pub workspace_root: PathBuf,
}

impl StubIncludeResolver {
    pub fn new(workspace_root: PathBuf) -> Self {
        Self { workspace_root }
    }
}

impl IncludeResolver for StubIncludeResolver {
    fn resolve(&self, raw: &Path, package: &str, _workspace: Option<&str>) -> PathBuf {
        if raw.is_absolute() {
            return raw.to_path_buf();
        }
        let pkg_dir = self.workspace_root.join(package);
        if raw == Path::new(".") {
            pkg_dir
        } else {
            pkg_dir.join(raw)
        }
    }
}

/// Per-target analysis context.
///
/// Holds borrowed handles to:
///   - the per-build `ArtifactRegistry` (for `intern_path` /
///     `intern_string` / `intern_artifact` / `intern_linker_input`)
///   - the orchestrator's `IncludeResolver`
/// Plus the analyzed target's identity (TargetId, ConfigId, label
/// package + workspace) so `resolve_package_include` knows where
/// `"."` resolves to.
///
/// Lifetime convention: borrowed for the duration of a single
/// `analyze()` call. The orchestrator constructs an
/// `AnalysisContext`, hands it to the rule's `analyze()`, and
/// drops it before moving to the next target. No interior
/// mutability — all mutation flows through `&mut self` on the
/// helper methods so borrow checking enforces "only one rule at
/// a time can register artifacts during its analysis."
pub struct AnalysisContext<'a> {
    native_compile_mode: crate::context::NativeCompileMode,
    pending_compile_declarations: Option<&'a mut crate::context::PendingCompileDeclarations>,
    target: TargetId,
    config_id: ConfigId,
    /// Rule's package (e.g. `tensorflow/core`).
    package: &'a str,
    /// Rule's workspace label (`None` = main workspace).
    workspace: Option<&'a str>,
    /// Per-repository output root directory (e.g. `<workspace>/rush-out` for
    /// the main repository or `<workspace>/rush-out/external/<repo>`).
    /// Used by `predicted_output_path` so rules can register
    /// generated artifacts at their actual on-disk locations,
    /// matching what `BuildContext::output_path` resolves to at
    /// build() time. Without this rules would register relative
    /// paths that consumers can't open.
    output_dir: &'a Path,
    registry: &'a mut ArtifactRegistry,
    include_resolver: &'a dyn IncludeResolver,
    /// Phase 5.C.4: toolchain for the build, borrowed by
    /// reference so analyze() can populate `ExecutableAction`
    /// payloads with the correct compiler / archiver / linker
    /// paths and flags. Mirrors what `BuildContext.toolchain`
    /// carries at build() time so analysis-side argv assembly
    /// agrees with execution-side argv assembly. Borrowed for
    /// the duration of the analyze() call; the orchestrator
    /// owns the Toolchain and clones it for BuildContext as
    /// needed.
    toolchain: &'a crate::context::Toolchain,
    /// Phase 5.C.4: workspace exec root. Used by analyze() to
    /// build CppCompile argv with `-I<workspace_root>` (Bazel's
    /// default include convention is package-rooted at the
    /// workspace) — mirrors what cc_library's compile_source
    /// adds to its argv at build time. Owned by the orchestrator
    /// and borrowed for the duration of analyze().
    workspace_root: &'a Path,
    /// Optional MAIN execution namespace; independent of this target's source
    /// repository root. Defaults to None for unchanged main-only callers.
    native_execution_root: Option<&'a Path>,
    /// Fatal admission failure for the source-only namespace profile. An
    /// incomplete action is insufficient: runtime may materialize its input.
    native_namespace_error: Option<String>,
    /// Phase 5.C.4: external workspace roots from the build's
    /// repository registration. Used by analyze() to add
    /// `-I<extra>` flags so cross-repo includes resolve (e.g.
    /// `#include "tsl/framework/device_type.h"` in a
    /// @local_xla target finds the file under @local_tsl's
    /// workspace root). Mirrors `BuildContext.extra_proto_paths`
    /// at build time.
    extra_proto_paths: &'a [PathBuf],
    /// Labeled dep providers in `node.dependencies` order, mirroring
    /// `BuildContext.dependency_providers_ordered`. Lets analyze() look
    /// up a SPECIFIC dependency's providers by label — needed so
    /// cc_library can expand a `filegroup` label appearing in `srcs`
    /// into that filegroup's files (via its `FilesInfo`) WITHOUT
    /// blindly collecting every dep's files (which would wrongly pull
    /// header-only filegroups / declared deps into the compile set).
    /// Defaults to an empty slice; the orchestrator attaches the real
    /// slice via `with_labeled_dep_providers`.
    labeled_dep_providers:
        &'a [(String, std::sync::Arc<crate::providers::ProviderSet>)],
}

impl<'a> AnalysisContext<'a> {
    pub fn new(
        target: TargetId,
        config_id: ConfigId,
        package: &'a str,
        workspace: Option<&'a str>,
        output_dir: &'a Path,
        registry: &'a mut ArtifactRegistry,
        include_resolver: &'a dyn IncludeResolver,
        toolchain: &'a crate::context::Toolchain,
        workspace_root: &'a Path,
        extra_proto_paths: &'a [PathBuf],
    ) -> Self {
        Self {
            native_compile_mode: crate::context::NativeCompileMode::Literal,
            pending_compile_declarations: None,
            target,
            config_id,
            package,
            workspace,
            output_dir,
            registry,
            include_resolver,
            toolchain,
            workspace_root,
            native_execution_root: None,
            native_namespace_error: None,
            extra_proto_paths,
            labeled_dep_providers: &[],
        }
    }

    /// Attach explicit per-command discovery metadata. Merely carrying the
    /// policy does not clear or bypass an existing fatal namespace error.
    pub fn with_native_compile_discovery(
        mut self,
        mode: crate::context::NativeCompileMode,
        declarations: &'a mut crate::context::PendingCompileDeclarations,
    ) -> Self {
        self.native_compile_mode = mode;
        self.pending_compile_declarations = Some(declarations);
        self
    }

    pub fn native_compile_mode(&self) -> crate::context::NativeCompileMode {
        self.native_compile_mode
    }

    /// Record an already-declared incomplete compile, without making it ready
    /// or schedulable. The diagnostic manager owns later execution.
    pub fn record_pending_compile(
        &mut self,
        action: rush_action::ids::DeclaredActionId,
        source: ArtifactId,
        object: ArtifactId,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.native_compile_mode == crate::context::NativeCompileMode::UncachedDiagnosticV1,
            "literal compile mode cannot record a pending discovery request"
        );
        let record = crate::context::PendingCompileDeclaration::from_registry(
            self.registry,
            action,
            self.target,
            self.config_id,
            source,
            object,
        )?;
        self.pending_compile_declarations
            .as_deref_mut()
            .ok_or_else(|| anyhow::anyhow!("pending compile declaration table is not attached"))?
            .record(record)
    }

    pub fn with_native_execution_root(mut self, root: Option<&'a Path>) -> Self {
        self.native_execution_root = root;
        self
    }

    pub fn native_execution_root(&self) -> Option<&Path> {
        self.native_execution_root
    }

    /// Preserve the first failed namespace proof for the orchestrator to reject
    /// before provider/snapshot adoption. This is independent of declaration
    /// completeness, which ordinary legacy genrules may legitimately require.
    pub fn fail_native_namespace_analysis(&mut self, error: impl Into<String>) {
        if self.native_namespace_error.is_none() {
            self.native_namespace_error = Some(error.into());
        }
    }

    pub fn native_namespace_analysis_error(&self) -> Option<&str> {
        self.native_namespace_error.as_deref()
    }

    /// Attach the labeled dep providers (in `node.dependencies` order).
    /// Builder so the many existing `AnalysisContext::new` call sites
    /// (tests included) compile unchanged; the orchestrator chains this
    /// for production analysis. The slice is borrowed for the analyze()
    /// call's lifetime.
    pub fn with_labeled_dep_providers(
        mut self,
        providers: &'a [(String, std::sync::Arc<crate::providers::ProviderSet>)],
    ) -> Self {
        self.labeled_dep_providers = providers;
        self
    }

    /// Borrow the labeled dep providers slice (in `node.dependencies`
    /// order). Lets `analyze()` build the same label→files view the
    /// legacy command-spec pass derives from
    /// `BuildContext.dependency_providers_ordered`.
    pub fn labeled_dep_providers(
        &self,
    ) -> &[(String, std::sync::Arc<crate::providers::ProviderSet>)] {
        self.labeled_dep_providers
    }

    /// Look up a specific dependency's providers by label, with
    /// repository-aware relative-label resolution. Exact canonical identity
    /// wins regardless of dependency order. Historical repository-mapping
    /// fallbacks remain available only when exactly one direct dependency
    /// matches, so `@r//p:x` cannot alias `@s//p:x`.
    pub fn get_dependency_provider(
        &self,
        label: &str,
    ) -> Option<&std::sync::Arc<crate::providers::ProviderSet>> {
        for (dep_label, providers) in self.labeled_dep_providers {
            if crate::providers::dep_label_matches_repository_exact(
                dep_label,
                label,
                self.package,
                self.workspace,
            ) {
                return Some(providers);
            }
        }

        let relative_suffix = label.strip_prefix(':').map(|target| {
            format!("//{}:{}", self.package, target)
        });
        let mut fallback = None;
        for (dep_label, providers) in self.labeled_dep_providers {
            if crate::providers::dep_label_matches(dep_label, label, self.package)
                || relative_suffix
                    .as_ref()
                    .is_some_and(|suffix| dep_label.ends_with(suffix))
            {
                if fallback.is_some() {
                    return None;
                }
                fallback = Some(providers);
            }
        }
        fallback
    }

    /// Phase 5.C.4: toolchain reference. Use to derive
    /// `ExecutableAction.executable` (e.g. via
    /// `ctx.intern_path(&ctx.toolchain().cxx)`) and to assemble
    /// argv with toolchain-provided cxxflags / cflags / ldflags.
    pub fn toolchain(&self) -> &crate::context::Toolchain {
        self.toolchain
    }

    /// Phase 5.C.4: workspace exec root. CppCompile argv
    /// includes `-I<workspace_root>` so package-rooted includes
    /// (`#include "pkg/foo.h"`) resolve.
    pub fn workspace_root(&self) -> &Path {
        self.workspace_root
    }

    /// Phase 5.C.4: external workspace roots. CppCompile argv
    /// includes `-I<extra>` for each so cross-repo includes
    /// resolve (e.g. an @local_xla target consuming an @local_tsl
    /// header).
    pub fn extra_proto_paths(&self) -> &[PathBuf] {
        self.extra_proto_paths
    }

    /// Phase 5.C.4: attach an `ExecutableAction` payload to a
    /// declared action so the scheduler can run it directly
    /// instead of falling through to legacy `build()`. Thin
    /// pass-through to the registry; lives on `AnalysisContext`
    /// so rules don't reach into `ctx.registry_mut()` for this
    /// common operation.
    pub fn set_executable_action(
        &mut self,
        id: rush_action::ids::DeclaredActionId,
        exec: rush_action::ids::ExecutableAction,
    ) {
        self.registry.set_executable_action(id, exec);
    }

    /// Identity of the target being analyzed.
    pub fn target(&self) -> TargetId {
        self.target
    }

    /// Configuration the target is being analyzed under. Always
    /// `ConfigId::DEFAULT` for now (rush is single-config); when
    /// multi-config lands, only the orchestrator changes — rules
    /// just call `ctx.config_id()` and stay correct.
    pub fn config_id(&self) -> ConfigId {
        self.config_id
    }

    pub fn package(&self) -> &str {
        self.package
    }

    pub fn workspace(&self) -> Option<&str> {
        self.workspace
    }

    /// Read-only access to the registry (for tests, census, and
    /// downstream consumers that need to look up `ArtifactData`
    /// by `ArtifactId`).
    pub fn registry(&self) -> &ArtifactRegistry {
        self.registry
    }

    /// Per-build output root. Rules use `predicted_output_path`
    /// (preferred) to register generated artifacts at the right
    /// absolute path; this exposes the raw root for any rule
    /// that needs it directly.
    pub fn output_dir(&self) -> &Path {
        self.output_dir
    }

    /// Compute the absolute path a generated artifact will live
    /// at when build() runs — `<output_dir>/<rel>`. Rules
    /// register generated artifacts with this path so consumers
    /// can later resolve the ArtifactId back to a file the
    /// linker can actually open. Mirrors
    /// `BuildContext::output_path` so the analysis-time and
    /// build-time paths agree.
    pub fn predicted_output_path(&self, rel: &Path) -> PathBuf {
        if rel.is_absolute() {
            rel.to_path_buf()
        } else {
            self.output_dir.join(rel)
        }
    }

    /// Package-qualified output path RELATIVE to `output_dir`, as a string.
    ///
    /// Phase 5.C.5a output-path collision fix (Option A): rules register
    /// generated artifacts with a path derived from `self.name` only, so two
    /// targets of the same name in different packages (e.g.
    /// `//tensorflow/core/ir:Dialect` vs `//tensorflow/core/ir/types:Dialect`)
    /// collided on `rush-out/<name>/...` and the second's `declare_action`
    /// hit `DuplicateProducer`. Qualify by the rule's package so each target
    /// gets a distinct path. MUST agree with
    /// `BuildContext::target_output_rel` (build/legacy passes) so PASS-A and
    /// PASS-B key the same `primary_output_path` and the verifier matches.
    pub fn target_output_rel(&self, name: &str, filename: &str) -> String {
        if self.package.is_empty() {
            format!("{}/{}", name, filename)
        } else {
            format!("{}/{}/{}", self.package, name, filename)
        }
    }

    /// Intern an exec-path. Cheap; same-path-twice gives the same
    /// `PathId`. Use when the rule has an absolute or
    /// already-resolved path; for relative `includes = [...]`
    /// attrs, prefer [`resolve_package_include`].
    pub fn intern_path(&mut self, p: &Path) -> PathId {
        self.registry.intern_path(p)
    }

    /// Intern a short string (define, link flag, env value).
    pub fn intern_string(&mut self, s: &str) -> StringId {
        self.registry.intern_string(s)
    }

    /// Resolve a relative include attribute against this rule's
    /// package, then intern the result. This is the canonical
    /// path for `includes = ["."]`, `includes = ["../foo"]`,
    /// `strip_include_prefix` resolution: rules call this BEFORE
    /// returning their CcCompilationContext, so what ends up in
    /// the provider DAG is already-resolved exec-root-relative
    /// paths. No orchestrator post-merge rewriting.
    pub fn resolve_package_include(&mut self, raw: &Path) -> PathId {
        let resolved = self.include_resolver.resolve(raw, self.package, self.workspace);
        self.registry.intern_path(&resolved)
    }

    /// Register a source artifact (a file that exists in the
    /// repo before the build). Sets `is_source = true` per phase
    /// 5.C0 P3 so producer validation knows this artifact has no
    /// producer by design.
    pub fn register_source_artifact(&mut self, path: &Path, kind: ArtifactKind) -> ArtifactId {
        let exec_path = self.registry.intern_path(path);
        let key = ArtifactKey {
            config: self.config_id,
            exec_path,
        };
        self.registry
            .intern_artifact(key, kind, /*is_source*/ true, self.target, None)
    }

    /// Register a generated artifact (produced by an action).
    /// Sets `is_source = false` per P3. Relative paths resolve
    /// against `output_dir` via `predicted_output_path` so the
    /// registry stores the same absolute path that build() would
    /// create at runtime.
    pub fn register_generated_artifact(
        &mut self,
        path: &Path,
        kind: ArtifactKind,
        producer: Option<rush_action::ActionId>,
    ) -> ArtifactId {
        let abs = self.predicted_output_path(path);
        let exec_path = self.registry.intern_path(&abs);
        let key = ArtifactKey {
            config: self.config_id,
            exec_path,
        };
        self.registry
            .intern_artifact(key, kind, /*is_source*/ false, self.target, producer)
    }

    /// Register a linker input. Returns the arena handle the rule
    /// places into its `DirectCcInfo.linker_inputs`.
    pub fn register_linker_input(
        &mut self,
        artifact: ArtifactId,
        alwayslink: bool,
    ) -> LinkerInputId {
        self.registry.intern_linker_input(LinkerInputData {
            owner: self.target,
            artifact,
            alwayslink,
            extras: None,
        })
    }

    /// Same as `register_linker_input` but with rich extras
    /// (linkstamps, per-input link flags). The `Arc` lets the
    /// caller share extras across multiple LinkerInputs that
    /// derive from the same source target.
    pub fn register_linker_input_with_extras(
        &mut self,
        artifact: ArtifactId,
        alwayslink: bool,
        extras: Arc<rush_action::ids::LinkerInputExtras>,
    ) -> LinkerInputId {
        self.registry.intern_linker_input(LinkerInputData {
            owner: self.target,
            artifact,
            alwayslink,
            extras: Some(extras),
        })
    }

    /// Phase 5.C0: declare a Complete action. The action's
    /// outputs gain `owner_action_declared` slots in the
    /// registry; duplicate producers are rejected at registration.
    /// Use this for rules whose declared inputs/outputs match
    /// what the scheduler would need to run them.
    pub fn declare_action(
        &mut self,
        mnemonic: &str,
        inputs: Vec<ArtifactId>,
        outputs: Vec<ArtifactId>,
    ) -> Result<DeclaredActionId, ProducerError> {
        let mnemonic_id = self.registry.intern_string(mnemonic);
        self.registry.declare_action(DeclaredAction {
            owner_target: self.target,
            mnemonic: mnemonic_id,
            inputs: inputs.into_boxed_slice(),
            outputs: outputs.into_boxed_slice(),
            status: rush_action::ids::ActionDeclarationStatus::Complete,
        })
    }

    /// Phase 5.C0 P4: declare an Incomplete action. Same shape
    /// as `declare_action` but flags the action as
    /// not-schedulable — phase 5.C's scheduler will fall back to
    /// legacy `build()` execution for these. Use when the
    /// rule's declared inputs/outputs are a subset of what the
    /// scheduler would actually need (typically: cross-target
    /// inputs the typed-provider path doesn't yet expose).
    /// `reason` should be a short static string explaining
    /// what's missing.
    pub fn declare_incomplete_action(
        &mut self,
        mnemonic: &str,
        inputs: Vec<ArtifactId>,
        outputs: Vec<ArtifactId>,
        reason: &'static str,
    ) -> Result<DeclaredActionId, ProducerError> {
        let mnemonic_id = self.registry.intern_string(mnemonic);
        self.registry.declare_action(DeclaredAction {
            owner_target: self.target,
            mnemonic: mnemonic_id,
            inputs: inputs.into_boxed_slice(),
            outputs: outputs.into_boxed_slice(),
            status: rush_action::ids::ActionDeclarationStatus::Incomplete { reason },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_OUTPUT_DIR: &str = "/tmp/rush-test-out";

    fn ctx_in<'a>(
        registry: &'a mut ArtifactRegistry,
        resolver: &'a dyn IncludeResolver,
        toolchain: &'a crate::context::Toolchain,
        workspace_root: &'a Path,
        extra_proto_paths: &'a [PathBuf],
        package: &'a str,
        workspace: Option<&'a str>,
    ) -> AnalysisContext<'a> {
        AnalysisContext::new(
            TargetId(1),
            ConfigId::DEFAULT,
            package,
            workspace,
            Path::new(TEST_OUTPUT_DIR),
            registry,
            resolver,
            toolchain,
            workspace_root,
            extra_proto_paths,
        )
    }

    #[test]
    fn config_id_threads_through() {
        let mut reg = ArtifactRegistry::new();
        let resolver = StubIncludeResolver::new(PathBuf::from("/ws"));
        let tc = crate::context::Toolchain::default();
        let ws = PathBuf::from("/ws");
        let extra: Vec<PathBuf> = Vec::new();
        let ctx = ctx_in(&mut reg, &resolver, &tc, &ws, &extra,"pkg", None);
        assert_eq!(ctx.config_id(), ConfigId::DEFAULT);
        assert_eq!(ctx.target(), TargetId(1));
        assert_eq!(ctx.package(), "pkg");
        assert!(ctx.workspace().is_none());
    }

    #[test]
    fn dependency_provider_lookup_prefers_exact_repository_identity() {
        let mut reg = ArtifactRegistry::new();
        let resolver = StubIncludeResolver::new(PathBuf::from("/ws"));
        let tc = crate::context::Toolchain::default();
        let ws = PathBuf::from("/ws");
        let extra: Vec<PathBuf> = Vec::new();
        let provider = |path: &str| {
            std::sync::Arc::new(
                crate::providers::ProviderSet::empty().with_files(
                    crate::providers::FilesInfo::new(vec![PathBuf::from(path)]),
                ),
            )
        };
        let external_s = provider("/out/external/repo-s/genfiles/p/gen.h");
        let external_r = provider("/out/external/repo-r/genfiles/p/gen.h");
        let root_r = provider("/out/external/repo-r/root");
        let main = provider("/out/genfiles/p/gen.h");
        let labeled = vec![
            ("@s//p:gen.h".to_string(), external_s),
            ("@r//p:gen.h".to_string(), external_r),
            ("@r//:r".to_string(), root_r),
            ("//p:gen.h".to_string(), main),
        ];
        let ctx = ctx_in(&mut reg, &resolver, &tc, &ws, &extra, "p", None)
            .with_labeled_dep_providers(&labeled);

        let file_for = |label: &str| {
            ctx.get_dependency_provider(label)
                .and_then(|providers| providers.get_files())
                .and_then(|files| files.files.first())
                .cloned()
        };
        assert_eq!(file_for(":gen.h"), Some(PathBuf::from("/out/genfiles/p/gen.h")));
        assert_eq!(
            file_for("@r//p:gen.h"),
            Some(PathBuf::from("/out/external/repo-r/genfiles/p/gen.h"))
        );
        assert_eq!(
            file_for("@s//p:gen.h"),
            Some(PathBuf::from("/out/external/repo-s/genfiles/p/gen.h"))
        );
        assert_eq!(
            file_for("@r"),
            Some(PathBuf::from("/out/external/repo-r/root"))
        );
        assert_eq!(
            file_for("@@r"),
            Some(PathBuf::from("/out/external/repo-r/root"))
        );

        let external_ctx = ctx_in(
            &mut reg,
            &resolver,
            &tc,
            &ws,
            &extra,
            "p",
            Some("r"),
        )
        .with_labeled_dep_providers(&labeled);
        let external_file_for = |label: &str| {
            external_ctx
                .get_dependency_provider(label)
                .and_then(|providers| providers.get_files())
                .and_then(|files| files.files.first())
                .cloned()
        };
        assert_eq!(
            external_file_for(":gen.h"),
            Some(PathBuf::from("/out/external/repo-r/genfiles/p/gen.h"))
        );
        assert_eq!(
            external_file_for("//p:gen.h"),
            Some(PathBuf::from("/out/external/repo-r/genfiles/p/gen.h"))
        );
        assert_eq!(
            external_file_for("@//p:gen.h"),
            Some(PathBuf::from("/out/genfiles/p/gen.h"))
        );
    }

    #[test]
    fn resolve_package_include_dot_resolves_to_package_dir() {
        // The TF idiom: includes = ["."] inside @local_xla//xla/mlir_hlo
        // must resolve to <ws_root>/xla/mlir_hlo, not the consumer's
        // CWD. With a stub resolver pinned at /ws, we expect
        // /ws/xla/mlir_hlo.
        let mut reg = ArtifactRegistry::new();
        let resolver = StubIncludeResolver::new(PathBuf::from("/ws"));
        let tc = crate::context::Toolchain::default();
        let ws = PathBuf::from("/ws");
        let extra: Vec<PathBuf> = Vec::new();
        let mut ctx = ctx_in(&mut reg, &resolver, &tc, &ws, &extra,"xla/mlir_hlo", Some("local_xla"));
        let id = ctx.resolve_package_include(Path::new("."));
        assert_eq!(reg.paths().path(id), Path::new("/ws/xla/mlir_hlo"));
    }

    #[test]
    fn resolve_package_include_relative_joins_under_package() {
        let mut reg = ArtifactRegistry::new();
        let resolver = StubIncludeResolver::new(PathBuf::from("/ws"));
        let tc = crate::context::Toolchain::default();
        let ws = PathBuf::from("/ws");
        let extra: Vec<PathBuf> = Vec::new();
        let mut ctx = ctx_in(&mut reg, &resolver, &tc, &ws, &extra,"tensorflow/core", None);
        let id = ctx.resolve_package_include(Path::new("public"));
        assert_eq!(reg.paths().path(id), Path::new("/ws/tensorflow/core/public"));
    }

    #[test]
    fn resolve_package_include_absolute_passes_through() {
        let mut reg = ArtifactRegistry::new();
        let resolver = StubIncludeResolver::new(PathBuf::from("/ws"));
        let tc = crate::context::Toolchain::default();
        let ws = PathBuf::from("/ws");
        let extra: Vec<PathBuf> = Vec::new();
        let mut ctx = ctx_in(&mut reg, &resolver, &tc, &ws, &extra,"pkg", None);
        let id = ctx.resolve_package_include(Path::new("/abs/path"));
        assert_eq!(reg.paths().path(id), Path::new("/abs/path"));
    }

    #[test]
    fn intern_string_and_path_dedup_through_context() {
        let mut reg = ArtifactRegistry::new();
        let resolver = StubIncludeResolver::new(PathBuf::from("/ws"));
        let tc = crate::context::Toolchain::default();
        let ws = PathBuf::from("/ws");
        let extra: Vec<PathBuf> = Vec::new();
        let mut ctx = ctx_in(&mut reg, &resolver, &tc, &ws, &extra,"pkg", None);
        let s1 = ctx.intern_string("FOO=1");
        let s2 = ctx.intern_string("FOO=1");
        assert_eq!(s1, s2);
        let p1 = ctx.intern_path(Path::new("foo/bar.h"));
        let p2 = ctx.intern_path(Path::new("foo/bar.h"));
        assert_eq!(p1, p2);
    }

    #[test]
    fn register_source_artifact_owner_action_is_none() {
        let mut reg = ArtifactRegistry::new();
        let resolver = StubIncludeResolver::new(PathBuf::from("/ws"));
        let tc = crate::context::Toolchain::default();
        let ws = PathBuf::from("/ws");
        let extra: Vec<PathBuf> = Vec::new();
        let aid = {
            let mut ctx = ctx_in(&mut reg, &resolver, &tc, &ws, &extra,"pkg", None);
            ctx.register_source_artifact(Path::new("src/main.cc"), ArtifactKind::Source)
        };
        let data = reg.artifact(aid);
        assert_eq!(data.kind, ArtifactKind::Source);
        assert!(data.owner_action.is_none());
        assert_eq!(data.owner_target, TargetId(1));
    }

    #[test]
    fn register_generated_artifact_carries_owner_action() {
        let mut reg = ArtifactRegistry::new();
        let resolver = StubIncludeResolver::new(PathBuf::from("/ws"));
        let tc = crate::context::Toolchain::default();
        let ws = PathBuf::from("/ws");
        let extra: Vec<PathBuf> = Vec::new();
        let action = rush_action::ActionId::new(blake3::hash(b"some action"));
        let aid = {
            let mut ctx = ctx_in(&mut reg, &resolver, &tc, &ws, &extra,"pkg", None);
            ctx.register_generated_artifact(
                Path::new("gen/foo.pb.h"),
                ArtifactKind::Header,
                Some(action.clone()),
            )
        };
        let data = reg.artifact(aid);
        assert_eq!(data.kind, ArtifactKind::Header);
        assert_eq!(
            data.owner_action.as_ref().unwrap().hash(),
            action.hash()
        );
    }

    #[test]
    fn register_linker_input_records_owner_target() {
        // Q4: owner: TargetId lands at the registration site,
        // not the rule call site. Verify the analysis context
        // sets owner = self.target automatically.
        let mut reg = ArtifactRegistry::new();
        let resolver = StubIncludeResolver::new(PathBuf::from("/ws"));
        let tc = crate::context::Toolchain::default();
        let ws = PathBuf::from("/ws");
        let extra: Vec<PathBuf> = Vec::new();
        let (lib_aid, lid) = {
            let mut ctx = ctx_in(&mut reg, &resolver, &tc, &ws, &extra,"pkg", None);
            let aid =
                ctx.register_source_artifact(Path::new("lib/libfoo.a"), ArtifactKind::StaticLibrary);
            let lid = ctx.register_linker_input(aid, /*alwayslink*/ false);
            (aid, lid)
        };
        assert_eq!(reg.linker_input_count(), 1);
        let data = reg.linker_input(lid);
        assert_eq!(data.owner, TargetId(1));
        assert_eq!(data.artifact, lib_aid);
        assert!(!data.alwayslink);
    }

    #[test]
    fn register_artifact_uses_context_config_id() {
        // Q5: artifact registration must use ctx.config_id(),
        // not a hardcoded ConfigId::DEFAULT. Build a context
        // with a non-default config to verify the value flows
        // through to ArtifactKey, which then collision-distinguishes
        // from a same-path registration under ConfigId::DEFAULT.
        let mut reg = ArtifactRegistry::new();
        let resolver = StubIncludeResolver::new(PathBuf::from("/ws"));
        let tc = crate::context::Toolchain::default();
        let ws = PathBuf::from("/ws");
        let extra: Vec<PathBuf> = Vec::new();
        let aid = {
            let mut ctx = AnalysisContext::new(
                TargetId(1),
                ConfigId(42),
                "pkg",
                None,
                Path::new(TEST_OUTPUT_DIR),
                &mut reg,
                &resolver,
                &tc,
                &ws,
                &extra,
            );
            assert_eq!(ctx.config_id(), ConfigId(42));
            ctx.register_source_artifact(Path::new("a.cc"), ArtifactKind::Source)
        };
        // Same path under a different ConfigId must produce a
        // distinct ArtifactId — collision-safety from 5.A.5.
        let exec_path = reg.artifact(aid).exec_path;
        let aid_default = reg.intern_artifact(
            ArtifactKey {
                config: ConfigId::DEFAULT,
                exec_path,
            },
            ArtifactKind::Source,
            true,
            TargetId(1),
            None,
        );
        assert_ne!(aid, aid_default);
    }
}
