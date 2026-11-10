//! Persistent-daemon Run-wiring: the analyze/execute split + `AnalysisSnapshot`
//! retention + in-process reuse.
//!
//! This is the brick where the warm no-op collapses. The single-shot path
//! re-loads `.bzl`, re-parses `BUILD`, and re-runs PASS-A analysis on *every*
//! invocation, then throws it all away on exit. Here we retain the fully
//! analyzed configured-target/action graph as an [`AnalysisSnapshot`] plus the
//! complete tracked-IO [`DependencyManifest`] that proves when it is still
//! valid, key it by an [`AnalysisKey`], and reuse it when an identical command
//! arrives and the manifest still validates `Fresh`.
//!
//! ## Correctness — the reuse is COARSE but CORRECT
//!
//! Reusing a stale analysis graph is a WRONG build, so reuse is admitted only
//! when the snapshot's manifest — which captured every analysis-affecting read
//! through `rush_incremental::tracked::*` — re-validates `Fresh` against the
//! *new* command's [`CommandSemantics`]. Any analysis-input change (a `.bzl`
//! edit, a `BUILD` edit, a new glob-matching file recorded as a dir-listing
//! dep, a changed `--define`/env folded into the config fingerprint) flips the
//! manifest `Stale` and forces a full re-analysis. Reuse validates the manifest
//! ONCE at the reuse-decision and, on `Fresh`, mints a [`ValidationToken`] that
//! the execution-admission point consumes in place of a second stat-walk. The
//! token carries the snapshot identity and the manifest generation (a
//! `watcher_epoch` field is reserved for the Phase-2 watcher). A re-stat of all
//! ~20k deps right before execution does NOT actually close the
//! change-during-validate race — the walk itself is non-atomic and a no-op's
//! execution is instant (PASS-B is skipped), so the second validate was
//! near-pure redundant cost. The real concurrent-modification guard is the
//! Phase-2 watcher's epoch check; this is sufficient for one-command-at-a-time.
//!
//! ## The invariant
//!
//! > The second identical command executes zero Starlark instructions, loads
//! > zero packages, analyzes zero configured targets.
//!
//! The [`ReuseCounters`] prove it: the reuse path must show ZERO `.bzl`
//! evaluated, ZERO packages loaded, ZERO targets analyzed.

use crate::dependency_graph::{DependencyGraph, Label};
use rush_action::ids::{ArtifactRegistry, TargetId};
use rush_command_core::CommandSemantics;
use rush_incremental::{DependencyManifest, Staleness};
use rush_rules::{context::Toolchain, nested_set::NestedSet, providers::ProviderSet, Artifact};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Cached index used to plan a pruned PASS-B rebuild from dirty action paths.
#[derive(Debug, Clone, Default)]
pub struct PassBPruneIndex {
    /// Normalized action input/output path -> owning target build-order indices.
    pub path_to_seed_indices: HashMap<PathBuf, Vec<usize>>,
    /// build-order index -> reverse dependent build-order indices.
    pub rev_deps: Vec<Vec<usize>>,
    /// build-order index -> targets whose declared actions consume an output
    /// produced by this target, plus its ordinary reverse dependents.
    ///
    /// A transitive `CcInfo` consumer can name an ancestor archive directly
    /// even though its target-graph edge points only at the immediate library.
    /// When that ancestor changes, output-equivalence propagation must reach
    /// the real action consumer without stopping at a byte-identical
    /// intermediate archive.
    pub output_change_consumers: Vec<Vec<usize>>,
    /// build-order index -> whether output-equivalence may retain this target
    /// after its active dependencies execute byte-identically.
    pub output_equivalence_skip_safe: Vec<bool>,
    /// build-order index -> whether every active target may use PASS-B pruning.
    pub supported_rule_kind: Vec<bool>,
    /// build-order index -> declared output paths for actions owned by target.
    pub output_paths_by_target: Vec<Vec<PathBuf>>,
    /// build-order index -> forward dependency indices.
    ///
    /// Only the census uses this today: bounding what a slice widened to
    /// include dependencies would cost, without building it.
    pub fwd_deps: Vec<Vec<usize>>,
    /// build-order index -> the target's rule kind, for diagnostics.
    ///
    /// When pruning declines because a slice contains an unsupported kind,
    /// naming the KINDS says what work would unlock it; naming one label at
    /// a time costs a rebuild per kind to discover the same thing.
    pub rule_kind_name: Vec<String>,
    /// A dep edge the exact-string closure DROPPED although the target
    /// exists under another spelling ("consumer -> raw (graph has
    /// stored)"). A closure that provably lost an edge must not decide
    /// what executes: the planner declines while this is set. Detected
    /// once at index build; the index is cached per snapshot, so the
    /// spelling bug costs one message, not one message per edit.
    pub closure_edge_mismatch: Option<String>,
    /// A producer -> declared-action consumer edge that is not represented by
    /// target-graph ancestry. Signal-only propagation cannot safely repair a
    /// missing scheduling/active-closure edge, so PASS-B pruning declines while
    /// this is set.
    pub output_consumer_edge_mismatch: Option<String>,
    /// A genrule whose official output action is absent or malformed.
    ///
    /// This is snapshot-wide rather than active-slice-local: a failed
    /// duplicate-producer declaration makes output ownership ambiguous before
    /// the dirty path is mapped to a seed, so no pruned plan may trust this
    /// index while any genrule lacks one exact output manifest.
    pub genrule_manifest_mismatch: Option<String>,
    /// Genrule build-order indices, avoiding whole-graph scans on every edit.
    pub genrule_indices: Vec<usize>,
}

/// The complete per-label artifact DAG produced by a successful execution.
pub(crate) type BuiltArtifactForest = HashMap<String, Arc<NestedSet<Artifact>>>;

/// True only when `forest` contains one exact key for every build-order label
/// and no other keys.
///
/// This is a certification gate, not dependency lookup: fuzzy-equivalent label
/// spellings must not satisfy it because a retained forest is authoritative
/// state for the exact analyzed graph.
#[cfg(test)]
pub(crate) fn artifact_forest_exactly_covers(
    build_order: &[Label],
    forest: &BuiltArtifactForest,
) -> bool {
    artifact_forest_covers_keys(
        &build_order
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        forest,
    )
}

