// Rush Build Benchmarking
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Build performance benchmarking and metrics.

use owo_colors::OwoColorize;
use std::time::{Duration, Instant};

/// Build benchmark results.
#[derive(Debug, Clone)]
pub struct BenchmarkResults {
    /// Total build time.
    pub total_duration: Duration,

    /// Time spent loading BUILD files.
    pub load_duration: Duration,

    /// Time spent in analysis phase.
    pub analysis_duration: Duration,

    /// Time spent in execution phase.
    pub execution_duration: Duration,

    /// Number of actions executed.
    pub actions_executed: usize,

    /// Number of cache hits.
    pub cache_hits: usize,

    /// Number of targets built.
    pub targets_built: usize,
}

impl BenchmarkResults {
    /// Create new benchmark results.
    pub fn new() -> Self {
        BenchmarkResults {
            total_duration: Duration::ZERO,
            load_duration: Duration::ZERO,
            analysis_duration: Duration::ZERO,
            execution_duration: Duration::ZERO,
            actions_executed: 0,
            cache_hits: 0,
            targets_built: 0,
        }
    }

    /// Calculate cache hit rate as percentage.
    pub fn cache_hit_rate(&self) -> f64 {
        if self.actions_executed == 0 {
            0.0
        } else {
            (self.cache_hits as f64 / self.actions_executed as f64) * 100.0
        }
    }

    /// Calculate actions per second.
    pub fn actions_per_second(&self) -> f64 {
        if self.execution_duration.as_secs_f64() == 0.0 {
            0.0
        } else {
            self.actions_executed as f64 / self.execution_duration.as_secs_f64()
        }
    }

    /// Print benchmark summary.
    pub fn print_summary(&self) {
        eprintln!("\n{}", "⏱️  Benchmark Results:".bold().cyan());

        let total = self.total_duration.as_secs_f64();
        let total_text = format!("{:.3}s", total);
        let total_colored = if total < 1.0 {
            total_text.green().to_string()
        } else if total < 10.0 {
            total_text.bright_white().to_string()
        } else {
            total_text.yellow().to_string()
        };
        eprintln!("   {} {}", "Total time:".bright_black(), total_colored);

        // Phase times with percentages
        let load_pct = (self.load_duration.as_secs_f64() / total) * 100.0;
        eprintln!(
            "   {} {:.3}s {}",
            "Load time:".bright_black(),
            self.load_duration.as_secs_f64().to_string().cyan(),
            format!("({:.1}%)", load_pct).bright_black()
        );

        let analysis_pct = (self.analysis_duration.as_secs_f64() / total) * 100.0;
        eprintln!(
            "   {} {:.3}s {}",
            "Analysis time:".bright_black(),
            self.analysis_duration.as_secs_f64().to_string().cyan(),
            format!("({:.1}%)", analysis_pct).bright_black()
        );

        let execution_pct = (self.execution_duration.as_secs_f64() / total) * 100.0;
        eprintln!(
            "   {} {:.3}s {}",
            "Execution time:".bright_black(),
            self.execution_duration.as_secs_f64().to_string().cyan(),
            format!("({:.1}%)", execution_pct).bright_black()
        );

        eprintln!(
            "   {} {}",
            "Targets built:".bright_black(),
            self.targets_built.to_string().cyan()
        );
        eprintln!(
            "   {} {}",
            "Actions executed:".bright_black(),
            self.actions_executed.to_string().cyan()
        );

        // Cache hits with color based on hit rate
        let cache_rate = self.cache_hit_rate();
        let cache_text = format!("{} ({:.1}%)", self.cache_hits, cache_rate);
        let cache_colored = if cache_rate > 80.0 {
            cache_text.green().to_string()
        } else if cache_rate > 50.0 {
            cache_text.yellow().to_string()
        } else {
            cache_text.bright_white().to_string()
        };
        eprintln!("   {} {}", "Cache hits:".bright_black(), cache_colored);

        // Actions per second
        let aps = self.actions_per_second();
        let aps_text = format!("{:.1}", aps);
        let aps_colored = if aps > 100.0 {
            aps_text.green().to_string()
        } else if aps > 50.0 {
            aps_text.bright_white().to_string()
        } else {
            aps_text.yellow().to_string()
        };
        eprintln!("   {} {}", "Actions/second:".bright_black(), aps_colored);
    }

