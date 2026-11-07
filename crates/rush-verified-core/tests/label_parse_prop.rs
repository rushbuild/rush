// Rush verified-core — Bazel label parse/render ORACLE
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT
//
// spec-owned (oracle): this file is the functional oracle for the
// label-parse-v1 verified scope. It witnesses the two round-trip properties
// from verification/dafny/label/Label.dfy on thousands of random well-formed
// labels, and proves the oracle is non-vacuous via deliberately-wrong parse/
// render variants it CATCHES. Deleting, #[ignore]-ing, or weakening these
// assertions is a spec-sensitive change (Spec-Change: oracle) and routes
// through the human Spec-Review gate. See ci/spec-owned.tsv.
//
//! Property/golden ORACLE for [`parse`] / [`CanonicalLabel::render`]. On every
//! random well-formed label it checks the two conjuncts of the Dafny `Label.dfy`
//! lemmas:
//!
//!   1. **RENDER-THEN-PARSE round-trip** — `parse(render(l), _, _) == Some(l)`
//!      (a canonical label renders to a string that parses back identically).
//!   2. **PARSE-THEN-RENDER idempotence/normalization** — `render(l)` re-parses
//!      and re-renders to the SAME string (`render` is a fixpoint through
//!      `parse`), and the implicit-target spellings normalize to the explicit
//!      `:lastseg` form.
//!
//! It also witnesses relative-`:t` resolution against context.
//!
//! Anti-vacuity is demonstrated with TWO deliberately-broken variants the
//! oracle must catch:
//!   * `drop_repo_render` (a renderer that omits the `@repo` prefix) is caught
//!     by RENDER-THEN-PARSE (the parsed-back label loses its repo);
//!   * `implicit_render` (a renderer that drops the `:target` when it equals the
//!     last package segment) is caught by PARSE-THEN-RENDER idempotence (its
//!     output re-renders to the explicit form, so the strings differ).

use proptest::prelude::*;
use rush_verified_core::{parse, CanonicalLabel};

// ---------------------------------------------------------------------------
// Generators: random WELL-FORMED canonical labels.
// ---------------------------------------------------------------------------

/// A single safe segment: 1..=4 chars from a small alphabet that excludes the
/// structural metacharacters `@`, `:`, `/`. Non-empty so it is a valid target
/// / repo / package segment.
fn safe_segment() -> impl Strategy<Value = String> {
    proptest::collection::vec(prop::sample::select(vec!['a', 'b', 'c', '0', '_', '.']), 1..=4)
        .prop_map(|cs| cs.into_iter().collect())
}

/// A well-formed package: 0..=3 safe segments joined by `/` (empty = root, no
/// leading/trailing/double slash by construction).
fn safe_package() -> impl Strategy<Value = String> {
    proptest::collection::vec(safe_segment(), 0..=3).prop_map(|segs| segs.join("/"))
}

/// A random well-formed [`CanonicalLabel`]: optional repo, well-formed package,
/// non-empty single-segment target.
fn well_formed_label() -> impl Strategy<Value = CanonicalLabel> {
    (
        prop::option::of(safe_segment()),
        safe_package(),
        safe_segment(),
    )
        .prop_map(|(repo, package, target)| CanonicalLabel {
            repo,
            package,
            target,
        })
}

// ---------------------------------------------------------------------------
// Deliberately-WRONG variants (anti-vacuity witnesses).
// ---------------------------------------------------------------------------

/// BUG: a renderer that drops the `@repo` prefix. The repo is lost, so
/// RENDER-THEN-PARSE no longer recovers the original label.
fn drop_repo_render(l: &CanonicalLabel) -> String {
    format!("//{}:{}", l.package, l.target)
}

/// BUG: a renderer that omits `:target` when the target equals the last package
/// segment (an "implicit target" shortcut). Its output re-renders to the
/// EXPLICIT form, so it is not a fixpoint — caught by PARSE-THEN-RENDER
/// idempotence.
fn implicit_render(l: &CanonicalLabel) -> String {
    let last = l.package.rsplit('/').next().unwrap_or(&l.package);
    let head = match &l.repo {
        Some(r) => format!("@{}//", r),
        None => "//".to_string(),
    };
    if !l.package.is_empty() && l.target == last {
        format!("{}{}", head, l.package)
    } else {
        format!("{}{}:{}", head, l.package, l.target)
    }
}