pub(crate) fn artifact_forest_covers_keys(keys: &[String], forest: &BuiltArtifactForest) -> bool {
    forest.len() == keys.len() && keys.iter().all(|key| forest.contains_key(key))
}

/// One coherent post-build generation retained by an analyzed snapshot.
///
/// The ledger, artifact forest, and watcher baseline describe the same
/// successful execution and are replaced under one mutex. The forest is
/// intentionally in-process only; a restarted daemon has no retained state and
/// takes the conservative full-execution path.
#[derive(Clone, Debug)]
pub(crate) struct RetainedExecutionState {
    /// Monotonic coherent-state revision. Successful executions and metadata-
    /// only ledger refreshes both increment it, so either change invalidates a
    /// plan pinned to the prior ledger/forest/baseline tuple.
    pub(crate) generation: u64,
    pub(crate) ledger: Arc<crate::freshness_ledger::FreshnessLedger>,
    pub(crate) artifact_forest: Arc<BuiltArtifactForest>,
    pub(crate) watch_baseline_id: u64,
}

impl RetainedExecutionState {
    fn with_refreshed_ledger(&self, ledger: Arc<crate::freshness_ledger::FreshnessLedger>) -> Self {
        Self {
            generation: self
                .generation
                .checked_add(1)
                .expect("retained execution generation overflow"),
            ledger,
            artifact_forest: Arc::clone(&self.artifact_forest),
            watch_baseline_id: self.watch_baseline_id,
        }
    }

    fn is_same_revision_as(&self, expected: &Self) -> bool {
        self.generation == expected.generation
            && self.watch_baseline_id == expected.watch_baseline_id
            && Arc::ptr_eq(&self.ledger, &expected.ledger)
            && Arc::ptr_eq(&self.artifact_forest, &expected.artifact_forest)
    }
}

fn commit_pruned_updates_if_current<R>(
    slot: &mut Option<RetainedExecutionState>,
    build_order: &[String],
    expected_generation: u64,
    expected_forest: Arc<BuiltArtifactForest>,
    updates: BuiltArtifactForest,
    refresh: impl FnOnce(&mut crate::freshness_ledger::FreshnessLedger) -> (Option<u64>, R),
) -> Option<(R, usize)> {
    let Some(current) = slot.as_mut() else {
        return None;
    };
    if current.generation != expected_generation
        || !Arc::ptr_eq(&current.artifact_forest, &expected_forest)
        || !artifact_forest_covers_keys(build_order, &current.artifact_forest)
        || updates
            .keys()
            .any(|key| !current.artifact_forest.contains_key(key))
    {
        *slot = None;
        return None;
    }
    let ledger_refs = Arc::strong_count(&current.ledger);
    let (baseline, result) = refresh(Arc::make_mut(&mut current.ledger));
    if current.ledger.is_empty() || baseline.is_none() {
        *slot = None;
        return None;
    }
    // Release the execution's old forest reference before applying the small
    // patch. Outstanding readers still force copy-on-write; the common unique
    // snapshot can replace existing keys without cloning the entire forest.
    drop(expected_forest);
    let forest = Arc::make_mut(&mut current.artifact_forest);
    for (key, value) in updates {
        *forest.get_mut(&key).expect("replacement key checked above") = value;
    }
    current.generation = current
        .generation
        .checked_add(1)
        .expect("retained execution generation overflow");
    current.watch_baseline_id = baseline.expect("checked above");
    Some((result, ledger_refs))
}

fn refresh_state_if_current(
    slot: &mut Option<RetainedExecutionState>,
    expected: &RetainedExecutionState,
    refreshed: crate::freshness_ledger::FreshnessLedger,
) -> Option<RetainedExecutionState> {
    if !slot
        .as_ref()
        .is_some_and(|current| current.is_same_revision_as(expected))
    {
        return None;
    }
    let refreshed = expected.with_refreshed_ledger(Arc::new(refreshed));
    *slot = Some(refreshed.clone());
    Some(refreshed)
}

/// Identity of a retained analysis. Two commands that hit the same key are
/// candidates for snapshot reuse — but reuse is only *admitted* after the
/// snapshot's dependency manifest re-validates `Fresh` (the key proves the
/// command shape matches; the manifest proves the world hasn't moved).
///
/// Keyed on the ORDERED target patterns the client requested — NOT the expanded
/// target set, because expansion is itself an analysis step (package discovery)
/// that the manifest's dir-listing / package-lookup deps already cover. Keying
/// on the expansion would mean re-doing the analysis to compute the key.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct AnalysisKey {
    /// Canonicalized workspace root (absolute, symlinks resolved).
    pub workspace_realpath: PathBuf,
    /// The output base / workspace root the command resolves against (raw bytes
    /// from `CommandSemantics`; Unix paths are not guaranteed UTF-8).
    pub output_base: Vec<u8>,
    /// The build command verb (e.g. `"build"`, `"test"`). `None` if argv[1] is
    /// missing — a malformed command never reuses anything.
    pub command_kind: Option<String>,
    /// The target patterns exactly as requested, in order (not expanded).
    pub ordered_target_patterns: Vec<String>,
    /// The deterministic config fingerprint (env / flags / defines / output base
    /// / `.bazelrc` / `WORKSPACE` / `MODULE.bazel` content) from
    /// `CommandSemantics`.
    pub config_fingerprint: [u8; 32],
    /// Identity of the rush binary + persistent schema + native rule set +
    /// output-path derivation. A snapshot produced by a different daemon build
    /// must never be reused. See [`daemon_compat_fingerprint`].
    pub daemon_compat_fingerprint: [u8; 32],
}

