// Phase 5.C.4 commit 110: integration tests for cross-
// thread timing attribution. Lives as an integration
// test (separate crate target) for the same reason as
// commits 104/105/106's tests — to dodge the
// not-quite-resolved pre-existing test rot in the
// lib's other test modules (some behavioral failures
// remain post-bcc5ee6).
//
// Pro's directive: "Do not emit a precise-looking
// own-time when the accounting is known incomplete.
// Fake precision is how dashboards become horoscopes
// with JSON."
//
// What's tested (Pro's 4 required tests):
//   1. same_thread_child_load_has_own_and_child_time
//   2. cross_thread_child_load_marks_attribution
//   3. cross_thread_child_load_does_not_emit_fake_own_time
//   4. json_includes_attribution
//
// All four are exercised via the public bzl_loader API
// (push/pop frame, record_cold_load_full_v2) without
// needing real .bzl file evaluation.

use rush_starlark::bzl_loader::{
    mark_active_frame_spawned_off_thread, pop_cold_load_frame_ex,
    push_cold_load_frame_ex, BzlTimingAttribution, GlobalBzlCache,
};
use std::path::PathBuf;
use std::time::Duration;

/// Pro test #1: same-thread parent+child pop yields
/// SameThreadComplete attribution AND emits both
/// own_duration and child_duration as Some.
#[test]
fn same_thread_child_load_has_own_and_child_time() {
    let cache = GlobalBzlCache::with_limits(8, 256 * 1024);
    let parent = PathBuf::from("/parent_110a.bzl");
    let child = PathBuf::from("/child_110a.bzl");

    let _ = push_cold_load_frame_ex(&parent);
    let _ = push_cold_load_frame_ex(&child);
    // Child cumulative = 100 ms.
    let child_pop = pop_cold_load_frame_ex(100_000_000);
    assert_eq!(
        child_pop.attribution,
        BzlTimingAttribution::SameThreadComplete,
        "child without children → SameThreadComplete"
    );
    cache.record_cold_load_full_v2(
        &child,
        Duration::from_millis(100),
        Some(Duration::from_nanos(child_pop.child_total_ns)),
        0,
        1,
        None,
        child_pop.attribution,
    );

    // Parent pop bubbles up child's cumulative.
    let parent_pop = pop_cold_load_frame_ex(500_000_000);
    assert_eq!(
        parent_pop.attribution,
        BzlTimingAttribution::SameThreadComplete,
        "parent whose child finished same-thread → SameThreadComplete"
    );
    cache.record_cold_load_full_v2(
        &parent,
        Duration::from_millis(500),
        Some(Duration::from_nanos(parent_pop.child_total_ns)),
        0,
        1,
        None,
        parent_pop.attribution,
    );

    let stats = cache.top_n_by_cumulative_cold_load_duration(10);
    let p = stats.iter().find(|s| s.path == parent).expect("recorded");
    assert_eq!(
        p.latest_attribution,
        BzlTimingAttribution::SameThreadComplete
    );
    assert_eq!(p.own_cold_load_duration_ns, Some(400_000_000));
    assert_eq!(p.child_load_duration_ns, Some(100_000_000));
}

/// Pro test #2: when the parent marks itself as
/// spawn-off-thread, the pop reports
/// CrossThreadChildrenObserved attribution.
#[test]
fn cross_thread_child_load_marks_attribution() {
    let cache = GlobalBzlCache::with_limits(8, 256 * 1024);
    let parent = PathBuf::from("/parent_110b.bzl");

    let _ = push_cold_load_frame_ex(&parent);
    // Simulate the non-tokio `thread::spawn` path
    // marking its own frame.
    mark_active_frame_spawned_off_thread();
    // Eval ran on another thread; our local
    // child_total_ns stays 0 (children pushed onto
    // the spawned thread's stack).
    let pop = pop_cold_load_frame_ex(900_000_000);
    assert_eq!(
        pop.attribution,
        BzlTimingAttribution::CrossThreadChildrenObserved,
        "spawn-off-thread parent → CrossThreadChildrenObserved"
    );
    cache.record_cold_load_full_v2(
        &parent,
        Duration::from_millis(900),
        None, // child_duration unreliable
        0,
        1,
        None,
        pop.attribution,
    );

    let stats = cache.top_n_by_cumulative_cold_load_duration(10);
    let p = stats.iter().find(|s| s.path == parent).expect("recorded");
    assert_eq!(
        p.latest_attribution,
        BzlTimingAttribution::CrossThreadChildrenObserved
    );
}

