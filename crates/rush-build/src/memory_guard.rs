// Rush Build - Memory Guard
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Memory monitoring and protection for Rush builds.
//!
//! Prevents excessive memory consumption by monitoring system memory usage
//! and bailing out when a threshold is exceeded, likely indicating a bug.

use anyhow::{bail, Result};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{error, info, warn};

/// Memory usage information in bytes.
#[derive(Debug, Clone, Copy)]
pub struct MemoryInfo {
    /// Total system memory
    pub total: u64,
    /// Available memory
    pub available: u64,
    /// Used memory (total - available)
    pub used: u64,
}

impl MemoryInfo {
    /// Get current system memory information from /proc/meminfo (Linux).
    pub fn current() -> Result<Self> {
        let meminfo = std::fs::read_to_string("/proc/meminfo")?;

        let mut total = None;
        let mut available = None;

        for line in meminfo.lines() {
            if let Some(val) = line.strip_prefix("MemTotal:") {
                total = Some(parse_meminfo_kb(val)?);
            } else if let Some(val) = line.strip_prefix("MemAvailable:") {
                available = Some(parse_meminfo_kb(val)?);
            }

            if total.is_some() && available.is_some() {
                break;
            }
        }

        let total = total.ok_or_else(|| anyhow::anyhow!("MemTotal not found in /proc/meminfo"))?;
        let available =
            available.ok_or_else(|| anyhow::anyhow!("MemAvailable not found in /proc/meminfo"))?;
        let used = total.saturating_sub(available);

        Ok(MemoryInfo {
            total,
            available,
            used,
        })
    }

    /// Convert bytes to GB for display.
    pub fn used_gb(&self) -> f64 {
        self.used as f64 / (1024.0 * 1024.0 * 1024.0)
    }

    /// Convert bytes to GB for display.
    pub fn total_gb(&self) -> f64 {
        self.total as f64 / (1024.0 * 1024.0 * 1024.0)
    }
}

/// Parse memory value from /proc/meminfo line (format: "123456 kB").
fn parse_meminfo_kb(s: &str) -> Result<u64> {
    let trimmed = s.trim();
    let kb_str = trimmed
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("Invalid meminfo format"))?;

    let kb = kb_str.parse::<u64>()?;
    Ok(kb * 1024) // Convert KB to bytes
}

/// Memory guard configuration.
#[derive(Debug, Clone)]
pub struct MemoryGuardConfig {
    /// Memory threshold in bytes. When exceeded, build will be aborted.
    pub threshold_bytes: u64,

    /// Early warning threshold (percentage of threshold_bytes).
    /// Abort when we hit this percentage to prevent getting close to limits.
    /// Default: 70% (abort at 70% of threshold, not 100%)
    pub early_abort_percentage: u8,

    /// Check interval for memory monitoring.
    pub check_interval: Duration,

    /// Whether memory guard is enabled.
    pub enabled: bool,
}

impl Default for MemoryGuardConfig {
    fn default() -> Self {
        // Calculate sensible threshold based on total system RAM
        // CONSERVATIVE: 40% of total system RAM (was 60%, reduced to prevent thrashing)
        // Early abort: 60% of that (so 24% of total RAM)
        //
        // Rationale: Large builds (TensorFlow) can trigger parallel .bzl loading
        // that exceeds estimates. Being conservative prevents system thrashing.

        let threshold_bytes = if let Ok(mem_info) = MemoryInfo::current() {
            // Check if user overrode with env var (in GB)
            if let Some(threshold_gb) = std::env::var("RUSH_MEMORY_THRESHOLD_GB")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
            {
                threshold_gb * 1024 * 1024 * 1024
            } else {
                // CONSERVATIVE: 40% of total system RAM (was 60%)
                // This leaves 60% for OS, other apps, and buffer against estimation errors
                (mem_info.total as f64 * 0.40) as u64
            }
        } else {
            // Fallback if we can't read system memory: conservative 10GB (was 16GB)
            10 * 1024 * 1024 * 1024
        };

        // Early abort at 60% by default (was 70%, can override with env var)
        let early_abort_pct = std::env::var("RUSH_MEMORY_EARLY_ABORT_PCT")
            .ok()
            .and_then(|s| s.parse::<u8>().ok())
            .unwrap_or(60); // Abort at 60% of threshold (24% total RAM)

        // Aggressive 1 second checks to catch spikes
        Self {
            threshold_bytes,
            early_abort_percentage: early_abort_pct.min(100),
            check_interval: Duration::from_secs(1),
            enabled: true,
        }
    }
}

