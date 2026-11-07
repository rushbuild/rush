// Phase 5.C.4 commit 105: integration tests for the
// .bzl load graph + identity diagnostics. Lives as an
// integration test (separate crate target) for the
// same reason as commit-104's tests — to dodge pre-
// existing test rot in the lib's other test modules.
//
// What's tested:
//   1. push_cold_load_frame_ex surfaces the parent
//      path correctly so callers can record load
//      edges.
//   2. record_load_edge populates BOTH direct_children
//      on the parent AND direct_parents on the child.
//   3. record_identity collapses multiple raw labels
//      onto a single resolved path AND surfaces them
//      via identity_collisions().
//   4. record_observed_depth keeps the max depth.
//   5. top_n_by_fanout sorts by direct_children_count.
//   6. top_n_by_reverse_dependents sorts by
//      direct_parents_count.

use rush_starlark::bzl_loader::{
    pop_cold_load_frame, push_cold_load_frame_ex, GlobalBzlCache,
};
use std::path::PathBuf;

/// push_ex returns the parent path of the new frame
/// (the previous top of stack, or None for the root
/// push) and the post-push depth.
#[test]
fn push_ex_surfaces_parent_and_depth() {
    let parent = PathBuf::from("/parent_105a.bzl");
    let child = PathBuf::from("/child_105a.bzl");

    let p = push_cold_load_frame_ex(&parent);
    assert_eq!(p.parent_path, None, "root push has no parent");
    assert_eq!(p.depth_after_push, 1);
    assert!(!p.recursive);

    let c = push_cold_load_frame_ex(&child);
    assert_eq!(c.parent_path.as_deref(), Some(parent.as_path()));
    assert_eq!(c.depth_after_push, 2);
    assert!(!c.recursive);

    // Clean up the stack so we don't pollute other
    // tests if cargo decides to share a thread.
    let _ = pop_cold_load_frame(0);
    let _ = pop_cold_load_frame(0);
}

/// record_load_edge populates BOTH sides of the edge
/// — the parent's direct_children grows by one AND the
/// child's direct_parents grows by one.
#[test]
fn record_load_edge_populates_both_sides() {
    let cache = GlobalBzlCache::with_limits(8, 256 * 1024);
    let p = PathBuf::from("/p_105b.bzl");
    let c1 = PathBuf::from("/c1_105b.bzl");
    let c2 = PathBuf::from("/c2_105b.bzl");

    cache.record_load_edge(&p, &c1);
    cache.record_load_edge(&p, &c2);
    // Duplicate edge — should NOT inflate counts.
    cache.record_load_edge(&p, &c1);

    let fanout = cache.top_n_by_fanout(10);
    let parent_stat = fanout.iter().find(|s| s.path == p).expect("parent present");
    assert_eq!(parent_stat.direct_children_count, 2, "deduped to 2 children");

    let rdeps = cache.top_n_by_reverse_dependents(10);
    let c1_stat = rdeps.iter().find(|s| s.path == c1).expect("c1 present");
    assert_eq!(c1_stat.direct_parents_count, 1);
    let c2_stat = rdeps.iter().find(|s| s.path == c2).expect("c2 present");
    assert_eq!(c2_stat.direct_parents_count, 1);
}

/// record_identity collapses multiple raw labels onto
/// a single resolved path AND surfaces them via
/// identity_collisions().
#[test]
fn identity_collisions_surfaces_multi_label_paths() {
    let cache = GlobalBzlCache::with_limits(8, 256 * 1024);
    let single_id = PathBuf::from("/single_id_105c.bzl");
    let multi_id = PathBuf::from("/multi_id_105c.bzl");

    cache.record_identity("//x:single_id.bzl", &single_id);
    cache.record_identity("//x:multi_id.bzl", &multi_id);
    cache.record_identity("@repo//x:multi_id.bzl", &multi_id);
    cache.record_identity(":multi_id.bzl", &multi_id);

    let collisions = cache.identity_collisions();
    assert_eq!(collisions.len(), 1, "only one path has >1 identity");
    assert_eq!(collisions[0].path, multi_id);
    assert_eq!(collisions[0].raw_identities.len(), 3);
}

/// record_observed_depth keeps the MAX across
/// observations.
#[test]
fn record_observed_depth_keeps_max() {
    let cache = GlobalBzlCache::with_limits(8, 256 * 1024);
    let p = PathBuf::from("/depth_105d.bzl");

    cache.record_observed_depth(&p, 3);
    cache.record_observed_depth(&p, 7);
    cache.record_observed_depth(&p, 2);

    // Surface via any accessor — fanout works because
    // record_observed_depth populated the entry.
    let stats = cache.top_n_by_fanout(10);
    let s = stats.iter().find(|s| s.path == p).expect("recorded");
    assert_eq!(s.max_observed_depth, 7, "max wins, not last");
}

/// top_n_by_fanout sorts by direct_children_count
/// descending; ties broken by path lexicographic.
#[test]
fn top_n_by_fanout_sorts_correctly() {
    let cache = GlobalBzlCache::with_limits(8, 256 * 1024);
    let a = PathBuf::from("/a_105e.bzl");
    let b = PathBuf::from("/b_105e.bzl");
    // a has 3 children, b has 1.
    cache.record_load_edge(&a, &PathBuf::from("/ca1.bzl"));
    cache.record_load_edge(&a, &PathBuf::from("/ca2.bzl"));
    cache.record_load_edge(&a, &PathBuf::from("/ca3.bzl"));
    cache.record_load_edge(&b, &PathBuf::from("/cb1.bzl"));

    let top = cache.top_n_by_fanout(10);
    let a_idx = top.iter().position(|s| s.path == a).expect("a present");
    let b_idx = top.iter().position(|s| s.path == b).expect("b present");
    assert!(a_idx < b_idx, "a (3 children) must rank above b (1 child)");
    assert_eq!(top[a_idx].direct_children_count, 3);
    assert_eq!(top[b_idx].direct_children_count, 1);
}

/// top_n_by_reverse_dependents sorts by
/// direct_parents_count descending.
#[test]
fn top_n_by_reverse_dependents_sorts_correctly() {
    let cache = GlobalBzlCache::with_limits(8, 256 * 1024);
    let popular = PathBuf::from("/popular_105f.bzl");
    let lonely = PathBuf::from("/lonely_105f.bzl");
    // popular is loaded by 3 parents; lonely by 1.
    cache.record_load_edge(&PathBuf::from("/p1.bzl"), &popular);
    cache.record_load_edge(&PathBuf::from("/p2.bzl"), &popular);
    cache.record_load_edge(&PathBuf::from("/p3.bzl"), &popular);
    cache.record_load_edge(&PathBuf::from("/p4.bzl"), &lonely);

    let top = cache.top_n_by_reverse_dependents(10);
    let pop_idx = top
        .iter()
        .position(|s| s.path == popular)
        .expect("popular present");
    let lon_idx = top
        .iter()
        .position(|s| s.path == lonely)
        .expect("lonely present");
    assert!(
        pop_idx < lon_idx,
        "popular (3 parents) must rank above lonely (1 parent)"
    );
    assert_eq!(top[pop_idx].direct_parents_count, 3);
    assert_eq!(top[lon_idx].direct_parents_count, 1);
}
