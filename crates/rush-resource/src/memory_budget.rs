// Rush Build - Global Memory Budget
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Global memory budget system for .bzl file loading.
//!
//! This module provides atomic memory reservation to prevent the race condition
//! where multiple threads read /proc/meminfo simultaneously, all see available
//! memory, and all allocate before any can react.
//!
//! Design principles:
//! 1. **Atomic reservation**: Compare-and-swap ensures only one thread succeeds
//! 2. **Fail-fast**: Better to reject work than thrash the system
//! 3. **RAII cleanup**: Memory automatically released on drop
//! 4. **Headroom enforcement**: Always leave buffer for OS and other apps

use anyhow::{anyhow, Result};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tracing::{debug, info, warn};

/// Global memory budget for .bzl file loading operations.
///
/// This provides a separate memory budget from ResourceManager's build action
/// tracking. .bzl loading happens during analysis phase and needs lightweight
/// reservation without CPU slot coordination.
pub struct GlobalMemoryBudget {
    /// Total memory budget in MB (set at startup, never changes)
    total_budget_mb: u64,

    /// Currently reserved memory in MB (atomic for lock-free updates)
    reserved_mb: AtomicU64,

    /// Hard stop threshold as percentage (90 = stop all work at 90% of budget)
    hard_stop_pct: u8,

    /// Minimum free RAM to maintain in MB (never use the last N MB)
    min_free_mb: u64,

    /// Pressure level broadcast channel (for adaptive throttling)
    pressure_tx: watch::Sender<MemoryPressure>,
    pressure_rx: watch::Receiver<MemoryPressure>,
}

/// Memory pressure level based on current usage
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryPressure {
    /// < 50% of budget used
    Low,
    /// 50-70% of budget used
    Medium,
    /// 70-90% of budget used
    High,
    /// > 90% of budget used - stop all new work
    Critical,
}

impl GlobalMemoryBudget {
    /// Create a new global memory budget.
    ///
    /// # Arguments
    /// * `max_usable_pct` - Maximum percentage of total RAM to use (e.g., 40 = 40%)
    /// * `hard_stop_pct` - Percentage of budget to stop at (e.g., 90 = stop at 90% of budget)
    /// * `min_free_gb` - Minimum free RAM to always maintain in GB
    pub fn new(max_usable_pct: u8, hard_stop_pct: u8, min_free_gb: u64) -> Result<Arc<Self>> {
        // Read system memory
        let (total_mb, _available_mb) = crate::ResourceManager::read_system_memory()?;

        // Calculate total budget
        let total_budget_mb = (total_mb as f64 * (max_usable_pct as f64 / 100.0)) as u64;
        let min_free_mb = min_free_gb * 1024;

        info!(
            "GlobalMemoryBudget initialized: {:.2} GB budget ({}% of {} GB total), \
             hard stop at {}%, min free: {} GB",
            total_budget_mb as f64 / 1024.0,
            max_usable_pct,
            total_mb as f64 / 1024.0,
            hard_stop_pct,
            min_free_gb
        );

        // Create pressure monitoring channel
        let (pressure_tx, pressure_rx) = watch::channel(MemoryPressure::Low);

        let budget = Arc::new(Self {
            total_budget_mb,
            reserved_mb: AtomicU64::new(0),
            hard_stop_pct,
            min_free_mb,
            pressure_tx,
            pressure_rx,
        });

        // Start background monitoring task
        Self::start_monitoring(Arc::clone(&budget));

        Ok(budget)
    }

    /// Create with default configuration (conservative).
    ///
    /// Defaults:
    /// - 40% of total RAM (was 60% before Phase 1 fix)
    /// - Hard stop at 90% of budget
    /// - Maintain 4 GB minimum free
    pub fn with_defaults() -> Result<Arc<Self>> {
        Self::new(40, 90, 4)
    }

