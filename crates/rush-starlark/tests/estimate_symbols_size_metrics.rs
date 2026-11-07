// Phase 5.C.4 commit 118: integration tests for the
// estimate_symbols_size metrics added in commit 118.
// These metrics give us a before/after line item for
// the commit-120 reuse optimization (without them the
// next optimization becomes "seemed faster" — Pro:
// "we are not running a vibes laboratory").
//
// Tests required by Pro's commit-118 directive:
//   1. estimate_call_counter_increments
//   2. estimate_duration_recorded
//   3. per_bzl_estimate_metrics_are_deterministic

use rush_starlark::bzl_loader::GlobalBzlCache;
use rush_starlark::evaluator::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn fake_symbols(n: usize) -> HashMap<String, Value> {
    (0..n)
        .map(|i| (format!("sym{}", i), Value::Int(i as i64)))
        .collect()
}

/// Pro test #1: each call to the cache's
/// `put_symbols` increments
/// `estimate_symbols_size_calls`.
#[test]
fn estimate_call_counter_increments() {
    let cache = GlobalBzlCache::with_limits(32, 256 * 1024 * 1024);
    let path_a = PathBuf::from("/test_call_count_a.bzl");
    let path_b = PathBuf::from("/test_call_count_b.bzl");

    let stats0 = cache.cache_stats();
    assert_eq!(stats0.estimate_symbols_size_calls, 0);

    // First insert triggers a fresh estimate.
    cache.put_symbols_public(path_a.clone(), fake_symbols(5));
    let stats1 = cache.cache_stats();
    assert_eq!(stats1.estimate_symbols_size_calls, 1);

    // Second distinct path triggers another fresh
    // estimate.
    cache.put_symbols_public(path_b.clone(), fake_symbols(10));
    let stats2 = cache.cache_stats();
    assert_eq!(stats2.estimate_symbols_size_calls, 2);

    // Re-insert path_a (repeat-miss reload) triggers
    // another estimate (commit 118 always recomputes
    // — commit 120 will add the reuse path).
    cache.put_symbols_public(path_a, fake_symbols(5));
    let stats3 = cache.cache_stats();
    assert_eq!(stats3.estimate_symbols_size_calls, 3);
    // The repeat-miss is now a candidate.
    assert_eq!(stats3.repeat_reload_estimate_candidates, 1);
}

/// Pro test #2: total_duration accumulator is
/// non-zero after at least one call.
#[test]
fn estimate_duration_recorded() {
    let cache = GlobalBzlCache::with_limits(32, 256 * 1024 * 1024);
    let path = PathBuf::from("/test_duration.bzl");

    cache.put_symbols_public(path, fake_symbols(20));
    let stats = cache.cache_stats();

    assert_eq!(stats.estimate_symbols_size_calls, 1);
    // The walk is fast but non-zero — even on the
    // most overclocked desktop, walking 20 Int values
    // takes at least a few hundred nanoseconds.
    assert!(
        stats.estimate_symbols_size_total_duration_ns > 0,
        "duration must be measurable: {} ns",
        stats.estimate_symbols_size_total_duration_ns
    );
}

/// Pro test #3: per-bzl metrics report the right
/// path attribution. Same path inserted twice
/// records 2 calls AGAINST that path; different
/// paths get separate accumulators.
#[test]
fn per_bzl_estimate_metrics_are_deterministic() {
    let cache = GlobalBzlCache::with_limits(32, 256 * 1024 * 1024);
    let p1 = PathBuf::from("/test_per_bzl_1.bzl");
    let p2 = PathBuf::from("/test_per_bzl_2.bzl");

    cache.put_symbols_public(p1.clone(), fake_symbols(3));
    cache.put_symbols_public(p1.clone(), fake_symbols(3));
    cache.put_symbols_public(p2.clone(), fake_symbols(5));

    // top_n_by_lookups returns per-path stats; we
    // filter to find ours specifically.
    let stats = cache.top_n_by_cumulative_cold_load_duration(usize::MAX);
    let find = |p: &Path| -> rush_starlark::bzl_loader::PerPathStats {
        stats
            .iter()
            .find(|s| s.path == p)
            .cloned()
            .unwrap_or_else(|| panic!("path not found: {}", p.display()))
    };

    // path1 inserted twice → 2 estimate calls.
    let s1 = find(&p1);
    assert_eq!(s1.estimate_symbols_size_calls, 2);
    assert_eq!(s1.estimated_bytes > 0, true);
    // path2 inserted once → 1 estimate call.
    let s2 = find(&p2);
    assert_eq!(s2.estimate_symbols_size_calls, 1);

    // Global counter equals 1 + 2 = 3.
    let global = cache.cache_stats();
    assert_eq!(global.estimate_symbols_size_calls, 3);
}
