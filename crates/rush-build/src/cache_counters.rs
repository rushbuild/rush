// Rush Build - Analyze-pass cache counters (phase 5.C.4 commit 89)
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Phase 5.C.4 commit 89: BUILD-file + .bzl cache hit/miss
//! counters for the analyze pass.
//!
//! Pro's commit-89 directive: *"Instrument build-file and
//! .bzl cache effectiveness. Track BUILD file cache
//! lookups/hits/misses/evictions, unique BUILD files
//! loaded, repeated loads by package, .bzl load cache
//! hits/misses if accessible, external repo path
//! resolution count."*
//!
//! Commit 88 already answered the unique-vs-total parse
//! question: TF macros target parses ONE file ONCE.
//! Commit 89 zooms into the SAME parse call to see where
//! its time goes — specifically, whether `.bzl` cache
//! effectiveness during that one parse explains the
//! 99.9-second cost.
//!
//! ## Data sources
//!
//! - **BUILD file cache**: the orchestrator's
//!   `build_files_seen: HashMap<String, ()>` provides the
//!   "skip-if-already-parsed" dedup. Counters here track
//!   how many times the dedup fires per analyze
//!   invocation.
//! - **.bzl cache**: `rush_starlark::GlobalBzlCache` already
//!   has internal `hits`/`misses`/`evictions` metrics
//!   accessible via `cache_stats()`. Commit 89 SNAPSHOTS
//!   those at the start and end of analyze and reports
//!   the delta.
//! - **External repo resolution**: counter incremented
//!   each time the orchestrator's repo-resolution code
//!   path fires for a non-main workspace label.

/// Phase 5.C.4 commit 89: aggregate cache counter snapshot.
///
/// One per analyze-pass invocation. Builds incrementally
/// during the parse loop, finalizes at the end of the
/// analyze-only return path.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct AnalyzeCacheCounters {
    /// Schema version for the JSON wire shape.
    pub schema_version: u32,

    // BUILD-file cache (HashMap-based dedup at orchestrator).
    /// Total BUILD-file lookups attempted.
    pub build_file_lookups: u64,
    /// Lookups satisfied by the in-pass dedup map.
    pub build_file_hits: u64,
    /// Lookups that required a new parse.
    pub build_file_misses: u64,

    // .bzl cache (GlobalBzlCache singleton).
    /// .bzl cache hits observed during this analyze pass.
    /// Computed as (end_hits - start_hits) from the
    /// GlobalBzlCache.
    pub bzl_cache_hits: u64,
    /// .bzl cache misses observed during this analyze
    /// pass.
    pub bzl_cache_misses: u64,
    /// .bzl cache evictions during this analyze pass.
    pub bzl_cache_evictions: u64,
    /// .bzl cache hit rate as a percentage (0.0-100.0).
    /// `None` when no lookups happened.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bzl_cache_hit_rate_percent: Option<f64>,

    // External repo resolution.
    /// Number of external repo path lookups.
    pub external_repo_resolutions: u64,

    // Phase 5.C.4 commit 103: top-N slowest cold-loaded
    /// .bzl files. Empty by default; populated when
    /// `RUSH_ANALYZE_CACHE_COUNTERS=1` is set. Top-N
    /// configurable via RUSH_ANALYZE_BZL_TOP_N (commit 103).
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub bzl_top_slowest_cold_loads: Vec<BzlColdLoadEntry>,
    /// Phase 5.C.4 commit 171 (Batch V): top-N `.bzl`
    /// files by repeat-miss count (post-eviction thrash
    /// signal). Empty when repeat-miss recording is
    /// disabled.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub bzl_top_by_repeat_misses: Vec<BzlRepeatMissEntry>,

    // Phase 5.C.4 commit 96: detailed .bzl cache breakdown.
    /// Cold .bzl misses (first lookup for the path).
    pub bzl_cold_misses: u64,
    /// Repeat .bzl misses (path was previously cached and
    /// evicted). High repeat-miss counts = thrashing.
    pub bzl_repeat_misses: u64,
    /// .bzl evictions triggered by entry-count limit.
    pub bzl_evictions_by_entry_limit: u64,
    /// .bzl evictions triggered by byte limit.
    pub bzl_evictions_by_byte_limit: u64,
    /// .bzl evictions explicitly requested.
    pub bzl_evictions_manual: u64,
    /// High-water entry count observed.
    pub bzl_peak_entry_count: u64,
    /// Number of distinct .bzl files ever seen.
    pub bzl_unique_files_seen: u64,
    /// Configured `max_entries` limit on the underlying
    /// GlobalBzlCache.
    pub bzl_max_entries: u64,
    /// Configured `max_memory_bytes` limit (rendered as
    /// MB) on the underlying GlobalBzlCache.
    pub bzl_max_memory_mb: u64,
    /// High-water memory usage observed (MB).
    pub bzl_peak_memory_mb: u64,

    /// Phase 5.C.4 commit 105: top-N .bzl files by
    /// forward fanout (direct child `load()` count).
    /// Empty when load-graph recording disabled.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub bzl_top_by_fanout: Vec<BzlLoadGraphEntry>,

    /// Phase 5.C.4 commit 105: top-N .bzl files by
    /// reverse fanout (number of distinct .bzl files
    /// that directly load this file). Helps spot the
    /// "loaded everywhere" pre-WORKSPACE root configs.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub bzl_top_by_reverse_dependents: Vec<BzlLoadGraphEntry>,

    /// Phase 5.C.4 commit 105: identity-collision
    /// summary. One entry per resolved path that was
    /// reached via more than one raw `load()` label.
    /// Empty when no collisions observed.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub bzl_identity_collisions: Vec<BzlIdentityCollisionEntry>,

    /// Phase 5.C.4 commit 106: top-N .bzl files by
    /// total coarse Starlark-operation volume
    /// (function + builtin + rule + macro + glob).
    /// Empty when op-counter recording disabled.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub bzl_top_by_starlark_ops: Vec<BzlStarlarkOpsEntry>,

    /// Phase 5.C.4 commit 106: process-wide totals
    /// across all .bzl files. Operationally useful as
    /// a sanity check ("we saw N rule invocations
    /// total"). Zero when op-counter recording
    /// disabled.
    pub bzl_total_function_calls: u64,
    pub bzl_total_builtin_calls: u64,
    pub bzl_total_rule_invocations: u64,
    pub bzl_total_macro_invocations: u64,
    pub bzl_total_glob_calls: u64,

    /// Phase 5.C.4 commit 118: process-wide
    /// estimate_symbols_size accounting (the recursive
    /// Value-walk inside `put_symbols`). Commit-114
    /// data showed this is the dominant cost inside
    /// finalize for tensorflow.bzl (~150 ms/call × 6
    /// cold loads ≈ 900 ms). The reuse-on-repeat-miss
    /// optimization (commit 120) targets these
    /// counters.
    pub bzl_estimate_symbols_size_calls: u64,
    pub bzl_estimate_symbols_size_total_ms: u64,
    /// Population of repeat-miss reloads that would
    /// be candidates for the commit-120 reuse path.
    /// Always recorded (commit 118); reuse_hits stays
    /// 0 until commit 120 enables
    /// `RUSH_BZL_REUSE_SIZE_ESTIMATE=1`.
    pub bzl_repeat_reload_estimate_candidates: u64,
    pub bzl_repeat_reload_estimate_reuse_hits: u64,
    pub bzl_repeat_reload_estimate_reuse_misses: u64,
    /// Verification mismatches under
    /// `RUSH_BZL_REUSE_SIZE_ESTIMATE_VERIFY=1`. A
    /// non-zero value here means the fingerprint is
    /// missing a relevant invariant.
    pub bzl_estimate_verification_mismatches: u64,

    /// Phase 5.C.4 commit 126 (Batch P): wait-category
    /// counters split off from the wall-time floor.
    ///
    /// `async_spawn_wait_*`: time spent at
    /// `rx.recv_timeout` in the non-tokio
    /// `thread::spawn` path of
    /// `load_bzl_from_evaluator`. Suspected dominant
    /// wait category for the 90-second floor.
    pub bzl_async_spawn_wait_ms: u64,
    pub bzl_async_spawn_wait_calls: u64,
    /// Aggregated `std::fs::metadata()` wait time.
    pub bzl_fs_metadata_wait_ms: u64,
    pub bzl_fs_metadata_wait_calls: u64,
    /// Phase 5.C.4 commit 133 (Batch Q): time spent
    /// acquiring the BzlLoader `loader_arc` Mutex
    /// across all recursive load_bzl_from_evaluator
    /// entries. Prime suspect for the 96.3% wait floor
    /// per the commit-131 decision doc.
    pub bzl_loader_lock_wait_ms: u64,
    pub bzl_loader_lock_wait_calls: u64,
    /// Phase 5.C.4 commit 133 (Batch Q): time spent
    /// in the "concurrent eval polling" sleep loop in
    /// load_bzl_from_evaluator. Each iteration sleeps
    /// 100ms; up to 600 iterations = 60s max per
    /// recursive load. Likely THE dominant wait.
    pub bzl_concurrent_eval_wait_ms: u64,
    pub bzl_concurrent_eval_wait_calls: u64,
    /// Phase 5.C.4 commit 133 (Batch Q): time spent
    /// in `bazel fetch` subprocess invocations from
    /// the orchestrator's external-repo resolution
    /// path. THE 90s wait per commit-133's smoking-gun
    /// finding.
    pub bzl_bazel_fetch_wait_ms: u64,
    pub bzl_bazel_fetch_wait_calls: u64,
    /// Phase 5.C.4 commit 134 (Batch Q): typed
    /// fetch-decision counters. Each call to
    /// `try_fetch_known_repo` produces exactly one
    /// decision. The sum of these five equals
    /// the total number of `try_fetch_known_repo`
    /// terminal decisions in this analyze pass
    /// (excluding short-circuit paths like the
    /// `SELF_SINGLETON_FETCHED` per-process dedup).
    pub bzl_fetch_invoked_count: u64,
    pub bzl_fetch_skipped_materialized_count: u64,
    pub bzl_fetch_skipped_known_rule_count: u64,
    pub bzl_fetch_materialized_missing_dir_count: u64,
    pub bzl_fetch_materialized_missing_build_count: u64,
    /// Per-repo total fetch duration (milliseconds).
    /// Only populated for repos where the decision was
    /// `Invoked`. Sorted-key JSON output for stable
    /// diffs. Used by commit 135's per-repo report.
    pub bzl_fetch_duration_by_repo:
        std::collections::BTreeMap<String, u64>,
    /// Phase 5.C.4 commit 135 (Batch Q): per-repo
    /// fetch attribution report. Sorted by `repo_name`
    /// (stable JSON diffs). One entry per repo that
    /// `try_fetch_known_repo` saw during this analyze
    /// pass.
    pub bzl_fetch_report: Vec<BzlFetchReportEntry>,
}

