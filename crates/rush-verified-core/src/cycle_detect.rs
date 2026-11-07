// Rush verified-core — reachable-subgraph cycle-detection model
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Pure index-graph model for the `cycle-detect-v1` verified scope (authored
//! DAFNY-FIRST; see `verification/dafny/cycle_detect/CycleDetect.dfy`).
//!
//! This is the DIRECT SIBLING of [`topo_roots`](crate::topological_sort_from_roots):
//! same index-graph abstraction (it reuses [`IndexGraph`] and [`reachable`]),
//! same reachable-from-roots scope. Where `topo-roots-v1` and
//! `nested-set-walk-v1` both *assume* acyclic graphs (their scope notes), this
//! scope verifies the cycle DETECTION that rush actually performs — completing
//! the graph story.
//!
//! ## True detection discipline (extracted from `dependency_graph.rs`)
//!
//! Rush's production [`DependencyGraph::topological_sort_from_roots`] computes
//! the union of nodes reachable from the requested roots, then runs
//! `topological_sort_scoped` (`crates/rush-build/src/dependency_graph.rs:539`).
//! That is Kahn's algorithm over the in-scope nodes; its cycle test is the
//! KAHN'S RESIDUAL at `dependency_graph.rs:638`:
//!
//! > ```text
//! > // Check for cycles within scope
//! > if sorted.len() != scope.len() { return Err("Circular dependency detected!") }
//! > ```
//!
//! i.e. the nodes whose in-degree never reaches zero (after a fixed-point
//! recovery pass for stale reverse-edge bookkeeping, which is a `Label`-graph
//! artifact with no analogue in a clean index-graph) are exactly the nodes that
//! lie on, or downstream-feed into, a directed cycle within the reachable
//! subgraph. This module mirrors that residual discipline EXACTLY (the
//! `sorted.len() != scope_count` test is identical to
//! [`topological_sort_from_roots`](crate::topological_sort_from_roots)'s own at
//! `topo_roots.rs:181`), and then extends it: it walks the residual subgraph to
//! extract a concrete WITNESS cycle, because a useful detector reports *which*
//! cycle, not merely *that* one exists.
//!
//! The properties mirror the Dafny golden spec
//! (`verification/dafny/cycle_detect/CycleDetect.dfy`):
//!
//!   * `IsValidCycle(g, roots, cyc)` — `cyc` is non-empty, every entry is
//!     in-bounds and reachable from `roots`, consecutive entries are edges, and
//!     the last entry edges back to the first (a length-1 cycle is a self-loop).
//!   * `HasCycleInReachable(g, roots)` — some valid cycle exists.
//!   * `DetectCycle` — `Some(c)` with `IsValidCycle(g, roots, c)` iff
//!     `HasCycleInReachable`, else `None`.
//!
//! Scope note (matching the Dafny model and `topo_roots`): a cycle UNREACHABLE
//! from `roots` is NOT reported — it does not block the build. And the
//! connection to `topo-roots-v1` is the cross-check the oracle binds:
//! [`detect_cycle`]`(g, r).is_none() == `[`topological_sort_from_roots`](crate::topological_sort_from_roots)`(g, r).is_some()`
//! over all in-bounds graphs (the two kernels are exact complements over the
//! reachable subgraph — `ValidTopo` is satisfiable IFF `detect_cycle` is `None`).

#[allow(unused_imports)]
use crate::contract_shim::*;

// The cycle-detection scope shares the topo-roots index-graph abstraction
// verbatim: `IndexGraph` (adjacency: `adjacency[n]` = the nodes `n` depends on)
// and the executable `reachable`/reachability-mask reachability twin. Reusing
// them (rather than mirroring) keeps the two sibling scopes provably over the
// SAME graph object, which is what makes the topo↔cycle cross-check in the
// oracle meaningful.
use crate::topo_roots::IndexGraph;

/// Detect a directed cycle within the subgraph of `graph` reachable from
/// `roots`, returning a witness cycle if one exists.
///
/// Mirrors the production cycle test in
/// [`DependencyGraph::topological_sort_from_roots`]
/// (`crates/rush-build/src/dependency_graph.rs:638`): compute the reachable
/// scope, run Kahn's algorithm restricted to it, and treat the residual (nodes
/// whose in-degree never reached zero) as the cyclic core. This function then
/// walks that residual to extract a concrete closing cycle.
///
/// Returns:
///   * `Some(cyc)` — a witness cycle, where `cyc` is a non-empty sequence of
///     in-bounds nodes reachable from `roots`, consecutive entries are edges
///     (`cyc[i] -> cyc[i+1]`), and the last edges back to the first
///     (`cyc[len-1] -> cyc[0]`). A length-1 `cyc` is a self-loop.
///   * `None` — the reachable subgraph is acyclic (so
///     [`topological_sort_from_roots`](crate::topological_sort_from_roots)
///     returns `Some`).
///
/// The contract below (under `--features creusot`) states the tractable
/// half-guarantee: whenever a `Some(cyc)` is returned, `cyc` is non-empty and
/// every entry is in-bounds. The `cycle-exists iff Some` biconditional and the
/// edge-closure of the witness are the heavier obligations discharged against
/// the executable reachability/edge twins in the proptest oracle — the same
/// Pearlite-vs-oracle split `topo_roots` uses.
#[cfg_attr(feature = "creusot",
    ensures(forall(|cyc: Vec<usize>|
        (result == Some(cyc)) ==>
            some_witness_logic(graph, cyc))))]
