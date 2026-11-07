//! Phase-2 persistent action freshness — the warm-no-op wavefront-skip.
//!
//! ## Why this exists
//!
//! On a daemon warm no-op, `manifest.validate() == Fresh` already proves no
//! ANALYSIS-relevant input moved (`.bzl`/`BUILD`/glob dir-listings/config
//! fingerprint), so reuse is admitted and PASS-A is skipped. But the reuse path
//! then re-ran the full PASS-B wavefront over every target: each `rule.build()`
//! re-derived its command specs, re-resolved srcs, and stat'd its action's
//! inputs/outputs through the in-memory action cache's hit-gate to conclude
//! "already built". Profiling //tensorflow/core:framework (1142 targets) put
//! that wavefront walk at ~149 ms of a ~192 ms warm no-op — the dominant phase.
//!
//! The action layer's freshness proof is, in essence: "every declared action's
//! inputs and outputs are unchanged since they were built." That proof does not
//! need the command specs re-derived — it only needs the action graph's
//! input/output FILES re-stat'd. The fully-analyzed action graph already lives
//! in the retained snapshot's [`ArtifactRegistry`] (every legacy cc rule
//! declares its `CcCompile`/`CcArchive`/`CcLink` actions there during the cold
//! build). So we can replay the SAME proof far more cheaply: enumerate every
//! unique input+output path in the declared action graph, stat it ONCE, and
//! compare to the metadata captured right after the cold build committed.
//!
//! ## Correctness — a cheaper REPLAY of the existing proof, not a smaller proof
//!
//! (Design reviewed with Codex gpt-5.5 xhigh, thread 019efda0.)
//!
//! * The ledger covers the SAME universe the action cache's hit-gate checks:
//!   declared-action inputs (sources + generated upstream outputs) and outputs.
//! * It uses the SAME rich metadata as the manifest's `FileMetadata` dep
//!   (`present`/`len`/`mode`/`mtime_ns`/`ctime_ns`/`dev`/`ino`) via the shared
//!   [`rush_incremental::stat_meta`]. Metadata mismatches are conservative by
//!   default; the live in-memory ledger records bounded content digests for
//!   small regular files so a pure `touch` / identical save can be proven
//!   byte-clean and pruned, while cold/reconstructed ledgers and unhashable
//!   paths still take the metadata-MISS rebuild.
//! * What the manifest covers and the ledger does NOT need to: the ACTION SET
//!   itself. A new source pulled in by a glob, a changed `BUILD`/`.bzl`, or a
//!   changed `--define` all flip `validate()` Stale and force a full
//!   re-analysis BEFORE the ledger is ever consulted. So the ledger is only
//!   asked "did any file in THIS fixed action graph move?" — never "did the
//!   graph shape change?".
//! * Toolchain/compiler: covered by the analysis config fingerprint
//!   (`validate()`), so a `gcc` swap that changes the toolchain identity already
//!   re-analyzes. The compiler binary is additionally an action input when the
//!   action graph records it; either way a clean ledger never serves a build
//!   produced by a different toolchain than the one the snapshot was keyed to.
//!
//! ## Invalidation rules (Codex traps, enforced by the caller)
//!
//! * The ledger is tied to the EXACT snapshot it was built from — it lives
//!   inside the immutable `AnalysisSnapshot`, so a re-analysis produces a fresh
//!   ledger; it can never be applied to a different action graph.
//! * The ledger is populated ONLY after a wavefront that built every target
//!   successfully. A failed/interrupted build leaves the ledger empty (→ the
//!   next warm command takes the full wavefront, which is correct).
//! * On a ledger MISS (any path moved), the caller runs the full wavefront and
//!   then RE-populates the ledger from the post-build registry, so the next
//!   no-op is fast again and reflects the just-built file state.

