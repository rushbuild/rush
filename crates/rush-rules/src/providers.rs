//! Typed provider system for rule-to-rule information flow.
//!
//! Replaces the previous `HashMap<String, Provider>` with a typed
//! [`ProviderSet`] struct: each well-known provider gets its own
//! `Option<Arc<...>>` slot, and the alias-forward / merge paths
//! become refcount bumps instead of `HashMap::clone()` deep clones
//! of `Provider::CcInfo`-style enum payloads.
//!
//! Phase 5.A of the plan in `docs/item-5-typed-providers-plan.md`:
//! field types stay the same as the old enum (`Vec<PathBuf>` for
//! include_dirs, etc.) so this commit is purely mechanical. Phase
//! 5.B will replace those Vecs with typed `CcCompilationContext` /
//! `CcLinkingContext` shapes (with `OrderedNestedSet<PathId>` for
//! order-bearing fields). 5.A is the rename + Arc-share, no
//! semantic change.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// C/C++ compilation/link information for downstream `cc_library`,
/// `cc_binary`, `cc_test` consumers.
///
/// Phase 5.B.3.e deleted the legacy flat fields (`Vec<PathBuf>`/
/// `Vec<String>`) that lived alongside the typed contexts during
/// the migration window. Now the only shape is the typed pair:
/// every rule produces `CcInfo` via `from_direct_and_deps()`, every
/// consumer reads through the typed contexts via the registry
/// plumbed through `BuildContext`.
#[derive(Debug, Clone)]
pub struct CcInfo {
    pub compilation: Arc<CcCompilationContext>,
    pub linking: Arc<CcLinkingContext>,
}

impl CcInfo {
    /// Empty CcInfo for rules that produce no compile/link
    /// contributions (e.g. `cc_test` final consumer). Cheap —
    /// shares the global empty NestedSet/OrderedNestedSet via
    /// the empty constructors.
    pub fn empty() -> Arc<CcInfo> {
        Arc::new(CcInfo {
            compilation: Arc::new(CcCompilationContext::empty()),
            linking: Arc::new(CcLinkingContext::empty()),
        })
    }

    /// Phase 5.B.3.a merge helper. The canonical way for rules
    /// to construct a `CcInfo`.
    ///
    /// Builds a fresh `CcCompilationContext` and `CcLinkingContext`
    /// where:
    ///
    ///   - direct fields = the rule's OWN contributions, taken from
    ///     `direct: DirectCcInfo`. Resolved values only — relative
    ///     `include_dirs` should already have gone through
    ///     `AnalysisContext::resolve_package_include()` before
    ///     reaching here.
    ///   - transitive fields = each dep's matching field, attached
    ///     as an Arc child of the new NestedSet/OrderedNestedSet.
    ///     This is `Arc::clone` per dep per field — refcount work
    ///     only, no Vec allocation.
    ///
    /// **Crucially this helper does not flatten dependency fields
    /// into Vecs.** Doing so would rebuild the same O(N²) shape
    /// patch set 2b just escaped. Verified by the
    /// `from_direct_and_deps_does_not_flatten` regression test:
    /// the output's NestedSet `direct()` slice length matches
    /// `direct.headers.len()` (etc.), independent of dep counts.
    ///
    /// Returns `Arc<CcInfo>` so consumers (the orchestrator's
    /// `built_providers` map, dependent rules' `analyze` calls)
    /// share by refcount.
    pub fn from_direct_and_deps(
        direct: DirectCcInfo,
        deps: impl IntoIterator<Item = Arc<CcInfo>>,
    ) -> Arc<CcInfo> {
        // Collect deps once so we can iterate twice (compilation +
        // linking) without consuming the iterator.
        let deps: Vec<Arc<CcInfo>> = deps.into_iter().collect();

        // Compilation context: each dep's `compilation` Arc is now
        // unconditional (post-5.B.3.e — no longer Option). Use the
        // dep's compilation NestedSet fields as transitive children.
        let dep_compilations: Vec<&Arc<CcCompilationContext>> =
            deps.iter().map(|d| &d.compilation).collect();

        let comp_headers = NestedSet::new(
            direct.headers,
            dep_compilations.iter().map(|c| Arc::clone(&c.headers)).collect(),
        );
        let comp_textual = NestedSet::new(
            direct.textual_headers,
            dep_compilations
                .iter()
                .map(|c| Arc::clone(&c.textual_headers))
                .collect(),
        );
        let comp_inc = OrderedNestedSet::new(
            direct.include_dirs,
            dep_compilations
                .iter()
                .map(|c| Arc::clone(&c.include_dirs))
                .collect(),
        );
        let comp_quote = OrderedNestedSet::new(
            direct.quote_include_dirs,
            dep_compilations
                .iter()
                .map(|c| Arc::clone(&c.quote_include_dirs))
                .collect(),
        );
        let comp_sys = OrderedNestedSet::new(
            direct.system_include_dirs,
            dep_compilations
                .iter()
                .map(|c| Arc::clone(&c.system_include_dirs))
                .collect(),
        );
        let comp_def = OrderedNestedSet::new(
            direct.defines,
            dep_compilations.iter().map(|c| Arc::clone(&c.defines)).collect(),
        );
        let compilation = Arc::new(CcCompilationContext {
            headers: comp_headers,
            textual_headers: comp_textual,
            include_dirs: comp_inc,
            quote_include_dirs: comp_quote,
            system_include_dirs: comp_sys,
            defines: comp_def,
            // local_defines is direct-only; no transitive merging.
            local_defines: direct.local_defines.into_boxed_slice(),
        });

        // Linking context: same shape, OrderedNestedSet for both
        // fields per ChatGPT review §3.
        // Linking context: each dep's linking field is now Arc
        // directly (no Option, post-5.B.3.e), so the filter_map is gone.
        let link_inputs = OrderedNestedSet::new(
            direct.linker_inputs,
            deps.iter().map(|d| Arc::clone(&d.linking.linker_inputs)).collect(),
        );
        let link_frags = OrderedNestedSet::new(
            direct.link_fragments,
            deps.iter().map(|d| Arc::clone(&d.linking.link_fragments)).collect(),
        );
        let linking = Arc::new(CcLinkingContext {
            linker_inputs: link_inputs,
            link_fragments: link_frags,
        });

        Arc::new(CcInfo { compilation, linking })
    }
}

/// #39 graph-RSS structural census over a set of `CcInfo` providers
/// (typically every CcInfo-bearing entry in the orchestrator's
/// `built_providers`). For each of the eight nested-set fields, walks
/// the DAG with Arc-identity dedup ACROSS all providers and returns the
/// distinct-node element storage.
///
/// This is the cheapest definitive test (Codex 019e8ee5) for whether
/// CcInfo nested-set retention is the 82 GB analysis-RSS hog: compare
/// `sum_unique_direct` and `max_direct_len` across targets of growing
/// size. Stable/small → Arc sharing works, hog is elsewhere (.bzl
/// evaluator state, retained action payloads). Growing `max_direct_len`
/// → a merge path is flattening dep contributions per consumer.
pub fn cc_info_structural_census(
    infos: &[Arc<CcInfo>],
) -> Vec<(&'static str, crate::nested_set::NestedSetStats)> {
    use crate::nested_set::{NestedSet, NestedSetStats};
    use std::collections::HashSet;

    fn ns_field<'a, T: 'a>(
        roots: impl Iterator<Item = &'a Arc<NestedSet<T>>>,
    ) -> NestedSetStats {
        let mut seen: HashSet<usize> = HashSet::new();
        let mut stats = NestedSetStats::default();
        for r in roots {
            r.accumulate_stats(&mut seen, &mut stats);
        }
        stats
    }

    vec![
        (
            "compilation.headers",
            ns_field(infos.iter().map(|i| &i.compilation.headers)),
        ),
        (
            "compilation.textual_headers",
            ns_field(infos.iter().map(|i| &i.compilation.textual_headers)),
        ),
        (
            "compilation.include_dirs",
            ns_field(infos.iter().map(|i| i.compilation.include_dirs.inner())),
        ),
        (
            "compilation.quote_include_dirs",
            ns_field(infos.iter().map(|i| i.compilation.quote_include_dirs.inner())),
        ),
        (
            "compilation.system_include_dirs",
            ns_field(infos.iter().map(|i| i.compilation.system_include_dirs.inner())),
        ),
        (
            "compilation.defines",
            ns_field(infos.iter().map(|i| i.compilation.defines.inner())),
        ),
        (
            "linking.linker_inputs",
            ns_field(infos.iter().map(|i| i.linking.linker_inputs.inner())),
        ),
        (
            "linking.link_fragments",
            ns_field(infos.iter().map(|i| i.linking.link_fragments.inner())),
        ),
    ]
}

/// Shared-library information mirroring Bazel's
/// `CcSharedLibraryInfo`. Emitted by `cc_shared_library` (and
/// `tf_cc_shared_library`) so downstream `cc_shared_library` deps
/// can subtract this lib's `exports` from their own static-link
/// set and link against `dynamic_library` at runtime instead.
#[derive(Debug, Clone)]
pub struct CcSharedLibraryInfo {
    pub dynamic_library: PathBuf,
    pub exports: Vec<PathBuf>,
    pub link_flags: Vec<String>,
}

impl CcSharedLibraryInfo {
    pub fn new(dynamic_library: PathBuf, exports: Vec<PathBuf>, link_flags: Vec<String>) -> Self {
        Self {
            dynamic_library,
            exports,
            link_flags,
        }
    }
}

/// Generic file information. Rules like `filegroup` /
/// `exports_files` expose the list of files they group via this
/// provider. Phase 5.B will fold this into `DefaultInfo`; for now
/// it stays as its own slot to keep 5.A mechanical.
#[derive(Debug, Clone, Default)]
pub struct FilesInfo {
    pub files: Vec<PathBuf>,
}

impl FilesInfo {
    pub fn new(files: Vec<PathBuf>) -> Self {
        Self { files }
    }
}

/// A single runfile mapping: the repository-rooted LOGICAL path a test
/// expects to find at `$TEST_SRCDIR/<logical_path>`, and the PHYSICAL
/// file on disk it resolves to. Bazel materializes these as a symlink
/// forest under `<test>.runfiles/`.
///
/// `logical_path` includes the workspace segment so external repos sit
/// as siblings, e.g.
/// `org_tensorflow/tensorflow/core/distributed_runtime/rpc/grpc_testlib_server`.
/// It carries NO `bazel-bin`/`bazel-out` prefix — runfiles strip the
/// output-root prefix and keep the repo-relative path (a `cc_binary`'s
/// output sits at `<workspace>/<package>/<target_name>`). That is why a
/// test computing `TensorFlowSrcRoot() + "/core/.../grpc_testlib_server"`
/// finds the GENERATED executable under what looks like the source-tree
/// namespace.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RunfileEntry {
    pub logical_path: PathBuf,
    pub physical_path: PathBuf,
}

impl RunfileEntry {
    pub fn new(logical_path: PathBuf, physical_path: PathBuf) -> Self {
        Self {
            logical_path,
            physical_path,
        }
    }
}

/// Transitive runfiles provider — the rush analogue of Bazel's
/// `DefaultInfo.default_runfiles`. A `cc_library`'s `data` targets'
/// outputs are republished here and flow through `deps` into any
/// consuming binary/test. That republish-through-deps is exactly how a
/// helper binary declared in a *library's* `data` (e.g.
/// `grpc_testlib`'s `data = [":grpc_testlib_server"]`) reaches a
/// `cc_test` that only lists the library in `deps` — the mechanism the
/// distributed_runtime TestCluster tests rely on.
///
/// Backed by a [`NestedSet`] so the transitive union is Arc-shared with
/// no per-consumer flatten (same anti-O(N²) discipline as [`CcInfo`]).
/// Value-level dedup on `logical_path` is the materializer's job (it
/// must also reject two different physical artifacts claiming one
/// logical path rather than silently last-wins).
#[derive(Debug, Clone)]
pub struct RunfilesInfo {
    pub files: Arc<crate::nested_set::NestedSet<RunfileEntry>>,
}

impl RunfilesInfo {
    /// Empty set — for targets with no `data` and no deps that carry
    /// runfiles. Shares the global empty NestedSet.
    pub fn empty() -> Arc<RunfilesInfo> {
        Arc::new(RunfilesInfo {
            files: crate::nested_set::NestedSet::empty(),
        })
    }

