// Rush verified-core — include-path (`-I`) dedup ORACLE
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT
//
// spec-owned (oracle): this file is the functional oracle for the
// include-dedup-v1 verified scope. It witnesses the search-order-preserving
// de-dup invariants on thousands of random (anchor, deps) include-flag vectors
// AND on golden witnesses (pinned-Abseil ordering, repeated dep roots), and
// proves the oracle is non-vacuous via deliberately-wrong de-dup variants it
// catches. Deleting, #[ignore]-ing, or weakening these assertions is a
// spec-sensitive change (Spec-Change: oracle) and routes through the human
// Spec-Review gate. See ci/spec-owned.tsv.
//
//! Property/golden ORACLE for
//! [`rush_verified_core::dedup_dep_include_flags`] /
//! [`rush_verified_core::normalized_include_sequence`].
//!
//! `-I` search order is significant: gcc/clang resolve a header against the
//! FIRST `-I` root that contains it, so a wrong de-dup either drops a needed
//! root (header-not-found / wrong header) or reorders the surviving roots
//! (a different root wins → wrong/ABI-incompatible header). The oracle checks
//! the real invariants on the full ordered sequence `anchor ++ kept_deps`:
//!
//!   1. **kept-tail subsequence** — the kept dep tail is an order-preserving
//!      subsequence of the dep input (no reordering, no invented tokens);
//!   2. **anchor verbatim** — the full sequence begins with the anchor exactly,
//!      element-for-element, multiplicity included (production does NOT
//!      self-dedup the anchor — a duplicated direct/linux-extra root survives);
//!   3. **dep-root at-most-once across phases** — no dep root that was already
//!      present in the anchor survives in the tail, and no dep root appears
//!      twice in the tail (first-occurrence-wins de-dup);
//!   4. **completeness** — every DISTINCT include root present in the input
//!      (anchor ∪ deps) is present in the output (no `-I` dropped);
//!   5. **first-occurrence / search-order preservation** — the position at
//!      which each distinct root first appears in the input is the position at
//!      which it appears in the output (the kept copy is the FIRST occurrence;
//!      relative order of distinct roots is preserved).
//!
//! Anti-vacuity is demonstrated two ways (each a deliberately-broken de-dup the
//! oracle CATCHES):
//!   * `mutant_last_occurrence_wins_caught` — a variant that keeps the LAST
//!     occurrence of a repeated dep root is caught by the first-occurrence /
//!     search-order property (5);
//!   * `mutant_sorts_output_caught` — a variant that sorts the output is caught
//!     by the kept-tail-subsequence (1) / first-occurrence (5) properties.

use proptest::prelude::*;
use rush_verified_core::{dedup_dep_include_flags, is_include_flag, normalized_include_sequence};

// ---------------------------------------------------------------------------
// Oracle checkers (shared by golden + property tests). Each returns
// Result<(), String> so proptest can report the failing case.
// ---------------------------------------------------------------------------

/// (1) the kept dep tail is an order-preserving subsequence of `deps`: walking
/// `deps` once left-to-right must consume all of `kept` in order.
fn check_kept_subsequence(deps: &[String], kept: &[String]) -> Result<(), String> {
    let mut it = deps.iter();
    for tok in kept {
        let mut found = false;
        for cand in it.by_ref() {
            if cand == tok {
                found = true;
                break;
            }
        }
        if !found {
            return Err(format!(
                "kept dep token {:?} is not a subsequence of deps (order broken or invented token)",
                tok
            ));
        }
    }
    Ok(())
}

/// (2) the full sequence begins with the anchor verbatim (element-for-element,
/// multiplicity included — the anchor is NOT self-deduped).
fn check_anchor_verbatim(anchor: &[String], seq: &[String]) -> Result<(), String> {
    if seq.len() < anchor.len() {
        return Err(format!(
            "output ({} tokens) shorter than anchor ({} tokens)",
            seq.len(),
            anchor.len()
        ));
    }
    if &seq[..anchor.len()] != anchor {
        return Err(format!(
            "output prefix {:?} does not equal anchor verbatim {:?}",
            &seq[..anchor.len()],
            anchor
        ));
    }
    Ok(())
}

