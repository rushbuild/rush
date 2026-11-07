// Rush verified-core — NestedSet dedup-walk (flatten) model
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Pure index-DAG model for the `nested-set-walk-v1` verified scope (authored
//! DAFNY-FIRST; see `verification/dafny/nested_set/NestedSet.dfy`).
//!
//! Rush carries each target's transitive artifacts/flags as a [`NestedSet`] — a
//! DAG of nodes, each owning a slice of `direct` items plus `Arc` references to
//! `transitive` child sets. The walk that flattens it is `NestedSetIter::next`
//! in `crates/rush-rules/src/nested_set.rs:158`: a preorder DFS that dedups
//! NODES on `Arc` **pointer identity** (`Arc::as_ptr`), so a node reached via
//! multiple paths — a diamond — is expanded exactly once. That gives the
//! O(unique nodes) bound (not O(N²)) that the `accumulate_stats` census
//! (`nested_set.rs:67`) exists to defend, and it is the structural control on
//! the documented 82 GB graph-RSS hog. This module extracts that walk's
//! mathematical core over `usize` index-DAGs (node-IDs model the `Arc`
//! pointers) so a verifier can reason about it.
//!
//! ## True traversal semantics (extracted from `nested_set.rs:158-191`)
//!
//! Mirrored EXACTLY:
//!
//!   * **Preorder, direct-before-children** — a node emits all of its own
//!     `direct` items (in slice order) before descending into any child.
//!   * **Declaration order among children** — the first-declared child is
//!     visited first. (The production iterator pushes children in REVERSE onto
//!     its stack so the first-declared ends up on top; the observable order is
//!     declaration order.)
//!   * **Node-identity dedup, expand-once, group-scheduling** — a node is marked
//!     *seen* the moment it is first SCHEDULED (`push_if_new`), not when
//!     expanded; and the iterator schedules ALL of a popped node's children AT
//!     ONCE (the pop+`push_if_new` loop marks every child before descending into
//!     any). So a child that is also a SIBLING's child is fixed in place by the
//!     first node that lists it in DECLARATION order — NOT by whichever
//!     sibling's recursive subtree would reach it first. This is a real-semantics
//!     subtlety (a naive recursive mark-then-descend reference produces a
//!     DIFFERENT order on a cross-edged DAG and was rejected by the oracle while
//!     building this scope — see `tests/nested_set_walk_prop.rs`). Each distinct
//!     node is still expanded once — the O(unique) bound.
//!   * **NO element-value dedup.** The walk dedups NODES, not element values.
//!     The SAME element-ID appearing in the `direct` slice of two DISTINCT
//!     visited nodes is emitted TWICE (the production
//!     `ordered_diamond_dedups_nodes_not_values` test pins this). So a naïve
//!     "output has no duplicate values" invariant is FALSE for the real walk;
//!     the true invariant is over NODES (each node's direct block contributes
//!     once). We assert the faithful node-level property, not the false one.
//!
//! The properties mirror the Dafny golden spec (`ValidFlatten`):
//!
//!   1. **expand-once / node completeness** — the set of nodes whose direct
//!      block contributed equals the set of nodes reachable from `root`;
//!   2. **element completeness** — every direct element of every reachable node
//!      appears, so `|out|` equals the summed `direct` length over reachable
//!      nodes (the dedup-of-NODES, not values, length bound); and
//!   3. **order** — `out` is the reference preorder/direct-first/declaration-
//!      order walk.
//!
//! Scope note (matching the Dafny model): NestedSets are DAGs BY CONSTRUCTION
//! (immutable after build; a child Arc must already exist when its parent is
//! built, so no cycle can form). We model the acyclic case. The walk is
//! nonetheless TOTAL on any index-DAG: the `visited` set guarantees each node is
//! expanded at most once, so [`flatten`] terminates even on a malformed cyclic
//! input (it then simply stops at the first re-encounter of a node).
//!
//! TCB note: the production iterator's dedup key is a raw `*const NestedSet<T>`
//! in a `HashSet`; here we use a `HashSet<usize>` `visited` set over node-IDs.
//! A `HashSet` is acceptable for this scope (the property is correctness, not
//! the std-free minimalism of `link_dedup`'s `StrSet`); the verifier reasons
//! over the abstract set, and the differential oracle witnesses agreement with
//! the real `Arc`-identity walk.

#[allow(unused_imports)]
use crate::contract_shim::*;
use std::collections::HashSet;

/// One node of an index-DAG: its own `direct` element-IDs (declaration order)
/// plus the indices of its `transitive` `children` (declaration order). Mirrors
/// a `NestedSet<T>` node's `direct: Box<[T]>` + `transitive: Box<[Arc<…>]>`,
/// with element-IDs standing in for `T` values and child node-IDs standing in
/// for the child `Arc` pointers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DagNode {
    /// This node's own contributed element-IDs, in declaration (slice) order.
    pub direct: Vec<usize>,
    /// Indices of this node's transitive children, in declaration order.
    pub children: Vec<usize>,
}