use rush_action::ids::ArtifactRegistry;
use rush_incremental::{content_digest_of_regular_file, stat_meta, FileMeta};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Upper bound (bytes) on a file we content-hash for content-change pruning.
/// A regular present file at or under this size gets a recorded content digest
/// so a pure mtime/ctime bump with identical bytes (touch / identical save /
/// git checkout of the same content) is recognized as clean instead of forcing
/// a full wavefront. Larger files (big `.a`/`.so` outputs) stay metadata-only —
/// a touch on them is rare and conservatively rebuilds. 8 MiB comfortably
/// covers every source/header and ordinary object file while bounding the
/// hashing cost at capture (TF-scale: only the source/input + small-output
/// side is hashed; giant link outputs are skipped).
const CONTENT_DIGEST_MAX_LEN: u64 = 8 * 1024 * 1024;

/// The outcome of [`FreshnessLedger::verify`].
///
/// `Clean` / `Dirty` are the original two-state verdict. `CleanWithRefresh`
/// (content-change pruning) is reported when every metadata mismatch was a pure
/// mtime/ctime bump over BYTE-IDENTICAL content (proven by a recorded + current
/// content digest match): the graph is provably up-to-date, but the recorded
/// metadata is stale and the caller MUST install the returned refreshed ledger
/// so the next no-op does not re-discover the same benign mismatch (Codex
/// 019f05ff: "refresh the ledger entry metadata, otherwise every later build
/// rediscovers the same benign mtime mismatch").
pub enum VerifyOutcome {
    /// Every tracked path's metadata matched exactly — skip the wavefront.
    Clean,
    /// Every mismatch was a content-unchanged mtime/ctime bump — skip the
    /// wavefront AND replace the snapshot's ledger with this refreshed one.
    CleanWithRefresh(FreshnessLedger),
    /// At least one path genuinely changed (size/mode/presence/kind/content) —
    /// run the full wavefront, or a proven affected reverse-dep slice. `reason`
    /// names the first dirty path; `paths` carries the full dirty path set so
    /// the caller never prunes to only one edit when several files changed.
    Dirty { reason: String, paths: Vec<PathBuf> },
}

/// The per-unique-path file-state table for one analyzed action graph, plus the
/// verdict of whether it represents a fully-built graph.
#[derive(Clone, Debug, Default)]
pub struct FreshnessLedger {
    /// Deduped path -> metadata captured right after the graph was built. Each
    /// distinct input/output path of every declared action appears once.
    paths: HashMap<PathBuf, FileMeta>,
    /// Content digests for the subset of `paths` that were regular present
    /// files at or under [`CONTENT_DIGEST_MAX_LEN`] at capture. Used by
    /// content-change pruning: on a pure mtime/ctime mismatch the current
    /// content digest is compared to the recorded one; a match proves the file
    /// is byte-identical and the mismatch is benign. NOT serialized into the
    /// cold-no-op certificate (a cold/reconstructed ledger has an empty map and
    /// simply falls back to the metadata-MISS rebuild — conservative, correct).
    content_digests: HashMap<PathBuf, [u8; 32]>,
    /// Number of declared actions the ledger was built from (diagnostics).
    action_count: usize,
}

