// Rush verified-core — include-path (`-I`) search-order dedup model
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Pure model for the `include-dedup-v1` verified scope.
//!
//! A C++ compile argv carries an ordered list of `-I<dir>` include-search
//! roots. The order is SEMANTICALLY SIGNIFICANT: gcc/clang search the `-I`
//! roots left-to-right and the FIRST root that contains a given header is the
//! one that resolves the `#include`. So a wrong de-dup is a real bug in two
//! distinct ways:
//!
//!   * **dropping** a needed `-I` root reintroduces *header not found* (or, more
//!     insidiously, silently resolves a header against a different root and
//!     pulls in a wrong/ABI-incompatible declaration); and
//!   * **reordering** the surviving roots changes which root wins for a header
//!     present in more than one — flipping e.g. the pinned-Abseil root behind a
//!     host `/usr/include/absl` and corrupting the build's ABI.
//!
//! The production assembly lives in
//! [`CcCompileCommandSpec::to_normalized_command_spec`] in
//! `crates/rush-rules/src/cc_command_spec.rs` (the de-dup is the
//! `dep_include_paths` loop at ≈ lines 457–467). It emits the include roots in
//! two phases:
//!
//!   1. an **anchor** prefix — the Linux extra-include roots (≈ line 425) then
//!      the rule's own `direct_include_paths` (≈ line 458) — pushed
//!      UNCONDITIONALLY, NOT de-duped against each other; followed by
//!   2. the transitive `dep_include_paths` (≈ line 462), each kept only when
//!      its full `-I<dir>` token is not already on the argv
//!      (`if !argv.contains(&flag)`), i.e. first-occurrence-wins against the
//!      anchor AND against earlier-kept dep roots.
//!
//! This module extracts that exact, I/O-free core so a verifier can reason
//! about it and the proptest oracle can witness its invariants on every build.
//!
//! ## Why the two-phase signature (and not one flat `dedup`)
//!
//! It is tempting to model this as a single `dedup_include_flags(flags) ->
//! flags` over the whole include stream. That would be UNFAITHFUL: production
//! does NOT de-duplicate the anchor against itself — a `direct_include_paths`
//! (or linux-extra) list that happens to repeat a root emits it twice, and a
//! flat first-occurrence dedup would silently collapse it. We mirror the real
//! two-phase shape: the anchor passes through verbatim, and only the dep roots
//! de-dup (against the anchor and each other). [`dedup_dep_include_flags`] is
//! the pure kernel; [`normalized_include_sequence`] is the convenience that
//! returns `anchor ++ kept_deps` (the full ordered `-I` sequence the compile
//! actually sees, modulo the excluded fs-probe trailers — see below).
//!
//! ## What is EXCLUDED from the pure core (and why)
//!
//! Two production steps are filesystem probes and are deliberately OUT OF
//! SCOPE — they would drag a `std::fs`/`std::env` dependency into the verified
//! core's trusted base and make the function non-deterministic:
//!
//!   * the workspace/genfiles trailer trio (≈ lines 480–484) — appended AFTER
//!     the dep-dedup loop, so it is not part of the de-dup decision at all; and
//!   * [`demote_host_system_includes`] / `pinned_absl_root()` (≈ lines 81–113)
//!     — which `retain`-drops `-I/usr/include` and conditionally appends the
//!     pinned-Abseil `-I` only when `…/com_google_absl/absl/base/options.h`
//!     exists on disk. That probe's RESULT can be injected as an ordinary
//!     anchor/dep root by the caller; the probe itself stays in production I/O.
//!
//! ## Token shape
//!
//! Every include root in production is emitted as a SINGLE token
//! `format!("-I{}", dir)` — there is no two-token `-I <dir>` form anywhere on
//! the compile argv (verified by grep over `cc_command_spec.rs` /
//! `rules_cc.rs`). The de-dup key is therefore the full `-I<dir>` string, and
//! this kernel operates on that single-token form, exactly as `argv.contains`
//! does. A `-Idir` and a (hypothetical) `-I dir` two-token form would be
//! DIFFERENT keys to production, so we do not conflate them either.
//!
//! The Pearlite contract below (under `--features creusot`) captures the
//! tractable algebraic invariant — the kept-dep output is a subsequence of the
//! dep input — directly. The cross-phase completeness / first-occurrence /
//! at-most-once obligations over the anchor∪dep root set are heavier set-level
//! properties discharged by the proptest oracle (`tests/include_dedup_prop.rs`)
//! and noted there as oracle-only.

#[allow(unused_imports)]
use crate::contract_shim::*;