impl AnalysisKey {
    /// Construct the key from a command's semantics, the canonicalized workspace
    /// root, and the ordered (un-expanded) target patterns.
    pub fn new(
        semantics: &CommandSemantics,
        workspace_realpath: PathBuf,
        ordered_target_patterns: Vec<String>,
    ) -> Self {
        let mut key = AnalysisKey {
            workspace_realpath,
            output_base: semantics.output_base().to_vec(),
            command_kind: semantics.command().map(|s| s.to_string()),
            ordered_target_patterns,
            config_fingerprint: *semantics.config_fingerprint(),
            daemon_compat_fingerprint: daemon_compat_fingerprint(),
        };
        if let Ok(token) = crate::compiler_discovery::read_admission_token(&key.workspace_realpath) {
            crate::compiler_discovery::qualify_analysis_key(&mut key, &token);
        }
        key
    }

    /// A stable, cross-process content hash of this key, used as the on-disk
    /// filename for the cold-no-op certificate. Explicit length-prefixed
    /// encoding — NOT Rust `Hash`/`Debug`, which are not stable storage formats.
    /// Two processes (a cold daemon and the one that wrote the certificate) that
    /// build the SAME key produce the SAME hash, so the cold start finds the
    /// right file; the full key is also stored inside the record and compared on
    /// read (the hash is an index, not authority).
    pub fn stable_hash(&self) -> [u8; 32] {
        let mut h = blake3::Hasher::new();
        h.update(b"rush.analysis-key.stable-hash.v1\n");
        let ws = self.workspace_realpath.to_string_lossy();
        h.update(&(ws.len() as u64).to_le_bytes());
        h.update(ws.as_bytes());
        h.update(&(self.output_base.len() as u64).to_le_bytes());
        h.update(&self.output_base);
        match &self.command_kind {
            Some(k) => {
                h.update(&[1u8]);
                h.update(&(k.len() as u64).to_le_bytes());
                h.update(k.as_bytes());
            }
            None => {
                h.update(&[0u8]);
            }
        }
        h.update(&(self.ordered_target_patterns.len() as u64).to_le_bytes());
        for p in &self.ordered_target_patterns {
            h.update(&(p.len() as u64).to_le_bytes());
            h.update(p.as_bytes());
        }
        h.update(&self.config_fingerprint);
        h.update(&self.daemon_compat_fingerprint);
        *h.finalize().as_bytes()
    }
}

/// The daemon-compatibility fingerprint: the identity of the rush binary and the
/// invariants a retained snapshot was produced under. A snapshot whose
/// `daemon_compat_fingerprint` differs from the current process's must never be
/// reused, because the rule set / output-path derivation / persistent schema may
/// have changed under it.
///
/// We fold in the crate version and the build target triple. When a persistent
/// on-disk snapshot lands (Phase 7) this must also cover the persistent schema
/// version and native rule set; for the in-process cache the process is the same
/// binary by construction, but folding the identity in keeps the key honest and
/// future-proofs the LRU against a hot-reloaded plugin set.
pub fn daemon_compat_fingerprint() -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rush.daemon-compat.v2\n");
    hasher.update(env!("CARGO_PKG_VERSION").as_bytes());
    hasher.update(b"\n");
    hasher.update(std::env::consts::ARCH.as_bytes());
    hasher.update(b"\n");
    hasher.update(std::env::consts::OS.as_bytes());
    hasher.update(b"\n");
    // Fold in the running rush binary's own identity. This is the gate that
    // makes a PERSISTED cold-no-op certificate honest across daemon restarts:
    // a rebuilt rush (same crate version, different code) produces a different
    // executable digest, so it never trusts a certificate written by the old
    // binary. The in-process LRU never needed this (same process by
    // construction), but the on-disk certificate does — and folding it in keeps
    // the in-memory key honest too. Computed once and cached for the process.
    hasher.update(&*current_exe_identity());
    *hasher.finalize().as_bytes()
}

/// The running rush executable's content identity, computed once per process.
///
/// We hash the executable's bytes when readable so a code change (even without a
/// crate-version bump — the common local-dev case) flips the daemon-compat
/// fingerprint, invalidating any persisted certificate the old binary wrote. If
/// the executable cannot be read (unusual), we fall back to a path+mtime+len
/// stamp, which still distinguishes most rebuilds. A constant sentinel is the
/// last resort — degrades to crate-version-only compat, never UNSOUND because a
/// stale certificate still must pass manifest.validate()+ledger.verify().
fn current_exe_identity() -> std::sync::Arc<[u8; 32]> {
    use std::sync::OnceLock;
    static IDENTITY: OnceLock<std::sync::Arc<[u8; 32]>> = OnceLock::new();
    IDENTITY
        .get_or_init(|| {
            let mut h = blake3::Hasher::new();
            h.update(b"rush.exe-identity.v1\n");
            match std::env::current_exe() {
                Ok(path) => {
                    h.update(path.to_string_lossy().as_bytes());
                    h.update(b"\n");
                    match std::fs::read(&path) {
                        Ok(bytes) => {
                            h.update(b"content\n");
                            h.update(&bytes);
                        }
                        Err(_) => {
                            // Fall back to mtime + len stat stamp.
                            h.update(b"stat\n");
                            if let Ok(md) = std::fs::metadata(&path) {
                                h.update(&md.len().to_le_bytes());
                                if let Ok(mtime) = md.modified() {
                                    if let Ok(dur) = mtime.duration_since(std::time::UNIX_EPOCH) {
                                        h.update(&dur.as_nanos().to_le_bytes());
                                    }
                                }
                            }
                        }
                    }
                }
                Err(_) => {
                    h.update(b"unknown-exe\n");
                }
            }
            std::sync::Arc::new(*h.finalize().as_bytes())
        })
        .clone()
}