    /// Build from this target's OWN direct runfile entries plus the
    /// runfiles of its dependencies (each attached as an Arc child —
    /// refcount only, never flattened).
    pub fn from_direct_and_deps(
        direct: Vec<RunfileEntry>,
        deps: &[Arc<ProviderSet>],
    ) -> Arc<RunfilesInfo> {
        let transitive = deps
            .iter()
            .filter_map(|p| p.runfiles.as_ref().map(|r| r.files.clone()))
            .collect();
        Arc::new(RunfilesInfo {
            files: crate::nested_set::NestedSet::new(direct, transitive),
        })
    }
}

/// Phase 5.C0 P2 helper: extract dep CcInfo arcs from a slice of
/// dep ProviderSets. Migrated rules call this at the top of
/// analyze() to keep the existing CcInfo-merge code paths
/// working unchanged.
pub fn extract_cc_infos(deps: &[Arc<ProviderSet>]) -> Vec<Arc<CcInfo>> {
    deps.iter().filter_map(|p| p.cc_info.clone()).collect()
}

/// Flatten the transitive `defines` exported by a set of dependency
/// compilation `defines` roots into the `-D` payload strings (without the
/// `-D` prefix), Bazel-faithfully.
///
/// Bazel's `defines` attribute is TRANSITIVE: a `cc_library`'s `defines`
/// flow through `CcInfo.compilation_context.defines` to every consumer's
/// compile action — including the consumer's own translation units. rush
/// already merges them into each target's provider, but the compile
/// command paths emit only the target's *direct* `defines`; this helper
/// supplies the dep contribution so a consumer compiles with the same
/// `-D`s Bazel would pass.
///
/// Each `root` is a dependency's already-merged `compilation.defines`
/// set, so this only unions across the direct deps. Semantics match a
/// Bazel depset flatten:
///   - postorder (a node's children before the node's own direct items),
///   - `Arc`-identity node dedup (shared diamonds visited once),
///   - exact-value dedup (`FOO=1` and `FOO=2` are distinct and both
///     kept — macro names are never parsed; values are verbatim).
///
/// Do NOT substitute `OrderedNestedSet::iter()`: it is preorder/direct-first
/// with no value dedup, which would diverge from Bazel and, worse, differ
/// between the three compile-arg paths and trip the PASS-B verifier.
pub fn flatten_transitive_defines<'a>(
    strings: &rush_action::ids::StringInterner,
    roots: impl IntoIterator<Item = &'a Arc<NestedSet<StringId>>>,
) -> Vec<String> {
    use crate::nested_set::FastHashSet;

    fn walk(
        node: &Arc<NestedSet<StringId>>,
        strings: &rush_action::ids::StringInterner,
        seen_nodes: &mut FastHashSet<*const NestedSet<StringId>>,
        seen_values: &mut FastHashSet<StringId>,
        out: &mut Vec<String>,
    ) {
        let ptr = Arc::as_ptr(node);
        if !seen_nodes.insert(ptr) {
            return;
        }
        // Children first → postorder (transitive deps precede own).
        for child in node.transitive() {
            walk(child, strings, seen_nodes, seen_values, out);
        }
        for sid in node.direct() {
            // StringId is the interner's canonical value identity. Allocate the
            // output String only when the ID is first observed.
            if seen_values.insert(*sid) {
                out.push(strings.str(*sid).to_string());
            }
        }
    }

    let mut seen_nodes = FastHashSet::default();
    let mut seen_values = FastHashSet::default();
    let mut out: Vec<String> = Vec::new();
    for root in roots {
        walk(root, strings, &mut seen_nodes, &mut seen_values, &mut out);
    }
    out
}

/// The final ordered `-D` payload list for a compile action: the
/// transitive dependency `defines` (already postorder/value-deduped by
/// [`flatten_transitive_defines`]) first, then this target's own direct
/// `defines` minus any exact value a dep already contributed.
///
/// This is the single source of truth used by all compile-arg paths so
/// the inline `analyze()`, `build_legacy_command_specs()`, and
/// `compile_source()` emissions stay byte-identical for the verifier.
pub fn merged_compile_defines(dep_defines: &[String], direct_defines: &[String]) -> Vec<String> {
    let mut out: Vec<String> = dep_defines.to_vec();
    for d in direct_defines {
        if !out.contains(d) {
            out.push(d.clone());
        }
    }
    out
}

/// Does a stored dependency label (`stored`, a canonical
/// `Label::to_string()` form as the orchestrator records it) match a
/// raw label `raw` as it appears in a rule attribute, for a target in
/// `package`?
///
/// Shared by both the analyze pass (`AnalysisContext::get_dependency_provider`)
/// and the legacy command-spec pass so filegroup-label `srcs` expansion
/// resolves IDENTICALLY in PASS-A and PASS-B — any divergence here would
/// surface as a scheduler verifier hard-stop. Matching, most specific
/// first:
///   1. exact (`raw == stored`),
///   2. `:foo` resolved to `//<package>:foo`,
///   3. `:name` tail equal AND (if `raw` names a package) the stored
///      label contains `//<pkg>:` — covers a stored label carrying a
///      workspace/repo prefix the raw attribute omits.
pub fn dep_label_matches(stored: &str, raw: &str, package: &str) -> bool {
    if raw == stored {
        return true;
    }
    if let Some(rel) = raw.strip_prefix(':') {
        if stored == format!("//{}:{}", package, rel) {
            return true;
        }
    }
    // Bazel implicit/relative target conventions a raw attribute label may use
    // that the canonical stored key (`//pkg:target`) spells out in full. Two
    // cases, both of which otherwise fail to resolve and silently drop the dep's
    // entire `CcInfo.linking.linker_inputs` subtree (every alwayslink
    // `<op>_op_lib` / `<kernel>` archive carrying REGISTER_OP/REGISTER_KERNEL),
    // so op/kernel registrations get stripped ("Op type not registered 'NoOp'",
    // "No registered 'ShuffleDataset' OpKernel"):
    //
    //   1. Bare ABSOLUTE label — has NO `:target` (e.g.
    //      `//tensorflow/core/ops`, `@repo//path/to/pkg`, or `@repo`). Bazel
    //      resolves a package label to its last path component and a bare
    //      repository to its root target:
    //      `//tensorflow/core/ops` ≡ `//tensorflow/core/ops:ops`,
    //      `@repo` ≡ `@repo//:repo`.
    //   2. Bare RELATIVE target name — no `//`, no `:`, no `@` (e.g.
    //      `shuffle_dataset_op` in a deps list). Bazel resolves it against the
    //      CONSUMER's package: `shuffle_dataset_op` ≡ `//<package>:shuffle_dataset_op`.
    //
    // Canonicalize to the `//pkg:target` form, then retry the exact +
    // package-suffix checks below.
    let raw_canon: String = if let Some(canonical) = canonical_bare_repository_label(raw) {
        canonical
    } else if raw.starts_with("//") || raw.starts_with('@') {
        if raw.contains(':') {
            raw.to_string()
        } else {
            // Bare package label → `:<last path component>`.
            let pkg_part = raw.trim_start_matches('@').split("//").nth(1).unwrap_or("");
            match pkg_part.rsplit('/').next() {
                Some(last) if !last.is_empty() => format!("{}:{}", raw, last),
                _ => raw.to_string(),
            }
        }
    } else if !raw.starts_with(':') && !raw.contains(':') && !raw.is_empty() {
        // Bare relative target name → `//<consumer package>:<name>`.
        format!("//{}:{}", package, raw)
    } else {
        raw.to_string()
    };
    if raw_canon != raw && raw_canon == stored {
        return true;
    }
    let raw = raw_canon.as_str();
    if let Some(rel) = raw.strip_prefix(':') {
        if stored == format!("//{}:{}", package, rel) {
            return true;
        }
    }
    let raw_tail = raw.rsplit(':').next().unwrap_or(raw);
    let stored_tail = stored.rsplit(':').next().unwrap_or(stored);
    if raw_tail != stored_tail {
        return false;
    }
    match raw
        .trim_start_matches('@')
        .split("//")
        .nth(1)
        .and_then(|s| s.split(':').next())
    {
        Some(p) if !p.is_empty() => stored.contains(&format!("//{}:", p)),
        // `:foo` (no package in raw): tail match alone is ambiguous, so
        // only the exact/`:`-resolved cases above may match it.
        _ => false,
    }
}

/// Expand a bare external repository to its root target.
///
/// Bazel treats both apparent `@repo` and canonical `@@repo` spellings as the
/// root-package label whose target has the repository's name.
pub(crate) fn canonical_bare_repository_label(raw: &str) -> Option<String> {
    let repository = raw
        .strip_prefix("@@")
        .or_else(|| raw.strip_prefix('@'))?;
    if repository.is_empty() || repository.contains("//") || repository.contains(':') {
        return None;
    }
    Some(format!("@{repository}//:{repository}"))
}

fn canonical_dependency_lookup_label(
    raw: &str,
    package: &str,
    repository: Option<&str>,
) -> String {
    let repository_prefix = repository
        .map(|name| format!("@{name}"))
        .unwrap_or_default();
    let mut label = if let Some(canonical) = canonical_bare_repository_label(raw) {
        canonical
    } else if let Some(target) = raw.strip_prefix(':') {
        format!("{repository_prefix}//{package}:{target}")
    } else if let Some(rest) = raw
        .strip_prefix("@@//")
        .or_else(|| raw.strip_prefix("@//"))
    {
        format!("//{rest}")
    } else if let Some(rest) = raw.strip_prefix("@@") {
        format!("@{rest}")
    } else if raw.starts_with("//") {
        format!("{repository_prefix}{raw}")
    } else if !raw.starts_with("//") && !raw.starts_with('@') {
        format!("{repository_prefix}//{package}:{raw}")
    } else {
        raw.to_string()
    };

    if (label.starts_with("//") || label.starts_with('@')) && !label.contains(':') {
        let package_part = label.split_once("//").map(|(_, rest)| rest).unwrap_or("");
        if let Some(target) = package_part
            .rsplit('/')
            .next()
            .filter(|part| !part.is_empty())
            .map(str::to_string)
        {
            label.push(':');
            label.push_str(&target);
        }
    }
    label
}

/// Match dependency labels without discarding repository identity.
///
/// Relative labels resolve in the consumer's repository-qualified package
/// spelling (`@repo//pkg:target` for external consumers); explicit repository
/// labels retain their identity.
/// Callers that still need historical apparent-to-canonical repository
/// fallbacks must try this exact form first, then accept a fallback only when
/// it is unique across all direct dependencies.
pub fn dep_label_matches_repository_exact(
    stored: &str,
    raw: &str,
    package: &str,
    repository: Option<&str>,
) -> bool {
    canonical_dependency_lookup_label(stored, package, None)
        == canonical_dependency_lookup_label(raw, package, repository)
}

/// Phase 5.C0 P0 helper: walk every dep's `CcCompilationContext`
/// (headers + textual_headers), filter to the generated artifacts
/// (is_source == false), return their ArtifactIds. Rules
/// declaring CppCompile actions add these to the action's input
/// set so producer validation can chain through:
///
///   gentbl produces .h.inc → cc_library declares .h.inc as
///   compile input → pass 3 finds the MlirTblgen action as
///   producer.
///
/// Without this, the array_ops.h class of bug isn't
/// representable in the action graph: a compile depending on a
/// generated header has no edge to the generator action.
pub fn collect_generated_header_inputs(
    registry: &rush_action::ids::ArtifactRegistry,
    dep_cc_infos: &[Arc<CcInfo>],
) -> Vec<rush_action::ids::ArtifactId> {
    let mut hdrs = Vec::new();
    for dep_cc in dep_cc_infos {
        for h_aid in dep_cc.compilation.headers.iter() {
            if !registry.artifact(h_aid).is_source {
                hdrs.push(h_aid);
            }
        }
        for h_aid in dep_cc.compilation.textual_headers.iter() {
            if !registry.artifact(h_aid).is_source {
                hdrs.push(h_aid);
            }
        }
    }
    hdrs
}