impl FreshnessLedger {
    /// Number of unique paths tracked.
    pub fn len(&self) -> usize {
        self.paths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    pub fn action_count(&self) -> usize {
        self.action_count
    }

    /// The per-path metadata table, for serialization into the cold-no-op
    /// certificate. Read-only view; reconstruction uses [`from_parts`].
    pub fn paths(&self) -> &HashMap<PathBuf, FileMeta> {
        &self.paths
    }

    /// True iff this action-freshness ledger tracks `path` as an input or
    /// output in the retained action graph.
    pub fn contains_path(&self, path: &Path) -> bool {
        self.paths.contains_key(path)
    }

    /// True iff every supplied path has a recorded content digest and its
    /// current bytes match that recorded digest.
    ///
    /// This is stricter than normal freshness: metadata may change because the
    /// producer just rewrote the file, but if the bytes are identical then any
    /// downstream action that consumes the file content would see the same input.
    /// Missing paths, unrecorded/oversized digests, or digest mismatches all
    /// return false so callers conservatively rebuild dependents.
    pub fn paths_content_unchanged_now(&self, paths: &[PathBuf]) -> bool {
        paths.iter().all(|path| {
            let Some(recorded_digest) = self.content_digests.get(path) else {
                return false;
            };
            let current = stat_meta(path);
            if !current.present {
                return false;
            }
            match content_digest_of_regular_file(path, CONTENT_DIGEST_MAX_LEN) {
                Some(current_digest) => &current_digest == recorded_digest,
                None => false,
            }
        })
    }

    /// All tracked path keys (inputs + outputs), for the Phase-4 ledger watch:
    /// the watcher installs a deduped parent-dir watch over exactly this set so
    /// a healthy no-op can skip the `verify()` stat-walk. Cheap clone of the keys.
    pub fn tracked_paths(&self) -> Vec<PathBuf> {
        self.paths.keys().cloned().collect()
    }

    /// Tracked paths that were present in the recorded baseline. Used by a
    /// pruned PASS-B rebuild to preserve the previous full ledger universe when
    /// refreshing metadata after rebuilding only an affected reverse-dep slice.
    pub fn recorded_present_paths(&self) -> Vec<PathBuf> {
        self.paths
            .iter()
            .filter_map(|(p, meta)| if meta.present { Some(p.clone()) } else { None })
            .collect()
    }

    /// Return the subset of `paths` that were present in this recorded
    /// baseline. Pruned rebuilds only refresh an affected slice; inactive paths
    /// are protected by the path-aware watcher, so the post-prune
    /// required-present check only needs the refreshed slice rather than a clone
    /// of the whole ledger universe.
    pub fn recorded_present_paths_in(&self, paths: &[PathBuf]) -> Vec<PathBuf> {
        paths
            .iter()
            .filter(|p| self.paths.get(*p).is_some_and(|meta| meta.present))
            .cloned()
            .collect()
    }

    /// Refresh this ledger after a pruned PASS-B rebuild.
    ///
    /// The full-build capture path can rebuild the whole ledger from scratch.
    /// A pruned rebuild intentionally visits only an affected reverse-dep slice,
    /// so this path starts from the previous full ledger and updates just the
    /// known affected action paths. Inactive-path safety comes from the
    /// path-aware ledger watcher window in the caller; do not stat the full
    /// inactive universe here or the pruned rebuild regresses to the old
    /// O(total ledger paths) capture cost. Content digests are refreshed only
    /// for `digest_paths`; callers keep that set narrow, but can include active
    /// outputs when downstream output-equivalence pruning needs byte proofs.
    pub fn refresh_after_pruned_rebuild(
        &self,
        refresh_paths: &[PathBuf],
        digest_paths: &[PathBuf],
    ) -> Self {
        let mut refreshed = self.clone();
        refreshed.refresh_after_pruned_rebuild_in_place(refresh_paths, digest_paths);
        refreshed
    }

    /// In-place form of [`refresh_after_pruned_rebuild`]. This is the warm
    /// pruned-edit hot path: cloning the whole ledger's HashMaps just to update
    /// a handful of paths costs measurable milliseconds.
    pub fn refresh_after_pruned_rebuild_in_place(
        &mut self,
        refresh_paths: &[PathBuf],
        digest_paths: &[PathBuf],
    ) {
        let refresh_set: std::collections::HashSet<PathBuf> =
            refresh_paths.iter().cloned().collect();
        let digest_set: std::collections::HashSet<PathBuf> =
            digest_paths.iter().cloned().collect();

        // Stat and digest in parallel, mutate the maps serially. The loop
        // was a plain serial walk - one stat plus up to an 8 MiB hash per
        // path, one path at a time - while build_complete does the same
        // measurement with a par_iter. On a wide slice the serial form
        // charges the whole refresh against the warm-edit wall clock.
        use rayon::prelude::*;
        let measured: Vec<(PathBuf, FileMeta, Option<[u8; 32]>)> = refresh_set
            .into_par_iter()
            .map(|path| {
                let current = stat_meta(&path);
                let digest = if current.present && digest_set.contains(&path) {
                    content_digest_of_regular_file(&path, CONTENT_DIGEST_MAX_LEN)
                } else {
                    None
                };
                (path, current, digest)
            })
            .collect();
        for (path, current, digest) in measured {
            self.paths.insert(path.clone(), current);
            if let Some(digest) = digest {
                self.content_digests.insert(path, digest);
            } else {
                self.content_digests.remove(&path);
            }
        }
    }

    /// True iff EVERY path the ledger recorded as PRESENT (`present == true`,
    /// i.e. an actual output/input that existed at capture) is still recorded
    /// present in the just-captured snapshot. Used by the Phase-4 watch-baseline
    /// install: if a required output was absent at capture (an external `rm` beat
    /// the post-build capture), the baseline is NOT trustworthy and the watcher
    /// must NOT authorize skipping `verify()` — the caller refuses to install a
    /// fast baseline and falls back to the full stat-walk (Codex 019f0368).
    ///
    /// Because `build_complete` captures `present` per path from the live stat
    /// right after the build, "all recorded-present" is exactly "the build
    /// produced every output it claimed" — a self-consistent baseline.
    pub fn all_recorded_present(&self, required_outputs: &[PathBuf]) -> bool {
        self.first_absent_required(required_outputs).is_none()
    }

    /// The first required path the ledger recorded as absent, in sorted
    /// order for reproducibility.
    ///
    /// One absent required path refuses the fast no-op baseline for the
    /// whole workspace, so "which one" is the only question that
    /// matters when it happens — and for a long time the diagnostic did
    /// not say, which is why a permanently-vetoed baseline on
    /// TensorFlow sat unexplained as a "minor follow-up". Every caller
    /// that reports the veto now names the path.
    pub fn first_absent_required<'a>(
        &self,
        required_outputs: &'a [PathBuf],
    ) -> Option<&'a Path> {
        let mut absent: Vec<&Path> = required_outputs
            .iter()
            .filter(|p| !self.paths.get(*p).map(|m| m.present).unwrap_or(false))
            .map(|p| p.as_path())
            .collect();
        absent.sort_unstable();
        absent.first().copied()
    }

    /// How many required paths are recorded absent.
    pub fn absent_required_count(&self, required_outputs: &[PathBuf]) -> usize {
        required_outputs
            .iter()
            .filter(|p| !self.paths.get(*p).map(|m| m.present).unwrap_or(false))
            .count()
    }

    /// Reconstruct a ledger from its persisted parts (the cold-no-op
    /// certificate path). The reconstructed ledger is byte-identical in behavior
    /// to one captured live: `verify()` re-stats every path and compares to the
    /// recorded `FileMeta`, so a ledger loaded from disk after a daemon restart
    /// validates exactly as the in-memory one would have.
    pub fn from_parts(paths: HashMap<PathBuf, FileMeta>, action_count: usize) -> Self {
        FreshnessLedger {
            paths,
            // A reconstructed (cold-certificate) ledger carries no content
            // digests, so content-change pruning is unavailable on the cold
            // path — `verify()` falls back to the metadata MISS, which is the
            // conservative (correct) behavior. Only the live in-memory ledger
            // built by `build_complete` enables content pruning.
            content_digests: HashMap::new(),
            action_count,
        }
    }

    /// Build a COMPLETE ledger over the no-op freshness universe, statting every
    /// unique path once.
    ///
    /// Called right after a PASS-B wavefront that built every target
    /// successfully, so the recorded metadata reflects the just-built file
    /// state. The universe (design reviewed with Codex gpt-5.5 xhigh, thread
    /// 019efda0) is the union of:
    ///
    ///   * `output_paths` — every ACTUAL output artifact of every target, from
    ///     the wavefront's `built_artifacts` map. This is the complete output
    ///     side and crucially covers targets (proto_library / genrule / …) whose
    ///     legacy `build()` does NOT declare actions into the registry, so the
    ///     declared-action graph alone would miss their outputs.
    ///   * declared-action inputs + outputs — the cc rules' compile/archive/link
    ///     file dependencies.
    ///   * every SOURCE artifact path the registry interned — the input side
    ///     that catches a touched EXISTING source (which leaves the glob
    ///     dir-listing unchanged, so `manifest.validate()` stays Fresh and would
    ///     otherwise miss it). Includes the inputs of legacy non-declaring rules.
    ///
    /// Over-collection is safe: a tracked path that legitimately changed just
    /// forces the full wavefront (still correct). A MISSED input/output would
    /// serve a stale build, so the union is deliberately conservative.
    ///
    /// Statting is parallelized with rayon — the set is ~1k–18k paths at
    /// TF-framework scale and the stats are independent.
    pub async fn build_complete(
        registry: &std::sync::Arc<tokio::sync::Mutex<ArtifactRegistry>>,
        output_paths: &[PathBuf],
    ) -> Self {
        use rayon::prelude::*;

        // Collect the unique path set under the registry lock, then release it
        // before the (potentially many-thousand) stats so we don't hold the
        // action-graph mutex across the I/O scan.
        let (unique_paths, action_count): (Vec<PathBuf>, usize) = {
            let reg = registry.lock().await;
            let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
            let mut count = 0usize;
            // Declared-action inputs + outputs (cc rules).
            for (_id, action) in reg.declared_actions() {
                count += 1;
                for aid in action.inputs.iter().chain(action.outputs.iter()) {
                    seen.insert(reg.artifact_path(*aid).to_path_buf());
                }
            }
            // Every source artifact the registry knows about (the input side,
            // including non-declaring legacy rules' sources).
            for p in reg.source_artifact_paths() {
                seen.insert(p.to_path_buf());
            }
            // Every actual target output (the complete output side).
            for p in output_paths {
                seen.insert(p.clone());
            }
            (seen.into_iter().collect(), count)
        };

        // Parallel stat of the unique path set (independent reads, hot dcache).
        // For content-change pruning, also record a content digest for each
        // regular present file at or under the size cap — this is the only
        // extra cost vs the metadata-only ledger, paid once at capture (after a
        // full build the file pages are hot). Outputs over the cap and
        // non-regular paths get no digest and stay metadata-only.
        let captured: Vec<(PathBuf, FileMeta, Option<[u8; 32]>)> = unique_paths
            .par_iter()
            .map(|p| {
                let meta = stat_meta(p);
                let digest = if meta.present {
                    content_digest_of_regular_file(p, CONTENT_DIGEST_MAX_LEN)
                } else {
                    None
                };
                (p.clone(), meta, digest)
            })
            .collect();

        let mut paths: HashMap<PathBuf, FileMeta> = HashMap::with_capacity(captured.len());
        let mut content_digests: HashMap<PathBuf, [u8; 32]> = HashMap::new();
        for (p, meta, digest) in captured {
            if let Some(d) = digest {
                content_digests.insert(p.clone(), d);
            }
            paths.insert(p, meta);
        }

        FreshnessLedger {
            paths,
            content_digests,
            action_count,
        }
    }

    /// Re-stat every tracked path and compare to the recorded metadata.
    ///
    /// Returns `Ok(())` when EVERY path's current metadata matches what was
    /// recorded (the whole action graph is up-to-date — the wavefront can be
    /// skipped). Returns `Err(reason)` naming the first path that moved. An
    /// empty ledger is treated as a MISS so a never-populated / cleared ledger
    /// never short-circuits a build.
    ///
    /// Parallelized with rayon. Dirty verification is exhaustive in
    /// [`verify_outcome`] so callers that prune PASS-B can cover every dirty
    /// path, not just the first mismatch.
    pub fn verify(&self) -> Result<(), String> {
        match self.verify_outcome() {
            VerifyOutcome::Clean | VerifyOutcome::CleanWithRefresh(_) => Ok(()),
            VerifyOutcome::Dirty { reason, .. } => Err(reason),
        }
    }

    /// Re-stat every tracked path, applying content-change pruning, and return
    /// the full [`VerifyOutcome`].
    ///
    /// A path is CLEAN when its current metadata matches the recorded metadata
    /// exactly. When the metadata differs, the only difference tolerated is a
    /// pure mtime/ctime bump over BYTE-IDENTICAL content: if presence, kind
    /// (mode), length, and identity (dev/ino) are unchanged AND a content
    /// digest was recorded AND the current content digest matches it, the path
    /// is content-clean — its bytes never changed, so no action that consumes
    /// it can produce a different output. Every such path is collected for a
    /// metadata REFRESH. Any other mismatch (deleted/appeared/size/mode/dev/ino
    /// change, or an mtime bump with no usable digest, or a digest MISMATCH) is
    /// DIRTY → the caller runs the full wavefront.
    ///
    /// Returns:
    ///   * `Clean` — every path matched exactly.
    ///   * `CleanWithRefresh(ledger)` — every mismatch was content-clean; the
    ///     returned ledger carries the refreshed (current) metadata for those
    ///     paths and MUST replace the snapshot's ledger.
    ///   * `Dirty(reason, paths)` — at least one path genuinely changed.
    ///
    /// Parallelized with rayon. The exact-match fast path stats only (the
    /// common no-op shape); content hashing is paid ONLY on the mtime-mismatch
    /// subset of paths (e.g. the one file a touch perturbed), never on the
    /// whole tree. Dirty verification is exhaustive, not first-mismatch
    /// short-circuiting, because PASS-B pruning must cover every dirty path.
    pub fn verify_outcome(&self) -> VerifyOutcome {
        use rayon::prelude::*;

        if self.paths.is_empty() {
            return VerifyOutcome::Dirty {
                reason: "freshness ledger empty (not populated)".to_string(),
                paths: Vec::new(),
            };
        }

        enum PathCheck {
            Clean,
            Refresh(PathBuf, FileMeta),
            Dirty(String, PathBuf),
        }

        let checks: Vec<PathCheck> = self
            .paths
            .par_iter()
            .map(|(path, recorded)| {
                let current = stat_meta(path);
                if current == *recorded {
                    return PathCheck::Clean;
                }
                // Metadata differs. Is it a pure mtime/ctime bump over identical
                // bytes? Require everything BUT mtime/ctime to be unchanged.
                let only_time_changed = recorded.present
                    && current.present
                    && recorded.len == current.len
                    && recorded.mode == current.mode
                    && recorded.dev == current.dev
                    && recorded.ino == current.ino;
                let mut time_change_reason = None;
                if only_time_changed {
                    match self.content_digests.get(path) {
                        Some(recorded_digest) => match content_digest_of_regular_file(
                            path,
                            CONTENT_DIGEST_MAX_LEN,
                        ) {
                            Some(cur_digest) if &cur_digest == recorded_digest => {
                                // Byte-identical: benign touch. Refresh to the
                                // current metadata so the next no-op matches
                                // exactly without re-hashing.
                                return PathCheck::Refresh(path.clone(), current);
                            }
                            Some(_) => {
                                time_change_reason = Some("content changed (digest mismatch)")
                            }
                            None => {
                                time_change_reason =
                                    Some("mtime/ctime changed (content no longer hashable)")
                            }
                        },
                        None => {
                            time_change_reason =
                                Some("mtime/ctime changed (no recorded content digest)")
                        }
                    }
                }
                let kind = if recorded.present && !current.present {
                    "output/input deleted"
                } else if !recorded.present && current.present {
                    "path appeared"
                } else if recorded.len != current.len {
                    "size changed"
                } else if recorded.mtime_ns != current.mtime_ns
                    || recorded.ctime_ns != current.ctime_ns
                {
                    time_change_reason.unwrap_or("mtime/ctime changed")
                } else {
                    "metadata changed"
                };
                PathCheck::Dirty(format!("{}: {}", kind, path.display()), path.clone())
            })
            .collect();

        let mut refreshed = Vec::new();
        let mut dirty = Vec::new();
        for check in checks {
            match check {
                PathCheck::Clean => {}
                PathCheck::Refresh(path, meta) => refreshed.push((path, meta)),
                PathCheck::Dirty(reason, path) => dirty.push((reason, path)),
            }
        }

        if !dirty.is_empty() {
            dirty.sort_by(|a, b| a.1.cmp(&b.1));
            let reason = dirty[0].0.clone();
            let paths = dirty.into_iter().map(|(_, path)| path).collect();
            return VerifyOutcome::Dirty { reason, paths };
        }

        if refreshed.is_empty() {
            return VerifyOutcome::Clean;
        }

        // Build the refreshed ledger: clone the recorded state and overwrite the
        // benign-touch paths' metadata with the current stat. Content digests
        // are unchanged (the bytes are identical, so the recorded digest still
        // holds). action_count is preserved (same action graph).
        let mut new_paths = self.paths.clone();
        for (p, m) in refreshed {
            new_paths.insert(p, m);
        }
        VerifyOutcome::CleanWithRefresh(FreshnessLedger {
            paths: new_paths,
            content_digests: self.content_digests.clone(),
            action_count: self.action_count,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn meta_of(p: &std::path::Path) -> FileMeta {
        stat_meta(p)
    }

    #[test]
    fn empty_ledger_verifies_as_miss() {
        let l = FreshnessLedger::default();
        assert!(l.verify().is_err(), "empty ledger must be a MISS");
    }

    #[test]
    fn clean_ledger_verifies_ok_and_touch_invalidates() {
        let dir = std::env::temp_dir().join(format!("rush-fresh-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("out.o");
        {
            let mut fh = std::fs::File::create(&f).unwrap();
            fh.write_all(b"hello").unwrap();
        }
        let mut paths = HashMap::new();
        paths.insert(f.clone(), meta_of(&f));
        // No content digest recorded → a pure mtime bump cannot be content-
        // pruned, so this ledger keeps the conservative metadata-MISS behavior.
        let ledger = FreshnessLedger {
            paths,
            content_digests: HashMap::new(),
            action_count: 1,
        };
        // Unchanged → clean.
        assert!(ledger.verify().is_ok(), "unchanged file must verify clean");

        // Touch (advance mtime) → MISS. Sleep a hair so mtime advances on
        // coarse-resolution filesystems, then rewrite.
        std::thread::sleep(std::time::Duration::from_millis(20));
        {
            let mut fh = std::fs::OpenOptions::new()
                .write(true)
                .open(&f)
                .unwrap();
            fh.write_all(b"hELLO").unwrap();
        }
        // Also force an explicit mtime bump via utimensat-equivalent: rewrite
        // changes mtime on every sane FS; assert the ledger now reports a MISS.
        assert!(
            ledger.verify().is_err(),
            "a rewritten/touched output must invalidate the ledger"
        );

        // Delete → MISS naming a deletion.
        std::fs::remove_file(&f).unwrap();
        let err = ledger.verify().unwrap_err();
        assert!(err.contains("deleted"), "deletion must be reported: {err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Build a single-file ledger WITH a recorded content digest (the live
    /// `build_complete` shape) so content-change pruning is enabled.
    fn digest_ledger(f: &std::path::Path) -> FreshnessLedger {
        let mut paths = HashMap::new();
        paths.insert(f.to_path_buf(), meta_of(f));
        let mut content_digests = HashMap::new();
        content_digests.insert(
            f.to_path_buf(),
            content_digest_of_regular_file(f, CONTENT_DIGEST_MAX_LEN).unwrap(),
        );
        FreshnessLedger {
            paths,
            content_digests,
            action_count: 1,
        }
    }

    #[test]
    fn pruned_refresh_digests_only_requested_paths() {
        let dir = std::env::temp_dir().join(format!("rush-fresh-pruned-refresh-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let input = dir.join("src.cc");
        let output = dir.join("app");
        std::fs::write(&input, b"int x = 1;\n").unwrap();
        std::fs::write(&output, b"linked-v1").unwrap();

        let mut paths = HashMap::new();
        paths.insert(input.clone(), meta_of(&input));
        paths.insert(output.clone(), meta_of(&output));
        let mut content_digests = HashMap::new();
        content_digests.insert(
            input.clone(),
            content_digest_of_regular_file(&input, CONTENT_DIGEST_MAX_LEN).unwrap(),
        );
        content_digests.insert(
            output.clone(),
            content_digest_of_regular_file(&output, CONTENT_DIGEST_MAX_LEN).unwrap(),
        );
        let ledger = FreshnessLedger {
            paths,
            content_digests,
            action_count: 1,
        };
        assert_eq!(
            ledger.recorded_present_paths_in(&[input.clone(), dir.join("missing")]),
            vec![input.clone()]
        );

        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&output, b"linked-v2").unwrap();
        let refreshed_without_output_digest = ledger.refresh_after_pruned_rebuild(
            &[input.clone(), output.clone()],
            &[input.clone()],
        );
        assert!(
            refreshed_without_output_digest
                .content_digests
                .contains_key(&input),
            "input digest must remain available for same-bytes pruning"
        );
        assert!(
            !refreshed_without_output_digest
                .content_digests
                .contains_key(&output),
            "paths omitted from digest_paths must lose stale digests"
        );
        assert_eq!(
            refreshed_without_output_digest.paths.get(&output),
            Some(&meta_of(&output))
        );

        let refreshed_with_output_digest = ledger.refresh_after_pruned_rebuild(
            &[input.clone(), output.clone()],
            &[input.clone(), output.clone()],
        );
        assert!(
            refreshed_with_output_digest
                .paths_content_unchanged_now(&[output.clone()]),
            "requested output digest must prove the rebuilt output's current bytes"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn content_unchanged_touch_is_pruned_to_clean_with_refresh() {
        let dir = std::env::temp_dir().join(format!("rush-fresh-prune-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("src.cc");
        std::fs::write(&f, b"int x = 1;\n").unwrap();
        let ledger = digest_ledger(&f);
        assert!(matches!(ledger.verify_outcome(), VerifyOutcome::Clean));

        // Rewrite the EXACT SAME bytes (mtime bumps, content identical) — the
        // touch-without-content-change case. Must prune to CleanWithRefresh,
        // NOT a Dirty full-wavefront MISS.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&f, b"int x = 1;\n").unwrap();
        match ledger.verify_outcome() {
            VerifyOutcome::CleanWithRefresh(refreshed) => {
                // The refreshed ledger must now verify EXACTLY clean (the stale
                // mtime was updated) so the next no-op pays no re-hash.
                assert!(
                    matches!(refreshed.verify_outcome(), VerifyOutcome::Clean),
                    "refreshed ledger must verify exactly Clean"
                );
            }
            VerifyOutcome::Clean => panic!("mtime should have changed → expected refresh"),
            VerifyOutcome::Dirty { reason, .. } => {
                panic!("content-identical touch must NOT be Dirty, got: {reason}")
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn real_content_change_is_dirty_not_pruned() {
        let dir = std::env::temp_dir().join(format!("rush-fresh-real-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("src.cc");
        std::fs::write(&f, b"int x = 1;\n").unwrap();
        let ledger = digest_ledger(&f);

        // Same length, DIFFERENT bytes (a real edit) — digest mismatch → DIRTY.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&f, b"int x = 2;\n").unwrap();
        assert!(
            matches!(ledger.verify_outcome(), VerifyOutcome::Dirty { .. }),
            "a real content change with identical length must be DIRTY (digest differs)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn size_change_is_dirty_even_with_digest() {
        let dir = std::env::temp_dir().join(format!("rush-fresh-size-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let f = dir.join("src.cc");
        std::fs::write(&f, b"int x = 1;\n").unwrap();
        let ledger = digest_ledger(&f);

        // Different length → size mismatch, never reaches the digest path → DIRTY.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&f, b"int x = 1; // longer now\n").unwrap();
        match ledger.verify_outcome() {
            VerifyOutcome::Dirty { reason, .. } => {
                assert!(reason.contains("size changed"), "got {reason}")
            }
            _ => panic!("size change must be Dirty"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