/// (3) cross-phase at-most-once for dep roots: no kept dep root was already in
/// the anchor, and no kept dep root repeats.
fn check_dep_at_most_once(anchor: &[String], kept: &[String]) -> Result<(), String> {
    use std::collections::HashSet;
    let anchor_set: HashSet<&String> = anchor.iter().collect();
    let mut seen: HashSet<&String> = HashSet::new();
    for tok in kept {
        if anchor_set.contains(tok) {
            return Err(format!(
                "kept dep root {:?} was already present in the anchor (should have deduped)",
                tok
            ));
        }
        if !seen.insert(tok) {
            return Err(format!("kept dep root {:?} appears more than once in the tail", tok));
        }
    }
    Ok(())
}

/// (4) completeness: every DISTINCT include root in the input (anchor ∪ deps)
/// survives in the full output sequence. Dropping an `-I` root is a real bug
/// (header-not-found / silent wrong-header resolution).
fn check_completeness(anchor: &[String], deps: &[String], seq: &[String]) -> Result<(), String> {
    use std::collections::HashSet;
    let out_set: HashSet<&String> = seq.iter().collect();
    for tok in anchor.iter().chain(deps.iter()) {
        if !out_set.contains(tok) {
            return Err(format!("include root {:?} present in input was dropped from output", tok));
        }
    }
    Ok(())
}

/// (5) first-occurrence / search-order preservation. For each DISTINCT token,
/// the order in which distinct tokens FIRST appear in the input (anchor then
/// deps) must equal the order in which they FIRST appear in the output. This
/// pins "the kept copy is the first occurrence" AND "relative search order of
/// distinct roots is preserved" — the property `-I` semantics actually depend
/// on.
fn check_first_occurrence_order(
    anchor: &[String],
    deps: &[String],
    seq: &[String],
) -> Result<(), String> {
    use std::collections::HashSet;
    // Distinct tokens in input first-appearance order.
    let mut seen_in: HashSet<String> = HashSet::new();
    let mut input_order: Vec<String> = Vec::new();
    for tok in anchor.iter().chain(deps.iter()) {
        if seen_in.insert(tok.clone()) {
            input_order.push(tok.clone());
        }
    }
    // Distinct tokens in output first-appearance order.
    let mut seen_out: HashSet<String> = HashSet::new();
    let mut output_order: Vec<String> = Vec::new();
    for tok in seq {
        if seen_out.insert(tok.clone()) {
            output_order.push(tok.clone());
        }
    }
    if input_order != output_order {
        return Err(format!(
            "distinct-root first-occurrence order changed: input {:?} != output {:?} \
             (search order / first-occurrence violated)",
            input_order, output_order
        ));
    }
    Ok(())
}

/// Run every invariant against the kernel output; returns the first failure.
fn check_all(anchor: &[String], deps: &[String]) -> Result<(), String> {
    let kept = dedup_dep_include_flags(anchor.to_vec(), deps.to_vec());
    let seq = normalized_include_sequence(anchor.to_vec(), deps.to_vec());
    // Internal consistency: normalized_include_sequence == anchor ++ kept.
    {
        let mut expect = anchor.to_vec();
        expect.extend(kept.iter().cloned());
        if seq != expect {
            return Err(format!(
                "normalized_include_sequence {:?} != anchor ++ kept {:?}",
                seq, expect
            ));
        }
    }
    check_kept_subsequence(deps, &kept)?;
    check_anchor_verbatim(anchor, &seq)?;
    check_dep_at_most_once(anchor, &kept)?;
    check_completeness(anchor, deps, &seq)?;
    check_first_occurrence_order(anchor, deps, &seq)?;
    Ok(())
}

/// Same battery of invariants, but checked against an ARBITRARY candidate
/// output sequence (used by the anti-vacuity mutants, which produce their own
/// full sequence rather than going through the kernel). `kept` is the candidate
/// tail (output with the anchor prefix stripped).
fn check_all_candidate(
    anchor: &[String],
    deps: &[String],
    seq: &[String],
) -> Result<(), String> {
    check_anchor_verbatim(anchor, seq)?;
    let kept = &seq[anchor.len().min(seq.len())..];
    check_kept_subsequence(deps, kept)?;
    check_dep_at_most_once(anchor, kept)?;
    check_completeness(anchor, deps, seq)?;
    check_first_occurrence_order(anchor, deps, seq)?;
    Ok(())
}