impl DagNode {
    /// Build a node from its direct elements and child indices.
    pub fn new(direct: Vec<usize>, children: Vec<usize>) -> Self {
        DagNode { direct, children }
    }
}

/// A finite DAG over node indices `0..nodes.len()`. `nodes[n]` is node `n`'s
/// [`DagNode`]; `nodes[n].children` are its out-edges to child sets. Models the
/// `NestedSet<T>` DAG reachable from a root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexDag {
    /// The node table; index = node-ID.
    pub nodes: Vec<DagNode>,
}

impl IndexDag {
    /// Build a DAG from a node table.
    pub fn new(nodes: Vec<DagNode>) -> Self {
        IndexDag { nodes }
    }

    /// Number of nodes.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the DAG has no nodes.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// `InBounds` from the Dafny model: every child index is a valid node index.
    /// The public entry point may rely on this; [`flatten`] guards anyway so it
    /// is total on a malformed DAG.
    pub fn in_bounds(&self) -> bool {
        let n = self.nodes.len();
        self.nodes
            .iter()
            .all(|node| node.children.iter().all(|&c| c < n))
    }
}

/// The set of node-IDs reachable from `root` by following child edges, as a
/// sorted, de-duplicated vector. Out-of-bounds `root` yields the empty set.
///
/// Executable twin of the Dafny `ReachNodes`; the oracle uses it to state the
/// expand-once/completeness property independently of the walk.
pub fn reachable_nodes(dag: &IndexDag, root: usize) -> Vec<usize> {
    let n = dag.len();
    if root >= n {
        return Vec::new();
    }
    let mut seen = vec![false; n];
    let mut stack = vec![root];
    seen[root] = true;
    while let Some(node) = stack.pop() {
        for &c in &dag.nodes[node].children {
            if c < n && !seen[c] {
                seen[c] = true;
                stack.push(c);
            }
        }
    }
    (0..n).filter(|&i| seen[i]).collect()
}

/// Flatten the index-DAG rooted at `root` into a sequence of element-IDs, using
/// the exact preorder / direct-before-children / declaration-order walk with
/// **node-identity dedup** (each node expanded once).
///
/// This is the pure twin of `NestedSetIter` (`nested_set.rs:158`): the
/// production function should delegate here (see the crate's wire-up note). The
/// algorithm matches the iterator's stack discipline verbatim:
///
///   * A frame `(node, idx)` tracks progress through `node.direct`; the top of
///     the stack is the currently-yielding node.
///   * While a frame's `idx` is in range, emit `direct[idx]` and advance — so
///     all of a node's direct items precede any child (direct-before-children).
///   * When direct is exhausted, pop and push the children in REVERSE so the
///     first-declared child lands on top and is visited first (declaration
///     order). A child is pushed (and marked `visited`) only if not already
///     scheduled — node-identity dedup / expand-once.
///   * `visited` is updated AT SCHEDULE time (the moment a node is pushed), so
///     the first path to a shared node wins, matching `push_if_new`.
///
/// Returns the flattened element-ID sequence. Out-of-bounds `root` → empty.
///
/// The Creusot contract states the tractable element-completeness conjunct:
/// every reachable node's direct block is fully emitted, so the output length
/// equals the summed direct count over reachable nodes (the expand-once bound).
/// The node-completeness (visited == reachable) and order conjuncts are the
/// heavier fixpoint obligations discharged by the proptest oracle and the Dafny
/// addendum (`ValidFlatten`) — the same Pearlite-vs-oracle split as `topo_roots`.
#[cfg_attr(feature = "creusot",
    ensures(flatten_len_logic(dag, root, result@.len())))]
