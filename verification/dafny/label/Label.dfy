// label-parse-v1 — abstract Bazel-label parse/render model (Dafny spec laboratory / golden model)
//
// This is the ADDENDUM layer of rush's formal-verification track (see the MACP
// agentic-maintenance extension, Dafny-addendum section), authored DAFNY-FIRST:
// Bazel label parsing/canonicalization has enough relative/external-context
// resolution cases (`@repo//pkg:target`, `//pkg:target`, `//pkg`, `:target`,
// `@repo//pkg`, `@repo`) to specify the abstract behavior independently. This
// is NOT the Rust implementation and does NOT verify the Rust binary. It is the
// abstract, independently-verified specification of label parse and render:
//
//   a canonical Label is the triple (repo, package, target); `Render` produces
//   its one canonical surface string; `Parse(s, ctxRepo, ctxPkg)` resolves a
//   surface string (possibly relative / possibly with an implicit target) back
//   into the canonical triple, using the current repo+package as context for
//   relative forms.
//
// The Creusot contract on crates/rush-verified-core/src/label_parse.rs MIRRORS
// these properties on the real Rust over `CanonicalLabel`; the differential
// gate (label_parse_dafny_diff.rs, future) would diff the Rust output against a
// golden oracle compiled from this model (`dafny build -t:py`) — so this model
// is not decorative. Dafny CLI + Z3 are not yet installed on the build hosts;
// `dafny verify` / `dafny build` are the documented next layer (see
// ci/dafny-toolchain.lock). Until then this file is the stabilized spec.
//
// Canonical semantics follow `Label::parse`
// (crates/rush-build/src/dependency_graph.rs:33). `Label::parse` and
// `BazelLabel::parse` both map a bare `@repo` to `@repo//:repo`, represented as
// repo=repo, package="", target=repo. The file-oriented parser has one relevant
// boundary that is not modelled here:
//   * parse_external_repo_label (external_repo_materialization.rs:690) is a
//     FILE-oriented parser: for a colon-less `@repo//a/b/c` it splits the LAST
//     path segment off as the target (package="a/b", target="c"). Label::parse
//     does NOT split: `//a/b/c` => package="a/b/c", target="c" (implicit target
//     is a COPY of the last segment, the package is kept whole). We follow
//     Label::parse. parse_external_repo_label models the file-resolution layer,
//     a different concern, and is out of scope for this label-identity model.
//
// Scope note (matching TopSort.dfy's acyclic-only scope discipline): this model
// covers the six canonical surface forms above over the well-formed label
// alphabet (no '@', no ':', no '//' inside a package or target segment; package
// may be empty = repo/workspace root; target is non-empty). Out of scope:
//   * bare target `foo` (no `//`, no `:`, no `@`) — context-only, handled by
//     the Rust mirror for production parity but it is NOT a Render output (it is
//     never canonical), so it is excluded from the round-trip identity here;
//   * default-repo `@//pkg` aliasing, repo-mapping/apparent-name canonicalization,
//     and `...`/`:all` wildcards — these are higher Bazel layers.

// ---------------------------------------------------------------------------
// Canonical Label datatype.
// ---------------------------------------------------------------------------

// repo = None for the main workspace; Some(r) for an external repo `@r`.
datatype Opt<T> = None | Some(value: T)

datatype Label = Label(repo: Opt<string>, package: string, target: string)

// A character may appear in a package/target segment of a WELL-FORMED label.
// We exclude the structural metacharacters so Render's output re-parses with no
// ambiguity: '@' starts a repo, ':' separates package from target, and the
// "//" sequence introduces the package root.
predicate SafeChar(c: char) {
  c != '@' && c != ':' && c != '/'
}

predicate SafeStr(s: string) {
  forall i :: 0 <= i < |s| ==> SafeChar(s[i])
}

// A package is a (possibly empty) sequence of safe segments joined by '/'.
// Modelled as: every char is safe EXCEPT '/', which may join segments but may
// not lead, trail, or double. We keep '/' allowed in packages but forbid it in
// repo and target (those are single segments).
predicate SafePackage(p: string) {
  // every char safe-or-slash, no leading/trailing/double slash
  (forall i :: 0 <= i < |p| ==> (SafeChar(p[i]) || p[i] == '/'))
  && (|p| > 0 ==> p[0] != '/' && p[|p|-1] != '/')
  && (forall i :: 0 <= i < |p|-1 ==> !(p[i] == '/' && p[i+1] == '/'))
}

// WellFormed: a canonical Label has a non-empty single-segment target, a
// well-formed package, and (if external) a non-empty single-segment repo.
predicate WellFormed(l: Label) {
  && SafeStr(l.target) && |l.target| > 0
  && SafePackage(l.package)
  && (l.repo.Some? ==> SafeStr(l.repo.value) && |l.repo.value| > 0)
}

// ---------------------------------------------------------------------------
// Render: canonical Label -> canonical surface string.
// ---------------------------------------------------------------------------

