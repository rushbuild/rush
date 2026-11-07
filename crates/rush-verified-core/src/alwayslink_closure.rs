// Rush verified-core — whole-archive/normal bucket-CONSTRUCTION from the dep DAG
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Pure index-DAG model for the `alwayslink-closure-v1` verified scope (authored
//! DAFNY-FIRST; see `verification/dafny/alwayslink_closure/AlwaysLinkClosure.dfy`).
//!
//! ## The production bug this kernel models
//!
//! The C++ test link assembler (`CcTestInstance`,
//! `crates/rush-rules/src/rules_cc.rs`) honors the `alwayslink` bit when it
//! walks the DIRECT deps' TYPED `CcInfo.linker_inputs` (≈ line 3294-3309 →
//! whole-archive bucket), but its TRANSITIVE artifact sweep (≈ line 3344-3357,
//! `iter_transitive_artifacts`) pulls every reachable `.a` into the NORMAL
//! bucket UNCONDITIONALLY — the alwayslink bit does NOT ride on plain
//! `Artifact`s. So a `REGISTER_OP`/`REGISTER_KERNEL` static-initializer archive
//! (MlirPassthroughOp, ShuffleDataset, NoOp/_Send/_Recv, …) reachable ONLY via
//! that transitive fallback is NOT whole-archived → the linker strips its
//! initializers (nothing references them) → runtime "no registered op/kernel".
//! Four corpus tests (api_test, backwards_compatibility_test,
//! shuffle_dataset_op_test, rewrite_utils_test) fail this way.
//!
//! The `link-plan-v1` scope ([`crate::assemble_link_plan`]) is VACUOUS against
//! this defect: it proves the link-line ASSEMBLY given correctly-tagged buckets.
//! This scope is one level UP — in BUCKET CONSTRUCTION from the dep DAG. The fix
//! operates on TYPED linker-input metadata `(archive-id, alwayslink)` carried
//! through the WHOLE reachable closure, not raw artifacts; an archive's
//! effective alwayslink status is the OR over ALL reachable declarations of it.
//!
//! ## Composition with `link-plan-v1`
//!
//! This kernel is the PRODUCER; [`assemble_link_plan`](crate::assemble_link_plan)
//! is the CONSUMER. The `(whole_archive, normal)` partition [`alwayslink_closure`]
//! returns is EXACTLY the bucket tagging `assemble_link_plan` ASSUMES as input:
//! feed each archive in `whole_archive` as a `DepLib { alwayslink: true }` and
//! each in `normal` as `DepLib { alwayslink: false }`, and `link-plan-v1`'s P2
//! (alwayslink-wins / no-double-list) is discharged by THIS scope's P3
//! (disjoint) + P5 (normal-not-alwayslink). Together: link-plan proved "given
//! correct buckets the link line is correct"; this proves "the buckets are
//! correct over the transitive closure".
//!
//! ## True semantics (the buckets are over ARCHIVE-IDs, not occurrences)
//!
//! A [`LinkInputDag`] is a DAG over node indices; node `n` carries its own typed
//! `inputs: Vec<(archive_id, alwayslink)>` plus child node-IDs (declaration
//! order). The same archive may be declared by MANY reachable nodes with
//! CONFLICTING bits; its effective status is the OR. The output buckets are over
//! the DE-DUPLICATED archive set, so a per-occurrence "no-dup" invariant would be
//! FALSE — the true property is over the de-duplicated archive SET (mirroring the
//! `link_plan` `retain`, but lifted from the direct deps to the whole closure).
//!
//! The properties mirror the Dafny golden spec (`ValidBucketing`):
//!
//!   1. **P1 ALWAYSLINK-WINS** (over the TRANSITIVE closure) — every archive
//!      alwayslink-declared by ANY reachable node is in `whole_archive` (a deep
//!      transitive declaration still counts — THE production bug);
//!   2. **P2 COMPLETENESS** — every reachable-declared archive is in
//!      `whole_archive ∪ normal`;
//!   3. **P3 DISJOINT** — `whole_archive ∩ normal = ∅` (no double-list);
//!   4. **P4 NO-SPURIOUS** — every bucketed archive is declared by some reachable
//!      node;
//!   5. **P5 NORMAL-NOT-ALWAYSLINK** — no `normal` archive is alwayslink-declared
//!      anywhere reachable.
//!
//! The Creusot contract carries the tractable P3 disjointness conjunct (plus the
//! [`in_whole_archive`] membership predicate it is stated against); the closure-
//! level P1/P2/P4/P5 conjuncts are the heavier fixpoint obligations left to the
//! proptest oracle (`tests/alwayslink_closure_prop.rs`) and the Dafny addendum
//! (`ValidBucketing`) — the same Pearlite-vs-oracle split as `nested_set_walk` /
//! `topo_roots` / `link_plan`.
//!
//! Scope note (matching the Dafny model): the dep DAG is acyclic BY CONSTRUCTION
//! (a child must already exist when its parent is built). We model the acyclic
//! case. The walk is nonetheless TOTAL on any index-DAG: the `visited` set marks
//! each node at most once, so the walk terminates even on a malformed cyclic
//! input (it stops at the first re-encounter of a node).

#[allow(unused_imports)]
use crate::contract_shim::*;
use std::collections::HashSet;

/// One node of the typed linker-input DAG: its own linker `inputs`
/// (`(archive_id, alwayslink)`, declaration order) plus the indices of its
/// `children` (declaration order). Mirrors a dep target's `CcInfo.linker_inputs`
/// + its `deps`/`implementation_deps` edges.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkNode {
    /// This node's own typed linker-inputs: `(archive_id, alwayslink)` pairs in
    /// declaration order. The same `archive_id` may appear twice here (or across
    /// nodes) with DIFFERENT `alwayslink` bits — the OR (alwayslink-wins)
    /// resolves it.
    pub inputs: Vec<(usize, bool)>,
    /// Indices of this node's dependency children, in declaration order.
    pub children: Vec<usize>,
}

