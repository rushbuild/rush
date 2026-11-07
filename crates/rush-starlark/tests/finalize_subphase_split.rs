// Phase 5.C.4 commit 111: integration tests for the
// finalize-subphase split (resolve_lazy_loads vs
// put_symbols_cache). Pro's directive: "Split finalize
// into at least: resolve_lazy_loads_duration,
// put_symbols_cache_duration, other_finalize_duration."
//
// This commit is the data checkpoint before any
// optimization (Pro: "It tells us which Tier 1
// candidate deserves code. No optimization yet.").
//
// Tests required by Pro:
//   1. finalize_subphases_sum_to_finalize_or_are_bounded
//   2. no_lazy_loads_case_records_resolve_lazy_loads
//   3. symbol_cache_case_records_put_symbols_cache

use rush_starlark::bzl_loader::{
    BzlTimingAttribution, ColdLoadSubphases, GlobalBzlCache,
};
use std::path::PathBuf;
use std::time::Duration;

/// Pro test #1 from 111: the resolve_lazy_loads +
/// put_symbols_cache durations sum to a value bounded
/// above by the total finalize duration (allowing for
/// a small "other" gap for inter-step work like the
/// loader-lock acquisition).
#[test]
fn finalize_subphases_sum_to_finalize_or_are_bounded() {
    let cache = GlobalBzlCache::with_limits(8, 256 * 1024);
    let p = PathBuf::from("/finalize_split_111a.bzl");

    cache.record_cold_load_full_v2(
        &p,
        Duration::from_millis(200), // cumulative
        Some(Duration::from_millis(50)),
        0,
        1,
        Some(ColdLoadSubphases {
            read: Some(Duration::from_millis(1)),
            parse: Some(Duration::from_millis(5)),
            eval: Some(Duration::from_millis(80)),
            finalize: Some(Duration::from_millis(60)),
            resolve_lazy_loads: Some(Duration::from_millis(40)),
            put_symbols_cache: Some(Duration::from_millis(15)),
        }),
        BzlTimingAttribution::SameThreadComplete,
    );

    let stats = cache.top_n_by_cumulative_cold_load_duration(10);
    let s = stats.iter().find(|s| s.path == p).expect("recorded");
    let finalize = s.subphase_finalize_ns.unwrap();
    let r = s.subphase_resolve_lazy_loads_ns.unwrap();
    let c = s.subphase_put_symbols_cache_ns.unwrap();
    assert_eq!(r + c, 55_000_000);
    assert!(
        r + c <= finalize,
        "resolve + put must not exceed finalize total: r+c={} finalize={}",
        r + c,
        finalize
    );
}

/// Pro test #2 from 111: even when there are no lazy
/// loads to resolve, `resolve_lazy_loads` is still
/// timed and recorded (a near-zero value, not None).
/// This is the "fast-path detection precursor" — we
/// need data showing what near-zero looks like before
/// commit 112 introduces the actual skip.
#[test]
fn no_lazy_loads_case_records_resolve_lazy_loads() {
    let cache = GlobalBzlCache::with_limits(8, 256 * 1024);
    let p = PathBuf::from("/no_lazy_loads_111b.bzl");

    // Simulate a load where resolve_lazy_loads ran
    // but found nothing — small duration, NOT None.
    cache.record_cold_load_full_v2(
        &p,
        Duration::from_millis(100),
        Some(Duration::from_millis(20)),
        0,
        1,
        Some(ColdLoadSubphases {
            read: Some(Duration::from_micros(500)),
            parse: Some(Duration::from_millis(2)),
            eval: Some(Duration::from_millis(40)),
            finalize: Some(Duration::from_millis(35)),
            resolve_lazy_loads: Some(Duration::from_micros(50)),
            put_symbols_cache: Some(Duration::from_millis(30)),
        }),
        BzlTimingAttribution::SameThreadComplete,
    );

    let stats = cache.top_n_by_cumulative_cold_load_duration(10);
    let s = stats.iter().find(|s| s.path == p).expect("recorded");
    assert_eq!(
        s.subphase_resolve_lazy_loads_ns,
        Some(50_000),
        "resolve_lazy_loads should be recorded even when there's nothing to resolve"
    );
    assert!(
        s.subphase_resolve_lazy_loads_ns.unwrap()
            < s.subphase_put_symbols_cache_ns.unwrap(),
        "no-lazy-load case should have resolve < put_symbols_cache"
    );
}

/// Pro test #3 from 111: when only the cache-insert
/// step runs (e.g. the load_bzl entry point that
/// doesn't call resolve_lazy_loads), put_symbols_cache
/// is recorded with a meaningful value AND
/// resolve_lazy_loads is None (not zero — explicitly
/// absent).
#[test]
fn symbol_cache_case_records_put_symbols_cache() {
    let cache = GlobalBzlCache::with_limits(8, 256 * 1024);
    let p = PathBuf::from("/symbol_cache_only_111c.bzl");

    // Simulate the load_bzl path: resolve_lazy_loads
    // = None, put_symbols_cache = Some(_), finalize
    // total matches put_symbols_cache.
    cache.record_cold_load_full_v2(
        &p,
        Duration::from_millis(80),
        Some(Duration::from_millis(10)),
        0,
        1,
        Some(ColdLoadSubphases {
            read: Some(Duration::from_micros(300)),
            parse: Some(Duration::from_millis(1)),
            eval: Some(Duration::from_millis(50)),
            finalize: Some(Duration::from_millis(18)),
            resolve_lazy_loads: None,
            put_symbols_cache: Some(Duration::from_millis(18)),
        }),
        BzlTimingAttribution::SameThreadComplete,
    );

    let stats = cache.top_n_by_cumulative_cold_load_duration(10);
    let s = stats.iter().find(|s| s.path == p).expect("recorded");
    assert!(
        s.subphase_resolve_lazy_loads_ns.is_none(),
        "load_bzl entry point should leave resolve_lazy_loads = None"
    );
    assert_eq!(
        s.subphase_put_symbols_cache_ns,
        Some(18_000_000),
        "put_symbols_cache should be recorded"
    );
    assert_eq!(
        s.subphase_finalize_ns,
        Some(18_000_000),
        "finalize total matches put_symbols_cache when no resolve step"
    );
}
