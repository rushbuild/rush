// Phase 5.C.4 commit 104: integration tests for the
// cumulative-vs-own cold-load attribution. Lives as
// an integration test (separate crate target) to
// dodge pre-existing test rot in
// crates/rush-starlark/src/{lib,evaluator,builtins}.rs
// — those `#[cfg(test)]` modules don't compile right
// now (28 unrelated type-mismatch errors from earlier
// refactors that changed `get_var()` and `Function`
// closure types). The integration test crate doesn't
// pull in the lib's test target, so the four Pro-
// requested commit-104 tests can run independently.
//
// Once that rot is cleaned up, these can move back
// inline as bzl_loader::tests.
//
// What's tested:
//   1. parent + 1 child → cumulative > own; own =
//      cumulative - child.
//   2. parent + 2 sibling children → siblings SUM into
//      parent's child_total (not double-subtracted,
//      not missed).
//   3. recursive load on same path → no panic, no
//      underflow, own/child marked None for safety.
//   4. data model uses EXPLICIT cumulative-vs-own
//      labels (no bare cold_load_duration field) and
//      satisfies own + child == cumulative when both
//      are present.

use rush_starlark::bzl_loader::{
    pop_cold_load_frame, push_cold_load_frame, ColdLoadSubphases, GlobalBzlCache,
};
use std::path::PathBuf;
use std::time::Duration;

/// Pro test #1: a parent cold-load whose evaluation
/// triggers one child load records cumulative > own,
/// with own = cumulative - child. Exercises the
/// thread-local frame stack push/pop accounting.
#[test]
fn parent_with_child_records_cumulative_greater_than_own() {
    let cache = GlobalBzlCache::with_limits(8, 256 * 1024);

    let parent = PathBuf::from("/parent_104a.bzl");
    let child = PathBuf::from("/child_104a.bzl");

    // Simulate: parent push → child push → child pop
    // (with its cumulative) → record child → parent
    // pop → record parent.
    let parent_rec = push_cold_load_frame(&parent);
    let child_rec = push_cold_load_frame(&child);
    assert!(!parent_rec);
    assert!(!child_rec);

    // Child cumulative = 100 ms.
    let (child_grandchild_total, child_recursion) = pop_cold_load_frame(100_000_000);
    assert_eq!(child_grandchild_total, 0);
    assert!(!child_recursion);
    cache.record_cold_load_full(
        &child,
        Duration::from_millis(100),
        Some(Duration::from_nanos(child_grandchild_total)),
        0,
        1,
        None,
    );

    // Parent cumulative = 500 ms. Frame stack pop
    // returns child_total = 100 ms (the child's
    // cumulative that was bubbled up).
    let (parent_child_total, parent_recursion) = pop_cold_load_frame(500_000_000);
    assert_eq!(parent_child_total, 100_000_000);
    assert!(!parent_recursion);
    cache.record_cold_load_full(
        &parent,
        Duration::from_millis(500),
        Some(Duration::from_nanos(parent_child_total)),
        0,
        1,
        None,
    );

    let stats = cache.top_n_by_cumulative_cold_load_duration(10);
    let p = stats
        .iter()
        .find(|s| s.path == parent)
        .expect("parent recorded");
    assert_eq!(p.cumulative_cold_load_duration_ns, 500_000_000);
    assert_eq!(p.child_load_duration_ns, Some(100_000_000));
    // own = 500 - 100 = 400 ms.
    assert_eq!(p.own_cold_load_duration_ns, Some(400_000_000));
    assert!(
        p.own_cold_load_duration_ns.unwrap() < p.cumulative_cold_load_duration_ns,
        "own must be < cumulative for a parent with children"
    );
}

/// Pro test #2: two sibling child loads under the
/// same parent get SUMMED into parent's child_total
/// (not double-subtracted, not missed). Validates the
/// frame stack's `child_total += popped_cumulative`
/// accumulator semantics.
#[test]
fn sibling_loads_do_not_double_subtract() {
    let cache = GlobalBzlCache::with_limits(8, 256 * 1024);

    let parent = PathBuf::from("/parent_104b.bzl");
    let s1 = PathBuf::from("/s1_104b.bzl");
    let s2 = PathBuf::from("/s2_104b.bzl");

    let _ = push_cold_load_frame(&parent);

    // Sibling 1 cumulative = 80 ms.
    let _ = push_cold_load_frame(&s1);
    let (s1_grand, _) = pop_cold_load_frame(80_000_000);
    cache.record_cold_load_full(
        &s1,
        Duration::from_millis(80),
        Some(Duration::from_nanos(s1_grand)),
        0,
        1,
        None,
    );

    // Sibling 2 cumulative = 120 ms.
    let _ = push_cold_load_frame(&s2);
    let (s2_grand, _) = pop_cold_load_frame(120_000_000);
    cache.record_cold_load_full(
        &s2,
        Duration::from_millis(120),
        Some(Duration::from_nanos(s2_grand)),
        0,
        1,
        None,
    );

    // Parent cumulative = 300 ms. child_total should
    // be 80 + 120 = 200 ms.
    let (parent_child_total, _) = pop_cold_load_frame(300_000_000);
    assert_eq!(
        parent_child_total, 200_000_000,
        "siblings must sum into child_total, not be double-counted or missed"
    );
    cache.record_cold_load_full(
        &parent,
        Duration::from_millis(300),
        Some(Duration::from_nanos(parent_child_total)),
        0,
        1,
        None,
    );

    let stats = cache.top_n_by_cumulative_cold_load_duration(10);
    let p = stats
        .iter()
        .find(|s| s.path == parent)
        .expect("parent recorded");
    assert_eq!(p.cumulative_cold_load_duration_ns, 300_000_000);
    assert_eq!(p.child_load_duration_ns, Some(200_000_000));
    // own = 300 - 200 = 100 ms.
    assert_eq!(p.own_cold_load_duration_ns, Some(100_000_000));
}

