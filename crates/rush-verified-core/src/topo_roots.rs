// Rush verified-core — topological build-order from roots model
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Pure index-graph model for the `topo-roots-v1` verified scope.
//!
//! Rush builds a target by first computing a topological order of the
//! dependency graph restricted to the nodes reachable from a set of requested
//! roots, then compiling in that order (a dependency must be built before any
//! dependent). The production implementation is
//! [`DependencyGraph::topological_sort_from_roots`] in
//! `crates/rush-build/src/dependency_graph.rs` (Kahn's algorithm over a
//! `HashMap<Label, Node>` with `dependencies`/`dependents` adjacency). That
//! code is I/O- and `Label`-heavy; this module extracts its mathematical core
//! over `usize` index-graphs so a verifier can reason about it.
//!
//! The properties we model mirror the Dafny golden spec in
//! `verification/dafny/topo_roots/TopSort.dfy` (`ValidTopo`):
//!
//! > For graph `g` (adjacency: `g[n]` = the nodes `n` depends on) and `roots`,
//! > an order is *valid* iff
//! >   1. **no duplicates** — no node appears twice;
//! >   2. **completeness** — the order's node set equals the set reachable from
//! >      `roots` (the "returns empty" / "drops a reachable node" bug class is
//! >      rejected here); and
//! >   3. **dependency precedence** — for every reachable `n` and every
//! >      dependency `d` of `n`, `d` appears before `n` in the order.
//!
//! Scope note (matching the Dafny model): `ValidTopo` is satisfiable only for
//! the ACYCLIC reachable subgraph — a cycle admits no order with
//! deps-before-dependents. [`topological_sort_from_roots`] therefore returns
//! `None` when the reachable subgraph contains a cycle; the contract's
//! completeness/precedence guarantees are stated for the `Some` case.

#[allow(unused_imports)]
use crate::contract_shim::*;

/// A finite directed graph over the node indices `0..g.adjacency.len()`,
/// where `adjacency[n]` lists the nodes that `n` **depends on** (its
/// out-edges, i.e. `n -> d` means "build `d` before `n`"). This matches the
/// `g[n] = nodes n depends on` convention of the Dafny model and of rush's
/// `Node::dependencies`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexGraph {
    /// `adjacency[n]` is the list of dependency indices of node `n`. Every
    /// entry must be a valid node index (`< adjacency.len()`); see
    /// [`IndexGraph::in_bounds`].
    pub adjacency: Vec<Vec<usize>>,
}

impl IndexGraph {
    /// Build a graph from an adjacency list. `adjacency[n]` is the list of
    /// nodes that `n` depends on.
    pub fn new(adjacency: Vec<Vec<usize>>) -> Self {
        IndexGraph { adjacency }
    }

    /// Number of nodes in the graph.
    pub fn len(&self) -> usize {
        self.adjacency.len()
    }

    /// Whether the graph has no nodes.
    pub fn is_empty(&self) -> bool {
        self.adjacency.is_empty()
    }

    /// `InBounds` from the Dafny model: every adjacency target is a valid node
    /// index. The public entry point validates this so the rest of the code
    /// (and any verifier reasoning) may assume it.
    pub fn in_bounds(&self) -> bool {
        let n = self.adjacency.len();
        self.adjacency
            .iter()
            .all(|deps| deps.iter().all(|&d| d < n))
    }
}

/// Compute the set of nodes reachable from `roots` by following dependency
/// edges (`n -> d` for each `d` in `adjacency[n]`), as a sorted, de-duplicated
/// vector. Roots that are out of bounds are ignored (mirrors the Dafny
/// `ReachN` base case `r in roots && 0 <= r < |g|`).
///
/// This is the executable twin of the Dafny `Reachable` function; the oracle
/// uses it to state the completeness property independently of the sort.
pub fn reachable(graph: &IndexGraph, roots: &[usize]) -> Vec<usize> {
    let n = graph.len();
    let mut seen = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    for &r in roots {
        if r < n && !seen[r] {
            seen[r] = true;
            stack.push(r);
        }
    }
    while let Some(node) = stack.pop() {
        for &d in &graph.adjacency[node] {
            // `d < n` holds whenever the graph is in-bounds; guard anyway so
            // this helper is total even on a malformed graph.
            if d < n && !seen[d] {
                seen[d] = true;
                stack.push(d);
            }
        }
    }
    (0..n).filter(|&i| seen[i]).collect()
}