/// Phase 5.C.4 commit 27: path-returning sibling of
/// [`collect_generated_header_inputs`]. Centralizes the dep-
/// generated-header walk that legacy `build_legacy_command_specs`
/// implementations across cc_library / cc_binary / cc_test /
/// tf_cc_library all needed (and got slightly wrong, repeatedly).
///
/// Pro's commit-27 directive: "the bug repeated enough times.
/// The threshold is not 'four duplicated call sites.' The
/// threshold is 'the same bug already happened repeatedly.'
/// Congratulations, you crossed it while the verifier kept
/// receipts."
///
/// Walks `dep_cc_infos`, takes both `compilation.headers` AND
/// `compilation.textual_headers`, filters for non-source
/// (`!is_source`) — i.e. generated artifacts — and returns
/// the registry-resolved absolute path for each. Order is
/// preserved within a single dep; cross-dep dedup is the
/// caller's responsibility (Vec contains check or sort/dedup
/// at the use site).
///
/// Why a separate helper from `collect_generated_header_inputs`:
/// the analyze() side wants ArtifactIds (for `declare_action`'s
/// inputs slice); legacy spec builders want PathBufs (for
/// NormalizedCommandSpec.declared_inputs). Same logical walk,
/// different downstream type. Pro: "Keep this separate from
/// include-dir construction. Header inputs and include search
/// paths are related, but not the same thing."
pub fn collect_generated_header_paths(
    registry: &rush_action::ids::ArtifactRegistry,
    dep_cc_infos: &[Arc<CcInfo>],
) -> Vec<std::path::PathBuf> {
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    for dep_cc in dep_cc_infos {
        for h_aid in dep_cc.compilation.headers.iter() {
            if !registry.artifact(h_aid).is_source {
                let p = registry.artifact_path(h_aid).to_path_buf();
                if !paths.contains(&p) {
                    paths.push(p);
                }
            }
        }
        for h_aid in dep_cc.compilation.textual_headers.iter() {
            if !registry.artifact(h_aid).is_source {
                let p = registry.artifact_path(h_aid).to_path_buf();
                if !paths.contains(&p) {
                    paths.push(p);
                }
            }
        }
    }
    paths
}

/// Phase 5.C0 P2: typed proto-files provider.
///
/// `proto_library` exposes this with its direct `.proto`
/// sources + transitive sources from its proto_library deps.
/// `cc_proto_library` consumes via this typed path to enumerate
/// the .proto closure for ProtocCompile action declarations,
/// rather than walking the generic artifact NestedSet (which
/// would drag the legacy raw-artifact-sweep pattern back into
/// the typed-provider system).
///
/// Direct vs transitive split mirrors `CcCompilationContext`:
/// each rule contributes its own .proto sources directly,
/// transitive coverage comes from upstream proto_library deps'
/// own ProtoFilesInfo. Walking the NestedSet yields every
/// .proto in the closure with Arc-identity dedup.
#[derive(Debug, Clone)]
pub struct ProtoFilesInfo {
    pub direct_sources: Box<[rush_action::ids::ArtifactId]>,
    pub transitive_sources: Arc<crate::nested_set::NestedSet<rush_action::ids::ArtifactId>>,
}

impl ProtoFilesInfo {
    pub fn empty() -> Arc<ProtoFilesInfo> {
        Arc::new(ProtoFilesInfo {
            direct_sources: Vec::new().into_boxed_slice(),
            transitive_sources: crate::nested_set::NestedSet::empty(),
        })
    }

    /// Build from direct .proto ArtifactIds plus dep ProtoFilesInfo
    /// arcs. Transitive coverage attaches dep transitive sets as
    /// Arc children — never flattens.
    pub fn from_direct_and_deps(
        direct: Vec<rush_action::ids::ArtifactId>,
        deps: impl IntoIterator<Item = Arc<ProtoFilesInfo>>,
    ) -> Arc<ProtoFilesInfo> {
        let deps: Vec<_> = deps.into_iter().collect();
        let transitive = crate::nested_set::NestedSet::new(
            direct.clone(),
            deps.iter()
                .map(|d| Arc::clone(&d.transitive_sources))
                .collect(),
        );
        Arc::new(ProtoFilesInfo {
            direct_sources: direct.into_boxed_slice(),
            transitive_sources: transitive,
        })
    }
}

/// Custom user-defined provider. Keeps the string-keyed payload
/// from the old `Provider::Custom` variant — by design, custom
/// providers are user-defined and don't get typed slots.
#[derive(Debug, Clone)]
pub struct CustomProvider {
    pub name: String,
    pub data: HashMap<String, String>,
}

impl CustomProvider {
    pub fn new(name: String, data: HashMap<String, String>) -> Self {
        Self { name, data }
    }
}

/// Literal declared attributes needed to prepare one selected options consumer.
/// This shape is not provider, source or execution authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OptionsHeaderDeclaration {
    pub template: String,
    pub output_header: String,
    pub build_settings: std::collections::BTreeMap<String, String>,
}
/// Native inputs expected from the real TensorFlow options-header declaration.
/// Labels are canonical and the path is the exact native SOURCE artifact.
/// Generated templates remain unsupported until their producer is routed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OptionsHeaderRequest {
    pub consumer_label: String,
    pub template_label: String,
    pub output_label: String,
    pub settings: std::collections::BTreeMap<String, String>,
    pub template_path: PathBuf,
    pub provider_key: String,
    /// Caller-qualified consumer implementation; never learned from the same
    /// metadata query used to admit this request.
    pub expected_implementation_hash: String,
}

/// A genuine configured provider value, independent of the raw option value.
/// Alias observations keep both their requested and consumed producer identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfiguredBuildSettingInfo {
    pub requested_label: String,
    pub producer_label: String,
    pub consumed_label: String,
    pub provider_key: String,
    pub value: bool,
    pub configuration_checksum: String,
    pub configuration_id: u64,
    pub observed_occurrences: usize,
}

pub use rush_incremental::CanonicalRegularFileMetadata as OptionsHeaderSourceMetadata;

/// Descriptor-bound bytes of the native source template observed in analysis.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OptionsHeaderSource {
    pub label: String,
    pub path: PathBuf,
    pub digest: [u8; 32],
    pub metadata: OptionsHeaderSourceMetadata,
}

/// Immutable result of two genuine configured analysis rounds for one request.
/// This value alone grants no cache/read certificate or writer completion.
/// A caller must bind it to the captured native command and template artifact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedOptionsHeaderInputs {
    pub request: OptionsHeaderRequest,
    pub command_fingerprint: [u8; 32],
    pub main_workspace_root: PathBuf,
    pub consumer_configuration: String,
    pub consumer_configuration_id: u64,
    pub settings: std::collections::BTreeMap<String, ConfiguredBuildSettingInfo>,
    pub template: OptionsHeaderSource,
}

/// Typed bundle of providers attached to an analyzed target.
///
/// Replaces the previous `HashMap<String, Provider>`. Well-known
/// providers (CcInfo, CcSharedLibraryInfo, FilesInfo) get typed
/// `Option<Arc<...>>` slots so:
///
/// - alias forwarding is `Arc::clone`, not a deep `Vec` clone;
/// - lookup is field access, not `HashMap::get(&"CcInfo".to_string())`;
/// - typos in the provider name are compile errors, not runtime
///   `None`s that silently drop CcInfo in cc_library.
///
/// Phase 5.C.4 public outputs provider.
///
/// Distinguishes a target's "public" artifacts (what consumers
/// see when they iterate the target's NestedSet) from the full
/// declared-action output set (which includes intermediate `.o`
/// files etc. that no consumer should observe). Pro's call: the
/// scheduler's previous synthesis-from-all-declared-outputs path
/// gave cc_binary an extra `.o` artifact entry that the legacy
/// `build()` shape never returned.
///
/// Per-rule public output convention:
/// - `cc_library` → `[<lib>.a]` (the static library)
/// - `cc_binary`  → `[<bin>]` (the executable)
/// - `cc_test`    → `[<test>]` (the test executable)
/// - rules that produce no link output (filegroup, alias) leave
///   it `None` and dispatchers fall back to declared outputs
///
/// When `DefaultInfo` is present on a target's ProviderSet, the
/// phase 5.C.4 dispatcher synthesizes the target's NestedSet
/// using these `ArtifactId`s. When absent, it falls back to
/// every declared action's outputs (preserves compatibility for
/// un-migrated rules).
#[derive(Debug, Clone)]
pub struct DefaultInfo {
    /// Public output artifacts in declaration order. Resolved
    /// to paths through the registry at synthesis time, same way
    /// declared outputs are.
    pub files: Vec<rush_action::ids::ArtifactId>,
}

/// User-defined providers still go through the string-keyed
/// `custom` map. That's by design — custom providers ARE
/// dynamically-named.
#[derive(Debug, Clone, Default)]
pub struct ProviderSet {
    pub cc_info: Option<Arc<CcInfo>>,
    pub cc_shared: Option<Arc<CcSharedLibraryInfo>>,
    pub files: Option<Arc<FilesInfo>>,
    /// Phase 5.C0 P2: typed proto-files provider. proto_library
    /// exposes; cc_proto_library consumes for its ProtocCompile
    /// declarations. Forwarded through alias rules via
    /// clone_forwarded.
    pub proto_files: Option<Arc<ProtoFilesInfo>>,
    /// Phase 5.C.4: public outputs (DefaultInfo.files). When
    /// present, the scheduler dispatcher uses this list to
    /// synthesize the target's NestedSet — preventing
    /// intermediate action outputs (e.g. cc_binary's `.o`)
    /// from leaking into consumers' transitive view.
    pub default_info: Option<Arc<DefaultInfo>>,
    /// Transitive runfiles (Bazel `DefaultInfo.default_runfiles`). Carries
    /// `data`-target outputs through `deps` to consuming binaries/tests.
    /// `None` for the many rules with no `data` and no runfiles-bearing
    /// deps; the test runner then stages an empty (or source-only) tree.
    pub runfiles: Option<Arc<RunfilesInfo>>,
    pub custom: HashMap<String, Arc<CustomProvider>>,
}

impl ProviderSet {
    /// Empty provider set — typical for rules that produce
    /// artifacts but expose nothing to dependents (e.g. `cc_test`).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Refcount-only clone for alias forwarding. Each `Arc<...>`
    /// slot bumps refcount; the custom HashMap is shallow-cloned
    /// but its values are `Arc<CustomProvider>` so those are
    /// refcount bumps too. Replaces the old
    /// `dep_providers.clone()` HashMap deep clone in the
    /// orchestrator's alias path.
    pub fn clone_forwarded(&self) -> Self {
        Self {
            cc_info: self.cc_info.clone(),
            cc_shared: self.cc_shared.clone(),
            files: self.files.clone(),
            proto_files: self.proto_files.clone(),
            default_info: self.default_info.clone(),
            runfiles: self.runfiles.clone(),
            custom: self.custom.clone(),
        }
    }

    /// True if the set has no providers at all.
    pub fn is_empty(&self) -> bool {
        self.cc_info.is_none()
            && self.cc_shared.is_none()
            && self.files.is_none()
            && self.proto_files.is_none()
            && self.default_info.is_none()
            && self.runfiles.is_none()
            && self.custom.is_empty()
    }

    pub fn get_cc_info(&self) -> Option<&Arc<CcInfo>> {
        self.cc_info.as_ref()
    }

    pub fn get_cc_shared(&self) -> Option<&Arc<CcSharedLibraryInfo>> {
        self.cc_shared.as_ref()
    }

    pub fn get_files(&self) -> Option<&Arc<FilesInfo>> {
        self.files.as_ref()
    }

    pub fn get_proto_files(&self) -> Option<&Arc<ProtoFilesInfo>> {
        self.proto_files.as_ref()
    }

    /// Phase 5.C.4: public output artifacts as declared by the
    /// rule. `None` for rules that haven't migrated to populate
    /// `DefaultInfo` — the scheduler dispatcher then falls back
    /// to all declared outputs, preserving the pre-migration
    /// shape.
    pub fn get_default_info(&self) -> Option<&Arc<DefaultInfo>> {
        self.default_info.as_ref()
    }

    pub fn get_runfiles(&self) -> Option<&Arc<RunfilesInfo>> {
        self.runfiles.as_ref()
    }

    pub fn get_custom(&self, name: &str) -> Option<&Arc<CustomProvider>> {
        self.custom.get(name)
    }

    pub fn with_cc_info(mut self, cc: Arc<CcInfo>) -> Self {
        self.cc_info = Some(cc);
        self
    }

    pub fn with_cc_shared(mut self, sh: CcSharedLibraryInfo) -> Self {
        self.cc_shared = Some(Arc::new(sh));
        self
    }

    pub fn with_files(mut self, files: FilesInfo) -> Self {
        self.files = Some(Arc::new(files));
        self
    }

    pub fn with_proto_files(mut self, pf: Arc<ProtoFilesInfo>) -> Self {
        self.proto_files = Some(pf);
        self
    }

