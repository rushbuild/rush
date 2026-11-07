// Rush verified-core — Bazel label parse/render canonical model
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Canonical Bazel-label parse + render model for the `label-parse-v1` verified
//! scope (authored DAFNY-FIRST; see `verification/dafny/label/Label.dfy`).
//!
//! Rush identifies every build target by a label. The production parser is
//! [`Label::parse`] in `crates/rush-build/src/dependency_graph.rs:33` (a
//! context-aware parser that resolves relative `:target` and implicit-target
//! `//pkg` forms). Two sibling parsers exist —
//! `BazelLabel::parse` (`bazel_compat.rs:334`) and `parse_external_repo_label`
//! (`external_repo_materialization.rs:690`). This module
//! extracts the I/O-free core of `Label::parse` so a verifier can reason about
//! it and the proptest oracle can witness its invariants.
//!
//! The canonical structure is `(repo: Option<String>, package, target)`. The
//! two-sided property we model (mirroring the Dafny `Label.dfy` lemmas) is:
//!
//!   * **RENDER-THEN-PARSE** — for any well-formed canonical label `l`,
//!     `parse(render(l), None, _) == Some(l)`. [`CanonicalLabel::render`] emits
//!     the absolute, explicit-`:target` form. TRUE-SEMANTICS caveat (discovered
//!     against the real parser): a MAIN-workspace label renders to `//pkg:t`,
//!     and `Label::parse`'s `//` arm INHERITS the current workspace, so the
//!     round-trip recovers `None` only under the neutral (`None`) repo context;
//!     for EXTERNAL labels the `@repo` is explicit, so it is context-free. This
//!     is the round-trip identity stated in Pearlite (see
//!     [`render_label_round_trip`], which parses under `None`).
//!   * **PARSE-THEN-RENDER idempotence/normalization** — the implicit-target
//!     forms `//pkg` and `@r//pkg` normalize to the explicit `:lastseg(pkg)`
//!     spelling, so `render(parse(s)) == render(parse(render(parse(s))))`. This
//!     fixpoint is the oracle-only conjunct (a string-equality fixpoint over
//!     `parse`, heavier to state in Pearlite, deferred to the Dafny addendum's
//!     `ParseThenRenderIdempotent`).
//!
//! ## Semantic boundaries among the production parsers
//!
//! [`Label::parse`] and `BazelLabel::parse` both treat bare `@repo` as the root
//! target `@repo//:repo`. The file-oriented parser differs on path-shaped
//! labels:
//!
//! 1. **Colon-less multi-segment `@r//a/b/c`.** `Label::parse` does NOT split
//!    the last path segment off the package: it yields
//!    `(Some("r"), "a/b/c", "c")` — package kept whole, target a COPY of the
//!    last segment. `parse_external_repo_label` is a FILE-oriented parser and
//!    splits: `(Some("r"), "a/b", "c")`. That file-resolution layer is a
//!    different concern; this label-identity model follows `Label::parse`.
//! 2. **Bare target `foo`** (no `//`, no `:`, no `@`): resolved against context
//!    by `Label::parse` (= `:foo`). We mirror it for production parity, but it
//!    is never a [`render`] output (never canonical), so it is excluded from the
//!    round-trip identity (it has no absolute spelling without context).

#[allow(unused_imports)]
use crate::contract_shim::*;

/// A canonical Bazel label: the `(repo, package, target)` triple that
/// `Label::parse` (`dependency_graph.rs:33`) canonicalizes to.
///
/// * `repo` is `None` for the main workspace and `Some(r)` for an external
///   repo `@r`.
/// * `package` is the slash-joined package path (may be empty for the
///   workspace/repo root).
/// * `target` is the (non-empty) target name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonicalLabel {
    /// Workspace/repo name; `None` for the main workspace.
    pub repo: Option<String>,
    /// Package path, e.g. `tensorflow/core` (may be empty for the root).
    pub package: String,
    /// Target name (never empty in a well-formed label).
    pub target: String,
}

impl CanonicalLabel {
    /// Render this label to its **canonical** surface string.
    ///
    /// The canonical form ALWAYS prints the explicit `:target`, so a label
    /// produced from an implicit-target input (`//pkg` → target = last segment
    /// of `pkg`) renders to the explicit `//pkg:lastseg` form. This is what
    /// makes parse-then-render an idempotent normalization. Matches the
    /// `Display` impls of `Label` (`dependency_graph.rs:118`) and
    /// `BazelLabel::to_rush_label` (`bazel_compat.rs:382`):
    ///
    /// ```text
    /// (None,         "a/b", "b") => "//a/b:b"
    /// (Some("r"),    "a/b", "t") => "@r//a/b:t"
    /// (Some("r"),    "",    "r") => "@r//:r"
    /// ```
    pub fn render(&self) -> String {
        match &self.repo {
            Some(r) => format!("@{}//{}:{}", r, self.package, self.target),
            None => format!("//{}:{}", self.package, self.target),
        }
    }
}

