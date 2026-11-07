// Rush verified-core — reachable-subgraph cycle-detection ORACLE
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT
//
// spec-owned (oracle): this file is the functional oracle for the
// cycle-detect-v1 verified scope. It witnesses the IsValidCycle /
// HasCycleInReachable properties from
// verification/dafny/cycle_detect/CycleDetect.dfy on thousands of random
// graphs, cross-checks the detector against the topo-roots-v1 kernel (the two
// are exact complements over the reachable subgraph), and proves the oracle is
// non-vacuous via TWO deliberately-wrong detector variants it catches.
// Deleting, #[ignore]-ing, or weakening these assertions is a spec-sensitive
// change (Spec-Change: oracle) and routes through the human Spec-Review gate.
// See ci/spec-owned.tsv.
//
//! Property/golden ORACLE for `detect_cycle`. It checks, on random graphs:
//!
//!   1. **acyclic agreement** — on upper-triangular (acyclic-by-construction)
//!      graphs, `detect_cycle == None` AND
//!      `topological_sort_from_roots == Some` (the two kernels AGREE);
//!   2. **witness validity** — on graphs with a reachable back-edge,
//!      `detect_cycle == Some(c)` where `c` is a genuine closing cycle
//!      (consecutive edges + last-edges-to-first), all nodes reachable;
//!   3. **scope match** — a cycle UNREACHABLE from roots is NOT reported; and
//!   4. **topo↔cycle complement** — over ALL generated graphs,
//!      `detect_cycle(g,r).is_none() == topological_sort_from_roots(g,r).is_some()`
//!      (binds the two sibling kernels — the high-value cross-check).
//!
//! Anti-vacuity is demonstrated TWO ways the prompt requires:
//!   * (a) `mutant_returns_none_on_real_cycle` — a detector that returns `None`
//!     on a real reachable cycle is caught by the cyclic-graph property; and
//!   * (b) `mutant_returns_non_cycle` — a detector that returns a non-cycle
//!     sequence is caught by `is_valid_cycle`.

use proptest::prelude::*;
use rush_verified_core::{detect_cycle, reachable, topological_sort_from_roots, IndexGraph};
use std::collections::HashSet;

// ---------------------------------------------------------------------------
// IsValidCycle checker (the oracle proper) — test-side twin of the Dafny
// `IsValidCycle(g, roots, cyc)` predicate.
// ---------------------------------------------------------------------------

