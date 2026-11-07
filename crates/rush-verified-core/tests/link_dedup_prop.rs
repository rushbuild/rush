// Rush verified-core — archive-path link-flag de-dup ORACLE
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT
//
// spec-owned (oracle): this file is the functional oracle for the
// link-dedup-v1 verified scope. It witnesses the de-dup invariants on
// thousands of random link-arg vectors AND on the historical
// liberror_codes_proto_impl_cc_impl.a double-reach bug, and proves the oracle
// is non-vacuous via deliberately-wrong de-dup variants it catches. Deleting,
// #[ignore]-ing, or weakening these assertions is a spec-sensitive change
// (Spec-Change: oracle) and routes through the human Spec-Review gate. See
// ci/spec-owned.tsv.
//
//! Property/golden ORACLE for [`rush_verified_core::dedup_archive_link_flags`].
//! On every generated link-arg vector it checks the five real invariants that
//! make the de-dup safe — a wrong de-dup either reintroduces duplicate-symbol
//! link failures OR drops a needed archive:
//!
//!   1. **subsequence** — the output is an order-preserving subsequence of the
//!      input (no reordering, no invented tokens);
//!   2. **archive at-most-once** — each distinct archive PATH appears at most
//!      once in the output (first occurrence, in whatever form, wins);
//!   3. **archive completeness** — every distinct archive path present in the
//!      input is present in the output (no archive dropped);
//!   4. **non-archive fidelity** — every non-archive token (`-l*`, `-L*`,
//!      `-Wl,*` other than the whole-archive markers, bare flags) is preserved
//!      with its original order AND multiplicity (NOT deduped);
//!   5. **triple integrity** — each `--whole-archive P --no-whole-archive`
//!      triple is kept or dropped as a UNIT (an emitted open marker is always
//!      immediately followed, in the output, by its archive and close marker).
//!
//! Anti-vacuity is demonstrated three ways (each a deliberately-broken de-dup
//! the oracle CATCHES):
//!   * `mutant_dedups_l_flags_caught` — a variant that also de-dups `-l` flags
//!     is caught by the non-archive-multiplicity property (4);
//!   * `mutant_reorders_caught` — a variant that sorts the output is caught by
//!     the subsequence property (1);
//!   * `mutant_splits_triple_caught` — a variant that drops only the 2nd member
//!     of a repeated whole-archive triple is caught by triple-integrity (5).

use proptest::prelude::*;
use rush_verified_core::{dedup_archive_link_flags, is_archive_path};
use std::collections::HashMap;
use std::collections::HashSet;

const WHOLE_ARCHIVE_OPEN: &str = "-Wl,--whole-archive";
const WHOLE_ARCHIVE_CLOSE: &str = "-Wl,--no-whole-archive";

// ---------------------------------------------------------------------------
// Oracle checkers (shared by golden + property tests). Each returns
// Result<(), String> so proptest can report the failing case.
// ---------------------------------------------------------------------------

/// (1) `out` is an order-preserving subsequence of `inp`: walking `inp` once
/// left-to-right must consume all of `out` in order.
fn check_subsequence(inp: &[String], out: &[String]) -> Result<(), String> {
    let mut it = inp.iter();
    for tok in out {
        let mut found = false;
        for cand in it.by_ref() {
            if cand == tok {
                found = true;
                break;
            }
        }
        if !found {
            return Err(format!(
                "output token {:?} is not a subsequence of input (order broken or invented token)",
                tok
            ));
        }
    }
    Ok(())
}

/// (2) each distinct archive path appears at most once in `out`.
fn check_archive_at_most_once(out: &[String]) -> Result<(), String> {
    let mut seen = HashSet::new();
    for tok in out {
        if is_archive_path(tok) && !seen.insert(tok.clone()) {
            return Err(format!("archive path {:?} appears more than once in output", tok));
        }
    }
    Ok(())
}

/// (3) every distinct archive path in `inp` is present in `out`.
fn check_archive_completeness(inp: &[String], out: &[String]) -> Result<(), String> {
    let in_archives: HashSet<&String> = inp.iter().filter(|t| is_archive_path(t)).collect();
    let out_archives: HashSet<&String> = out.iter().filter(|t| is_archive_path(t)).collect();
    for a in in_archives {
        if !out_archives.contains(a) {
            return Err(format!("archive path {:?} present in input was dropped from output", a));
        }
    }
    Ok(())
}

/// "Ordinary" non-archive token = a non-archive token that is NOT one of the
/// two whole-archive markers. These pass through the de-dup completely
/// untouched (order + multiplicity); the markers do NOT (they ride along with
/// their archive inside a deduped triple), so they are governed by triple
/// integrity (5) instead, not by this property.
fn is_ordinary_flag(t: &str) -> bool {
    !is_archive_path(t) && t != WHOLE_ARCHIVE_OPEN && t != WHOLE_ARCHIVE_CLOSE
}