fn v(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

// ---------------------------------------------------------------------------
// Golden / specific-witness cases (always run, no randomness).
// ---------------------------------------------------------------------------

/// The pinned-Abseil ordering hazard in miniature: a dep root that repeats a
/// root already on the anchor must NOT be re-emitted (the anchor copy, earlier
/// in search order, wins). Re-emitting it later is harmless for resolution but
/// re-emitting it EARLIER would flip search order — the de-dup keeps the first.
#[test]
fn witness_dep_repeat_of_anchor_root_dropped() {
    let anchor = v(&["-I/data/rush-cache/external/com_google_absl", "-Itensorflow"]);
    let deps = v(&[
        "-Itensorflow",                                   // already in anchor → drop
        "-Ibazel-out/k8/bin",                             // new → keep
        "-I/data/rush-cache/external/com_google_absl",    // already in anchor → drop
        "-Ibazel-out/k8/bin",                             // dup of earlier dep → drop
    ]);
    check_all(&anchor, &deps).unwrap();
    let seq = normalized_include_sequence(anchor.clone(), deps);
    assert_eq!(
        seq,
        v(&[
            "-I/data/rush-cache/external/com_google_absl",
            "-Itensorflow",
            "-Ibazel-out/k8/bin",
        ])
    );
}

/// REAL-SEMANTICS GOLDEN: the anchor is pushed verbatim, so a root duplicated
/// WITHIN the anchor survives twice. A naive flat dedup over the whole stream
/// would wrongly collapse it. The dep phase still dedups against (either copy
/// of) that root.
#[test]
fn witness_anchor_internal_duplicate_survives() {
    let anchor = v(&["-I/dup", "-I/dup", "-I/other"]);
    let deps = v(&["-I/dup", "-I/fresh"]);
    let seq = normalized_include_sequence(anchor.clone(), deps.clone());
    assert_eq!(seq, v(&["-I/dup", "-I/dup", "-I/other", "-I/fresh"]));
    // The oracle's anchor-verbatim + completeness invariants ACCEPT this …
    check_all(&anchor, &deps).unwrap();
}

/// Empty dep list keeps nothing; the sequence is exactly the anchor.
#[test]
fn witness_empty_deps() {
    let anchor = v(&["-I/a", "-I/b"]);
    let seq = normalized_include_sequence(anchor.clone(), Vec::new());
    assert_eq!(seq, anchor);
    check_all(&anchor, &[]).unwrap();
}

// ---------------------------------------------------------------------------
// ANTI-VACUITY: deliberately-wrong de-dup variants the oracle MUST catch.
// ---------------------------------------------------------------------------

/// MUTANT A: keeps the LAST occurrence of a repeated dep root instead of the
/// first (it pushes every dep, then retains the last copy of each). This flips
/// search order for a header present in two roots. Caught by first-occurrence /
/// search-order preservation (5).
fn mutant_last_occurrence_wins(anchor: Vec<String>, deps: Vec<String>) -> Vec<String> {
    use std::collections::HashSet;
    // Keep last occurrence within deps, and drop deps already in the anchor.
    let anchor_set: HashSet<&String> = anchor.iter().collect();
    let mut kept_rev: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for tok in deps.iter().rev() {
        if anchor_set.contains(tok) {
            continue;
        }
        if seen.insert(tok.clone()) {
            kept_rev.push(tok.clone());
        }
    }
    kept_rev.reverse();
    let mut out = anchor;
    out.extend(kept_rev);
    out
}

#[test]
fn mutant_last_occurrence_wins_caught() {
    // dep root /x appears, then /y, then /x again. First-occurrence-wins keeps
    // [/x, /y]; last-occurrence-wins keeps [/y, /x] — a different search order.
    let anchor = v(&["-I/a"]);
    let deps = v(&["-I/x", "-I/y", "-I/x"]);
    let bad = mutant_last_occurrence_wins(anchor.clone(), deps.clone());
    assert_eq!(bad, v(&["-I/a", "-I/y", "-I/x"])); // last-wins reorders distinct roots
    assert!(
        check_all_candidate(&anchor, &deps, &bad).is_err(),
        "oracle went vacuous: accepted a last-occurrence-wins de-dup (search order flipped)"
    );
    // The real function keeps first occurrence, preserving search order.
    check_all(&anchor, &deps).unwrap();
    let good = normalized_include_sequence(anchor, deps);
    assert_eq!(good, v(&["-I/a", "-I/x", "-I/y"]));
}

/// MUTANT B: sorts the output (de-dups, then sorts the whole sequence). Caught
/// by anchor-verbatim (2) and/or first-occurrence order (5) — sorting destroys
/// the input search order.
fn mutant_sorts_output(anchor: Vec<String>, deps: Vec<String>) -> Vec<String> {
    use std::collections::HashSet;
    let anchor_set: HashSet<&String> = anchor.iter().collect();
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = anchor.clone();
    for tok in deps {
        if !anchor_set.contains(&tok) && seen.insert(tok.clone()) {
            out.push(tok);
        }
    }
    out.sort();
    out
}

#[test]
fn mutant_sorts_output_caught() {
    // Choose an input whose correct sequence is NOT already sorted.
    let anchor = v(&["-I/z", "-I/a"]);
    let deps = v(&["-I/m"]);
    let bad = mutant_sorts_output(anchor.clone(), deps.clone());
    assert_eq!(bad, v(&["-I/a", "-I/m", "-I/z"])); // sorted vs input search order
    assert!(
        check_all_candidate(&anchor, &deps, &bad).is_err(),
        "oracle went vacuous: accepted a sorted (reordered) de-dup"
    );
    check_all(&anchor, &deps).unwrap();
    let good = normalized_include_sequence(anchor, deps);
    assert_eq!(good, v(&["-I/z", "-I/a", "-I/m"])); // search order preserved
}

// ---------------------------------------------------------------------------
// Property: random (anchor, deps) include-flag vectors always satisfy all
// invariants.
// ---------------------------------------------------------------------------

/// One include-root flag drawn from a small pool, so duplicates (both
/// within-anchor, anchor-vs-dep, and within-deps) are FREQUENT and the de-dup
/// path is heavily exercised.
fn include_flag() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("-I/usr/include/x86_64-linux-gnu".to_string()),
        Just("-Itensorflow".to_string()),
        Just("-Ibazel-out/k8/bin".to_string()),
        Just("-I/data/rush-cache/external/com_google_absl".to_string()),
        Just("-Ithird_party/eigen3".to_string()),
        Just("-I".to_string()), // degenerate empty-dir form
    ]
}