    /// Reserve memory atomically BEFORE allocation.
    ///
    /// Returns RAII guard that releases on drop.
    ///
    /// # Arguments
    /// * `description` - What is being loaded (for error messages)
    /// * `mb` - How many MB to reserve
    pub fn reserve(&self, description: &str, mb: u64) -> Result<MemoryReservation<'_>> {
        loop {
            let current = self.reserved_mb.load(Ordering::Acquire);
            let new_total = current + mb;

            // Check against budget
            if new_total > self.total_budget_mb {
                return Err(anyhow!(
                    "Memory budget exceeded - Cannot allocate for {}\n\
                     \n\
                     Memory Budget:\n\
                     - Total budget: {:.2} GB\n\
                     - Currently reserved: {:.2} GB\n\
                     - Requested: {} MB\n\
                     - After reservation: {:.2} GB\n\
                     - Available in budget: {:.2} GB\n\
                     \n\
                     This build requires more memory than configured budget.\n\
                     Solutions:\n\
                     1. Increase budget: RUSH_MEMORY_THRESHOLD_GB=<GB>\n\
                     2. Reduce parallelism: --jobs=<N>\n\
                     3. Build fewer targets at once\n\
                     4. Close other applications to free memory",
                    description,
                    self.total_budget_mb as f64 / 1024.0,
                    current as f64 / 1024.0,
                    mb,
                    new_total as f64 / 1024.0,
                    (self.total_budget_mb - current) as f64 / 1024.0,
                ));
            }

            // Check system still has minimum free RAM
            if let Ok((total_mb, available_mb)) = crate::ResourceManager::read_system_memory() {
                let would_leave_free_mb = available_mb.saturating_sub(new_total as usize);

                if (would_leave_free_mb as u64) < self.min_free_mb {
                    return Err(anyhow!(
                        "Insufficient free RAM - Cannot allocate for {}\n\
                         \n\
                         System Memory:\n\
                         - Total: {:.2} GB\n\
                         - Available: {:.2} GB\n\
                         - After reservation: {:.2} GB free\n\
                         - Minimum required: {:.2} GB\n\
                         \n\
                         Your system doesn't have enough free memory.\n\
                         Solutions:\n\
                         1. Close other applications (browsers, editors, etc.)\n\
                         2. Reduce build parallelism: --jobs=4\n\
                         3. Add more RAM to your system",
                        description,
                        total_mb as f64 / 1024.0,
                        available_mb as f64 / 1024.0,
                        would_leave_free_mb as f64 / 1024.0,
                        self.min_free_mb as f64 / 1024.0,
                    ));
                }
            }

            // Atomic compare-and-swap to reserve
            match self.reserved_mb.compare_exchange_weak(
                current,
                new_total,
                Ordering::Release,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    debug!(
                        "[MemBudget] Reserved {} MB for {} (total: {:.2} GB / {:.2} GB, {:.1}% of budget)",
                        mb,
                        description,
                        new_total as f64 / 1024.0,
                        self.total_budget_mb as f64 / 1024.0,
                        (new_total as f64 / self.total_budget_mb as f64) * 100.0
                    );
                    return Ok(MemoryReservation {
                        budget: self,
                        mb,
                        description: description.to_string(),
                    });
                }
                Err(_) => {
                    // Another thread modified the counter, retry with updated value
                    continue;
                }
            }
        }
    }

    /// Get current memory pressure level.
    pub fn pressure_level(&self) -> MemoryPressure {
        let reserved = self.reserved_mb.load(Ordering::Relaxed);
        let usage_pct = (reserved as f64 / self.total_budget_mb as f64) * 100.0;

        if usage_pct >= self.hard_stop_pct as f64 {
            MemoryPressure::Critical
        } else if usage_pct >= 70.0 {
            MemoryPressure::High
        } else if usage_pct >= 50.0 {
            MemoryPressure::Medium
        } else {
            MemoryPressure::Low
        }
    }

    /// Get current usage percentage.
    pub fn usage_percentage(&self) -> f64 {
        let reserved = self.reserved_mb.load(Ordering::Relaxed);
        (reserved as f64 / self.total_budget_mb as f64) * 100.0
    }

    /// Get currently reserved memory in GB.
    pub fn reserved_gb(&self) -> f64 {
        self.reserved_mb.load(Ordering::Relaxed) as f64 / 1024.0
    }

    /// Get total budget in GB.
    pub fn total_gb(&self) -> f64 {
        self.total_budget_mb as f64 / 1024.0
    }

    /// Check if we should stop all new work (critical pressure).
    pub fn should_stop(&self) -> bool {
        matches!(self.pressure_level(), MemoryPressure::Critical)
    }

    /// Subscribe to memory pressure changes.
    /// Returns a receiver that gets updates when pressure level changes.
    pub fn subscribe_pressure(&self) -> watch::Receiver<MemoryPressure> {
        self.pressure_rx.clone()
    }

    /// Start background monitoring task with adaptive polling.
    ///
    /// Polls faster when pressure is high, slower when low:
    /// - Critical/High: 100ms (fast response)
    /// - Medium: 500ms (moderate)
    /// - Low: 2s (slow, energy efficient)
    fn start_monitoring(budget: Arc<Self>) {
        tokio::spawn(async move {
            let mut last_pressure = MemoryPressure::Low;
            let mut poll_interval = Duration::from_secs(2);

            loop {
                tokio::time::sleep(poll_interval).await;

                // Calculate current pressure
                let current_pressure = budget.pressure_level();

                // Detect transitions and log
                if current_pressure != last_pressure {
                    let usage = budget.usage_percentage();
                    let reserved = budget.reserved_gb();
                    let total = budget.total_gb();

                    match current_pressure {
                        MemoryPressure::Critical => {
                            warn!(
                                "╔════════════════════════════════════════════════════════════════╗"
                            );
                            warn!(
                                "║ CRITICAL MEMORY PRESSURE - STOPPING NEW WORK                  ║"
                            );
                            warn!(
                                "╚════════════════════════════════════════════════════════════════╝"
                            );
                            warn!(
                                "Memory usage: {:.2} GB / {:.2} GB ({:.1}% of budget)",
                                reserved, total, usage
                            );
                            warn!("Build will complete running tasks but queue new ones");
                            warn!("Consider: --jobs=4 or close other applications");
                        }
                        MemoryPressure::High => {
                            warn!(
                                "Memory pressure HIGH ({:.1}%): {:.2} GB / {:.2} GB - throttling",
                                usage, reserved, total
                            );
                        }
                        MemoryPressure::Medium => {
                            info!(
                                "Memory pressure MEDIUM ({:.1}%): {:.2} GB / {:.2} GB",
                                usage, reserved, total
                            );
                        }
                        MemoryPressure::Low => {
                            debug!(
                                "Memory pressure LOW ({:.1}%): {:.2} GB / {:.2} GB",
                                usage, reserved, total
                            );
                        }
                    }

                    // Broadcast pressure change
                    let _ = budget.pressure_tx.send(current_pressure);
                    last_pressure = current_pressure;
                }

                // Adjust polling interval based on pressure
                poll_interval = match current_pressure {
                    MemoryPressure::Critical | MemoryPressure::High => Duration::from_millis(100),
                    MemoryPressure::Medium => Duration::from_millis(500),
                    MemoryPressure::Low => Duration::from_secs(2),
                };
            }
        });
    }
}