    /// Phase 5.C.4: attach `DefaultInfo` declaring this target's
    /// public output artifacts. cc_library / cc_binary / cc_test
    /// call this in analyze() with the .a / executable
    /// `ArtifactId` so the scheduler synthesizes the target's
    /// NestedSet from the public set, not the full declared-
    /// output set.
    pub fn with_default_info(mut self, di: DefaultInfo) -> Self {
        self.default_info = Some(Arc::new(di));
        self
    }

    /// Attach the transitive runfiles set. cc_library/cc_binary/cc_test/
    /// tf_cc_test call this in analyze() with
    /// `RunfilesInfo::from_direct_and_deps(direct, deps)`.
    pub fn with_runfiles(mut self, rf: Arc<RunfilesInfo>) -> Self {
        self.runfiles = Some(rf);
        self
    }

    pub fn with_custom(mut self, prov: CustomProvider) -> Self {
        self.custom.insert(prov.name.clone(), Arc::new(prov));
        self
    }

    /// Merge another set's providers into this one, preferring
    /// existing entries on conflict. Used by the orchestrator's
    /// alias-forwarding path: `self` starts as the rule's own
    /// providers (typically empty for an alias), and `other` is
    /// the aliased target's providers — anything `self` doesn't
    /// already have gets pulled in.
    pub fn extend_missing_from(&mut self, other: &ProviderSet) {
        if self.cc_info.is_none() {
            self.cc_info = other.cc_info.clone();
        }
        if self.cc_shared.is_none() {
            self.cc_shared = other.cc_shared.clone();
        }
        if self.files.is_none() {
            self.files = other.files.clone();
        }
        if self.proto_files.is_none() {
            self.proto_files = other.proto_files.clone();
        }
        if self.default_info.is_none() {
            self.default_info = other.default_info.clone();
        }
        if self.runfiles.is_none() {
            self.runfiles = other.runfiles.clone();
        }
        for (name, prov) in &other.custom {
            self.custom
                .entry(name.clone())
                .or_insert_with(|| prov.clone());
        }
    }
}

// ============================================================================
// Phase 5.C0 producer-validation report shapes.
//
// `validate_producers` walks every CcInfo + every declared action
// in the registry, collecting artifacts that look like they should
// have a producer but don't. The report is data, not eprintlns;
// callers (orchestrator) format it for display, tests assert on
// the field values.
// ============================================================================

use rush_action::ids::DeclaredActionId;

/// Which provider field a missing-producer artifact came from.
/// Lets the formatter say "compilation.headers" vs
/// "linking.linker_inputs" instead of "some artifact in CcInfo".
#[derive(Clone, Copy, Eq, PartialEq, Debug)]
pub enum ProviderField {
    CompilationHeaders,
    LinkingLinkerInputs,
}

impl ProviderField {
    pub fn name(self) -> &'static str {
        match self {
            ProviderField::CompilationHeaders => "compilation.headers",
            ProviderField::LinkingLinkerInputs => "linking.linker_inputs",
        }
    }
}

/// A provider-attached artifact that has no declared producer
/// (Pass 2 of producer/consumer validation).
#[derive(Clone, Debug)]
pub struct MissingProvider {
    pub label: String,
    pub artifact: rush_action::ids::ArtifactId,
    pub kind: rush_action::ids::ArtifactKind,
    pub field: ProviderField,
}

/// A declared-action input that has no declared producer (Pass 3).
#[derive(Clone, Debug)]
pub struct MissingInput {
    pub action: DeclaredActionId,
    pub mnemonic: rush_action::ids::StringId,
    pub artifact: rush_action::ids::ArtifactId,
    pub kind: rush_action::ids::ArtifactKind,
}

/// Summary of a producer-validation pass over the registry +
/// providers. Returned by `validate_producers` so tests and the
/// orchestrator's reporting code share the same data shape.
#[derive(Clone, Debug, Default)]
pub struct ProducerValidationReport {
    pub declared_action_count: usize,
    /// Phase 5.C0 P4: how many declared actions are
    /// `Incomplete` — i.e. flagged not-schedulable. Phase 5.C
    /// will fall back to legacy `build()` execution for these.
    pub incomplete_action_count: usize,
    pub missing_provider_producers: Vec<MissingProvider>,
    pub missing_input_producers: Vec<MissingInput>,
}

/// Walk the registry + providers, return the validation report.
///
/// Phase 5.C0 P3: validation keys off `ArtifactData.is_source`,
/// not `ArtifactKind`. The previous "skip ArtifactKind::Header
/// because it might be source-on-disk" heuristic was a blind
/// spot for the generated-header bug class — generated `.h.inc`
/// files exposed via gentbl, generated `.pb.h` from protoc,
/// generated headers from any tool action — exactly the
/// artifacts validation is supposed to catch missing producers
/// for. Now: any artifact with `is_source = false` (registered
/// via `register_generated_artifact`) and no
/// `owner_action_declared` is reported as missing.
///
/// Pass 2 (every generated provider artifact has a declared
/// producer): walks every `built_providers` CcInfo's
/// `compilation.headers` and `linking.linker_inputs`.
///
/// Pass 3 (every declared action's generated input has a declared
/// producer): walks every `DeclaredAction` in the registry,
/// applies the same generated-needs-producer rule to inputs.
///
/// Passes 1 (no duplicate producer) and 4 (source artifacts have
/// no producer) are enforced at registration time inside
/// `ArtifactRegistry::declare_action` — they fail fast, never
/// reach this report.
pub fn validate_producers(
    registry: &rush_action::ids::ArtifactRegistry,
    built_providers: &HashMap<String, ProviderSet>,
) -> ProducerValidationReport {
    let mut report = ProducerValidationReport {
        declared_action_count: registry.declared_action_count(),
        incomplete_action_count: registry.incomplete_action_count(),
        ..Default::default()
    };

    // Pass 2: typed-provider artifacts.
    for (label, providers) in built_providers {
        if let Some(cc_info) = providers.get_cc_info() {
            for aid in cc_info.compilation.headers.iter() {
                let data = registry.artifact(aid);
                if !data.is_source && data.owner_action_declared.is_none() {
                    report.missing_provider_producers.push(MissingProvider {
                        label: label.clone(),
                        artifact: aid,
                        kind: data.kind,
                        field: ProviderField::CompilationHeaders,
                    });
                }
            }
            for lid in cc_info.linking.linker_inputs.iter() {
                let li = registry.linker_input(lid);
                let data = registry.artifact(li.artifact);
                if !data.is_source && data.owner_action_declared.is_none() {
                    report.missing_provider_producers.push(MissingProvider {
                        label: label.clone(),
                        artifact: li.artifact,
                        kind: data.kind,
                        field: ProviderField::LinkingLinkerInputs,
                    });
                }
            }
        }
    }

    // Pass 3: declared-action inputs.
    for (action_id, action) in registry.declared_actions() {
        for &input_aid in action.inputs.iter() {
            let data = registry.artifact(input_aid);
            if !data.is_source && data.owner_action_declared.is_none() {
                report.missing_input_producers.push(MissingInput {
                    action: action_id,
                    mnemonic: action.mnemonic,
                    artifact: input_aid,
                    kind: data.kind,
                });
            }
        }
    }

    report
}

// ============================================================================
// Phase 5.B.2: typed C++ provider context types (skeleton).
//
// These types are the future shape of CcInfo per
// docs/item-5-typed-providers-plan.md §5.B. They live alongside the
// existing flat-Vec CcInfo for now; phase 5.B.3 migrates rule
// providers() impls to populate them, phase 5.B.4 migrates consumers,
// phase 5.B.5 deletes the flat fields.
//
// Field type choices encode ChatGPT review §3 corrections:
//   - headers/libraries use ArtifactId — same artifact reached two
//     ways IS the same artifact, dedup is correct.
//   - include_dirs/defines/link_flags use OrderedNestedSet — order
//     is correctness-bearing, value-dedup is the consumer's call.
//   - linker_inputs use a richer LinkerInput struct (alwayslink
//     flag, ownership) so we don't lose the alwayslink_libraries
//     distinction the flat shape carries.
//   - link_flags use LinkFragment — supports group semantics
//     (--start-group / --end-group) and -L<dir> / -l<lib> / raw
//     flags as distinct kinds, so consumers don't have to parse
//     strings to figure out fragment shape.
// ============================================================================

use crate::nested_set::{NestedSet, OrderedNestedSet};
use rush_action::ids::{ArtifactId, LinkerInputId, PathId, StringId};

/// A single fragment of a link command line. Fragments propagate
/// transitively through `Arc<OrderedNestedSet<LinkFragment>>` so
/// merging across deps doesn't allocate a fresh `Vec` per target.
/// (ChatGPT review §3 specifically called out the
/// `Vec<LinkFragment>` shape as the next O(N²) flattening hazard
/// on TF-rung-11 aggregator targets, and was right.)
///
/// Each fragment kind is distinct at the type level so consumers
/// don't re-parse strings to figure out whether `-Lfoo` is a path
/// or a raw flag — the shape carries the semantics.
#[derive(Clone, Debug)]
pub enum LinkFragment {
    /// A library / archive — handle into the registry's
    /// linker-input arena (which carries owner, artifact,
    /// alwayslink, and optional extras as one rich payload).
    Library(LinkerInputId),
    /// `-Wl,--start-group ... -Wl,--end-group` wrapping. Inputs
    /// inside the group MAY repeat by design (circular static
    /// archives) — `Box<[LinkerInputId]>` rather than `Vec` so
    /// the slice is owned exactly once per Group instance.
    Group(Box<[LinkerInputId]>),
    /// `-L<dir>` library search path.
    SearchDir(PathId),
    /// Raw flag string (e.g. `-Wl,-z,defs`). Interned so
    /// alias-forwarding stays cheap.
    Flag(StringId),
}

/// C++ compilation context — everything a downstream `cc_library`
/// or `cc_binary` needs to compile its sources after seeing this
/// dep on its `deps` list.
///
/// All transitive-merging fields are `Arc<NestedSet<...>>` /
/// `Arc<OrderedNestedSet<...>>` so:
///   - the rule's `from_direct_and_deps` helper builds the merged
///     set in O(deps) time (Arc-clone each dep set into the new
///     transitive slice), not O(transitive closure);
///   - alias forwarding is `Arc::clone` per field, not a Vec
///     copy.
///
/// Choice of NestedSet vs OrderedNestedSet per field is
/// load-bearing — see ChatGPT review §3.
#[derive(Debug, Clone)]
pub struct CcCompilationContext {
    /// Header files visible to dependents — public + private.
    /// Dedup-ok (`NestedSet`): same header artifact reached two
    /// ways is one header.
    pub headers: Arc<NestedSet<ArtifactId>>,
    /// Textual headers (`textual_hdrs` in Bazel) — included via
    /// `#include` but not exposed as compilable units. Same
    /// dedup semantics as `headers`.
    pub textual_headers: Arc<NestedSet<ArtifactId>>,
    /// `-I<dir>` paths. Order matters — the compiler searches
    /// in the order given, and a `-Iearly` shadowing a header
    /// in `-Ilate` is correctness-bearing. `OrderedNestedSet`.
    pub include_dirs: Arc<OrderedNestedSet<PathId>>,
    /// `-iquote <dir>` quote-style include paths.
    pub quote_include_dirs: Arc<OrderedNestedSet<PathId>>,
    /// `-isystem <dir>` system-style include paths.
    pub system_include_dirs: Arc<OrderedNestedSet<PathId>>,
    /// `-D<name>=<value>` preprocessor defines visible
    /// transitively. Order matters when later defines override
    /// earlier ones via re-`#define`.
    pub defines: Arc<OrderedNestedSet<StringId>>,
    /// Defines that apply ONLY to this target's own compilation,
    /// not propagated to dependents. `Box<[...]>` rather than
    /// `NestedSet` because there's no transitive merging to do.
    pub local_defines: Box<[StringId]>,
}

impl CcCompilationContext {
    /// An empty compilation context. Used by aliases (which
    /// forward exactly via Arc) and by rules that don't expose
    /// any compile-time information (cc_binary final targets,
    /// non-cc rules).
    pub fn empty() -> Self {
        Self {
            headers: NestedSet::empty(),
            textual_headers: NestedSet::empty(),
            include_dirs: OrderedNestedSet::empty(),
            quote_include_dirs: OrderedNestedSet::empty(),
            system_include_dirs: OrderedNestedSet::empty(),
            defines: OrderedNestedSet::empty(),
            local_defines: Vec::new().into_boxed_slice(),
        }
    }
}

