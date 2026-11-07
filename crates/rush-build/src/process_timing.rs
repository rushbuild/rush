// Rush Build - Process-level wall-vs-CPU timing (phase 5.C.4 commit 125)
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Phase 5.C.4 commit 125: process-level CPU vs wall
//! reporting for the analyze pass.
//!
//! Pro's commit-125 directive (Batch P): *"Add process-
//! level CPU vs wall reporting for analyze-only timing.
//! Collect wall time, user CPU time, system CPU time,
//! CPU utilization percentage, voluntary context
//! switches, involuntary context switches, major/minor
//! page faults. Use getrusage(RUSAGE_SELF) on Unix."*
//!
//! Pro's diagnosis of the Batch O measurement:
//!
//!   wall time ≈ 90 s
//!   CPU time  ≈ 1.7 – 3.0 s
//!
//! "That means the remaining 90s floor is probably
//! wait/blocking/I/O/lock/fetch/async scheduling time,
//! not pure interpreter CPU."
//!
//! This module turns that accidental observation into a
//! first-class report. Operators looking at a build
//! that runs slow can see immediately whether they're
//! CPU-bound or wait-bound.
//!
//! ## Cost model
//!
//! - Disabled (gate `RUSH_ANALYZE_PROCESS_TIMING=1`):
//!   zero — the snapshot pair isn't even taken.
//! - Enabled: two getrusage() calls (entry + exit) and a
//!   `Instant::now()` pair. Total cost is microseconds.

use std::time::{Duration, Instant};

/// Phase 5.C.4 commit 125: snapshot of process-level
/// resource usage at a point in time. Built from
/// `getrusage(RUSAGE_SELF)` on Unix; zeroed on
/// platforms that don't expose it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessResourceSnapshot {
    pub wall: Instant,
    pub user_cpu: Duration,
    pub sys_cpu: Duration,
    pub voluntary_context_switches: u64,
    pub involuntary_context_switches: u64,
    pub major_page_faults: u64,
    pub minor_page_faults: u64,
    pub max_resident_set_kb: u64,
}

impl ProcessResourceSnapshot {
    /// Synthetic "zero" snapshot anchored at the given
    /// instant. For tests + serialization defaults.
    pub fn zero_at(wall: Instant) -> Self {
        Self {
            wall,
            user_cpu: Duration::ZERO,
            sys_cpu: Duration::ZERO,
            voluntary_context_switches: 0,
            involuntary_context_switches: 0,
            major_page_faults: 0,
            minor_page_faults: 0,
            max_resident_set_kb: 0,
        }
    }
}

impl ProcessResourceSnapshot {
    /// Capture a snapshot at the current instant.
    ///
    /// On Unix, this calls `getrusage(RUSAGE_SELF)`. On
    /// other platforms (including any future build for
    /// non-Unix), all rusage fields are zero and only
    /// `wall` is meaningful — callers should still
    /// compute wall-time deltas.
    #[allow(dead_code)]
    pub fn capture() -> Self {
        let wall = Instant::now();
        #[cfg(unix)]
        {
            let r = nix::sys::resource::getrusage(
                nix::sys::resource::UsageWho::RUSAGE_SELF,
            );
            match r {
                Ok(usage) => {
                    let user = usage.user_time();
                    let sys = usage.system_time();
                    return Self {
                        wall,
                        user_cpu: Duration::new(
                            user.tv_sec() as u64,
                            (user.tv_usec() as u32) * 1000,
                        ),
                        sys_cpu: Duration::new(
                            sys.tv_sec() as u64,
                            (sys.tv_usec() as u32) * 1000,
                        ),
                        voluntary_context_switches: usage
                            .voluntary_context_switches() as u64,
                        involuntary_context_switches: usage
                            .involuntary_context_switches() as u64,
                        major_page_faults: usage.major_page_faults() as u64,
                        minor_page_faults: usage.minor_page_faults() as u64,
                        max_resident_set_kb: usage.max_rss() as u64,
                    };
                }
                Err(_) => {}
            }
        }
        Self {
            wall,
            user_cpu: Duration::ZERO,
            sys_cpu: Duration::ZERO,
            voluntary_context_switches: 0,
            involuntary_context_switches: 0,
            major_page_faults: 0,
            minor_page_faults: 0,
            max_resident_set_kb: 0,
        }
    }
}

