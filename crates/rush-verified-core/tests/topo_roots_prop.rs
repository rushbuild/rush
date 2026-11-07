// Rush verified-core — topological build-order ORACLE
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT
//
// spec-owned (oracle): this file is the functional oracle for the
// topo-roots-v1 verified scope. It witnesses the three ValidTopo properties
// from verification/dafny/topo_roots/TopSort.dfy on thousands of random
// acyclic graphs, and proves the oracle is non-vacuous via a deliberately
// wrong variant it catches. Deleting, #[ignore]-ing, or weakening these
// assertions is a spec-sensitive change (Spec-Change: oracle) and routes
// through the human Spec-Review gate. See ci/spec-owned.tsv.
//
//! Property/golden ORACLE for `topological_sort_from_roots`. It checks, on
//! every accepted (acyclic, in-scope) graph, the three conjuncts of the Dafny
//! `ValidTopo` predicate:
//!
//!   1. **no duplicates** — no node appears twice in the order;
//!   2. **completeness** — the order's node set equals
//!      [`rush_verified_core::reachable`]`(graph, roots)` (the "returns empty"
//!      / "drops a reachable node" bug class is rejected here); and
//!   3. **dependency precedence** — every dependency of an ordered node that
//!      is itself in scope appears before it.
//!
//! Anti-vacuity is demonstrated two ways:
//!   * `wrong_sort_violates_precedence` runs a deliberately-broken sort (it
//!     emits the reachable set in ASCENDING index order, ignoring edges) and
//!     shows the precedence checker catches it — so the oracle is not trivially
//!     satisfiable.
//!   * the generator deliberately emits multi-root, diamond, and deep-chain
//!     graphs, so the search space contains the orderings that would expose a
//!     mis-ordered or incomplete result.

use proptest::prelude::*;
use rush_verified_core::{reachable, topological_sort_from_roots, IndexGraph};
use std::collections::HashSet;

// ---------------------------------------------------------------------------
// ValidTopo checker (the oracle proper) — shared by golden + property tests.
// ---------------------------------------------------------------------------

