// Rush verified-core — whole-archive/normal bucket-CONSTRUCTION ORACLE
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT
//
// spec-owned (oracle): this file is the functional oracle for the
// alwayslink-closure-v1 verified scope. It witnesses the ValidBucketing
// properties P1-P5 from
// verification/dafny/alwayslink_closure/AlwaysLinkClosure.dfy on thousands of
// random acyclic typed linker-input DAGs (archives declared by multiple nodes
// with CONFLICTING alwayslink bits, to exercise alwayslink-wins) AND on the
// golden transitive-alwayslink bug witness, and proves the oracle is
// non-vacuous via deliberately-wrong bucketings it catches — including the
// mutant that LITERALLY reproduces the production bug (honors only direct,
// depth-1 alwayslink and drops transitively-reached alwayslink archives to
// normal). Deleting, #[ignore]-ing, or weakening these assertions is a
// spec-sensitive change (Spec-Change: oracle) and routes through the human
// Spec-Review gate. See ci/spec-owned.tsv.
//
//! Property/golden ORACLE for [`rush_verified_core::alwayslink_closure`] (the
//! whole-archive/normal bucket CONSTRUCTION from the typed linker-input DAG). On
//! every accepted (acyclic, in-bounds) DAG it checks the five conjuncts of the
//! Dafny `ValidBucketing` predicate. Getting ANY wrong is a real link bug — a
//! dropped transitive alwayslink archive is stripped (runtime "no registered
//! op/kernel", THE production bug), a double-listed archive re-extracts members
//! ("multiple definition"), a dropped archive reintroduces undefined symbols:
//!
//!   * **P1 ALWAYSLINK-WINS** (over the TRANSITIVE closure) — every archive
//!     alwayslink-declared by ANY reachable node is in `whole_archive`. The KEY
//!     property: an archive reachable ONLY via a deep transitive path still gets
//!     whole-archived.
//!   * **P2 COMPLETENESS** — every reachable-declared archive is in
//!     `whole_archive ∪ normal`.
//!   * **P3 DISJOINT** — `whole_archive ∩ normal = ∅` (no double-list).
//!   * **P4 NO-SPURIOUS** — every bucketed archive is declared by some reachable
//!     node.
//!   * **P5 NORMAL-NOT-ALWAYSLINK** — no `normal` archive is alwayslink-declared
//!     anywhere reachable.
//!
//! Anti-vacuity is demonstrated three ways (each a deliberately-broken bucketing
//! the oracle CATCHES):
//!   * `mutant_demotes_transitive_alwayslink` — honors only DIRECT (depth-1)
//!     alwayslink, dropping transitively-reached alwayslink archives to normal.
//!     THIS IS LITERALLY THE PRODUCTION BUG → caught by P1.
//!   * `mutant_double_lists` — leaves an alwayslink archive in BOTH sets →
//!     caught by P3.
//!   * `mutant_drops_archive` — omits a reachable archive → caught by P2.

use proptest::prelude::*;
use rush_verified_core::{
    alwayslink_closure, reachable_link_nodes as reachable_nodes, LinkInputDag, LinkNode,
};
use std::collections::HashSet;

// ---------------------------------------------------------------------------
// Closure projections over the INPUT DAG — the abstract sets the (whole, normal)
// partition must realise. These mirror ReachableWholeArchiveSet /
// ReachableArchiveSet / IsAlwayslinkSomewhereReachable from AlwaysLinkClosure.dfy,
// recomputed INDEPENDENTLY here (a flat scan over the reachable node set) so the
// checker does not just re-run the implementation's walk.
// ---------------------------------------------------------------------------

/// Archive-ids alwayslink-declared by ANY reachable node (the OR over the whole
/// transitive closure).
fn reachable_whole_set(dag: &LinkInputDag, root: usize) -> HashSet<usize> {
    let mut set = HashSet::new();
    for n in reachable_nodes(dag, root) {
        for &(archive, aw) in &dag.nodes[n].inputs {
            if aw {
                set.insert(archive);
            }
        }
    }
    set
}

/// All distinct archive-ids declared by any reachable node.
fn reachable_archive_set(dag: &LinkInputDag, root: usize) -> HashSet<usize> {
    let mut set = HashSet::new();
    for n in reachable_nodes(dag, root) {
        for &(archive, _) in &dag.nodes[n].inputs {
            set.insert(archive);
        }
    }
    set
}