// ---------------------------------------------------------------------------
// Golden / specific-witness cases (always run, no randomness).
// ---------------------------------------------------------------------------

fn lbl(repo: Option<&str>, package: &str, target: &str) -> CanonicalLabel {
    CanonicalLabel {
        repo: repo.map(str::to_string),
        package: package.to_string(),
        target: target.to_string(),
    }
}

#[test]
fn witness_render_then_parse_real_forms() {
    // Golden witnesses drawn from real TF labels.
    for l in [
        lbl(None, "tensorflow/core", "framework"),
        lbl(Some("local_tsl"), "tsl/platform", "macros"),
        lbl(Some("snappy"), "", "snappy"),
        lbl(None, "", "root_lib"),
    ] {
        assert_eq!(parse(&l.render(), None, ""), Some(l.clone()), "round-trip {l:?}");
    }
}

#[test]
fn witness_implicit_target_normalizes() {
    // `//foo/bar` parses to target=bar and renders to the explicit `//foo/bar:bar`.
    let l = parse("//foo/bar", None, "").unwrap();
    assert_eq!(l, lbl(None, "foo/bar", "bar"));
    assert_eq!(l.render(), "//foo/bar:bar");
    // Re-parsing the canonical render is a fixpoint.
    assert_eq!(parse(&l.render(), None, ""), Some(l));
}

#[test]
fn witness_bare_external_repository_is_root_target() {
    let shorthand = parse("@pybind11", None, "").unwrap();
    let explicit = parse("@pybind11//:pybind11", None, "").unwrap();

    assert_eq!(shorthand, explicit);
    assert_eq!(shorthand, lbl(Some("pybind11"), "", "pybind11"));
    assert_eq!(shorthand.render(), "@pybind11//:pybind11");
}

#[test]
fn witness_relative_resolves_against_context() {
    assert_eq!(
        parse(":helper", Some("ws"), "pkg/sub"),
        Some(lbl(Some("ws"), "pkg/sub", "helper"))
    );
}

/// ANTI-VACUITY #1: a renderer that drops the repo prefix MUST be caught by the
/// RENDER-THEN-PARSE round-trip on at least one label — proving the round-trip
/// check is meaningful (not trivially satisfiable).
#[test]
fn drop_repo_render_is_caught_by_round_trip() {
    let l = lbl(Some("repo"), "a/b", "t");
    // The CORRECT render round-trips.
    assert_eq!(parse(&l.render(), None, ""), Some(l.clone()));
    // The BUGGY render loses the repo, so parse-back differs from the original.
    let buggy = drop_repo_render(&l);
    assert_ne!(
        parse(&buggy, None, ""),
        Some(l.clone()),
        "oracle went vacuous: drop-repo render still round-tripped"
    );
}

/// ANTI-VACUITY #2: a renderer that omits the implicit target MUST be caught by
/// PARSE-THEN-RENDER idempotence — its output re-renders to the explicit form,
/// so the canonical string is not a fixpoint of the buggy renderer.
#[test]
fn implicit_render_is_caught_by_idempotence() {
    let l = lbl(None, "foo/bar", "bar"); // target == last package segment
    // The CORRECT render is a fixpoint through parse.
    let canonical = l.render();
    let reparsed = parse(&canonical, None, "").unwrap();
    assert_eq!(reparsed.render(), canonical);
    // The BUGGY render drops `:bar`; parsing it and re-rendering yields the
    // EXPLICIT form, which differs from the buggy string.
    let buggy = implicit_render(&l);
    assert_eq!(buggy, "//foo/bar");
    let buggy_reparsed = parse(&buggy, None, "").unwrap();
    assert_ne!(
        buggy_reparsed.render(),
        buggy,
        "oracle went vacuous: non-canonical render was treated as a fixpoint"
    );
    assert_eq!(buggy_reparsed.render(), "//foo/bar:bar");
}

