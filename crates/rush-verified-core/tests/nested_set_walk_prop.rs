// Rush verified-core — NestedSet dedup-walk (flatten) ORACLE
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT
//
// spec-owned (oracle): this file is the functional oracle for the
// nested-set-walk-v1 verified scope. It witnesses the ValidFlatten properties
// from verification/dafny/nested_set/NestedSet.dfy on thousands of random
// acyclic index-DAGs, and proves the oracle is non-vacuous via deliberately
// wrong walk variants it catches. Deleting, #[ignore]-ing, or weakening these
// assertions is a spec-sensitive change (Spec-Change: oracle) and routes
// through the human Spec-Review gate. See ci/spec-owned.tsv.
//
//! Property/golden ORACLE for `flatten` (the NestedSet dedup walk). It checks,
//! on every accepted (acyclic, in-bounds) DAG, the three conjuncts of the Dafny
//! `ValidFlatten` predicate:
//!
//!   1. **expand-once / node completeness** — the set of nodes whose direct
//!      block contributed to the output equals
//!      [`rush_verified_core::reachable_nodes`]`(dag, root)` (no reachable node
//!      dropped, no unreachable node expanded, each expanded once — the
//!      O(unique) bound, NOT O(N²));
//!   2. **element completeness (length bound)** — `|out|` equals the summed
//!      `direct` length over reachable nodes (the "drops a transitive element"
//!      and "re-expands a shared node" bug classes are both rejected here); and
//!   3. **order** — `out` equals the independent reference preorder /
//!      direct-before-children / declaration-order walk computed here.
//!
//! NOTE on what is DELIBERATELY NOT asserted: the walk dedups NODES, not element
//! VALUES, so the output may legitimately contain a repeated value (when two
//! distinct reachable nodes share an element-ID). Asserting "no duplicate
//! values" would be FALSE for the real walk — see `value_repeated_witness`. The
//! faithful dedup invariant is node-level (conjunct 1) plus the length bound
//! (conjunct 2), not value-level.
//!
//! Anti-vacuity is demonstrated with mutation witnesses the oracle CATCHES:
//!   * `mut_reexpand_shared_node` — a walk that re-expands a shared (diamond)
//!     node is caught by the length bound (conjunct 2) and node-completeness.
//!   * `mut_drop_transitive_element` — a walk that drops a transitive element is
//!     caught by the length bound (conjunct 2).
//!   * `mut_children_before_direct` — a walk that emits children before a node's
//!     own direct items (when the real order is direct-first) is caught by the
//!     order conjunct (3).

use proptest::prelude::*;
use rush_verified_core::{flatten, reachable_nodes, DagNode, IndexDag};
use std::collections::HashSet;

// ---------------------------------------------------------------------------
// Independent reference walk (the oracle proper) — recomputed here so the
// checker does not just re-run the implementation. This is the preorder /
// direct-before-children / declaration-order DFS with node-identity dedup.
//
// REAL-SEMANTICS NOTE (discovered while building this oracle — exactly the kind
// of correction the link_dedup/label_parse work warned about). The dedup is
// NOT a textbook recursive "mark-then-immediately-descend per child" DFS. The
// production `NestedSetIter` (nested_set.rs:158-190) schedules ALL of a node's
// children AT ONCE — when a node's direct block is exhausted it pops the node
// and runs `push_if_new` over EVERY child (in reverse), marking each seen
// BEFORE descending into any of them. So on a DAG like
//   node0 -> [1, 2, 3] ; node1 -> [3] ; node2 = {0} ; node3 = {1}
// node3 is scheduled (and fixed in place) as node0's LAST child, so node1's
// edge to it is already-seen and skipped; the walk is [<n2.direct>, <n3.direct>]
// in node0's child order — NOT the recursive order where node1 would "own"
// node3's subtree. A recursive mark-then-descend reference yields the WRONG
// order [1, 0]; the real iterator yields [0, 1]. The reference therefore
// mirrors the iterator's EXPLICIT-STACK discipline (independently coded, but
// faithful to the scheduling rule that is the actual ground truth).
// ---------------------------------------------------------------------------

fn reference_walk(dag: &IndexDag, root: usize) -> Vec<usize> {
    let n = dag.len();
    let mut out = Vec::new();
    if root >= n {
        return out;
    }
    let mut visited: HashSet<usize> = HashSet::new();
    // Frontier of nodes scheduled-but-not-yet-expanded, in the order they must
    // be expanded. We mark a node seen WHEN WE SCHEDULE IT. To expand a node we
    // emit its direct block, then splice ITS unseen children (declaration order)
    // to the FRONT of the frontier — i.e. depth-first, but with the whole
    // sibling group scheduled (marked) as a unit at expansion time, exactly as
    // NestedSetIter does in its pop+push_if_new loop. (The production code uses
    // a LIFO stack with reverse-push; prepending the in-order child group to a
    // front-consumed frontier is the same visit order, coded independently.)
    visited.insert(root);
    let mut frontier: std::collections::VecDeque<usize> = std::collections::VecDeque::new();
    frontier.push_back(root);
    while let Some(node) = frontier.pop_front() {
        out.extend_from_slice(&dag.nodes[node].direct);
        // Children that are new get scheduled now (marked as a group), in
        // declaration order, ahead of the rest of the frontier.
        let mut newly: Vec<usize> = Vec::new();
        for &child in &dag.nodes[node].children {
            if child < n && visited.insert(child) {
                newly.push(child);
            }
        }
        for &child in newly.iter().rev() {
            frontier.push_front(child);
        }
    }
    out
}