/// The `-I` include-flag prefix. A token is an include-search-root flag iff it
/// begins with this prefix; everything else (`-D…`, `-std=…`, a bare source
/// path, …) is opaque to this kernel.
const INCLUDE_PREFIX: &str = "-I";

/// Whether `s` is a single-token include-search-root flag (`-I<dir>`).
///
/// Mirrors production exactly: include roots are emitted as `format!("-I{}",
/// dir)`, so the test is purely lexical — a leading `-I`. The bare `-I` with no
/// directory (`"-I"` alone) is still classified as an include flag here (its
/// "dir" is the empty string); production never emits it, but treating it
/// uniformly keeps the model total.
pub fn is_include_flag(s: &str) -> bool {
    s.starts_with(INCLUDE_PREFIX)
}

/// De-duplicate the transitive dependency include flags against an anchor and
/// against each other, preserving search order and first-occurrence.
///
/// This is the pure twin of the `dep_include_paths` loop in
/// `crates/rush-rules/src/cc_command_spec.rs` (≈ lines 462–467); the production
/// function should delegate here (see the crate's wire-up note). Semantics,
/// preserved exactly:
///
///   * `anchor` is the set of `-I` flags ALREADY on the argv when the dep loop
///     runs (the linux-extra roots followed by `direct_include_paths`). It is
///     consulted for membership but NOT emitted by this function and NOT
///     de-duped — production pushes it verbatim beforehand.
///   * For each flag in `deps`, in order: keep it iff its full `-I<dir>` token
///     has not already been seen — neither in `anchor` nor in a previously-kept
///     dep. First occurrence wins; later duplicates are dropped. This is
///     exactly `if !argv.contains(&flag) { argv.push(flag) }`.
///
/// The returned vector is the ordered list of dep flags to APPEND after the
/// anchor. Non-`-I` tokens are not expected in `deps` (production only feeds it
/// `-I<dir>` flags), but should one appear it is treated as an ordinary token
/// keyed by its full string — i.e. it is still subject to the
/// `argv.contains`-style first-occurrence dedup, byte-for-byte matching what
/// production's `argv.contains(&flag)` would do for any repeated token.
#[cfg_attr(feature = "creusot",
    ensures(is_subsequence_logic(result@, deps@)))]
pub fn dedup_dep_include_flags(anchor: Vec<String>, deps: Vec<String>) -> Vec<String> {
    let mut seen: alloc_set::StrSet = alloc_set::StrSet::new();
    // Pre-seed `seen` with the anchor flags: production's `argv.contains`
    // checks against everything already on the argv, and the only include
    // tokens there are the anchor roots.
    for a in &anchor {
        seen.insert(a);
    }
    let mut kept: Vec<String> = Vec::with_capacity(deps.len());
    let mut i = 0usize;
    while i < deps.len() {
        let flag = &deps[i];
        if seen.insert(flag) {
            kept.push(flag.clone());
        }
        i += 1;
    }
    proof_assert!(true);
    kept
}

/// The full ordered `-I` search sequence the compile sees: the anchor verbatim,
/// followed by the de-duplicated dep roots ([`dedup_dep_include_flags`]).
///
/// Convenience over the kernel for callers/oracles that want the complete
/// resulting sequence rather than just the appended tail. The fs-probe trailers
/// (workspace/genfiles trio, host-include demotion, pinned-Abseil append) are
/// OUT OF SCOPE here — see the module docs.
pub fn normalized_include_sequence(anchor: Vec<String>, deps: Vec<String>) -> Vec<String> {
    let kept = dedup_dep_include_flags(anchor.clone(), deps);
    let mut out: Vec<String> = Vec::with_capacity(anchor.len() + kept.len());
    out.extend(anchor);
    out.extend(kept);
    out
}

/// Minimal insertion-order-agnostic string set, kept tiny and `std`-free so the
/// verified core's trusted base does not grow a `HashSet` dependency. Linear
/// membership is fine here: a compile's include list is short relative to a
/// build, and the production path's correctness (not its asymptotics — it uses
/// `Vec::contains`, itself linear) is what we model. Mirrors `link_dedup`'s
/// `StrSet`.
mod alloc_set {
    /// A set of owned strings with `insert` returning whether the value was new
    /// (mirroring `std::collections::HashSet::insert`).
    pub struct StrSet {
        items: Vec<String>,
    }

    impl StrSet {
        /// An empty set.
        pub fn new() -> Self {
            StrSet { items: Vec::new() }
        }

        /// Insert `value`; return `true` iff it was not already present.
        pub fn insert(&mut self, value: &str) -> bool {
            if self.items.iter().any(|v| v == value) {
                false
            } else {
                self.items.push(value.to_string());
                true
            }
        }
    }
}