/// Assert all three `ValidTopo` conjuncts of `order` against `(graph, roots)`.
/// Returns `Ok(())` if valid, `Err(reason)` otherwise so proptest can report
/// the failing case.
fn check_valid_topo(
    graph: &IndexGraph,
    roots: &[usize],
    order: &[usize],
) -> Result<(), String> {
    // (1) no duplicates
    let mut seen = HashSet::new();
    for &x in order {
        if !seen.insert(x) {
            return Err(format!("duplicate node {} in order {:?}", x, order));
        }
    }

    // (2) completeness: node set == reachable set
    let order_set: HashSet<usize> = order.iter().copied().collect();
    let reach_set: HashSet<usize> = reachable(graph, roots).into_iter().collect();
    if order_set != reach_set {
        return Err(format!(
            "node set {:?} != reachable set {:?}",
            order_set, reach_set
        ));
    }

    // (3) dependency precedence: for every ordered node n and every dependency
    // d of n that is in scope, pos(d) < pos(n).
    let pos: std::collections::HashMap<usize, usize> = order
        .iter()
        .enumerate()
        .map(|(i, &x)| (x, i))
        .collect();
    for (&n, &n_pos) in &pos {
        for &d in &graph.adjacency[n] {
            if let Some(&d_pos) = pos.get(&d) {
                if d_pos >= n_pos {
                    return Err(format!(
                        "dependency {} of {} does not precede it (pos {} >= {}) in {:?}",
                        d, n, d_pos, n_pos, order
                    ));
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Golden / specific-witness cases (always run, no randomness).
// ---------------------------------------------------------------------------

#[test]
fn witness_linear_chain() {
    // 0->1->2: order must be 2,1,0.
    let g = IndexGraph::new(vec![vec![1], vec![2], vec![]]);
    let order = topological_sort_from_roots(&g, &[0]).expect("acyclic");
    check_valid_topo(&g, &[0], &order).unwrap();
    assert_eq!(order, vec![2, 1, 0]);
}

#[test]
fn witness_diamond() {
    let g = IndexGraph::new(vec![vec![1, 2], vec![3], vec![3], vec![]]);
    let order = topological_sort_from_roots(&g, &[0]).expect("acyclic");
    check_valid_topo(&g, &[0], &order).unwrap();
}

#[test]
fn witness_cycle_rejected() {
    // A cycle reachable from the root has NO valid order: the sort must say so
    // rather than returning a bogus (incomplete) order.
    let g = IndexGraph::new(vec![vec![1], vec![0]]);
    assert_eq!(topological_sort_from_roots(&g, &[0]), None);
}

/// ANTI-VACUITY: a deliberately-wrong "sort" that emits the reachable set in
/// ascending index order (ignoring edges) MUST be caught by the precedence
/// conjunct of the oracle on at least one graph — proving the oracle is
/// meaningful. If `check_valid_topo` accepted this, the oracle would be
/// vacuous.
#[test]
fn wrong_sort_violates_precedence() {
    // 0 depends on 1 (0 -> 1). Ascending order [0,1] places dependency 1 AFTER
    // its dependent 0 — a precedence violation the oracle must reject.
    let g = IndexGraph::new(vec![vec![1], vec![]]);
    let roots = [0usize];
    let bogus = {
        let mut r = reachable(&g, &roots);
        r.sort_unstable(); // ascending index order, ignores dependency edges
        r
    };
    assert_eq!(bogus, vec![0, 1]);
    let verdict = check_valid_topo(&g, &roots, &bogus);
    assert!(
        verdict.is_err(),
        "oracle went vacuous: it accepted a precedence-violating order {:?}",
        bogus
    );
    // And the correct sort is accepted, so the oracle isn't rejecting everything.
    let good = topological_sort_from_roots(&g, &roots).unwrap();
    check_valid_topo(&g, &roots, &good).unwrap();
}

/// ANTI-VACUITY (completeness): an order that DROPS a reachable node must be
/// caught by conjunct (2).
#[test]
fn wrong_sort_violates_completeness() {
    let g = IndexGraph::new(vec![vec![1], vec![]]);
    let roots = [0usize];
    // Drop node 1 — still acyclic-looking, still in dependency order, but
    // incomplete.
    let incomplete = vec![0usize];
    assert!(
        check_valid_topo(&g, &roots, &incomplete).is_err(),
        "oracle went vacuous: accepted an incomplete order"
    );
}

// ---------------------------------------------------------------------------
// Property: random ACYCLIC index-graphs always yield a ValidTopo order.
// ---------------------------------------------------------------------------

/// Generate a random ACYCLIC graph (and a root set) over `n` nodes by only
/// allowing edges `n -> d` with `d > n`. This upper-triangular construction is
/// acyclic by construction, so `topological_sort_from_roots` must always
/// return `Some` and that result must satisfy `ValidTopo`.
fn acyclic_graph_and_roots() -> impl Strategy<Value = (IndexGraph, Vec<usize>)> {
    (1usize..=10).prop_flat_map(|n| {
        // For each node i, choose a subset of the strictly-higher indices.
        let rows: Vec<_> = (0..n)
            .map(|i| {
                let candidates: Vec<usize> = (i + 1..n).collect();
                proptest::sample::subsequence(candidates.clone(), 0..=candidates.len())
            })
            .collect();
        let roots = proptest::sample::subsequence((0..n).collect::<Vec<_>>(), 0..=n);
        (rows, roots).prop_map(|(adjacency, roots)| (IndexGraph::new(adjacency), roots))
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3000))]

    /// On any acyclic in-bounds graph, the sort returns Some and the result is
    /// a valid topological order (all three ValidTopo conjuncts).
    #[test]
    fn acyclic_yields_valid_topo((graph, roots) in acyclic_graph_and_roots()) {
        prop_assert!(graph.in_bounds(), "generator must emit in-bounds graphs");
        let order = topological_sort_from_roots(&graph, &roots);
        prop_assert!(order.is_some(), "acyclic graph must sort: {:?}", graph);
        let order = order.unwrap();
        if let Err(why) = check_valid_topo(&graph, &roots, &order) {
            prop_assert!(false, "ValidTopo violated: {}", why);
        }
    }

    /// The reachable() twin agrees with the sort's node set on acyclic graphs
    /// (completeness, stated independently of the checker's HashSet).
    #[test]
    fn reachable_equals_order_set((graph, roots) in acyclic_graph_and_roots()) {
        let order = topological_sort_from_roots(&graph, &roots).unwrap();
        let mut order_sorted = order.clone();
        order_sorted.sort_unstable();
        let mut reach = reachable(&graph, &roots);
        reach.sort_unstable();
        prop_assert_eq!(order_sorted, reach);
    }
}

// ---------------------------------------------------------------------------
// Property: graphs WITH cycles in scope are rejected (return None).
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    /// A self-loop on a reachable node makes the reachable subgraph cyclic, so
    /// the sort must return None (no valid order exists). This guards the
    /// "report a cycle instead of an incomplete order" property.
    #[test]
    fn reachable_self_loop_is_rejected(n in 1usize..=8, root in 0usize..8) {
        let root = root % n;
        // Build a chain root -> root+1 -> ... so `root` reaches a self-loop,
        // by adding a self-edge on `root` itself.
        let mut adjacency: Vec<Vec<usize>> = (0..n)
            .map(|i| if i + 1 < n { vec![i + 1] } else { vec![] })
            .collect();
        adjacency[root].push(root); // self-loop on a reachable node
        let g = IndexGraph::new(adjacency);
        prop_assert_eq!(topological_sort_from_roots(&g, &[root]), None);
    }
}