/// Assert all five `ValidBucketing` conjuncts of `(whole, normal)` against
/// `(dag, root)`. Returns `Ok(())` if valid, `Err(reason)` otherwise.
fn check_valid_bucketing(
    dag: &LinkInputDag,
    root: usize,
    whole: &[usize],
    normal: &[usize],
) -> Result<(), String> {
    let whole_set: HashSet<usize> = whole.iter().copied().collect();
    let normal_set: HashSet<usize> = normal.iter().copied().collect();
    let reach_whole = reachable_whole_set(dag, root);
    let reach_all = reachable_archive_set(dag, root);

    // P1 ALWAYSLINK-WINS (over the transitive closure): every alwayslink-reachable
    // archive is in whole_archive.
    for &a in &reach_whole {
        if !whole_set.contains(&a) {
            return Err(format!(
                "P1: alwayslink-reachable archive {} missing from whole_archive {:?}",
                a, whole
            ));
        }
    }

    // P2 COMPLETENESS: every reachable-declared archive is in whole ∪ normal.
    for &a in &reach_all {
        if !whole_set.contains(&a) && !normal_set.contains(&a) {
            return Err(format!("P2: reachable archive {} dropped from both buckets", a));
        }
    }

    // P3 DISJOINT: no archive in both buckets.
    for &a in whole {
        if normal_set.contains(&a) {
            return Err(format!("P3: archive {} double-listed (in both buckets)", a));
        }
    }

    // P4 NO-SPURIOUS: every bucketed archive is reachable-declared.
    for &a in whole.iter().chain(normal.iter()) {
        if !reach_all.contains(&a) {
            return Err(format!("P4: spurious archive {} not declared by any reachable node", a));
        }
    }

    // P5 NORMAL-NOT-ALWAYSLINK: no normal archive is alwayslink anywhere reachable.
    for &a in normal {
        if reach_whole.contains(&a) {
            return Err(format!(
                "P5: normal archive {} IS alwayslink-declared somewhere reachable",
                a
            ));
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Golden / specific-witness cases (always run, no randomness).
// ---------------------------------------------------------------------------

fn node(inputs: &[(usize, bool)], children: &[usize]) -> LinkNode {
    LinkNode::new(inputs.to_vec(), children.to_vec())
}

/// THE BUG, as a positive witness: an alwayslink archive (a REGISTER_KERNEL
/// static-initializer archive like ShuffleDataset) reachable ONLY via a deep
/// transitive child chain MUST be in whole_archive — never normal. This is the
/// exact shape that, under the production transitive-sweep, was dropped to
/// normal and stripped by the linker.
#[test]
fn witness_transitive_alwayslink_whole_archived() {
    // root(0) -> mid(1) -> leaf(2); leaf declares archive 9 alwayslink.
    let dag = LinkInputDag::new(vec![
        node(&[(1, false)], &[1]),
        node(&[], &[2]),
        node(&[(9, true)], &[]),
    ]);
    let (whole, normal) = alwayslink_closure(&dag, 0);
    check_valid_bucketing(&dag, 0, &whole, &normal).unwrap();
    assert!(whole.contains(&9), "deep transitive alwayslink archive 9 must be whole-archived");
    assert!(!normal.contains(&9));
}

/// An archive declared alwayslink by one node and plain by another → whole only
/// (alwayslink-wins, no double-list — the closure analogue of link_plan's
/// retain).
#[test]
fn witness_conflicting_bits_alwayslink_wins() {
    let dag = LinkInputDag::new(vec![
        node(&[(7, false)], &[1]),
        node(&[(7, true)], &[]),
    ]);
    let (whole, normal) = alwayslink_closure(&dag, 0);
    check_valid_bucketing(&dag, 0, &whole, &normal).unwrap();
    assert_eq!(whole, vec![7]);
    assert!(normal.is_empty());
}

// ---------------------------------------------------------------------------
// ANTI-VACUITY: deliberately-wrong bucketings the oracle MUST catch.
// ---------------------------------------------------------------------------

/// MUTANT A — THE PRODUCTION BUG. A bucketing that honors alwayslink ONLY for
/// the DIRECT (depth-1) deps of `root`, sweeping every OTHER reachable archive
/// into normal UNCONDITIONALLY (the alwayslink bit does not ride past depth-1).
/// This is exactly `CcTestInstance`'s direct-CcInfo + transitive-artifact-sweep
/// split. Caught by P1.
fn mutant_demotes_transitive_alwayslink(
    dag: &LinkInputDag,
    root: usize,
) -> (Vec<usize>, Vec<usize>) {
    let n = dag.len();
    let mut whole: Vec<usize> = Vec::new();
    let mut normal: Vec<usize> = Vec::new();
    let mut seen: HashSet<usize> = HashSet::new();
    if root >= n {
        return (whole, normal);
    }
    // Direct (depth-1) inputs of root honor alwayslink.
    let direct_children: HashSet<usize> = if root < n {
        dag.nodes[root].children.iter().copied().collect()
    } else {
        HashSet::new()
    };
    for &(archive, aw) in &dag.nodes[root].inputs {
        if seen.insert(archive) {
            if aw {
                whole.push(archive);
            } else {
                normal.push(archive);
            }
        }
    }
    // Everything else reachable → normal bucket UNCONDITIONALLY (the bug).
    for node in reachable_nodes(dag, root) {
        if node == root {
            continue;
        }
        // Even the direct children's archives go to normal here unless we are at
        // depth-1; to keep the mutant honest we only spare the root's own inputs.
        let _ = direct_children;
        for &(archive, _aw) in &dag.nodes[node].inputs {
            if seen.insert(archive) {
                normal.push(archive); // alwayslink bit DROPPED
            }
        }
    }
    (whole, normal)
}

#[test]
fn mutant_demotes_transitive_alwayslink_caught() {
    // Deep transitive alwayslink archive 9 — the mutant drops it to normal.
    let dag = LinkInputDag::new(vec![
        node(&[(1, false)], &[1]),
        node(&[], &[2]),
        node(&[(9, true)], &[]),
    ]);
    let (whole, normal) = mutant_demotes_transitive_alwayslink(&dag, 0);
    assert!(
        normal.contains(&9) && !whole.contains(&9),
        "mutant should have demoted transitive alwayslink archive 9 to normal"
    );
    assert!(
        check_valid_bucketing(&dag, 0, &whole, &normal).is_err(),
        "oracle went vacuous: accepted the PRODUCTION BUG (transitive alwayslink \
         archive dropped to the normal bucket → linker strips its initializers)"
    );
    // The verified kernel whole-archives it.
    let (gw, gn) = alwayslink_closure(&dag, 0);
    check_valid_bucketing(&dag, 0, &gw, &gn).unwrap();
    assert!(gw.contains(&9));
}

/// MUTANT B: leaves an alwayslink archive in BOTH sets (skips the disjointness
/// split). Caught by P3.
fn mutant_double_lists(dag: &LinkInputDag, root: usize) -> (Vec<usize>, Vec<usize>) {
    let (whole, mut normal) = alwayslink_closure(dag, root);
    // BUG: also push every whole-archived archive back into normal.
    for &a in &whole {
        if !normal.contains(&a) {
            normal.push(a);
        }
    }
    (whole, normal)
}

#[test]
fn mutant_double_lists_caught() {
    let dag = LinkInputDag::new(vec![node(&[(3, true), (4, false)], &[])]);
    let (whole, normal) = mutant_double_lists(&dag, 0);
    assert!(
        whole.contains(&3) && normal.contains(&3),
        "mutant should have double-listed archive 3"
    );
    assert!(
        check_valid_bucketing(&dag, 0, &whole, &normal).is_err(),
        "oracle went vacuous: accepted an alwayslink archive double-listed in both buckets"
    );
    let (gw, gn) = alwayslink_closure(&dag, 0);
    check_valid_bucketing(&dag, 0, &gw, &gn).unwrap();
}

/// MUTANT C: omits a reachable archive entirely (drops it from both buckets).
/// Caught by P2.
fn mutant_drops_archive(dag: &LinkInputDag, root: usize, drop: usize) -> (Vec<usize>, Vec<usize>) {
    let (mut whole, mut normal) = alwayslink_closure(dag, root);
    whole.retain(|&a| a != drop);
    normal.retain(|&a| a != drop);
    (whole, normal)
}

#[test]
fn mutant_drops_archive_caught() {
    let dag = LinkInputDag::new(vec![node(&[(5, false), (6, false)], &[])]);
    let (whole, normal) = mutant_drops_archive(&dag, 0, 6);
    assert!(!whole.contains(&6) && !normal.contains(&6), "mutant should have dropped archive 6");
    assert!(
        check_valid_bucketing(&dag, 0, &whole, &normal).is_err(),
        "oracle went vacuous: accepted a bucketing that drops a reachable archive"
    );
    let (gw, gn) = alwayslink_closure(&dag, 0);
    check_valid_bucketing(&dag, 0, &gw, &gn).unwrap();
}

// ---------------------------------------------------------------------------
// Property: random ACYCLIC typed linker-input DAGs always yield a valid
// bucketing.
// ---------------------------------------------------------------------------

/// Generate a random ACYCLIC DAG (and a root) over `n` nodes by only allowing
/// child edges `n -> c` with `c > n` (upper-triangular ⇒ acyclic, and creates
/// diamonds so the same archive is reached through multiple paths). Archive-ids
/// are drawn from a SMALL pool so the SAME archive is frequently declared by
/// several nodes with CONFLICTING alwayslink bits — forcing the alwayslink-wins
/// OR over the transitive closure (P1/P5).
fn acyclic_dag_and_root() -> impl Strategy<Value = (LinkInputDag, usize)> {
    (1usize..=10).prop_flat_map(|n| {
        let rows: Vec<_> = (0..n)
            .map(|i| {
                // inputs: 0..=4 typed (archive_id in 0..8, alwayslink bool) pairs.
                let inputs = prop::collection::vec((0usize..8, any::<bool>()), 0..=4);
                let candidates: Vec<usize> = (i + 1..n).collect();
                let children =
                    proptest::sample::subsequence(candidates.clone(), 0..=candidates.len());
                (inputs, children)
            })
            .collect();
        let root = 0usize..n;
        (rows, root).prop_map(|(rows, root)| {
            let nodes = rows
                .into_iter()
                .map(|(inputs, children)| LinkNode::new(inputs, children))
                .collect();
            (LinkInputDag::new(nodes), root)
        })
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3000))]

    /// On any acyclic in-bounds DAG, the bucket closure satisfies all five
    /// ValidBucketing conjuncts (P1-P5).
    #[test]
    fn acyclic_yields_valid_bucketing((dag, root) in acyclic_dag_and_root()) {
        prop_assert!(dag.in_bounds(), "generator must emit in-bounds DAGs");
        let (whole, normal) = alwayslink_closure(&dag, root);
        if let Err(why) = check_valid_bucketing(&dag, root, &whole, &normal) {
            prop_assert!(false, "ValidBucketing violated: {}", why);
        }
    }

    /// The whole-archive bucket EQUALS the alwayslink-reachable set, and the
    /// normal bucket EQUALS the rest — stated independently of the checker.
    /// (P1 + P5 together pin the whole bucket exactly; P2 + P3 pin the normal
    /// bucket exactly.)
    #[test]
    fn buckets_equal_closure_projections((dag, root) in acyclic_dag_and_root()) {
        let (whole, normal) = alwayslink_closure(&dag, root);
        let whole_set: HashSet<usize> = whole.iter().copied().collect();
        let normal_set: HashSet<usize> = normal.iter().copied().collect();
        let reach_whole = reachable_whole_set(&dag, root);
        let reach_all = reachable_archive_set(&dag, root);
        let reach_normal: HashSet<usize> =
            reach_all.difference(&reach_whole).copied().collect();
        prop_assert_eq!(&whole_set, &reach_whole, "whole bucket != alwayslink-reachable set");
        prop_assert_eq!(&normal_set, &reach_normal, "normal bucket != reachable-minus-alwayslink set");
        // No archive listed twice within a bucket (identity de-dup).
        prop_assert_eq!(whole.len(), whole_set.len(), "whole bucket has a duplicate");
        prop_assert_eq!(normal.len(), normal_set.len(), "normal bucket has a duplicate");
    }

    /// IDEMPOTENT determinism: the closure is a pure function of (dag, root).
    #[test]
    fn closure_is_deterministic((dag, root) in acyclic_dag_and_root()) {
        prop_assert_eq!(alwayslink_closure(&dag, root), alwayslink_closure(&dag, root));
    }
}