    /// Print detailed benchmark report.
    pub fn print_detailed(&self) {
        self.print_summary();

        eprintln!();
        eprintln!("{}", "📊 Performance Breakdown:".bold().cyan());

        // Phase percentages with visual bars
        let total = self.total_duration.as_secs_f64();
        let load_pct = (self.load_duration.as_secs_f64() / total) * 100.0;
        let analysis_pct = (self.analysis_duration.as_secs_f64() / total) * 100.0;
        let execution_pct = (self.execution_duration.as_secs_f64() / total) * 100.0;

        Self::print_phase_bar("Load", load_pct, self.load_duration.as_secs_f64());
        Self::print_phase_bar(
            "Analysis",
            analysis_pct,
            self.analysis_duration.as_secs_f64(),
        );
        Self::print_phase_bar(
            "Execution",
            execution_pct,
            self.execution_duration.as_secs_f64(),
        );

        // Efficiency metrics
        eprintln!();
        eprintln!("{}", "📈 Efficiency Metrics:".bold().cyan());

        let cache_rate = self.cache_hit_rate();
        let cache_text = format!("{:.1}%", cache_rate);
        let cache_colored = if cache_rate > 80.0 {
            cache_text.green().to_string()
        } else if cache_rate > 50.0 {
            cache_text.yellow().to_string()
        } else {
            cache_text.red().to_string()
        };
        eprintln!(
            "   {} {}",
            "Cache efficiency:".bright_black(),
            cache_colored
        );

        if self.actions_executed > 0 {
            let avg_action_time =
                self.execution_duration.as_secs_f64() / self.actions_executed as f64;
            let avg_text = format!("{:.3}s", avg_action_time);
            let avg_colored = if avg_action_time < 0.1 {
                avg_text.green().to_string()
            } else if avg_action_time < 1.0 {
                avg_text.bright_white().to_string()
            } else {
                avg_text.yellow().to_string()
            };
            eprintln!("   {} {}", "Avg action time:".bright_black(), avg_colored);
        }

        if self.targets_built > 0 {
            let avg_target_time = self.total_duration.as_secs_f64() / self.targets_built as f64;
            eprintln!(
                "   {} {}",
                "Avg target time:".bright_black(),
                format!("{:.3}s", avg_target_time).cyan()
            );
        }
    }

    /// Print a phase bar with percentage and time
    fn print_phase_bar(phase: &str, percentage: f64, duration: f64) {
        let bar_width = 40;
        let filled = ((percentage / 100.0) * bar_width as f64) as usize;
        let empty = bar_width - filled;

        let bar = format!("{}{}", "█".repeat(filled), "░".repeat(empty));

        let bar_colored = if percentage > 60.0 {
            bar.yellow().to_string()
        } else if percentage > 30.0 {
            bar.cyan().to_string()
        } else {
            bar.green().to_string()
        };

        eprintln!(
            "   {:>9}: {} {:>5.1}% {}",
            phase.bright_black(),
            bar_colored,
            percentage.to_string().bright_white(),
            format!("({:.3}s)", duration).bright_black()
        );
    }
}

impl Default for BenchmarkResults {
    fn default() -> Self {
        Self::new()
    }
}

/// Timer for measuring build phase durations.
#[derive(Debug)]
pub struct BenchmarkTimer {
    start: Instant,
    results: BenchmarkResults,
}

impl BenchmarkTimer {
    /// Create a new benchmark timer.
    pub fn new() -> Self {
        BenchmarkTimer {
            start: Instant::now(),
            results: BenchmarkResults::new(),
        }
    }

    /// Record loading phase duration.
    pub fn record_load(&mut self, duration: Duration) {
        self.results.load_duration = duration;
    }

    /// Record analysis phase duration.
    pub fn record_analysis(&mut self, duration: Duration) {
        self.results.analysis_duration = duration;
    }

    /// Record execution phase duration.
    pub fn record_execution(&mut self, duration: Duration) {
        self.results.execution_duration = duration;
    }

    /// Record action execution.
    pub fn record_action(&mut self, cache_hit: bool) {
        self.results.actions_executed += 1;
        if cache_hit {
            self.results.cache_hits += 1;
        }
    }

    /// Record target built.
    pub fn record_target(&mut self) {
        self.results.targets_built += 1;
    }

    /// Finalize and get results.
    pub fn finish(mut self) -> BenchmarkResults {
        self.results.total_duration = self.start.elapsed();
        self.results
    }
}

impl Default for BenchmarkTimer {
    fn default() -> Self {
        Self::new()
    }
}