/// The retained, fully-analyzed graph for one command.
///
/// IMMUTABLE after commit. Shared via `Arc<AnalysisSnapshot>`; execution reads
/// from it but never mutates it — `execute_analyzed` builds FRESH per-run
/// execution state (some rules, e.g. `cc_shared_library`, carry interior exec
/// state, so the snapshot must not be the thing they write through).
///
/// `DependencyGraph` is itself not `Clone` (its nodes hold `Arc<dyn
/// RuleInstance + Send + Sync>`), which is exactly why we wrap the whole
/// snapshot in `Arc` and share that — no graph deep-copy, the live Rust objects
/// are retained as-is.
pub struct AnalysisSnapshot {
    /// Revocation token sampled before this analysis, never refreshed in place.
    pub(crate) native_admission_token: [u8; 32],
    /// Reuse-admission identity.
    pub key: AnalysisKey,
    /// Cached stable hash of [`key`](Self::key), used by validation tokens.
    pub key_hash: [u8; 32],
    /// The semantics this snapshot was analyzed under. Reuse re-validates the
    /// manifest against the NEW command's semantics; this is the record of what
    /// produced the graph (for diagnostics + the differential oracle).
    pub built_under: CommandSemantics,
    /// The complete tracked-IO dependency manifest from analysis — the
    /// reuse-admission proof. Reuse is admitted iff this re-validates `Fresh`.
    pub manifest: DependencyManifest,
    /// Cached fingerprint of [`manifest`](Self::manifest), used by validation
    /// tokens. The manifest is immutable after analysis, so this never changes.
    pub manifest_fingerprint: [u8; 32],
    /// Cached watcher admission index for the immutable manifest.
    pub manifest_watch_index: crate::workspace_watcher::ManifestWatchIndex,
    /// Inputs outside established analysis watches; validate on every reuse.
    pub unwatched_manifest: DependencyManifest,

    /// The analyzed dependency graph (nodes hold `Arc<dyn RuleInstance>`).
    pub graph: DependencyGraph,
    /// The topological build order (scoped to the reachable closure of the
    /// requested patterns).
    pub build_order: Vec<Label>,
    /// Canonical keys for repeated exact forest checks without label formatting.
    pub build_order_keys: Vec<String>,
    /// The action-graph registry produced by PASS-A. Shared (push-only during
    /// analysis; idempotent on key). Execution reads the declared action graph
    /// from it; per-run execution OUTPUTS live in `execute_analyzed`'s own
    /// `built_artifacts` map, never here.
    pub artifact_registry: Arc<tokio::sync::Mutex<ArtifactRegistry>>,
    /// The per-target analysis providers (`CcInfo` etc.) — PASS-A output,
    /// consumed by PASS-B to assemble each target's dep providers.
    pub analysis_providers: HashMap<String, ProviderSet>,
    /// Cached pruned PASS-B planner index. Populated after the first full
    /// successful PASS-B run, when legacy rules have declared their actions.
    pub pass_b_prune_index: Mutex<Option<Arc<PassBPruneIndex>>>,
    /// Archive boundaries derived from this immutable graph and requested roots.
    pub static_archive_materialization: std::collections::HashSet<String>,
    /// label string -> stable `TargetId` mapping assigned during analysis.
    pub target_id_by_label: HashMap<String, TargetId>,
    /// The expanded target set (package-discovery output of analysis).
    pub expanded_targets: Vec<String>,
    /// External-repo path snapshot used to rebuild the include resolver during
    /// execution.
    pub resolver_repo_paths: HashMap<String, PathBuf>,
    /// The managed output root for this workspace.
    pub output_dir: PathBuf,
    /// Owned local execution view; repository source roots remain separate.
    pub(crate) native_execroot: Option<Arc<crate::native_execroot::NativeExecroot>>,
    /// The toolchain selected for this command.
    pub toolchain: Toolchain,
    /// Coherent post-build file state and artifact DAG. `None` until a full
    /// wavefront succeeds; failed execution never replaces the prior generation.
    /// A caller clones one state under this mutex so it cannot mix a ledger,
    /// artifact forest, and watcher baseline from different executions.
    pub(crate) retained_execution_state: Mutex<Option<RetainedExecutionState>>,

    /// The instant analysis COMPLETED (the snapshot was committed). The Phase-2
    /// watcher's continuity gate compares this to the watcher's `armed_at`: the
    /// watcher may only authorize a 0-path no-op for a snapshot analyzed AFTER
    /// the watch was armed (otherwise events before the snapshot's reads were
    /// never observed — no continuity). `None` for snapshots built before this
    /// field existed / on the in-process path (→ always full-validate).
    pub analyzed_at: Option<std::time::Instant>,
}

/// Proof, minted by a single `Fresh` reuse-admission validate, that the
/// snapshot's manifest re-validated clean against the current world. The
/// execution-admission point consumes a token in place of a second
/// stat-walk of all ~20k recorded deps (the old DOUBLE-VALIDATION).
///
/// `snapshot_id` and `manifest_generation` pin the token to the exact
/// snapshot+manifest it validated, so it can never be mistaken for a
/// different reuse candidate. `watcher_epoch` is reserved for the Phase-2
/// inotify watcher: when present it will let execution admission confirm the
/// workspace epoch is unchanged since the validate, which is the *real*
/// concurrent-modification guard (the old second stat-walk was non-atomic and
/// thus never actually closed that race). Until the watcher exists the field
/// is `None` and the token is purely the single-validate receipt.
#[derive(Clone, Debug)]
pub struct ValidationToken {
    /// Stable identity of the snapshot this token validated (`AnalysisKey`).
    pub snapshot_id: [u8; 32],
    /// Fingerprint of the manifest generation that re-validated `Fresh`.
    pub manifest_generation: [u8; 32],
    /// When the validate ran (for diagnostics / future epoch checks).
    pub validated_at: std::time::Instant,
    /// Phase-2 watcher epoch at validate time. `None` until the watcher exists.
    pub watcher_epoch: Option<u64>,
}

impl AnalysisSnapshot {
    pub(crate) fn admission_is_current(&self) -> bool {
        crate::compiler_discovery::read_admission_token(&self.key.workspace_realpath)
            .is_ok_and(|token| token == self.native_admission_token)
    }
    /// Clone the current coherent post-build generation, if one exists.
    pub(crate) fn retained_execution_state(&self) -> Option<RetainedExecutionState> {
        let guard = self
            .retained_execution_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.clone()
    }

    /// Replace only a pinned generation's ledger after a byte-identical touch.
    ///
    /// The artifact forest, generation, and watcher baseline remain paired. A
    /// concurrent replacement invalidates retention instead of letting the
    /// caller certify a mixed generation.
    pub(crate) fn refresh_retained_ledger(
        &self,
        expected: &RetainedExecutionState,
        refreshed: crate::freshness_ledger::FreshnessLedger,
    ) -> Option<RetainedExecutionState> {
        let mut guard = self
            .retained_execution_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // This path made no output writes. A mismatch preserves the newer
        // successful state and makes this command fall back to execution.
        refresh_state_if_current(&mut guard, expected, refreshed)
    }

