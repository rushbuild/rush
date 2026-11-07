// Build progress tracking
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Progress reporting for builds.
//!
//! Tracks compilation progress, cache hits, and timing statistics.

use indicatif::{ProgressBar, ProgressStyle};
use owo_colors::OwoColorize;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Build progress tracker
#[derive(Clone)]
pub struct BuildProgress {
    /// Total actions to execute
    total_actions: Arc<AtomicUsize>,

    /// Completed actions
    completed_actions: Arc<AtomicUsize>,

    /// Cache hits
    cache_hits: Arc<AtomicUsize>,

    /// Targets skipped (incremental builds)
    targets_skipped: Arc<AtomicUsize>,

    /// Start time
    start_time: Instant,

    /// Progress bar (optional, for visual feedback)
    progress_bar: Arc<Mutex<Option<ProgressBar>>>,

    /// Whether progress bar is enabled
    bar_enabled: Arc<AtomicBool>,
}

impl BuildProgress {
    /// Create a new progress tracker
    pub fn new() -> Self {
        Self {
            total_actions: Arc::new(AtomicUsize::new(0)),
            completed_actions: Arc::new(AtomicUsize::new(0)),
            cache_hits: Arc::new(AtomicUsize::new(0)),
            targets_skipped: Arc::new(AtomicUsize::new(0)),
            start_time: Instant::now(),
            progress_bar: Arc::new(Mutex::new(None)),
            bar_enabled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Enable progress bar with visual feedback
    pub fn enable_progress_bar(&self) {
        self.bar_enabled.store(true, Ordering::Release);

        let pb = ProgressBar::new(0);
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} ({percent}%) {msg}")
                .unwrap()
                .progress_chars("█▓░"),
        );

        *self.progress_bar.lock().unwrap() = Some(pb);
    }

    /// Update progress bar with current state
    fn update_progress_bar(&self) {
        if !self.bar_enabled.load(Ordering::Acquire) {
            return;
        }

        if let Ok(guard) = self.progress_bar.lock() {
            if let Some(pb) = guard.as_ref() {
                let (completed, total) = self.current();
                let cache_hits = self.cache_hit_count();
                let elapsed = self.elapsed();

                // Calculate ETA
                let eta_msg = if completed > 0 && completed < total {
                    let avg_time = elapsed.as_secs_f64() / completed as f64;
                    let remaining = total - completed;
                    let eta_secs = avg_time * remaining as f64;
                    format!("ETA: {:.0}s | ", eta_secs)
                } else {
                    String::new()
                };

                // Cache hit rate
                let cache_msg = if completed > 0 {
                    let rate = (cache_hits as f64 / completed as f64) * 100.0;
                    format!("{}Cache: {:.0}% | ", eta_msg, rate)
                } else {
                    eta_msg
                };

                // Elapsed time
                let time_msg = format!("{}Elapsed: {:.1}s", cache_msg, elapsed.as_secs_f64());

                pb.set_length(total as u64);
                pb.set_position(completed as u64);
                pb.set_message(time_msg);
            }
        }
    }

    /// Finish the progress bar
    pub fn finish_progress_bar(&self) {
        if !self.bar_enabled.load(Ordering::Acquire) {
            return;
        }

        if let Ok(mut guard) = self.progress_bar.lock() {
            if let Some(pb) = guard.take() {
                let (completed, total) = self.current();
                let elapsed = self.elapsed();
                let cache_rate = self.cache_hit_rate();

                let final_msg = format!(
                    "Completed {}/{} actions | Cache: {}% | Time: {:.2}s",
                    completed,
                    total,
                    cache_rate,
                    elapsed.as_secs_f64()
                );

                pb.finish_with_message(final_msg);
                // Explicitly drop to release any background threads
                drop(pb);
            }
        }
    }

    /// Set total number of actions
    pub fn set_total(&self, total: usize) {
        self.total_actions.store(total, Ordering::Release);
        self.update_progress_bar();
    }

    /// Increment total actions
    pub fn add_total(&self, count: usize) {
        self.total_actions.fetch_add(count, Ordering::Release);
        self.update_progress_bar();
    }

    /// Record action completion
    pub fn complete_action(&self) {
        self.completed_actions.fetch_add(1, Ordering::Release);
        self.update_progress_bar();
    }

    /// Record cache hit
    pub fn record_cache_hit(&self) {
        self.cache_hits.fetch_add(1, Ordering::Release);
        self.complete_action();
    }

    /// Record target skip (incremental build)
    pub fn record_target_skip(&self) {
        self.targets_skipped.fetch_add(1, Ordering::Release);
    }

    /// Get targets skipped count
    pub fn targets_skipped_count(&self) -> usize {
        self.targets_skipped.load(Ordering::Acquire)
    }