/// Phase 5.C.4 commit 125: delta between two snapshots
/// = the resource usage actually consumed by the work
/// in between. This is what operators want to see for
/// the analyze pass.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ProcessTimingSummary {
    pub schema_version: u32,
    /// Wall-clock duration in milliseconds.
    pub wall_ms: u64,
    /// User CPU time in milliseconds.
    pub user_cpu_ms: u64,
    /// System CPU time in milliseconds.
    pub sys_cpu_ms: u64,
    /// Combined CPU (user + sys) in milliseconds.
    pub total_cpu_ms: u64,
    /// CPU utilization as a percentage of wall time.
    /// 100% means single-threaded fully busy. Values
    /// can exceed 100% on multi-core parallel work.
    /// `wait_ratio_pct = 100 - cpu_utilization_pct`
    /// approximates how much of wall was wait/block.
    pub cpu_utilization_pct: f64,
    /// Voluntary context switches (the process gave up
    /// the CPU willingly, typically blocked on I/O or
    /// a lock).
    pub voluntary_context_switches: u64,
    /// Involuntary context switches (the kernel
    /// preempted, typically because the time slice
    /// expired). High values can indicate scheduler
    /// pressure.
    pub involuntary_context_switches: u64,
    /// Major page faults (had to read from disk).
    pub major_page_faults: u64,
    /// Minor page faults (page-table miss only).
    pub minor_page_faults: u64,
    /// Peak resident-set size in KB.
    pub max_resident_set_kb: u64,
}

const PROCESS_TIMING_SCHEMA_VERSION: u32 = 1;

impl ProcessTimingSummary {
    /// Compute the delta between two snapshots.
    ///
    /// Wall time is the elapsed Instant. CPU times and
    /// counters are subtracted; rusage counters are
    /// process-cumulative so the subtract gives the
    /// "between snapshots" delta.
    pub fn from_delta(
        start: &ProcessResourceSnapshot,
        end: &ProcessResourceSnapshot,
    ) -> Self {
        let wall = end.wall.duration_since(start.wall);
        let user_cpu = end.user_cpu.saturating_sub(start.user_cpu);
        let sys_cpu = end.sys_cpu.saturating_sub(start.sys_cpu);
        let total_cpu = user_cpu + sys_cpu;
        let wall_ms = wall.as_millis() as u64;
        let user_cpu_ms = user_cpu.as_millis() as u64;
        let sys_cpu_ms = sys_cpu.as_millis() as u64;
        let total_cpu_ms = total_cpu.as_millis() as u64;
        let cpu_utilization_pct = if wall_ms > 0 {
            (total_cpu_ms as f64 / wall_ms as f64) * 100.0
        } else {
            0.0
        };
        Self {
            schema_version: PROCESS_TIMING_SCHEMA_VERSION,
            wall_ms,
            user_cpu_ms,
            sys_cpu_ms,
            total_cpu_ms,
            cpu_utilization_pct,
            voluntary_context_switches: end
                .voluntary_context_switches
                .saturating_sub(start.voluntary_context_switches),
            involuntary_context_switches: end
                .involuntary_context_switches
                .saturating_sub(start.involuntary_context_switches),
            major_page_faults: end
                .major_page_faults
                .saturating_sub(start.major_page_faults),
            minor_page_faults: end
                .minor_page_faults
                .saturating_sub(start.minor_page_faults),
            max_resident_set_kb: end.max_resident_set_kb,
        }
    }

    /// Render the summary as human-readable lines.
    pub fn format_human(&self) -> Vec<String> {
        let wait_ratio_pct = (100.0 - self.cpu_utilization_pct).max(0.0);
        vec![
            format!(
                "wall={} ms  user_cpu={} ms  sys_cpu={} ms  total_cpu={} ms",
                self.wall_ms,
                self.user_cpu_ms,
                self.sys_cpu_ms,
                self.total_cpu_ms,
            ),
            format!(
                "cpu_utilization={:.1}%  approx_wait_ratio={:.1}%",
                self.cpu_utilization_pct, wait_ratio_pct,
            ),
            format!(
                "context_switches: voluntary={} involuntary={}",
                self.voluntary_context_switches,
                self.involuntary_context_switches,
            ),
            format!(
                "page_faults: major={} minor={}  max_rss_kb={}",
                self.major_page_faults,
                self.minor_page_faults,
                self.max_resident_set_kb,
            ),
        ]
    }
}