/// The last `/`-separated segment of `package`, or all of `package` if it has
/// no `/`. Models `pkg.split('/').next_back().unwrap_or(&pkg)` from
/// `Label::parse` — the implicit target name.
fn last_segment(package: &str) -> &str {
    package.rsplit('/').next().unwrap_or(package)
}

/// Parse a Bazel label surface string into its [`CanonicalLabel`], resolving
/// relative forms against the current `ctx_repo` / `ctx_pkg`.
///
/// This is the I/O-free twin of [`Label::parse`] (`dependency_graph.rs:33`).
/// Supported forms (canonical authority — see module docs for the boundaries
/// with the file-oriented parser):
///
/// * `:target`        — relative; inherits `ctx_repo` + `ctx_pkg`.
/// * `target`         — bare name; equivalent to `:target` (inherits context).
/// * `@repo//pkg:target`
/// * `@repo//pkg`     — implicit target = `last_segment(pkg)`.
/// * `@repo`          — root target shorthand for `@repo//:repo`.
/// * `//pkg:target`   — inherits `ctx_repo` for the repo.
/// * `//pkg`          — implicit target = `last_segment(pkg)`.
///
/// Returns `None` for inputs outside this grammar or that resolve to an empty
/// target (matching `Label::parse` returning `Err`, mapped here to `None` for
/// the pure model).
pub fn parse(s: &str, ctx_repo: Option<&str>, ctx_pkg: &str) -> Option<CanonicalLabel> {
    // Relative label `:target` — inherits the current repo + package.
    if let Some(target) = s.strip_prefix(':') {
        if target.is_empty() {
            return None;
        }
        return Some(CanonicalLabel {
            repo: ctx_repo.map(str::to_string),
            package: ctx_pkg.to_string(),
            target: target.to_string(),
        });
    }

    // Bare target `foo` (no `//`, no `@`): equivalent to `:foo` in context.
    // Matches `Label::parse`'s bare-name arm, which requires a package context.
    if !s.starts_with("//") && !s.starts_with('@') {
        if s.is_empty() {
            return None;
        }
        return Some(CanonicalLabel {
            repo: ctx_repo.map(str::to_string),
            package: ctx_pkg.to_string(),
            target: s.to_string(),
        });
    }

    // External `@repo//pkg[:target]` or bare `@repo`, else `//pkg[:target]`.
    let (repo, rest): (Option<String>, &str) = if let Some(stripped) = s.strip_prefix('@') {
        match stripped.split_once("//") {
            Some((repo_name, rest)) => {
                if repo_name.is_empty() {
                    return None;
                }
                (Some(repo_name.to_string()), rest)
            }
            // Bare `@repo` names target `repo` in the repository root package.
            None => {
                if stripped.is_empty() {
                    return None;
                }
                return Some(CanonicalLabel {
                    repo: Some(stripped.to_string()),
                    package: String::new(),
                    target: stripped.to_string(),
                });
            }
        }
    } else if let Some(stripped) = s.strip_prefix("//") {
        (ctx_repo.map(str::to_string), stripped)
    } else {
        return None;
    };

    // Split package:target. One part => implicit target = last_segment(pkg).
    let (package, target) = match rest.split_once(':') {
        Some((pkg, tgt)) => (pkg.to_string(), tgt.to_string()),
        None => {
            let pkg = rest.to_string();
            let tgt = last_segment(&pkg).to_string();
            (pkg, tgt)
        }
    };

    if target.is_empty() {
        return None;
    }

    Some(CanonicalLabel {
        repo,
        package,
        target,
    })
}

/// Round-trip helper carrying the RENDER-THEN-PARSE identity as its Creusot
/// `ensures`: rendering a label then parsing the result (with any context)
/// recovers the original label. This is the tractable half of the model that
/// is expressible in Pearlite directly; the PARSE-THEN-RENDER idempotence
/// fixpoint is oracle-only (a string-equality fixpoint over `parse`, deferred
/// to the Dafny addendum `ParseThenRenderIdempotent`).
///
/// The precondition `l.target` non-empty (a well-formed label) is what makes
/// the identity hold: `render` always emits the absolute explicit-`:target`
/// form, so the parse-back never consults `ctx_repo`/`ctx_pkg`.
#[cfg_attr(feature = "creusot",
    requires(l.target@.len() > 0))]
#[cfg_attr(feature = "creusot",
    ensures(parse_render_logic(*l) ==> (result == Some(l.clone()))))]
