// topo-roots-v1 — abstract build-order model (Dafny spec laboratory / golden model)
//
// This is the ADDENDUM layer of rush's formal-verification track (see the MACP
// agentic-maintenance extension, Dafny-addendum section). It is NOT the Rust
// implementation and does NOT verify the Rust binary. It is the abstract,
// independently-verified specification of `topological_sort_from_roots`:
//
//   given a graph `g` (adjacency: g[n] = nodes n depends on) and a set of
//   `roots`, a valid build order lists exactly the reachable nodes, with no
//   duplicates, and every dependency placed BEFORE the node that needs it.
//
// The Creusot contract on crates/rush-verified-core/src/topo_roots.rs MIRRORS
// these properties on the real Rust over usize index-graphs; the differential
// gate (topo_roots_dafny_diff.rs) diffs the Rust output against a golden
// oracle compiled from this model (`dafny build -t:py`) — so this model is
// not decorative. Dafny CLI + Z3 are not yet installed on the build hosts;
// `dafny verify` / `dafny build` are the documented next layer (see
// ci/dafny-toolchain.lock). Until then this file is the stabilized spec.
//
// Scope note: ValidTopo is satisfiable only for the ACYCLIC reachable
// subgraph (a cycle admits no order with deps-before-dependents). Cycle
// detection/behaviour is a separate property, modelled separately when the
// link-plan scope (cc-link-plan-v1) lands.

// Every adjacency target is a valid node index.
predicate InBounds(g: seq<seq<int>>) {
  forall n :: 0 <= n < |g| ==>
    forall d :: d in g[n] ==> 0 <= d < |g|
}

// Nodes reachable from `roots` within `fuel` expansion steps. |g| steps
// saturate reachability (no simple path is longer than the node count).
function ReachN(g: seq<seq<int>>, roots: seq<int>, fuel: nat): set<int>
  decreases fuel
{
  if fuel == 0 then
    set r | r in roots && 0 <= r < |g| :: r
  else
    var prev := ReachN(g, roots, fuel - 1);
    prev + set d | 0 <= d < |g| && exists n :: n in prev && d in g[n] :: d
}

function Reachable(g: seq<seq<int>>, roots: seq<int>): set<int> {
  ReachN(g, roots, |g|)
}

predicate NoDup(s: seq<int>) {
  forall i, j :: 0 <= i < j < |s| ==> s[i] != s[j]
}

// Position of x in s, or -1 if absent.
function Index(s: seq<int>, x: int): int {
  if |s| == 0 then -1
  else if s[0] == x then 0
  else var r := Index(s[1..], x); if r < 0 then -1 else r + 1
}

// The golden property. order is a valid build order for (g, roots) iff:
//   (1) no node appears twice,
//   (2) the order's node set is EXACTLY the reachable set (completeness — the
//       "returns an empty list" / "drops a reachable node" bug class is
//       rejected here), and
//   (3) every dependency precedes the node that needs it.
predicate ValidTopo(g: seq<seq<int>>, roots: seq<int>, order: seq<int>)
  requires InBounds(g)
{
  var r := Reachable(g, roots);
  && NoDup(order)
  && (set x | x in order :: x) == r
  && (forall n, d :: n in r && d in g[n] ==>
        d in r && Index(order, d) < Index(order, n))
}

// Proof target (to be discharged once the Dafny toolchain is installed): a
// constructive TopSort whose result satisfies ValidTopo for any acyclic,
// in-bounds graph. The method body (Kahn / DFS post-order) + the termination
// and completeness lemmas are the work the verifier checks; the SPEC above is
// what the Creusot contract and the differential oracle bind to.
//
// method TopSort(g: seq<seq<int>>, roots: seq<int>) returns (order: seq<int>)
//   requires InBounds(g)
//   requires Acyclic(g, Reachable(g, roots))
//   ensures ValidTopo(g, roots, order)