/// Pro test #3: when attribution is
/// CrossThreadChildrenObserved, the recorder MUST NOT
/// emit a precise-looking own_duration. Convention:
/// caller passes child_duration=None, which leaves
/// own_cold_load_duration_ns = None permanently for
/// that load.
#[test]
fn cross_thread_child_load_does_not_emit_fake_own_time() {
    let cache = GlobalBzlCache::with_limits(8, 256 * 1024);
    let parent = PathBuf::from("/parent_110c.bzl");

    let _ = push_cold_load_frame_ex(&parent);
    mark_active_frame_spawned_off_thread();
    let pop = pop_cold_load_frame_ex(1_500_000_000);
    assert_eq!(
        pop.attribution,
        BzlTimingAttribution::CrossThreadChildrenObserved
    );

    // Caller honors the contract: child_duration=None
    // when attribution is unreliable.
    cache.record_cold_load_full_v2(
        &parent,
        Duration::from_millis(1500),
        None,
        0,
        1,
        None,
        pop.attribution,
    );

    let stats = cache.top_n_by_cumulative_cold_load_duration(10);
    let p = stats.iter().find(|s| s.path == parent).expect("recorded");
    // own/child must both be None — no fake own-time.
    assert!(
        p.own_cold_load_duration_ns.is_none(),
        "own_cold_load_duration_ns must be None for CrossThreadChildrenObserved"
    );
    assert!(
        p.child_load_duration_ns.is_none(),
        "child_load_duration_ns must be None for CrossThreadChildrenObserved"
    );
    // Cumulative is still recorded — that part IS
    // reliable.
    assert_eq!(p.cumulative_cold_load_duration_ns, 1_500_000_000);
}

/// Pro test #4: the cache's `latest_attribution`
/// distinguishes all four documented states. Verified
/// directly on PerPathStats. The wire-shape JSON test
/// (orchestrator BzlColdLoadEntry with `attribution`
/// string) lives in rush-build's cache_counters unit
/// tests (see `json_attribution_field_serializes`)
/// because rush-build isn't a dev-dep of rush-starlark.
#[test]
fn cache_records_all_four_attribution_states() {
    let cache = GlobalBzlCache::with_limits(8, 256 * 1024);

    let p_same = PathBuf::from("/same_110d.bzl");
    let p_cross = PathBuf::from("/cross_110d.bzl");
    let p_recur = PathBuf::from("/recur_110d.bzl");
    let p_unknown = PathBuf::from("/unknown_110d.bzl");

    cache.record_cold_load_full_v2(
        &p_same,
        Duration::from_millis(10),
        Some(Duration::from_millis(2)),
        0,
        1,
        None,
        BzlTimingAttribution::SameThreadComplete,
    );
    cache.record_cold_load_full_v2(
        &p_cross,
        Duration::from_millis(20),
        None,
        0,
        1,
        None,
        BzlTimingAttribution::CrossThreadChildrenObserved,
    );
    cache.record_cold_load_full_v2(
        &p_recur,
        Duration::from_millis(30),
        None,
        0,
        1,
        None,
        BzlTimingAttribution::RecursionSuppressed,
    );
    // The legacy record_cold_load shim sets Unknown.
    cache.record_cold_load(&p_unknown, Duration::from_millis(40), 0, 1);

    let stats = cache.top_n_by_cumulative_cold_load_duration(10);
    let find = |p: &PathBuf| stats.iter().find(|s| s.path == *p).cloned().unwrap();
    assert_eq!(
        find(&p_same).latest_attribution,
        BzlTimingAttribution::SameThreadComplete
    );
    assert_eq!(
        find(&p_cross).latest_attribution,
        BzlTimingAttribution::CrossThreadChildrenObserved
    );
    assert_eq!(
        find(&p_recur).latest_attribution,
        BzlTimingAttribution::RecursionSuppressed
    );
    assert_eq!(
        find(&p_unknown).latest_attribution,
        BzlTimingAttribution::Unknown
    );
}