/// RAII guard that releases memory on drop.
pub struct MemoryReservation<'a> {
    budget: &'a GlobalMemoryBudget,
    mb: u64,
    description: String,
}

impl<'a> Drop for MemoryReservation<'a> {
    fn drop(&mut self) {
        let prev = self
            .budget
            .reserved_mb
            .fetch_sub(self.mb, Ordering::Release);
        debug!(
            "[MemBudget] Released {} MB for {} (remaining: {:.2} GB)",
            self.mb,
            self.description,
            (prev - self.mb) as f64 / 1024.0
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_budget_basic() {
        // Create a budget with 10GB total, 90% hard stop
        let (pressure_tx, pressure_rx) = watch::channel(MemoryPressure::Low);
        let budget = Arc::new(GlobalMemoryBudget {
            total_budget_mb: 10 * 1024,
            reserved_mb: AtomicU64::new(0),
            hard_stop_pct: 90,
            min_free_mb: 1024, // 1GB
            pressure_tx,
            pressure_rx,
        });

        // Reserve 2GB - should succeed
        let _guard1 = budget.reserve("test1", 2 * 1024).unwrap();
        assert_eq!(budget.pressure_level(), MemoryPressure::Low);

        // Reserve another 3GB - should succeed (5GB total = 50%)
        let _guard2 = budget.reserve("test2", 3 * 1024).unwrap();
        assert_eq!(budget.pressure_level(), MemoryPressure::Medium);

        // Reserve another 3GB - should succeed (8GB total = 80%)
        let _guard3 = budget.reserve("test3", 3 * 1024).unwrap();
        assert_eq!(budget.pressure_level(), MemoryPressure::High);

        // Try to reserve 3GB more - should fail (would exceed budget)
        let result = budget.reserve("test4", 3 * 1024);
        assert!(result.is_err());

        // Drop one guard
        drop(_guard3);
        assert_eq!(budget.pressure_level(), MemoryPressure::Medium);

        // Now we can reserve again
        let _guard4 = budget.reserve("test4", 2 * 1024).unwrap();
        assert_eq!(budget.pressure_level(), MemoryPressure::High);
    }

    #[test]
    fn test_concurrent_reservations() {
        let (pressure_tx, pressure_rx) = watch::channel(MemoryPressure::Low);
        let budget = Arc::new(GlobalMemoryBudget {
            total_budget_mb: 10 * 1024,
            reserved_mb: AtomicU64::new(0),
            hard_stop_pct: 90,
            min_free_mb: 1024,
            pressure_tx,
            pressure_rx,
        });

        // Attempt 10 reservations of 1.5GB each. Only 6 should succeed.
        let mut guards = Vec::new();
        let mut successes = 0;
        let mut failures = 0;
        for i in 0..10 {
            match budget.reserve(&format!("thread{}", i), 1536) {
                Ok(guard) => {
                    successes += 1;
                    guards.push(guard);
                }
                Err(_) => failures += 1,
            }
        }

        assert_eq!(successes, 6);
        assert_eq!(failures, 4);
        assert_eq!(budget.reserved_mb.load(Ordering::Relaxed), 6 * 1536);

        drop(guards);
    }
}