// ---------------------------------------------------------------------------
// Property: random well-formed labels round-trip and normalize idempotently.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4000))]

    /// RENDER-THEN-PARSE: any well-formed canonical label renders to a string
    /// that parses back to the IDENTICAL label under NO-repo context.
    ///
    /// TRUE-SEMANTICS NOTE (discovered against the real parser): the canonical
    /// render of a MAIN-workspace label (`repo: None`) is `//pkg:t`, and
    /// `Label::parse`'s `//` arm INHERITS `current_workspace` for the repo. So
    /// the round-trip recovers `None` only when parsed under a `None` repo
    /// context (the neutral context). For EXTERNAL labels (`@r//...`) the repo
    /// is explicit in the surface string, so the round-trip is context-free —
    /// asserted separately below. This is why [`render_label_round_trip`] parses
    /// under `None` context.
    #[test]
    fn render_then_parse_round_trips(l in well_formed_label()) {
        let rendered = l.render();
        let back = parse(&rendered, None, "");
        prop_assert_eq!(back.as_ref(), Some(&l), "render={:?}", rendered);
    }

    /// RENDER-THEN-PARSE for EXTERNAL labels is context-FREE: the `@repo` is
    /// explicit in the surface string, so any context recovers the same label.
    #[test]
    fn external_render_then_parse_is_context_free(
        repo in safe_segment(),
        package in safe_package(),
        target in safe_segment(),
    ) {
        let l = CanonicalLabel { repo: Some(repo), package, target };
        let rendered = l.render();
        let neutral = parse(&rendered, None, "");
        prop_assert_eq!(neutral.as_ref(), Some(&l));
        let with_ctx = parse(&rendered, Some("OTHER"), "other/pkg");
        prop_assert_eq!(with_ctx.as_ref(), Some(&l));
    }

    /// PARSE-THEN-RENDER idempotence: rendering a well-formed label, parsing it,
    /// and rendering AGAIN yields the byte-identical canonical string.
    #[test]
    fn parse_then_render_is_idempotent(l in well_formed_label()) {
        let canonical = l.render();
        let reparsed = parse(&canonical, None, "").expect("canonical render must parse");
        prop_assert_eq!(reparsed.render(), canonical);
    }

    /// IMPLICIT-TARGET NORMALIZATION: the implicit spelling `<head>pkg`
    /// (non-empty pkg) parses to the SAME label as the explicit
    /// `<head>pkg:lastseg(pkg)`, and both render to the explicit form.
    #[test]
    fn implicit_and_explicit_normalize_equal(
        repo in prop::option::of(safe_segment()),
        package in safe_package().prop_filter("non-empty pkg", |p| !p.is_empty()),
    ) {
        let head = match &repo {
            Some(r) => format!("@{}//", r),
            None => "//".to_string(),
        };
        let last = package.rsplit('/').next().unwrap().to_string();
        let implicit = parse(&format!("{head}{package}"), None, "").unwrap();
        let explicit = parse(&format!("{head}{package}:{last}"), None, "").unwrap();
        prop_assert_eq!(&implicit, &explicit);
        prop_assert_eq!(implicit.render(), explicit.render());
    }

    /// BARE EXTERNAL REPOSITORY NORMALIZATION: `@repo` and `@repo//:repo`
    /// identify the same root-package target for every valid repository name.
    #[test]
    fn bare_external_repository_normalizes(repo in safe_segment()) {
        let shorthand = parse(&format!("@{repo}"), None, "").unwrap();
        let explicit = parse(&format!("@{repo}//:{repo}"), None, "").unwrap();

        prop_assert_eq!(&shorthand, &explicit);
        prop_assert!(shorthand.package.is_empty());
        prop_assert_eq!(shorthand.target, repo);
    }

    /// RELATIVE resolution: `:t` inherits the current repo + package verbatim.
    #[test]
    fn relative_inherits_context(
        ctx_repo in prop::option::of(safe_segment()),
        ctx_pkg in safe_package(),
        target in safe_segment(),
    ) {
        let parsed = parse(&format!(":{target}"), ctx_repo.as_deref(), &ctx_pkg).unwrap();
        prop_assert_eq!(parsed, CanonicalLabel { repo: ctx_repo, package: ctx_pkg, target });
    }
}
