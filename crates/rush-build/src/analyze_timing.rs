// Rush Build - Analyze-pass phase timing (phase 5.C.4 commit 82)
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Phase 5.C.4 commit 82: lightweight analyze-pass timing
//! instrumentation.
//!
//! Pro's commit-82 directive: *"Measure why analysis is slow
//! before optimizing it. Keep it cheap: wall-clock durations,
//! counts, no heap profiler, no per-target spam by default."*
//!
//! ## What this is
//!
//! - A bag of per-phase wall-clock durations + counts.
//! - One instance per analyze-pass invocation; lives in the
//!   orchestrator's local frame.
//! - Records via [`AnalyzeTimings::record`] at phase
//!   boundaries; renders via human/JSON formatters at the end.
//!
//! ## What this is NOT
//!
//! - Not a heap profiler. Memory / RSS reporting is a
//!   separate concern (cf. CLAUDE.md profiling section).
//! - Not a per-target tracer. The struct aggregates by
//!   phase, not by target — TF-scale workloads produce
//!   thousands of targets and per-target output would be
//!   operationally useless.
//! - Not a profiler-feature replacement.
//!   `cargo build --release --features profile` still gives
//!   the per-scope `profile_scope!` data; this struct is the
//!   ALWAYS-AVAILABLE coarse-grained summary that lands in
//!   every analyze run.

use std::time::{Duration, Instant};

/// Phase 5.C.4 commit 82: one logical analyze-pass phase.
///
/// Each variant maps to a stage in the orchestrator's
/// analyze-only / preflight pipeline. Ordering reflects
/// the typical execution sequence.
///
/// JSON shape (commit 54b style):
///   "workspace_load"
///   "target_expansion"
///   "build_file_parse"
///   "dependency_resolution"
///   "action_dag_construction"
///   "scheduler_preflight"
///   "legacy_breakdown"
///   "action_key_explain"
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalyzePhase {
    /// WORKSPACE file loading + external repo bootstrap.
    WorkspaceLoad,
    /// Target pattern expansion (`//...`, `:all`, etc.).
    TargetExpansion,
    /// BUILD file parsing + RuleInstance creation.
    BuildFileParse,
    /// Dependency graph resolution (cross-target deps).
    DependencyResolution,
    /// ActionDag::build + scheduler plan construction.
    ActionDagConstruction,
    /// SchedulerPreflightReport construction.
    SchedulerPreflight,
    /// Legacy breakdown / census construction.
    LegacyBreakdown,
    /// ActionKeySpec / CanonicalActionKeyInput dumps.
    ActionKeyExplain,
}

impl AnalyzePhase {
    /// Short human label (matches the JSON encoding).
    pub fn label(&self) -> &'static str {
        match self {
            Self::WorkspaceLoad => "workspace_load",
            Self::TargetExpansion => "target_expansion",
            Self::BuildFileParse => "build_file_parse",
            Self::DependencyResolution => "dependency_resolution",
            Self::ActionDagConstruction => "action_dag_construction",
            Self::SchedulerPreflight => "scheduler_preflight",
            Self::LegacyBreakdown => "legacy_breakdown",
            Self::ActionKeyExplain => "action_key_explain",
        }
    }

    /// Ordered list of all phases for deterministic
    /// human / JSON output.
    pub fn all() -> &'static [AnalyzePhase] {
        &[
            Self::WorkspaceLoad,
            Self::TargetExpansion,
            Self::BuildFileParse,
            Self::DependencyResolution,
            Self::ActionDagConstruction,
            Self::SchedulerPreflight,
            Self::LegacyBreakdown,
            Self::ActionKeyExplain,
        ]
    }
}

/// Phase 5.C.4 commit 82: per-phase timing entry.
///
/// `count` is a phase-specific counter — e.g. number of
/// BUILD files parsed during `BuildFileParse`. `None`
/// when the phase doesn't track a count.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PhaseEntry {
    /// Wall-clock duration spent in this phase.
    #[serde(serialize_with = "ser_duration_ms")]
    pub duration: Duration,
    /// Phase-specific counter (BUILD files parsed,
    /// targets resolved, etc.). `None` when not tracked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
}