impl MemoryGuardConfig {
    /// Create config with threshold in GB.
    pub fn with_threshold_gb(threshold_gb: u64) -> Self {
        Self {
            threshold_bytes: threshold_gb * 1024 * 1024 * 1024,
            early_abort_percentage: 70,
            check_interval: Duration::from_secs(1),
            enabled: true,
        }
    }

    /// Get threshold in GB.
    pub fn threshold_gb(&self) -> f64 {
        self.threshold_bytes as f64 / (1024.0 * 1024.0 * 1024.0)
    }

    /// Get effective abort threshold in bytes (considering early abort percentage).
    pub fn effective_abort_threshold(&self) -> u64 {
        (self.threshold_bytes as f64 * (self.early_abort_percentage as f64 / 100.0)) as u64
    }

    /// Get effective abort threshold in GB.
    pub fn effective_abort_threshold_gb(&self) -> f64 {
        self.effective_abort_threshold() as f64 / (1024.0 * 1024.0 * 1024.0)
    }
}

/// Memory guard monitor that runs in the background.
pub struct MemoryGuard {
    config: MemoryGuardConfig,
    running: Arc<AtomicBool>,
    threshold_exceeded: Arc<AtomicBool>,
    peak_memory: Arc<AtomicU64>,
    baseline_memory: Arc<AtomicU64>,
    abort_signal: Arc<tokio::sync::Notify>,
    resource_manager: Arc<Mutex<Option<Arc<rush_resource::ResourceManager>>>>,
}

impl MemoryGuard {
    /// Create a new memory guard with the given configuration.
    pub fn new(config: MemoryGuardConfig) -> Self {
        // Capture baseline memory usage at creation time
        let baseline = if let Ok(mem_info) = MemoryInfo::current() {
            mem_info.used
        } else {
            0
        };

        Self {
            config,
            running: Arc::new(AtomicBool::new(false)),
            threshold_exceeded: Arc::new(AtomicBool::new(false)),
            peak_memory: Arc::new(AtomicU64::new(0)),
            baseline_memory: Arc::new(AtomicU64::new(baseline)),
            abort_signal: Arc::new(tokio::sync::Notify::new()),
            resource_manager: Arc::new(Mutex::new(None)),
        }
    }

    /// Set the resource manager for parallelization metrics.
    pub fn set_resource_manager(&self, resource_manager: Arc<rush_resource::ResourceManager>) {
        if let Ok(mut rm) = self.resource_manager.lock() {
            *rm = Some(resource_manager);
        }
    }