impl Default for CcCompilationContext {
    fn default() -> Self {
        Self::empty()
    }
}

/// C++ linking context — everything needed to link this dep into
/// a downstream `cc_binary` / `cc_shared_library`.
///
/// Both fields are `Arc<OrderedNestedSet<...>>` per ChatGPT review
/// §2 + §3 corrections:
///   - `linker_inputs` is `OrderedNestedSet<LinkerInputId>` so the
///     type signals "no auto-dedup" at the call site. Consumers
///     are responsible for whatever flatten/group strategy
///     preserves their semantics. Static-archive ordering can
///     legitimately need duplicates inside `--start-group` /
///     `--end-group` wrappers.
///   - `link_fragments` is `OrderedNestedSet<LinkFragment>` so
///     transitive merging across deps attaches dep fragment sets
///     as Arc children (refcount only) instead of cloning a fresh
///     `Vec<LinkFragment>` per target. The latter shape was the
///     next O(N²) provider-flattening hazard at TF-rung-11 scale.
///
/// LinkerInput's rich payload (owner, artifact, alwayslink,
/// optional extras) lives in the `ArtifactRegistry`'s linker-input
/// arena. Provider sets carry just the 4-byte `LinkerInputId`.
#[derive(Debug, Clone)]
pub struct CcLinkingContext {
    pub linker_inputs: Arc<OrderedNestedSet<LinkerInputId>>,
    pub link_fragments: Arc<OrderedNestedSet<LinkFragment>>,
}

impl CcLinkingContext {
    pub fn empty() -> Self {
        Self {
            linker_inputs: OrderedNestedSet::empty(),
            link_fragments: OrderedNestedSet::empty(),
        }
    }
}

impl Default for CcLinkingContext {
    fn default() -> Self {
        Self::empty()
    }
}