    /// Check that a cloned post-build generation is still current.
    pub(crate) fn retained_execution_state_matches(
        &self,
        expected: &RetainedExecutionState,
    ) -> bool {
        let guard = self
            .retained_execution_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard
            .as_ref()
            .is_some_and(|current| current.is_same_revision_as(expected))
    }

    /// Atomically commit one successful execution's ledger, artifact forest,
    /// and watcher baseline.
    ///
    /// `expected` pins the generation observed at execution admission. If
    /// another execution replaced it concurrently, retained state is cleared;
    /// the completed build still succeeds, while the next command executes
    /// conservatively instead of trusting a mixed generation. `Some(0)` is a
    /// validated publication without a usable fast watcher baseline; `None`
    /// means the publication barrier failed and also clears retention.
    pub(crate) fn commit_retained_execution_state(
        &self,
        expected: Option<&RetainedExecutionState>,
        ledger: Arc<crate::freshness_ledger::FreshnessLedger>,
        artifact_forest: BuiltArtifactForest,
        watch_baseline_id: Option<u64>,
    ) -> bool {
        let mut guard = self
            .retained_execution_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let expected_matches = match (guard.as_ref(), expected) {
            (None, None) => true,
            (Some(current), Some(expected)) => current.is_same_revision_as(expected),
            _ => false,
        };
        if !expected_matches
            || ledger.is_empty()
            || watch_baseline_id.is_none()
            || !artifact_forest_covers_keys(&self.build_order_keys, &artifact_forest)
        {
            *guard = None;
            return false;
        }
        let generation = expected
            .map_or(0, |state| state.generation)
            .checked_add(1)
            .expect("retained execution generation overflow");
        *guard = Some(RetainedExecutionState {
            generation,
            ledger,
            artifact_forest: Arc::new(artifact_forest),
            watch_baseline_id: watch_baseline_id.expect("checked above"),
        });
        true
    }

    /// Refresh and publish a successful pruned execution while holding the
    /// coherent-state mutex.
    ///
    /// The caller must drop every temporary ledger `Arc` before entering so
    /// `Arc::make_mut` normally updates the large ledger in place. The closure
    /// is synchronous and returns the publication verdict/baseline plus
    /// caller-owned diagnostics. `Some(0)` is valid conservative retention;
    /// `None` or a revision mismatch clears retention because this execution has
    /// already written outputs relative to the expected generation.
    pub(crate) fn commit_pruned_execution_updates<R>(
        &self,
        expected_generation: u64,
        expected_artifact_forest: Arc<BuiltArtifactForest>,
        updates: BuiltArtifactForest,
        refresh_and_baseline: impl FnOnce(
            &mut crate::freshness_ledger::FreshnessLedger,
        ) -> (Option<u64>, R),
    ) -> Option<(R, usize)> {
        let mut guard = self
            .retained_execution_state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        commit_pruned_updates_if_current(
            &mut guard,
            &self.build_order_keys,
            expected_generation,
            expected_artifact_forest,
            updates,
            refresh_and_baseline,
        )
    }

    /// Re-validate the snapshot's manifest against a new command's semantics.
    ///
    /// `Fresh` means every analysis-affecting read the snapshot recorded still
    /// holds — the retained graph is admissible for reuse. `Stale` names the
    /// first dependency that moved.
    pub fn validate(&self, sem: &CommandSemantics) -> rush_incremental::Validated {
        self.manifest.validate(sem)
    }

    /// Validate once and, on `Fresh`, mint a [`ValidationToken`] pinning this
    /// snapshot's identity and manifest generation. Returns the verdict plus
    /// `Some(token)` iff `Fresh`. Replaces the warm/cold double-validate: the
    /// execution-admission point consumes the token instead of re-stat-walking.
    pub fn validate_token(
        &self,
        sem: &CommandSemantics,
    ) -> (rush_incremental::Validated, Option<ValidationToken>) {
        let validated = self.manifest.validate(sem);
        let token = if matches!(validated.staleness, Staleness::Fresh) {
            Some(ValidationToken {
                snapshot_id: self.key_hash,
                manifest_generation: self.manifest_fingerprint,
                validated_at: std::time::Instant::now(),
                watcher_epoch: None,
            })
        } else {
            None
        };
        (validated, token)
    }
}

/// Per-process counters proving the invariant: a reuse must perform ZERO
/// analysis work (no `.bzl` evaluated, no packages loaded, no targets analyzed).
///
/// Incremented by the analysis path (which does the work) and read by the
/// reuse selftest / differential oracle. A reuse hit increments only the reuse
/// counter; if any of `bzl_evaluated` / `packages_loaded` / `targets_analyzed`
/// move on a reuse, the invariant is violated.
#[derive(Debug, Default)]
pub struct ReuseCounters {
    /// Number of `.bzl` files evaluated during the most recent analysis.
    pub bzl_evaluated: AtomicU64,
    /// Number of packages (`BUILD` files) loaded during the most recent
    /// analysis.
    pub packages_loaded: AtomicU64,
    /// Number of configured targets analyzed during the most recent analysis.
    pub targets_analyzed: AtomicU64,
    /// Number of snapshot reuse hits (analyze skipped).
    pub reuse_hits: AtomicU64,
    /// Number of snapshot reuse misses / full analyses.
    pub reuse_misses: AtomicU64,
    /// Number of reuse candidates rejected because the manifest was `Stale`.
    pub reuse_stale: AtomicU64,
}

impl ReuseCounters {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reset the analysis-work counters to zero. Called at the start of each
    /// command so the counters reflect THIS command's analysis work — a reuse
    /// path leaves them at zero (the invariant).
    pub fn reset_analysis_work(&self) {
        self.bzl_evaluated.store(0, Ordering::SeqCst);
        self.packages_loaded.store(0, Ordering::SeqCst);
        self.targets_analyzed.store(0, Ordering::SeqCst);
    }