fn ser_duration_ms<S: serde::Serializer>(
    d: &Duration,
    s: S,
) -> Result<S::Ok, S::Error> {
    // Serialize as integer milliseconds. CI consumers
    // doing thresholded gating get a stable, parseable
    // integer; humans reading the JSON see units inline.
    s.serialize_u64(d.as_millis() as u64)
}

/// Phase 5.C.4 commit 82: analyze-pass phase timings.
///
/// One instance per analyze-pass invocation. The
/// orchestrator builds it locally, calls
/// [`record`](Self::record) at phase boundaries, then
/// renders via [`format_human`](Self::format_human) and
/// optionally [`to_json_object`](Self::to_json_object).
///
/// Cost when enabled: 2 `Instant::now()` calls per phase
/// boundary. Cost when disabled: zero — the orchestrator
/// short-circuits the entire instrumentation path when
/// `RUSH_ANALYZE_TIMING` is not set.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct AnalyzeTimings {
    /// Per-phase entries. Phases not visited stay absent
    /// from the map; rendering iterates `AnalyzePhase::all()`
    /// in deterministic order, emitting only present
    /// phases.
    pub phases: std::collections::HashMap<AnalyzePhase, PhaseEntry>,
    /// Total elapsed time from instantiation to last
    /// `record`/`finalize` call.
    #[serde(serialize_with = "ser_duration_ms")]
    pub total: Duration,
    /// Schema version. Bumped on breaking JSON shape
    /// changes; CI consumers gate on equality. Today's
    /// shape: `{phases: {phase: {duration: ms, count: N?}},
    /// total: ms, schema_version: 1}`.
    pub schema_version: u32,
}

impl AnalyzeTimings {
    /// Build an empty timings record. Today the
    /// `schema_version` is 1.
    pub fn new() -> Self {
        Self {
            phases: std::collections::HashMap::new(),
            total: Duration::default(),
            schema_version: ANALYZE_TIMING_SCHEMA_VERSION,
        }
    }

    /// Record a phase's elapsed time + optional count.
    /// Idempotent — calling for the same phase twice
    /// replaces the prior entry.
    pub fn record(
        &mut self,
        phase: AnalyzePhase,
        duration: Duration,
        count: Option<u64>,
    ) {
        self.phases.insert(phase, PhaseEntry { duration, count });
        // Track total as sum of recorded phases. Future
        // refinement: subtract overlapping phases if any
        // analyze paths run phases concurrently.
        self.total = self.phases.values().map(|e| e.duration).sum();
    }

    /// Render as human-readable stderr lines. Each phase
    /// appears in `AnalyzePhase::all()` order; absent
    /// phases are skipped. Format mirrors the JSON
    /// shape's millisecond units.
    pub fn format_human(&self) -> Vec<String> {
        let mut out = Vec::new();
        out.push(format!(
            "analyze-pass timing (schema_version={}, total {} ms):",
            self.schema_version,
            self.total.as_millis(),
        ));
        for phase in AnalyzePhase::all() {
            if let Some(entry) = self.phases.get(phase) {
                let mut line = format!(
                    "  {}: {} ms",
                    phase.label(),
                    entry.duration.as_millis()
                );
                if let Some(c) = entry.count {
                    line.push_str(&format!(" ({} item(s))", c));
                }
                out.push(line);
            }
        }
        out
    }

    /// True iff at least one phase was recorded. The
    /// orchestrator uses this to skip empty timing dumps.
    pub fn is_empty(&self) -> bool {
        self.phases.is_empty()
    }
}

/// Phase 5.C.4 commit 82: schema version for the timing
/// JSON wire format.
///
/// Today's shape:
///   {
///     "schema_version": 1,
///     "total": <ms>,
///     "phases": {
///       "workspace_load": {"duration": <ms>, "count": <N?>},
///       ...
///     }
///   }
///
/// Bumped on breaking JSON shape changes. CI consumers
/// gate on equality.
pub const ANALYZE_TIMING_SCHEMA_VERSION: u32 = 1;

