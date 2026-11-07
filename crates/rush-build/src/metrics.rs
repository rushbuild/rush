// Rush Build Metrics
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Build metrics and analytics tracking.

use owo_colors::OwoColorize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Build metrics collector.
#[derive(Debug, Clone)]
pub struct BuildMetrics {
    /// Inner metrics data.
    inner: Arc<Mutex<MetricsData>>,
}

/// Internal metrics data.
#[derive(Debug)]
struct MetricsData {
    /// Build start time.
    build_start: Option<Instant>,

    /// Total actions executed.
    total_actions: usize,

    /// Actions served from cache.
    cache_hits: usize,

    /// Actions executed locally.
    local_executions: usize,

    /// Remote cache hits.
    remote_cache_hits: usize,

    /// Time spent in different phases.
    phase_times: HashMap<String, Duration>,

    /// Time spent per action type.
    action_times: HashMap<String, Duration>,

    /// Per-target metrics.
    target_metrics: HashMap<String, TargetMetrics>,
}

/// Metrics for a single target.
#[derive(Debug, Clone)]
pub struct TargetMetrics {
    /// Time to build this target.
    pub duration: Duration,

    /// Number of actions for this target.
    pub actions: usize,

    /// Cache hit rate for this target.
    pub cache_hits: usize,

    /// Success status.
    pub success: bool,
}

impl BuildMetrics {
    /// Create a new metrics collector.
    pub fn new() -> Self {
        BuildMetrics {
            inner: Arc::new(Mutex::new(MetricsData {
                build_start: None,
                total_actions: 0,
                cache_hits: 0,
                local_executions: 0,
                remote_cache_hits: 0,
                phase_times: HashMap::new(),
                action_times: HashMap::new(),
                target_metrics: HashMap::new(),
            })),
        }
    }

    /// Mark build start.
    pub fn start_build(&self) {
        let mut data = self.inner.lock().unwrap();
        data.build_start = Some(Instant::now());
    }

    /// Get build elapsed time.
    pub fn build_duration(&self) -> Option<Duration> {
        let data = self.inner.lock().unwrap();
        data.build_start.map(|start| start.elapsed())
    }

    /// Record an action execution.
    pub fn record_action(&self, _mnemonic: &str, was_cached: bool, from_remote: bool) {
        let mut data = self.inner.lock().unwrap();
        data.total_actions += 1;

        if was_cached {
            data.cache_hits += 1;
            if from_remote {
                data.remote_cache_hits += 1;
            }
        } else {
            data.local_executions += 1;
        }
    }

    /// Record time spent in a phase.
    pub fn record_phase(&self, phase: &str, duration: Duration) {
        let mut data = self.inner.lock().unwrap();
        *data
            .phase_times
            .entry(phase.to_string())
            .or_insert(Duration::ZERO) += duration;
    }

    /// Record time spent on an action type.
    pub fn record_action_time(&self, mnemonic: &str, duration: Duration) {
        let mut data = self.inner.lock().unwrap();
        *data
            .action_times
            .entry(mnemonic.to_string())
            .or_insert(Duration::ZERO) += duration;
    }

    /// Record target build metrics.
    pub fn record_target(
        &self,
        target: &str,
        duration: Duration,
        actions: usize,
        cache_hits: usize,
        success: bool,
    ) {
        let mut data = self.inner.lock().unwrap();
        data.target_metrics.insert(
            target.to_string(),
            TargetMetrics {
                duration,
                actions,
                cache_hits,
                success,
            },
        );
    }

    /// Get cache hit rate as percentage.
    pub fn cache_hit_rate(&self) -> f64 {
        let data = self.inner.lock().unwrap();
        if data.total_actions == 0 {
            return 0.0;
        }
        (data.cache_hits as f64 / data.total_actions as f64) * 100.0
    }

    /// Get remote cache hit rate as percentage.
    pub fn remote_cache_hit_rate(&self) -> f64 {
        let data = self.inner.lock().unwrap();
        if data.cache_hits == 0 {
            return 0.0;
        }
        (data.remote_cache_hits as f64 / data.cache_hits as f64) * 100.0
    }

    /// Get total number of actions.
    pub fn total_actions(&self) -> usize {
        let data = self.inner.lock().unwrap();
        data.total_actions
    }

    /// Get number of cache hits.
    pub fn cache_hits(&self) -> usize {
        let data = self.inner.lock().unwrap();
        data.cache_hits
    }

    /// Get number of local executions.
    pub fn local_executions(&self) -> usize {
        let data = self.inner.lock().unwrap();
        data.local_executions
    }