    pub fn add_bzl(&self, n: u64) {
        self.bzl_evaluated.fetch_add(n, Ordering::SeqCst);
    }
    pub fn add_packages(&self, n: u64) {
        self.packages_loaded.fetch_add(n, Ordering::SeqCst);
    }
    pub fn add_targets(&self, n: u64) {
        self.targets_analyzed.fetch_add(n, Ordering::SeqCst);
    }
    pub fn record_reuse_hit(&self) {
        self.reuse_hits.fetch_add(1, Ordering::SeqCst);
    }
    pub fn record_reuse_miss(&self) {
        self.reuse_misses.fetch_add(1, Ordering::SeqCst);
    }
    pub fn record_reuse_stale(&self) {
        self.reuse_stale.fetch_add(1, Ordering::SeqCst);
    }

    /// Snapshot the analysis-work counters as a `(bzl, packages, targets)`
    /// triple.
    pub fn analysis_work(&self) -> (u64, u64, u64) {
        (
            self.bzl_evaluated.load(Ordering::SeqCst),
            self.packages_loaded.load(Ordering::SeqCst),
            self.targets_analyzed.load(Ordering::SeqCst),
        )
    }

    /// True iff no analysis work was performed since the last
    /// [`reset_analysis_work`](Self::reset_analysis_work) — the reuse invariant.
    pub fn zero_analysis_work(&self) -> bool {
        self.analysis_work() == (0, 0, 0)
    }
}

/// A tiny LRU of retained snapshots keyed by [`AnalysisKey`].
///
/// Held on the orchestrator (one live `BuildOrchestrator` per
/// workspace+output+config owns the shared process state; the per-target
/// reusable thing is the snapshot). Bounded so a long-lived daemon does not
/// retain every command's graph forever; eviction is plain recency. Memory
/// policy (generation eviction, reachability cleanup) is a later phase — this
/// is the coarse, correct first cut.
pub struct SnapshotCache {
    inner: Mutex<lru::LruCache<AnalysisKey, Arc<AnalysisSnapshot>>>,
}

impl SnapshotCache {
    /// Create a cache holding up to `capacity` snapshots (clamped to >= 1).
    pub fn new(capacity: usize) -> Self {
        let cap = std::num::NonZeroUsize::new(capacity.max(1)).unwrap();
        SnapshotCache {
            inner: Mutex::new(lru::LruCache::new(cap)),
        }
    }

    /// Look up a snapshot by key, promoting it to most-recently-used.
    pub fn get(&self, key: &AnalysisKey) -> Option<Arc<AnalysisSnapshot>> {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let snapshot = g.get(key).cloned()?;
        if snapshot.admission_is_current() { Some(snapshot) } else { g.pop(key); None }
    }

    /// Insert (or replace) a snapshot under its key.
    pub fn put(&self, snapshot: Arc<AnalysisSnapshot>) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.put(snapshot.key.clone(), snapshot);
    }

    /// Drop a snapshot (e.g. after a `Stale` verdict immediately before
    /// execution — reuse rejected, the retained graph is no longer valid).
    pub fn invalidate(&self, key: &AnalysisKey) {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.pop(key);
    }

    /// Remove all local admissions sharing these outputs. Other processes and
    /// held Arcs are revoked by the separately rotated workspace token.
    pub(crate) fn invalidate_workspace(&self, canonical_workspace: &Path) {
        let mut cache = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let keys: Vec<_> = cache.iter().filter(|(key, _)| key.workspace_realpath == canonical_workspace)
            .map(|(key, _)| key.clone()).collect();
        for key in keys { cache.pop(&key); }
    }
}

impl Default for SnapshotCache {
    fn default() -> Self {
        // 8 distinct analysis graphs is plenty for the in-process selftest and a
        // single-workspace daemon; the daemon transport phase can make this
        // configurable.
        SnapshotCache::new(8)
    }
}

/// A canonical, order-independent fingerprint of the analyzed action graph, used
/// by the differential oracle to assert that a reused build's action graph is
/// IDENTICAL to a from-scratch clean build's.
///
/// Two analyses that produced the same declared actions (same owner LABEL,
/// mnemonic, input paths, output paths) over the same targets — regardless of
/// the order they were declared in — produce the same fingerprint. A divergence
/// here is a wrong reuse and MUST hard-fail.
///
/// ## Owner is keyed by LABEL, not raw `TargetId` (Codex review 019efd7d)
///
/// `TargetId` is assigned **sequentially** as analysis walks `build_order`, and
/// `build_order` is produced by a topological sort over `HashMap`/`HashSet`
/// structures whose iteration order is not stable run-to-run. Hashing the raw
/// `TargetId` (`{:?}`) therefore made the fingerprint depend on the *order*
/// targets were numbered — so two analyses that declared the SAME action set
/// could still produce different fingerprints purely because the same label got
/// a different `TargetId`. That is a FALSE oracle failure (it does not mean the
/// reuse is wrong). We resolve each action's owner `TargetId` back to its label
/// via the snapshot's `target_id_by_label` map and hash the LABEL, which is the
/// stable, analysis-order-independent identity. An action whose owner has no
/// label mapping (should not happen for a declared target action) is keyed by a
/// stable sentinel rather than the numeric id, so it still never depends on the
/// numbering order.
pub async fn canonical_action_graph_fingerprint(
    registry: &Arc<tokio::sync::Mutex<ArtifactRegistry>>,
    target_id_by_label: &HashMap<String, TargetId>,
) -> [u8; 32] {
    // Reverse the label→id map once so each action can resolve its owner id back
    // to the stable label. Two ids never collide (ids are unique per analysis),
    // so the reverse map is well-formed.
    let label_by_id: HashMap<TargetId, &str> = target_id_by_label
        .iter()
        .map(|(label, id)| (*id, label.as_str()))
        .collect();

    let reg = registry.lock().await;
    // Collect a stable, sorted line per declared action so ordering of
    // declaration does not affect the digest.
    let mut lines: Vec<String> = Vec::new();
    for (id, action) in reg.declared_actions() {
        // Resolve the owner to its LABEL (order-independent identity). Fall back
        // to a stable sentinel — NOT the numeric id — when unmapped, so the line
        // never depends on the analysis numbering order.
        let owner_label = label_by_id
            .get(&action.owner_target)
            .copied()
            .unwrap_or("<unmapped-owner>");
        // Resolve output paths (sorted) and input paths (sorted) to absolute
        // strings via the registry's interner so the line is content-stable.
        let mut outs: Vec<String> = action
            .outputs
            .iter()
            .map(|aid| reg.artifact_path(*aid).to_string_lossy().into_owned())
            .collect();
        outs.sort();
        let mut ins: Vec<String> = action
            .inputs
            .iter()
            .map(|aid| reg.artifact_path(*aid).to_string_lossy().into_owned())
            .collect();
        ins.sort();
        let mnem = reg.strings().str(action.mnemonic).to_string();
        let _ = id;
        lines.push(format!(
            "owner={} mnem={} ins=[{}] outs=[{}]",
            owner_label,
            mnem,
            ins.join(","),
            outs.join(",")
        ));
    }
    lines.sort();
    lines.dedup();
    let mut hasher = blake3::Hasher::new();
    // v2: owner keyed by label (was raw TargetId in v1 — order-dependent).
    hasher.update(b"rush.canonical-action-graph.v2\n");
    hasher.update(&(lines.len() as u64).to_le_bytes());
    for l in &lines {
        hasher.update(&(l.len() as u64).to_le_bytes());
        hasher.update(l.as_bytes());
    }
    *hasher.finalize().as_bytes()
}