/// Phase 5.C.4 commit 82: timing-instrumentation gate.
///
/// Returns `true` when the operator has explicitly
/// requested timing output via the
/// `RUSH_ANALYZE_TIMING` env var. The orchestrator
/// short-circuits the entire `Instant::now()` measurement
/// path when this returns `false`, so disabled-state cost
/// is zero.
///
/// Pro's commit-82 directive: "Disabled by default or
/// only in requested modes." Today's gate is env-var
/// only; a CLI flag (`--analyze-timing`) is a small
/// follow-up.
pub fn timing_enabled() -> bool {
    std::env::var("RUSH_ANALYZE_TIMING").as_deref() == Ok("1")
}

/// Phase 5.C.4 commit 82: timing-as-JSON gate.
///
/// Returns `true` when the operator has requested JSON
/// timing output via `RUSH_ANALYZE_TIMING_JSON=1`.
/// Implies `timing_enabled()`.
pub fn timing_json_enabled() -> bool {
    timing_enabled()
        && std::env::var("RUSH_ANALYZE_TIMING_JSON").as_deref() == Ok("1")
}

/// Phase 5.C.4 commit 82: RAII timer scope.
///
/// Records the elapsed time on Drop into the parent
/// `AnalyzeTimings` for the named phase. Designed for
/// the most common case — a single phase boundary
/// straddling a code block:
///
/// ```ignore
/// let timer = PhaseTimer::start(&mut timings, AnalyzePhase::BuildFileParse);
/// parse_build_files()?;
/// drop(timer); // records elapsed
/// ```
///
/// Or as a scope guard:
///
/// ```ignore
/// {
///     let _t = PhaseTimer::start(&mut timings, AnalyzePhase::BuildFileParse);
///     parse_build_files()?;
/// } // dropped here
/// ```
///
/// Today the timer doesn't track a count — callers that
/// want per-phase counts call [`AnalyzeTimings::record`]
/// directly with the count after the scope exits.
pub struct PhaseTimer<'a> {
    timings: &'a mut AnalyzeTimings,
    phase: AnalyzePhase,
    start: Instant,
    count: Option<u64>,
}

impl<'a> PhaseTimer<'a> {
    /// Start timing a phase. The elapsed duration is
    /// recorded on Drop.
    pub fn start(timings: &'a mut AnalyzeTimings, phase: AnalyzePhase) -> Self {
        Self {
            timings,
            phase,
            start: Instant::now(),
            count: None,
        }
    }

    /// Attach a count to the phase entry. Persists when
    /// the timer is dropped.
    pub fn set_count(&mut self, count: u64) {
        self.count = Some(count);
    }
}

