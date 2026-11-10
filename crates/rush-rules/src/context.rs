use crate::nested_set::{NestedSet, NestedSetIter};
use crate::Artifact;
use anyhow::Result;
use async_trait::async_trait;
use rush_action::{Action, ActionResult};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Interface for executing actions
#[async_trait]
pub trait ActionManager: Send + Sync {
    /// Execute an action
    async fn execute(&self, action: Action) -> Result<ActionResult>;
}

/// Toolchain information for compilation
#[derive(Debug, Clone)]
pub struct Toolchain {
    pub cc: PathBuf,
    pub cxx: PathBuf,
    pub ar: PathBuf,
    pub ld: PathBuf,
    pub cflags: Vec<String>,
    pub cxxflags: Vec<String>,
    pub ldflags: Vec<String>,
}

/// The C/C++ compiler rush should use, honoring `RUSH_CC` / `RUSH_CXX`.
///
/// `rules_cc` reads `RUSH_CXX` when it compiles, but this toolchain (which
/// tf_cc_binary and tf_cc_test compile AND link with) hardcoded `g++`. So
/// overriding the compiler produced a MIXED build: cc_library objects from
/// one compiler, tf_cc_binary objects and the final link from another —
/// the same two-resolutions-of-one-tool defect that made rush generate
/// protos with one protoc and compile them against another's headers.
impl Default for Toolchain {
    fn default() -> Self { Self::from_environment_lookup(|name| std::env::var_os(name)) }
}

impl Toolchain {
    /// Shared exact base driver selection for execution and immutable-client
    /// admission checks. Preserve the existing UTF-8 trim/fallback policy.
    pub fn from_environment_lookup(mut lookup: impl FnMut(&str) -> Option<std::ffi::OsString>) -> Self {
        let mut driver = |name, fallback: &str| lookup(name)
            .and_then(|value| value.into_string().ok()).map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()).map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(fallback));
        Self { cc:driver("RUSH_CC","gcc"),cxx:driver("RUSH_CXX","g++"),
            ar:PathBuf::from("ar"),ld:PathBuf::from("ld"),
            cflags:Vec::new(),cxxflags:Vec::new(),ldflags:Vec::new() }
    }

    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_compiler(mut self, cc: PathBuf, cxx: PathBuf) -> Self {
        self.cc = cc;
        self.cxx = cxx;
        self
    }

    pub fn with_flags(mut self, cflags: Vec<String>, cxxflags: Vec<String>) -> Self {
        self.cflags = cflags;
        self.cxxflags = cxxflags;
        self
    }
}

/// Build context passed to rule execution
pub struct BuildContext {
    /// Source root for the rule's owning repository.
    pub workspace_root: PathBuf,
    /// Main workspace root that owns shared caches and fetched toolchains.
    pub main_workspace_root: PathBuf,
    /// Rush-owned MAIN execution namespace for a requested external closure.
    /// Source repository identity stays in workspace_root. None preserves the
    /// existing main-only execution/REAPI contract.
    pub native_execution_root: Option<PathBuf>,
    pub output_dir: PathBuf,
    pub package_path: Option<PathBuf>,
    /// Canonical repository name for an external target; `None` for main.
    pub repository: Option<String>,
    pub toolchain: Arc<Toolchain>,

    /// Dependency artifacts keyed by canonical label string. Each
    /// entry is a depset DAG node — the dep's own DIRECT artifacts
    /// plus references to its deps' nested sets (patch set 2b). The
    /// orchestrator constructs each NestedSet once when a target
    /// finishes building; consumers walk via `iter` to enumerate
    /// transitive items with O(unique nodes) cost (Arc-identity
    /// dedup), instead of the prior per-cc_library
    /// `for dep ... for artifact in dep_artifacts { push.clone() }`
    /// pattern that allocated a fresh `Vec<Artifact>` of size
    /// O(transitive closure) for every target — a multi-GB cost at
    /// TF-rung-11 scale (RSS exploded from 4 GB → 57 GB across the
    /// final 270 events of the 2026-04-27 controlled run when
    /// aggregator targets started building). See
    /// docs/memory-pressure-analysis.md.
    pub dependency_artifacts: HashMap<String, Arc<NestedSet<Artifact>>>,
    pub dependency_providers: HashMap<String, crate::providers::ProviderSet>,
    /// Phase 5.C.5a provider-threading fix: the same dependency
    /// providers as `dependency_providers`, but in deterministic
    /// `node.dependencies` declaration order rather than randomized
    /// HashMap iteration order. PASS-A (`analyze`) consumes deps via
    /// an ordered `Vec<Arc<ProviderSet>>`; PASS-B
    /// (`build_legacy_command_specs`) and the legacy runtime
    /// (`compile_source`) must consume the SAME order so the emitted
    /// `-I` include sequence matches across all three. Iterating the
    /// HashMap instead produced an order-only verifier divergence on
    /// every target transiting the mutex/nsync cone (the include SET
    /// was identical; only the sequence differed). The HashMap is
    /// kept for direct `.get()` lookups by label.
    pub dependency_providers_ordered:
        Vec<(String, crate::providers::ProviderSet)>,
    pub action_manager: Option<Arc<dyn ActionManager>>,
    /// Additional proto import roots (roots of registered external workspaces),
    /// passed to protoc via additional --proto_path flags so cross-repo `.proto`
    /// imports (e.g. TF's `tsl/protobuf/test_log.proto`) resolve.
    pub extra_proto_paths: Vec<PathBuf>,
    /// Phase 5.B.3.d plumbing: Arc-shared handle to the per-build
    /// `ArtifactRegistry`. `None` for legacy/test code paths that
    /// construct BuildContext without an orchestrator. When `Some`,
    /// build()-time consumers acquire a brief lock to deref the
    /// `PathId` / `StringId` / `ArtifactId` / `LinkerInputId` values
    /// that 5.B.3.b/c-migrated rules placed into the typed
    /// `CcCompilationContext` / `CcLinkingContext` provider fields.
    /// Mutex (vs RwLock) is fine because the orchestrator's analysis
    /// loop is sequential and the lock is held only briefly per
    /// dereference.
    pub artifact_registry:
        Option<Arc<tokio::sync::Mutex<rush_action::ids::ArtifactRegistry>>>,