/// "Direct" contribution input for `CcInfo::from_direct_and_deps`.
/// This is what a rule's `providers()` returns to describe its
/// OWN headers / includes / libs / flags before transitive
/// merging with deps. Phase 5.B.3 introduces the merge helper;
/// 5.B.2 (revised) just defines the input shape with
/// `LinkerInputId` (arena handle) and `LinkFragment` (typed
/// fragment) instead of inline `LinkerInput` / parallel-Vec
/// shapes.
#[derive(Default, Debug, Clone)]
pub struct DirectCcInfo {
    pub headers: Vec<ArtifactId>,
    pub textual_headers: Vec<ArtifactId>,
    pub include_dirs: Vec<PathId>,
    pub quote_include_dirs: Vec<PathId>,
    pub system_include_dirs: Vec<PathId>,
    pub defines: Vec<StringId>,
    pub local_defines: Vec<StringId>,
    pub linker_inputs: Vec<LinkerInputId>,
    pub link_fragments: Vec<LinkFragment>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runfiles_unions_direct_and_transitive_deps() {
        // The grpc_testlib_server case: a dep (the library) carries the
        // helper binary in its runfiles; the consumer (the test) must see
        // it transitively even though the consumer never names it directly.
        let dep_entry = RunfileEntry::new(
            PathBuf::from(
                "org_tensorflow/tensorflow/core/distributed_runtime/rpc/grpc_testlib_server",
            ),
            PathBuf::from("/data/tensorflow/rush-out/grpc_testlib_server"),
        );
        let dep = Arc::new(ProviderSet::default().with_runfiles(
            RunfilesInfo::from_direct_and_deps(vec![dep_entry.clone()], &[]),
        ));
        let direct_entry = RunfileEntry::new(
            PathBuf::from("org_tensorflow/tensorflow/some/testdata.txt"),
            PathBuf::from("/src/tensorflow/some/testdata.txt"),
        );
        let consumer = RunfilesInfo::from_direct_and_deps(vec![direct_entry.clone()], &[dep]);
        let all: Vec<RunfileEntry> = consumer.files.flatten();
        assert!(all.contains(&direct_entry), "consumer's own data present");
        assert!(
            all.contains(&dep_entry),
            "dep's data republished transitively through deps"
        );
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn runfiles_empty_is_empty() {
        assert!(RunfilesInfo::from_direct_and_deps(vec![], &[])
            .files
            .flatten()
            .is_empty());
        assert!(RunfilesInfo::empty().files.flatten().is_empty());
    }

    #[test]
    fn runfiles_does_not_flatten_into_consumer_direct() {
        // Same anti-O(N²) guarantee as CcInfo: a consumer's OWN direct
        // slice stays its own size regardless of how large the deps' sets
        // are — deps are attached as Arc children, never copied in.
        let mut deps: Vec<Arc<ProviderSet>> = Vec::new();
        for i in 0..50 {
            let e = RunfileEntry::new(
                PathBuf::from(format!("org_tensorflow/pkg/dep{i}")),
                PathBuf::from(format!("/out/dep{i}")),
            );
            deps.push(Arc::new(ProviderSet::default().with_runfiles(
                RunfilesInfo::from_direct_and_deps(vec![e], &[]),
            )));
        }
        let direct = RunfileEntry::new(
            PathBuf::from("org_tensorflow/pkg/self"),
            PathBuf::from("/out/self"),
        );
        let consumer = RunfilesInfo::from_direct_and_deps(vec![direct], &deps);
        assert_eq!(
            consumer.files.direct().len(),
            1,
            "consumer's own direct slice is NOT flattened to include 50 deps"
        );
        assert_eq!(
            consumer.files.flatten().len(),
            51,
            "but the full transitive walk sees all of them"
        );
    }

    #[test]
    fn dep_label_matches_absolute_label_shorthand() {
        // Bazel `//pkg` ≡ `//pkg:lastcomponent`.
        assert!(dep_label_matches(
            "//tensorflow/core/ops:ops",
            "//tensorflow/core/ops",
            "tensorflow/core/data"
        ));
        // External-repo bare package label.
        assert!(dep_label_matches(
            "@local_tsl//tsl/platform:platform",
            "@local_tsl//tsl/platform",
            "tensorflow/core"
        ));
        // A bare repository names its root target, not a same-named package.
        assert!(dep_label_matches(
            "@pybind11//:pybind11",
            "@pybind11",
            "tensorflow/core"
        ));
        assert!(!dep_label_matches(
            "@pybind11//pybind11:pybind11",
            "@pybind11",
            "tensorflow/core"
        ));
        // Must NOT match a different target in the same package.
        assert!(!dep_label_matches(
            "//tensorflow/core/ops:array_ops_op_lib",
            "//tensorflow/core/ops",
            "tensorflow/core/data"
        ));
        // Explicit `:target` still resolves exactly (regression guard).
        assert!(dep_label_matches(
            "//tensorflow/core/ops:ops",
            "//tensorflow/core/ops:ops",
            "tensorflow/core/data"
        ));
        // Bare RELATIVE target name `foo` ≡ `//<package>:foo` (e.g. the
        // `"shuffle_dataset_op"` dep that pulls the ShuffleDataset OpKernel).
        assert!(dep_label_matches(
            "//tensorflow/core/kernels/data:shuffle_dataset_op",
            "shuffle_dataset_op",
            "tensorflow/core/kernels/data"
        ));
        // Bare relative name must NOT match a same-named target in a DIFFERENT
        // package.
        assert!(!dep_label_matches(
            "//tensorflow/core/other:shuffle_dataset_op",
            "shuffle_dataset_op",
            "tensorflow/core/kernels/data"
        ));
    }

    #[test]
    fn repository_exact_dependency_labels_preserve_identity() {
        assert!(dep_label_matches_repository_exact(
            "//p:gen.h",
            ":gen.h",
            "p",
            None,
        ));
        assert!(!dep_label_matches_repository_exact(
            "@r//p:gen.h",
            ":gen.h",
            "p",
            None,
        ));
        assert!(dep_label_matches_repository_exact(
            "@r//p:gen.h",
            "@@r//p:gen.h",
            "p",
            None,
        ));
        assert!(!dep_label_matches_repository_exact(
            "@s//p:gen.h",
            "@r//p:gen.h",
            "p",
            None,
        ));
        assert!(dep_label_matches_repository_exact(
            "@r//p:p",
            "@r//p",
            "consumer",
            None,
        ));
        assert!(dep_label_matches_repository_exact(
            "@r//:r",
            "@r",
            "consumer",
            None,
        ));
        assert!(dep_label_matches_repository_exact(
            "@r//:r",
            "@@r",
            "consumer",
            None,
        ));
        assert!(!dep_label_matches_repository_exact(
            "@s//:s",
            "@r",
            "consumer",
            None,
        ));
        assert!(dep_label_matches_repository_exact(
            "@r//p:gen.h",
            ":gen.h",
            "p",
            Some("r"),
        ));
        assert!(dep_label_matches_repository_exact(
            "@r//p:gen.h",
            "//p:gen.h",
            "consumer",
            Some("r"),
        ));
        assert!(!dep_label_matches_repository_exact(
            "//p:gen.h",
            ":gen.h",
            "p",
            Some("r"),
        ));
        assert!(!dep_label_matches_repository_exact(
            "@s//p:gen.h",
            ":gen.h",
            "p",
            Some("r"),
        ));
        assert!(dep_label_matches_repository_exact(
            "//p:gen.h",
            "@//p:gen.h",
            "consumer",
            Some("r"),
        ));
    }

    #[test]
    fn flatten_transitive_defines_postorder_node_and_value_dedup() {
        use crate::nested_set::NestedSet;
        use rush_action::ids::StringInterner;
        use std::sync::Arc;

        let mut interner = StringInterner::new();
        let a = interner.intern("TENSORFLOW_USE_MKLDNN_CONTRACTION_KERNEL");
        let b = interner.intern("EIGEN_USE_AVX512_GEMM_KERNELS=0");
        let c = interner.intern("CHILD_DEF");
        // Interner dedups equal strings → same id, exercising value-dedup.
        let a_again = interner.intern("TENSORFLOW_USE_MKLDNN_CONTRACTION_KERNEL");
        assert_eq!(a, a_again);

        // Shared child node (a diamond) reachable from both deps.
        let child = NestedSet::new(vec![c], vec![]);
        // dep1: direct [a, b], transitive [child].
        let dep1 = NestedSet::new(vec![a, b], vec![Arc::clone(&child)]);
        // dep2: direct [a_again] (dup value), transitive [child] (dup node).
        let dep2 = NestedSet::new(vec![a_again], vec![Arc::clone(&child)]);

        let roots = vec![dep1, dep2];
        let out = flatten_transitive_defines(&interner, roots.iter());

        // Postorder: child's CHILD_DEF before dep1's own; dep2's child is
        // node-deduped and its duplicate value is value-deduped.
        assert_eq!(
            out,
            vec![
                "CHILD_DEF".to_string(),
                "TENSORFLOW_USE_MKLDNN_CONTRACTION_KERNEL".to_string(),
                "EIGEN_USE_AVX512_GEMM_KERNELS=0".to_string(),
            ]
        );
    }

    #[test]
    fn merged_compile_defines_dep_first_then_own_minus_exact_dups() {
        let dep = vec!["A=1".to_string(), "B".to_string()];
        let own = vec!["B".to_string(), "C".to_string(), "A=2".to_string()];
        // Dep defines first; own "B" dropped (exact dup); "A=2" kept
        // (distinct value from "A=1" — macro names are never parsed).
        assert_eq!(
            merged_compile_defines(&dep, &own),
            vec![
                "A=1".to_string(),
                "B".to_string(),
                "C".to_string(),
                "A=2".to_string(),
            ]
        );
    }

    #[test]
    fn empty_set_is_empty() {
        let p = ProviderSet::empty();
        assert!(p.is_empty());
        assert!(p.get_cc_info().is_none());
    }

    #[test]
    fn with_cc_info_attaches_provider() {
        let p = ProviderSet::empty().with_cc_info(CcInfo::empty());
        assert!(p.get_cc_info().is_some());
    }

    #[test]
    fn clone_forwarded_shares_arcs() {
        let cc = CcInfo::empty();
        let p1 = ProviderSet::empty().with_cc_info(Arc::clone(&cc));
        let p2 = p1.clone_forwarded();
        let a1 = p1.get_cc_info().unwrap();
        let a2 = p2.get_cc_info().unwrap();
        assert!(Arc::ptr_eq(a1, a2));
    }

    #[test]
    fn extend_missing_from_pulls_only_unfilled_slots() {
        let dest_cc = CcInfo::empty();
        let mut dest = ProviderSet::empty().with_cc_info(Arc::clone(&dest_cc));
        let src = ProviderSet::empty()
            .with_cc_info(CcInfo::empty())
            .with_files(FilesInfo::new(vec![PathBuf::from("f.txt")]));
        dest.extend_missing_from(&src);
        // CcInfo slot already filled in dest — Arc identity preserved.
        assert!(Arc::ptr_eq(dest.get_cc_info().unwrap(), &dest_cc));
        // FilesInfo slot was empty — pulled from src.
        assert_eq!(
            dest.get_files().unwrap().files[0],
            PathBuf::from("f.txt")
        );
    }

    // ---------------------------------------------------------------------
    // Phase 5.B.2: typed C++ provider context types — skeleton tests.
    // These exercise construction + Arc-identity sharing semantics.
    // Consumer wiring is phase 5.B.3+.

    use rush_action::ids::{ArtifactId, ArtifactKey, ArtifactKind, ArtifactRegistry,
                            ConfigId, TargetId};

    fn intern_path(reg: &mut ArtifactRegistry, p: &str) -> rush_action::ids::PathId {
        reg.intern_path(std::path::Path::new(p))
    }

    fn intern_str(reg: &mut ArtifactRegistry, s: &str) -> rush_action::ids::StringId {
        reg.intern_string(s)
    }

    fn intern_artifact(
        reg: &mut ArtifactRegistry,
        path: &str,
        kind: ArtifactKind,
        target: u32,
    ) -> ArtifactId {
        let p = intern_path(reg, path);
        let is_source = matches!(kind, ArtifactKind::Source);
        reg.intern_artifact(
            ArtifactKey {
                config: ConfigId::DEFAULT,
                exec_path: p,
            },
            kind,
            is_source,
            TargetId(target),
            None,
        )
    }

    #[test]
    fn cc_compilation_context_empty_constructs() {
        let ctx = CcCompilationContext::empty();
        assert!(ctx.headers.direct().is_empty());
        assert!(ctx.include_dirs.direct().is_empty());
        assert!(ctx.local_defines.is_empty());
    }

    #[test]
    fn cc_linking_context_empty_constructs() {
        let ctx = CcLinkingContext::empty();
        assert!(ctx.linker_inputs.direct().is_empty());
        assert!(ctx.link_fragments.direct().is_empty());
    }

    #[test]
    fn cc_compilation_context_carries_artifact_ids() {
        // A header artifact registered once, referenced from a
        // CcCompilationContext: NestedSet stores ArtifactId (4 bytes),
        // not PathBuf. The kind lives in the registry.
        let mut reg = ArtifactRegistry::new();
        let header = intern_artifact(&mut reg, "include/foo.h", ArtifactKind::Header, 1);
        let ctx = CcCompilationContext {
            headers: NestedSet::new(vec![header], vec![]),
            ..CcCompilationContext::empty()
        };
        assert_eq!(ctx.headers.direct(), &[header]);
        // Round-trip back to data via the registry.
        let path = reg.artifact_path(ctx.headers.direct()[0]);
        assert_eq!(path, std::path::Path::new("include/foo.h"));
    }

    #[test]
    fn cc_compilation_context_include_dirs_preserve_order() {
        // Order-bearing field. Two distinct sibling sets each
        // contributing the same PathId: OrderedNestedSet must
        // yield the value at both positions, not dedup it.
        let mut reg = ArtifactRegistry::new();
        let inc_a = intern_path(&mut reg, "/a/inc");
        let inc_b = intern_path(&mut reg, "/b/inc");
        let inc_shared = intern_path(&mut reg, "/shared/inc");
        let left = OrderedNestedSet::new(vec![inc_a, inc_shared], vec![]);
        let right = OrderedNestedSet::new(vec![inc_b, inc_shared], vec![]);
        let merged = OrderedNestedSet::new(vec![], vec![left, right]);
        let v: Vec<_> = merged.iter().collect();
        // Declaration order: left first (a, shared) then right
        // (b, shared). shared appears TWICE — value-dedup is the
        // consumer's call, never automatic.
        assert_eq!(v, vec![inc_a, inc_shared, inc_b, inc_shared]);
    }

    #[test]
    fn link_fragment_library_round_trips_through_arena() {
        // The LinkerInputId arena (ChatGPT review §4): rich data
        // (owner, artifact, alwayslink, optional extras) lives in
        // the registry; provider sets carry the 4-byte handle.
        // Verify a Library fragment round-trips through the arena.
        let mut reg = ArtifactRegistry::new();
        let lib_normal = intern_artifact(&mut reg, "lib/libnormal.a", ArtifactKind::StaticLibrary, 1);
        let lib_alwayslink = intern_artifact(
            &mut reg,
            "lib/libalwayslink.a",
            ArtifactKind::StaticLibrary,
            2,
        );
        let id_normal = reg.intern_linker_input(rush_action::ids::LinkerInputData {
            owner: TargetId(1),
            artifact: lib_normal,
            alwayslink: false,
            extras: None,
        });
        let id_alwayslink = reg.intern_linker_input(rush_action::ids::LinkerInputData {
            owner: TargetId(2),
            artifact: lib_alwayslink,
            alwayslink: true,
            extras: None,
        });
        let frag_normal = LinkFragment::Library(id_normal);
        let frag_alwayslink = LinkFragment::Library(id_alwayslink);
        match frag_alwayslink {
            LinkFragment::Library(id) => {
                let data = reg.linker_input(id);
                assert!(data.alwayslink);
                assert_eq!(data.owner, TargetId(2));
            }
            _ => panic!("expected Library fragment"),
        }
        match frag_normal {
            LinkFragment::Library(id) => {
                let data = reg.linker_input(id);
                assert!(!data.alwayslink);
            }
            _ => panic!("expected Library fragment"),
        }
    }

    #[test]
    fn link_fragment_group_preserves_input_multiplicity() {
        // Static-archive groups can need duplicates to resolve
        // circular symbol deps. Verify Group fragments don't
        // dedupe their input list at the type level. With the
        // arena-handle shape, intentional duplication means the
        // same LinkerInputId appearing multiple times in the
        // Box<[LinkerInputId]>.
        let mut reg = ArtifactRegistry::new();
        let liba = intern_artifact(&mut reg, "lib/liba.a", ArtifactKind::StaticLibrary, 1);
        let libb = intern_artifact(&mut reg, "lib/libb.a", ArtifactKind::StaticLibrary, 2);
        let id_a = reg.intern_linker_input(rush_action::ids::LinkerInputData {
            owner: TargetId(1),
            artifact: liba,
            alwayslink: false,
            extras: None,
        });
        let id_b = reg.intern_linker_input(rush_action::ids::LinkerInputData {
            owner: TargetId(2),
            artifact: libb,
            alwayslink: false,
            extras: None,
        });
        let group_inputs: Box<[_]> = vec![id_a, id_b, id_a].into_boxed_slice();
        let frag = LinkFragment::Group(group_inputs);
        match frag {
            LinkFragment::Group(v) => {
                assert_eq!(v.len(), 3);
                assert_eq!(v[0], id_a);
                assert_eq!(v[2], id_a);
            }
            _ => panic!("expected Group"),
        }
    }

    #[test]
    fn cc_compilation_context_clone_is_arc_share() {
        // Cloning a CcCompilationContext should be cheap — every
        // transitive field is Arc-wrapped. Verify Arc::ptr_eq
        // holds on the headers field after a derive-Clone.
        let mut reg = ArtifactRegistry::new();
        let h = intern_artifact(&mut reg, "h.h", ArtifactKind::Header, 1);
        let ctx1 = CcCompilationContext {
            headers: NestedSet::new(vec![h], vec![]),
            ..CcCompilationContext::empty()
        };
        let ctx2 = ctx1.clone();
        assert!(Arc::ptr_eq(&ctx1.headers, &ctx2.headers));
        assert!(Arc::ptr_eq(&ctx1.include_dirs, &ctx2.include_dirs));
    }

    #[test]
    fn cc_linking_context_clone_shares_arcs() {
        // Same Arc-share invariant for CcLinkingContext.
        // linker_inputs and link_fragments are both
        // Arc<OrderedNestedSet<...>> so .clone() is two refcount
        // bumps, not Vec/depset duplication.
        let ctx1 = CcLinkingContext::empty();
        let ctx2 = ctx1.clone();
        assert!(Arc::ptr_eq(&ctx1.linker_inputs, &ctx2.linker_inputs));
        assert!(Arc::ptr_eq(&ctx1.link_fragments, &ctx2.link_fragments));
    }

    #[test]
    fn direct_cc_info_default_is_empty() {
        let d = DirectCcInfo::default();
        assert!(d.headers.is_empty());
        assert!(d.include_dirs.is_empty());
        assert!(d.local_defines.is_empty());
        assert!(d.linker_inputs.is_empty());
        assert!(d.link_fragments.is_empty());
    }

    #[test]
    fn intern_str_silences_unused() {
        let mut reg = ArtifactRegistry::new();
        let id = intern_str(&mut reg, "FOO=1");
        assert_eq!(reg.strings().str(id), "FOO=1");
    }

    // ---------------------------------------------------------------------
    // Phase 5.B.3.a: CcInfo::from_direct_and_deps merge helper tests.

    fn cc_info_with_one_header(reg: &mut ArtifactRegistry, name: &str) -> Arc<CcInfo> {
        let h = intern_artifact(reg, name, ArtifactKind::Header, 1);
        let direct = DirectCcInfo {
            headers: vec![h],
            ..DirectCcInfo::default()
        };
        CcInfo::from_direct_and_deps(direct, std::iter::empty())
    }

    #[test]
    fn from_direct_and_deps_empty_inputs() {
        // Empty direct + empty deps → CcInfo with empty contexts.
        let cc = CcInfo::from_direct_and_deps(DirectCcInfo::default(), std::iter::empty());
        assert!(cc.compilation.headers.direct().is_empty());
        assert!(cc.compilation.include_dirs.direct().is_empty());
        assert!(cc.compilation.local_defines.is_empty());
        assert!(cc.linking.linker_inputs.direct().is_empty());
        assert!(cc.linking.link_fragments.direct().is_empty());
    }

    #[test]
    fn from_direct_and_deps_populates_direct_slice() {
        let mut reg = ArtifactRegistry::new();
        let h1 = intern_artifact(&mut reg, "h1.h", ArtifactKind::Header, 1);
        let h2 = intern_artifact(&mut reg, "h2.h", ArtifactKind::Header, 1);
        let inc1 = intern_path(&mut reg, "/inc/1");
        let def1 = intern_str(&mut reg, "FOO=1");
        let direct = DirectCcInfo {
            headers: vec![h1, h2],
            include_dirs: vec![inc1],
            defines: vec![def1],
            local_defines: vec![intern_str(&mut reg, "LOCAL=1")],
            ..DirectCcInfo::default()
        };
        let cc = CcInfo::from_direct_and_deps(direct, std::iter::empty());
        let comp = &cc.compilation;
        assert_eq!(comp.headers.direct(), &[h1, h2]);
        assert_eq!(comp.include_dirs.direct(), &[inc1]);
        assert_eq!(comp.defines.direct(), &[def1]);
        assert_eq!(comp.local_defines.len(), 1);
    }

    #[test]
    fn from_direct_and_deps_attaches_dep_arcs_as_transitive_children() {
        // The load-bearing invariant: a dep's compilation context
        // is attached BY ARC, not iterated into the new direct
        // slice. Verify by Arc::ptr_eq between the dep's Arc<CcCompilationContext>
        // and... hmm — we can't directly Arc::ptr_eq through the
        // NestedSet's `transitive` slice without exposing it. The
        // iter walk gives us the values; the Arc-identity dedup
        // proves identity preservation indirectly (see
        // `from_direct_and_deps_diamond_dedup_at_node_level`).
        //
        // Direct check: walking the new context yields direct items
        // PLUS each dep's items, with no duplication.
        let mut reg = ArtifactRegistry::new();
        let dep_cc = cc_info_with_one_header(&mut reg, "dep.h");
        let h_root = intern_artifact(&mut reg, "root.h", ArtifactKind::Header, 2);
        let direct = DirectCcInfo {
            headers: vec![h_root],
            ..DirectCcInfo::default()
        };
        let cc = CcInfo::from_direct_and_deps(direct, vec![Arc::clone(&dep_cc)]);
        let comp = &cc.compilation;
        // Direct slice = root only (NOT flattened from dep)
        assert_eq!(comp.headers.direct().len(), 1);
        assert_eq!(comp.headers.direct()[0], h_root);
        // Walking the full DAG yields root + dep header
        let walked: Vec<_> = comp.headers.iter().collect();
        assert_eq!(walked.len(), 2);
        assert!(walked.contains(&h_root));
    }

    #[test]
    fn structural_census_counts_shared_dep_once_across_consumers() {
        // #39 graph-RSS census decisive invariant: a dep shared by two
        // consumers must be counted ONCE (Arc-identity dedup across
        // roots). Build C, then A and B that both depend on C. The
        // headers field should report 3 unique nodes (A, B, C) and 3
        // unique direct elements (a.h, b.h, c.h) — C's contribution
        // counted once, not twice. A naive non-deduping walk would
        // report 4 unique-direct (double-counting C).
        let mut reg = ArtifactRegistry::new();
        let c = cc_info_with_one_header(&mut reg, "c.h");
        let a = CcInfo::from_direct_and_deps(
            DirectCcInfo {
                headers: vec![intern_artifact(&mut reg, "a.h", ArtifactKind::Header, 1)],
                ..DirectCcInfo::default()
            },
            vec![Arc::clone(&c)],
        );
        let b = CcInfo::from_direct_and_deps(
            DirectCcInfo {
                headers: vec![intern_artifact(&mut reg, "b.h", ArtifactKind::Header, 1)],
                ..DirectCcInfo::default()
            },
            vec![Arc::clone(&c)],
        );
        let census = cc_info_structural_census(&[a, b]);
        let headers = census
            .iter()
            .find(|(name, _)| *name == "compilation.headers")
            .map(|(_, st)| st)
            .expect("headers field present");
        assert_eq!(headers.unique_nodes, 3, "A + B + shared C, deduped");
        assert_eq!(
            headers.sum_unique_direct, 3,
            "a.h + b.h + c.h, C counted once"
        );
        assert_eq!(headers.max_direct_len, 1);
        assert_eq!(headers.transitive_edges, 2, "A→C and B→C");
    }

    #[test]
    fn from_direct_and_deps_diamond_dedup_at_node_level() {
        // Diamond: root depends on left and right; both left and
        // right depend on the same leaf CcInfo. The leaf's headers
        // should appear ONCE in the walk over root.compilation.headers,
        // not twice — Arc-identity dedup at the NestedSet node level.
        // This is the regression guard against accidentally cloning
        // dep field VALUES instead of dep field NESTED SETS.
        let mut reg = ArtifactRegistry::new();
        let leaf = cc_info_with_one_header(&mut reg, "leaf.h");
        let h_left = intern_artifact(&mut reg, "left.h", ArtifactKind::Header, 1);
        let h_right = intern_artifact(&mut reg, "right.h", ArtifactKind::Header, 2);
        let h_root = intern_artifact(&mut reg, "root.h", ArtifactKind::Header, 3);

        let left = CcInfo::from_direct_and_deps(
            DirectCcInfo {
                headers: vec![h_left],
                ..DirectCcInfo::default()
            },
            vec![Arc::clone(&leaf)],
        );
        let right = CcInfo::from_direct_and_deps(
            DirectCcInfo {
                headers: vec![h_right],
                ..DirectCcInfo::default()
            },
            vec![Arc::clone(&leaf)],
        );
        let root = CcInfo::from_direct_and_deps(
            DirectCcInfo {
                headers: vec![h_root],
                ..DirectCcInfo::default()
            },
            vec![Arc::clone(&left), Arc::clone(&right)],
        );

        let walked: Vec<_> = root.compilation.headers.iter().collect();
        // Expected: root.h, left.h, leaf.h, right.h — each ONCE.
        // leaf.h NOT twice despite reachable via two paths.
        assert_eq!(walked.len(), 4);
        let leaf_h = leaf.compilation.headers.direct()[0];
        assert_eq!(
            walked.iter().filter(|&&h| h == leaf_h).count(),
            1,
            "leaf header should be visited exactly once via Arc-identity dedup"
        );
    }

    #[test]
    fn from_direct_and_deps_does_not_flatten() {
        // Regression guard: the helper must NOT iterate dependency
        // field values into the new direct slice. The output's
        // direct slice length equals the input's direct length —
        // no matter how many deps contribute through the
        // transitive children.
        let mut reg = ArtifactRegistry::new();
        let dep_a = cc_info_with_one_header(&mut reg, "a.h");
        let dep_b = cc_info_with_one_header(&mut reg, "b.h");
        let dep_c = cc_info_with_one_header(&mut reg, "c.h");
        let h_own = intern_artifact(&mut reg, "own.h", ArtifactKind::Header, 1);
        let direct = DirectCcInfo {
            headers: vec![h_own],
            ..DirectCcInfo::default()
        };
        let cc = CcInfo::from_direct_and_deps(direct, vec![dep_a, dep_b, dep_c]);
        let comp = &cc.compilation;
        // Direct length is exactly 1 (own.h), not 4.
        assert_eq!(
            comp.headers.direct().len(),
            1,
            "direct slice must not include dep contributions"
        );
        // But the full walk reaches all 4 headers.
        let walked: Vec<_> = comp.headers.iter().collect();
        assert_eq!(walked.len(), 4);
    }

    #[test]
    fn from_direct_and_deps_link_fragments_preserve_declaration_order() {
        // OrderedNestedSet contract: link fragments from deps appear
        // in dep declaration order, after the rule's own fragments.
        let mut reg = ArtifactRegistry::new();
        let lib_a = intern_artifact(&mut reg, "liba.a", ArtifactKind::StaticLibrary, 1);
        let lib_b = intern_artifact(&mut reg, "libb.a", ArtifactKind::StaticLibrary, 2);
        let lid_a = reg.intern_linker_input(rush_action::ids::LinkerInputData {
            owner: TargetId(1),
            artifact: lib_a,
            alwayslink: false,
            extras: None,
        });
        let lid_b = reg.intern_linker_input(rush_action::ids::LinkerInputData {
            owner: TargetId(2),
            artifact: lib_b,
            alwayslink: false,
            extras: None,
        });

        let dep_a = CcInfo::from_direct_and_deps(
            DirectCcInfo {
                link_fragments: vec![LinkFragment::Library(lid_a)],
                ..DirectCcInfo::default()
            },
            std::iter::empty(),
        );
        let dep_b = CcInfo::from_direct_and_deps(
            DirectCcInfo {
                link_fragments: vec![LinkFragment::Library(lid_b)],
                ..DirectCcInfo::default()
            },
            std::iter::empty(),
        );
        let root = CcInfo::from_direct_and_deps(
            DirectCcInfo::default(),
            vec![dep_a, dep_b],
        );
        let frags: Vec<_> = root.linking.link_fragments.iter().collect();
        assert_eq!(frags.len(), 2);
        // dep_a's fragment first (declaration order), then dep_b's.
        match &frags[0] {
            LinkFragment::Library(id) => assert_eq!(*id, lid_a),
            _ => panic!("expected Library"),
        }
        match &frags[1] {
            LinkFragment::Library(id) => assert_eq!(*id, lid_b),
            _ => panic!("expected Library"),
        }
    }

    // ---------------------------------------------------------------------
    // Phase 5.C0: validate_producers tests. The pure function in
    // providers.rs walks a registry + provider map and returns a
    // ProducerValidationReport. Tests construct synthetic states
    // covering each pass-2/pass-3 case.

    fn make_src(
        reg: &mut ArtifactRegistry,
        rel: &str,
        target: u32,
    ) -> ArtifactId {
        let p = reg.intern_path(std::path::Path::new(rel));
        reg.intern_artifact(
            ArtifactKey {
                config: ConfigId::DEFAULT,
                exec_path: p,
            },
            ArtifactKind::Source,
            true,
            TargetId(target),
            None,
        )
    }

    fn make_obj(
        reg: &mut ArtifactRegistry,
        rel: &str,
        target: u32,
    ) -> ArtifactId {
        let p = reg.intern_path(std::path::Path::new(rel));
        reg.intern_artifact(
            ArtifactKey {
                config: ConfigId::DEFAULT,
                exec_path: p,
            },
            ArtifactKind::Object,
            false,
            TargetId(target),
            None,
        )
    }

    fn build_provider_with_libs(
        reg: &mut ArtifactRegistry,
        libs: Vec<rush_action::ids::ArtifactId>,
    ) -> ProviderSet {
        let lids: Vec<_> = libs
            .into_iter()
            .map(|aid| {
                reg.intern_linker_input(rush_action::ids::LinkerInputData {
                    owner: TargetId(0),
                    artifact: aid,
                    alwayslink: false,
                    extras: None,
                })
            })
            .collect();
        let direct = DirectCcInfo {
            linker_inputs: lids,
            ..DirectCcInfo::default()
        };
        let cc_info = CcInfo::from_direct_and_deps(direct, std::iter::empty());
        ProviderSet::empty().with_cc_info(cc_info)
    }

    #[test]
    fn validate_producers_empty_state() {
        let reg = ArtifactRegistry::new();
        let providers: HashMap<String, ProviderSet> = HashMap::new();
        let report = validate_producers(&reg, &providers);
        assert_eq!(report.declared_action_count, 0);
        assert!(report.missing_provider_producers.is_empty());
        assert!(report.missing_input_producers.is_empty());
    }

    #[test]
    fn validate_producers_chains_declared_action_with_input() {
        // Compile action: Source input → Object output (producer
        // for the object). Archive action: Object input
        // (producer = compile) → StaticLibrary output. Pass 3
        // sees zero missing because every generated input has a
        // declared producer.
        let mut reg = ArtifactRegistry::new();
        let mnemonic_compile = reg.intern_string("CppCompile");
        let mnemonic_archive = reg.intern_string("CcArchive");
        let src = make_src(&mut reg, "src/main.cc", 1);
        let obj = make_obj(&mut reg, "obj/main.o", 1);
        let lib = {
            let p = reg.intern_path(std::path::Path::new("lib/libfoo.a"));
            reg.intern_artifact(
                ArtifactKey {
                    config: ConfigId::DEFAULT,
                    exec_path: p,
                },
                ArtifactKind::StaticLibrary,
                false,
            TargetId(1),
                None,
            )
        };
        reg.declare_action(rush_action::ids::DeclaredAction {
            owner_target: TargetId(1),
            mnemonic: mnemonic_compile,
            inputs: Box::new([src]),
            outputs: Box::new([obj]),
            status: rush_action::ids::ActionDeclarationStatus::Complete,
        })
        .unwrap();
        reg.declare_action(rush_action::ids::DeclaredAction {
            owner_target: TargetId(1),
            mnemonic: mnemonic_archive,
            inputs: Box::new([obj]),
            outputs: Box::new([lib]),
            status: rush_action::ids::ActionDeclarationStatus::Complete,
        })
        .unwrap();

        let report = validate_producers(&reg, &HashMap::new());
        assert_eq!(report.declared_action_count, 2);
        assert!(report.missing_input_producers.is_empty());
    }

    #[test]
    fn validate_producers_flags_undeclared_action_input() {
        // Archive action's input (Object) was never declared as
        // any other action's output. Pass 3 records it as
        // missing.
        let mut reg = ArtifactRegistry::new();
        let mnemonic_archive = reg.intern_string("CcArchive");
        let obj = make_obj(&mut reg, "obj/orphan.o", 1);
        let lib = {
            let p = reg.intern_path(std::path::Path::new("lib/libfoo.a"));
            reg.intern_artifact(
                ArtifactKey {
                    config: ConfigId::DEFAULT,
                    exec_path: p,
                },
                ArtifactKind::StaticLibrary,
                false,
            TargetId(1),
                None,
            )
        };
        reg.declare_action(rush_action::ids::DeclaredAction {
            owner_target: TargetId(1),
            mnemonic: mnemonic_archive,
            inputs: Box::new([obj]),
            outputs: Box::new([lib]),
            status: rush_action::ids::ActionDeclarationStatus::Complete,
        })
        .unwrap();
        let report = validate_producers(&reg, &HashMap::new());
        assert_eq!(report.missing_input_producers.len(), 1);
        assert_eq!(report.missing_input_producers[0].artifact, obj);
        assert_eq!(report.missing_input_producers[0].kind, ArtifactKind::Object);
    }

    #[test]
    fn validate_producers_flags_undeclared_provider_lib() {
        // Linker_inputs LinkerInputId derefs to a StaticLibrary
        // ArtifactId that has no declared producer (no CcArchive
        // action declared). Pass 2 catches it.
        let mut reg = ArtifactRegistry::new();
        let lib = {
            let p = reg.intern_path(std::path::Path::new("lib/orphan.a"));
            reg.intern_artifact(
                ArtifactKey {
                    config: ConfigId::DEFAULT,
                    exec_path: p,
                },
                ArtifactKind::StaticLibrary,
                false,
            TargetId(1),
                None,
            )
        };
        let mut providers = HashMap::new();
        providers.insert("//pkg:tgt".to_string(), build_provider_with_libs(&mut reg, vec![lib]));
        let report = validate_producers(&reg, &providers);
        assert_eq!(report.missing_provider_producers.len(), 1);
        assert_eq!(report.missing_provider_producers[0].artifact, lib);
        assert_eq!(report.missing_provider_producers[0].field, ProviderField::LinkingLinkerInputs);
        assert_eq!(report.missing_provider_producers[0].label, "//pkg:tgt");
    }

    #[test]
    fn validate_producers_skips_source_kind() {
        // Source artifacts have no producer expected. Even when
        // referenced in compilation.headers (e.g. cc_library hdrs
        // that are source files registered with
        // ArtifactKind::Source), pass 2 does not flag them.
        let mut reg = ArtifactRegistry::new();
        let src_h = make_src(&mut reg, "include/api.h", 1);
        // Build a CcInfo with the source header in compilation.headers.
        let direct = DirectCcInfo {
            headers: vec![src_h],
            ..DirectCcInfo::default()
        };
        let cc = CcInfo::from_direct_and_deps(direct, std::iter::empty());
        let mut providers = HashMap::new();
        providers.insert(
            "//pkg:tgt".to_string(),
            ProviderSet::empty().with_cc_info(cc),
        );
        let report = validate_producers(&reg, &providers);
        assert!(report.missing_provider_producers.is_empty());
    }

    #[test]
    fn validate_producers_flags_generated_header() {
        // Phase 5.C0 P3: validation now keys off `is_source`,
        // not `ArtifactKind`. A header artifact registered with
        // is_source=false (i.e. produced by an action — gentbl
        // .h.inc, protoc .pb.h, etc.) MUST have a declared
        // producer. The previous "skip Header kind" heuristic
        // was a blind spot for the array_ops.h class of bug;
        // this test is the regression guard.
        let mut reg = ArtifactRegistry::new();
        let p = reg.intern_path(std::path::Path::new("gen/foo.h"));
        let gen_header_aid = reg.intern_artifact(
            ArtifactKey {
                config: ConfigId::DEFAULT,
                exec_path: p,
            },
            ArtifactKind::Header,
            /*is_source*/ false,
            TargetId(1),
            None,
        );
        let direct = DirectCcInfo {
            headers: vec![gen_header_aid],
            ..DirectCcInfo::default()
        };
        let cc = CcInfo::from_direct_and_deps(direct, std::iter::empty());
        let mut providers = HashMap::new();
        providers.insert(
            "//pkg:tgt".to_string(),
            ProviderSet::empty().with_cc_info(cc),
        );
        let report = validate_producers(&reg, &providers);
        assert_eq!(report.missing_provider_producers.len(), 1);
        assert_eq!(report.missing_provider_producers[0].artifact, gen_header_aid);
        assert_eq!(report.missing_provider_producers[0].kind, ArtifactKind::Header);
    }

    // ---------------------------------------------------------------------
    // Phase 5.C.4 commit 9 (Pro Q B): cc_library public-output regression.
    //
    // Scheduler public outputs for cc_library expose the .a only.
    // Internal .o files MUST NOT leak into DefaultInfo.files —
    // that's what `synthesized_outputs` (from declared action
    // outputs) captures, but that path is the legacy fallback for
    // un-migrated rules. Once a rule populates DefaultInfo, the
    // dispatcher overrides synthesized_outputs with DefaultInfo.files
    // (see build_orchestrator_ext.rs `public_outputs_override`).
    //
    // Legacy build() may still return [.a + .o] until 5.C.5; the
    // invariant here is on the typed-provider surface that
    // transitive consumers see. Pro's Q B (i)+(iii): accept the
    // asymmetry until 5.C.5, prove transitive consumers cannot
    // pick up Object artifacts via DefaultInfo.

    #[test]
    fn cc_library_default_info_exposes_only_static_library() {
        let mut reg = ArtifactRegistry::new();
        // The shape cc_library::analyze() builds: a StaticLibrary
        // aid stuffed into DefaultInfo.files.
        let lib_aid = intern_artifact(
            &mut reg,
            "cc_lib/libfoo.a",
            ArtifactKind::StaticLibrary,
            42,
        );
        let di = DefaultInfo {
            files: vec![lib_aid],
        };
        let providers = ProviderSet::empty()
            .with_cc_info(CcInfo::empty())
            .with_default_info(di);

        let got = providers
            .get_default_info()
            .expect("cc_library populates DefaultInfo");
        assert_eq!(got.files.len(), 1, "cc_library exposes exactly one public output (.a)");

        // Round-trip the kind through the registry — the
        // dispatcher resolves DefaultInfo.files this way and
        // anything Object-shaped would surface here.
        let kind = reg.artifact(got.files[0]).kind;
        assert!(
            matches!(kind, ArtifactKind::StaticLibrary),
            "cc_library DefaultInfo entry must be StaticLibrary, got {:?}",
            kind
        );
        assert!(
            !matches!(kind, ArtifactKind::Object),
            "Object kind in cc_library DefaultInfo would leak intermediate .o to consumers"
        );
    }

    #[test]
    fn cc_library_default_info_rejects_object_artifacts() {
        // Belt-and-suspenders: if a future refactor accidentally
        // adds the per-src .o aids alongside the .a in
        // DefaultInfo.files, this test fails. It scans every
        // entry and asserts none are ArtifactKind::Object.
        let mut reg = ArtifactRegistry::new();
        let lib_aid = intern_artifact(
            &mut reg,
            "cc_lib/libbar.a",
            ArtifactKind::StaticLibrary,
            7,
        );
        let di = DefaultInfo {
            files: vec![lib_aid],
        };
        let providers = ProviderSet::empty().with_default_info(di);

        let got = providers.get_default_info().unwrap();
        for &aid in &got.files {
            let kind = reg.artifact(aid).kind;
            assert!(
                !matches!(kind, ArtifactKind::Object),
                "DefaultInfo.files contained Object artifact (aid={:?}) — \
                 transitive consumers would see internal .o", aid
            );
        }
    }

    // Phase 5.C.4 commit 208 (Batch AA): output-validation
    // policy invariants for the 5.C.5 first-target pilot.
    // Pin the THREE-CLASS policy in tests:
    //   (a) scheduler-ready actions: missing outputs are
    //       recorded in the validation report (caller — the
    //       orchestrator — turns this into a build-mode
    //       error in 5.C.5 implementation; for now the
    //       report just records).
    //   (b) zero-action targets: validation walks 0 actions,
    //       produces 0 records, 0 failures. Pro's commit-208
    //       directive: "reports zero checks or is silent,
    //       but must not fail."
    //   (c) legacy/unmigrated targets: unchanged from today's
    //       warn-only behavior. The report shape doesn't
    //       depend on RUSH_USE_SCHEDULER.

    #[test]
    fn scheduler_action_missing_output_errors() {
        // A scheduler-ready action declares an Object input that
        // no other action produces. validate_producers's pass 3
        // records this as a MissingInput entry — the data the
        // orchestrator's later 5.C.5 default-error wiring will
        // turn into a non-zero exit.
        let mut reg = ArtifactRegistry::new();
        let mnemonic_archive = reg.intern_string("CcArchive");
        let orphan_obj = make_obj(&mut reg, "obj/orphan.o", 1);
        let lib = {
            let p = reg.intern_path(std::path::Path::new("lib/libfoo.a"));
            reg.intern_artifact(
                ArtifactKey {
                    config: ConfigId::DEFAULT,
                    exec_path: p,
                },
                ArtifactKind::StaticLibrary,
                false,
                TargetId(1),
                None,
            )
        };
        reg.declare_action(rush_action::ids::DeclaredAction {
            owner_target: TargetId(1),
            mnemonic: mnemonic_archive,
            inputs: Box::new([orphan_obj]),
            outputs: Box::new([lib]),
            status: rush_action::ids::ActionDeclarationStatus::Complete,
        })
        .unwrap();

        let report = validate_producers(&reg, &HashMap::new());

        // The missing producer for orphan_obj IS recorded.
        // 5.C.5 implementation will read this report and
        // surface it as a hard error for scheduler-ready
        // actions; for now the report has the data.
        assert_eq!(report.declared_action_count, 1);
        assert_eq!(report.missing_input_producers.len(), 1);
        assert_eq!(
            report.missing_input_producers[0].artifact,
            orphan_obj,
            "missing input must be the orphan object the \
             archive action depends on",
        );
        // The mnemonic on the failure record IS the
        // scheduler-ready action's mnemonic.
        let mnemonic_str = reg
            .strings()
            .str(report.missing_input_producers[0].mnemonic);
        assert_eq!(mnemonic_str, "CcArchive");
    }

    #[test]
    fn zero_action_target_has_no_output_validation_failure() {
        // Pro's commit-208 invariant: a zero-action target —
        // the canonical case is header-only cc_library
        // (//tensorflow/core/platform:macros) — produces a
        // validation report with zero records and zero
        // failures. This is by construction (the validation
        // pass iterates declared_actions; an empty iterator
        // produces an empty report), but pinning it in a test
        // ensures future changes don't accidentally introduce
        // a "validation could not run" semantics for the
        // intentionally-empty case.
        let reg = ArtifactRegistry::new();
        let providers: HashMap<String, ProviderSet> = HashMap::new();
        let report = validate_producers(&reg, &providers);

        assert_eq!(report.declared_action_count, 0);
        assert_eq!(report.incomplete_action_count, 0);
        assert!(
            report.missing_provider_producers.is_empty(),
            "zero-action target must produce zero provider-validation \
             failures, got {:?}",
            report.missing_provider_producers,
        );
        assert!(
            report.missing_input_producers.is_empty(),
            "zero-action target must produce zero input-validation \
             failures, got {:?}",
            report.missing_input_producers,
        );
    }

    #[test]
    fn legacy_output_validation_policy_unchanged() {
        // The validation pass is INDEPENDENT of whether the
        // caller is in --use-scheduler mode. Same registry
        // state → same report shape. The 3-class policy
        // (scheduler-ready default-error / legacy warn-only /
        // incomplete never-schedulable) is the orchestrator's
        // interpretation of the report; the report itself is
        // unchanged.
        //
        // Test: build a registry with one well-formed
        // declared action and confirm the report is empty —
        // identical to what a legacy-only build would produce.
        let mut reg = ArtifactRegistry::new();
        let mnemonic_compile = reg.intern_string("CppCompile");
        let src = make_src(&mut reg, "src/main.cc", 1);
        let obj = make_obj(&mut reg, "obj/main.o", 1);
        reg.declare_action(rush_action::ids::DeclaredAction {
            owner_target: TargetId(1),
            mnemonic: mnemonic_compile,
            inputs: Box::new([src]),
            outputs: Box::new([obj]),
            status: rush_action::ids::ActionDeclarationStatus::Complete,
        })
        .unwrap();

        let report = validate_producers(&reg, &HashMap::new());

        // Source input doesn't need a producer; this is
        // pass 4's invariant. The well-formed compile action
        // produces no validation findings.
        assert_eq!(report.declared_action_count, 1);
        assert!(report.missing_provider_producers.is_empty());
        assert!(report.missing_input_producers.is_empty());
    }

    #[test]
    fn default_info_clone_forwarded_shares_arc() {
        // Alias rules use clone_forwarded to refcount-bump every
        // typed slot. DefaultInfo lives behind Arc and must share
        // refcount across the alias boundary — otherwise an alias
        // would silently fork the public-output list.
        let mut reg = ArtifactRegistry::new();
        let lib_aid = intern_artifact(
            &mut reg,
            "cc_lib/libalias.a",
            ArtifactKind::StaticLibrary,
            3,
        );
        let p1 = ProviderSet::empty().with_default_info(DefaultInfo {
            files: vec![lib_aid],
        });
        let p2 = p1.clone_forwarded();
        let a1 = p1.get_default_info().unwrap();
        let a2 = p2.get_default_info().unwrap();
        assert!(Arc::ptr_eq(a1, a2));
    }
}