    /// Get current progress (completed, total)
    pub fn current(&self) -> (usize, usize) {
        (
            self.completed_actions.load(Ordering::Acquire),
            self.total_actions.load(Ordering::Acquire),
        )
    }

    /// Get cache hit count
    pub fn cache_hit_count(&self) -> usize {
        self.cache_hits.load(Ordering::Acquire)
    }

    /// Get elapsed time
    pub fn elapsed(&self) -> Duration {
        self.start_time.elapsed()
    }

    /// Get progress percentage (0-100)
    pub fn percentage(&self) -> u32 {
        let (completed, total) = self.current();
        if total == 0 {
            return 0;
        }
        ((completed as f64 / total as f64) * 100.0) as u32
    }

    /// Get cache hit rate (0-100)
    pub fn cache_hit_rate(&self) -> u32 {
        let (completed, _) = self.current();
        if completed == 0 {
            return 0;
        }
        let hits = self.cache_hit_count();
        ((hits as f64 / completed as f64) * 100.0) as u32
    }

    /// Print progress line
    pub fn print_progress(&self, message: &str) {
        if !self.bar_enabled.load(Ordering::Acquire) {
            return;
        }

        // When progress bar is enabled, print above it.
        if let Ok(guard) = self.progress_bar.lock() {
            if let Some(pb) = guard.as_ref() {
                pb.println(format!("  {}", message));
            }
        }
    }

    /// Print action progress with emoji and color
    pub fn print_action(&self, action: &str, target: &str) {
        if !self.bar_enabled.load(Ordering::Acquire) {
            return;
        }

        // When progress bar is enabled, print above it.
        if let Ok(guard) = self.progress_bar.lock() {
            if let Some(pb) = guard.as_ref() {
                let (completed, total) = self.current();
                if total > 0 {
                    let progress_str = format!("[{}/{}]", completed, total);
                    let action_str = action.to_string();
                    let target_str = target.to_string();
                    pb.println(format!(
                        "  {} {} {}",
                        progress_str.cyan(),
                        action_str.bold(),
                        target_str.bright_white()
                    ));
                } else {
                    pb.println(format!("  {} {}", action.bold(), target.bright_white()));
                }
            }
        }
    }

    /// Print summary statistics
    pub fn print_summary(&self) {
        let (completed, total) = self.current();
        let elapsed = self.elapsed();
        let cache_hits = self.cache_hit_count();
        let targets_skipped = self.targets_skipped_count();

        eprintln!("\n{}", "📊 Build Summary:".bold().cyan());

        // Actions
        let actions_text = format!("Actions: {}/{} completed", completed, total);
        eprintln!("   {}", actions_text.bright_white());

        // Cache hits
        let cache_rate = self.cache_hit_rate();
        let cache_text = format!("Cache hits: {} ({}%)", cache_hits, cache_rate);
        if cache_rate > 80 {
            eprintln!("   {}", cache_text.green());
        } else if cache_rate > 50 {
            eprintln!("   {}", cache_text.yellow());
        } else {
            eprintln!("   {}", cache_text.bright_white());
        }

        // Skipped targets
        if targets_skipped > 0 {
            let skip_text = format!("Targets skipped: {} (up to date)", targets_skipped);
            eprintln!("   {}", skip_text.green());
        }

        // Time
        let time_text = format!("Time: {:.2}s", elapsed.as_secs_f64());
        if elapsed.as_secs_f64() < 1.0 {
            eprintln!("   {}", time_text.green());
        } else if elapsed.as_secs_f64() < 10.0 {
            eprintln!("   {}", time_text.bright_white());
        } else {
            eprintln!("   {}", time_text.yellow());
        }

        // Average time per action
        if completed > 0 {
            let avg_time = elapsed.as_secs_f64() / completed as f64;
            let avg_text = format!("Avg: {:.0}ms per action", avg_time * 1000.0);
            eprintln!("   {}", avg_text.bright_black());
        }
    }
}

impl Default for BuildProgress {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_progress_tracking() {
        let progress = BuildProgress::new();

        progress.set_total(10);
        assert_eq!(progress.current(), (0, 10));
        assert_eq!(progress.percentage(), 0);

        progress.complete_action();
        progress.complete_action();
        assert_eq!(progress.current(), (2, 10));
        assert_eq!(progress.percentage(), 20);

        progress.record_cache_hit();
        assert_eq!(progress.current(), (3, 10));
        assert_eq!(progress.cache_hit_count(), 1);
        assert_eq!(progress.cache_hit_rate(), 33);
    }

    #[test]
    fn test_add_total() {
        let progress = BuildProgress::new();

        progress.set_total(5);
        progress.add_total(3);
        assert_eq!(progress.current(), (0, 8));
    }
}