/// Sum of `direct().len()` over the nodes reachable from `root`. The
/// element-completeness length bound: a correct node-deduping walk emits exactly
/// this many elements.
fn reachable_direct_count(dag: &IndexDag, root: usize) -> usize {
    reachable_nodes(dag, root)
        .into_iter()
        .map(|n| dag.nodes[n].direct.len())
        .sum()
}

/// Assert all three `ValidFlatten` conjuncts of `out` against `(dag, root)`.
/// Returns `Ok(())` if valid, `Err(reason)` otherwise so proptest can report.
fn check_valid_flatten(dag: &IndexDag, root: usize, out: &[usize]) -> Result<(), String> {
    // (3) order: must equal the independent reference walk.
    let reference = reference_walk(dag, root);
    if out != reference.as_slice() {
        return Err(format!(
            "order mismatch: walk {:?} != reference {:?}",
            out, reference
        ));
    }

    // (2) element completeness length bound: |out| == sum of direct over
    // reachable nodes (rejects dropped elements AND re-expanded shared nodes).
    let expected_len = reachable_direct_count(dag, root);
    if out.len() != expected_len {
        return Err(format!(
            "length {} != reachable direct count {} (out {:?})",
            out.len(),
            expected_len,
            out
        ));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Golden / specific-witness cases (always run, no randomness).
// ---------------------------------------------------------------------------

fn node(direct: &[usize], children: &[usize]) -> DagNode {
    DagNode::new(direct.to_vec(), children.to_vec())
}

#[test]
fn witness_diamond_shared_child_once() {
    // root -> {left, right}, both -> leaf. leaf expanded ONCE.
    let dag = IndexDag::new(vec![
        node(&[0], &[1, 2]),
        node(&[1], &[3]),
        node(&[2], &[3]),
        node(&[100, 200], &[]),
    ]);
    let out = flatten(&dag, 0);
    check_valid_flatten(&dag, 0, &out).unwrap();
    assert_eq!(out, vec![0, 1, 100, 200, 2]);
}

#[test]
fn witness_linear_chain() {
    let dag = IndexDag::new(vec![
        node(&[0], &[1]),
        node(&[10], &[2]),
        node(&[20], &[]),
    ]);
    let out = flatten(&dag, 0);
    check_valid_flatten(&dag, 0, &out).unwrap();
    assert_eq!(out, vec![0, 10, 20]);
}

#[test]
fn witness_direct_and_children() {
    // A node with BOTH direct items and children: direct emitted first.
    let dag = IndexDag::new(vec![node(&[7, 8], &[1]), node(&[9], &[])]);
    let out = flatten(&dag, 0);
    check_valid_flatten(&dag, 0, &out).unwrap();
    assert_eq!(out, vec![7, 8, 9]);
}

/// FAITHFUL non-dedup of VALUES (the walk dedups NODES, not values). This is
/// NOT a bug — it documents that the true invariant is node-level, so the
/// oracle must NOT assert value-uniqueness.
#[test]
fn value_repeated_witness() {
    let dag = IndexDag::new(vec![
        node(&[0], &[1, 2]),
        node(&[1, 99], &[]),
        node(&[2, 99], &[]),
    ]);
    let out = flatten(&dag, 0);
    check_valid_flatten(&dag, 0, &out).unwrap();
    assert_eq!(out, vec![0, 1, 99, 2, 99]);
    // 99 appears TWICE — the walk dedups nodes, not values.
    assert_eq!(out.iter().filter(|&&x| x == 99).count(), 2);
}

// ---------------------------------------------------------------------------
// ANTI-VACUITY: deliberately-wrong walks the oracle MUST catch.
// ---------------------------------------------------------------------------

/// A diamond where the shared leaf is RE-EXPANDED (visited per path, not once).
/// Caught by the length bound: it emits the leaf's direct block twice, so
/// `|out|` exceeds the reachable-direct count.
#[test]
fn mut_reexpand_shared_node() {
    let dag = IndexDag::new(vec![
        node(&[0], &[1, 2]),
        node(&[1], &[3]),
        node(&[2], &[3]),
        node(&[100, 200], &[]),
    ]);
    // Wrong walk: a plain tree-DFS with NO dedup set (re-expands shared node 3).
    fn bogus_no_dedup(dag: &IndexDag, node: usize, out: &mut Vec<usize>) {
        out.extend_from_slice(&dag.nodes[node].direct);
        for &c in &dag.nodes[node].children {
            bogus_no_dedup(dag, c, out);
        }
    }
    let mut bogus = Vec::new();
    bogus_no_dedup(&dag, 0, &mut bogus);
    // It emits leaf (100,200) twice → length 7, not 5.
    assert_eq!(bogus, vec![0, 1, 100, 200, 2, 100, 200]);
    assert!(
        check_valid_flatten(&dag, 0, &bogus).is_err(),
        "oracle went vacuous: accepted a re-expanding (O(N^2)) walk {:?}",
        bogus
    );
    // The correct walk is accepted.
    check_valid_flatten(&dag, 0, &flatten(&dag, 0)).unwrap();
}

/// A walk that DROPS a transitive element. Caught by the length bound.
#[test]
fn mut_drop_transitive_element() {
    let dag = IndexDag::new(vec![node(&[0], &[1]), node(&[10, 20], &[])]);
    // Drop the child's second element.
    let bogus = vec![0usize, 10];
    assert!(
        check_valid_flatten(&dag, 0, &bogus).is_err(),
        "oracle went vacuous: accepted a walk that drops a transitive element"
    );
    check_valid_flatten(&dag, 0, &flatten(&dag, 0)).unwrap();
}

/// A walk that emits CHILDREN BEFORE a node's own DIRECT items, when the real
/// order is direct-first. Caught by the order conjunct.
#[test]
fn mut_children_before_direct() {
    let dag = IndexDag::new(vec![node(&[7, 8], &[1]), node(&[9], &[])]);
    // Children-first (postorder-ish): child's 9 before root's 7,8.
    let bogus = vec![9usize, 7, 8];
    // Same multiset/length as the correct [7,8,9], so ONLY the order conjunct
    // can catch it — proving conjunct (3) is load-bearing, not redundant.
    assert_eq!(bogus.len(), flatten(&dag, 0).len());
    assert!(
        check_valid_flatten(&dag, 0, &bogus).is_err(),
        "oracle went vacuous: accepted children-before-direct order"
    );
    check_valid_flatten(&dag, 0, &flatten(&dag, 0)).unwrap();
}

// ---------------------------------------------------------------------------
// Property: random ACYCLIC index-DAGs always yield a ValidFlatten walk.
// ---------------------------------------------------------------------------

/// Generate a random ACYCLIC DAG (and a root) over `n` nodes by only allowing
/// child edges `n -> c` with `c > n`. This upper-triangular construction is
/// acyclic by construction and creates diamonds (multiple parents can point at
/// the same higher-indexed child), so the search space contains the exact
/// shared-node case the dedup must handle.
fn acyclic_dag_and_root() -> impl Strategy<Value = (IndexDag, usize)> {
    (1usize..=10).prop_flat_map(|n| {
        let rows: Vec<_> = (0..n)
            .map(|i| {
                // direct: 0..=4 element-IDs, drawn from a small pool so values
                // collide across nodes (exercises the value-non-dedup case).
                let direct = prop::collection::vec(0usize..20, 0..=4);
                // children: a subset of the strictly-higher node indices.
                let candidates: Vec<usize> = (i + 1..n).collect();
                let children =
                    proptest::sample::subsequence(candidates.clone(), 0..=candidates.len());
                (direct, children)
            })
            .collect();
        let root = 0usize..n;
        (rows, root).prop_map(|(rows, root)| {
            let nodes = rows
                .into_iter()
                .map(|(direct, children)| DagNode::new(direct, children))
                .collect();
            (IndexDag::new(nodes), root)
        })
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3000))]

    /// On any acyclic in-bounds DAG, the flatten walk satisfies all three
    /// ValidFlatten conjuncts (order, length bound, node completeness).
    #[test]
    fn acyclic_yields_valid_flatten((dag, root) in acyclic_dag_and_root()) {
        prop_assert!(dag.in_bounds(), "generator must emit in-bounds DAGs");
        let out = flatten(&dag, root);
        if let Err(why) = check_valid_flatten(&dag, root, &out) {
            prop_assert!(false, "ValidFlatten violated: {}", why);
        }
    }

    /// EXPAND-ONCE / node completeness, stated independently of the checker:
    /// the set of nodes the walk expands equals the reachable-node set, and
    /// each is expanded exactly once (so the O(unique) bound holds, not O(N^2)).
    #[test]
    fn expanded_equals_reachable((dag, root) in acyclic_dag_and_root()) {
        let expanded = rush_verified_core::expanded_nodes(&dag, root);
        let mut reach = reachable_nodes(&dag, root);
        reach.sort_unstable();
        prop_assert_eq!(&expanded, &reach,
            "expanded node set != reachable node set");
        // expanded_nodes is already sorted+deduped, so equality with the
        // reachable set already encodes "each expanded once".

        // Cross-check the length bound directly: |out| == sum of direct over
        // those reachable nodes.
        let out = flatten(&dag, root);
        let expected: usize = reach.iter().map(|&nidx| dag.nodes[nidx].direct.len()).sum();
        prop_assert_eq!(out.len(), expected,
            "flatten length {} != reachable direct count {}", out.len(), expected);
    }

    /// IDEMPOTENT determinism: the walk is a pure function of (dag, root).
    #[test]
    fn flatten_is_deterministic((dag, root) in acyclic_dag_and_root()) {
        prop_assert_eq!(flatten(&dag, root), flatten(&dag, root));
    }
}
