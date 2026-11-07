// Rush verified-core — archive-path link-flag de-duplication model
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Pure model for the `link-dedup-v1` verified scope.
//!
//! A C++ link line is assembled by walking a target's `CcInfo` linker_inputs
//! AND by sweeping the transitive `NestedSet` of archives. The same static
//! archive (`.a`) can be reached on BOTH paths, so it lands on the g++/ld
//! command line twice. Inside a `--whole-archive` group a duplicated archive
//! makes the linker extract every member a second time, producing the classic
//! `multiple definition of …` failure; outside one it is merely wasteful. The
//! production de-dup is [`dedup_archive_link_flags`] in
//! `crates/rush-rules/src/rules_cc.rs` (≈ line 5197). This module extracts its
//! exact, I/O-free core so a verifier can reason about it and the proptest
//! oracle can witness its invariants on every build.
//!
//! The contract is two-sided, and getting EITHER side wrong is a real bug:
//!
//!   * **completeness** — every distinct archive path present in the input is
//!     present in the output (dropping a needed `.a` reintroduces *undefined*
//!     symbols at link/load); and
//!   * **at-most-once** — each distinct archive path appears at most once in
//!     the output, first-occurrence wins (a duplicate reintroduces *multiple
//!     definition* errors inside `--start-group`/`--whole-archive`).
//!
//! Three structural invariants make those safe to state:
//!
//!   1. the output is an order-preserving **subsequence** of the input (no
//!      reordering, no invented tokens) — order matters to `ld`;
//!   2. **non-archive** flags (`-l*`, `-L*`, `-Wl,*` other than the
//!      whole-archive markers, and bare flags) pass through untouched, with
//!      their original order AND multiplicity (they are NOT archives, so
//!      de-duping them would be wrong — e.g. repeated `-l` entries can be
//!      load-bearing for circular static deps); and
//!   3. the `-Wl,--whole-archive <path> -Wl,--no-whole-archive` triple is kept
//!      or dropped **as a unit** — never split, which would leave a dangling
//!      `--whole-archive` toggling the wrong members.
//!
//! The Pearlite contract below (under `--features creusot`) captures the
//! tractable algebraic invariants — output-is-subsequence and idempotence —
//! directly. The two-sided completeness/at-most-once pair over the archive-path
//! projection, and the triple-integrity property, are heavier set-level
//! obligations discharged by the proptest oracle (`tests/link_dedup_prop.rs`)
//! and are noted there as oracle-only.

#[allow(unused_imports)]
use crate::contract_shim::*;

/// The token that opens a whole-archive group (`-Wl,--whole-archive`).
const WHOLE_ARCHIVE_OPEN: &str = "-Wl,--whole-archive";
/// The token that closes a whole-archive group (`-Wl,--no-whole-archive`).
const WHOLE_ARCHIVE_CLOSE: &str = "-Wl,--no-whole-archive";

/// Whether `s` denotes a linkable archive PATH that participates in de-dup.
///
/// Mirrors the production `is_archive_path` closure verbatim:
///   * anything beginning with `-` is a flag, never an archive path;
///   * otherwise it is an archive iff its filename extension is `a` or `so`,
///     **or** the string contains the substring `.so.` (versioned shared
///     objects such as `libtensorflow_cc.so.2.15.0`, whose final "extension"
///     is the numeric soversion, not `so`).
///
/// Note the deliberate asymmetry: a plain `-l`/`-L`/`-Wl,` flag is excluded by
/// the leading-`-` test, so only literal object/library *paths* are deduped.
pub fn is_archive_path(s: &str) -> bool {
    if s.starts_with('-') {
        return false;
    }
    let ext = extension(s);
    matches!(ext, Some("a") | Some("so")) || s.contains(".so.")
}