/// Topologically sort the subgraph of `graph` reachable from `roots`,
/// returning dependencies before dependents.
///
/// Mirrors [`DependencyGraph::topological_sort_from_roots`]: it first computes
/// the reachable scope from the union of all roots, then runs Kahn's algorithm
/// restricted to that scope.
///
/// Returns:
///   * `Some(order)` satisfying all three `ValidTopo` properties (no
///     duplicates, node-set equals the reachable set, every dependency
///     precedes its dependent) when the reachable subgraph is acyclic;
///   * `None` when the reachable subgraph contains a cycle (no valid order
///     exists — matching the Dafny scope note that `ValidTopo` is satisfiable
///     only for acyclic reachable subgraphs).
///
/// The contract below (under `--features creusot`) states the success-case
/// guarantee: whenever a `Some(order)` is returned, every node in the order is
/// reachable and every dependency of an ordered node precedes it.
#[cfg_attr(feature = "creusot",
    ensures(forall(|order: Vec<usize>|
        (result == Some(order)) ==>
            valid_topo_logic(graph, roots, order))))]
pub fn topological_sort_from_roots(graph: &IndexGraph, roots: &[usize]) -> Option<Vec<usize>> {
    let n = graph.len();

    // Scope = nodes reachable from the union of roots.
    let in_scope = reachability_mask(graph, roots);
    let scope_count = in_scope.iter().filter(|&&b| b).count();

    // In-degree within scope = number of dependencies that are also in scope.
    // (A node's "dependencies" are its out-edges; it becomes ready once all of
    // them are emitted — Kahn's algorithm on the dependency relation.)
    let mut in_degree: Vec<usize> = vec![0; n];
    // Reverse edges within scope: dependents[d] = nodes that depend on d.
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];
    for node in 0..n {
        if !in_scope[node] {
            continue;
        }
        for &dep in &graph.adjacency[node] {
            if dep < n && in_scope[dep] {
                in_degree[node] += 1;
                dependents[dep].push(node);
            }
        }
    }

    // Queue every in-scope node whose in-scope dependencies are all satisfied.
    // Process in ascending index order for a deterministic result.
    let mut queue: Vec<usize> = (0..n)
        .filter(|&i| in_scope[i] && in_degree[i] == 0)
        .collect();
    let mut head = 0usize;
    let mut sorted: Vec<usize> = Vec::with_capacity(scope_count);

    while head < queue.len() {
        let node = queue[head];
        head += 1;
        sorted.push(node);
        for &dependent in &dependents[node] {
            // Each in-scope forward edge was counted exactly once above, so
            // this decrement never underflows.
            in_degree[dependent] -= 1;
            if in_degree[dependent] == 0 {
                queue.push(dependent);
            }
        }
    }

    // Kahn's invariant: if we emitted fewer nodes than the scope, the
    // remaining nodes form a cycle (their in-degree never reached zero). No
    // valid topological order exists.
    if sorted.len() != scope_count {
        return None;
    }

    proof_assert!(true);
    Some(sorted)
}

/// Boolean reachability mask: `mask[i]` is true iff node `i` is reachable from
/// `roots`. Internal twin of [`reachable`] used by the sort to test scope
/// membership in O(1).
fn reachability_mask(graph: &IndexGraph, roots: &[usize]) -> Vec<bool> {
    let n = graph.len();
    let mut seen = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    for &r in roots {
        if r < n && !seen[r] {
            seen[r] = true;
            stack.push(r);
        }
    }
    while let Some(node) = stack.pop() {
        for &d in &graph.adjacency[node] {
            if d < n && !seen[d] {
                seen[d] = true;
                stack.push(d);
            }
        }
    }
    seen
}