    /// Whether legacy `cc_library`-style build paths should materialize their
    /// static archive output. The orchestrator disables this for library
    /// targets that are only being built to satisfy provider/header/object
    /// propagation and have no downstream link action consuming their `.a`.
    pub materialize_static_libraries: bool,

    /// Dirty input paths from a pruned PASS-B run. Legacy rules may use this to
    /// skip keying unchanged sibling actions when the graph policy already says
    /// no material aggregate output (such as a static archive) is needed.
    pub dirty_paths: Vec<PathBuf>,
}

/// How a dependency label was resolved. See
/// [`BuildContext::resolve_dependency`].
///
/// The 2026-08-22 review's rule: **no invisible fallback**. Resolution
/// used to be an `Option` over a chain of five increasingly loose
/// strategies, so a lenient match was indistinguishable from an exact
/// one, and a total miss was indistinguishable from "this dep has no
/// artifacts". Two silent stale-build bugs came out of that ambiguity.
///
/// `Exact` and `CanonicalLabel` are Bazel-conformant spellings.
/// Everything below is a compatibility fallback some corpus currently
/// depends on; each is counted so the load-bearing ones can be
/// identified and closed one at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionKind {
    /// Matched a dependency key verbatim.
    Exact,
    /// `//pkg` / `@repo//pkg` shorthand, or a same-package `:target` /
    /// bare `target` reference. Bazel accepts these spellings.
    CanonicalLabel,
    /// Matched by package+target SUFFIX, ignoring the repository
    /// component. Divergent: two repositories may expose the same
    /// package path.
    LegacySuffixFallback,
    /// Matched on target NAME alone across direct deps, when exactly
    /// one candidate had that name. Divergent.
    LegacyTargetNameFallback,
}

impl ResolutionKind {
    pub fn is_exact(self) -> bool {
        matches!(self, ResolutionKind::Exact | ResolutionKind::CanonicalLabel)
    }
    /// The LOOSER of two kinds — a canonical spelling that then needed
    /// a legacy fallback is reported as the legacy one.
    pub fn min_conformance(self, other: ResolutionKind) -> ResolutionKind {
        use ResolutionKind::*;
        let rank = |k: ResolutionKind| match k {
            Exact => 0,
            CanonicalLabel => 1,
            LegacySuffixFallback => 2,
            LegacyTargetNameFallback => 3,
        };
        if rank(self) >= rank(other) { self } else { other }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ResolutionKind::Exact => "exact",
            ResolutionKind::CanonicalLabel => "canonical",
            ResolutionKind::LegacySuffixFallback => "legacy-suffix",
            ResolutionKind::LegacyTargetNameFallback => "legacy-target-name",
        }
    }
}

/// Process-wide counts of non-exact label resolutions, so the
/// compatibility fallbacks can be measured on a real corpus before any
/// of them is removed. Reported by `rush build` when
/// `RUSH_LABEL_FALLBACK_REPORT=1`.
pub mod label_fallback_counters {
    use std::sync::atomic::{AtomicU64, Ordering};
    pub static CANONICAL: AtomicU64 = AtomicU64::new(0);
    pub static SUFFIX: AtomicU64 = AtomicU64::new(0);
    pub static TARGET_NAME: AtomicU64 = AtomicU64::new(0);
    pub static AMBIGUOUS_MISS: AtomicU64 = AtomicU64::new(0);
    pub static MISS: AtomicU64 = AtomicU64::new(0);