/// Logic-level statement that `out` is an order-preserving subsequence of `inp`
/// — i.e. there is a strictly-increasing index map from `out` into `inp` with
/// matching elements. Defined as a Creusot `logic` fn so the `ensures` on
/// [`dedup_dep_include_flags`] can refer to it; erased entirely in the default
/// build.
///
/// This is the strongest invariant we can state in Pearlite without a fixpoint:
/// it already rules out reordering and invented tokens in the kept-dep tail.
/// The cross-phase invariants — every distinct anchor∪dep root survives
/// (completeness, no dropped `-I`), each distinct root appears at most once in
/// `anchor ++ kept`, and the kept copy is the FIRST occurrence (search-order
/// preservation) — are set-level obligations left to the proptest oracle
/// (oracle-only — see `tests/include_dedup_prop.rs`).
#[cfg(feature = "creusot")]
#[logic]
fn is_subsequence_logic(out: Seq<String>, inp: Seq<String>) -> bool {
    pearlite! {
        exists(|f: Mapping<Int, Int>|
            // a strictly increasing index map …
            (forall(|i: Int, j: Int|
                0 <= i && i < j && j < out.len() ==> f.get(i) < f.get(j)))
            // … landing in bounds …
            && (forall(|i: Int|
                0 <= i && i < out.len() ==> 0 <= f.get(i) && f.get(i) < inp.len()))
            // … and selecting the matching element at each step.
            && (forall(|i: Int|
                0 <= i && i < out.len() ==> out[i] == inp[f.get(i)])))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn is_include_flag_classifies() {
        assert!(is_include_flag("-I/usr/include"));
        assert!(is_include_flag("-Ibazel-out/k8/bin"));
        assert!(is_include_flag("-I")); // degenerate empty-dir form
        assert!(!is_include_flag("-D_GNU_SOURCE"));
        assert!(!is_include_flag("-std=c++17"));
        assert!(!is_include_flag("foo.cc"));
        assert!(!is_include_flag("-isystem")); // not an `-I` token
    }

    #[test]
    fn dep_dedups_against_anchor_first_wins() {
        // A dep root already present in the anchor is dropped.
        let kept = dedup_dep_include_flags(
            v(&["-I/a", "-I/b"]),
            v(&["-I/b", "-I/c"]),
        );
        assert_eq!(kept, v(&["-I/c"]));
    }

    #[test]
    fn dep_dedups_against_earlier_deps() {
        // Repeated dep root: first occurrence kept, later dropped.
        let kept = dedup_dep_include_flags(
            v(&["-I/a"]),
            v(&["-I/x", "-I/y", "-I/x"]),
        );
        assert_eq!(kept, v(&["-I/x", "-I/y"]));
    }

    #[test]
    fn anchor_not_self_deduped_in_full_sequence() {
        // REAL SEMANTICS: production pushes the anchor (linux-extras + direct)
        // verbatim — a duplicate WITHIN the anchor survives twice. A flat
        // dedup over the whole stream would wrongly collapse it.
        let seq = normalized_include_sequence(
            v(&["-I/dup", "-I/dup"]),
            v(&["-I/dup", "-I/new"]),
        );
        assert_eq!(seq, v(&["-I/dup", "-I/dup", "-I/new"]));
    }

    #[test]
    fn full_sequence_order_preserved() {
        let seq = normalized_include_sequence(
            v(&["-I/a", "-I/b"]),
            v(&["-I/b", "-I/c", "-I/a", "-I/d"]),
        );
        // anchor verbatim, then only the dep roots not already seen, in order.
        assert_eq!(seq, v(&["-I/a", "-I/b", "-I/c", "-I/d"]));
    }

    #[test]
    fn empty_deps_keeps_nothing() {
        assert_eq!(
            dedup_dep_include_flags(v(&["-I/a"]), Vec::new()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn empty_anchor_dedups_deps_among_themselves() {
        let kept = dedup_dep_include_flags(Vec::new(), v(&["-I/a", "-I/a", "-I/b"]));
        assert_eq!(kept, v(&["-I/a", "-I/b"]));
    }

    #[test]
    fn idempotent_on_kept_tail() {
        // De-duping an already-deduped dep tail against the same anchor is a
        // no-op (every kept flag is now in `seen` via the anchor OR is unique).
        let anchor = v(&["-I/a", "-I/b"]);
        let deps = v(&["-I/b", "-I/c", "-I/c", "-I/d"]);
        let once = dedup_dep_include_flags(anchor.clone(), deps);
        let twice = dedup_dep_include_flags(anchor, once.clone());
        assert_eq!(once, twice);
    }
}
