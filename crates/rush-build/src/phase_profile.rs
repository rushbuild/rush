// Per-request phase profiling for the warm-no-op decision path.
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT
//
// `RUSH_PROFILE_PHASES=<path.json>` turns on a structured, per-build
// breakdown of where a (no-op) build spends its server-side wall time. This
// is NOT the cumulative `rush-profiling` counter (that aggregates across all
// scopes for the lifetime of a process and is compile-time gated); this is a
// single-build record meant to answer "for THIS no-op, which phase dominated
// the ~120ms?" without recompiling.
//
// The daemon serializes builds one-at-a-time behind `command_lock`
// (command_service.rs), so a process-global recorder observes exactly one
// build's phases between `begin()` and `finish()`. We follow the existing
// `RUSH_REUSE_TIMING` model (build_orchestrator_ext.rs) — real
// `Instant`-measured durations recorded at the very call sites the warm/cold
// reuse decision already times, plus the manifest dep count and ledger path
// count so the JSON ties latency to the universe each phase walked.
//
// Phases recorded on the warm reuse fast path, in order:
//   request_decode_to_orchestrator  client semantics -> orchestrator entry
//   analysis_key_lookup             AnalysisKey::new + snapshot_cache.get
//   manifest_validate               the single manifest.validate() + token mint
//                                   (Phase 1 killed the old second validate; the
//                                   "manifest_validate_again" phase is no longer
//                                   emitted on the reuse fast path)
//   ledger_validate                 FreshnessLedger::verify() (the path stat-walk)
//   execute_analyzed                PASS-B wavefront (0 if skipped on clean ledger)
//   response                        early-return bookkeeping
// Counts: manifest_deps, ledger_paths, ledger_actions, path ("warm"|"cold").

use std::io::Write;
use std::sync::Mutex;
use std::time::Duration;

use once_cell::sync::Lazy;
use serde::Serialize;

/// One named phase measurement.
#[derive(Debug, Clone, Serialize)]
pub struct PhaseSample {
    pub name: &'static str,
    pub ms: f64,
}

/// The full per-build record written to the `RUSH_PROFILE_PHASES` path.
#[derive(Debug, Clone, Serialize)]
pub struct PhaseRecord {
    /// Which reuse path produced the no-op: "warm", "cold", or "miss".
    pub path: &'static str,
    /// Ordered phase timings (ms).
    pub phases: Vec<PhaseSample>,
    /// Sum of all recorded phase ms (server-side decision-path total).
    pub total_ms: f64,
    /// Number of dependencies in the validated manifest.
    pub manifest_deps: usize,
    /// Number of unique paths stat-walked by the freshness ledger.
    pub ledger_paths: usize,
    /// Number of declared actions backing those paths.
    pub ledger_actions: usize,
    /// Whether PASS-B wavefront was skipped (clean ledger).
    pub wavefront_skipped: bool,
    /// Target patterns requested (for context).
    pub targets: Vec<String>,
}

struct Recorder {
    enabled: bool,
    out_path: Option<String>,
    phases: Vec<PhaseSample>,
    manifest_deps: usize,
    ledger_paths: usize,
    ledger_actions: usize,
    wavefront_skipped: bool,
    targets: Vec<String>,
}

impl Recorder {
    fn fresh() -> Self {
        let out_path = std::env::var("RUSH_PROFILE_PHASES").ok().filter(|s| !s.is_empty());
        Recorder {
            enabled: out_path.is_some(),
            out_path,
            phases: Vec::new(),
            manifest_deps: 0,
            ledger_paths: 0,
            ledger_actions: 0,
            wavefront_skipped: false,
            targets: Vec::new(),
        }
    }
}

static RECORDER: Lazy<Mutex<Recorder>> = Lazy::new(|| Mutex::new(Recorder::fresh()));

/// True when `RUSH_PROFILE_PHASES` is set to a non-empty path.
pub fn enabled() -> bool {
    RECORDER.lock().map(|r| r.enabled).unwrap_or(false)
}

/// Start a fresh per-build record. Re-reads the env var so the daemon picks
/// up a per-client setting on each command. Cheap no-op when disabled.
pub fn begin(targets: &[String]) {
    let mut r = RECORDER.lock().unwrap_or_else(|p| p.into_inner());
    *r = Recorder::fresh();
    r.targets = targets.to_vec();
}

/// Record a phase duration. No-op when disabled (cheap lock + flag check).
pub fn record(name: &'static str, dur: Duration) {
    let mut r = RECORDER.lock().unwrap_or_else(|p| p.into_inner());
    if !r.enabled {
        return;
    }
    r.phases.push(PhaseSample {
        name,
        ms: dur.as_secs_f64() * 1e3,
    });
}

/// Record the manifest dependency count (the universe manifest_validate walks).
pub fn set_manifest_deps(n: usize) {
    let mut r = RECORDER.lock().unwrap_or_else(|p| p.into_inner());
    if r.enabled {
        r.manifest_deps = n;
    }
}

/// Record the ledger path/action counts (the universe ledger_validate walks).
pub fn set_ledger_counts(paths: usize, actions: usize) {
    let mut r = RECORDER.lock().unwrap_or_else(|p| p.into_inner());
    if r.enabled {
        r.ledger_paths = paths;
        r.ledger_actions = actions;
    }
}

/// Record whether the PASS-B wavefront was skipped (clean ledger).
pub fn set_wavefront_skipped(skipped: bool) {
    let mut r = RECORDER.lock().unwrap_or_else(|p| p.into_inner());
    if r.enabled {
        r.wavefront_skipped = skipped;
    }
}

/// Finish the record and write it to the `RUSH_PROFILE_PHASES` path as JSON.
/// `path` is the reuse path that produced the result ("warm"/"cold"/"miss").
/// No-op when disabled.
pub fn finish(path: &'static str) {
    let (out_path, record) = {
        let mut r = RECORDER.lock().unwrap_or_else(|p| p.into_inner());
        if !r.enabled {
            return;
        }
        let total_ms = r.phases.iter().map(|p| p.ms).sum();
        let record = PhaseRecord {
            path,
            phases: std::mem::take(&mut r.phases),
            total_ms,
            manifest_deps: r.manifest_deps,
            ledger_paths: r.ledger_paths,
            ledger_actions: r.ledger_actions,
            wavefront_skipped: r.wavefront_skipped,
            targets: std::mem::take(&mut r.targets),
        };
        // Disarm so a fall-through full build doesn't double-write.
        r.enabled = false;
        (r.out_path.clone(), record)
    };

    let Some(out_path) = out_path else { return };
    match serde_json::to_string_pretty(&record) {
        Ok(json) => {
            if let Err(e) = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&out_path)
                .and_then(|mut f| f.write_all(json.as_bytes()))
            {
                eprintln!("⚠️  RUSH_PROFILE_PHASES: failed to write {out_path}: {e}");
            } else {
                eprintln!(
                    "📊 phase profile ({path}, total {:.3}ms over {} phases) -> {out_path}",
                    record.total_ms,
                    record.phases.len(),
                );
            }
        }
        Err(e) => eprintln!("⚠️  RUSH_PROFILE_PHASES: serialize failed: {e}"),
    }
}