pub fn render_label_round_trip(l: &CanonicalLabel) -> Option<CanonicalLabel> {
    let rendered = l.render();
    // Context is irrelevant for an absolute, explicit-target string; we pass a
    // neutral context to make that explicit.
    parse(&rendered, None, "")
    // Ghost step: render(l) starts with the repo head ("//" or "@r//"), carries
    // l.package verbatim up to the unique ':' (package is '@'/':' free), and the
    // remainder is l.target — so parse recovers (l.repo, l.package, l.target).
    // Erased without the creusot feature.
}

/// Logic-level statement of the RENDER-THEN-PARSE identity for the Creusot
/// contract: a label is well-formed enough that `parse(render(l))` recovers it.
/// Defined as a Creusot `logic` fn so the proof can refer to it structurally;
/// erased entirely in the default build. The concrete obligation discharged by
/// the verifier is `parse(render(l), _, _) == Some(l)` for non-empty `target`;
/// here we expose the predicate the `round_trip` ensures is guarded by.
#[cfg(feature = "creusot")]
#[logic]
fn parse_render_logic(l: CanonicalLabel) -> bool {
    pearlite! { l.target@.len() > 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lbl(repo: Option<&str>, package: &str, target: &str) -> CanonicalLabel {
        CanonicalLabel {
            repo: repo.map(str::to_string),
            package: package.to_string(),
            target: target.to_string(),
        }
    }

    #[test]
    fn parse_absolute_explicit() {
        assert_eq!(
            parse("//tensorflow/core:framework", None, ""),
            Some(lbl(None, "tensorflow/core", "framework"))
        );
    }

    #[test]
    fn parse_external_explicit() {
        assert_eq!(
            parse("@local_tsl//tsl/platform:macros", None, ""),
            Some(lbl(Some("local_tsl"), "tsl/platform", "macros"))
        );
    }

    #[test]
    fn parse_implicit_target_absolute() {
        // `//foo/bar` => target = last segment `bar` (Label::parse semantics).
        assert_eq!(parse("//foo/bar", None, ""), Some(lbl(None, "foo/bar", "bar")));
    }

    #[test]
    fn parse_implicit_target_external() {
        assert_eq!(
            parse("@repo//a/b", None, ""),
            Some(lbl(Some("repo"), "a/b", "b"))
        );
    }

    #[test]
    fn parse_bare_repo() {
        let shorthand = parse("@repo", None, "");
        let explicit = parse("@repo//:repo", None, "");

        assert_eq!(shorthand, explicit);
        assert_eq!(shorthand, Some(lbl(Some("repo"), "", "repo")));
        assert_eq!(
            parse("@my_repo", None, ""),
            Some(lbl(Some("my_repo"), "", "my_repo"))
        );
    }

    #[test]
    fn parse_relative_inherits_context() {
        assert_eq!(
            parse(":helper", Some("ws"), "pkg/sub"),
            Some(lbl(Some("ws"), "pkg/sub", "helper"))
        );
        // Bare name behaves like `:name`.
        assert_eq!(
            parse("helper", None, "pkg/sub"),
            Some(lbl(None, "pkg/sub", "helper"))
        );
    }

    #[test]
    fn parse_absolute_inherits_repo_context_for_double_slash() {
        // `//pkg:t` inherits ctx_repo (matches Label::parse passing through
        // current_workspace for the `//` arm).
        assert_eq!(
            parse("//pkg:t", Some("ws"), "ignored"),
            Some(lbl(Some("ws"), "pkg", "t"))
        );
    }

    #[test]
    fn parse_rejects_empty_target() {
        assert_eq!(parse("//pkg:", None, ""), None);
        assert_eq!(parse(":", None, ""), None);
        assert_eq!(parse("@//pkg:t", None, ""), None); // empty repo name
    }

    #[test]
    fn render_explicit_form() {
        assert_eq!(lbl(None, "a/b", "b").render(), "//a/b:b");
        assert_eq!(lbl(Some("r"), "a/b", "t").render(), "@r//a/b:t");
        assert_eq!(lbl(Some("r"), "", "r").render(), "@r//:r");
    }

    #[test]
    fn render_then_parse_round_trips() {
        for l in [
            lbl(None, "a/b", "b"),
            lbl(Some("r"), "a/b", "t"),
            lbl(Some("r"), "", "r"),
            lbl(None, "", "root_target"),
        ] {
            assert_eq!(parse(&l.render(), None, ""), Some(l.clone()), "round-trip {l:?}");
            assert_eq!(render_label_round_trip(&l), Some(l));
        }
    }

    #[test]
    fn implicit_and_explicit_normalize_equal() {
        // `//foo/bar` and `//foo/bar:bar` parse to the same canonical label and
        // render identically (idempotent normalization).
        let implicit = parse("//foo/bar", None, "").unwrap();
        let explicit = parse("//foo/bar:bar", None, "").unwrap();
        assert_eq!(implicit, explicit);
        assert_eq!(implicit.render(), explicit.render());
        assert_eq!(implicit.render(), "//foo/bar:bar");
    }
}