pub fn detect_cycle(graph: &IndexGraph, roots: &[usize]) -> Option<Vec<usize>> {
    let n = graph.len();

    // Scope = nodes reachable from the union of roots (the production code's
    // `get_reachable_nodes` union, then `topological_sort_scoped`).
    let in_scope = reachability_mask(graph, roots);

    // In-degree within scope = number of dependencies that are also in scope —
    // identical accounting to `topo_roots::topological_sort_from_roots`.
    let mut in_degree: Vec<usize> = vec![0; n];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut scope_count = 0usize;
    for node in 0..n {
        if !in_scope[node] {
            continue;
        }
        scope_count += 1;
        for &dep in &graph.adjacency[node] {
            if dep < n && in_scope[dep] {
                in_degree[node] += 1;
                dependents[dep].push(node);
            }
        }
    }

    // Kahn's peel: drain every in-scope node whose in-scope dependencies are
    // all emitted. `peeled[i]` marks a node that reached in-degree 0.
    let mut peeled = vec![false; n];
    let mut queue: Vec<usize> = (0..n)
        .filter(|&i| in_scope[i] && in_degree[i] == 0)
        .collect();
    let mut head = 0usize;
    let mut peeled_count = 0usize;
    while head < queue.len() {
        let node = queue[head];
        head += 1;
        peeled[node] = true;
        peeled_count += 1;
        for &dependent in &dependents[node] {
            in_degree[dependent] -= 1;
            if in_degree[dependent] == 0 {
                queue.push(dependent);
            }
        }
    }

    // KAHN'S RESIDUAL (dependency_graph.rs:638 / topo_roots.rs:181): if fewer
    // nodes peeled than the scope holds, the un-peeled in-scope nodes form (and
    // feed into) a cycle. Acyclic reachable subgraph -> no residual -> None.
    if peeled_count == scope_count {
        return None;
    }

    // `residual[i]` = in-scope and never peeled. Every such node still has at
    // least one un-peeled in-scope dependency, so following dependency edges
    // that stay in the residual can never dead-end — a walk of length > n must
    // repeat a node, and the segment between the two visits is a cycle. Extract
    // one concrete witness via that walk.
    let residual: Vec<bool> = (0..n).map(|i| in_scope[i] && !peeled[i]).collect();

    // Start from any residual node (lowest index for determinism). Walk into a
    // residual dependency at each step, recording the path and the step index
    // at which each node was first entered, until we revisit a node — the loop
    // closes the cycle.
    let start = (0..n).find(|&i| residual[i]).expect("residual is non-empty");
    let mut visit_pos: Vec<Option<usize>> = vec![None; n];
    let mut path: Vec<usize> = Vec::new();
    let mut cur = start;
    loop {
        if let Some(first_seen) = visit_pos[cur] {
            // `cur` closes the loop: path[first_seen..] is the cycle, and its
            // last node has `cur` as a residual dependency (that is why we
            // stepped to `cur`), so the last->first edge holds.
            return Some(path[first_seen..].to_vec());
        }
        visit_pos[cur] = Some(path.len());
        path.push(cur);
        // Step into the first residual dependency. One must exist: a residual
        // node's in-degree never reached 0, so it has an un-peeled in-scope
        // dependency, which is itself residual.
        let next = graph.adjacency[cur]
            .iter()
            .copied()
            .find(|&d| d < n && residual[d])
            .expect("residual node must have a residual dependency");
        cur = next;
    }
}

/// Boolean reachability mask: `mask[i]` is true iff node `i` is reachable from
/// `roots`. Internal twin of [`crate::reachable`] (kept private here, mirroring
/// `topo_roots`'s own `reachability_mask`) used to test scope membership in
/// O(1). Behaves identically on the shared [`IndexGraph`].
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