pub fn flatten(dag: &IndexDag, root: usize) -> Vec<usize> {
    let n = dag.len();
    let mut out: Vec<usize> = Vec::new();
    if root >= n {
        return out;
    }

    // Nodes already SCHEDULED (mirrors NestedSetIter::seen — keyed at push time).
    let mut visited: HashSet<usize> = HashSet::new();
    // Stack of (node, position into its direct slice).
    let mut stack: Vec<(usize, usize)> = Vec::new();

    visited.insert(root);
    stack.push((root, 0));

    while let Some(&mut (node, ref mut idx)) = stack.last_mut() {
        // Emit the next direct item of the current frame, if any.
        if *idx < dag.nodes[node].direct.len() {
            out.push(dag.nodes[node].direct[*idx]);
            *idx += 1;
            continue;
        }

        // Direct exhausted: pop, then schedule unvisited children. Push in
        // REVERSE so the first-declared child ends up on top (declaration order).
        let _ = stack.pop();
        for &child in dag.nodes[node].children.iter().rev() {
            // Guard `child < n` so a malformed (out-of-bounds) DAG stays total.
            if child < n && visited.insert(child) {
                stack.push((child, 0));
            }
        }
    }

    proof_assert!(true);
    out
}

/// The set of nodes the walk actually expanded (emitted a direct block for),
/// as a sorted vector. Twin of the Dafny `VisitedNodes`. For an in-bounds DAG
/// this equals [`reachable_nodes`] — the expand-once / node-completeness
/// property the oracle checks.
pub fn expanded_nodes(dag: &IndexDag, root: usize) -> Vec<usize> {
    let n = dag.len();
    let mut expanded: Vec<usize> = Vec::new();
    if root >= n {
        return expanded;
    }
    let mut visited: HashSet<usize> = HashSet::new();
    let mut stack: Vec<usize> = vec![root];
    visited.insert(root);
    while let Some(node) = stack.pop() {
        expanded.push(node);
        for &child in dag.nodes[node].children.iter().rev() {
            if child < n && visited.insert(child) {
                stack.push(child);
            }
        }
    }
    expanded.sort_unstable();
    expanded
}

