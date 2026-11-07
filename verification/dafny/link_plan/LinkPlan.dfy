// link-plan-v1 — abstract C++ test link-PLAN assembly model (Dafny spec
// laboratory / golden model)
//
// This is the ADDENDUM layer of rush's formal-verification track (see the MACP
// agentic-maintenance extension, Dafny-addendum section). It is NOT the Rust
// implementation and does NOT verify the Rust binary. It is the abstract,
// independently-verified specification of the PURE link-argument assembly that
// lives inside `CcTestInstance::link_binary`
// (crates/rush-rules/src/rules_cc.rs ≈ line 3289-3403):
//
//   given the test's object files, the dep libraries each tagged
//   (path, alwayslink), the pass-through linker flags, and a `linksTestMain`
//   bit, assemble the ld/g++ argument tail in the order ld needs:
//   alwayslink (whole-archive) libs FIRST, then the normal libs wrapped in a
//   single --start-group/--end-group, then the pass-through flags (with the
//   tf_cc_test benchmark arm appended when test_main is in the closure), then
//   the output args. The `dedup_archive_link_flags` sub-kernel (link-dedup-v1)
//   sits downstream of THIS assembly and is modelled separately.
//
// The Creusot contract on crates/rush-verified-core/src/link_plan.rs MIRRORS
// the tractable conjuncts here (the whole-archive-before-normal ordering and a
// bucket-membership predicate) on the real Rust; the heavier set-level
// conjuncts (P2 alwayslink-wins / no-double-list, P5 completeness) are
// discharged by the proptest oracle (tests/link_plan_prop.rs), the same
// Pearlite-vs-oracle split as link-dedup-v1 / topo-roots-v1 / nested-set-v1.
// Dafny CLI + Z3 are not yet installed on the build hosts; `dafny verify` /
// `dafny build` are the documented next layer (see ci/dafny-toolchain.lock).
// Until then this file is the stabilized spec the oracle binds to.
//
// REAL-SEMANTICS NOTES (extracted from the production code; a naively-assumed
// invariant that was FALSE here is documented rather than asserted):
//   * GROUP CONDITION (P4): production wraps the normal libs in
//     --start-group/--end-group iff there is AT LEAST ONE normal lib
//     (`if !normal_libs.is_empty()`), NOT ≥2. We mirror `>= 1` EXACTLY.
//   * ALWAYSLINK-WINS (P2): a path advertised alwayslink by ANY dep is
//     whole-archived and is removed from the normal bucket
//     (`normal_libs.retain(|p| !whole_archive_libs.contains(p))`), so it is
//     never double-listed even if another dep advertised the same path plain.
//   * FIRST-BUCKET-WINS de-dup: within each bucket production keeps the first
//     occurrence of a path (the `!contains` guards), so each distinct path is
//     emitted at most once per bucket; combined with P2 a path lands in
//     exactly one bucket.
//   * TESTMAIN (P6): when `linksTestMain`, production appends `-lbenchmark`
//     and `-lpthread` to the pass-through region (guarded so `-lbenchmark` is
//     not duplicated), and NEVER `-lbenchmark_main` (that defines its own
//     main(), colliding with test_main.o's main()). This is the exact arm
//     whose ABSENCE was tonight's latent `-lbenchmark` gap.

// ---------------------------------------------------------------------------
// Abstract model. Paths are modelled as ints (path-id equivalence classes);
// two dep libs with the same id denote the same archive on disk.
// ---------------------------------------------------------------------------

datatype DepLib = DepLib(path: int, alwayslink: bool)

// A link token is one of the structural markers, an object, a normal/whole
// archive path, or a pass-through flag. The abstract output is a seq of these.
datatype Tok =
    | Obj(o: int)              // an object file
    | WAOpen                   // -Wl,--whole-archive
    | WAClose                  // -Wl,--no-whole-archive
    | Lib(p: int)              // an archive path on the line (whole or normal)
    | GroupStart               // -Wl,--start-group
    | GroupEnd                 // -Wl,--end-group
    | Pass(f: int)             // a pass-through flag (flag-id)
    | Benchmark                // -lbenchmark  (testmain arm)
    | BenchPthread             // -lpthread    (testmain arm)

// Does `p` appear as an alwayslink dep ANYWHERE in `deps`?
predicate IsAlwayslinkAnywhere(deps: seq<DepLib>, p: int) {
    exists i :: 0 <= i < |deps| && deps[i].path == p && deps[i].alwayslink
}

// The set of distinct paths that should be whole-archived: every path that is
// alwayslink in at least one dep.
function WholeArchiveSet(deps: seq<DepLib>): set<int> {
    set i | 0 <= i < |deps| && deps[i].alwayslink :: deps[i].path
}

// The set of distinct paths that land in the normal group: every dep path that
// is NOT whole-archived anywhere (alwayslink-wins).
function NormalSet(deps: seq<DepLib>): set<int> {
    set i | 0 <= i < |deps| && !IsAlwayslinkAnywhere(deps, deps[i].path)
        :: deps[i].path
}

// All distinct dep paths.
function AllPaths(deps: seq<DepLib>): set<int> {
    set i | 0 <= i < |deps| :: deps[i].path
}

// Position of the first token satisfying `pred`, or -1.
function FirstIndex(out: seq<Tok>, pred: Tok -> bool): int {
    if |out| == 0 then -1
    else if pred(out[0]) then 0
    else var r := FirstIndex(out[1..], pred); if r < 0 then -1 else r + 1
}

// Whether `out` contains a token satisfying `pred`.
predicate Contains(out: seq<Tok>, pred: Tok -> bool) {
    exists i :: 0 <= i < |out| && pred(out[i])
}