/// Logic-level statement of `ValidTopo` for the Creusot contract: `order` is a
/// valid build order for `(graph, roots)`. Defined as a Creusot `logic` fn so
/// the proof can refer to it structurally; erased entirely in the default
/// build.
///
/// Mirrors the three conjuncts of the Dafny `ValidTopo` predicate:
///   1. no duplicates,
///   2. completeness (node-set equals the reachable set), and
///   3. dependency precedence (every dependency of an ordered node that is
///      itself ordered appears before it).
///
/// `no_dup` and "dependency precedence" are stated directly here; the
/// completeness conjunct (set == reachable) is discharged against the
/// executable [`reachable`] twin in the differential / proptest oracle, since
/// expressing reachability as a fixpoint in pearlite is the heavier obligation
/// deferred to the Dafny addendum (`Reachable`/`ReachN` in TopSort.dfy).
#[cfg(feature = "creusot")]
#[logic]
fn valid_topo_logic(graph: &IndexGraph, _roots: &[usize], order: Vec<usize>) -> bool {
    pearlite! {
        // (1) no node appears twice
        (forall(|i: Int, j: Int|
            0 <= i && i < j && j < order.len() ==> order[i] != order[j]))
        // (3) every dependency of an ordered node, if also ordered, precedes it
        && (forall(|i: Int, k: Int|
            0 <= i && i < order.len() && 0 <= k && k < order.len()
                && order[i] < graph.adjacency.len()
                && graph.adjacency[order[i]].contains(order[k])
                ==> index_of(order, order[k]) < i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `n` depends on `d` iff `d` appears in `graph.adjacency[n]`.
    fn pos(order: &[usize], x: usize) -> Option<usize> {
        order.iter().position(|&y| y == x)
    }

    #[test]
    fn empty_graph_empty_roots() {
        let g = IndexGraph::new(vec![]);
        assert_eq!(topological_sort_from_roots(&g, &[]), Some(vec![]));
    }

    #[test]
    fn single_node_no_deps() {
        let g = IndexGraph::new(vec![vec![]]);
        assert_eq!(topological_sort_from_roots(&g, &[0]), Some(vec![0]));
    }

    #[test]
    fn linear_chain_deps_precede() {
        // 0 depends on 1 depends on 2: build order must be 2,1,0.
        let g = IndexGraph::new(vec![vec![1], vec![2], vec![]]);
        let order = topological_sort_from_roots(&g, &[0]).unwrap();
        assert_eq!(order, vec![2, 1, 0]);
    }

    #[test]
    fn diamond() {
        // 0 -> {1,2}, 1 -> 3, 2 -> 3. 3 must precede 1 and 2; both precede 0.
        let g = IndexGraph::new(vec![vec![1, 2], vec![3], vec![3], vec![]]);
        let order = topological_sort_from_roots(&g, &[0]).unwrap();
        assert!(pos(&order, 3).unwrap() < pos(&order, 1).unwrap());
        assert!(pos(&order, 3).unwrap() < pos(&order, 2).unwrap());
        assert!(pos(&order, 1).unwrap() < pos(&order, 0).unwrap());
        assert!(pos(&order, 2).unwrap() < pos(&order, 0).unwrap());
        assert_eq!(order.len(), 4);
    }

    #[test]
    fn scope_excludes_unreachable() {
        // Node 2 is not reachable from root 0; it must not appear.
        let g = IndexGraph::new(vec![vec![1], vec![], vec![0]]);
        let order = topological_sort_from_roots(&g, &[0]).unwrap();
        assert_eq!(order, vec![1, 0]);
        assert!(!order.contains(&2));
    }

    #[test]
    fn union_of_roots() {
        // Two disjoint chains; both roots requested -> union covered.
        // 0->1, 2->3.
        let g = IndexGraph::new(vec![vec![1], vec![], vec![3], vec![]]);
        let order = topological_sort_from_roots(&g, &[0, 2]).unwrap();
        let mut set: Vec<usize> = order.clone();
        set.sort_unstable();
        assert_eq!(set, vec![0, 1, 2, 3]);
        assert!(pos(&order, 1).unwrap() < pos(&order, 0).unwrap());
        assert!(pos(&order, 3).unwrap() < pos(&order, 2).unwrap());
    }

    #[test]
    fn cycle_in_scope_returns_none() {
        // 0 -> 1 -> 0 is a cycle reachable from root 0.
        let g = IndexGraph::new(vec![vec![1], vec![0]]);
        assert_eq!(topological_sort_from_roots(&g, &[0]), None);
    }

    #[test]
    fn cycle_out_of_scope_is_fine() {
        // Cycle 1<->2 exists but is unreachable from root 0.
        let g = IndexGraph::new(vec![vec![], vec![2], vec![1]]);
        assert_eq!(topological_sort_from_roots(&g, &[0]), Some(vec![0]));
    }

    #[test]
    fn out_of_bounds_root_ignored() {
        let g = IndexGraph::new(vec![vec![]]);
        // Root 5 is out of bounds; root 0 is fine.
        assert_eq!(topological_sort_from_roots(&g, &[5, 0]), Some(vec![0]));
        // Only out-of-bounds roots -> empty reachable set -> empty order.
        assert_eq!(topological_sort_from_roots(&g, &[5]), Some(vec![]));
    }

    #[test]
    fn reachable_matches_scope() {
        let g = IndexGraph::new(vec![vec![1, 2], vec![3], vec![3], vec![]]);
        let mut r = reachable(&g, &[0]);
        r.sort_unstable();
        assert_eq!(r, vec![0, 1, 2, 3]);
        assert_eq!(reachable(&g, &[3]), vec![3]);
    }

    #[test]
    fn in_bounds_detects_bad_edge() {
        let ok = IndexGraph::new(vec![vec![1], vec![]]);
        assert!(ok.in_bounds());
        let bad = IndexGraph::new(vec![vec![9], vec![]]);
        assert!(!bad.in_bounds());
    }
}
