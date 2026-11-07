// Rush Build Profiling
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Build profiling and trace generation.
//!
//! Generates Chrome Trace Event Format JSON for visualization in chrome://tracing

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static NEXT_TRACE_THREAD_ID: AtomicU32 = AtomicU32::new(1);

thread_local! {
    static TRACE_THREAD_ID: u32 = NEXT_TRACE_THREAD_ID.fetch_add(1, Ordering::Relaxed);
}

/// Chrome Trace Event.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TraceEvent {
    /// Event name.
    name: String,

    /// Event category.
    cat: String,

    /// Event phase: B (begin), E (end), X (complete), i (instant).
    ph: String,

    /// Timestamp in microseconds.
    ts: u64,

    /// Process ID.
    pid: u32,

    /// Thread ID.
    tid: u32,

    /// Duration in microseconds (for complete events).
    #[serde(skip_serializing_if = "Option::is_none")]
    dur: Option<u64>,

    /// Event arguments.
    #[serde(skip_serializing_if = "Option::is_none")]
    args: Option<serde_json::Value>,
}

/// Build profiler that records trace events.
#[derive(Debug, Clone)]
pub struct BuildProfiler {
    start_time: Instant,
    started_unix_ns: u64,
    events: Arc<Mutex<Vec<TraceEvent>>>,
    thread_names: Arc<Mutex<HashMap<u32, String>>>,
    lane_pool: Arc<Mutex<LanePool>>,
    enabled: bool,
}

/// Free-list of virtual trace lanes; see scope_on_lane.
#[derive(Debug, Default)]
struct LanePool {
    free: Vec<u32>,
    next: u32,
}

impl BuildProfiler {
    /// Create a new profiler.
    pub fn new(enabled: bool) -> Self {
        BuildProfiler {
            start_time: Instant::now(),
            started_unix_ns: unix_now_ns(),
            events: Arc::new(Mutex::new(Vec::new())),
            thread_names: Arc::new(Mutex::new(HashMap::new())),
            lane_pool: Arc::new(Mutex::new(LanePool::default())),
            enabled,
        }
    }

    /// Record a complete event (has duration).
    pub fn record_event(&self, name: &str, category: &str, duration: Duration) {
        if !self.enabled {
            return;
        }

        let dur = duration.as_micros() as u64;
        let finished_at = self.start_time.elapsed().as_micros() as u64;
        self.record_complete_at(
            name,
            category,
            finished_at.saturating_sub(dur),
            dur,
            None,
        );
    }

    /// Record an instant event (no duration).
    pub fn record_instant(&self, name: &str, category: &str) {
        if !self.enabled {
            return;
        }

        let ts = self.start_time.elapsed().as_micros() as u64;

        let event = TraceEvent {
            name: name.to_string(),
            cat: category.to_string(),
            ph: "i".to_string(),
            ts,
            pid: std::process::id(),
            tid: self.current_thread_id(),
            dur: None,
            args: None,
        };

        if let Ok(mut events) = self.events.lock() {
            events.push(event);
        }
    }

    /// Record event with arguments.
    pub fn record_with_args(
        &self,
        name: &str,
        category: &str,
        duration: Duration,
        args: serde_json::Value,
    ) {
        if !self.enabled {
            return;
        }

        let dur = duration.as_micros() as u64;
        let finished_at = self.start_time.elapsed().as_micros() as u64;
        self.record_complete_at(
            name,
            category,
            finished_at.saturating_sub(dur),
            dur,
            Some(args),
        );
    }

    /// Create a scoped event that records automatically on drop.
    pub fn scope(&self, name: &str, category: &str) -> ScopedEvent {
        ScopedEvent {
            profiler: self.clone(),
            name: name.to_string(),
            category: category.to_string(),
            start: Instant::now(),
            start_ts: self.start_time.elapsed().as_micros() as u64,
            lane: None,
        }
    }

    /// A scoped event on a virtual lane instead of the recording thread.
    ///
    /// Wavefront target spans are created and dropped in async blocks that
    /// one driver thread polls, so plain scope() stamps every overlapping
    /// target with the SAME tid and a trace viewer stacks 96-wide
    /// concurrency into one unreadable row. Lanes are pooled: a span takes
    /// the lowest free lane and returns it on drop, so the trace shows as
    /// many rows as the real peak concurrency, no more.
    pub fn scope_on_lane(&self, name: &str, category: &str) -> ScopedEvent {
        let lane = self.acquire_lane();
        ScopedEvent {
            profiler: self.clone(),
            name: name.to_string(),
            category: category.to_string(),
            start: Instant::now(),
            start_ts: self.start_time.elapsed().as_micros() as u64,
            lane: Some(lane),
        }
    }

    fn acquire_lane(&self) -> u32 {
        let mut pool = self.lane_pool.lock().unwrap();
        let lane = pool.free.pop().unwrap_or_else(|| {
            pool.next += 1;
            // Offset keeps lanes clear of real TRACE_THREAD_ID values.
            10_000 + pool.next - 1
        });
        if let Ok(mut names) = self.thread_names.lock() {
            names
                .entry(lane)
                .or_insert_with(|| format!("lane [{}]", lane - 10_000));
        }
        lane
    }