    /// Print detailed metrics summary.
    pub fn print_summary(&self) {
        let data = self.inner.lock().unwrap();

        eprintln!();
        eprintln!("{}", "📊 Build Metrics:".bold().cyan());

        // Overall stats
        if let Some(start) = data.build_start {
            let elapsed = start.elapsed().as_secs_f64();
            let time_colored = if elapsed < 1.0 {
                format!("{:.2}s", elapsed).green().to_string()
            } else if elapsed < 10.0 {
                format!("{:.2}s", elapsed).bright_white().to_string()
            } else {
                format!("{:.2}s", elapsed).yellow().to_string()
            };
            eprintln!("   {} {}", "Total time:".bright_black(), time_colored);
        }

        eprintln!(
            "   {} {}",
            "Total actions:".bright_black(),
            data.total_actions.to_string().cyan()
        );

        // Calculate cache hit rate inline (can't call self.cache_hit_rate() while holding lock)
        let cache_rate = if data.total_actions == 0 {
            0.0
        } else {
            (data.cache_hits as f64 / data.total_actions as f64) * 100.0
        };
        let cache_text = format!("{} ({:.1}%)", data.cache_hits, cache_rate);
        let cache_colored = if cache_rate > 80.0 {
            cache_text.green().to_string()
        } else if cache_rate > 50.0 {
            cache_text.yellow().to_string()
        } else {
            cache_text.bright_white().to_string()
        };
        eprintln!("   {} {}", "Cache hits:".bright_black(), cache_colored);

        eprintln!(
            "   {} {}",
            "Local executions:".bright_black(),
            data.local_executions.to_string().cyan()
        );

        if data.remote_cache_hits > 0 {
            // Calculate remote cache hit rate inline (can't call method while holding lock)
            let remote_rate = if data.cache_hits == 0 {
                0.0
            } else {
                (data.remote_cache_hits as f64 / data.cache_hits as f64) * 100.0
            };
            eprintln!(
                "   {} {} {}",
                "Remote cache hits:".bright_black(),
                data.remote_cache_hits.to_string().cyan(),
                format!("({:.1}% of cached)", remote_rate).bright_black()
            );
        }

        // Phase timings
        if !data.phase_times.is_empty() {
            eprintln!();
            eprintln!("   {}:", "Phase timings".bold());
            let mut phases: Vec<_> = data.phase_times.iter().collect();
            phases.sort_by_key(|(_, duration)| std::cmp::Reverse(*duration));
            for (phase, duration) in phases {
                eprintln!(
                    "     {} {}",
                    format!("{}:", phase).bright_black(),
                    format!("{:.2}s", duration.as_secs_f64()).cyan()
                );
            }
        }

        // Action type timings
        if !data.action_times.is_empty() {
            eprintln!();
            eprintln!("   {}:", "Action timings".bold());
            let mut actions: Vec<_> = data.action_times.iter().collect();
            actions.sort_by_key(|(_, duration)| std::cmp::Reverse(*duration));
            for (action, duration) in actions.iter().take(5) {
                eprintln!(
                    "     {} {}",
                    format!("{}:", action).bright_black(),
                    format!("{:.2}s", duration.as_secs_f64()).cyan()
                );
            }
        }

        // Top slowest targets
        if !data.target_metrics.is_empty() {
            eprintln!();
            eprintln!("   {}:", "Slowest targets".bold().yellow());
            let mut targets: Vec<_> = data.target_metrics.iter().collect();
            targets.sort_by_key(|(_, metrics)| std::cmp::Reverse(metrics.duration));
            for (target, metrics) in targets.iter().take(5) {
                let cache_pct = if metrics.actions > 0 {
                    (metrics.cache_hits as f64 / metrics.actions as f64) * 100.0
                } else {
                    0.0
                };

                eprintln!(
                    "     {} {} {} {} {}",
                    target.bright_white(),
                    "-".bright_black(),
                    format!("{:.2}s", metrics.duration.as_secs_f64()).yellow(),
                    format!("({} actions,", metrics.actions).bright_black(),
                    format!("{:.0}% cached)", cache_pct).bright_black()
                );
            }
        }
    }

    /// Export metrics as JSON.
    pub fn to_json(&self) -> serde_json::Value {
        let data = self.inner.lock().unwrap();

        let build_duration_secs = data
            .build_start
            .map(|start| start.elapsed().as_secs_f64())
            .unwrap_or(0.0);

        serde_json::json!({
            "build_duration_secs": build_duration_secs,
            "total_actions": data.total_actions,
            "cache_hits": data.cache_hits,
            "local_executions": data.local_executions,
            "remote_cache_hits": data.remote_cache_hits,
            "cache_hit_rate": self.cache_hit_rate(),
            "remote_cache_hit_rate": self.remote_cache_hit_rate(),
            "phase_times": data.phase_times.iter()
                .map(|(k, v)| (k.clone(), v.as_secs_f64()))
                .collect::<HashMap<_, _>>(),
            "action_times": data.action_times.iter()
                .map(|(k, v)| (k.clone(), v.as_secs_f64()))
                .collect::<HashMap<_, _>>(),
        })
    }

    /// Export metrics as CSV format.
    pub fn to_csv(&self) -> String {
        let data = self.inner.lock().unwrap();

        let build_duration_secs = data
            .build_start
            .map(|start| start.elapsed().as_secs_f64())
            .unwrap_or(0.0);

        let mut csv = String::new();

        // Header
        csv.push_str("metric,value\n");

        // Basic metrics
        csv.push_str(&format!("build_duration_secs,{:.3}\n", build_duration_secs));
        csv.push_str(&format!("total_actions,{}\n", data.total_actions));
        csv.push_str(&format!("cache_hits,{}\n", data.cache_hits));
        csv.push_str(&format!("local_executions,{}\n", data.local_executions));
        csv.push_str(&format!("remote_cache_hits,{}\n", data.remote_cache_hits));
        csv.push_str(&format!("cache_hit_rate,{:.2}\n", self.cache_hit_rate()));
        csv.push_str(&format!(
            "remote_cache_hit_rate,{:.2}\n",
            self.remote_cache_hit_rate()
        ));

        // Phase times
        csv.push_str("\nphase,duration_secs\n");
        let mut phases: Vec<_> = data.phase_times.iter().collect();
        phases.sort_by_key(|(name, _)| name.as_str());
        for (phase, duration) in phases {
            csv.push_str(&format!("{},{:.3}\n", phase, duration.as_secs_f64()));
        }

        // Action times
        csv.push_str("\naction,duration_secs\n");
        let mut actions: Vec<_> = data.action_times.iter().collect();
        actions.sort_by_key(|(name, _)| name.as_str());
        for (action, duration) in actions {
            csv.push_str(&format!("{},{:.3}\n", action, duration.as_secs_f64()));
        }

        csv
    }
}

impl Default for BuildMetrics {
    fn default() -> Self {
        Self::new()
    }
}
