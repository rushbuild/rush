// cycle-detect-v1 — abstract cycle-detection model (Dafny spec laboratory / golden model)
//
// This is the ADDENDUM layer of rush's formal-verification track (see the MACP
// agentic-maintenance extension, Dafny-addendum section). It is NOT the Rust
// implementation and does NOT verify the Rust binary. It is the abstract,
// independently-verified specification of `detect_cycle`:
//
//   given a graph `g` (adjacency: g[n] = nodes n depends on) and a set of
//   `roots`, decide whether the subgraph REACHABLE FROM `roots` contains a
//   directed cycle, and if so return a witness cycle (a non-empty sequence of
//   reachable, in-bounds nodes whose consecutive entries are edges and whose
//   last entry edges back to the first).
//
// The Creusot contract on crates/rush-verified-core/src/cycle_detect.rs MIRRORS
// the tractable part of these properties on the real Rust over usize
// index-graphs (Some(c) ==> c is non-empty and in-bounds); the
// `cycle-exists IFF Some` biconductional is the heavier obligation discharged
// by the differential / proptest oracle (tests/cycle_detect_prop.rs), the same
// Pearlite-vs-oracle split topo-roots-v1 uses. Dafny CLI + Z3 are not yet
// installed on the build hosts; `dafny verify` / `dafny build` are the
// documented next layer (see ci/dafny-toolchain.lock). Until then this file is
// the stabilized spec.
//
// DIRECT SIBLING of verification/dafny/topo_roots/TopSort.dfy: same
// seq<seq<int>> adjacency, same InBounds, the SAME fuel-bounded Reachable, and
// the SAME reachable-from-roots scope restriction (a cycle UNREACHABLE from
// roots does not block the build, so it is out of scope — mirroring
// ValidTopo's reachable restriction). The connection is explicit below:
// ValidTopo is satisfiable (TopSort succeeds) IFF DetectCycle returns None.
// This completes the graph story TopSort.dfy left open in its scope note
// ("Cycle detection/behaviour is a separate property, modelled separately").

// Every adjacency target is a valid node index. (Identical to TopSort.dfy.)
predicate InBounds(g: seq<seq<int>>) {
  forall n :: 0 <= n < |g| ==>
    forall d :: d in g[n] ==> 0 <= d < |g|
}

// Nodes reachable from `roots` within `fuel` expansion steps. |g| steps
// saturate reachability (no simple path is longer than the node count).
// (Identical to TopSort.dfy's ReachN — the shared reachability kernel.)
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

// A single directed edge n -> d in the graph.
predicate IsEdge(g: seq<seq<int>>, n: int, d: int) {
  0 <= n < |g| && d in g[n]
}

// `cyc` is a valid witness cycle WITHIN the reachable subgraph:
//   (1) non-empty;
//   (2) every entry is in-bounds AND reachable from roots (scope match — a
//       cycle entirely outside the reachable subgraph is NOT a valid witness);
//   (3) consecutive entries are edges: cyc[i] -> cyc[i+1]; and
//   (4) the last entry edges back to the first (cyc[|cyc|-1] -> cyc[0]),
//       closing the loop. (A length-1 cycle is the self-loop cyc[0] -> cyc[0].)
predicate IsValidCycle(g: seq<seq<int>>, roots: seq<int>, cyc: seq<int>) {
  var r := Reachable(g, roots);
  && |cyc| >= 1
  && (forall i :: 0 <= i < |cyc| ==> 0 <= cyc[i] < |g| && cyc[i] in r)
  && (forall i :: 0 <= i < |cyc| - 1 ==> IsEdge(g, cyc[i], cyc[i + 1]))
  && IsEdge(g, cyc[|cyc| - 1], cyc[0])
}

// There EXISTS a directed cycle within the reachable subgraph. Stated via the
// witness predicate: some valid cycle exists. (Equivalently: some reachable
// node lies on a directed cycle all of whose nodes are reachable — since the
// reachable subgraph is closed under edges, any cycle through a reachable node
// is entirely reachable.)
predicate HasCycleInReachable(g: seq<seq<int>>, roots: seq<int>) {
  exists cyc :: IsValidCycle(g, roots, cyc)
}

// Proof target (to be discharged once the Dafny toolchain is installed): a
// constructive DetectCycle whose result is a faithful decision procedure for
// HasCycleInReachable, returning a witness on the positive side.
//
//   - returns Some(c) with IsValidCycle(g, roots, c) AND HasCycleInReachable, or
//   - returns None AND !HasCycleInReachable.
//
// i.e. result.Some? <==> HasCycleInReachable(g, roots), and on the Some? side
// the carried witness is a genuine reachable cycle. The method body (Kahn's
// residual to find the cyclic node set, then a DFS walk over that residual to
// extract the closing path — exactly the real Rust discipline) plus the
// soundness/completeness lemmas are the work the verifier checks; the SPEC
// above is what the Creusot contract and the differential oracle bind to.
//
// datatype Opt<T> = None | Some(value: T)
//
// method DetectCycle(g: seq<seq<int>>, roots: seq<int>) returns (res: Opt<seq<int>>)
//   requires InBounds(g)
//   ensures res.Some? <==> HasCycleInReachable(g, roots)
//   ensures res.Some? ==> IsValidCycle(g, roots, res.value)

// ───────────────────────────────────────────────────────────────────────────
// Connection to TopSort.dfy (the graph story, completed).
//
// TopSort.dfy's ValidTopo is satisfiable ONLY for the acyclic reachable
// subgraph (its scope note). The two kernels are therefore exact complements
// over the reachable subgraph: the reachable subgraph admits a valid build
// order IFF it has no cycle. This is the cross-check the Rust oracle binds in
// tests/cycle_detect_prop.rs:
//
//     detect_cycle(g, roots) == None  <==>  topological_sort_from_roots(g, roots) == Some(_)
//
// lemma TopoSatisfiableIffNoCycle(g: seq<seq<int>>, roots: seq<int>)
//   requires InBounds(g)
//   ensures (exists order :: ValidTopo(g, roots, order)) <==> !HasCycleInReachable(g, roots)
// ───────────────────────────────────────────────────────────────────────────