/// Phase 5.C.4 commit 135 (Batch Q): per-repo fetch
/// attribution row. Mirrors `rush_starlark::
/// FetchReportEntry` but lives in the rush-build wire
/// shape so the analyze-report JSON has it.
/// `Default` is intended for tests/fixtures ONLY.
/// Production fetch reports should populate all
/// relevant fields explicitly via
/// `FetchReportEntry::from_decision` and the
/// orchestrator projection (commit 150). Per Pro's
/// Batch T letter: "do not spend a commit on this
/// unless it starts leaking into production
/// construction."
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize)]
pub struct BzlFetchReportEntry {
    pub repo_name: String,
    /// One of: "invoked", "skipped_materialized",
    /// "skipped_materialized_manifest_valid" (149),
    /// "skipped_materialized_trust_mode"      (149),
    /// "skipped_known_rule",
    /// "materialized_missing_dir",
    /// "materialized_missing_build".
    pub decision: String,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub materialized_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub had_build: Option<bool>,
    /// Phase 5.C.4 commit 150 (Batch S): manifest
    /// status + provenance. Omitted from JSON when
    /// None. `skipped_with_manifest` and
    /// `skipped_by_trust_mode` default false; their
    /// sum is at most 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_reason: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skipped_with_manifest: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skipped_by_trust_mode: bool,
}

/// Phase 5.C.4 commit 89: schema version for the cache-
/// counter JSON wire format.
pub const ANALYZE_CACHE_COUNTERS_SCHEMA_VERSION: u32 = 1;

/// Phase 5.C.4 commit 89: builder used by the orchestrator
/// to populate counters incrementally.
#[derive(Debug, Default)]
pub struct AnalyzeCacheCountersBuilder {
    build_file_lookups: u64,
    build_file_hits: u64,
    build_file_misses: u64,
    external_repo_resolutions: u64,
    /// .bzl cache snapshot at start of analyze (subtract
    /// from end snapshot to compute delta).
    bzl_start: BzlSnapshot,
}

/// Internal snapshot of GlobalBzlCache state at a point
/// in time. Used to compute deltas across the analyze
/// pass. Commit 96 expanded this to include cold/repeat
/// misses + eviction-by-reason counters.
#[derive(Debug, Clone, Copy, Default)]
struct BzlSnapshot {
    hits: u64,
    misses: u64,
    evictions: u64,
    cold_misses: u64,
    repeat_misses: u64,
    evictions_by_entry_limit: u64,
    evictions_by_byte_limit: u64,
    evictions_manual: u64,
}

impl AnalyzeCacheCountersBuilder {
    /// Build a fresh builder, capturing the .bzl cache
    /// state at construction time. Counters reported in
    /// `finish` are deltas relative to this snapshot.
    pub fn new() -> Self {
        Self::default()
    }

    /// Capture the .bzl cache baseline snapshot. Pass
    /// hits / misses / evictions from
    /// `GlobalBzlCache::cache_stats()`. Commit 96 keeps
    /// this API for back-compat; commit 96 adds
    /// `snapshot_bzl_start_full` for the detailed
    /// counters.
    pub fn snapshot_bzl_start(
        &mut self,
        hits: u64,
        misses: u64,
        evictions: u64,
    ) {
        self.bzl_start = BzlSnapshot {
            hits,
            misses,
            evictions,
            ..self.bzl_start
        };
    }

    /// Phase 5.C.4 commit 96: capture the full
    /// .bzl-cache baseline snapshot (cold/repeat misses,
    /// eviction-by-reason counters). Caller passes the
    /// values from `GlobalBzlCache::cache_stats()`.
    pub fn snapshot_bzl_start_full(
        &mut self,
        hits: u64,
        misses: u64,
        evictions: u64,
        cold_misses: u64,
        repeat_misses: u64,
        evictions_by_entry_limit: u64,
        evictions_by_byte_limit: u64,
        evictions_manual: u64,
    ) {
        self.bzl_start = BzlSnapshot {
            hits,
            misses,
            evictions,
            cold_misses,
            repeat_misses,
            evictions_by_entry_limit,
            evictions_by_byte_limit,
            evictions_manual,
        };
    }

    /// Record one BUILD-file lookup that was satisfied by
    /// the orchestrator's in-pass dedup map (no re-parse
    /// needed).
    pub fn record_build_file_hit(&mut self) {
        self.build_file_lookups += 1;
        self.build_file_hits += 1;
    }

    /// Record one BUILD-file lookup that required a new
    /// parse.
    pub fn record_build_file_miss(&mut self) {
        self.build_file_lookups += 1;
        self.build_file_misses += 1;
    }

    /// Record one external-repo path resolution.
    pub fn record_external_repo_resolution(&mut self) {
        self.external_repo_resolutions += 1;
    }

    /// Finalize: compute .bzl cache deltas from the
    /// final snapshot and produce the immutable summary.
    /// Back-compat 3-arg variant (commit 89).
    pub fn finish(
        self,
        bzl_end_hits: u64,
        bzl_end_misses: u64,
        bzl_end_evictions: u64,
    ) -> AnalyzeCacheCounters {
        self.finish_full(
            bzl_end_hits,
            bzl_end_misses,
            bzl_end_evictions,
            None,
        )
    }

    /// Phase 5.C.4 commit 96: full-detail finalize.
    /// `end_full` carries the detailed end-state fields
    /// from `GlobalBzlCache::cache_stats()`. When `None`,
    /// the detailed deltas all read zero (back-compat
    /// path).
    pub fn finish_full(
        self,
        bzl_end_hits: u64,
        bzl_end_misses: u64,
        bzl_end_evictions: u64,
        end_full: Option<BzlEndFullSnapshot>,
    ) -> AnalyzeCacheCounters {
        let bzl_cache_hits = bzl_end_hits.saturating_sub(self.bzl_start.hits);
        let bzl_cache_misses = bzl_end_misses.saturating_sub(self.bzl_start.misses);
        let bzl_cache_evictions =
            bzl_end_evictions.saturating_sub(self.bzl_start.evictions);
        let bzl_total = bzl_cache_hits + bzl_cache_misses;
        let bzl_cache_hit_rate_percent = if bzl_total > 0 {
            Some((bzl_cache_hits as f64 / bzl_total as f64) * 100.0)
        } else {
            None
        };
        let (
            bzl_cold_misses,
            bzl_repeat_misses,
            bzl_evictions_by_entry_limit,
            bzl_evictions_by_byte_limit,
            bzl_evictions_manual,
            bzl_peak_entry_count,
            bzl_unique_files_seen,
            bzl_max_entries,
            bzl_max_memory_mb,
            bzl_peak_memory_mb,
            bzl_top_slowest_cold_loads,
            bzl_top_by_repeat_misses,
            bzl_top_by_fanout,
            bzl_top_by_reverse_dependents,
            bzl_identity_collisions,
            bzl_top_by_starlark_ops,
            bzl_total_function_calls,
            bzl_total_builtin_calls,
            bzl_total_rule_invocations,
            bzl_total_macro_invocations,
            bzl_total_glob_calls,
            bzl_estimate_symbols_size_calls,
            bzl_estimate_symbols_size_total_ms,
            bzl_repeat_reload_estimate_candidates,
            bzl_repeat_reload_estimate_reuse_hits,
            bzl_repeat_reload_estimate_reuse_misses,
            bzl_estimate_verification_mismatches,
            bzl_async_spawn_wait_ms,
            bzl_async_spawn_wait_calls,
            bzl_fs_metadata_wait_ms,
            bzl_fs_metadata_wait_calls,
            bzl_loader_lock_wait_ms,
            bzl_loader_lock_wait_calls,
            bzl_concurrent_eval_wait_ms,
            bzl_concurrent_eval_wait_calls,
            bzl_bazel_fetch_wait_ms,
            bzl_bazel_fetch_wait_calls,
            bzl_fetch_invoked_count,
            bzl_fetch_skipped_materialized_count,
            bzl_fetch_skipped_known_rule_count,
            bzl_fetch_materialized_missing_dir_count,
            bzl_fetch_materialized_missing_build_count,
            bzl_fetch_duration_by_repo,
            bzl_fetch_report,
        ) = if let Some(e) = end_full {
            (
                e.cold_misses.saturating_sub(self.bzl_start.cold_misses),
                e.repeat_misses.saturating_sub(self.bzl_start.repeat_misses),
                e.evictions_by_entry_limit
                    .saturating_sub(self.bzl_start.evictions_by_entry_limit),
                e.evictions_by_byte_limit
                    .saturating_sub(self.bzl_start.evictions_by_byte_limit),
                e.evictions_manual
                    .saturating_sub(self.bzl_start.evictions_manual),
                e.peak_entry_count,
                e.unique_files_seen,
                e.max_entries,
                e.max_memory_mb,
                e.peak_memory_mb,
                e.top_slowest_cold_loads,
                e.top_by_repeat_misses,
                e.top_by_fanout,
                e.top_by_reverse_dependents,
                e.identity_collisions,
                e.top_by_starlark_ops,
                e.total_function_calls,
                e.total_builtin_calls,
                e.total_rule_invocations,
                e.total_macro_invocations,
                e.total_glob_calls,
                e.estimate_symbols_size_calls,
                e.estimate_symbols_size_total_ms,
                e.repeat_reload_estimate_candidates,
                e.repeat_reload_estimate_reuse_hits,
                e.repeat_reload_estimate_reuse_misses,
                e.estimate_verification_mismatches,
                e.async_spawn_wait_ms,
                e.async_spawn_wait_calls,
                e.fs_metadata_wait_ms,
                e.fs_metadata_wait_calls,
                e.loader_lock_wait_ms,
                e.loader_lock_wait_calls,
                e.concurrent_eval_wait_ms,
                e.concurrent_eval_wait_calls,
                e.bazel_fetch_wait_ms,
                e.bazel_fetch_wait_calls,
                e.fetch_invoked_count,
                e.fetch_skipped_materialized_count,
                e.fetch_skipped_known_rule_count,
                e.fetch_materialized_missing_dir_count,
                e.fetch_materialized_missing_build_count,
                e.fetch_duration_by_repo,
                e.fetch_report,
            )
        } else {
            (
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                0,
                0,
                0,
                0,
                0,
                0, 0, 0, 0, 0, 0,
                0, 0, 0, 0,
                0, 0,
                0, 0,
                0, 0,
                // commit 134: fetch decision counters + per-repo map
                0, 0, 0, 0, 0,
                std::collections::BTreeMap::new(),
                // commit 135: per-repo report
                Vec::new(),
            )
        };
        AnalyzeCacheCounters {
            schema_version: ANALYZE_CACHE_COUNTERS_SCHEMA_VERSION,
            build_file_lookups: self.build_file_lookups,
            build_file_hits: self.build_file_hits,
            build_file_misses: self.build_file_misses,
            bzl_cache_hits,
            bzl_cache_misses,
            bzl_cache_evictions,
            bzl_cache_hit_rate_percent,
            external_repo_resolutions: self.external_repo_resolutions,
            bzl_top_slowest_cold_loads,
            bzl_top_by_repeat_misses,
            bzl_cold_misses,
            bzl_repeat_misses,
            bzl_evictions_by_entry_limit,
            bzl_evictions_by_byte_limit,
            bzl_evictions_manual,
            bzl_peak_entry_count,
            bzl_unique_files_seen,
            bzl_max_entries,
            bzl_max_memory_mb,
            bzl_peak_memory_mb,
            bzl_top_by_fanout,
            bzl_top_by_reverse_dependents,
            bzl_identity_collisions,
            bzl_top_by_starlark_ops,
            bzl_total_function_calls,
            bzl_total_builtin_calls,
            bzl_total_rule_invocations,
            bzl_total_macro_invocations,
            bzl_total_glob_calls,
            bzl_estimate_symbols_size_calls,
            bzl_estimate_symbols_size_total_ms,
            bzl_repeat_reload_estimate_candidates,
            bzl_repeat_reload_estimate_reuse_hits,
            bzl_repeat_reload_estimate_reuse_misses,
            bzl_estimate_verification_mismatches,
            bzl_async_spawn_wait_ms,
            bzl_async_spawn_wait_calls,
            bzl_fs_metadata_wait_ms,
            bzl_fs_metadata_wait_calls,
            bzl_loader_lock_wait_ms,
            bzl_loader_lock_wait_calls,
            bzl_concurrent_eval_wait_ms,
            bzl_concurrent_eval_wait_calls,
            bzl_bazel_fetch_wait_ms,
            bzl_bazel_fetch_wait_calls,
            bzl_fetch_invoked_count,
            bzl_fetch_skipped_materialized_count,
            bzl_fetch_skipped_known_rule_count,
            bzl_fetch_materialized_missing_dir_count,
            bzl_fetch_materialized_missing_build_count,
            bzl_fetch_duration_by_repo,
            bzl_fetch_report,
        }
    }
}