/// Pro test #3: a load chain that re-enters the same
/// path doesn't panic, doesn't underflow, and marks
/// own-time as None for safety (Pro: "mark own-time
/// as experimental and only emit it when confident").
#[test]
fn recursive_or_repeated_load_does_not_panic() {
    let cache = GlobalBzlCache::with_limits(8, 256 * 1024);

    let p = PathBuf::from("/recursive_104c.bzl");

    let outer_rec = push_cold_load_frame(&p);
    let inner_rec = push_cold_load_frame(&p);
    assert!(!outer_rec, "first push isn't recursive");
    assert!(inner_rec, "second push of same path is recursive");

    // Inner pop reports recursion_detected = true.
    let (_inner_child, inner_rec_out) = pop_cold_load_frame(50_000_000);
    assert!(inner_rec_out);
    // Inner records with child_duration=None
    // (recursion-detected path).
    cache.record_cold_load_full(&p, Duration::from_millis(50), None, 0, 1, None);

    // Outer pop also reports recursion (propagated up
    // via pop's side effect).
    let (outer_child_total, outer_rec_out) = pop_cold_load_frame(200_000_000);
    assert!(outer_rec_out);
    assert_eq!(outer_child_total, 50_000_000);
    cache.record_cold_load_full(&p, Duration::from_millis(200), None, 0, 1, None);

    let stats = cache.top_n_by_cumulative_cold_load_duration(10);
    let entry = stats
        .iter()
        .find(|s| s.path == p)
        .expect("recursive path recorded");
    // Cumulative accumulated across both record calls.
    assert_eq!(entry.cumulative_cold_load_duration_ns, 250_000_000);
    // own/child stay None — never set during the
    // recursive sequence.
    assert!(
        entry.own_cold_load_duration_ns.is_none(),
        "own-time must be None when recursion was detected"
    );
    assert!(
        entry.child_load_duration_ns.is_none(),
        "child-time must be None when recursion was detected"
    );
}

/// Pro test #4: the per-path stats use EXPLICIT
/// cumulative-vs-own labels — there is no bare
/// "cold_load_duration" in the data model anymore,
/// only `cumulative_cold_load_duration_ns`,
/// `own_cold_load_duration_ns`, and
/// `child_load_duration_ns`. Subphase totals are
/// recorded under their own named fields.
#[test]
fn json_labels_cumulative_vs_own_clearly() {
    let cache = GlobalBzlCache::with_limits(8, 256 * 1024);
    let p = PathBuf::from("/labels_104d.bzl");

    cache.record_cold_load_full(
        &p,
        Duration::from_millis(123),
        Some(Duration::from_millis(45)),
        4096,
        17,
        Some(ColdLoadSubphases {
            read: Some(Duration::from_millis(2)),
            parse: Some(Duration::from_millis(8)),
            eval: Some(Duration::from_millis(100)),
            finalize: Some(Duration::from_millis(13)),
        }),
    );

    let stats = cache.top_n_by_cumulative_cold_load_duration(10);
    let e = stats.iter().find(|s| s.path == p).expect("recorded");

    // Explicit cumulative label.
    assert_eq!(e.cumulative_cold_load_duration_ns, 123_000_000);
    // Explicit own/child labels with Option type.
    assert_eq!(e.own_cold_load_duration_ns, Some(78_000_000));
    assert_eq!(e.child_load_duration_ns, Some(45_000_000));
    // Explicit subphase labels.
    assert_eq!(e.subphase_read_ns, Some(2_000_000));
    assert_eq!(e.subphase_parse_ns, Some(8_000_000));
    assert_eq!(e.subphase_eval_ns, Some(100_000_000));
    assert_eq!(e.subphase_finalize_ns, Some(13_000_000));
    // Verify the explicit identity by construction:
    // own + child == cumulative when both are present.
    let own = e.own_cold_load_duration_ns.unwrap();
    let child = e.child_load_duration_ns.unwrap();
    assert_eq!(
        own + child,
        e.cumulative_cold_load_duration_ns,
        "explicit identity: own + child == cumulative"
    );
}