/// `Ok(())` iff `cyc` is a valid witness cycle within the subgraph reachable
/// from `roots`: non-empty; every entry in-bounds AND reachable; consecutive
/// entries are edges; last entry edges back to the first.
fn is_valid_cycle(graph: &IndexGraph, roots: &[usize], cyc: &[usize]) -> Result<(), String> {
    // (1) non-empty
    if cyc.is_empty() {
        return Err("witness cycle is empty".to_string());
    }
    let n = graph.len();
    let reach: HashSet<usize> = reachable(graph, roots).into_iter().collect();

    // (2) in-bounds AND reachable
    for &x in cyc {
        if x >= n {
            return Err(format!("cycle node {} out of bounds (n={})", x, n));
        }
        if !reach.contains(&x) {
            return Err(format!("cycle node {} not reachable from roots", x));
        }
    }

    // (3) consecutive entries are edges cyc[i] -> cyc[i+1]
    for w in cyc.windows(2) {
        if !graph.adjacency[w[0]].contains(&w[1]) {
            return Err(format!("no edge {} -> {} in cycle", w[0], w[1]));
        }
    }

    // (4) last edges back to first
    let last = cyc[cyc.len() - 1];
    if !graph.adjacency[last].contains(&cyc[0]) {
        return Err(format!(
            "cycle does not close: no edge {} -> {}",
            last, cyc[0]
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Golden / specific-witness cases (always run, no randomness).
// ---------------------------------------------------------------------------

#[test]
fn witness_self_loop() {
    // 0 -> 0.
    let g = IndexGraph::new(vec![vec![0]]);
    let cyc = detect_cycle(&g, &[0]).expect("self-loop reported");
    is_valid_cycle(&g, &[0], &cyc).unwrap();
    assert_eq!(cyc, vec![0]);
}

#[test]
fn witness_two_cycle() {
    // 0 -> 1 -> 0.
    let g = IndexGraph::new(vec![vec![1], vec![0]]);
    let cyc = detect_cycle(&g, &[0]).expect("2-cycle reported");
    is_valid_cycle(&g, &[0], &cyc).unwrap();
    assert_eq!(cyc.len(), 2);
}

#[test]
fn witness_three_cycle() {
    // 0 -> 1 -> 2 -> 0.
    let g = IndexGraph::new(vec![vec![1], vec![2], vec![0]]);
    let cyc = detect_cycle(&g, &[0]).expect("3-cycle reported");
    is_valid_cycle(&g, &[0], &cyc).unwrap();
    assert_eq!(cyc.len(), 3);
}

#[test]
fn witness_cycle_behind_unreached_node() {
    // Root 0 has no deps; the 1<->2 cycle is unreachable from it.
    let g = IndexGraph::new(vec![vec![], vec![2], vec![1]]);
    assert_eq!(detect_cycle(&g, &[0]), None);
    // ...and the kernels agree the acyclic-reachable scope sorts fine.
    assert!(topological_sort_from_roots(&g, &[0]).is_some());
}

// ---------------------------------------------------------------------------
// ANTI-VACUITY mutant (a): a detector that returns None on a real reachable
// cycle. The cyclic-graph property must catch it.
// ---------------------------------------------------------------------------

/// Mutant detector that NEVER reports a cycle. On a graph with a reachable
/// cycle, the oracle's "must be Some" assertion fails — proving the property is
/// not vacuously satisfiable by a do-nothing detector.
fn mutant_always_none(_graph: &IndexGraph, _roots: &[usize]) -> Option<Vec<usize>> {
    None
}

#[test]
fn mutant_returns_none_on_real_cycle() {
    // 0 -> 1 -> 0 is a reachable cycle; the real detector reports it.
    let g = IndexGraph::new(vec![vec![1], vec![0]]);
    assert!(
        detect_cycle(&g, &[0]).is_some(),
        "real detector must report the reachable cycle"
    );
    // The mutant does NOT — and the oracle's positive-side check catches it.
    assert!(
        mutant_always_none(&g, &[0]).is_none(),
        "mutant is the always-None detector"
    );
    // This is exactly the assertion `cyclic_graph_reports_valid_witness` makes;
    // run it against the mutant to show non-vacuity.
    let verdict = mutant_always_none(&g, &[0]);
    assert!(
        verdict.is_none(),
        "sanity: mutant returned a value it shouldn't"
    );
    // The oracle would FAIL here (expects Some) — demonstrate the gap:
    assert_ne!(
        verdict.is_some(),
        detect_cycle(&g, &[0]).is_some(),
        "mutant disagrees with the real detector on a real cycle (oracle catches it)"
    );
}

// ---------------------------------------------------------------------------
// ANTI-VACUITY mutant (b): a detector that returns a non-cycle sequence.
// is_valid_cycle must reject it.
// ---------------------------------------------------------------------------

/// Mutant detector that returns the reachable set verbatim (NOT a closing
/// cycle in general). `is_valid_cycle` must reject it on at least one graph.
fn mutant_returns_reachable(graph: &IndexGraph, roots: &[usize]) -> Option<Vec<usize>> {
    let r = reachable(graph, roots);
    if r.is_empty() {
        None
    } else {
        Some(r)
    }
}

#[test]
fn mutant_returns_non_cycle() {
    // 0 -> 1 -> 2 -> 0 reachable from 0; reachable set is {0,1,2} but in sorted
    // order [0,1,2] the closing edge 2 -> 0 holds yet consecutive 0 -> 1, 1 ->
    // 2 also hold — so use a graph where the reachable set is NOT a closing
    // cycle. 0 -> 1, 1 -> 0 (cycle) plus 0 -> 2 (acyclic spur): reachable set
    // {0,1,2}; the sequence [0,1,2] has no edge 1 -> 2, so it is NOT a cycle.
    let g = IndexGraph::new(vec![vec![1, 2], vec![0], vec![]]);
    let bogus = mutant_returns_reachable(&g, &[0]).expect("non-empty reachable");
    let mut sorted = bogus.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, vec![0, 1, 2]);
    assert!(
        is_valid_cycle(&g, &[0], &bogus).is_err(),
        "oracle went vacuous: accepted a non-cycle sequence {:?}",
        bogus
    );
    // And the REAL detector's witness IS accepted, so the oracle isn't
    // rejecting everything.
    let good = detect_cycle(&g, &[0]).expect("real cycle present");
    is_valid_cycle(&g, &[0], &good).unwrap();
}

// ---------------------------------------------------------------------------
// Generators.
// ---------------------------------------------------------------------------

/// Random ACYCLIC graph (upper-triangular: edges `n -> d` only with `d > n`)
/// plus a root set — identical construction to `topo_roots_prop`.
fn acyclic_graph_and_roots() -> impl Strategy<Value = (IndexGraph, Vec<usize>)> {
    (1usize..=10).prop_flat_map(|n| {
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

/// Random graph with a single injected back-edge that is REACHABLE from the
/// chosen root. Built as a chain 0 -> 1 -> ... -> (n-1) (so node 0 reaches all)
/// plus a back-edge `hi -> lo` with `lo < hi`, which closes a cycle that node 0
/// reaches. Root is always 0.
fn cyclic_reachable_graph() -> impl Strategy<Value = (IndexGraph, Vec<usize>)> {
    (2usize..=10).prop_flat_map(|n| {
        // Pick lo < hi for the back-edge hi -> lo.
        (0usize..n - 1).prop_flat_map(move |lo| {
            (lo + 1..n).prop_map(move |hi| {
                let mut adjacency: Vec<Vec<usize>> = (0..n)
                    .map(|i| if i + 1 < n { vec![i + 1] } else { vec![] })
                    .collect();
                adjacency[hi].push(lo); // back-edge closes a cycle lo..hi
                (IndexGraph::new(adjacency), vec![0usize])
            })
        })
    })
}

/// Random graph whose ONLY cycle sits behind an unreachable node. Node 0 is the
/// root with NO outgoing edges; the cycle (a 2-cycle hi<->lo among nodes >= 1)
/// is therefore unreachable from 0.
fn cycle_unreachable_graph() -> impl Strategy<Value = (IndexGraph, Vec<usize>)> {
    (3usize..=10).prop_map(|n| {
        // Nodes 1..n form a chain 1 -> 2 -> ... -> (n-1) plus a back-edge
        // (n-1) -> 1, all UNREACHABLE from root 0 (which has no deps).
        let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (i, row) in adjacency.iter_mut().enumerate().take(n - 1).skip(1) {
            row.push(i + 1);
        }
        adjacency[n - 1].push(1); // back-edge: cycle among 1..n
        (IndexGraph::new(adjacency), vec![0usize])
    })
}

// ---------------------------------------------------------------------------
// Properties.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(3000))]

    /// (1) ACYCLIC AGREEMENT: on an acyclic graph the detector reports None and
    /// the topo sort reports Some — the two kernels agree.
    #[test]
    fn acyclic_no_cycle_and_topo_some((graph, roots) in acyclic_graph_and_roots()) {
        prop_assert!(graph.in_bounds(), "generator must emit in-bounds graphs");
        prop_assert_eq!(
            detect_cycle(&graph, &roots),
            None,
            "acyclic graph must report no cycle: {:?}",
            graph
        );
        prop_assert!(
            topological_sort_from_roots(&graph, &roots).is_some(),
            "acyclic graph must topo-sort: {:?}",
            graph
        );
    }

    /// (4) TOPO↔CYCLE COMPLEMENT (the high-value cross-check): over the acyclic
    /// family, no-cycle iff sorts. Stated as the biconditional the prompt
    /// requires, binding the two sibling kernels.
    #[test]
    fn complement_holds_acyclic((graph, roots) in acyclic_graph_and_roots()) {
        prop_assert_eq!(
            detect_cycle(&graph, &roots).is_none(),
            topological_sort_from_roots(&graph, &roots).is_some(),
            "topo↔cycle complement violated on {:?}",
            graph
        );
    }

    /// (2) WITNESS VALIDITY: on a graph with a reachable back-edge the detector
    /// reports Some(c) where c is a genuine closing cycle, AND the topo sort
    /// reports None (complement). Binds the cross-check on the cyclic side.
    #[test]
    fn cyclic_graph_reports_valid_witness((graph, roots) in cyclic_reachable_graph()) {
        let cyc = detect_cycle(&graph, &roots);
        prop_assert!(cyc.is_some(), "reachable cycle must be reported: {:?}", graph);
        let cyc = cyc.unwrap();
        if let Err(why) = is_valid_cycle(&graph, &roots, &cyc) {
            prop_assert!(false, "IsValidCycle violated: {} (cyc={:?})", why, cyc);
        }
        // Complement: a reachable cycle blocks the topo sort.
        prop_assert!(
            topological_sort_from_roots(&graph, &roots).is_none(),
            "cyclic reachable graph must NOT topo-sort: {:?}",
            graph
        );
        prop_assert_eq!(
            detect_cycle(&graph, &roots).is_none(),
            topological_sort_from_roots(&graph, &roots).is_some(),
            "topo↔cycle complement violated on cyclic {:?}",
            graph
        );
    }

    /// (3) SCOPE MATCH: a cycle entirely behind an unreachable node is NOT
    /// reported, and the reachable scope still topo-sorts.
    #[test]
    fn unreachable_cycle_not_reported((graph, roots) in cycle_unreachable_graph()) {
        prop_assert_eq!(
            detect_cycle(&graph, &roots),
            None,
            "unreachable cycle must NOT be reported: {:?}",
            graph
        );
        prop_assert!(
            topological_sort_from_roots(&graph, &roots).is_some(),
            "reachable scope (just the root) must topo-sort: {:?}",
            graph
        );
    }
}