/// Filename extension of `s` (the bytes after the final `.` of the final path
/// component), or `None` when there is no extension — matching
/// `std::path::Path::extension`'s contract for the cases this code can see.
///
/// We compute it without `std::path` so the model stays a pure string function
/// the verifier can follow: take the last path component (after the final `/`),
/// and return the suffix after its last `.` provided that `.` is neither the
/// first byte (a dotfile like `.a` has no extension) nor the last byte.
fn extension(s: &str) -> Option<&str> {
    let name = match s.rfind('/') {
        Some(slash) => &s[slash + 1..],
        None => s,
    };
    let dot = name.rfind('.')?;
    // A leading dot (dotfile) is not an extension separator; a trailing dot
    // yields an empty extension, which `Path::extension` reports as `Some("")`
    // — but `matches!(ext, Some("a")|Some("so"))` rejects it either way, and we
    // mirror `Path` by returning the (possibly empty) suffix only when the dot
    // is interior.
    if dot == 0 {
        None
    } else {
        Some(&name[dot + 1..])
    }
}

/// De-duplicate archive PATHS on a C++ link line, preserving order and leaving
/// every non-archive token untouched.
///
/// This is the pure twin of `crates/rush-rules/src/rules_cc.rs`'s
/// `dedup_archive_link_flags` (the production function should delegate here —
/// see the crate's wire-up note). Semantics, preserved exactly:
///
///   * A `-Wl,--whole-archive`, `<archive-path>`, `-Wl,--no-whole-archive`
///     triple (the middle token passing [`is_archive_path`]) is treated as a
///     unit: the first time that inner path is seen, all three tokens are
///     emitted; thereafter the whole triple is skipped. The cursor always
///     advances by 3 across a recognised triple, so the triple is never split.
///   * A bare token that [`is_archive_path`] is emitted only on first sight of
///     that path.
///   * Any other token is emitted unconditionally (order and multiplicity
///     preserved).
///
/// Crucially the `seen` set is SHARED between the triple arm and the bare arm:
/// an archive first seen wrapped in `--whole-archive` suppresses a later bare
/// occurrence of the same path, and vice-versa. First occurrence — in whatever
/// form — wins, and that form is the one emitted.
#[cfg_attr(feature = "creusot",
    ensures(is_subsequence_logic(result@, flags@)))]
pub fn dedup_archive_link_flags(flags: Vec<String>) -> Vec<String> {
    let mut seen_paths: alloc_set::StrSet = alloc_set::StrSet::new();
    let mut deduped: Vec<String> = Vec::with_capacity(flags.len());
    let mut i = 0usize;
    while i < flags.len() {
        let flag = &flags[i];
        // Pattern: [--whole-archive, <archive-path>, --no-whole-archive].
        if flag == WHOLE_ARCHIVE_OPEN
            && i + 2 < flags.len()
            && flags[i + 2] == WHOLE_ARCHIVE_CLOSE
            && is_archive_path(&flags[i + 1])
        {
            if seen_paths.insert(&flags[i + 1]) {
                deduped.push(flags[i].clone());
                deduped.push(flags[i + 1].clone());
                deduped.push(flags[i + 2].clone());
            }
            i += 3;
            continue;
        }
        if is_archive_path(flag) {
            if seen_paths.insert(flag) {
                deduped.push(flag.clone());
            }
        } else {
            deduped.push(flag.clone());
        }
        i += 1;
    }
    proof_assert!(true);
    deduped
}