/// Env-var gate. When `RUSH_ANALYZE_PROCESS_TIMING=1`
/// is set, the orchestrator captures a snapshot at
/// analyze-pass entry and emits a delta summary at
/// exit. Default off (no rusage calls made).
pub fn process_timing_enabled() -> bool {
    std::env::var("RUSH_ANALYZE_PROCESS_TIMING").as_deref() == Ok("1")
}

/// JSON-output gate. Requires the human gate to be on
/// first (same pattern as parse_timing /
/// cache_counters / hotspots).
pub fn process_timing_json_enabled() -> bool {
    process_timing_enabled()
        && std::env::var("RUSH_ANALYZE_PROCESS_TIMING_JSON").as_deref()
            == Ok("1")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// commit 125 test #1: a non-zero delta serializes
    /// to JSON with the documented field set.
    #[test]
    fn cpu_wall_summary_serializes() {
        let start = ProcessResourceSnapshot::capture();
        // Do a tiny bit of CPU-bound work so the delta
        // isn't degenerate zero.
        let mut acc: u64 = 0;
        for i in 0..10_000u64 {
            acc = acc.wrapping_add(i.wrapping_mul(31));
        }
        std::hint::black_box(acc);
        let end = ProcessResourceSnapshot::capture();
        let s = ProcessTimingSummary::from_delta(&start, &end);
        let json = serde_json::to_string(&s).expect("serialize");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("parse");
        assert_eq!(parsed["schema_version"], 1);
        assert!(parsed["wall_ms"].as_u64().is_some());
        assert!(parsed["user_cpu_ms"].as_u64().is_some());
        assert!(parsed["sys_cpu_ms"].as_u64().is_some());
        assert!(parsed["total_cpu_ms"].as_u64().is_some());
        assert!(parsed["cpu_utilization_pct"].as_f64().is_some());
    }

    /// commit 125 test #2: utilization is computed
    /// correctly from the wall + cpu fields. We can't
    /// guarantee a specific value (real rusage is
    /// non-deterministic) but the relationship
    /// `total_cpu_ms / wall_ms * 100 = utilization`
    /// must hold.
    #[test]
    fn cpu_utilization_computed() {
        // Synthesize snapshots with known deltas.
        let now = Instant::now();
        let mut start = ProcessResourceSnapshot::zero_at(now);
        start.user_cpu = Duration::from_millis(0);
        start.sys_cpu = Duration::from_millis(0);
        let mut end = ProcessResourceSnapshot::zero_at(
            now + Duration::from_millis(1000),
        );
        end.user_cpu = Duration::from_millis(300);
        end.sys_cpu = Duration::from_millis(100);
        let s = ProcessTimingSummary::from_delta(&start, &end);
        assert_eq!(s.wall_ms, 1000);
        assert_eq!(s.user_cpu_ms, 300);
        assert_eq!(s.sys_cpu_ms, 100);
        assert_eq!(s.total_cpu_ms, 400);
        assert!((s.cpu_utilization_pct - 40.0).abs() < 0.001);
    }

    /// commit 125 test #3: the env-var gate defaults
    /// to disabled.
    #[test]
    fn disabled_by_default() {
        let prior = std::env::var("RUSH_ANALYZE_PROCESS_TIMING").ok();
        std::env::remove_var("RUSH_ANALYZE_PROCESS_TIMING");
        assert!(!process_timing_enabled());
        std::env::set_var("RUSH_ANALYZE_PROCESS_TIMING", "1");
        assert!(process_timing_enabled());
        match prior {
            Some(v) => std::env::set_var("RUSH_ANALYZE_PROCESS_TIMING", v),
            None => std::env::remove_var("RUSH_ANALYZE_PROCESS_TIMING"),
        }
    }
}