/// (4) ordinary non-archive flags (`-l*`, `-L*`, `-Wl,*` other than the
/// whole-archive markers, bare operands) are preserved with original order AND
/// multiplicity (NOT deduped). Order is already pinned by (1), so equal
/// per-token counts fully characterise multiplicity preservation.
fn check_non_archive_fidelity(inp: &[String], out: &[String]) -> Result<(), String> {
    let count = |v: &[String]| -> HashMap<String, usize> {
        let mut m = HashMap::new();
        for t in v {
            if is_ordinary_flag(t) {
                *m.entry(t.clone()).or_insert(0) += 1;
            }
        }
        m
    };
    let ci = count(inp);
    let co = count(out);
    if ci != co {
        return Err(format!(
            "ordinary non-archive flag multiplicity changed: input {:?} != output {:?}",
            ci, co
        ));
    }
    Ok(())
}

/// Count of a specific marker token in a sequence.
fn marker_count(seq: &[String], marker: &str) -> usize {
    seq.iter().filter(|t| t.as_str() == marker).count()
}

/// (5) triple integrity — markers are only ever removed as PART OF a whole
/// `<open> <archive> <close>` unit, never split.
///
/// We can't pin marker structure absolutely (subtle but real: when the de-dup
/// drops a duplicate triple, an unrelated *loose* open from before it and a
/// loose close from after it can become adjacent to a surviving archive and so
/// be re-recognised as a fresh triple in the output — see the
/// `triple_reformation_is_legitimate` golden). What IS invariant is the
/// accounting: production removes markers only in matched open/close PAIRS
/// (one open + one close per dropped triple). So the number of opens removed
/// equals the number of closes removed:
///
/// ```text
/// count(open, input)  - count(open, output)
///   == count(close, input) - count(close, output)
/// ```
///
/// Combined with the subsequence property (1, which forbids invented tokens
/// and reordering), this is exactly the "kept-or-dropped as a unit" guarantee:
///   * a variant that drops only a triple's *closing* member removes a close
///     without a matching open → LHS 0 ≠ RHS 1 (caught — `mutant_splits_triple`);
///   * a lone unmatched marker in the input is simply never removed, so it
///     contributes 0 to both sides (legitimate passthrough, accepted).
fn check_triple_integrity(inp: &[String], out: &[String]) -> Result<(), String> {
    let open_removed =
        marker_count(inp, WHOLE_ARCHIVE_OPEN) as isize - marker_count(out, WHOLE_ARCHIVE_OPEN) as isize;
    let close_removed = marker_count(inp, WHOLE_ARCHIVE_CLOSE) as isize
        - marker_count(out, WHOLE_ARCHIVE_CLOSE) as isize;
    if open_removed != close_removed {
        return Err(format!(
            "whole-archive markers not removed in matched pairs: \
             {} opens removed vs {} closes removed (a triple was split) in output {:?}",
            open_removed, close_removed, out
        ));
    }
    Ok(())
}

/// Run every invariant; returns the first failure.
fn check_all(inp: &[String], out: &[String]) -> Result<(), String> {
    check_subsequence(inp, out)?;
    check_archive_at_most_once(out)?;
    check_archive_completeness(inp, out)?;
    check_non_archive_fidelity(inp, out)?;
    check_triple_integrity(inp, out)?;
    Ok(())
}