impl<'a> Drop for PhaseTimer<'a> {
    fn drop(&mut self) {
        let elapsed = self.start.elapsed();
        self.timings.record(self.phase, elapsed, self.count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn timings_default_is_empty() {
        let t = AnalyzeTimings::new();
        assert!(t.is_empty());
        assert_eq!(t.schema_version, 1);
        assert_eq!(t.total, Duration::default());
    }

    #[test]
    fn record_appends_phase_and_updates_total() {
        let mut t = AnalyzeTimings::new();
        t.record(AnalyzePhase::WorkspaceLoad, Duration::from_millis(50), None);
        t.record(
            AnalyzePhase::BuildFileParse,
            Duration::from_millis(120),
            Some(7),
        );
        assert_eq!(t.phases.len(), 2);
        assert_eq!(t.total, Duration::from_millis(170));
        let parse_entry = &t.phases[&AnalyzePhase::BuildFileParse];
        assert_eq!(parse_entry.duration, Duration::from_millis(120));
        assert_eq!(parse_entry.count, Some(7));
    }

    #[test]
    fn record_idempotent_replaces_prior_entry() {
        let mut t = AnalyzeTimings::new();
        t.record(AnalyzePhase::WorkspaceLoad, Duration::from_millis(10), None);
        t.record(AnalyzePhase::WorkspaceLoad, Duration::from_millis(99), Some(3));
        assert_eq!(t.phases.len(), 1);
        let entry = &t.phases[&AnalyzePhase::WorkspaceLoad];
        assert_eq!(entry.duration, Duration::from_millis(99));
        assert_eq!(entry.count, Some(3));
        assert_eq!(t.total, Duration::from_millis(99));
    }

    #[test]
    fn format_human_emits_header_and_ordered_phases() {
        let mut t = AnalyzeTimings::new();
        // Record OUT OF ORDER to verify the formatter
        // emits in AnalyzePhase::all() order.
        t.record(
            AnalyzePhase::ActionDagConstruction,
            Duration::from_millis(80),
            None,
        );
        t.record(
            AnalyzePhase::WorkspaceLoad,
            Duration::from_millis(20),
            None,
        );
        let lines = t.format_human();
        assert!(lines[0].contains("schema_version=1"));
        assert!(lines[0].contains("total 100 ms"));
        // workspace_load comes before action_dag_construction
        // in AnalyzePhase::all(), so it must appear first
        // in the rendered output regardless of recording
        // order.
        let ws_idx = lines.iter().position(|l| l.contains("workspace_load: 20"));
        let dag_idx = lines
            .iter()
            .position(|l| l.contains("action_dag_construction: 80"));
        assert!(ws_idx.is_some() && dag_idx.is_some());
        assert!(ws_idx.unwrap() < dag_idx.unwrap());
    }

    #[test]
    fn format_human_includes_count_when_set() {
        let mut t = AnalyzeTimings::new();
        t.record(
            AnalyzePhase::BuildFileParse,
            Duration::from_millis(50),
            Some(42),
        );
        let lines = t.format_human();
        assert!(
            lines.iter().any(|l| l.contains("42 item(s)")),
            "expected count in output; got {:?}",
            lines
        );
    }

    #[test]
    fn json_serialization_matches_schema_v1_shape() {
        let mut t = AnalyzeTimings::new();
        t.record(
            AnalyzePhase::WorkspaceLoad,
            Duration::from_millis(25),
            None,
        );
        t.record(
            AnalyzePhase::BuildFileParse,
            Duration::from_millis(150),
            Some(11),
        );
        let json = serde_json::to_string(&t).expect("serialize");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["schema_version"], 1);
        assert_eq!(parsed["total"], 175);
        assert_eq!(parsed["phases"]["workspace_load"]["duration"], 25);
        assert!(
            parsed["phases"]["workspace_load"].get("count").is_none(),
            "count must be absent when not set"
        );
        assert_eq!(parsed["phases"]["build_file_parse"]["duration"], 150);
        assert_eq!(parsed["phases"]["build_file_parse"]["count"], 11);
    }

    #[test]
    fn phase_timer_drop_records_elapsed() {
        let mut t = AnalyzeTimings::new();
        {
            let _timer = PhaseTimer::start(&mut t, AnalyzePhase::WorkspaceLoad);
            sleep(Duration::from_millis(5));
        }
        let entry = &t.phases[&AnalyzePhase::WorkspaceLoad];
        assert!(
            entry.duration >= Duration::from_millis(4),
            "elapsed must be at least the sleep duration; got {:?}",
            entry.duration
        );
        // No count attached.
        assert_eq!(entry.count, None);
    }

    #[test]
    fn phase_timer_set_count_persists_on_drop() {
        let mut t = AnalyzeTimings::new();
        {
            let mut timer = PhaseTimer::start(&mut t, AnalyzePhase::BuildFileParse);
            timer.set_count(17);
        }
        let entry = &t.phases[&AnalyzePhase::BuildFileParse];
        assert_eq!(entry.count, Some(17));
    }

    /// Pro's commit-82 directive: "Disabled by default or
    /// only in requested modes." Pin that the gate is
    /// off when the env var is absent.
    #[test]
    fn timing_disabled_by_default() {
        // Save/restore RUSH_ANALYZE_TIMING so other tests
        // aren't affected.
        let prior = std::env::var("RUSH_ANALYZE_TIMING").ok();
        std::env::remove_var("RUSH_ANALYZE_TIMING");
        assert!(!timing_enabled());
        std::env::set_var("RUSH_ANALYZE_TIMING", "1");
        assert!(timing_enabled());
        match prior {
            Some(v) => std::env::set_var("RUSH_ANALYZE_TIMING", v),
            None => std::env::remove_var("RUSH_ANALYZE_TIMING"),
        }
    }
}