/// Phase 5.C.4 commit 103/104: per-`.bzl` cold-load
/// report entry. Embedded in
/// `AnalyzeCacheCounters.bzl_top_slowest_cold_loads`.
///
/// Operators investigating analyze-pass cost start at
/// the highest-duration entries.
///
/// Commit 104 splits cold-load duration into:
///   - `cumulative_cold_load_ms`: total wall-clock time
///     for this `.bzl` load INCLUDING transitive child
///     loads triggered by Starlark `load()` calls. Same
///     value as commit-103's `cold_load_ms` field.
///   - `own_cold_load_ms`: cumulative minus the sum of
///     immediate-child cumulative durations. Emitted
///     only when the thread-local frame stack saw no
///     recursive loads (Option = None otherwise).
///   - `child_load_ms`: sum of immediate-child
///     cumulative durations. Same Option semantics.
///   - `subphase_*_ms`: per-subphase wall time (read /
///     parse / eval / finalize). `eval` INCLUDES
///     transitive child load time.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BzlColdLoadEntry {
    /// Absolute path of the `.bzl` file.
    pub path: String,
    /// Cumulative cold-load wall-clock duration in
    /// milliseconds (sum across `cold_load_count`
    /// loads), INCLUDING transitive child loads.
    pub cumulative_cold_load_ms: u64,
    /// Number of distinct cold loads (1 if loaded once;
    /// >1 if evicted then re-loaded).
    pub cold_load_count: u64,
    /// On-disk `.bzl` file size in bytes.
    pub file_size_bytes: u64,
    /// Number of symbols exported by the most recent
    /// successful load.
    pub exported_symbols_count: u64,
    /// Phase 5.C.4 commit 104: own time (cumulative -
    /// child). None when child accounting was
    /// unreliable (recursive load on same path).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub own_cold_load_ms: Option<u64>,
    /// Phase 5.C.4 commit 104: sum of immediate-child
    /// cumulative durations. None alongside
    /// `own_cold_load_ms` when accounting unreliable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub child_load_ms: Option<u64>,
    /// Phase 5.C.4 commit 104: wall time for the file-
    /// read subphase (`std::fs::read_to_string`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subphase_read_ms: Option<u64>,
    /// Phase 5.C.4 commit 104: wall time for the
    /// Starlark parse subphase.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subphase_parse_ms: Option<u64>,
    /// Phase 5.C.4 commit 104: wall time for the
    /// Starlark eval subphase. INCLUDES time spent in
    /// transitive child `.bzl` loads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subphase_eval_ms: Option<u64>,
    /// Phase 5.C.4 commit 104: wall time for the
    /// finalize subphase (lazy-load resolution + cache
    /// insert).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subphase_finalize_ms: Option<u64>,
    /// Phase 5.C.4 commit 111: sub-finalize split.
    /// Wall time inside `resolve_lazy_loads`. Bounded
    /// above by `subphase_finalize_ms`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subphase_resolve_lazy_loads_ms: Option<u64>,
    /// Phase 5.C.4 commit 111: sub-finalize split.
    /// Wall time inside `put_symbols_cache`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subphase_put_symbols_cache_ms: Option<u64>,
    /// Phase 5.C.4 commit 110: attribution quality of
    /// the most recent cold-load for this path. Values:
    /// `same_thread_complete`,
    /// `cross_thread_children_observed`,
    /// `recursion_suppressed`, `unknown`. When this is
    /// anything other than `same_thread_complete`,
    /// callers MUST NOT present `own_cold_load_ms` as
    /// precise (Pro: "no fake own-time").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attribution: Option<String>,
}
/// summary entry. Embedded in
/// `AnalyzeCacheCounters.bzl_top_by_fanout` /
/// `..._top_by_reverse_dependents`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BzlLoadGraphEntry {
    /// Resolved absolute path of the `.bzl` file.
    pub path: String,
    /// Count of distinct `.bzl` files this file directly
    /// `load()`s (forward fanout).
    pub direct_children_count: usize,
    /// Count of distinct `.bzl` files that directly
    /// `load()` this file (reverse fanout).
    pub direct_parents_count: usize,
    /// Deepest observed cold-load frame stack height
    /// for this file. 1 = top of chain (loaded from
    /// BUILD with no .bzl parent on stack). Useful for
    /// distinguishing "deep chain" from "wide fanout"
    /// thrash patterns.
    pub max_observed_depth: usize,
    /// On-disk `.bzl` file size in bytes (echoed for
    /// readability; same value carried by
    /// `BzlColdLoadEntry`).
    pub file_size_bytes: u64,
}

/// Phase 5.C.4 commit 106: per-`.bzl` coarse Starlark-
/// operation summary. Embedded in
/// `AnalyzeCacheCounters.bzl_top_by_starlark_ops`.
///
/// NOT a profiler — just five integer counters per
/// .bzl. Operators looking for "where Starlark
/// evaluation spends its op budget" use these to
/// distinguish macro-heavy files, glob-heavy files,
/// and rule-dispatch-heavy files.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BzlStarlarkOpsEntry {
    /// Resolved absolute path.
    pub path: String,
    /// User-defined `def`-function call count.
    pub function_calls: u64,
    /// Built-in function call count (registered
    /// natives like `len`, `select`, `provider`).
    pub builtin_calls: u64,
    /// Rule-call captures (e.g. `cc_library(...)`).
    pub rule_invocations: u64,
    /// Macro-expander invocations.
    pub macro_invocations: u64,
    /// `glob()` calls — split from builtin_calls so
    /// the filesystem-heavy operations are visible at
    /// a glance.
    pub glob_calls: u64,
    /// Sum across all five categories.
    pub total_ops: u64,
}

/// Phase 5.C.4 commit 105: identity-collision entry —
/// a single resolved path reached via multiple raw
/// `load()` labels.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BzlIdentityCollisionEntry {
    /// Resolved absolute path.
    pub path: String,
    /// All distinct raw `load()` labels (and other
    /// entry strings) that resolved to this path. Sort
    /// is alphabetical for determinism.
    pub raw_identities: Vec<String>,
}

/// Phase 5.C.4 commit 171 (Batch V): repeat-miss
/// hotspot entry for a single `.bzl` path.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BzlRepeatMissEntry {
    /// Resolved absolute path.
    pub path: String,
    /// Total lookup attempts recorded for the path.
    pub lookups: u64,
    /// Non-cold misses after an earlier eviction.
    pub repeat_misses: u64,
    /// Total misses for the path.
    pub misses: u64,
}