    pub fn note(kind: super::ResolutionKind) {
        match kind {
            super::ResolutionKind::Exact => {}
            super::ResolutionKind::CanonicalLabel => {
                CANONICAL.fetch_add(1, Ordering::Relaxed);
            }
            super::ResolutionKind::LegacySuffixFallback => {
                SUFFIX.fetch_add(1, Ordering::Relaxed);
            }
            super::ResolutionKind::LegacyTargetNameFallback => {
                TARGET_NAME.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    pub fn note_ambiguous() { AMBIGUOUS_MISS.fetch_add(1, Ordering::Relaxed); }
    pub fn note_miss() { MISS.fetch_add(1, Ordering::Relaxed); }

    /// `(canonical, suffix, target_name, ambiguous, miss)`
    pub fn snapshot() -> (u64, u64, u64, u64, u64) {
        (
            CANONICAL.load(Ordering::Relaxed),
            SUFFIX.load(Ordering::Relaxed),
            TARGET_NAME.load(Ordering::Relaxed),
            AMBIGUOUS_MISS.load(Ordering::Relaxed),
            MISS.load(Ordering::Relaxed),
        )
    }

    /// One line, or None when every resolution was exact.
    pub fn summary() -> Option<String> {
        let (c, s, t, a, m) = snapshot();
        if c == 0 && s == 0 && t == 0 && a == 0 && m == 0 {
            return None;
        }
        Some(format!(
            "🏷️  label resolution: {} canonical, {} legacy-suffix, {} \
             legacy-target-name, {} ambiguous-miss, {} miss",
            c, s, t, a, m
        ))
    }
}

impl BuildContext {
    pub fn new(workspace_root: PathBuf, output_dir: PathBuf) -> Self {
        Self {
            main_workspace_root: workspace_root.clone(),
            native_execution_root: None,
            workspace_root,
            output_dir,
            package_path: None,
            repository: None,
            toolchain: Arc::new(Toolchain::default()),

            dependency_artifacts: HashMap::new(),
            dependency_providers: HashMap::new(),
            dependency_providers_ordered: Vec::new(),
            action_manager: None,
            extra_proto_paths: Vec::new(),
            artifact_registry: None,
            materialize_static_libraries: true,
            dirty_paths: Vec::new(),
        }
    }

    /// Package-qualified output directory for a target.
    ///
    /// Phase 5.C.5a (output-path collision fix, Option A from
    /// docs/analysis/output-path-collision-audit.md): rush historically
    /// derived a target's output dir from `self.name` ONLY
    /// (`output_dir.join(name)`), so two targets with the same name in
    /// different packages — e.g. `//tensorflow/core/ir:Dialect` and
    /// `//tensorflow/core/ir/types:Dialect` — collided on
    /// `rush-out/Dialect/`. The second target's `declare_action` then hit
    /// `ArtifactRegistry` `DuplicateProducer` (single-owner), was silently
    /// swallowed, and the verifier reported a legacy-only divergence. Qualify
    /// by the package path (bazel-faithful: bazel-bin is package-rooted) so
    /// each target gets a distinct dir. `package_path = None` (external
    /// header-only stubs that emit no real artifacts) keeps the bare-name
    /// layout. The orchestrator scopes `output_dir` itself by repository
    /// before constructing this context, so `package_path` only needs to
    /// disambiguate targets within that repository.
    pub fn target_output_dir(&self, name: &str) -> PathBuf {
        match &self.package_path {
            Some(pkg) if !pkg.as_os_str().is_empty() => {
                self.output_dir.join(pkg).join(name)
            }
            _ => self.output_dir.join(name),
        }
    }

    /// Package-qualified output path RELATIVE to `output_dir`, as a string —
    /// for `register_generated_artifact` call sites that register
    /// rel-to-output-base paths (analyze PASS-A). Must agree with
    /// [`Self::target_output_dir`] so PASS-A and PASS-B (legacy specs) key
    /// the same `primary_output_path` and the verifier matches.
    pub fn target_output_rel(&self, name: &str, filename: &str) -> String {
        match &self.package_path {
            Some(pkg) if !pkg.as_os_str().is_empty() => {
                format!("{}/{}/{}", pkg.display(), name, filename)
            }
            _ => format!("{}/{}", name, filename),
        }
    }

    /// Attach an artifact registry handle (typically the orchestrator's
    /// per-build `Arc<Mutex<ArtifactRegistry>>`). Build()-time consumers
    /// acquire a lock to deref typed IDs.
    pub fn with_artifact_registry(
        mut self,
        registry: Arc<tokio::sync::Mutex<rush_action::ids::ArtifactRegistry>>,
    ) -> Self {
        self.artifact_registry = Some(registry);
        self
    }

    pub fn with_extra_proto_paths(mut self, paths: Vec<PathBuf>) -> Self {
        self.extra_proto_paths = paths;
        self
    }

    pub fn with_action_manager(mut self, manager: Arc<dyn ActionManager>) -> Self {
        self.action_manager = Some(manager);
        self
    }

    pub fn with_materialize_static_libraries(mut self, materialize: bool) -> Self {
        self.materialize_static_libraries = materialize;
        self
    }

    pub fn materialize_static_libraries(&self) -> bool {
        self.materialize_static_libraries
    }

    pub fn with_dirty_paths(mut self, dirty_paths: Vec<PathBuf>) -> Self {
        self.dirty_paths = dirty_paths
            .into_iter()
            .map(|p| {
                if p.is_absolute() {
                    p
                } else {
                    self.workspace_root.join(p)
                }
            })
            .collect();
        self
    }

    pub fn dirty_paths(&self) -> &[PathBuf] {
        &self.dirty_paths
    }

    pub fn path_is_dirty(&self, path: &Path) -> bool {
        let normalized = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace_root.join(path)
        };
        self.dirty_paths.iter().any(|p| p == &normalized)
    }

    pub fn with_package(mut self, package_path: PathBuf) -> Self {
        self.package_path = Some(package_path);
        self
    }

    pub fn with_repository(mut self, repository: Option<String>) -> Self {
        self.repository = repository;
        self
    }

    pub fn with_native_execution_root(mut self, root: Option<PathBuf>) -> Self {
        self.native_execution_root = root;
        self
    }

    pub fn native_execution_root(&self) -> Option<&Path> {
        self.native_execution_root.as_deref()
    }

    pub fn with_main_workspace_root(mut self, main_workspace_root: PathBuf) -> Self {
        self.main_workspace_root = main_workspace_root;
        self
    }

    pub fn with_toolchain(mut self, toolchain: Toolchain) -> Self {
        self.toolchain = Arc::new(toolchain);
        self
    }

    /// Phase 5.C.5a: `providers` is now an ORDERED list in
    /// `node.dependencies` declaration order (was a HashMap). We keep
    /// both representations: the ordered Vec drives include-dir
    /// collection in PASS-B / runtime so the emitted `-I` sequence
    /// matches PASS-A's ordered-Vec walk, and the derived HashMap
    /// preserves the existing `.get(label)` / `.values()` lookups.
    pub fn with_dependencies(
        mut self,
        deps: HashMap<String, Arc<NestedSet<Artifact>>>,
        providers: Vec<(String, crate::providers::ProviderSet)>,
    ) -> Self {
        self.dependency_artifacts = deps;
        self.dependency_providers =
            providers.iter().cloned().collect();
        self.dependency_providers_ordered = providers;
        self
    }

    /// Look up the dep's NestedSet by label, with relative-label
    /// resolution (`:foo` resolves against the current package, then
    /// a suffix scan handles external-workspace edges keyed as
    /// `@repo//<pkg>:<target>`).
    /// Bazel absolute-label shorthand: `//pkg/path` == `//pkg/path:path`,
    /// `@repo//pkg` == `@repo//pkg:pkg`, and `@repo` == `@repo//:repo`.
    /// Mirrors `dep_label_matches` for rule-side context lookups.
    fn canonicalize_absolute_label_shorthand(dep: &str) -> Option<String> {
        if let Some(canonical) = crate::providers::canonical_bare_repository_label(dep) {
            return Some(canonical);
        }
        if !(dep.starts_with("//") || dep.starts_with('@')) || dep.contains(':') {
            return None;
        }
        let pkg_part = dep.split("//").nth(1)?;
        let last = pkg_part.rsplit('/').next()?;
        if last.is_empty() {
            return None;
        }
        Some(format!("{}:{}", dep, last))
    }

    /// Resolve a dependency label to its artifact set AND say how it
    /// resolved. Every non-exact path is counted; see
    /// [`ResolutionKind`].
    pub fn resolve_dependency(
        &self,
        dep: &str,
    ) -> Option<(ResolutionKind, &Arc<NestedSet<Artifact>>)> {
        let out = self.resolve_dependency_inner(dep);
        match &out {
            Some((kind, _)) => label_fallback_counters::note(*kind),
            None => label_fallback_counters::note_miss(),
        }
        out
    }

    fn resolve_dependency_inner(
        &self,
        dep: &str,
    ) -> Option<(ResolutionKind, &Arc<NestedSet<Artifact>>)> {
        if let Some(set) = self.dependency_artifacts.get(dep) {
            return Some((ResolutionKind::Exact, set));
        }
        // Absolute-label shorthands recurse through their explicit-target form
        // so the remaining fallbacks all apply.
        if let Some(canon) = Self::canonicalize_absolute_label_shorthand(dep) {
            return self
                .resolve_dependency_inner(&canon)
                .map(|(k, set)| (k.min_conformance(ResolutionKind::CanonicalLabel), set));
        }
        // Normalize a same-package target reference to its absolute label.
        // Bazel accepts both `:target` and the bare `target` form within a
        // package (e.g. a genrule's `tools = ["gen_x"]` naming a sibling
        // py_binary). `dependency_artifacts` is keyed by the absolute
        // `//pkg:target`, so without this a bare tool label misses and the
        // genrule's `$(location <tool>)` is left unexpanded.
        let same_pkg_target = if let Some(t) = dep.strip_prefix(':') {
            Some(t)
        } else if !dep.is_empty()
            && !dep.contains('/')
            && !dep.contains(':')
            && !dep.starts_with('@')
        {
            Some(dep)
        } else {
            None
        };
        if let (Some(target), Some(package)) = (same_pkg_target, self.package_path.as_ref()) {
            let absolute_label = format!("//{}:{}", package.display(), target);
            if let Some(set) = self.dependency_artifacts.get(&absolute_label) {
                return Some((ResolutionKind::CanonicalLabel, set));
            }
            let pkg_str = package.display().to_string();
            let suffix = format!("//{}:{}", pkg_str, target);
            for (key, set) in &self.dependency_artifacts {
                if key.ends_with(&suffix) {
                    return Some((ResolutionKind::LegacySuffixFallback, set));
                }
            }
        }

        // Repository mappings can rewrite a dependency's canonical package
        // while the consuming rule retains its apparent label, e.g.
        // `//third_party/implib_so:make_stub` becomes
        // `@implib_so//:make_stub`. This map contains only the current
        // target's direct deps, so accept a target-name match when it is
        // unique; ambiguity deliberately remains a miss.
        let target = dep.rsplit_once(':').map(|(_, target)| target);
        if let Some(target) = target {
            let mut matched = None;
            for (key, set) in &self.dependency_artifacts {
                if key.rsplit_once(':').map(|(_, name)| name) == Some(target) {
                    if matched.is_some() {
                        label_fallback_counters::note_ambiguous();
                        return None;
                    }
                    matched = Some(set);
                }
            }
            if let Some(set) = matched {
                return Some((ResolutionKind::LegacyTargetNameFallback, set));
            }
        }
        None
    }

    fn lookup_dependency_set(&self, dep: &str) -> Option<&Arc<NestedSet<Artifact>>> {
        self.resolve_dependency(dep).map(|(_, set)| set)
    }

    /// Direct artifacts the dep produced (NOT transitive). The dep's
    /// own `build()` return: the `.a` it built, the `.o` files from
    /// its compile, the `.h` headers it owns. Use when the consumer
    /// cares about a specific dep's outputs (e.g. "did MY direct dep
    /// emit the .pb.h I'm about to include?"). For a transitive walk
    /// over the full DAG, call [`iter_transitive_artifacts`].
    ///
    /// The naming is deliberately distinct from
    /// [`iter_transitive_artifacts`] / [`transitive_artifacts`] so
    /// future contributors don't accidentally rebuild the O(N²)
    /// flattening this rename specifically prevents — see
    /// docs/memory-pressure-followup.md §2.
    pub fn direct_artifacts(&self, dep: &str) -> Option<&[Artifact]> {
        self.lookup_dependency_set(dep).map(|s| s.direct())
    }

    /// Walk the dep's transitive artifacts as an iterator. Each
    /// reachable NestedSet node is visited once (Arc-identity dedup),
    /// so a popular shared dep appears just once even if reached
    /// through multiple DAG paths. Use only at link / aggregation
    /// time. Calling this from a normal cc_library `build()` is the
    /// regression we explicitly guard against — see patch set 2b
    /// notes.
    pub fn iter_transitive_artifacts(&self, dep: &str) -> Option<NestedSetIter<Artifact>> {
        self.lookup_dependency_set(dep).map(|s| s.iter())
    }

    /// Raw NestedSet handle for forwarding (alias-shaped rules) or
    /// nesting in another consumer's depset. Cheap (Arc clone).
    pub fn transitive_artifacts(&self, dep: &str) -> Option<&Arc<NestedSet<Artifact>>> {
        self.lookup_dependency_set(dep)
    }

    // Compatibility shims that keep the old names compiling while we
    // transition all call sites in this commit. Marked `#[deprecated]`
    // so future work routes through the explicit direct/transitive
    // names. Remove when the in-tree call sites have all been
    // updated (most are renamed below).
    #[deprecated(note = "use direct_artifacts() — see context.rs::direct_artifacts")]
    pub fn get_dependency_artifacts(&self, dep: &str) -> Option<&[Artifact]> {
        self.direct_artifacts(dep)
    }
    #[deprecated(note = "use iter_transitive_artifacts() or transitive_artifacts()")]
    pub fn iter_dependency_artifacts(&self, dep: &str) -> Option<NestedSetIter<Artifact>> {
        self.iter_transitive_artifacts(dep)
    }
    #[deprecated(note = "use transitive_artifacts()")]
    pub fn get_dependency_nested_set(&self, dep: &str) -> Option<&Arc<NestedSet<Artifact>>> {
        self.transitive_artifacts(dep)
    }

    /// Get providers for a specific dependency
    pub fn get_dependency_providers(
        &self,
        dep: &str,
    ) -> Option<&crate::providers::ProviderSet> {
        // Try exact match first
        if let Some(providers) = self.dependency_providers.get(dep) {
            return Some(providers);
        }

        // Resolve absolute-label shorthands through their explicit-target form.
        if let Some(canon) = Self::canonicalize_absolute_label_shorthand(dep) {
            return self.get_dependency_providers(&canon);
        }

        // Same two-step relative-label resolution as
        // get_dependency_artifacts above.
        if let Some(target) = dep.strip_prefix(':') {
            let package = self
                .package_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            let absolute_label = format!("//{}:{}", package, target);
            if let Some(providers) = self.dependency_providers.get(&absolute_label) {
                return Some(providers);
            }
            let suffix = format!("//{}:{}", package, target);
            for (key, provs) in &self.dependency_providers_ordered {
                if key.ends_with(&suffix) {
                    return Some(provs);
                }
            }
        }

        // #39(A) collection-gap fix: fall back to the SAME ordered
        // `dep_label_matches()` walk that `get_dependency_files()` uses, so a
        // root/dep named in a different-but-equivalent label form still
        // resolves its ProviderSet. The exact `.get()` above misses when the
        // requested `dep` and the stored key differ only by canonical form
        // (e.g. `@local_xla//xla:xla_data_proto_cc_impl` vs the canonicalized
        // key, or `//tensorflow/core/ir:Dialect` vs an `@org_tensorflow//`-
        // prefixed key). cc_shared_library's Phase-2 collection
        // (rules_cc.rs) calls `get_dependency_providers(target)` per direct
        // root/dep and SKIPS the target's entire transitive
        // `linking.linker_inputs` subtree on `None` — so a single missed
        // lookup drops every archive that direct dep would have contributed
        // to the link (built-but-MISSING_FROM_PARAMS; #39 collection gap,
        // Codex 019e88db). This mirrors `get_dependency_files()` exactly so
        // provider and files lookups resolve labels identically.
        let package = self
            .package_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        for (dep_label, providers) in &self.dependency_providers_ordered {
            if crate::providers::dep_label_matches(dep_label, dep, &package) {
                return Some(providers);
            }
        }

        None
    }

    /// Resolve a `filegroup` (or other `FilesInfo`-providing) label to its
    /// files, for cc_library `srcs` filegroup expansion in the legacy
    /// command-spec (PASS-B) and runtime build() paths. Walks
    /// `dependency_providers_ordered` in `node.dependencies` order and
    /// resolves repository-exact identity first, then accepts the historical
    /// `dep_label_matches` fallback only when it is unique. The package string
    /// is derived the same way as the analyze pass's
    /// `AnalysisContext::package`, so PASS-A and PASS-B expand identically
    /// without aliasing same-tail labels from different repositories.
    pub fn get_dependency_files(
        &self,
        label: &str,
    ) -> Option<Vec<PathBuf>> {
        let package = self
            .package_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        for (dep_label, providers) in &self.dependency_providers_ordered {
            if crate::providers::dep_label_matches_repository_exact(
                dep_label,
                label,
                &package,
                self.repository.as_deref(),
            ) {
                return providers.get_files().map(|fi| fi.files.clone());
            }
        }

        let relative_suffix = label.strip_prefix(':').map(|target| {
            format!("//{}:{}", package, target)
        });
        let mut fallback = None;
        for (dep_label, providers) in &self.dependency_providers_ordered {
            if crate::providers::dep_label_matches(dep_label, label, &package)
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
        fallback.and_then(|providers| providers.get_files().map(|fi| fi.files.clone()))
    }

    /// Get output path for a file
    pub fn output_path(&self, relative_path: &str) -> PathBuf {
        self.output_dir.join(relative_path)
    }

    /// Get source path for a file (relative to package if set, else workspace)
    pub fn source_path(&self, relative_path: &str) -> PathBuf {
        if let Some(ref package) = self.package_path {
            self.workspace_root.join(package).join(relative_path)
        } else {
            self.workspace_root.join(relative_path)
        }
    }
    /// Execute an action using the configured ActionManager
    pub async fn execute_action(&self, mut action: Action) -> Result<ActionResult> {
        // Bzlmod-gated header-shadow fix (Codex 019eebc6 -iquote insight, applied
        // at the universal action funnel). rush hands compiles bare PACKAGE-DIR
        // -I per dep (e.g. -I<repo>/src/google/protobuf) alongside the real root
        // -I<repo>/src. A bare package dir on the angle-include path lets a system
        // `#include <endian.h>` resolve into protobuf's src/google/protobuf/endian.h
        // ("template with C linkage"). Demote any -I<dir> that is a strict subdir
        // of ANOTHER -I root (excluding workspace_root) to -iquote<dir>: it still
        // satisfies quoted "google/protobuf/X.h" includes (resolved via the parent
        // root anyway) but no longer shadows system <...> headers. Gated so the
        // certified TF/463 path (byte-sensitive) is untouched.
        if std::env::var("RUSH_ENABLE_BZLMOD").as_deref() == Ok("1") {
            // Drop an explicit -I/usr/include so the system include dir falls to
            // its DEFAULT (last) search position rather than preceding the from-
            // source external repo roots. Without this, a system `/usr/include/
            // absl/...` (Debian's `inline namespace debian9`) shadows the from-
            // source abseil-cpp headers, and the .o then references absl::debian9::*
            // symbols that the rush-built libabsl (plain absl::) does not provide →
            // "undefined reference to absl::debian9::InitializeLog()" at link. g++
            // still searches /usr/include by default, just after the -I roots, so
            // from-source absl/protobuf headers win. (Mirrors
            // demote_host_system_includes, which this compile path does not call.)
            let args_before = action.command.args.clone();
            action.command.args.retain(|a| a != "-I/usr/include");
            // Ensure each external repo ROOT is on the -I path. The builder for
            // this action (a cc_binary path) may emit only the deps' bare package
            // dirs (e.g. <abseil-cpp>/absl/base) and NOT the repo root, so a
            // cross-repo `#include "absl/base/options.h"` then falls through to the
            // default /usr/include search → SYSTEM absl (debian9 inline namespace)
            // instead of the from-source abseil-cpp (lts namespace), yielding
            // "undefined reference to absl::debian9::*" at link. Adding -I<root>
            // (any -I beats the default system path) makes from-source headers win.
            // (The subdir demotion below keeps these roots as -I.)
            if action.command.args.iter().any(|a| a.starts_with("-I")) {
                for root in &self.extra_proto_paths {
                    let flag = format!("-I{}", root.display());
                    if !action.command.args.contains(&flag) {
                        action.command.args.push(flag);
                    }
                }
            }
            demote_shadowing_subdir_includes(&mut action.command.args, &self.extra_proto_paths);
            // These mutations change the actual compile command. The action.id
            // (content hash → cache key) was computed by the builder BEFORE this
            // funnel, so without recomputing it the cache would restore a STALE
            // object compiled with the un-mutated args (e.g. one built with
            // -I/usr/include that picked up system absl's `debian9` namespace).
            // Recompute the id from the mutated command so the cache is keyed by
            // what actually runs.
            if action.command.args != args_before {
                action.id = Action::compute_id(
                    &action.inputs,
                    &action.command,
                    &action.env,
                )?;
            }
        }
        if std::env::var("RUSH_DUMP_CC_ARGV").is_ok()
            && action.command.args.iter().any(|a| a.contains("main_no_generators"))
        {
            eprintln!(
                "RUSH_CC_ARGV(exec) exe={} args={:?}",
                action.command.executable.display(),
                action.command.args
            );
        }
        if let Some(manager) = &self.action_manager {
            manager.execute(action).await
        } else {
            anyhow::bail!("No ActionManager configured in BuildContext")
        }
    }
}

/// Rewrite `-I<dir>` to `-iquote<dir>` for any include dir that is a PACKAGE
/// subdirectory of an external repo root (the `repo_roots` = extra_proto_paths /
/// materialized external workspace roots) — but keep the repo roots THEMSELVES as
/// `-I`. A bare package dir on the angle-include path lets a system `#include
/// <foo.h>` resolve into a repo package dir of the same basename (e.g. protobuf's
/// src/google/protobuf/endian.h shadowing the system <endian.h>); `-iquote` still
/// satisfies quoted "pkg/foo.h" (resolved via the repo root anyway) but removes
/// the shadow. The repo roots must stay `-I` so both quoted "absl/base/X.h" AND
/// angle <absl/base/X.h> resolve to the from-source headers (demoting a repo root
/// to -iquote breaks angle includes → the system copy wins, e.g. absl's debian9
/// inline-namespace vs the from-source lts namespace).
fn demote_shadowing_subdir_includes(args: &mut [String], repo_roots: &[std::path::PathBuf]) {
    let roots: Vec<String> = repo_roots
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    for a in args.iter_mut() {
        if let Some(dir) = a.strip_prefix("-I") {
            let dir = dir.to_string();
            // Keep the repo roots themselves as -I (angle includes need them).
            if roots.iter().any(|r| *r == dir) {
                continue;
            }
            // Demote a package dir (strict subdir of a repo root) to -iquote.
            let is_pkg_subdir = roots
                .iter()
                .any(|r| dir.starts_with(&format!("{}/", r)));
            if is_pkg_subdir {
                *a = format!("-iquote{}", dir);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_toolchain_creation() {
        let toolchain = Toolchain::new()
            .with_compiler(PathBuf::from("clang"), PathBuf::from("clang++"))
            .with_flags(vec!["-O2".to_string()], vec!["-std=c++17".to_string()]);

        assert_eq!(toolchain.cc, PathBuf::from("clang"));
        assert_eq!(toolchain.cxx, PathBuf::from("clang++"));
        assert_eq!(toolchain.cflags, vec!["-O2"]);
    }

    #[test]
    fn test_build_context() {
        let context = BuildContext::new(PathBuf::from("/workspace"), PathBuf::from("/output"));

        assert_eq!(context.workspace_root, PathBuf::from("/workspace"));
        assert_eq!(context.main_workspace_root, PathBuf::from("/workspace"));
        assert_eq!(context.output_dir, PathBuf::from("/output"));
        assert_eq!(
            context.source_path("src/main.cc"),
            PathBuf::from("/workspace/src/main.cc")
        );
        assert_eq!(
            context.output_path("obj/main.o"),
            PathBuf::from("/output/obj/main.o")
        );
    }

    #[test]
    fn external_context_keeps_main_workspace_identity() {
        let context = BuildContext::new(
            PathBuf::from("/workspace/.cache/rush/external/repo"),
            PathBuf::from("/output/external/repo"),
        )
        .with_repository(Some("repo".to_string()))
        .with_main_workspace_root(PathBuf::from("/workspace"));

        assert_eq!(
            context.workspace_root,
            PathBuf::from("/workspace/.cache/rush/external/repo")
        );
        assert_eq!(context.main_workspace_root, PathBuf::from("/workspace"));
    }

    #[test]
    fn bare_external_repository_lookup_uses_root_target() {
        let root_artifacts = NestedSet::new(Vec::new(), Vec::new());
        let root_provider = crate::providers::ProviderSet::empty().with_files(
            crate::providers::FilesInfo::new(vec![PathBuf::from("/out/root-repo")]),
        );
        let context = BuildContext::new(PathBuf::from("/ws"), PathBuf::from("/out"))
            .with_package(PathBuf::from("consumer"))
            .with_dependencies(
                HashMap::from([("@repo//:repo".to_string(), root_artifacts.clone())]),
                vec![("@repo//:repo".to_string(), root_provider)],
            );

        assert_eq!(
            BuildContext::canonicalize_absolute_label_shorthand("@repo"),
            Some("@repo//:repo".to_string())
        );
        assert!(Arc::ptr_eq(
            context.transitive_artifacts("@repo").unwrap(),
            &root_artifacts
        ));
        assert_eq!(
            context.get_dependency_files("@repo"),
            Some(vec![PathBuf::from("/out/root-repo")])
        );
        assert!(context.get_dependency_providers("@repo").is_some());
    }

    #[test]
    fn dependency_files_lookup_prefers_exact_repository_identity() {
        let provider = |path: &str| {
            crate::providers::ProviderSet::empty().with_files(
                crate::providers::FilesInfo::new(vec![PathBuf::from(path)]),
            )
        };
        let context = BuildContext::new(PathBuf::from("/ws"), PathBuf::from("/out"))
            .with_package(PathBuf::from("p"))
            .with_dependencies(
                HashMap::new(),
                vec![
                    (
                        "@s//p:gen.h".to_string(),
                        provider("/out/external/repo-s/genfiles/p/gen.h"),
                    ),
                    (
                        "@r//p:gen.h".to_string(),
                        provider("/out/external/repo-r/genfiles/p/gen.h"),
                    ),
                    (
                        "//p:gen.h".to_string(),
                        provider("/out/genfiles/p/gen.h"),
                    ),
                ],
            );

        assert_eq!(
            context.get_dependency_files(":gen.h"),
            Some(vec![PathBuf::from("/out/genfiles/p/gen.h")])
        );
        assert_eq!(
            context.get_dependency_files("@r//p:gen.h"),
            Some(vec![PathBuf::from(
                "/out/external/repo-r/genfiles/p/gen.h"
            )])
        );
        assert_eq!(
            context.get_dependency_files("@s//p:gen.h"),
            Some(vec![PathBuf::from(
                "/out/external/repo-s/genfiles/p/gen.h"
            )])
        );

        let external_context = BuildContext::new(
            PathBuf::from("/ws"),
            PathBuf::from("/out/external/repo-r"),
        )
        .with_package(PathBuf::from("p"))
        .with_repository(Some("r".to_string()))
        .with_dependencies(
            HashMap::new(),
            vec![
                (
                    "@s//p:gen.h".to_string(),
                    provider("/out/external/repo-s/genfiles/p/gen.h"),
                ),
                (
                    "@r//p:gen.h".to_string(),
                    provider("/out/external/repo-r/genfiles/p/gen.h"),
                ),
                (
                    "//p:gen.h".to_string(),
                    provider("/out/genfiles/p/gen.h"),
                ),
            ],
        );
        assert_eq!(
            external_context.get_dependency_files(":gen.h"),
            Some(vec![PathBuf::from(
                "/out/external/repo-r/genfiles/p/gen.h"
            )])
        );
        assert_eq!(
            external_context.get_dependency_files("//p:gen.h"),
            Some(vec![PathBuf::from(
                "/out/external/repo-r/genfiles/p/gen.h"
            )])
        );
        assert_eq!(
            external_context.get_dependency_files("@//p:gen.h"),
            Some(vec![PathBuf::from("/out/genfiles/p/gen.h")])
        );
    }

    // Phase 5.C.5a output-path collision fix (Option A). Two targets with
    // the SAME name in DIFFERENT packages must get DISTINCT output dirs so
    // their declared artifacts don't collide on the single-owner
    // ArtifactRegistry (the BD-1 / core/ir:Dialect-vs-core/ir/types:Dialect
    // class). Regression guard for the package-qualified path helpers.
    #[test]
    fn test_package_qualified_output_paths_disambiguate_same_name() {
        let mk = |pkg: &str| {
            let mut c = BuildContext::new(PathBuf::from("/ws"), PathBuf::from("/out"));
            c.package_path = Some(PathBuf::from(pkg));
            c
        };
        let ir = mk("tensorflow/core/ir");
        let ir_types = mk("tensorflow/core/ir/types");

        // Same target name "Dialect" in two packages → distinct dirs.
        assert_eq!(
            ir.target_output_dir("Dialect"),
            PathBuf::from("/out/tensorflow/core/ir/Dialect")
        );
        assert_eq!(
            ir_types.target_output_dir("Dialect"),
            PathBuf::from("/out/tensorflow/core/ir/types/Dialect")
        );
        assert_ne!(
            ir.target_output_dir("Dialect"),
            ir_types.target_output_dir("Dialect"),
            "same-name targets in different packages must not collide"
        );

        // The relative form (used by analyze register sites) agrees.
        assert_eq!(
            ir.target_output_rel("Dialect", "libDialect.a"),
            "tensorflow/core/ir/Dialect/libDialect.a"
        );
        assert_ne!(
            ir.target_output_rel("Dialect", "libDialect.a"),
            ir_types.target_output_rel("Dialect", "libDialect.a")
        );

        // package_path = None (external header-only stubs, no real outputs)
        // keeps the bare-name layout.
        let none = BuildContext::new(PathBuf::from("/ws"), PathBuf::from("/out"));
        assert_eq!(none.target_output_dir("foo"), PathBuf::from("/out/foo"));
        assert_eq!(none.target_output_rel("foo", "libfoo.a"), "foo/libfoo.a");
    }
}