fn include_flags() -> impl Strategy<Value = Vec<String>> {
    proptest::collection::vec(include_flag(), 0..=14)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    /// All invariants hold on every generated (anchor, deps) pair.
    #[test]
    fn dedup_satisfies_all_invariants(anchor in include_flags(), deps in include_flags()) {
        if let Err(why) = check_all(&anchor, &deps) {
            prop_assert!(false, "invariant violated on anchor={:?} deps={:?}: {}", anchor, deps, why);
        }
    }

    /// Idempotence: re-deduping the kept tail against the same anchor is a
    /// no-op (everything kept is now either in the anchor's `seen` or unique).
    #[test]
    fn dedup_is_idempotent(anchor in include_flags(), deps in include_flags()) {
        let once = dedup_dep_include_flags(anchor.clone(), deps);
        let twice = dedup_dep_include_flags(anchor, once.clone());
        prop_assert_eq!(once, twice);
    }

    /// Every distinct token in deps that is NOT in the anchor survives exactly
    /// once in the kept tail (paired completeness + at-most-once for the tail).
    #[test]
    fn dep_distinct_roots_survive_once(anchor in include_flags(), deps in include_flags()) {
        use std::collections::HashSet;
        let kept = dedup_dep_include_flags(anchor.clone(), deps.clone());
        let anchor_set: HashSet<&String> = anchor.iter().collect();
        let kept_set: HashSet<&String> = kept.iter().collect();
        for tok in &deps {
            if !anchor_set.contains(tok) {
                prop_assert!(kept_set.contains(tok),
                    "dep root {:?} not in anchor was dropped from kept tail", tok);
            }
        }
        // and at-most-once
        let mut seen = HashSet::new();
        for tok in &kept {
            prop_assert!(seen.insert(tok.clone()), "kept tail has duplicate {:?}", tok);
        }
        // sanity: all kept are include flags here (generator only emits -I)
        for tok in &kept {
            prop_assert!(is_include_flag(tok));
        }
    }
}
