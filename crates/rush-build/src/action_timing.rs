// Rush Build - opt-in per-action timing JSONL.
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Per-action wall timing for incremental profiling.
//!
//! `RUSH_ACTION_TIMING=<path.jsonl>` appends one JSON record per action that
//! the legacy execution path runs or serves from cache. The gate is off by
//! default, so normal builds pay only an environment check at instrumentation
//! sites.

use once_cell::sync::Lazy;
use serde::Serialize;
use std::io::Write;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static STATE: Lazy<Mutex<TimingState>> = Lazy::new(|| Mutex::new(TimingState::from_process()));
static WRITE_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

#[derive(Debug)]
struct TimingState {
    out_path: Option<String>,
}

impl TimingState {
    fn from_process() -> Self {
        Self {
            out_path: std::env::var("RUSH_ACTION_TIMING")
                .ok()
                .filter(|p| !p.is_empty()),
        }
    }
}

#[derive(Debug, Serialize)]
struct ActionTimingRecord<'a> {
    schema_version: u32,
    event: &'static str,
    owner: Option<&'a str>,
    mnemonic: &'a str,
    action_id: String,
    primary_output: Option<String>,
    executable: String,
    argv_len: usize,
    argv_bytes: usize,
    started_unix_ns: u64,
    finished_unix_ns: u64,
    execution_started_unix_ns: Option<u64>,
    execution_finished_unix_ns: Option<u64>,
    wall_ms: f64,
    execution_ms: Option<f64>,
    non_execution_ms: Option<f64>,
    /// Time blocked waiting for a ResourceManager permit — queueing, not
    /// work. Split out because `non_execution_ms` fused it with real
    /// manager overhead, and on a TensorFlow-scale run that fused bucket
    /// was 31% of aggregate action wall: too big to leave unattributed
    /// when asking where an incremental build's time goes.
    resource_wait_ms: Option<f64>,
    /// Post-execution commit: fingerprint the outputs and tee them into the
    /// CAS. Previously invisible because the record was emitted first.
    commit_ms: Option<f64>,
    /// Cache lookup and the hit gate (value comparison, CAS restoration
    /// attempt, and virtual-hit decision).
    cache_lookup_ms: Option<f64>,
    /// Whatever is left: wall minus execution, resource wait, commit and
    /// lookup. This is the action manager's own cost.
    manager_overhead_ms: Option<f64>,
    exit_code: Option<i32>,
    cached: bool,
}

/// Per-action phase durations. Every field is optional because not every
/// path has every phase (a cache hit never waits for a permit and never
/// commits).
#[derive(Debug, Default, Clone, Copy)]
pub struct PhaseBreakdown {
    pub resource_wait: Option<Duration>,
    pub commit: Option<Duration>,
    pub cache_lookup: Option<Duration>,
}

/// Start a fresh per-command action timing sink.
///
/// The daemon receives the client's environment in `CommandSemantics`; it must
/// not read the long-lived daemon process environment here. Standalone/test
/// paths that never call `begin()` still fall back to the process environment
/// captured when `STATE` is first initialized.
pub fn begin(semantics: &rush_command_core::CommandSemantics) {
    let out_path = semantics
        .getenv(b"RUSH_ACTION_TIMING")
        .and_then(|v| std::str::from_utf8(v).ok())
        .filter(|p| !p.is_empty())
        .map(str::to_owned);
    let mut state = STATE.lock().unwrap_or_else(|p| p.into_inner());
    state.out_path = out_path;
}

fn current_path() -> Option<String> {
    STATE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .out_path
        .clone()
}

pub fn enabled() -> bool {
    current_path().is_some()
}

pub struct ExecutionTimer {
    started_at: std::time::Instant,
    started_unix_ns: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct ExecutionPhase {
    started_unix_ns: u64,
    finished_unix_ns: u64,
    duration: Duration,
}

impl ExecutionTimer {
    pub fn start_if_enabled() -> Option<Self> {
        enabled().then(|| Self {
            started_at: std::time::Instant::now(),
            started_unix_ns: unix_now_ns(),
        })
    }

    pub fn finish(self) -> ExecutionPhase {
        ExecutionPhase {
            started_unix_ns: self.started_unix_ns,
            finished_unix_ns: unix_now_ns(),
            duration: self.started_at.elapsed(),
        }
    }
}

fn unix_now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}

pub fn record(
    action: &rush_action::Action,
    owner: Option<&str>,
    wall: Duration,
    exit_code: Option<i32>,
    cached: bool,
) {
    record_with_execution(action, owner, wall, None, exit_code, cached);
}

pub fn record_with_execution(
    action: &rush_action::Action,
    owner: Option<&str>,
    wall: Duration,
    execution: Option<ExecutionPhase>,
    exit_code: Option<i32>,
    cached: bool,
) {
    record_full(
        action,
        owner,
        wall,
        execution,
        PhaseBreakdown::default(),
        exit_code,
        cached,
    );
}

/// Record an action with its full phase decomposition.
pub fn record_full(
    action: &rush_action::Action,
    owner: Option<&str>,
    wall: Duration,
    execution: Option<ExecutionPhase>,
    phases: PhaseBreakdown,
    exit_code: Option<i32>,
    cached: bool,
) {
    let Some(path) = current_path() else { return };

    let primary_output = action.outputs.first().map(|out| match out {
        rush_action::Artifact::Source(p) => p.display().to_string(),
        rush_action::Artifact::Derived { output_path, .. } => output_path.display().to_string(),
    });
    let finished_unix_ns = unix_now_ns();
    let wall_ns = wall.as_nanos().min(u64::MAX as u128) as u64;
    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    let accounted = execution
        .as_ref()
        .map(|p| p.duration)
        .unwrap_or_default()
        .saturating_add(phases.resource_wait.unwrap_or_default())
        .saturating_add(phases.commit.unwrap_or_default())
        .saturating_add(phases.cache_lookup.unwrap_or_default());
    let record = ActionTimingRecord {
        schema_version: 4,
        event: "action_timing",
        owner,
        mnemonic: &action.mnemonic,
        action_id: action.id.to_string(),
        primary_output,
        executable: action.command.executable.display().to_string(),
        argv_len: action.command.args.len(),
        argv_bytes: action.command.args.iter().map(|arg| arg.len()).sum(),
        started_unix_ns: finished_unix_ns.saturating_sub(wall_ns),
        finished_unix_ns,
        execution_started_unix_ns: execution
            .as_ref()
            .map(|phase| phase.started_unix_ns),
        execution_finished_unix_ns: execution
            .as_ref()
            .map(|phase| phase.finished_unix_ns),
        wall_ms: wall.as_secs_f64() * 1e3,
        execution_ms: execution
            .as_ref()
            .map(|phase| phase.duration.as_secs_f64() * 1e3),
        non_execution_ms: execution
            .as_ref()
            .map(|phase| wall.saturating_sub(phase.duration).as_secs_f64() * 1e3),
        resource_wait_ms: phases.resource_wait.map(ms),
        commit_ms: phases.commit.map(ms),
        cache_lookup_ms: phases.cache_lookup.map(ms),
        manager_overhead_ms: Some(ms(wall.saturating_sub(accounted))),
        exit_code,
        cached,
    };
    let Ok(line) = serde_json::to_string(&record) else {
        return;
    };

    let _guard = WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}