    fn release_lane(&self, lane: u32) {
        if let Ok(mut pool) = self.lane_pool.lock() {
            pool.free.push(lane);
        }
    }

    fn record_complete_at(
        &self,
        name: &str,
        category: &str,
        ts: u64,
        dur: u64,
        args: Option<serde_json::Value>,
    ) {
        let tid = self.current_thread_id();
        self.record_complete_at_tid(name, category, ts, dur, args, tid);
    }

    fn record_complete_at_tid(
        &self,
        name: &str,
        category: &str,
        ts: u64,
        dur: u64,
        args: Option<serde_json::Value>,
        tid: u32,
    ) {
        // ScopedEvent::drop reaches this directly, so the gate belongs here
        // rather than only in record_event. Without it a disabled profiler
        // still accumulated a TraceEvent per scope, and the daemon holds one
        // profiler across every build it serves.
        if !self.enabled {
            return;
        }
        let event = TraceEvent {
            name: name.to_string(),
            cat: category.to_string(),
            ph: "X".to_string(),
            ts,
            pid: std::process::id(),
            tid,
            dur: Some(dur),
            args,
        };

        if let Ok(mut events) = self.events.lock() {
            events.push(event);
        }
    }

    fn current_thread_id(&self) -> u32 {
        let id = TRACE_THREAD_ID.with(|id| *id);
        let thread = std::thread::current();
        let base_name = thread.name().unwrap_or("worker");
        if let Ok(mut names) = self.thread_names.lock() {
            names
                .entry(id)
                .or_insert_with(|| format!("{base_name} [{id}]"));
        }
        id
    }

    /// Write trace to JSON file.
    pub fn write_trace(&self, path: &Path) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let pid = std::process::id();
        // Drain rather than borrow: a daemon holds one profiler across every
        // build it serves, so without the drain each written trace repeated
        // all prior builds' events and the Vec grew for the life of the
        // process (~4200 events per whole-repo build). Draining makes each
        // file exactly one build's events and bounds the memory.
        let events: Vec<TraceEvent> = {
            let mut guard = self.events.lock().unwrap();
            std::mem::take(&mut *guard)
        };
        let thread_names = self.thread_names.lock().unwrap();
        let mut trace_events = Vec::with_capacity(events.len() + thread_names.len() + 1);
        trace_events.push(TraceEvent {
            name: "process_name".to_string(),
            cat: "__metadata".to_string(),
            ph: "M".to_string(),
            ts: 0,
            pid,
            tid: 0,
            dur: None,
            args: Some(serde_json::json!({ "name": "rush" })),
        });
        for (tid, name) in thread_names.iter() {
            trace_events.push(TraceEvent {
                name: "thread_name".to_string(),
                cat: "__metadata".to_string(),
                ph: "M".to_string(),
                ts: 0,
                pid,
                tid: *tid,
                dur: None,
                args: Some(serde_json::json!({ "name": name })),
            });
        }
        trace_events.extend(events.iter().cloned());

        let trace = serde_json::json!({
            "traceEvents": trace_events,
            "displayTimeUnit": "ms",
            "otherData": {
                "version": env!("CARGO_PKG_VERSION"),
                "tool": "rush",
                "started_unix_ns": self.started_unix_ns.to_string(),
                "clock": "monotonic-relative",
            }
        });

        let mut file = File::create(path)?;
        file.write_all(serde_json::to_string_pretty(&trace)?.as_bytes())?;

        eprintln!("\n📊 Trace written to: {}", path.display());
        eprintln!("   Open in https://ui.perfetto.dev/ to visualize");

        Ok(())
    }

    /// Get event count.
    pub fn event_count(&self) -> usize {
        self.events.lock().unwrap().len()
    }
}

/// Scoped event that records duration automatically.
pub struct ScopedEvent {
    profiler: BuildProfiler,
    name: String,
    category: String,
    start: Instant,
    start_ts: u64,
    lane: Option<u32>,
}

impl Drop for ScopedEvent {
    fn drop(&mut self) {
        let duration = self.start.elapsed();
        match self.lane {
            Some(lane) => {
                self.profiler.record_complete_at_tid(
                    &self.name,
                    &self.category,
                    self.start_ts,
                    duration.as_micros() as u64,
                    None,
                    lane,
                );
                self.profiler.release_lane(lane);
            }
            None => self.profiler.record_complete_at(
                &self.name,
                &self.category,
                self.start_ts,
                duration.as_micros() as u64,
                None,
            ),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_profiler_basic() {
        let profiler = BuildProfiler::new(true);

        profiler.record_event("test_event", "test", Duration::from_millis(100));
        profiler.record_instant("instant", "test");

        assert_eq!(profiler.event_count(), 2);
    }

    #[test]
    fn test_disabled_profiler() {
        let profiler = BuildProfiler::new(false);

        profiler.record_event("test", "test", Duration::from_millis(100));

        assert_eq!(profiler.event_count(), 0);
    }

    #[test]
    fn test_scoped_event() {
        let profiler = BuildProfiler::new(true);

        {
            let _scope = profiler.scope("scoped_test", "test");
            std::thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(profiler.event_count(), 1);
    }
}
