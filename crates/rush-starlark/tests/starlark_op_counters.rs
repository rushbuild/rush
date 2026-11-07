// Phase 5.C.4 commit 106: integration tests for the
// coarse Starlark-op counters. Lives as an
// integration test (separate crate target) for the
// same reason as commit-104/105's tests.
//
// What's tested:
//   1. record_starlark_op bumps only when a cold-load
//      frame is on the stack (no-op otherwise).
//   2. pop_cold_load_frame_ex returns the accumulated
//      ops from the popped frame.
//   3. OpCounts::total() sums all five categories.
//   4. GlobalBzlCache.record_starlark_ops folds ops
//      into per-path metrics; multiple cold loads of
//      the same path accumulate.
//   5. top_n_by_starlark_ops sorts by total volume.
//   6. ops attribute to the TOP frame (parent), not
//      a nested child frame.

use rush_starlark::bzl_loader::{
    pop_cold_load_frame_ex, push_cold_load_frame, record_starlark_op,
    GlobalBzlCache, OpCounts, StarlarkOpKind,
};
use std::path::PathBuf;

#[test]
fn record_op_is_noop_without_active_frame() {
    // Pre-condition: no frames active for this thread
    // (cargo gives integration tests fresh threads, so
    // this is a safe assumption at test entry).
    let cache = GlobalBzlCache::with_limits(8, 256 * 1024);
    let p = PathBuf::from("/noop_106a.bzl");

    record_starlark_op(StarlarkOpKind::FunctionCall);
    record_starlark_op(StarlarkOpKind::BuiltinCall);
    record_starlark_op(StarlarkOpKind::GlobCall);

    // No frame ⇒ nothing recorded ⇒ no per-path entry.
    let top = cache.top_n_by_starlark_ops(10);
    assert!(top.iter().all(|s| s.path != p));
}

#[test]
fn pop_ex_returns_accumulated_ops() {
    let cache = GlobalBzlCache::with_limits(8, 256 * 1024);
    let p = PathBuf::from("/pop_ex_106b.bzl");

    let _ = push_cold_load_frame(&p);
    record_starlark_op(StarlarkOpKind::FunctionCall);
    record_starlark_op(StarlarkOpKind::FunctionCall);
    record_starlark_op(StarlarkOpKind::BuiltinCall);
    record_starlark_op(StarlarkOpKind::RuleInvocation);
    record_starlark_op(StarlarkOpKind::MacroInvocation);
    record_starlark_op(StarlarkOpKind::GlobCall);
    record_starlark_op(StarlarkOpKind::GlobCall);

    let pop = pop_cold_load_frame_ex(0);
    assert_eq!(pop.ops.function_calls, 2);
    assert_eq!(pop.ops.builtin_calls, 1);
    assert_eq!(pop.ops.rule_invocations, 1);
    assert_eq!(pop.ops.macro_invocations, 1);
    assert_eq!(pop.ops.glob_calls, 2);
    assert_eq!(pop.ops.total(), 7);

    cache.record_starlark_ops(&p, pop.ops);
    let top = cache.top_n_by_starlark_ops(10);
    let s = top.iter().find(|s| s.path == p).expect("recorded");
    assert_eq!(s.starlark_ops.total(), 7);
}

#[test]
fn opcounts_total_sums_all_categories() {
    let mut oc = OpCounts::default();
    oc.function_calls = 10;
    oc.builtin_calls = 20;
    oc.rule_invocations = 5;
    oc.macro_invocations = 3;
    oc.glob_calls = 12;
    assert_eq!(oc.total(), 50);
}

#[test]
fn record_starlark_ops_accumulates_across_cold_loads() {
    let cache = GlobalBzlCache::with_limits(8, 256 * 1024);
    let p = PathBuf::from("/accumulate_106c.bzl");

    // Two synthetic cold loads of the same path —
    // ops should ADD, not overwrite.
    cache.record_starlark_ops(
        &p,
        OpCounts {
            function_calls: 10,
            builtin_calls: 5,
            rule_invocations: 1,
            macro_invocations: 0,
            glob_calls: 0,
        },
    );
    cache.record_starlark_ops(
        &p,
        OpCounts {
            function_calls: 3,
            builtin_calls: 2,
            rule_invocations: 0,
            macro_invocations: 1,
            glob_calls: 4,
        },
    );

    let top = cache.top_n_by_starlark_ops(10);
    let s = top.iter().find(|s| s.path == p).expect("recorded");
    assert_eq!(s.starlark_ops.function_calls, 13);
    assert_eq!(s.starlark_ops.builtin_calls, 7);
    assert_eq!(s.starlark_ops.rule_invocations, 1);
    assert_eq!(s.starlark_ops.macro_invocations, 1);
    assert_eq!(s.starlark_ops.glob_calls, 4);
    assert_eq!(s.starlark_ops.total(), 26);
}

#[test]
fn top_n_by_starlark_ops_sorts_by_total() {
    let cache = GlobalBzlCache::with_limits(8, 256 * 1024);
    let high = PathBuf::from("/high_106d.bzl");
    let low = PathBuf::from("/low_106d.bzl");

    cache.record_starlark_ops(
        &high,
        OpCounts {
            function_calls: 100,
            ..Default::default()
        },
    );
    cache.record_starlark_ops(
        &low,
        OpCounts {
            function_calls: 10,
            ..Default::default()
        },
    );

    let top = cache.top_n_by_starlark_ops(10);
    let high_idx = top.iter().position(|s| s.path == high).expect("high present");
    let low_idx = top.iter().position(|s| s.path == low).expect("low present");
    assert!(high_idx < low_idx, "high (100 ops) ranks above low (10 ops)");
}

#[test]
fn ops_attribute_to_top_frame() {
    let cache = GlobalBzlCache::with_limits(8, 256 * 1024);
    let parent = PathBuf::from("/parent_106e.bzl");
    let child = PathBuf::from("/child_106e.bzl");

    // Parent frame on stack, do some ops attributed
    // to parent.
    let _ = push_cold_load_frame(&parent);
    record_starlark_op(StarlarkOpKind::FunctionCall);
    record_starlark_op(StarlarkOpKind::FunctionCall);

    // Now push child — ops while child is on top
    // attribute to CHILD.
    let _ = push_cold_load_frame(&child);
    record_starlark_op(StarlarkOpKind::BuiltinCall);
    record_starlark_op(StarlarkOpKind::BuiltinCall);
    record_starlark_op(StarlarkOpKind::BuiltinCall);
    let child_pop = pop_cold_load_frame_ex(0);
    cache.record_starlark_ops(&child, child_pop.ops);

    // Pop back to parent; one more op should
    // attribute to parent.
    record_starlark_op(StarlarkOpKind::RuleInvocation);
    let parent_pop = pop_cold_load_frame_ex(0);
    cache.record_starlark_ops(&parent, parent_pop.ops);

    // Verify attribution.
    let top = cache.top_n_by_starlark_ops(10);
    let p = top.iter().find(|s| s.path == parent).expect("parent");
    let c = top.iter().find(|s| s.path == child).expect("child");
    assert_eq!(p.starlark_ops.function_calls, 2, "parent's pre-child ops");
    assert_eq!(p.starlark_ops.rule_invocations, 1, "parent's post-child op");
    assert_eq!(p.starlark_ops.builtin_calls, 0, "no builtin counted on parent");
    assert_eq!(c.starlark_ops.builtin_calls, 3, "all 3 builtins on child");
    assert_eq!(c.starlark_ops.function_calls, 0, "no function counted on child");
}