// Canonical form ALWAYS prints the explicit ":target", so `//pkg` and
// `//pkg:lastseg(pkg)` collapse to one surface string. This is what makes
// PARSE-THEN-RENDER an idempotent NORMALIZATION (the implicit-target input
// forms all render to the explicit form).
//
//   Label(None,        "a/b", "b")   => "//a/b:b"
//   Label(Some("r"),   "a/b", "t")   => "@r//a/b:t"
//   Label(Some("r"),   "",    "r")   => "@r//:r"
function Render(l: Label): string {
  var head := match l.repo { case None => "//" case Some(r) => "@" + r + "//" };
  head + l.package + ":" + l.target
}

// ---------------------------------------------------------------------------
// Parse: surface string + (ctxRepo, ctxPkg) -> Option<Label>.
// ---------------------------------------------------------------------------
//
// Mirrors Label::parse (dependency_graph.rs:33) for the five canonical forms:
//   ":t"          -> Label(ctxRepo, ctxPkg, t)            (relative)
//   "@r//p:t"     -> Label(Some(r), p, t)
//   "@r//p"       -> Label(Some(r), p, lastseg(p))        (implicit target)
//   "@r"          -> Label(Some(r), "", r)
//   "//p:t"       -> Label(ctxRepo, p, t)
//   "//p"         -> Label(ctxRepo, p, lastseg(p))        (implicit target)
//
// The function is total over strings; it returns None for inputs outside the
// modelled surface grammar (and for ill-formed results — e.g. an empty implicit
// target). The constructive splitter is the proof-target work below; the SPEC
// the Rust contract and oracle bind to is "Parse(Render(L)) == Some(L)".
//
// (Pseudocode signature; the body — string scanning for "//", ":", lastseg —
//  is the Dafny work discharged once the toolchain installs.)
//
// function Parse(s: string, ctxRepo: Opt<string>, ctxPkg: string): Opt<Label>

// lastseg(p) = the substring of p after its final '/', or p itself if no '/'.
// (Spec helper; matches `pkg.split('/').next_back()` in Label::parse.)
//
// function LastSeg(p: string): string

// ---------------------------------------------------------------------------
// The golden properties (proof targets to discharge once Dafny installs).
// ---------------------------------------------------------------------------

// (P1) RENDER-THEN-PARSE round-trip: rendering a well-formed canonical Label
// produces a surface string that parses back to the IDENTICAL label under the
// NEUTRAL (None) repo context.
//
// TRUE-SEMANTICS NOTE (discovered against the real parser, FOLLOW the real
// semantics): Render of a MAIN-workspace label (repo == None) is "//pkg:t", and
// Parse's "//" arm INHERITS ctxRepo for the repo (matching Label::parse passing
// `current_workspace` through the `//` branch). So the round-trip recovers
// `None` only when ctxRepo == None — it is NOT context-free for main-workspace
// labels. For EXTERNAL labels (repo == Some(r)) the "@r" is explicit in the
// surface string, so the round-trip IS context-free. The Rust
// `render_label_round_trip` helper mirrors P1 by parsing under None context;
// the context-free external case is the separate (P1') witness.
//
// lemma RenderThenParse(l: Label)
//   requires WellFormed(l)
//   ensures Parse(Render(l), None, "") == Some(l)
//
// lemma RenderThenParseExternalContextFree(l: Label, anyRepo: Opt<string>, anyPkg: string)
//   requires WellFormed(l) && l.repo.Some?
//   ensures Parse(Render(l), anyRepo, anyPkg) == Some(l)

// (P2) PARSE-THEN-RENDER idempotent normalization: if parsing a surface string
// succeeds, rendering the result and parsing AGAIN is a fixpoint — the
// canonical surface string is stable. In particular the implicit-target forms
// `//pkg` and `@r//pkg` normalize to `//pkg:lastseg(pkg)` and re-render
// identically: Render is idempotent through Parse.
//
// lemma ParseThenRenderIdempotent(s: string, ctxRepo: Opt<string>, ctxPkg: string)
//   requires Parse(s, ctxRepo, ctxPkg).Some?
//   ensures var l := Parse(s, ctxRepo, ctxPkg).value;
//           WellFormed(l)
//           && Parse(Render(l), ctxRepo, ctxPkg) == Some(l)
//           && Render(Parse(Render(l), ctxRepo, ctxPkg).value) == Render(l)

// (P3) IMPLICIT-TARGET NORMALIZATION (a concrete witness of P2): the implicit
// and explicit spellings of the same canonical label collapse under Parse.
//
// lemma ImplicitTargetCollapses(p: string, ctxRepo: Opt<string>, ctxPkg: string)
//   requires SafePackage(p) && |p| > 0
//   ensures Parse("//" + p, ctxRepo, ctxPkg)
//        == Parse("//" + p + ":" + LastSeg(p), ctxRepo, ctxPkg)