impl LinkNode {
    /// Build a node from its typed linker-inputs and child indices.
    pub fn new(inputs: Vec<(usize, bool)>, children: Vec<usize>) -> Self {
        LinkNode { inputs, children }
    }
}

/// A finite DAG over node indices `0..nodes.len()`. `nodes[n]` is node `n`'s
/// [`LinkNode`]; `nodes[n].children` are its dependency out-edges. Models the
/// dependency DAG reachable from a root target, with each node carrying the
/// TYPED linker-input metadata the production transitive sweep currently drops.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkInputDag {
    /// The node table; index = node-ID.
    pub nodes: Vec<LinkNode>,
}

impl LinkInputDag {
    /// Build a DAG from a node table.
    pub fn new(nodes: Vec<LinkNode>) -> Self {
        LinkInputDag { nodes }
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
    /// [`alwayslink_closure`] guards anyway so it is total on a malformed DAG.
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
/// closure properties independently of the bucket walk.
pub fn reachable_nodes(dag: &LinkInputDag, root: usize) -> Vec<usize> {
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

/// Whether `archive` is a member of the `whole_archive` bucket — the membership
/// predicate the Creusot contract's disjointness conjunct is stated against.
pub fn in_whole_archive(whole_archive: &[usize], archive: usize) -> bool {
    whole_archive.contains(&archive)
}

/// Compute the BUCKET PARTITION of every archive reachable from `root` over the
/// typed linker-input DAG, with **alwayslink-wins over the WHOLE transitive
/// closure**.
///
/// Returns `(whole_archive, normal)`:
///   * `whole_archive` — every archive-id declared `alwayslink` by ANY reachable
///     node (the OR over the transitive closure — a deep transitive declaration
///     still counts), de-duplicated, in first-seen declaration order;
///   * `normal` — every reachable-declared archive-id NOT in `whole_archive`,
///     de-duplicated, in first-seen declaration order.
///
/// This is the pure twin the production transitive-sweep bucketing should
/// delegate to (see the crate's wire-up note): it stops the transitive sweep
/// from losing the alwayslink bit. The algorithm:
///
///   * walk the node-DAG with NODE-identity de-dup (each node visited once — the
///     same `visited`-set discipline as `nested_set_walk`/`reachable_nodes`), in
///     declaration order;
///   * for every reachable node, OR each input's `alwayslink` bit into a
///     per-archive accumulator, recording first-seen order;
///   * finally split: an archive goes to `whole_archive` iff its accumulated bit
///     is set, else to `normal`. The split runs AFTER the full walk so a later,
///     deeper alwayslink declaration still promotes an archive first seen plain
///     (the closure analogue of `link_plan`'s `retain`).
///
/// Out-of-bounds `root` → `(empty, empty)`.
///
/// The Creusot contract states the tractable P3 disjointness conjunct: the two
/// output buckets share no archive. The closure-level P1 alwayslink-wins, P2
/// completeness, P4 no-spurious, and P5 normal-not-alwayslink conjuncts are the
/// heavier fixpoint obligations discharged by the proptest oracle and the Dafny
/// addendum (`ValidBucketing`) — the same split as `link_plan`/`nested_set_walk`.
#[cfg_attr(feature = "creusot",
    ensures(buckets_disjoint(result.0@, result.1@)))]
pub fn alwayslink_closure(dag: &LinkInputDag, root: usize) -> (Vec<usize>, Vec<usize>) {
    let n = dag.len();
    if root >= n {
        return (Vec::new(), Vec::new());
    }

    // First-seen archive order + the OR-accumulated alwayslink bit per archive.
    let mut order: Vec<usize> = Vec::new();
    let mut seen_archive: HashSet<usize> = HashSet::new();
    // alwayslink[i] is the accumulated bit for the archive at order[i].
    let mut alwayslink: Vec<bool> = Vec::new();

    // Node walk with identity de-dup (mirrors NestedSetIter's `seen` set):
    // each reachable node contributes its inputs exactly once.
    let mut visited: HashSet<usize> = HashSet::new();
    let mut stack: Vec<usize> = vec![root];
    visited.insert(root);
    while let Some(node) = stack.pop() {
        for &(archive, aw) in &dag.nodes[node].inputs {
            if seen_archive.insert(archive) {
                order.push(archive);
                alwayslink.push(aw);
            } else if aw {
                // OR the bit in: a later (possibly deeper) alwayslink declaration
                // promotes an archive first seen plain — alwayslink-wins over the
                // whole closure (the fix for the transitive-sweep bit loss).
                let pos = order.iter().position(|&a| a == archive).unwrap();
                alwayslink[pos] = true;
            }
        }
        // Schedule unvisited children in REVERSE so first-declared is popped
        // first (declaration order), matching the nested_set_walk discipline.
        for &child in dag.nodes[node].children.iter().rev() {
            if child < n && visited.insert(child) {
                stack.push(child);
            }
        }
    }

    // Split AFTER the full walk, preserving first-seen order within each bucket.
    let mut whole_archive: Vec<usize> = Vec::new();
    let mut normal: Vec<usize> = Vec::new();
    for (i, &archive) in order.iter().enumerate() {
        if alwayslink[i] {
            whole_archive.push(archive);
        } else {
            normal.push(archive);
        }
    }

    proof_assert!(true);
    (whole_archive, normal)
}

/// Logic-level statement that the two output buckets are DISJOINT (P3) for the
/// Creusot contract: no archive appears in both `whole_archive` and `normal`.
/// Defined as a Creusot `logic` fn so the `ensures` on [`alwayslink_closure`]
/// can refer to it; erased entirely in the default build.
///
/// This is the tractable, set-level slice of `ValidBucketing` Pearlite can carry
/// without a reachability fixpoint: there is no element held by both buckets (the
/// multiple-definition guard — exactly the conjunct `link-plan-v1`'s P2 ASSUMED
/// of its already-tagged input). The closure-level P1 alwayslink-wins (over the
/// transitive closure), P2 completeness, P4 no-spurious, and P5
/// normal-not-alwayslink are the heavier obligations left to the proptest oracle
/// (`tests/alwayslink_closure_prop.rs`) and the Dafny addendum.
#[cfg(feature = "creusot")]
#[logic]
fn buckets_disjoint(whole_archive: Seq<usize>, normal: Seq<usize>) -> bool {
    pearlite! {
        forall(|i: Int, j: Int|
            0 <= i && i < whole_archive.len() && 0 <= j && j < normal.len()
                ==> whole_archive[i] != normal[j])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(inputs: &[(usize, bool)], children: &[usize]) -> LinkNode {
        LinkNode::new(inputs.to_vec(), children.to_vec())
    }

    #[test]
    fn empty_dag_root_no_inputs() {
        let dag = LinkInputDag::new(vec![node(&[], &[])]);
        assert_eq!(alwayslink_closure(&dag, 0), (vec![], vec![]));
    }

    #[test]
    fn out_of_bounds_root_is_empty() {
        let dag = LinkInputDag::new(vec![node(&[(1, true)], &[])]);
        assert_eq!(alwayslink_closure(&dag, 5), (vec![], vec![]));
        assert_eq!(reachable_nodes(&dag, 5), Vec::<usize>::new());
    }

    #[test]
    fn direct_inputs_only() {
        // Root declares archive 1 alwayslink, 2 plain.
        let dag = LinkInputDag::new(vec![node(&[(1, true), (2, false)], &[])]);
        let (whole, normal) = alwayslink_closure(&dag, 0);
        assert_eq!(whole, vec![1]);
        assert_eq!(normal, vec![2]);
    }

    #[test]
    fn transitive_alwayslink_is_whole_archived() {
        // THE BUG: archive 9 is alwayslink, declared ONLY by a deep transitive
        // child (node 2), reachable from root via 0 -> 1 -> 2. It MUST land in
        // whole_archive, not normal.
        //   node 0 = root, children [1]
        //   node 1 = mid,  children [2]
        //   node 2 = leaf, inputs [(9, true)]
        let dag = LinkInputDag::new(vec![
            node(&[(1, false)], &[1]),
            node(&[], &[2]),
            node(&[(9, true)], &[]),
        ]);
        let (whole, normal) = alwayslink_closure(&dag, 0);
        assert!(whole.contains(&9), "transitive alwayslink archive 9 must be whole-archived");
        assert!(!normal.contains(&9));
        assert_eq!(normal, vec![1]);
    }

    #[test]
    fn alwayslink_wins_over_plain_across_nodes() {
        // archive 7 declared alwayslink by one reachable node and plain by
        // another → whole_archive only (alwayslink-wins, no double-list).
        //   node 0 = root, inputs [(7, false)], children [1]
        //   node 1 = dep,  inputs [(7, true)]
        let dag = LinkInputDag::new(vec![
            node(&[(7, false)], &[1]),
            node(&[(7, true)], &[]),
        ]);
        let (whole, normal) = alwayslink_closure(&dag, 0);
        assert_eq!(whole, vec![7]);
        assert!(normal.is_empty());
    }

    #[test]
    fn diamond_archive_deduped() {
        // Diamond: both children declare the same plain archive 5. It appears in
        // normal exactly once (identity de-dup over archives).
        //   node 0 = root, children [1, 2]
        //   node 1, node 2 = inputs [(5, false)]
        let dag = LinkInputDag::new(vec![
            node(&[], &[1, 2]),
            node(&[(5, false)], &[]),
            node(&[(5, false)], &[]),
        ]);
        let (whole, normal) = alwayslink_closure(&dag, 0);
        assert!(whole.is_empty());
        assert_eq!(normal, vec![5]);
    }

    #[test]
    fn unreachable_node_excluded() {
        // node 2 is not reachable from root 0; its alwayslink archive 99 must NOT
        // appear in either bucket.
        let dag = LinkInputDag::new(vec![
            node(&[(1, false)], &[1]),
            node(&[], &[]),
            node(&[(99, true)], &[]),
        ]);
        let (whole, normal) = alwayslink_closure(&dag, 0);
        assert!(whole.is_empty());
        assert_eq!(normal, vec![1]);
        assert!(!whole.contains(&99) && !normal.contains(&99));
    }

    #[test]
    fn buckets_are_disjoint() {
        let dag = LinkInputDag::new(vec![
            node(&[(1, true), (2, false)], &[1]),
            node(&[(2, true), (3, false)], &[]),
        ]);
        let (whole, normal) = alwayslink_closure(&dag, 0);
        // archive 2 was plain-then-alwayslink → whole only.
        assert!(whole.contains(&2));
        for a in &whole {
            assert!(!normal.contains(a), "buckets must be disjoint (archive {})", a);
        }
    }

    #[test]
    fn in_bounds_detects_bad_child() {
        let ok = LinkInputDag::new(vec![node(&[], &[1]), node(&[], &[])]);
        assert!(ok.in_bounds());
        let bad = LinkInputDag::new(vec![node(&[], &[9]), node(&[], &[])]);
        assert!(!bad.in_bounds());
    }
}