/// Phase 5.C.4 commit 96 / 103: end-state snapshot for
/// the detailed .bzl cache stats. Passed to
/// `finish_full` from the orchestrator after the
/// analyze pass.
#[derive(Debug, Clone, Default)]
pub struct BzlEndFullSnapshot {
    pub cold_misses: u64,
    pub repeat_misses: u64,
    pub evictions_by_entry_limit: u64,
    pub evictions_by_byte_limit: u64,
    pub evictions_manual: u64,
    pub peak_entry_count: u64,
    pub unique_files_seen: u64,
    pub max_entries: u64,
    pub max_memory_mb: u64,
    pub peak_memory_mb: u64,
    /// Phase 5.C.4 commit 103: pre-computed top-N
    /// slowest cold-load entries. Orchestrator builds
    /// from `GlobalBzlCache::top_n_by_cold_load_duration`.
    pub top_slowest_cold_loads: Vec<BzlColdLoadEntry>,
    /// Phase 5.C.4 commit 171 (Batch V): pre-computed top-N
    /// by repeat misses (`PerPathStats::repeat_misses`).
    pub top_by_repeat_misses: Vec<BzlRepeatMissEntry>,
    /// Phase 5.C.4 commit 105: pre-computed top-N by
    /// forward fanout (`direct_children_count`).
    pub top_by_fanout: Vec<BzlLoadGraphEntry>,
    /// Phase 5.C.4 commit 105: pre-computed top-N by
    /// reverse fanout (`direct_parents_count`).
    pub top_by_reverse_dependents: Vec<BzlLoadGraphEntry>,
    /// Phase 5.C.4 commit 105: identity-collision
    /// entries (one per resolved path with >1 raw
    /// `load()` label).
    pub identity_collisions: Vec<BzlIdentityCollisionEntry>,
    /// Phase 5.C.4 commit 106: top-N by Starlark-op
    /// volume.
    pub top_by_starlark_ops: Vec<BzlStarlarkOpsEntry>,
    /// Phase 5.C.4 commit 106: process-wide op totals.
    pub total_function_calls: u64,
    pub total_builtin_calls: u64,
    pub total_rule_invocations: u64,
    pub total_macro_invocations: u64,
    pub total_glob_calls: u64,
    /// Phase 5.C.4 commit 118: estimate_symbols_size
    /// metrics (totals + repeat-miss accounting).
    pub estimate_symbols_size_calls: u64,
    pub estimate_symbols_size_total_ms: u64,
    pub repeat_reload_estimate_candidates: u64,
    pub repeat_reload_estimate_reuse_hits: u64,
    pub repeat_reload_estimate_reuse_misses: u64,
    pub estimate_verification_mismatches: u64,
    /// Phase 5.C.4 commit 126 (Batch P): wait
    /// category counters.
    pub async_spawn_wait_ms: u64,
    pub async_spawn_wait_calls: u64,
    pub fs_metadata_wait_ms: u64,
    pub fs_metadata_wait_calls: u64,
    /// Phase 5.C.4 commit 133 (Batch Q): BzlLoader
    /// Mutex wait time.
    pub loader_lock_wait_ms: u64,
    pub loader_lock_wait_calls: u64,
    pub concurrent_eval_wait_ms: u64,
    pub concurrent_eval_wait_calls: u64,
    pub bazel_fetch_wait_ms: u64,
    pub bazel_fetch_wait_calls: u64,
    /// Phase 5.C.4 commit 134 (Batch Q): fetch
    /// decision counters.
    pub fetch_invoked_count: u64,
    pub fetch_skipped_materialized_count: u64,
    pub fetch_skipped_known_rule_count: u64,
    pub fetch_materialized_missing_dir_count: u64,
    pub fetch_materialized_missing_build_count: u64,
    pub fetch_duration_by_repo:
        std::collections::BTreeMap<String, u64>,
    /// Phase 5.C.4 commit 135 (Batch Q): per-repo
    /// fetch report. Sorted by repo_name (stable
    /// JSON diffs).
    pub fetch_report: Vec<BzlFetchReportEntry>,
}

impl AnalyzeCacheCounters {
    /// True iff no observations recorded (zero analyze
    /// work).
    pub fn is_empty(&self) -> bool {
        self.build_file_lookups == 0
            && self.bzl_cache_hits == 0
            && self.bzl_cache_misses == 0
            && self.external_repo_resolutions == 0
    }

    /// Render as human-readable stderr lines.
    pub fn format_human(&self) -> Vec<String> {
        let mut out = Vec::new();
        out.push(format!(
            "analyze-pass cache counters (schema_version={}):",
            self.schema_version
        ));
        out.push(format!(
            "  build_file_lookups:        {}", self.build_file_lookups
        ));
        out.push(format!(
            "    hits (in-pass dedup):    {}", self.build_file_hits
        ));
        out.push(format!(
            "    misses (parsed fresh):   {}", self.build_file_misses
        ));
        out.push(format!(
            "  bzl_cache_hits:            {}", self.bzl_cache_hits
        ));
        out.push(format!(
            "  bzl_cache_misses:          {}", self.bzl_cache_misses
        ));
        // Phase 5.C.4 commit 96: cold vs repeat miss
        // discrimination. Repeat misses are the
        // thrashing signal.
        if self.bzl_cold_misses + self.bzl_repeat_misses > 0 {
            out.push(format!(
                "    cold_misses:             {}",
                self.bzl_cold_misses
            ));
            out.push(format!(
                "    repeat_misses (thrash):  {}{}",
                self.bzl_repeat_misses,
                if self.bzl_repeat_misses > 0 {
                    "  ← .bzl thrash detected"
                } else {
                    ""
                },
            ));
        }
        // Phase 5.C.4 commit 171 (Batch V): top-N by
        // repeat misses (post-eviction thrash signal).
        if !self.bzl_top_by_repeat_misses.is_empty() {
            out.push("  top_by_repeat_misses (repeat-miss hot spots):".to_string());
            for e in &self.bzl_top_by_repeat_misses {
                let basename = std::path::Path::new(&e.path)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| e.path.clone());
                out.push(format!(
                    "    {:50} {} repeats ({} lookups, {} misses)",
                    basename,
                    e.repeat_misses,
                    e.lookups,
                    e.misses
                ));
            }
        }
        out.push(format!(
            "  bzl_cache_evictions:       {}", self.bzl_cache_evictions
        ));
        // Phase 5.C.4 commit 96: eviction reason
        // breakdown. Entry-bound vs byte-bound tells
        // operators which cache limit to tune.
        if self.bzl_evictions_by_entry_limit
            + self.bzl_evictions_by_byte_limit
            + self.bzl_evictions_manual
            > 0
        {
            out.push(format!(
                "    by_entry_limit:          {}",
                self.bzl_evictions_by_entry_limit
            ));
            out.push(format!(
                "    by_byte_limit:           {}",
                self.bzl_evictions_by_byte_limit
            ));
            out.push(format!(
                "    manual:                  {}",
                self.bzl_evictions_manual
            ));
        }
        if let Some(rate) = self.bzl_cache_hit_rate_percent {
            out.push(format!("  bzl_cache_hit_rate:        {:.1}%", rate));
        }
        // Phase 5.C.4 commit 96: configured limits +
        // high-water marks. Operators tuning caps need
        // to see both current configuration and observed
        // pressure.
        if self.bzl_max_entries > 0 {
            out.push(format!(
                "  bzl_max_entries:           {} (peak observed: {})",
                self.bzl_max_entries, self.bzl_peak_entry_count
            ));
        }
        if self.bzl_max_memory_mb > 0 {
            out.push(format!(
                "  bzl_max_memory_mb:         {} (peak observed: {})",
                self.bzl_max_memory_mb, self.bzl_peak_memory_mb
            ));
        }
        if self.bzl_unique_files_seen > 0 {
            out.push(format!(
                "  bzl_unique_files_seen:     {}",
                self.bzl_unique_files_seen
            ));
        }
        out.push(format!(
            "  external_repo_resolutions: {}",
            self.external_repo_resolutions
        ));
        // Phase 5.C.4 commit 103: top-N slowest cold-load
        // entries. Operators investigating analyze-pass
        // cost start with the entries at the top.
        // Phase 5.C.4 commit 105: load-graph summary.
        // Forward fanout — files this .bzl directly
        // `load()`s.
        if !self.bzl_top_by_fanout.is_empty() {
            out.push("  top_by_fanout (forward, direct children):".to_string());
            for e in &self.bzl_top_by_fanout {
                let basename = std::path::Path::new(&e.path)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| e.path.clone());
                out.push(format!(
                    "    {:50} {} children (depth max {}, {} KB)",
                    basename,
                    e.direct_children_count,
                    e.max_observed_depth,
                    e.file_size_bytes / 1024,
                ));
            }
        }
        // Reverse fanout — files that directly load this
        // .bzl. Big reverse-fanout = pre-WORKSPACE root.
        if !self.bzl_top_by_reverse_dependents.is_empty() {
            out.push("  top_by_reverse_dependents (loaded by N others):".to_string());
            for e in &self.bzl_top_by_reverse_dependents {
                let basename = std::path::Path::new(&e.path)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| e.path.clone());
                out.push(format!(
                    "    {:50} loaded-by {} (depth max {}, {} KB)",
                    basename,
                    e.direct_parents_count,
                    e.max_observed_depth,
                    e.file_size_bytes / 1024,
                ));
            }
        }
        // Identity-collision summary: same resolved path
        // reached via different raw label strings.
        if !self.bzl_identity_collisions.is_empty() {
            out.push("  identity_collisions:".to_string());
            for c in &self.bzl_identity_collisions {
                let basename = std::path::Path::new(&c.path)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| c.path.clone());
                out.push(format!(
                    "    {:50} {} distinct labels",
                    basename,
                    c.raw_identities.len(),
                ));
                for label in &c.raw_identities {
                    out.push(format!("      ↪ {}", label));
                }
            }
        }
        // Phase 5.C.4 commit 118: estimate_symbols_size
        // accounting line. Skip when zero so legacy
        // runs without the metric don't get noise.
        if self.bzl_estimate_symbols_size_calls > 0
            || self.bzl_repeat_reload_estimate_candidates > 0
        {
            out.push(format!(
                "  estimate_symbols_size: calls={} total_ms={} avg_ms={}",
                self.bzl_estimate_symbols_size_calls,
                self.bzl_estimate_symbols_size_total_ms,
                if self.bzl_estimate_symbols_size_calls > 0 {
                    self.bzl_estimate_symbols_size_total_ms
                        / self.bzl_estimate_symbols_size_calls
                } else {
                    0
                },
            ));
            out.push(format!(
                "    repeat-miss candidates: {} (reuse: {} hits / {} misses, verify mismatches: {})",
                self.bzl_repeat_reload_estimate_candidates,
                self.bzl_repeat_reload_estimate_reuse_hits,
                self.bzl_repeat_reload_estimate_reuse_misses,
                self.bzl_estimate_verification_mismatches,
            ));
        }
        // Phase 5.C.4 commit 126 (Batch P): wait-
        // category counters. Skip when zero to keep
        // legacy reports clean.
        if self.bzl_async_spawn_wait_calls > 0
            || self.bzl_fs_metadata_wait_calls > 0
            || self.bzl_loader_lock_wait_calls > 0
        {
            out.push(format!(
                "  wait_categories: async_spawn_recv={} ms ({} calls)  fs_metadata={} ms ({} calls)",
                self.bzl_async_spawn_wait_ms,
                self.bzl_async_spawn_wait_calls,
                self.bzl_fs_metadata_wait_ms,
                self.bzl_fs_metadata_wait_calls,
            ));
            if self.bzl_loader_lock_wait_calls > 0 {
                let avg_ms = self
                    .bzl_loader_lock_wait_ms
                    .checked_div(self.bzl_loader_lock_wait_calls)
                    .unwrap_or(0);
                out.push(format!(
                    "    loader_arc_lock_wait={} ms ({} calls, avg {} ms)",
                    self.bzl_loader_lock_wait_ms,
                    self.bzl_loader_lock_wait_calls,
                    avg_ms,
                ));
            }
            if self.bzl_concurrent_eval_wait_calls > 0 {
                let avg_ms = self
                    .bzl_concurrent_eval_wait_ms
                    .checked_div(self.bzl_concurrent_eval_wait_calls)
                    .unwrap_or(0);
                out.push(format!(
                    "    concurrent_eval_polling_wait={} ms ({} calls, avg {} ms)  ← 100ms-sleep loop",
                    self.bzl_concurrent_eval_wait_ms,
                    self.bzl_concurrent_eval_wait_calls,
                    avg_ms,
                ));
            }
            if self.bzl_bazel_fetch_wait_calls > 0 {
                let avg_ms = self
                    .bzl_bazel_fetch_wait_ms
                    .checked_div(self.bzl_bazel_fetch_wait_calls)
                    .unwrap_or(0);
                out.push(format!(
                    "    bazel_fetch_subprocess_wait={} ms ({} calls, avg {} ms)  ← THE 90s",
                    self.bzl_bazel_fetch_wait_ms,
                    self.bzl_bazel_fetch_wait_calls,
                    avg_ms,
                ));
            }
        }

        // Phase 5.C.4 commit 134 (Batch Q): fetch
        // decision counters. Always emitted when any
        // fetch-decision was recorded, so operators can
        // see whether
        // `RUSH_FETCH_SKIP_IF_MATERIALIZED=1` is firing
        // and which decision distribution this analyze
        // produced.
        let any_decision = self.bzl_fetch_invoked_count
            + self.bzl_fetch_skipped_materialized_count
            + self.bzl_fetch_skipped_known_rule_count
            + self.bzl_fetch_materialized_missing_dir_count
            + self.bzl_fetch_materialized_missing_build_count;
        if any_decision > 0 {
            out.push(format!(
                "  fetch_decisions: invoked={} skipped_materialized={} skipped_known_rule={} missing_dir={} missing_build={}",
                self.bzl_fetch_invoked_count,
                self.bzl_fetch_skipped_materialized_count,
                self.bzl_fetch_skipped_known_rule_count,
                self.bzl_fetch_materialized_missing_dir_count,
                self.bzl_fetch_materialized_missing_build_count,
            ));
        }

        // Phase 5.C.4 commit 135 (Batch Q): per-repo
        // fetch attribution report. Two sections:
        //   1. Top-N repos by `Invoked` duration
        //      (descending). What spent wall time.
        //   2. The full materialized-skip count summary
        //      already covered by the
        //      fetch_decisions line above, so this
        //      section only shows the costly repos.
        if !self.bzl_fetch_report.is_empty() {
            let mut by_duration: Vec<&BzlFetchReportEntry> = self
                .bzl_fetch_report
                .iter()
                .filter(|e| e.duration_ms > 0)
                .collect();
            by_duration.sort_by(|a, b| {
                b.duration_ms
                    .cmp(&a.duration_ms)
                    .then_with(|| a.repo_name.cmp(&b.repo_name))
            });
            if !by_duration.is_empty() {
                out.push(format!(
                    "  top_fetch_duration ({} repos invoked):",
                    by_duration.len()
                ));
                for entry in by_duration.iter().take(10) {
                    out.push(format!(
                        "    @{:<40} {:>6} ms  ({})",
                        entry.repo_name, entry.duration_ms, entry.decision,
                    ));
                }
            }
        }

        // Phase 5.C.4 commit 106: process-wide
        // Starlark-op totals + top-N by ops volume.
        let any_ops_total = self.bzl_total_function_calls
            + self.bzl_total_builtin_calls
            + self.bzl_total_rule_invocations
            + self.bzl_total_macro_invocations
            + self.bzl_total_glob_calls;
        if any_ops_total > 0 {
            out.push(format!(
                "  starlark_op_totals: function={} builtin={} rule={} macro={} glob={}",
                self.bzl_total_function_calls,
                self.bzl_total_builtin_calls,
                self.bzl_total_rule_invocations,
                self.bzl_total_macro_invocations,
                self.bzl_total_glob_calls,
            ));
        }
        if !self.bzl_top_by_starlark_ops.is_empty() {
            out.push("  top_by_starlark_ops:".to_string());
            for e in &self.bzl_top_by_starlark_ops {
                let basename = std::path::Path::new(&e.path)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| e.path.clone());
                out.push(format!(
                    "    {:50} total={} (fn={} builtin={} rule={} macro={} glob={})",
                    basename,
                    e.total_ops,
                    e.function_calls,
                    e.builtin_calls,
                    e.rule_invocations,
                    e.macro_invocations,
                    e.glob_calls,
                ));
            }
        }
        if !self.bzl_top_slowest_cold_loads.is_empty() {
            out.push("  top_slowest_cold_loads:".to_string());
            for e in &self.bzl_top_slowest_cold_loads {
                let basename = std::path::Path::new(&e.path)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| e.path.clone());
                // Phase 5.C.4 commit 104: render cumulative
                // plus an `own=…` suffix when available.
                let own_suffix = match e.own_cold_load_ms {
                    Some(own) => format!(" [own {} ms]", own),
                    None => " [own n/a]".to_string(),
                };
                out.push(format!(
                    "    {:50} {} ms cumulative{}  (× {} cold, {} KB, {} symbols)",
                    basename,
                    e.cumulative_cold_load_ms,
                    own_suffix,
                    e.cold_load_count,
                    e.file_size_bytes / 1024,
                    e.exported_symbols_count,
                ));
                // Phase 5.C.4 commit 104: subphase
                // breakdown line, only when any subphase
                // total is present.
                if e.subphase_read_ms.is_some()
                    || e.subphase_parse_ms.is_some()
                    || e.subphase_eval_ms.is_some()
                    || e.subphase_finalize_ms.is_some()
                {
                    out.push(format!(
                        "      subphases: read={} parse={} eval={} finalize={} (ms)",
                        e.subphase_read_ms.map(|v| v.to_string()).unwrap_or_else(|| "n/a".into()),
                        e.subphase_parse_ms.map(|v| v.to_string()).unwrap_or_else(|| "n/a".into()),
                        e.subphase_eval_ms.map(|v| v.to_string()).unwrap_or_else(|| "n/a".into()),
                        e.subphase_finalize_ms.map(|v| v.to_string()).unwrap_or_else(|| "n/a".into()),
                    ));
                    // Phase 5.C.4 commit 111: sub-finalize
                    // split shown on its own line when
                    // either component is recorded.
                    if e.subphase_resolve_lazy_loads_ms.is_some()
                        || e.subphase_put_symbols_cache_ms.is_some()
                    {
                        out.push(format!(
                            "        finalize: resolve_lazy_loads={} put_symbols_cache={} (ms)",
                            e.subphase_resolve_lazy_loads_ms.map(|v| v.to_string()).unwrap_or_else(|| "n/a".into()),
                            e.subphase_put_symbols_cache_ms.map(|v| v.to_string()).unwrap_or_else(|| "n/a".into()),
                        ));
                    }
                }
            }
        }
        out
    }
}