    /// Start monitoring memory in the background.
    pub fn start_monitoring(&self) -> tokio::task::JoinHandle<()> {
        if !self.config.enabled {
            info!("Memory guard is disabled");
            return tokio::spawn(async {});
        }

        let config = self.config.clone();
        let running = self.running.clone();
        let threshold_exceeded = self.threshold_exceeded.clone();
        let peak_memory = self.peak_memory.clone();
        let baseline_memory = self.baseline_memory.clone();
        let abort_signal = self.abort_signal.clone();
        let resource_manager = self.resource_manager.clone();

        running.store(true, Ordering::SeqCst);

        // Read total system memory and baseline for logging
        let (total_system_gb, baseline_gb) = if let Ok(mem_info) = MemoryInfo::current() {
            let baseline = baseline_memory.load(Ordering::Relaxed);
            (
                mem_info.total_gb(),
                baseline as f64 / (1024.0 * 1024.0 * 1024.0),
            )
        } else {
            (0.0, 0.0)
        };

        info!(
            "Memory guard starting: effective_abort={:.2} GB ({}% of {:.2} GB max), total_system={:.2} GB, baseline={:.2} GB, check_interval={:?}",
            config.effective_abort_threshold_gb(),
            config.early_abort_percentage,
            config.threshold_gb(),
            total_system_gb,
            baseline_gb,
            config.check_interval
        );

        tokio::spawn(async move {
            let start_time = Instant::now();
            let mut check_count = 0;

            while running.load(Ordering::SeqCst) {
                sleep(config.check_interval).await;

                match MemoryInfo::current() {
                    Ok(mem_info) => {
                        check_count += 1;

                        // Calculate Rush's memory usage as delta from baseline
                        let baseline = baseline_memory.load(Ordering::Relaxed);
                        let rush_memory_used = mem_info.used.saturating_sub(baseline);

                        // Update peak memory (tracking Rush's delta, not total system memory)
                        let mut peak = peak_memory.load(Ordering::Relaxed);
                        while rush_memory_used > peak {
                            match peak_memory.compare_exchange_weak(
                                peak,
                                rush_memory_used,
                                Ordering::SeqCst,
                                Ordering::Relaxed,
                            ) {
                                Ok(_) => break,
                                Err(x) => peak = x,
                            }
                        }

                        // Check EFFECTIVE threshold (early abort at 60% by default)
                        let effective_threshold = config.effective_abort_threshold();

                        // CRITICAL: Also check available memory
                        // If available memory is getting dangerously low, abort even if we haven't
                        // hit our percentage threshold. This catches external memory pressure.
                        let available_gb = mem_info.available as f64 / (1024.0 * 1024.0 * 1024.0);
                        let low_available_threshold_gb = 2.0; // Abort if < 2GB available

                        if rush_memory_used > effective_threshold {
                            threshold_exceeded.store(true, Ordering::SeqCst);
                            warn!("╔════════════════════════════════════════════════════════════════╗");
                            warn!(
                                "║ MEMORY USAGE HIGH - Resource manager will throttle            ║"
                            );
                            warn!("╚════════════════════════════════════════════════════════════════╝");
                            let rush_used_gb = rush_memory_used as f64 / (1024.0 * 1024.0 * 1024.0);
                            let baseline_gb = baseline as f64 / (1024.0 * 1024.0 * 1024.0);
                            warn!(
                                "Rush memory usage: {:.2} GB / {:.2} GB (threshold), baseline: {:.2} GB",
                                rush_used_gb,
                                config.effective_abort_threshold_gb(),
                                baseline_gb
                            );
                            warn!(
                                "Configured: {}% of {:.2} GB max threshold",
                                config.early_abort_percentage,
                                config.threshold_gb()
                            );
                            warn!(
                                "Build runtime: {:.1}s, memory checks: {}",
                                start_time.elapsed().as_secs_f64(),
                                check_count
                            );
                            warn!(
                                "Peak Rush memory: {:.2} GB",
                                peak as f64 / (1024.0 * 1024.0 * 1024.0)
                            );
                            warn!("");
                            warn!(
                                "Build will continue with reduced parallelism via PSI throttling."
                            );
                            warn!("Resource manager will queue new jobs and let running jobs complete.");
                            warn!("");
                            warn!("To adjust thresholds:");
                            warn!(
                                "  RUSH_MEMORY_THRESHOLD_GB=<max_gb> (default: 40% of total RAM)"
                            );
                            warn!("  RUSH_MEMORY_EARLY_ABORT_PCT=<percent> (default: 60)");

                            // Signal abort to main build process
                            abort_signal.notify_waiters();

                            // ABORT - memory threshold exceeded, likely indicates memory leak or bug
                            // Resource manager throttling didn't prevent the issue
                        } else if available_gb < low_available_threshold_gb {
                            threshold_exceeded.store(true, Ordering::SeqCst);
                            warn!("╔════════════════════════════════════════════════════════════════╗");
                            warn!(
                                "║ AVAILABLE MEMORY LOW - Throttling build                       ║"
                            );
                            warn!("╚════════════════════════════════════════════════════════════════╝");
                            warn!(
                                "Available memory: {:.2} GB (low threshold: {:.2} GB)",
                                available_gb, low_available_threshold_gb
                            );
                            let rush_used_gb = rush_memory_used as f64 / (1024.0 * 1024.0 * 1024.0);
                            let baseline_gb = baseline as f64 / (1024.0 * 1024.0 * 1024.0);
                            warn!(
                                "Rush memory: {:.2} GB / {:.2} GB (effective threshold), baseline: {:.2} GB",
                                rush_used_gb,
                                config.effective_abort_threshold_gb(),
                                baseline_gb
                            );
                            warn!(
                                "Build runtime: {:.1}s, memory checks: {}",
                                start_time.elapsed().as_secs_f64(),
                                check_count
                            );
                            warn!("");
                            warn!("System available memory is low.");
                            warn!(
                                "This may indicate external memory pressure from other processes."
                            );
                            warn!("Build will continue with minimal parallelism (1 CPU) to avoid OOM.");
                            warn!("");

                            // Signal abort to main build process
                            abort_signal.notify_waiters();

                            // ABORT - available memory critically low, must stop to prevent OOM kill
                        }

                        // Log memory status periodically (every 10 checks)
                        if check_count % 10 == 0 {
                            let rush_used_gb = rush_memory_used as f64 / (1024.0 * 1024.0 * 1024.0);
                            let usage_pct =
                                (rush_memory_used as f64 / effective_threshold as f64) * 100.0;

                            // Get parallelization metrics if resource manager is available
                            let rm_clone = {
                                if let Ok(rm_guard) = resource_manager.lock() {
                                    rm_guard.clone()
                                } else {
                                    None
                                }
                            };

                            let parallel_info = if let Some(rm) = rm_clone {
                                let stats = rm.stats().await;
                                format!(" | {}", stats.format_parallelization())
                            } else {
                                String::new()
                            };

                            if usage_pct > 60.0 {
                                warn!(
                                    "Rush memory status: {:.2} GB / {:.2} GB ({:.1}%), peak: {:.2} GB{}",
                                    rush_used_gb,
                                    config.effective_abort_threshold_gb(),
                                    usage_pct,
                                    peak as f64 / (1024.0 * 1024.0 * 1024.0),
                                    parallel_info
                                );
                            } else {
                                info!(
                                    "Rush memory status: {:.2} GB / {:.2} GB ({:.1}%), peak: {:.2} GB{}",
                                    rush_used_gb,
                                    config.effective_abort_threshold_gb(),
                                    usage_pct,
                                    peak as f64 / (1024.0 * 1024.0 * 1024.0),
                                    parallel_info
                                );
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to read memory info: {}", e);
                    }
                }
            }

            running.store(false, Ordering::SeqCst);
            info!("Memory guard stopped");
        })
    }

    /// Stop monitoring.
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    /// Check if threshold was exceeded.
    pub fn threshold_exceeded(&self) -> bool {
        self.threshold_exceeded.load(Ordering::SeqCst)
    }

    /// Get peak memory usage in bytes.
    pub fn peak_memory_bytes(&self) -> u64 {
        self.peak_memory.load(Ordering::SeqCst)
    }

    /// Get peak memory usage in GB.
    pub fn peak_memory_gb(&self) -> f64 {
        self.peak_memory_bytes() as f64 / (1024.0 * 1024.0 * 1024.0)
    }

    /// Check current memory and return error if threshold exceeded.
    pub fn check_or_abort(&self) -> Result<()> {
        if !self.config.enabled {
            return Ok(());
        }

        if self.threshold_exceeded() {
            let mem_info = MemoryInfo::current()?;
            let baseline = self.baseline_memory.load(Ordering::Relaxed);
            let rush_memory_used = mem_info.used.saturating_sub(baseline);
            let rush_used_gb = rush_memory_used as f64 / (1024.0 * 1024.0 * 1024.0);
            let baseline_gb = baseline as f64 / (1024.0 * 1024.0 * 1024.0);
            bail!(
                "Memory threshold exceeded: Rush used {:.2} GB / {:.2} GB (peak: {:.2} GB), baseline: {:.2} GB\n\
                 This likely indicates a bug in Rush. Please report this issue with:\n\
                 - Target being built\n\
                 - Memory configuration\n\
                 - Build logs",
                rush_used_gb,
                self.config.threshold_gb(),
                self.peak_memory_gb(),
                baseline_gb
            );
        }

        Ok(())
    }

    /// Get a future that completes when the abort signal is triggered.
    /// Use this to race against long-running operations.
    pub async fn wait_for_abort(&self) {
        self.abort_signal.notified().await;
    }

    /// Check if abort was signaled (non-blocking).
    pub fn abort_signaled(&self) -> bool {
        self.threshold_exceeded()
    }

    /// Dump diagnostic information to help debug memory issues.
    pub fn dump_diagnostics(&self, output_dir: &Path) -> Result<()> {
        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let dump_file = output_dir.join(format!("memory_dump_{}.txt", timestamp));

        let mem_info = MemoryInfo::current()?;

        let mut content = String::new();
        content.push_str("=== Rush Memory Diagnostic Dump ===\n");
        content.push_str(&format!("Timestamp: {}\n\n", chrono::Utc::now()));

        content.push_str("Memory Usage:\n");
        content.push_str(&format!("  Total:     {:.2} GB\n", mem_info.total_gb()));
        content.push_str(&format!("  Used:      {:.2} GB\n", mem_info.used_gb()));
        content.push_str(&format!(
            "  Available: {:.2} GB\n",
            mem_info.available as f64 / (1024.0 * 1024.0 * 1024.0)
        ));
        content.push_str(&format!("  Peak:      {:.2} GB\n\n", self.peak_memory_gb()));

        content.push_str("Threshold:\n");
        content.push_str(&format!(
            "  Configured: {:.2} GB\n",
            self.config.threshold_gb()
        ));
        content.push_str(&format!("  Exceeded:   {}\n\n", self.threshold_exceeded()));

        // Add process memory information
        if let Ok(proc_status) = std::fs::read_to_string("/proc/self/status") {
            content.push_str("Process Memory (from /proc/self/status):\n");
            for line in proc_status.lines() {
                if line.starts_with("Vm") || line.starts_with("Rss") {
                    content.push_str(&format!("  {}\n", line));
                }
            }
            content.push('\n');
        }

        std::fs::write(&dump_file, content)?;
        error!("Memory diagnostics dumped to: {}", dump_file.display());

        Ok(())
    }
}

impl Drop for MemoryGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_info() {
        let mem_info = MemoryInfo::current().expect("Failed to get memory info");
        assert!(mem_info.total > 0);
        assert!(mem_info.used > 0);
        assert!(mem_info.used <= mem_info.total);
    }

    #[tokio::test]
    async fn test_memory_guard_basic() {
        let config = MemoryGuardConfig {
            threshold_bytes: 1024 * 1024 * 1024 * 1024, // 1 TB - won't be exceeded
            early_abort_percentage: 70,
            check_interval: Duration::from_millis(100),
            enabled: true,
        };

        let guard = MemoryGuard::new(config);
        let handle = guard.start_monitoring();

        sleep(Duration::from_millis(300)).await;

        assert!(!guard.threshold_exceeded());
        guard.stop();
        handle.await.unwrap();
    }
}
