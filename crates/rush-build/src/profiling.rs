// Rush Build - Build Profiling and Flame Graphs
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Build profiling and flame graph generation.
//!
//! Captures detailed timing information during builds and generates flame graphs
//! for visualizing where time is spent. Helps identify bottlenecks and optimize
//! build performance.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::time::Instant;

/// Build profiler.
pub struct BuildProfiler {
    /// Profiling spans.
    spans: Vec<ProfileSpan>,

    /// Active spans (stack).
    active_spans: Vec<usize>,

    /// Start time.
    start_time: Instant,

    /// Span metadata.
    metadata: HashMap<usize, SpanMetadata>,
}

/// Profiling span.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileSpan {
    /// Span ID.
    pub id: usize,

    /// Span name.
    pub name: String,

    /// Category (e.g., "compile", "link", "test").
    pub category: String,

    /// Start time (microseconds from build start).
    pub start_us: u64,

    /// Duration (microseconds).
    pub duration_us: u64,

    /// Parent span ID.
    pub parent_id: Option<usize>,

    /// Thread/worker ID.
    pub thread_id: usize,

    /// Custom attributes.
    pub attributes: HashMap<String, String>,
}

/// Span metadata (not serialized).
struct SpanMetadata {
    start_instant: Instant,
}

impl BuildProfiler {
    /// Create a new build profiler.
    pub fn new() -> Self {
        BuildProfiler {
            spans: Vec::new(),
            active_spans: Vec::new(),
            start_time: Instant::now(),
            metadata: HashMap::new(),
        }
    }

    /// Start a new span.
    pub fn start_span(&mut self, name: String, category: String, thread_id: usize) -> usize {
        let span_id = self.spans.len();
        let now = Instant::now();
        let parent_id = self.active_spans.last().copied();

        let span = ProfileSpan {
            id: span_id,
            name,
            category,
            start_us: now.duration_since(self.start_time).as_micros() as u64,
            duration_us: 0, // Will be set when span ends
            parent_id,
            thread_id,
            attributes: HashMap::new(),
        };

        self.metadata
            .insert(span_id, SpanMetadata { start_instant: now });

        self.spans.push(span);
        self.active_spans.push(span_id);

        span_id
    }

    /// End a span.
    pub fn end_span(&mut self, span_id: usize) {
        if let Some(pos) = self.active_spans.iter().position(|&id| id == span_id) {
            self.active_spans.remove(pos);
        }

        if let Some(metadata) = self.metadata.get(&span_id) {
            let duration = metadata.start_instant.elapsed();

            if let Some(span) = self.spans.get_mut(span_id) {
                span.duration_us = duration.as_micros() as u64;
            }

            self.metadata.remove(&span_id);
        }
    }

    /// Add attribute to current span.
    pub fn add_attribute(&mut self, key: String, value: String) {
        if let Some(&span_id) = self.active_spans.last() {
            if let Some(span) = self.spans.get_mut(span_id) {
                span.attributes.insert(key, value);
            }
        }
    }

    /// Get all spans.
    pub fn spans(&self) -> &[ProfileSpan] {
        &self.spans
    }

    /// Generate flame graph in folded stack format.
    pub fn to_folded_stacks(&self) -> String {
        let mut stacks: HashMap<String, u64> = HashMap::new();

        for span in &self.spans {
            let stack = self.build_stack_name(span);
            *stacks.entry(stack).or_insert(0) += span.duration_us;
        }

        let mut lines: Vec<_> = stacks
            .iter()
            .map(|(stack, duration)| format!("{} {}", stack, duration))
            .collect();

        lines.sort();
        lines.join("\n")
    }

    /// Build stack name for a span.
    fn build_stack_name(&self, span: &ProfileSpan) -> String {
        let mut parts = Vec::new();
        let mut current = Some(span);

        while let Some(s) = current {
            parts.push(s.name.clone());

            current = s.parent_id.and_then(|id| self.spans.get(id));
        }

        parts.reverse();
        parts.join(";")
    }

    /// Generate Chrome tracing JSON format.
    pub fn to_chrome_trace(&self) -> Result<String> {
        #[derive(Serialize)]
        struct TraceEvent {
            name: String,
            cat: String,
            ph: String,
            ts: u64,
            dur: u64,
            pid: usize,
            tid: usize,
            args: HashMap<String, String>,
        }

        let events: Vec<_> = self
            .spans
            .iter()
            .map(|span| TraceEvent {
                name: span.name.clone(),
                cat: span.category.clone(),
                ph: "X".to_string(), // Complete event
                ts: span.start_us,
                dur: span.duration_us,
                pid: 1, // Process ID (constant for single build)
                tid: span.thread_id,
                args: span.attributes.clone(),
            })
            .collect();

        serde_json::to_string_pretty(&events).context("Failed to serialize trace")
    }

    /// Export profile data.
    pub fn export(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(&self.spans)?;
        std::fs::write(path, json).context("Failed to write profile data")
    }