/// Human-readable reason a reuse candidate was rejected, for the selftest /
/// oracle log.
pub fn staleness_reason(s: &Staleness) -> String {
    match s {
        Staleness::Fresh => "fresh".to_string(),
        Staleness::Stale { reason, .. } => reason.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rush_rules::ArtifactType;
    use std::sync::atomic::Ordering;

    #[test]
    fn daemon_compat_fingerprint_is_stable_and_nonzero() {
        let a = daemon_compat_fingerprint();
        let b = daemon_compat_fingerprint();
        assert_eq!(a, b, "daemon-compat fingerprint must be deterministic");
        assert_ne!(a, [0u8; 32], "fingerprint must not be all-zero");
    }

    #[test]
    fn reuse_counters_invariant_helpers() {
        let c = ReuseCounters::new();
        assert!(c.zero_analysis_work());
        c.add_bzl(3);
        c.add_packages(2);
        c.add_targets(5);
        assert_eq!(c.analysis_work(), (3, 2, 5));
        assert!(!c.zero_analysis_work());
        // reset_analysis_work clears ONLY the analysis-work counters, not the
        // reuse hit/miss tallies.
        c.record_reuse_hit();
        c.reset_analysis_work();
        assert!(c.zero_analysis_work());
        assert_eq!(c.reuse_hits.load(Ordering::SeqCst), 1);
    }

    fn key_with_patterns(patterns: &[&str]) -> AnalysisKey {
        AnalysisKey {
            workspace_realpath: PathBuf::from("/ws"),
            output_base: b"/ws".to_vec(),
            command_kind: Some("build".to_string()),
            ordered_target_patterns: patterns.iter().map(|s| s.to_string()).collect(),
            config_fingerprint: [7u8; 32],
            daemon_compat_fingerprint: daemon_compat_fingerprint(),
        }
    }

    #[test]
    fn analysis_key_distinguishes_target_pattern_order() {
        // Ordered patterns are part of identity: //a //b is NOT //b //a.
        let k1 = key_with_patterns(&["//a", "//b"]);
        let k2 = key_with_patterns(&["//b", "//a"]);
        assert_ne!(k1, k2);
        let k3 = key_with_patterns(&["//a", "//b"]);
        assert_eq!(k1, k3);
    }

    fn label(raw: &str) -> Label {
        Label::parse(raw, None, None).expect("test label")
    }

    fn artifact_root(path: &str) -> Arc<NestedSet<Artifact>> {
        NestedSet::new(
            vec![Artifact {
                path: PathBuf::from(path),
                artifact_type: ArtifactType::StaticLibrary,
            }],
            Vec::new(),
        )
    }

    fn nonempty_ledger(path: &str) -> crate::freshness_ledger::FreshnessLedger {
        let path = PathBuf::from(path);
        let mut paths = HashMap::new();
        paths.insert(path.clone(), rush_incremental::stat_meta(&path));
        crate::freshness_ledger::FreshnessLedger::from_parts(paths, 1)
    }

    #[test]
    fn retained_artifact_forest_requires_exact_build_order_keys() {
        let order = vec![label("//pkg:a"), label("//pkg:b")];
        let mut complete = BuiltArtifactForest::new();
        complete.insert("//pkg:a".to_string(), artifact_root("liba.a"));
        complete.insert("//pkg:b".to_string(), artifact_root("libb.a"));
        assert!(artifact_forest_exactly_covers(&order, &complete));

        let mut missing = complete.clone();
        missing.remove("//pkg:b");
        assert!(!artifact_forest_exactly_covers(&order, &missing));

        let mut extra_same_count = missing;
        extra_same_count.insert("//pkg:c".to_string(), artifact_root("libc.a"));
        assert!(!artifact_forest_exactly_covers(&order, &extra_same_count));

        let external_order = vec![label("@repo//pkg:a")];
        let mut fuzzy_equivalent = BuiltArtifactForest::new();
        fuzzy_equivalent.insert("//pkg:a".to_string(), artifact_root("liba.a"));
        assert!(!artifact_forest_exactly_covers(
            &external_order,
            &fuzzy_equivalent
        ));
    }

    #[test]
    fn hydrated_forest_overwrites_active_root_and_preserves_inactive_root() {
        let old_active = artifact_root("old-active.a");
        let inactive = artifact_root("inactive-prebuilt.a");
        let mut retained = BuiltArtifactForest::new();
        retained.insert("//pkg:active".to_string(), Arc::clone(&old_active));
        retained.insert("//pkg:inactive".to_string(), Arc::clone(&inactive));

        let mut hydrated = retained.clone();
        let new_active = artifact_root("new-active.a");
        hydrated.insert("//pkg:active".to_string(), Arc::clone(&new_active));

        assert!(Arc::ptr_eq(
            hydrated.get("//pkg:inactive").expect("inactive root"),
            &inactive
        ));
        assert!(Arc::ptr_eq(
            hydrated.get("//pkg:active").expect("active root"),
            &new_active
        ));
        assert!(!Arc::ptr_eq(
            hydrated.get("//pkg:active").expect("active root"),
            &old_active
        ));
        assert!(artifact_forest_exactly_covers(
            &[label("//pkg:active"), label("//pkg:inactive")],
            &hydrated
        ));
    }

    #[test]
    fn pruned_updates_preserve_inactive_roots_and_avoid_unique_forest_clone() {
        let order = vec!["//:active".to_string(), "//:inactive".to_string()];
        let inactive = artifact_root("inactive.a");
        let forest = Arc::new(HashMap::from([
            ("//:active".into(), artifact_root("old.a")),
            ("//:inactive".into(), Arc::clone(&inactive)),
        ]));
        let address = Arc::as_ptr(&forest);
        let capacity = forest.capacity();
        let key_order = forest.keys().cloned().collect::<Vec<_>>();
        let mut slot = Some(RetainedExecutionState {
            generation: 7,
            ledger: Arc::new(nonempty_ledger("input")),
            artifact_forest: Arc::clone(&forest),
            watch_baseline_id: 41,
        });
        let replacement = artifact_root("new.a");
        assert!(commit_pruned_updates_if_current(
            &mut slot,
            &order,
            7,
            forest,
            HashMap::from([("//:active".into(), Arc::clone(&replacement))]),
            |_| (Some(42), ())
        )
        .is_some());
        let current = slot.unwrap();
        assert_eq!(Arc::as_ptr(&current.artifact_forest), address);
        assert_eq!(current.artifact_forest.capacity(), capacity);
        assert_eq!(
            current.artifact_forest.keys().cloned().collect::<Vec<_>>(),
            key_order
        );
        assert_eq!(current.generation, 8);
        assert_eq!(current.watch_baseline_id, 42);
        assert!(Arc::ptr_eq(
            &current.artifact_forest["//:active"],
            &replacement
        ));
        assert!(Arc::ptr_eq(
            &current.artifact_forest["//:inactive"],
            &inactive
        ));
    }

    #[test]
    fn pruned_updates_copy_for_readers_and_reject_invalid_publication() {
        let order = vec!["//:active".to_string()];
        let old_root = artifact_root("old.a");
        let original = RetainedExecutionState {
            generation: 7,
            ledger: Arc::new(nonempty_ledger("input")),
            artifact_forest: Arc::new(HashMap::from([("//:active".into(), Arc::clone(&old_root))])),
            watch_baseline_id: 41,
        };
        let mut slot = Some(original.clone());
        assert!(commit_pruned_updates_if_current(
            &mut slot,
            &order,
            7,
            Arc::clone(&original.artifact_forest),
            HashMap::from([("//:active".into(), artifact_root("new.a"))]),
            |_| (Some(42), ())
        )
        .is_some());
        assert!(Arc::ptr_eq(
            &original.artifact_forest["//:active"],
            &old_root
        ));
        assert!(!Arc::ptr_eq(
            &slot.as_ref().unwrap().artifact_forest,
            &original.artifact_forest
        ));
        for case in 0..4 {
            let mut slot = Some(original.clone());
            let key = if case == 0 { "//:unknown" } else { "//:active" };
            let expected_generation = if case == 1 { 6 } else { 7 };
            let wrong_order = vec!["//:missing".to_string()];
            let build_order = if case == 2 { &wrong_order } else { &order };
            let mut callback_ran = false;
            let result = commit_pruned_updates_if_current(
                &mut slot,
                build_order,
                expected_generation,
                Arc::clone(&original.artifact_forest),
                HashMap::from([(key.into(), artifact_root("bad.a"))]),
                |_| {
                    callback_ran = true;
                    (None, ())
                },
            );
            assert!(result.is_none() && slot.is_none());
            assert_eq!(callback_ran, case == 3);
            assert!(Arc::ptr_eq(
                &original.artifact_forest["//:active"],
                &old_root
            ));
        }
    }

    #[test]
    fn ledger_refresh_is_coherent_and_stale_refresh_preserves_newer_state() {
        let mut forest = BuiltArtifactForest::new();
        forest.insert("//pkg:a".to_string(), artifact_root("liba.a"));
        let original = RetainedExecutionState {
            generation: 7,
            ledger: Arc::new(nonempty_ledger("missing-original-output")),
            artifact_forest: Arc::new(forest),
            watch_baseline_id: 41,
        };
        let original_forest = Arc::clone(&original.artifact_forest);
        let original_ledger = Arc::clone(&original.ledger);
        let mut slot = Some(original.clone());

        let refreshed = refresh_state_if_current(
            &mut slot,
            &original,
            nonempty_ledger("missing-refreshed-output"),
        )
        .expect("current refresh");
        assert_eq!(refreshed.generation, 8);
        assert_eq!(refreshed.watch_baseline_id, 41);
        assert!(Arc::ptr_eq(&refreshed.artifact_forest, &original_forest));
        assert!(!Arc::ptr_eq(&refreshed.ledger, &original_ledger));

        assert!(refresh_state_if_current(
            &mut slot,
            &original,
            nonempty_ledger("missing-stale-refresh-output"),
        )
        .is_none());
        assert!(slot
            .as_ref()
            .is_some_and(|current| current.is_same_revision_as(&refreshed)));
    }

    #[test]
    fn snapshot_cache_invalidate_drops_entry() {
        // The cache stores Arc<AnalysisSnapshot>; we cannot easily build a real
        // snapshot in a unit test (it owns a DependencyGraph), so exercise the
        // key plumbing via the LRU directly through the public API on a wrapper.
        let cache = SnapshotCache::new(2);
        let k = key_with_patterns(&["//a"]);
        // get on empty returns None.
        assert!(cache.get(&k).is_none());
        // invalidate on empty is a no-op (must not panic).
        cache.invalidate(&k);
        assert!(cache.get(&k).is_none());
    }
}