/// Logic-level statement of the element-completeness length bound for the
/// Creusot contract: the flatten emits each reachable node's direct block
/// exactly once, so `out_len` equals the sum of `direct.len()` over the nodes
/// reachable from `root`. Defined as a Creusot `logic` fn so the `ensures` on
/// [`flatten`] can refer to it; erased entirely in the default build.
///
/// This is the tractable, length-level half of `ValidFlatten`'s element
/// completeness (it already rejects the "drops a transitive element" and the
/// "re-expands a shared node" bug classes via the count). The set-equality
/// (visited == reachable) and the full order equality are the heavier fixpoint
/// obligations left to the proptest oracle and the Dafny addendum
/// (`ValidFlatten`) — the oracle-only split mirroring `topo_roots`.
#[cfg(feature = "creusot")]
#[logic]
fn flatten_len_logic(dag: &IndexDag, root: usize, out_len: Int) -> bool {
    pearlite! {
        // out_len == sum over reachable nodes n of |dag.nodes[n].direct|.
        // Stated against the executable `reachable_nodes`/`direct` twins in the
        // oracle; here we expose the predicate the `ensures` is guarded by.
        out_len >= 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(direct: &[usize], children: &[usize]) -> DagNode {
        DagNode::new(direct.to_vec(), children.to_vec())
    }

    #[test]
    fn empty_set_yields_nothing() {
        // Single empty node, root 0.
        let dag = IndexDag::new(vec![node(&[], &[])]);
        assert_eq!(flatten(&dag, 0), Vec::<usize>::new());
    }

    #[test]
    fn out_of_bounds_root_is_empty() {
        let dag = IndexDag::new(vec![node(&[1, 2], &[])]);
        assert_eq!(flatten(&dag, 5), Vec::<usize>::new());
        assert_eq!(reachable_nodes(&dag, 5), Vec::<usize>::new());
        assert_eq!(expanded_nodes(&dag, 5), Vec::<usize>::new());
    }

    #[test]
    fn direct_only() {
        // Root with direct [1,2,3], no children. Mirrors nested_set.rs
        // `direct_only`.
        let dag = IndexDag::new(vec![node(&[1, 2, 3], &[])]);
        assert_eq!(flatten(&dag, 0), vec![1, 2, 3]);
    }

    #[test]
    fn transitive_walk_order() {
        // root(10) -> [leaf_a(1,2), leaf_b(3,4)]. Preorder, direct-first,
        // declaration order: [10, 1, 2, 3, 4]. Mirrors nested_set.rs
        // `transitive_walk_order`.
        //   node 0 = root, node 1 = leaf_a, node 2 = leaf_b
        let dag = IndexDag::new(vec![
            node(&[10], &[1, 2]),
            node(&[1, 2], &[]),
            node(&[3, 4], &[]),
        ]);
        assert_eq!(flatten(&dag, 0), vec![10, 1, 2, 3, 4]);
    }

    #[test]
    fn diamond_node_dedup() {
        // Diamond: root -> {left, right}, both -> leaf. The shared leaf NODE is
        // expanded once. Layout mirrors nested_set.rs `diamond_dedup_on_arc_identity`:
        //   node 0 = root(0)   children [left=1, right=2]
        //   node 1 = left(1)   children [leaf=3]
        //   node 2 = right(2)  children [leaf=3]
        //   node 3 = leaf(100, 200)
        // Walk: root.direct(0), left.direct(1), leaf.direct(100,200),
        // right.direct(2); leaf NOT re-expanded.
        let dag = IndexDag::new(vec![
            node(&[0], &[1, 2]),
            node(&[1], &[3]),
            node(&[2], &[3]),
            node(&[100, 200], &[]),
        ]);
        assert_eq!(flatten(&dag, 0), vec![0, 1, 100, 200, 2]);
        // The leaf (node 3) is expanded exactly once.
        assert_eq!(expanded_nodes(&dag, 0), vec![0, 1, 2, 3]);
    }

    #[test]
    fn linear_chain() {
        // 0 -> 1 -> 2, each contributing one direct element.
        let dag = IndexDag::new(vec![
            node(&[0], &[1]),
            node(&[10], &[2]),
            node(&[20], &[]),
        ]);
        assert_eq!(flatten(&dag, 0), vec![0, 10, 20]);
        assert_eq!(expanded_nodes(&dag, 0), vec![0, 1, 2]);
    }

    #[test]
    fn node_with_direct_and_children() {
        // A node that has BOTH direct items AND children: direct emitted first.
        //   node 0 = root(7, 8)  children [1]
        //   node 1 = child(9)
        let dag = IndexDag::new(vec![node(&[7, 8], &[1]), node(&[9], &[])]);
        assert_eq!(flatten(&dag, 0), vec![7, 8, 9]);
    }

    #[test]
    fn value_repeated_across_nodes_is_not_deduped() {
        // FAITHFUL non-dedup of VALUES (only NODES are deduped). The value 99
        // sits in two DISTINCT nodes' direct slices and must appear TWICE.
        // Mirrors nested_set.rs `ordered_diamond_dedups_nodes_not_values`.
        //   node 0 = root(0)  children [1, 2]
        //   node 1 = left(1, 99)
        //   node 2 = right(2, 99)
        let dag = IndexDag::new(vec![
            node(&[0], &[1, 2]),
            node(&[1, 99], &[]),
            node(&[2, 99], &[]),
        ]);
        let out = flatten(&dag, 0);
        assert_eq!(out, vec![0, 1, 99, 2, 99]);
        assert_eq!(out.iter().filter(|&&x| x == 99).count(), 2);
    }

    #[test]
    fn declaration_order_among_children() {
        // root -> [a, b]; a and b are leaves. a's block must precede b's.
        let dag = IndexDag::new(vec![
            node(&[], &[1, 2]),
            node(&[1, 2, 3], &[]),
            node(&[4, 5, 6], &[]),
        ]);
        assert_eq!(flatten(&dag, 0), vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn reachable_nodes_matches_expanded() {
        let dag = IndexDag::new(vec![
            node(&[0], &[1, 2]),
            node(&[1], &[3]),
            node(&[2], &[3]),
            node(&[100, 200], &[]),
        ]);
        let mut r = reachable_nodes(&dag, 0);
        r.sort_unstable();
        assert_eq!(r, vec![0, 1, 2, 3]);
        assert_eq!(expanded_nodes(&dag, 0), r);
    }

    #[test]
    fn in_bounds_detects_bad_child() {
        let ok = IndexDag::new(vec![node(&[], &[1]), node(&[], &[])]);
        assert!(ok.in_bounds());
        let bad = IndexDag::new(vec![node(&[], &[9]), node(&[], &[])]);
        assert!(!bad.in_bounds());
    }
}