    /// Generate summary statistics.
    pub fn summary(&self) -> ProfileSummary {
        let mut category_times: HashMap<String, u64> = HashMap::new();
        let mut thread_times: HashMap<usize, u64> = HashMap::new();

        let mut total_time_us = 0;

        for span in &self.spans {
            if span.parent_id.is_none() {
                // Only count top-level spans for total time
                total_time_us = total_time_us.max(span.start_us + span.duration_us);
            }

            *category_times.entry(span.category.clone()).or_insert(0) += span.duration_us;
            *thread_times.entry(span.thread_id).or_insert(0) += span.duration_us;
        }

        let longest_span = self.spans.iter().max_by_key(|s| s.duration_us).cloned();

        ProfileSummary {
            total_time_us,
            total_spans: self.spans.len(),
            category_times,
            thread_times,
            longest_span,
        }
    }
}

impl Default for BuildProfiler {
    fn default() -> Self {
        Self::new()
    }
}

/// Profile summary statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileSummary {
    /// Total build time (microseconds).
    pub total_time_us: u64,

    /// Total number of spans.
    pub total_spans: usize,

    /// Time per category.
    pub category_times: HashMap<String, u64>,

    /// Time per thread.
    pub thread_times: HashMap<usize, u64>,

    /// Longest span.
    pub longest_span: Option<ProfileSpan>,
}

impl ProfileSummary {
    /// Get total time in milliseconds.
    pub fn total_time_ms(&self) -> f64 {
        self.total_time_us as f64 / 1000.0
    }

    /// Get category breakdown as percentages.
    pub fn category_percentages(&self) -> HashMap<String, f64> {
        let total: u64 = self.category_times.values().sum();

        if total == 0 {
            return HashMap::new();
        }

        self.category_times
            .iter()
            .map(|(cat, &time)| {
                let percentage = (time as f64 / total as f64) * 100.0;
                (cat.clone(), percentage)
            })
            .collect()
    }

    /// Print summary to stdout.
    pub fn print(&self) {
        eprintln!("Build Profile Summary");
        eprintln!("=====================");
        eprintln!("Total Time: {:.2} ms", self.total_time_ms());
        eprintln!("Total Spans: {}", self.total_spans);
        eprintln!();

        eprintln!("Time by Category:");
        let mut categories: Vec<_> = self.category_times.iter().collect();
        categories.sort_by_key(|(_, &time)| std::cmp::Reverse(time));

        for (category, &time_us) in categories {
            let time_ms = time_us as f64 / 1000.0;
            let percentage = (time_us as f64 / self.total_time_us as f64) * 100.0;
            eprintln!("  {}: {:.2} ms ({:.1}%)", category, time_ms, percentage);
        }

        eprintln!();

        if let Some(ref span) = self.longest_span {
            let span_ms = span.duration_us as f64 / 1000.0;
            eprintln!("Longest Span: {} ({:.2} ms)", span.name, span_ms);
        }
    }
}

/// RAII span guard for automatic span ending.
pub struct SpanGuard<'a> {
    profiler: &'a mut BuildProfiler,
    span_id: usize,
}

impl<'a> SpanGuard<'a> {
    /// Create a new span guard.
    pub fn new(
        profiler: &'a mut BuildProfiler,
        name: String,
        category: String,
        thread_id: usize,
    ) -> Self {
        let span_id = profiler.start_span(name, category, thread_id);
        SpanGuard { profiler, span_id }
    }

    /// Add attribute to this span.
    pub fn add_attr(&mut self, key: String, value: String) {
        if let Some(span) = self.profiler.spans.get_mut(self.span_id) {
            span.attributes.insert(key, value);
        }
    }
}

impl<'a> Drop for SpanGuard<'a> {
    fn drop(&mut self) {
        self.profiler.end_span(self.span_id);
    }
}

/// Macro for easy span creation.
#[macro_export]
macro_rules! profile_span {
    ($profiler:expr, $name:expr, $category:expr, $thread_id:expr) => {
        $crate::profiling::SpanGuard::new(
            $profiler,
            $name.to_string(),
            $category.to_string(),
            $thread_id,
        )
    };
}

/// Flame graph generator.
pub struct FlameGraphGenerator {
    /// Folded stacks data.
    stacks: String,
}

impl FlameGraphGenerator {
    /// Create from profiler.
    pub fn from_profiler(profiler: &BuildProfiler) -> Self {
        FlameGraphGenerator {
            stacks: profiler.to_folded_stacks(),
        }
    }

    /// Create from folded stacks string.
    pub fn from_stacks(stacks: String) -> Self {
        FlameGraphGenerator { stacks }
    }