fn v(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

// ---------------------------------------------------------------------------
// Golden / specific-witness cases (always run, no randomness).
// ---------------------------------------------------------------------------

/// The real bug: `liberror_codes_proto_impl_cc_impl.a` reached twice (once via
/// the CcInfo linker_inputs walk, once via the transitive NestedSet sweep),
/// each time wrapped in its own whole-archive triple. The second triple must be
/// dropped as a unit, leaving exactly one.
#[test]
fn witness_error_codes_proto_double_reach() {
    let input = v(&[
        "-Wl,--whole-archive",
        "bazel-out/k8/bin/liberror_codes_proto_impl_cc_impl.a",
        "-Wl,--no-whole-archive",
        "-lpthread",
        "-Wl,--whole-archive",
        "bazel-out/k8/bin/liberror_codes_proto_impl_cc_impl.a",
        "-Wl,--no-whole-archive",
    ]);
    let out = dedup_archive_link_flags(input.clone());
    check_all(&input, &out).unwrap();
    assert_eq!(
        out,
        v(&[
            "-Wl,--whole-archive",
            "bazel-out/k8/bin/liberror_codes_proto_impl_cc_impl.a",
            "-Wl,--no-whole-archive",
            "-lpthread",
        ])
    );
}

/// Same archive reached once bare and once whole-archive-wrapped: first form
/// wins (here bare), the wrapped triple is dropped as a unit.
#[test]
fn witness_bare_then_wrapped_same_path() {
    let input = v(&[
        "libfoo.a",
        "-Wl,--whole-archive",
        "libfoo.a",
        "-Wl,--no-whole-archive",
    ]);
    let out = dedup_archive_link_flags(input.clone());
    check_all(&input, &out).unwrap();
    assert_eq!(out, v(&["libfoo.a"]));
}

/// Versioned shared object (`.so.2.15.0`) is an archive path and deduped.
#[test]
fn witness_versioned_so_deduped() {
    let input = v(&["libtensorflow_cc.so.2.15.0", "-L/x", "libtensorflow_cc.so.2.15.0"]);
    let out = dedup_archive_link_flags(input.clone());
    check_all(&input, &out).unwrap();
    assert_eq!(out, v(&["libtensorflow_cc.so.2.15.0", "-L/x"]));
}

/// SUBTLETY GOLDEN: dropping a duplicate triple can let an unrelated loose
/// open (before it) and loose close (after it) wrap a surviving archive and be
/// re-recognised as a triple in the output. This is real production behaviour
/// (greedy left-to-right triple recognition over the emitted stream), and it is
/// why triple-integrity (5) is stated as matched-pair removal accounting rather
/// than absolute marker-structure preservation.
#[test]
fn triple_reformation_is_legitimate() {
    let input = v(&[
        "-Wl,--whole-archive",
        "libb.a",
        "-Wl,--no-whole-archive",
        "-Wl,--whole-archive", // loose open (next token is not a close)
        "-Wl,--whole-archive",
        "libb.a", // duplicate triple -> dropped as a unit
        "-Wl,--no-whole-archive",
        "dir/libc.a",
        "-Wl,--no-whole-archive", // loose close
    ]);
    let out = dedup_archive_link_flags(input.clone());
    // The surviving loose open + libc + loose close re-form a triple.
    assert_eq!(
        out,
        v(&[
            "-Wl,--whole-archive",
            "libb.a",
            "-Wl,--no-whole-archive",
            "-Wl,--whole-archive",
            "dir/libc.a",
            "-Wl,--no-whole-archive",
        ])
    );
    check_all(&input, &out).unwrap();
}

// ---------------------------------------------------------------------------
// ANTI-VACUITY: deliberately-wrong de-dup variants the oracle MUST catch.
// ---------------------------------------------------------------------------

/// MUTANT A: also de-dups `-l` flags. Caught by non-archive fidelity (4).
fn mutant_dedups_everything(flags: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    flags.into_iter().filter(|t| seen.insert(t.clone())).collect()
}

#[test]
fn mutant_dedups_l_flags_caught() {
    // Two distinct -lpthread must both survive; the mutant collapses them.
    let input = v(&["-lpthread", "liba.a", "-lpthread"]);
    let bad = mutant_dedups_everything(input.clone());
    assert_eq!(bad, v(&["-lpthread", "liba.a"])); // mutant dropped a -lpthread
    assert!(
        check_all(&input, &bad).is_err(),
        "oracle went vacuous: accepted a de-dup that collapsed -l flags"
    );
    // The real function passes.
    let good = dedup_archive_link_flags(input.clone());
    check_all(&input, &good).unwrap();
    assert_eq!(good, v(&["-lpthread", "liba.a", "-lpthread"]));
}

/// MUTANT B: reorders the output (sorts it). Caught by subsequence (1).
fn mutant_reorders(flags: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out: Vec<String> = flags
        .into_iter()
        .filter(|t| !is_archive_path(t) || seen.insert(t.clone()))
        .collect();
    out.sort();
    out
}

#[test]
fn mutant_reorders_caught() {
    // Choose an input whose correct de-dup is NOT already sorted.
    let input = v(&["libz.a", "liba.a"]);
    let bad = mutant_reorders(input.clone());
    assert_eq!(bad, v(&["liba.a", "libz.a"])); // reordered vs input
    assert!(
        check_all(&input, &bad).is_err(),
        "oracle went vacuous: accepted a reordering de-dup"
    );
    let good = dedup_archive_link_flags(input.clone());
    check_all(&input, &good).unwrap();
    assert_eq!(good, input); // both distinct archives kept, order preserved
}

/// MUTANT C: on a repeated whole-archive triple, drops ONLY the 2nd triple's
/// closing marker (splitting the triple) instead of the whole unit. Caught by
/// triple integrity (5).
fn mutant_splits_triple(flags: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < flags.len() {
        if flags[i] == WHOLE_ARCHIVE_OPEN
            && i + 2 < flags.len()
            && flags[i + 2] == WHOLE_ARCHIVE_CLOSE
            && is_archive_path(&flags[i + 1])
        {
            if seen.insert(flags[i + 1].clone()) {
                out.push(flags[i].clone());
                out.push(flags[i + 1].clone());
                out.push(flags[i + 2].clone());
            } else {
                // BUG: emit a dangling open marker, dropping archive + close.
                out.push(flags[i].clone());
            }
            i += 3;
            continue;
        }
        out.push(flags[i].clone());
        i += 1;
    }
    out
}

#[test]
fn mutant_splits_triple_caught() {
    let input = v(&[
        "-Wl,--whole-archive",
        "liba.a",
        "-Wl,--no-whole-archive",
        "-Wl,--whole-archive",
        "liba.a",
        "-Wl,--no-whole-archive",
    ]);
    let bad = mutant_splits_triple(input.clone());
    // The mutant left a dangling open marker from the 2nd triple.
    assert_eq!(
        bad,
        v(&[
            "-Wl,--whole-archive",
            "liba.a",
            "-Wl,--no-whole-archive",
            "-Wl,--whole-archive",
        ])
    );
    assert!(
        check_all(&input, &bad).is_err(),
        "oracle went vacuous: accepted a split whole-archive triple"
    );
    let good = dedup_archive_link_flags(input.clone());
    check_all(&input, &good).unwrap();
    assert_eq!(
        good,
        v(&["-Wl,--whole-archive", "liba.a", "-Wl,--no-whole-archive"])
    );
}

// ---------------------------------------------------------------------------
// Property: random link-arg vectors always satisfy all five invariants.
// ---------------------------------------------------------------------------

/// One link token drawn from a hazard-rich alphabet: real archive paths (which
/// will collide and need de-duping), versioned `.so`s, whole-archive markers,
/// `-l`/`-L`/`-Wl,` flags (which must NOT be deduped), and bare object paths.
fn token() -> impl Strategy<Value = String> {
    prop_oneof![
        // Archive paths from a small pool, so duplicates are frequent.
        prop_oneof![
            Just("liba.a".to_string()),
            Just("libb.a".to_string()),
            Just("dir/libc.a".to_string()),
            Just("libd.so".to_string()),
            Just("libtf.so.2.15.0".to_string()),
        ],
        // Whole-archive markers (emitted standalone; the generator below also
        // assembles proper triples).
        Just(WHOLE_ARCHIVE_OPEN.to_string()),
        Just(WHOLE_ARCHIVE_CLOSE.to_string()),
        // Non-archive flags that must survive with full multiplicity.
        prop_oneof![
            Just("-lpthread".to_string()),
            Just("-lm".to_string()),
            Just("-L/usr/lib".to_string()),
            Just("-Wl,-soname,libx.so".to_string()),
            Just("-pthread".to_string()),
        ],
        // Bare non-archive operands (object files, plain names).
        prop_oneof![Just("foo.o".to_string()), Just("plain".to_string())],
    ]
}

/// A token sequence built from random tokens, but ALSO injecting well-formed
/// whole-archive triples so the triple-dedup path is heavily exercised.
fn link_args() -> impl Strategy<Value = Vec<String>> {
    let archive_pool = prop_oneof![
        Just("liba.a".to_string()),
        Just("libb.a".to_string()),
        Just("dir/libc.a".to_string()),
        Just("libtf.so.2.15.0".to_string()),
    ];
    // An "item" is either a single random token or a complete triple.
    let item = prop_oneof![
        token().prop_map(|t| vec![t]),
        archive_pool.prop_map(|p| vec![
            WHOLE_ARCHIVE_OPEN.to_string(),
            p,
            WHOLE_ARCHIVE_CLOSE.to_string(),
        ]),
    ];
    proptest::collection::vec(item, 0..=12).prop_map(|items| items.concat())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    /// All five invariants hold on every generated link-arg vector.
    #[test]
    fn dedup_satisfies_all_invariants(input in link_args()) {
        let out = dedup_archive_link_flags(input.clone());
        if let Err(why) = check_all(&input, &out) {
            prop_assert!(false, "invariant violated on {:?}: {}", input, why);
        }
    }

    /// Idempotence: dedup(dedup(x)) == dedup(x).
    #[test]
    fn dedup_is_idempotent(input in link_args()) {
        let once = dedup_archive_link_flags(input);
        let twice = dedup_archive_link_flags(once.clone());
        prop_assert_eq!(once, twice);
    }
}