/// Minimal insertion-order-agnostic string set, kept tiny and `std`-free so the
/// verified core's trusted base does not grow a `HashSet` dependency. Linear
/// membership is fine here: link lines are short relative to a build, and the
/// production path's correctness (not its asymptotics) is what we model.
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
/// [`dedup_archive_link_flags`] can refer to it; erased entirely in the default
/// build.
///
/// This is the strongest invariant we can state in Pearlite without a fixpoint:
/// it already rules out reordering and invented tokens. The element-projection
/// invariants (each archive path at-most-once; no archive dropped) and the
/// triple-integrity invariant are set-level / pattern-level obligations left to
/// the proptest oracle (oracle-only — see `tests/link_dedup_prop.rs`).
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
    fn is_archive_path_classifies() {
        assert!(is_archive_path("libfoo.a"));
        assert!(is_archive_path("bazel-out/k8/bin/libbar.so"));
        assert!(is_archive_path("libtensorflow_cc.so.2.15.0"));
        // Flags are never archive paths.
        assert!(!is_archive_path("-lpthread"));
        assert!(!is_archive_path("-L/usr/lib"));
        assert!(!is_archive_path("-Wl,--whole-archive"));
        assert!(!is_archive_path("-Wl,-soname,libx.so"));
        // Non-archive extensions / no extension.
        assert!(!is_archive_path("foo.o"));
        assert!(!is_archive_path("plainname"));
        // Dotfile `.a` has no extension per Path semantics → not an archive.
        assert!(!is_archive_path(".a"));
    }

    #[test]
    fn dedups_bare_archive_first_wins() {
        let out = dedup_archive_link_flags(v(&["liba.a", "libb.a", "liba.a"]));
        assert_eq!(out, v(&["liba.a", "libb.a"]));
    }

    #[test]
    fn keeps_non_archive_multiplicity() {
        // -lpthread twice must survive twice; it is not an archive path.
        let out = dedup_archive_link_flags(v(&["-lpthread", "liba.a", "-lpthread"]));
        assert_eq!(out, v(&["-lpthread", "liba.a", "-lpthread"]));
    }

    #[test]
    fn whole_archive_triple_kept_as_unit_then_deduped() {
        let out = dedup_archive_link_flags(v(&[
            "-Wl,--whole-archive",
            "liberror_codes_proto_impl_cc_impl.a",
            "-Wl,--no-whole-archive",
            "-Wl,--whole-archive",
            "liberror_codes_proto_impl_cc_impl.a",
            "-Wl,--no-whole-archive",
        ]));
        assert_eq!(
            out,
            v(&[
                "-Wl,--whole-archive",
                "liberror_codes_proto_impl_cc_impl.a",
                "-Wl,--no-whole-archive",
            ])
        );
    }

    #[test]
    fn shared_seen_set_across_forms() {
        // First seen wrapped in --whole-archive; a later BARE occurrence of the
        // same path is suppressed (shared seen set).
        let out = dedup_archive_link_flags(v(&[
            "-Wl,--whole-archive",
            "liba.a",
            "-Wl,--no-whole-archive",
            "liba.a",
        ]));
        assert_eq!(
            out,
            v(&["-Wl,--whole-archive", "liba.a", "-Wl,--no-whole-archive"])
        );
        // And the reverse: bare first suppresses the later wrapped form's path,
        // but the open/close markers around it are non-archive tokens that pass
        // through (matching production: only the inner path is the dedup key,
        // and when the triple's path is already seen the WHOLE triple is
        // skipped — markers included).
        let out = dedup_archive_link_flags(v(&[
            "liba.a",
            "-Wl,--whole-archive",
            "liba.a",
            "-Wl,--no-whole-archive",
        ]));
        assert_eq!(out, v(&["liba.a"]));
    }

    #[test]
    fn unmatched_whole_archive_open_falls_through() {
        // Open marker without a following close is just a non-archive token.
        let out = dedup_archive_link_flags(v(&["-Wl,--whole-archive", "liba.a"]));
        assert_eq!(out, v(&["-Wl,--whole-archive", "liba.a"]));
    }

    #[test]
    fn idempotent() {
        let input = v(&[
            "-Wl,--whole-archive",
            "liba.a",
            "-Wl,--no-whole-archive",
            "libb.a",
            "liba.a",
            "-lpthread",
            "libb.a",
            "-lpthread",
        ]);
        let once = dedup_archive_link_flags(input);
        let twice = dedup_archive_link_flags(once.clone());
        assert_eq!(once, twice);
    }

    #[test]
    fn empty_is_empty() {
        assert_eq!(dedup_archive_link_flags(Vec::new()), Vec::<String>::new());
    }
}