    /// Generate SVG flame graph.
    pub fn generate_svg(&self) -> Result<String> {
        // In practice, this would use the inferno or flamegraph crate
        // For now, generate a simple SVG structure

        let mut svg =
            String::from(r#"<svg viewBox="0 0 1200 800" xmlns="http://www.w3.org/2000/svg">"#);
        svg.push_str("\n<style>rect { stroke: white; } text { font-family: monospace; font-size: 12px; }</style>\n");

        let lines: Vec<_> = self.stacks.lines().collect();
        let total_samples: u64 = lines
            .iter()
            .filter_map(|line| line.split_whitespace().last())
            .filter_map(|s| s.parse::<u64>().ok())
            .sum();

        let mut y = 0;

        for line in lines {
            let parts: Vec<_> = line.split_whitespace().collect();
            if parts.len() < 2 {
                continue;
            }

            let samples = parts
                .last()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let width = (samples as f64 / total_samples as f64) * 1200.0;

            let stack_name = parts[..parts.len() - 1].join(" ");

            svg.push_str(&format!(
                r#"<rect x="0" y="{}" width="{}" height="20" fill="orange"/>"#,
                y, width
            ));

            svg.push_str(&format!(
                r#"<text x="5" y="{}" fill="black">{}</text>"#,
                y + 15,
                stack_name
            ));

            y += 22;
        }

        svg.push_str("\n</svg>");

        Ok(svg)
    }

    /// Write SVG to file.
    pub fn write_svg<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let svg = self.generate_svg()?;
        std::fs::write(path.as_ref(), svg).context("Failed to write SVG")
    }
}

/// Performance counter for detailed metrics.
pub struct PerformanceCounter {
    /// Counter name.
    name: String,

    /// Sample count.
    count: u64,

    /// Total value.
    total: u64,

    /// Minimum value.
    min: u64,

    /// Maximum value.
    max: u64,
}

impl PerformanceCounter {
    /// Create a new performance counter.
    pub fn new(name: String) -> Self {
        PerformanceCounter {
            name,
            count: 0,
            total: 0,
            min: u64::MAX,
            max: 0,
        }
    }

    /// Record a sample.
    pub fn record(&mut self, value: u64) {
        self.count += 1;
        self.total += value;
        self.min = self.min.min(value);
        self.max = self.max.max(value);
    }

    /// Get average value.
    pub fn average(&self) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        self.total as f64 / self.count as f64
    }

    /// Get statistics.
    pub fn stats(&self) -> CounterStats {
        CounterStats {
            name: self.name.clone(),
            count: self.count,
            total: self.total,
            min: if self.count > 0 { self.min } else { 0 },
            max: self.max,
            average: self.average(),
        }
    }
}

/// Counter statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CounterStats {
    /// Counter name.
    pub name: String,

    /// Sample count.
    pub count: u64,

    /// Total value.
    pub total: u64,

    /// Minimum value.
    pub min: u64,

    /// Maximum value.
    pub max: u64,

    /// Average value.
    pub average: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_profiler_basic() {
        let mut profiler = BuildProfiler::new();

        let span1 = profiler.start_span("compile".to_string(), "build".to_string(), 0);
        thread::sleep(Duration::from_millis(10));
        profiler.end_span(span1);

        let spans = profiler.spans();
        assert_eq!(spans.len(), 1);
        assert!(spans[0].duration_us > 0);
    }

    #[test]
    fn test_nested_spans() {
        let mut profiler = BuildProfiler::new();

        let outer = profiler.start_span("build".to_string(), "build".to_string(), 0);
        let inner = profiler.start_span("compile".to_string(), "compile".to_string(), 0);
        thread::sleep(Duration::from_millis(5));
        profiler.end_span(inner);
        profiler.end_span(outer);

        let spans = profiler.spans();
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[1].parent_id, Some(0));
    }

    #[test]
    fn test_folded_stacks() {
        let mut profiler = BuildProfiler::new();

        let outer = profiler.start_span("build".to_string(), "build".to_string(), 0);
        let inner = profiler.start_span("compile".to_string(), "compile".to_string(), 0);
        profiler.end_span(inner);
        profiler.end_span(outer);

        let stacks = profiler.to_folded_stacks();
        assert!(stacks.contains("build;compile"));
    }

    #[test]
    fn test_summary() {
        let mut profiler = BuildProfiler::new();

        profiler.start_span("compile1".to_string(), "compile".to_string(), 0);
        profiler.end_span(0);

        profiler.start_span("link1".to_string(), "link".to_string(), 0);
        profiler.end_span(1);

        let summary = profiler.summary();
        assert_eq!(summary.total_spans, 2);
        assert!(summary.category_times.contains_key("compile"));
        assert!(summary.category_times.contains_key("link"));
    }

    #[test]
    fn test_performance_counter() {
        let mut counter = PerformanceCounter::new("test".to_string());

        counter.record(10);
        counter.record(20);
        counter.record(30);

        let stats = counter.stats();
        assert_eq!(stats.count, 3);
        assert_eq!(stats.min, 10);
        assert_eq!(stats.max, 30);
        assert_eq!(stats.average, 20.0);
    }
}