/// Logic-level statement of the tractable `detect_cycle` post-condition for the
/// Creusot contract: a returned witness `cyc` is non-empty and every entry is
/// in-bounds. Defined as a Creusot `logic` fn so the proof can refer to it
/// structurally; erased entirely in the default build.
///
/// This is deliberately the TRACTABLE half (cf. `topo_roots::valid_topo_logic`,
/// which states no-dup + precedence in Pearlite but defers completeness to the
/// oracle). The full `IsValidCycle` (consecutive entries are edges, last edges
/// back to first, all reachable) and the `Some <==> HasCycleInReachable`
/// biconditional are expressed against the executable reachability/edge twins
/// in the proptest oracle (`tests/cycle_detect_prop.rs`) — the same
/// Pearlite-vs-oracle split, because edge-closure-as-a-fixpoint is the heavier
/// obligation deferred to the Dafny addendum (`IsValidCycle` /
/// `HasCycleInReachable` in CycleDetect.dfy).
#[cfg(feature = "creusot")]
#[logic]
fn some_witness_logic(graph: &IndexGraph, cyc: Vec<usize>) -> bool {
    pearlite! {
        // (1) the witness is non-empty
        cyc.len() > 0
        // (2) every entry is an in-bounds node index
        && (forall(|i: Int|
            0 <= i && i < cyc.len() ==> cyc[i] < graph.adjacency.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify `cyc` is a genuine closing cycle over `graph`: non-empty,
    /// in-bounds, consecutive edges, last->first edge. (Test-side twin of the
    /// Dafny `IsValidCycle` edge conjuncts; reachability is checked separately.)
    fn is_closing_cycle(graph: &IndexGraph, cyc: &[usize]) -> bool {
        if cyc.is_empty() {
            return false;
        }
        let n = graph.len();
        if !cyc.iter().all(|&x| x < n) {
            return false;
        }
        for w in cyc.windows(2) {
            if !graph.adjacency[w[0]].contains(&w[1]) {
                return false;
            }
        }
        graph.adjacency[cyc[cyc.len() - 1]].contains(&cyc[0])
    }

    #[test]
    fn empty_graph_no_cycle() {
        let g = IndexGraph::new(vec![]);
        assert_eq!(detect_cycle(&g, &[]), None);
    }

    #[test]
    fn single_node_no_deps_no_cycle() {
        let g = IndexGraph::new(vec![vec![]]);
        assert_eq!(detect_cycle(&g, &[0]), None);
    }

    #[test]
    fn acyclic_chain_no_cycle() {
        // 0 -> 1 -> 2, acyclic.
        let g = IndexGraph::new(vec![vec![1], vec![2], vec![]]);
        assert_eq!(detect_cycle(&g, &[0]), None);
    }

    #[test]
    fn self_loop_witness() {
        // 0 -> 0 is a self-loop reachable from root 0.
        let g = IndexGraph::new(vec![vec![0]]);
        let cyc = detect_cycle(&g, &[0]).expect("self-loop");
        assert_eq!(cyc, vec![0]);
        assert!(is_closing_cycle(&g, &cyc));
    }

    #[test]
    fn two_cycle_witness() {
        // 0 -> 1 -> 0.
        let g = IndexGraph::new(vec![vec![1], vec![0]]);
        let cyc = detect_cycle(&g, &[0]).expect("2-cycle");
        assert!(is_closing_cycle(&g, &cyc));
        assert_eq!(cyc.len(), 2);
    }

    #[test]
    fn three_cycle_witness() {
        // 0 -> 1 -> 2 -> 0.
        let g = IndexGraph::new(vec![vec![1], vec![2], vec![0]]);
        let cyc = detect_cycle(&g, &[0]).expect("3-cycle");
        assert!(is_closing_cycle(&g, &cyc));
        assert_eq!(cyc.len(), 3);
    }

    #[test]
    fn cycle_behind_unreached_node_not_reported() {
        // Node 0 is a lone root with no deps; the cycle 1<->2 is unreachable.
        let g = IndexGraph::new(vec![vec![], vec![2], vec![1]]);
        assert_eq!(detect_cycle(&g, &[0]), None);
    }

    #[test]
    fn cycle_reached_through_prefix() {
        // 0 -> 1, 1 -> 2 -> 1: the 1<->2 cycle is reached via 0.
        let g = IndexGraph::new(vec![vec![1], vec![2], vec![1]]);
        let cyc = detect_cycle(&g, &[0]).expect("reachable cycle");
        assert!(is_closing_cycle(&g, &cyc));
        // The witness is the cycle proper (1,2), not the acyclic prefix node 0.
        assert!(!cyc.contains(&0));
    }

    #[test]
    fn out_of_bounds_root_ignored() {
        let g = IndexGraph::new(vec![vec![0]]);
        // Root 5 OOB -> empty scope -> no cycle reported.
        assert_eq!(detect_cycle(&g, &[5]), None);
        // Root 0 reaches the self-loop.
        assert!(detect_cycle(&g, &[0]).is_some());
    }
}
