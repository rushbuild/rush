//! Rush Build System Profiling Infrastructure
//!
//! This crate provides lightweight profiling for Rush build operations.
//! Enable with --features profile to collect timing data.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use dashmap::DashMap;
use once_cell::sync::Lazy;

/// Global profiling data
static PROFILE_DATA: Lazy<DashMap<&'static str, ProfileEntry>> = Lazy::new(|| DashMap::new());

/// Per-scope profiling statistics
#[derive(Debug)]
struct ProfileEntry {
    /// Total time spent in microseconds
    total_time_us: AtomicU64,
    /// Number of calls
    call_count: AtomicUsize,
}

impl ProfileEntry {
    fn new() -> Self {
        Self {
            total_time_us: AtomicU64::new(0),
            call_count: AtomicUsize::new(0),
        }
    }

    fn add_sample(&self, duration_us: u64) {
        self.total_time_us.fetch_add(duration_us, Ordering::Relaxed);
        self.call_count.fetch_add(1, Ordering::Relaxed);
    }

    fn get_stats(&self) -> (u64, usize) {
        let total = self.total_time_us.load(Ordering::Relaxed);
        let count = self.call_count.load(Ordering::Relaxed);
        (total, count)
    }
}

/// RAII guard that records timing when dropped
pub struct ProfileGuard {
    name: &'static str,
    start: Instant,
}

impl ProfileGuard {
    /// Create a new profiling guard for the given scope name
    #[inline]
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            start: Instant::now(),
        }
    }
}

impl Drop for ProfileGuard {
    #[inline]
    fn drop(&mut self) {
        let elapsed_us = self.start.elapsed().as_micros() as u64;

        // Get or create entry for this scope
        let entry = PROFILE_DATA
            .entry(self.name)
            .or_insert_with(|| ProfileEntry::new());

        entry.add_sample(elapsed_us);
    }
}

/// Print profiling report to stderr
pub fn print_report() {
    if PROFILE_DATA.is_empty() {
        return;
    }

    eprintln!("\n📊 Profile Report:");
    eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

    // Collect and sort by total time
    let mut entries: Vec<_> = PROFILE_DATA
        .iter()
        .map(|entry| {
            let (total_us, count) = entry.value().get_stats();
            (*entry.key(), total_us, count)
        })
        .collect();

    entries.sort_by_key(|(_, total_us, _)| std::cmp::Reverse(*total_us));

    let total_time_us: u64 = entries.iter().map(|(_, total, _)| total).sum();

    for (name, total_us, count) in entries {
        let total_ms = total_us as f64 / 1000.0;
        let avg_us = if count > 0 {
            total_us / count as u64
        } else {
            0
        };
        let avg_ms = avg_us as f64 / 1000.0;
        let percentage = (total_us as f64 / total_time_us as f64) * 100.0;

        eprintln!(
            "   {:40} {:>8.2}s ({:>5.1}%)  {} calls  avg {:>8.2}ms",
            name,
            total_ms / 1000.0,
            percentage,
            count,
            avg_ms
        );
    }

    eprintln!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    eprintln!(
        "   Total profiled time: {:.2}s\n",
        total_time_us as f64 / 1_000_000.0
    );
}

/// Reset all profiling data (useful for testing)
pub fn reset() {
    PROFILE_DATA.clear();
}

/// Macro to create a profiling scope
///
/// Usage:
/// ```ignore
/// use rush_profiling::profile_scope;
///
/// fn my_function() {
///     profile_scope!("my_function");
///     // ... function body ...
/// }
/// ```
#[macro_export]
macro_rules! profile_scope {
    ($name:expr) => {
        #[cfg(feature = "enabled")]
        let _profile_guard = $crate::ProfileGuard::new($name);
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_basic_profiling() {
        reset();

        {
            let _guard = ProfileGuard::new("test_scope");
            thread::sleep(Duration::from_millis(10));
        }

        let entry = PROFILE_DATA.get("test_scope").unwrap();
        let (total_us, count) = entry.get_stats();

        assert_eq!(count, 1);
        assert!(total_us >= 10_000); // At least 10ms
    }

    #[test]
    fn test_multiple_calls() {
        reset();

        for _ in 0..5 {
            let _guard = ProfileGuard::new("multi_scope");
            thread::sleep(Duration::from_millis(1));
        }

        let entry = PROFILE_DATA.get("multi_scope").unwrap();
        let (total_us, count) = entry.get_stats();

        assert_eq!(count, 5);
        assert!(total_us >= 5_000); // At least 5ms total
    }
}