// A whole-archive emission of path `p` is the triple [WAOpen, Lib(p), WAClose].
predicate WrappedAt(out: seq<Tok>, i: int, p: int) {
    0 <= i && i + 2 < |out|
        && out[i] == WAOpen && out[i+1] == Lib(p) && out[i+2] == WAClose
}

// Index of the unique GroupStart / GroupEnd (or -1 when absent).
function GroupStartIdx(out: seq<Tok>): int { FirstIndex(out, (t: Tok) => t == GroupStart) }
function GroupEndIdx(out: seq<Tok>): int { FirstIndex(out, (t: Tok) => t == GroupEnd) }

// Is token `t` a "whole-archive region" token (an open/lib/close belonging to a
// whole-archive emission)? We approximate the region as: any WAOpen/WAClose, or
// a Lib(p) whose path is whole-archived.
predicate InWholeArchiveRegion(deps: seq<DepLib>, t: Tok) {
    t == WAOpen || t == WAClose
        || (t.Lib? && t.p in WholeArchiveSet(deps))
}

predicate IsNormalLibTok(deps: seq<DepLib>, t: Tok) {
    t.Lib? && t.p in NormalSet(deps) && !(t.p in WholeArchiveSet(deps))
}

predicate IsPassTok(t: Tok) {
    t.Pass? || t == Benchmark || t == BenchPthread
}

// ---------------------------------------------------------------------------
// ValidLinkPlan — the golden property. `out` is a valid assembly for the
// inputs iff P1..P6 hold. (Objects precede everything; output args are modelled
// as the implicit tail after `out`, so P3's "before the output args" is the
// "everything in `out` is link material" envelope.)
// ---------------------------------------------------------------------------
predicate ValidLinkPlan(
    objects: seq<int>, deps: seq<DepLib>, passthrough: seq<int>,
    linksTestMain: bool, out: seq<Tok>)
{
    var wa := WholeArchiveSet(deps);
    var nm := NormalSet(deps);

    // P1 BUCKET — every alwayslink path is emitted wrapped in a whole-archive
    // triple; every normal-bucket path is emitted as a bare Lib in the group.
    && (forall p :: p in wa ==> exists i :: WrappedAt(out, i, p))
    && (forall p :: p in nm ==> Lib(p) in out)

    // P2 ALWAYSLINK-WINS / no-double-list — a whole-archived path is NOT also
    // present as a normal (non-whole-archive) Lib, and the two buckets are
    // disjoint as sets.
    && (wa * nm == {})
    && (forall p :: p in wa ==>
            forall i :: 0 <= i < |out| && IsNormalLibTok(deps, out[i]) ==> out[i].p != p)

    // P3 ORDER — every whole-archive-region token precedes every normal-lib
    // token, which precedes every pass-through-region token. (objects, modelled
    // separately, lead; the output args trail after `out`.)
    && (forall i, j :: 0 <= i < |out| && 0 <= j < |out|
            && InWholeArchiveRegion(deps, out[i]) && IsNormalLibTok(deps, out[j])
            ==> i < j)
    && (forall i, j :: 0 <= i < |out| && 0 <= j < |out|
            && IsNormalLibTok(deps, out[i]) && IsPassTok(out[j])
            ==> i < j)
    && (forall i, j :: 0 <= i < |out| && 0 <= j < |out|
            && InWholeArchiveRegion(deps, out[i]) && IsPassTok(out[j])
            ==> i < j)

    // P4 GROUP — the --start-group/--end-group bracket is present IFF there is
    // ≥1 normal lib (mirroring `if !normal_libs.is_empty()` EXACTLY), and when
    // present it brackets all the normal libs (start before, end after).
    && (Contains(out, (t: Tok) => t == GroupStart) <==> |nm| >= 1)
    && (Contains(out, (t: Tok) => t == GroupEnd) <==> |nm| >= 1)
    && (|nm| >= 1 ==>
            (forall i :: 0 <= i < |out| && IsNormalLibTok(deps, out[i])
                ==> GroupStartIdx(out) < i < GroupEndIdx(out)))

    // P5 COMPLETENESS — every distinct dep path appears at least once as a Lib.
    && (forall p :: p in AllPaths(deps) ==> Lib(p) in out)

    // P6 TESTMAIN — -lbenchmark is in the pass-through region IFF linksTestMain,
    // and -lbenchmark_main never appears (it is unrepresentable: there is no
    // BenchmarkMain token, which is the model-level statement of "never").
    && (Benchmark in out <==> linksTestMain)
}

// Proof target (to be discharged once the Dafny toolchain is installed): a
// constructive AssembleLinkPlan whose result satisfies ValidLinkPlan for any
// inputs. The method body (bucket-partition with alwayslink-wins retain, then
// emit whole-archive triples, then the optional group, then pass-through +
// benchmark arm) and the ordering/completeness lemmas are the work the verifier
// checks; the SPEC above is what the Creusot contract and the differential
// oracle bind to.
//
// method AssembleLinkPlan(
//     objects: seq<int>, deps: seq<DepLib>, passthrough: seq<int>,
//     linksTestMain: bool) returns (out: seq<Tok>)
//   ensures ValidLinkPlan(objects, deps, passthrough, linksTestMain, out)
//
// Key lemmas the body needs:
//   * BucketDisjoint: WholeArchiveSet(deps) * NormalSet(deps) == {}
//     (P2 — follows from NormalSet's `!IsAlwayslinkAnywhere` guard).
//   * BucketCover:   WholeArchiveSet(deps) + NormalSet(deps) == AllPaths(deps)
//     (P5 — every path is alwayslink-somewhere or not).
//   * EmitOrder:     the three-phase emission places all WA-region tokens at
//     indices < all normal-lib indices < all pass-region indices (P3).
//   * GroupBracket:  start/end are pushed iff |NormalSet| >= 1 and around the
//     normal libs (P4).