/// Phase 5.C.4 commit 89: cache-counter gate.
pub fn cache_counters_enabled() -> bool {
    std::env::var("RUSH_ANALYZE_CACHE_COUNTERS").as_deref() == Ok("1")
}

/// Phase 5.C.4 commit 89: cache-counter JSON gate.
pub fn cache_counters_json_enabled() -> bool {
    cache_counters_enabled()
        && std::env::var("RUSH_ANALYZE_CACHE_COUNTERS_JSON").as_deref()
            == Ok("1")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pro's commit-89 directive: BUILD-file cache hit count
    /// increments on each recorded hit.
    #[test]
    fn build_file_cache_hit_count_increments() {
        let mut b = AnalyzeCacheCountersBuilder::new();
        b.record_build_file_hit();
        b.record_build_file_hit();
        b.record_build_file_hit();
        let c = b.finish(0, 0, 0);
        assert_eq!(c.build_file_hits, 3);
        assert_eq!(c.build_file_lookups, 3);
        assert_eq!(c.build_file_misses, 0);
    }

    /// Pro's commit-89 directive: miss count increments
    /// separately from hits.
    #[test]
    fn build_file_cache_miss_count_increments() {
        let mut b = AnalyzeCacheCountersBuilder::new();
        b.record_build_file_miss();
        b.record_build_file_miss();
        b.record_build_file_hit();
        let c = b.finish(0, 0, 0);
        assert_eq!(c.build_file_misses, 2);
        assert_eq!(c.build_file_hits, 1);
        assert_eq!(c.build_file_lookups, 3);
    }

    /// Pro's commit-89 directive: "repeated_lookup_hits_cache".
    /// Pin that recording a hit after a miss surfaces the
    /// hit-rate signal.
    #[test]
    fn repeated_lookup_hits_cache() {
        let mut b = AnalyzeCacheCountersBuilder::new();
        b.record_build_file_miss();
        for _ in 0..9 {
            b.record_build_file_hit();
        }
        let c = b.finish(0, 0, 0);
        // 1 miss + 9 hits → 90% hit rate.
        assert_eq!(c.build_file_lookups, 10);
        assert_eq!(c.build_file_hits, 9);
        assert_eq!(c.build_file_misses, 1);
    }

    /// Pro's commit-89 directive: cache_summary_json_has_schema_version.
    #[test]
    fn cache_summary_json_has_schema_version() {
        let b = AnalyzeCacheCountersBuilder::new();
        let c = b.finish(0, 0, 0);
        let json = serde_json::to_string(&c).expect("serialize");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["schema_version"], 1);
    }

    /// Phase 5.C.4 commit 89: .bzl cache deltas computed
    /// from start/end snapshots. Pin that
    /// `bzl_cache_hits` reflects the DELTA, not the
    /// absolute end count.
    #[test]
    fn bzl_cache_deltas_computed_from_snapshots() {
        let mut b = AnalyzeCacheCountersBuilder::new();
        b.snapshot_bzl_start(100, 50, 5);
        // During this analyze pass: +47 hits, +3 misses,
        // +1 eviction.
        let c = b.finish(147, 53, 6);
        assert_eq!(c.bzl_cache_hits, 47);
        assert_eq!(c.bzl_cache_misses, 3);
        assert_eq!(c.bzl_cache_evictions, 1);
        // Hit rate = 47 / (47+3) = 94%.
        assert!(c.bzl_cache_hit_rate_percent.unwrap() > 93.9);
        assert!(c.bzl_cache_hit_rate_percent.unwrap() < 94.1);
    }

    /// Phase 5.C.4 commit 89: hit rate is None when no
    /// bzl lookups happened (avoids division-by-zero +
    /// avoids the wire shape suggesting "0% hit rate"
    /// when reality is "no data").
    #[test]
    fn bzl_hit_rate_none_when_no_lookups() {
        let b = AnalyzeCacheCountersBuilder::new();
        let c = b.finish(0, 0, 0);
        assert!(c.bzl_cache_hit_rate_percent.is_none());
        let json = serde_json::to_string(&c).expect("serialize");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("parse");
        assert!(
            parsed.get("bzl_cache_hit_rate_percent").is_none(),
            "field must be absent in JSON when None"
        );
    }

    /// Phase 5.C.4 commit 89: external-repo resolution
    /// counter increments per call.
    #[test]
    fn external_repo_resolutions_count() {
        let mut b = AnalyzeCacheCountersBuilder::new();
        for _ in 0..4 {
            b.record_external_repo_resolution();
        }
        let c = b.finish(0, 0, 0);
        assert_eq!(c.external_repo_resolutions, 4);
    }

    /// Phase 5.C.4 commit 89: is_empty true when no
    /// counters fired.
    #[test]
    fn is_empty_when_no_observations() {
        let b = AnalyzeCacheCountersBuilder::new();
        let c = b.finish(0, 0, 0);
        assert!(c.is_empty());
    }

    /// Phase 5.C.4 commit 96: cold vs repeat-miss delta
    /// computed from snapshots.
    #[test]
    fn bzl_cache_records_cold_miss() {
        let mut b = AnalyzeCacheCountersBuilder::new();
        b.snapshot_bzl_start_full(0, 0, 0, 0, 0, 0, 0, 0);
        // End state: 10 cold misses, no repeats yet.
        let c = b.finish_full(
            0,
            10,
            0,
            Some(BzlEndFullSnapshot {
                cold_misses: 10,
                repeat_misses: 0,
                evictions_by_entry_limit: 0,
                evictions_by_byte_limit: 0,
                evictions_manual: 0,
                peak_entry_count: 8,
                unique_files_seen: 10,
                max_entries: 50,
                max_memory_mb: 512,
                peak_memory_mb: 42,
                top_slowest_cold_loads: Vec::new(),
                top_by_repeat_misses: Vec::new(),
                top_by_fanout: Vec::new(),
                top_by_reverse_dependents: Vec::new(),
                identity_collisions: Vec::new(),
                top_by_starlark_ops: Vec::new(),
                total_function_calls: 0,
                total_builtin_calls: 0,
                total_rule_invocations: 0,
                total_macro_invocations: 0,
                total_glob_calls: 0,
                estimate_symbols_size_calls: 0,
                estimate_symbols_size_total_ms: 0,
                repeat_reload_estimate_candidates: 0,
                repeat_reload_estimate_reuse_hits: 0,
                repeat_reload_estimate_reuse_misses: 0,
                estimate_verification_mismatches: 0,
                async_spawn_wait_ms: 0,
                async_spawn_wait_calls: 0,
                fs_metadata_wait_ms: 0,
                fs_metadata_wait_calls: 0,
                loader_lock_wait_ms: 0,
                loader_lock_wait_calls: 0,
                concurrent_eval_wait_ms: 0,
                concurrent_eval_wait_calls: 0,
                bazel_fetch_wait_ms: 0,
                bazel_fetch_wait_calls: 0,
                fetch_invoked_count: 0,
                fetch_skipped_materialized_count: 0,
                fetch_skipped_known_rule_count: 0,
                fetch_materialized_missing_dir_count: 0,
                fetch_materialized_missing_build_count: 0,
                fetch_duration_by_repo: std::collections::BTreeMap::new(),
                fetch_report: Vec::new(),
            }),
        );
        assert_eq!(c.bzl_cold_misses, 10);
        assert_eq!(c.bzl_repeat_misses, 0);
        assert_eq!(c.bzl_peak_entry_count, 8);
        assert_eq!(c.bzl_unique_files_seen, 10);
        assert_eq!(c.bzl_max_entries, 50);
    }

    /// Phase 5.C.4 commit 96: hit recorded on top of
    /// the existing hits accumulator.
    #[test]
    fn bzl_cache_records_hit() {
        let mut b = AnalyzeCacheCountersBuilder::new();
        b.snapshot_bzl_start_full(5, 0, 0, 0, 0, 0, 0, 0);
        let c = b.finish_full(15, 0, 0, None);
        // 15 - 5 = 10 hits during this pass.
        assert_eq!(c.bzl_cache_hits, 10);
    }

    /// Phase 5.C.4 commit 96: repeat-miss delta surfaces
    /// the thrashing signal.
    #[test]
    fn bzl_cache_records_repeat_miss_after_eviction() {
        let mut b = AnalyzeCacheCountersBuilder::new();
        // Start: 50 cold + 0 repeat. End: 50 cold + 15
        // repeat (delta of +15 in repeat). This means
        // 15 paths were evicted then re-fetched during
        // this pass.
        b.snapshot_bzl_start_full(0, 0, 0, 50, 0, 0, 0, 0);
        let c = b.finish_full(
            0,
            15,
            0,
            Some(BzlEndFullSnapshot {
                cold_misses: 50,
                repeat_misses: 15,
                evictions_by_entry_limit: 15,
                evictions_by_byte_limit: 0,
                evictions_manual: 0,
                peak_entry_count: 50,
                unique_files_seen: 50,
                max_entries: 50,
                max_memory_mb: 512,
                peak_memory_mb: 100,
                top_slowest_cold_loads: Vec::new(),
                top_by_repeat_misses: Vec::new(),
                top_by_fanout: Vec::new(),
                top_by_reverse_dependents: Vec::new(),
                identity_collisions: Vec::new(),
                top_by_starlark_ops: Vec::new(),
                total_function_calls: 0,
                total_builtin_calls: 0,
                total_rule_invocations: 0,
                total_macro_invocations: 0,
                total_glob_calls: 0,
                estimate_symbols_size_calls: 0,
                estimate_symbols_size_total_ms: 0,
                repeat_reload_estimate_candidates: 0,
                repeat_reload_estimate_reuse_hits: 0,
                repeat_reload_estimate_reuse_misses: 0,
                estimate_verification_mismatches: 0,
                async_spawn_wait_ms: 0,
                async_spawn_wait_calls: 0,
                fs_metadata_wait_ms: 0,
                fs_metadata_wait_calls: 0,
                loader_lock_wait_ms: 0,
                loader_lock_wait_calls: 0,
                concurrent_eval_wait_ms: 0,
                concurrent_eval_wait_calls: 0,
                bazel_fetch_wait_ms: 0,
                bazel_fetch_wait_calls: 0,
                fetch_invoked_count: 0,
                fetch_skipped_materialized_count: 0,
                fetch_skipped_known_rule_count: 0,
                fetch_materialized_missing_dir_count: 0,
                fetch_materialized_missing_build_count: 0,
                fetch_duration_by_repo: std::collections::BTreeMap::new(),
                fetch_report: Vec::new(),
            }),
        );
        assert_eq!(c.bzl_cold_misses, 0, "no new cold misses this pass");
        assert_eq!(c.bzl_repeat_misses, 15, "15 paths re-fetched after eviction");
    }

    /// Phase 5.C.4 commit 96: entry-limit eviction reason.
    #[test]
    fn bzl_cache_eviction_reason_entry_limit() {
        let mut b = AnalyzeCacheCountersBuilder::new();
        b.snapshot_bzl_start_full(0, 0, 0, 0, 0, 0, 0, 0);
        let c = b.finish_full(
            0,
            0,
            20,
            Some(BzlEndFullSnapshot {
                evictions_by_entry_limit: 20,
                evictions_by_byte_limit: 0,
                evictions_manual: 0,
                ..Default::default()
            }),
        );
        assert_eq!(c.bzl_evictions_by_entry_limit, 20);
        assert_eq!(c.bzl_evictions_by_byte_limit, 0);
        assert_eq!(c.bzl_evictions_manual, 0);
    }

    /// Phase 5.C.4 commit 96: byte-limit eviction reason.
    #[test]
    fn bzl_cache_eviction_reason_byte_limit() {
        let mut b = AnalyzeCacheCountersBuilder::new();
        b.snapshot_bzl_start_full(0, 0, 0, 0, 0, 0, 0, 0);
        let c = b.finish_full(
            0,
            0,
            8,
            Some(BzlEndFullSnapshot {
                evictions_by_entry_limit: 0,
                evictions_by_byte_limit: 8,
                evictions_manual: 0,
                ..Default::default()
            }),
        );
        assert_eq!(c.bzl_evictions_by_byte_limit, 8);
        assert_eq!(c.bzl_evictions_by_entry_limit, 0);
    }

    /// Phase 5.C.4 commit 96: JSON shape pins
    /// schema_version=1 + all new commit-96 fields.
    #[test]
    fn bzl_cache_json_has_schema_version() {
        let mut b = AnalyzeCacheCountersBuilder::new();
        b.snapshot_bzl_start_full(0, 0, 0, 0, 0, 0, 0, 0);
        let c = b.finish_full(
            10,
            7,
            3,
            Some(BzlEndFullSnapshot {
                cold_misses: 6,
                repeat_misses: 1,
                evictions_by_entry_limit: 2,
                evictions_by_byte_limit: 1,
                evictions_manual: 0,
                peak_entry_count: 50,
                unique_files_seen: 7,
                max_entries: 50,
                max_memory_mb: 512,
                peak_memory_mb: 100,
                top_slowest_cold_loads: Vec::new(),
                top_by_repeat_misses: Vec::new(),
                top_by_fanout: Vec::new(),
                top_by_reverse_dependents: Vec::new(),
                identity_collisions: Vec::new(),
                top_by_starlark_ops: Vec::new(),
                total_function_calls: 0,
                total_builtin_calls: 0,
                total_rule_invocations: 0,
                total_macro_invocations: 0,
                total_glob_calls: 0,
                estimate_symbols_size_calls: 0,
                estimate_symbols_size_total_ms: 0,
                repeat_reload_estimate_candidates: 0,
                repeat_reload_estimate_reuse_hits: 0,
                repeat_reload_estimate_reuse_misses: 0,
                estimate_verification_mismatches: 0,
                async_spawn_wait_ms: 0,
                async_spawn_wait_calls: 0,
                fs_metadata_wait_ms: 0,
                fs_metadata_wait_calls: 0,
                loader_lock_wait_ms: 0,
                loader_lock_wait_calls: 0,
                concurrent_eval_wait_ms: 0,
                concurrent_eval_wait_calls: 0,
                bazel_fetch_wait_ms: 0,
                bazel_fetch_wait_calls: 0,
                fetch_invoked_count: 0,
                fetch_skipped_materialized_count: 0,
                fetch_skipped_known_rule_count: 0,
                fetch_materialized_missing_dir_count: 0,
                fetch_materialized_missing_build_count: 0,
                fetch_duration_by_repo: std::collections::BTreeMap::new(),
                fetch_report: Vec::new(),
            }),
        );
        let json = serde_json::to_string(&c).expect("serialize");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["schema_version"], 1);
        assert_eq!(parsed["bzl_cold_misses"], 6);
        assert_eq!(parsed["bzl_repeat_misses"], 1);
        assert_eq!(parsed["bzl_evictions_by_entry_limit"], 2);
        assert_eq!(parsed["bzl_evictions_by_byte_limit"], 1);
        assert_eq!(parsed["bzl_peak_entry_count"], 50);
        assert_eq!(parsed["bzl_unique_files_seen"], 7);
        assert_eq!(parsed["bzl_max_entries"], 50);
        assert_eq!(parsed["bzl_max_memory_mb"], 512);
    }

    /// Phase 5.C.4 commit 96: back-compat — the 3-arg
    /// `finish` still works (no end_full).
    #[test]
    fn finish_back_compat_3arg() {
        let mut b = AnalyzeCacheCountersBuilder::new();
        b.snapshot_bzl_start(0, 0, 0);
        let c = b.finish(5, 3, 1);
        assert_eq!(c.bzl_cache_hits, 5);
        assert_eq!(c.bzl_cache_misses, 3);
        assert_eq!(c.bzl_cache_evictions, 1);
        // commit-96 fields default to zero.
        assert_eq!(c.bzl_cold_misses, 0);
        assert_eq!(c.bzl_repeat_misses, 0);
    }

    /// Phase 5.C.4 commit 103: top-N cold-load entries
    /// flow through to the final counter struct.
    #[test]
    fn per_bzl_timing_records_unique_file() {
        let mut b = AnalyzeCacheCountersBuilder::new();
        b.snapshot_bzl_start_full(0, 0, 0, 0, 0, 0, 0, 0);
        let c = b.finish_full(
            0,
            0,
            0,
            Some(BzlEndFullSnapshot {
                top_slowest_cold_loads: vec![BzlColdLoadEntry {
                    path: "/path/to/tensorflow.bzl".to_string(),
                    cumulative_cold_load_ms: 8000,
                    cold_load_count: 1,
                    file_size_bytes: 120 * 1024,
                    exported_symbols_count: 240,
                    own_cold_load_ms: None,
                    child_load_ms: None,
                    subphase_read_ms: None,
                    subphase_parse_ms: None,
                    subphase_eval_ms: None,
                subphase_finalize_ms: None,
                subphase_resolve_lazy_loads_ms: None,
                subphase_put_symbols_cache_ms: None,
                attribution: None,
                }],
                top_by_repeat_misses: Vec::new(),
                ..Default::default()
            }),
        );
        assert_eq!(c.bzl_top_slowest_cold_loads.len(), 1);
        assert_eq!(c.bzl_top_slowest_cold_loads[0].path,
                   "/path/to/tensorflow.bzl");
        assert_eq!(c.bzl_top_slowest_cold_loads[0].cumulative_cold_load_ms, 8000);
        assert_eq!(c.bzl_top_slowest_cold_loads[0].cold_load_count, 1);
        assert_eq!(c.bzl_top_slowest_cold_loads[0].exported_symbols_count, 240);
    }

    /// Phase 5.C.4 commit 103: top-N is in the order the
    /// snapshot provided (orchestrator sorts via cache's
    /// top_n_by_cold_load_duration). Pin determinism by
    /// asserting the snapshot order is preserved.
    #[test]
    fn top_n_slowest_bzl_is_deterministic() {
        let entries = vec![
            BzlColdLoadEntry {
                path: "a.bzl".to_string(),
                cumulative_cold_load_ms: 100,
                cold_load_count: 1,
                file_size_bytes: 0,
                exported_symbols_count: 0,
                own_cold_load_ms: None,
                child_load_ms: None,
                subphase_read_ms: None,
                subphase_parse_ms: None,
                subphase_eval_ms: None,
                subphase_finalize_ms: None,
            subphase_resolve_lazy_loads_ms: None,
            subphase_put_symbols_cache_ms: None,
            attribution: None,
            },
            BzlColdLoadEntry {
                path: "b.bzl".to_string(),
                cumulative_cold_load_ms: 50,
                cold_load_count: 1,
                file_size_bytes: 0,
                exported_symbols_count: 0,
                own_cold_load_ms: None,
                child_load_ms: None,
                subphase_read_ms: None,
                subphase_parse_ms: None,
                subphase_eval_ms: None,
                subphase_finalize_ms: None,
            subphase_resolve_lazy_loads_ms: None,
            subphase_put_symbols_cache_ms: None,
            attribution: None,
            },
        ];
        let mut b = AnalyzeCacheCountersBuilder::new();
        b.snapshot_bzl_start_full(0, 0, 0, 0, 0, 0, 0, 0);
        let c = b.finish_full(
            0,
            0,
            0,
            Some(BzlEndFullSnapshot {
                top_slowest_cold_loads: entries.clone(),
                top_by_repeat_misses: Vec::new(),
                ..Default::default()
            }),
        );
        assert_eq!(c.bzl_top_slowest_cold_loads, entries);
    }

    /// Phase 5.C.4 commit 103: JSON shape pins the
    /// bzl_top_slowest_cold_loads field at schema v1.
    #[test]
    fn json_has_schema_version_with_top_n() {
        let mut b = AnalyzeCacheCountersBuilder::new();
        b.snapshot_bzl_start_full(0, 0, 0, 0, 0, 0, 0, 0);
        let c = b.finish_full(
            0,
            0,
            0,
            Some(BzlEndFullSnapshot {
                top_slowest_cold_loads: vec![BzlColdLoadEntry {
                    path: "x.bzl".to_string(),
                    cumulative_cold_load_ms: 1234,
                    cold_load_count: 2,
                    file_size_bytes: 4096,
                    exported_symbols_count: 17,
                    own_cold_load_ms: None,
                    child_load_ms: None,
                    subphase_read_ms: None,
                    subphase_parse_ms: None,
                    subphase_eval_ms: None,
                subphase_finalize_ms: None,
                subphase_resolve_lazy_loads_ms: None,
                subphase_put_symbols_cache_ms: None,
                attribution: None,
                }],
                top_by_repeat_misses: Vec::new(),
                ..Default::default()
            }),
        );
        let json = serde_json::to_string(&c).expect("serialize");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["schema_version"], 1);
        let arr = parsed["bzl_top_slowest_cold_loads"]
            .as_array()
            .expect("array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["path"], "x.bzl");
        assert_eq!(arr[0]["cumulative_cold_load_ms"], 1234);
        assert_eq!(arr[0]["cold_load_count"], 2);
        assert_eq!(arr[0]["file_size_bytes"], 4096);
        assert_eq!(arr[0]["exported_symbols_count"], 17);
        // Phase 5.C.4 commit 104: skip_serializing_if
        // means absent fields don't appear in JSON.
        assert!(arr[0].get("own_cold_load_ms").is_none());
        assert!(arr[0].get("child_load_ms").is_none());
        assert!(arr[0].get("subphase_read_ms").is_none());
    }

    /// Phase 5.C.4 commit 110: the wire-shape JSON
    /// surfaces the attribution string when present.
    /// Pro's test #4 from BBB.
    #[test]
    fn json_attribution_field_serializes() {
        let entries: Vec<BzlColdLoadEntry> = [
            "same_thread_complete",
            "cross_thread_children_observed",
            "recursion_suppressed",
            "unknown",
        ]
        .iter()
        .enumerate()
        .map(|(i, label)| BzlColdLoadEntry {
            path: format!("/t{}.bzl", i),
            cumulative_cold_load_ms: 100 + i as u64,
            cold_load_count: 1,
            file_size_bytes: 1024,
            exported_symbols_count: 5,
            own_cold_load_ms: None,
            child_load_ms: None,
            subphase_read_ms: None,
            subphase_parse_ms: None,
            subphase_eval_ms: None,
            subphase_finalize_ms: None,
            subphase_resolve_lazy_loads_ms: None,
            subphase_put_symbols_cache_ms: None,
            attribution: Some(label.to_string()),
        })
        .collect();

        let mut b = AnalyzeCacheCountersBuilder::new();
        b.snapshot_bzl_start_full(0, 0, 0, 0, 0, 0, 0, 0);
        let c = b.finish_full(
            0,
            0,
            0,
            Some(BzlEndFullSnapshot {
                top_slowest_cold_loads: entries,
                top_by_repeat_misses: Vec::new(),
                ..Default::default()
            }),
        );
        let json = serde_json::to_string(&c).expect("serialize");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("parse");
        let arr = parsed["bzl_top_slowest_cold_loads"]
            .as_array()
            .expect("array");
        assert_eq!(arr.len(), 4);
        assert_eq!(arr[0]["attribution"], "same_thread_complete");
        assert_eq!(arr[1]["attribution"], "cross_thread_children_observed");
        assert_eq!(arr[2]["attribution"], "recursion_suppressed");
        assert_eq!(arr[3]["attribution"], "unknown");

        // Entries with attribution: None are still
        // omitted from the JSON via skip_serializing_if.
        let omit_entry = BzlColdLoadEntry {
            path: "/no_attrib.bzl".to_string(),
            cumulative_cold_load_ms: 999,
            cold_load_count: 1,
            file_size_bytes: 0,
            exported_symbols_count: 0,
            own_cold_load_ms: None,
            child_load_ms: None,
            subphase_read_ms: None,
            subphase_parse_ms: None,
            subphase_eval_ms: None,
            subphase_finalize_ms: None,
            subphase_resolve_lazy_loads_ms: None,
            subphase_put_symbols_cache_ms: None,
            attribution: None,
        };
        let omit_json = serde_json::to_string(&omit_entry).expect("ser");
        assert!(
            !omit_json.contains("attribution"),
            "attribution: None must be omitted: {}",
            omit_json
        );
    }

    /// Phase 5.C.4 commit 89: env-var gate disabled by
    /// default.
    #[test]
    fn cache_counters_gate_disabled_by_default() {
        let prior = std::env::var("RUSH_ANALYZE_CACHE_COUNTERS").ok();
        std::env::remove_var("RUSH_ANALYZE_CACHE_COUNTERS");
        assert!(!cache_counters_enabled());
        std::env::set_var("RUSH_ANALYZE_CACHE_COUNTERS", "1");
        assert!(cache_counters_enabled());
        match prior {
            Some(v) => std::env::set_var("RUSH_ANALYZE_CACHE_COUNTERS", v),
            None => std::env::remove_var("RUSH_ANALYZE_CACHE_COUNTERS"),
        }
    }

    // Phase 5.C.4 commit 135 (Batch Q): per-repo fetch
    // report tests.

    /// Helper: build a synthetic
    /// AnalyzeCacheCounters fixture with a given set
    /// of report entries.
    fn fixture_with_report(
        report: Vec<BzlFetchReportEntry>,
    ) -> AnalyzeCacheCounters {
        AnalyzeCacheCountersBuilder::default().finish_full(
            0,
            0,
            0,
            Some(BzlEndFullSnapshot {
                cold_misses: 0,
                repeat_misses: 0,
                evictions_by_entry_limit: 0,
                evictions_by_byte_limit: 0,
                evictions_manual: 0,
                peak_entry_count: 0,
                unique_files_seen: 0,
                max_entries: 0,
                max_memory_mb: 0,
                peak_memory_mb: 0,
                top_slowest_cold_loads: Vec::new(),
                top_by_repeat_misses: Vec::new(),
                top_by_fanout: Vec::new(),
                top_by_reverse_dependents: Vec::new(),
                identity_collisions: Vec::new(),
                top_by_starlark_ops: Vec::new(),
                total_function_calls: 0,
                total_builtin_calls: 0,
                total_rule_invocations: 0,
                total_macro_invocations: 0,
                total_glob_calls: 0,
                estimate_symbols_size_calls: 0,
                estimate_symbols_size_total_ms: 0,
                repeat_reload_estimate_candidates: 0,
                repeat_reload_estimate_reuse_hits: 0,
                repeat_reload_estimate_reuse_misses: 0,
                estimate_verification_mismatches: 0,
                async_spawn_wait_ms: 0,
                async_spawn_wait_calls: 0,
                fs_metadata_wait_ms: 0,
                fs_metadata_wait_calls: 0,
                loader_lock_wait_ms: 0,
                loader_lock_wait_calls: 0,
                concurrent_eval_wait_ms: 0,
                concurrent_eval_wait_calls: 0,
                bazel_fetch_wait_ms: 0,
                bazel_fetch_wait_calls: 0,
                fetch_invoked_count: 0,
                fetch_skipped_materialized_count: 0,
                fetch_skipped_known_rule_count: 0,
                fetch_materialized_missing_dir_count: 0,
                fetch_materialized_missing_build_count: 0,
                fetch_duration_by_repo:
                    std::collections::BTreeMap::new(),
                fetch_report: report,
            }),
        )
    }

    #[test]
    fn fetch_report_groups_by_repo() {
        // Each repo appears at most once in the report
        // (latest decision wins per repo).
        let report = vec![
            BzlFetchReportEntry {
                repo_name: "alpha".into(),
                decision: "invoked".into(),
                duration_ms: 11000,
                materialized_path: None,
                had_build: None,
                ..Default::default()
            },
            BzlFetchReportEntry {
                repo_name: "beta".into(),
                decision: "skipped_materialized".into(),
                duration_ms: 0,
                materialized_path: Some("/ext/beta".into()),
                had_build: Some(true),
                ..Default::default()
            },
        ];
        let c = fixture_with_report(report);
        let names: Vec<&str> = c
            .bzl_fetch_report
            .iter()
            .map(|e| e.repo_name.as_str())
            .collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        // Same repo must not appear twice.
        let mut seen = std::collections::HashSet::new();
        for e in &c.bzl_fetch_report {
            assert!(
                seen.insert(e.repo_name.clone()),
                "duplicate repo in report: {}",
                e.repo_name
            );
        }
    }

    #[test]
    fn fetch_report_json_stable() {
        // JSON output must be deterministic across two
        // serializations of equal-content reports —
        // sorted by repo_name (caller's responsibility,
        // checked here).
        let report = vec![
            BzlFetchReportEntry {
                repo_name: "alpha".into(),
                decision: "invoked".into(),
                duration_ms: 11000,
                materialized_path: None,
                had_build: None,
                ..Default::default()
            },
            BzlFetchReportEntry {
                repo_name: "beta".into(),
                decision: "skipped_known_rule".into(),
                duration_ms: 0,
                materialized_path: Some("/ext/beta".into()),
                had_build: Some(true),
                ..Default::default()
            },
        ];
        let c1 = fixture_with_report(report.clone());
        let c2 = fixture_with_report(report);
        let j1 = serde_json::to_string(&c1.bzl_fetch_report)
            .expect("ser1");
        let j2 = serde_json::to_string(&c2.bzl_fetch_report)
            .expect("ser2");
        assert_eq!(j1, j2, "JSON must be byte-identical");
        // Optional fields with None should be omitted
        // (skip_serializing_if).
        assert!(
            !j1.contains("\"materialized_path\":null"),
            "None materialized_path must be omitted: {}",
            j1
        );
        assert!(
            !j1.contains("\"had_build\":null"),
            "None had_build must be omitted: {}",
            j1
        );
    }

    // =====================================================
    // Phase 5.C.4 commit 150 (Batch S): manifest status
    // surfaced in fetch report
    // =====================================================

    #[test]
    fn fetch_report_includes_manifest_status() {
        let report = vec![
            BzlFetchReportEntry {
                repo_name: "alpha".into(),
                decision: "skipped_materialized_manifest_valid".into(),
                duration_ms: 0,
                materialized_path: Some("/ext/alpha".into()),
                had_build: Some(true),
                manifest_status: Some("valid".into()),
                manifest_path: Some(
                    "/ws/.cache/rush/external-manifests/alpha.json".into(),
                ),
                manifest_reason: None,
                skipped_with_manifest: true,
                skipped_by_trust_mode: false,
            },
            BzlFetchReportEntry {
                repo_name: "beta".into(),
                decision: "skipped_materialized_trust_mode".into(),
                duration_ms: 0,
                materialized_path: Some("/ext/beta".into()),
                had_build: Some(true),
                manifest_status: Some("missing".into()),
                manifest_path: Some(
                    "/ws/.cache/rush/external-manifests/beta.json".into(),
                ),
                manifest_reason: None,
                skipped_with_manifest: false,
                skipped_by_trust_mode: true,
            },
        ];
        let c = fixture_with_report(report);
        // Manifest status fields survive into the
        // emitted JSON.
        let json = serde_json::to_string(&c.bzl_fetch_report)
            .expect("serialize");
        assert!(
            json.contains("manifest_status"),
            "JSON must include manifest_status: {}",
            json
        );
        assert!(
            json.contains("\"valid\""),
            "JSON must include the valid status: {}",
            json
        );
        assert!(
            json.contains("\"missing\""),
            "JSON must include the missing status: {}",
            json
        );
        // skipped_with_manifest is true for alpha,
        // omitted (false) for beta.
        assert!(
            json.contains("\"skipped_with_manifest\":true"),
            "alpha's true must serialize: {}",
            json
        );
        assert!(
            json.contains("\"skipped_by_trust_mode\":true"),
            "beta's true must serialize: {}",
            json
        );
    }

    #[test]
    fn skipped_trust_mode_reported_distinctly() {
        let report = vec![BzlFetchReportEntry {
            repo_name: "gamma".into(),
            decision: "skipped_materialized_trust_mode".into(),
            duration_ms: 0,
            materialized_path: Some("/ext/gamma".into()),
            had_build: Some(true),
            manifest_status: Some("incomplete".into()),
            manifest_path: Some("/ws/.cache/rush/external-manifests/gamma.json".into()),
            manifest_reason: None,
            skipped_with_manifest: false,
            skipped_by_trust_mode: true,
        }];
        let c = fixture_with_report(report);
        let entry = &c.bzl_fetch_report[0];
        assert_eq!(
            entry.decision,
            "skipped_materialized_trust_mode"
        );
        assert!(
            entry.skipped_by_trust_mode,
            "trust-mode flag must be true"
        );
        assert!(
            !entry.skipped_with_manifest,
            "manifest-valid flag must be false"
        );
        assert_eq!(
            entry.manifest_status.as_deref(),
            Some("incomplete")
        );
    }

    #[test]
    fn skipped_manifest_valid_reported_distinctly() {
        let report = vec![BzlFetchReportEntry {
            repo_name: "delta".into(),
            decision: "skipped_materialized_manifest_valid".into(),
            duration_ms: 0,
            materialized_path: Some("/ext/delta".into()),
            had_build: Some(true),
            manifest_status: Some("valid".into()),
            manifest_path: Some("/ws/.cache/rush/external-manifests/delta.json".into()),
            manifest_reason: None,
            skipped_with_manifest: true,
            skipped_by_trust_mode: false,
        }];
        let c = fixture_with_report(report);
        let entry = &c.bzl_fetch_report[0];
        assert_eq!(
            entry.decision,
            "skipped_materialized_manifest_valid"
        );
        assert!(
            entry.skipped_with_manifest,
            "manifest-valid flag must be true"
        );
        assert!(
            !entry.skipped_by_trust_mode,
            "trust-mode flag must be false"
        );
        assert_eq!(entry.manifest_status.as_deref(), Some("valid"));
    }

    #[test]
    fn top_fetch_duration_deterministic() {
        // The format_human top-N section sorts by
        // duration descending, then by repo_name
        // ascending as a tie-break. Verify both
        // orderings.
        let report = vec![
            BzlFetchReportEntry {
                repo_name: "z_repo".into(),
                decision: "invoked".into(),
                duration_ms: 5000,
                materialized_path: None,
                had_build: None,
                ..Default::default()
            },
            BzlFetchReportEntry {
                repo_name: "a_repo".into(),
                decision: "invoked".into(),
                duration_ms: 5000,
                materialized_path: None,
                had_build: None,
                ..Default::default()
            },
            BzlFetchReportEntry {
                repo_name: "big".into(),
                decision: "invoked".into(),
                duration_ms: 20000,
                materialized_path: None,
                had_build: None,
                ..Default::default()
            },
            BzlFetchReportEntry {
                repo_name: "skipped_no_dur".into(),
                decision: "skipped_materialized".into(),
                duration_ms: 0,
                materialized_path: Some("/ext/x".into()),
                had_build: Some(true),
                ..Default::default()
            },
        ];
        let c = fixture_with_report(report);
        let human = c.format_human();
        // Find the top_fetch_duration block + verify ordering.
        let lines: Vec<&String> = human
            .iter()
            .skip_while(|l| !l.contains("top_fetch_duration"))
            .collect();
        assert!(
            !lines.is_empty(),
            "top_fetch_duration block missing"
        );
        // Expected order: big (20000) → a_repo (5000) →
        // z_repo (5000). skipped_no_dur is filtered out.
        let body: Vec<&str> = lines
            .iter()
            .skip(1)
            .take(3)
            .map(|s| s.as_str())
            .collect();
        assert_eq!(body.len(), 3);
        assert!(body[0].contains("big"), "first must be big: {}", body[0]);
        assert!(
            body[1].contains("a_repo"),
            "second must be a_repo (alpha-sorted tie-break): {}",
            body[1]
        );
        assert!(
            body[2].contains("z_repo"),
            "third must be z_repo: {}",
            body[2]
        );
        // skipped_no_dur must NOT appear (filtered out).
        for line in &lines {
            assert!(
                !line.contains("skipped_no_dur"),
                "zero-duration entries must be filtered: {}",
                line
            );
        }
    }
}
