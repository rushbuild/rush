// nested-set-walk-v1 — abstract NestedSet flatten model (Dafny spec laboratory / golden model)
//
// This is the ADDENDUM layer of rush's formal-verification track (see the MACP
// agentic-maintenance extension, Dafny-addendum section). It is NOT the Rust
// implementation and does NOT verify the Rust binary. It is the abstract,
// independently-verified specification of the NestedSet dedup walk — the
// preorder DFS that flattens a `NestedSet<T>` to a sequence of items.
//
// The production walk is `NestedSetIter::next` in
// crates/rush-rules/src/nested_set.rs:158 (a preorder DFS over a DAG of nodes;
// each node owns `direct` items + `transitive` child sets; nodes are deduped on
// Arc POINTER identity so a node reached through multiple paths/diamonds is
// expanded exactly once — the O(unique) bound, not O(N^2)). The Creusot
// contract on crates/rush-verified-core/src/nested_set_walk.rs MIRRORS the
// tractable properties on the real Rust over index-DAGs (node-IDs model the Arc
// pointers); the proptest oracle (tests/nested_set_walk_prop.rs) witnesses the
// completeness + dedup + order conjuncts and diffs against a golden walk.
// Dafny CLI + Z3 are not yet installed on the build hosts; `dafny verify` /
// `dafny build` are the documented next layer (see ci/dafny-toolchain.lock).
// Until then this file is the stabilized spec.
//
// THE ABSTRACTION. A NestedSet is a DAG over node-IDs `0 <= n < |dag|`. Node `n`
// is `dag[n] == Node(direct, children)` where `direct` is a seq of element-IDs
// (declaration order) and `children` is a seq of child node-IDs (declaration
// order). The walk flattens the DAG rooted at `root` to a seq of element-IDs.
//
// TRUE TRAVERSAL SEMANTICS (extracted from nested_set.rs:158-191 — match these
// EXACTLY):
//   * PREORDER, DIRECT-BEFORE-CHILDREN: a node emits ALL of its own `direct`
//     items (in slice order) BEFORE descending into any child.
//   * DECLARATION ORDER among children: the first-declared child is visited
//     first. (The Rust pushes children in REVERSE onto a stack so the
//     first-declared ends up on top — the observable order is declaration
//     order.)
//   * NODE-IDENTITY DEDUP, EXPAND-ONCE, GROUP-SCHEDULING: each distinct NODE is
//     expanded exactly once. A node is marked "seen" the moment it is first
//     SCHEDULED (pushed), not when expanded. Crucially, when a node is expanded
//     the iterator schedules ALL of its children AT ONCE (its pop+`push_if_new`
//     loop marks every child before descending into any), so a child that is
//     ALSO a sibling's child is fixed in place by the FIRST node that lists it
//     in declaration order — NOT by whichever sibling's recursive subtree would
//     reach it first. This is a TEXTBOOK-DFS DISCREPANCY discovered while
//     building the oracle: a naive recursive mark-then-descend walk produces a
//     DIFFERENT order than the real iterator on a cross-edged DAG (e.g.
//     node0->[1,2,3], node1->[3]: the iterator emits node2 then node3 in
//     node0's child order; recursion would let node1 "own" node3). The model
//     below uses a FRONTIER (group-scheduling) discipline to match the iterator
//     exactly. This is Arc-pointer identity in Rust, modelled as node-ID
//     identity here.
//   * NO ELEMENT-VALUE DEDUP. The walk dedups NODES, not element values. If the
//     SAME element-ID appears in the `direct` slice of two DISTINCT visited
//     nodes, it is emitted TWICE (confirmed by the production test
//     `ordered_diamond_dedups_nodes_not_values`). The "no-dup" property of the
//     flatten is therefore over NODES (each node's direct block appears once),
//     NOT over element values. The Rust mirror + oracle state the node-level
//     invariant; element multiplicity is the FAITHFUL truth, not a bug.
//
// Scope note: NestedSets are DAGs BY CONSTRUCTION (immutable after build, edges
// point only to already-built children, so no cycle can form). We model the
// acyclic case; `WalkN` is fuel-bounded so the spec is total even on a
// hypothetical cyclic input (it then simply stops expanding at the fuel bound —
// the production walk's `seen` set likewise guarantees termination on any input
// because each node is expanded at most once).

datatype Node = Node(direct: seq<int>, children: seq<int>)

// Every child index of every node is a valid node index.
predicate InBounds(dag: seq<Node>) {
  forall n :: 0 <= n < |dag| ==>
    forall c :: c in dag[n].children ==> 0 <= c < |dag|
}

// ---------------------------------------------------------------------------
// Reachability over the NODE-DAG (mirrors TopSort.dfy's ReachN fuel pattern).
// ReachNodesN(dag, root, fuel) = the set of node-IDs reachable from `root` in
// at most `fuel` child-hops. |dag| hops saturate (no simple path is longer than
// the node count).
// ---------------------------------------------------------------------------
function ReachNodesN(dag: seq<Node>, root: int, fuel: nat): set<int>
  decreases fuel
{
  if !(0 <= root < |dag|) then {}
  else if fuel == 0 then {root}
  else
    var prev := ReachNodesN(dag, root, fuel - 1);
    prev + set d | 0 <= d < |dag| && exists n :: n in prev && d in dag[n].children :: d
}

function ReachNodes(dag: seq<Node>, root: int): set<int> {
  ReachNodesN(dag, root, |dag|)
}

// The MULTISET of element-IDs the walk must emit: every direct element of every
// reachable NODE, counted once per reachable node it appears in (NO value
// dedup — value multiplicity across distinct nodes is faithful). Expressed as a
// set of (node, position) pairs projected to their element so completeness can
// be stated without a multiset library: an element-occurrence is identified by
// the (reachable node, index-in-its-direct-slice) that produced it.
function ReachableOccurrences(dag: seq<Node>, root: int): set<(int, int)> {
  set n, i | n in ReachNodes(dag, root) && 0 <= i < |dag[n].direct| :: (n, i)
}

predicate NoDup<X(==)>(s: seq<X>) {
  forall i, j :: 0 <= i < j < |s| ==> s[i] != s[j]
}

// ---------------------------------------------------------------------------
// The constructive walk, as a SPEC function, used to STATE the golden order.
// `Flatten` is the reference preorder / direct-before-children /
// declaration-order walk with node-identity dedup and GROUP-SCHEDULING — the
// faithful model of NestedSetIter (see header).
//
// We model the iterator's stack as a `frontier` seq of scheduled-but-not-yet-
// expanded node-IDs (front = next to expand). To expand the front node we emit
// its direct block, compute its NEW (unvisited) children in declaration order
// as a GROUP, mark them all visited, and splice that group to the FRONT of the
// frontier. (The Rust uses a LIFO stack with reverse-push; prepending the
// in-declaration-order child group to a front-consumed frontier yields the same
// visit order — the difference from a recursive DFS is precisely that the whole
// sibling group is marked before any deeper expansion.)
//
// `fuel` bounds the loop; |dag| steps suffice because each node is expanded at
// most once (a node enters the frontier only when first marked visited).
// ---------------------------------------------------------------------------

// New children of `kids` not yet in `visited`, in declaration order, deduping
// repeats within `kids` itself.
function NewChildren(dag: seq<Node>, kids: seq<int>, visited: set<int>): seq<int>
  decreases |kids|
{
  if |kids| == 0 then []
  else if 0 <= kids[0] < |dag| && kids[0] !in visited then
    [kids[0]] + NewChildren(dag, kids[1..], visited + {kids[0]})
  else
    NewChildren(dag, kids[1..], visited)
}

function WalkFrontier(dag: seq<Node>, frontier: seq<int>, visited: set<int>, fuel: nat): (seq<int>, set<int>)
  requires InBounds(dag)
  requires forall f :: f in frontier ==> 0 <= f < |dag|
  decreases fuel
{
  if fuel == 0 || |frontier| == 0 then ([], visited)
  else
    var n := frontier[0];
    var newKids := NewChildren(dag, dag[n].children, visited);
    var visited' := visited + (set k | k in newKids);
    // Splice the new child group to the front (group-scheduling), expand rest.
    var (rest, v2) := WalkFrontier(dag, newKids + frontier[1..], visited', fuel - 1);
    (dag[n].direct + rest, v2)
}

// Flatten the DAG rooted at `root`. Fuel = |dag| + 1 (each node expanded once;
// +1 covers the initial root frame).
function Flatten(dag: seq<Node>, root: int): seq<int>
  requires InBounds(dag)
{
  if !(0 <= root < |dag|) then []
  else WalkFrontier(dag, [root], {root}, |dag| + 1).0
}

// The set of node-IDs the walk actually expanded (its updated `visited`).
function VisitedNodes(dag: seq<Node>, root: int): set<int>
  requires InBounds(dag)
{
  if !(0 <= root < |dag|) then {}
  else WalkFrontier(dag, [root], {root}, |dag| + 1).1
}

// ---------------------------------------------------------------------------
// The golden property. `out` is a valid flatten of the DAG rooted at `root` iff:
//   (1) EXPAND-ONCE / completeness over NODES: the set of nodes whose direct
//       blocks contributed to `out` is EXACTLY ReachNodes(dag, root) — no
//       reachable node is dropped, no unreachable node is expanded, and (by the
//       walk's construction) each is expanded once. We capture this as: the
//       visited-node set equals the reachable-node set.
//   (2) ELEMENT COMPLETENESS: every direct element of every reachable node
//       appears in `out` (so the |out| length equals the total direct count
//       over reachable nodes — the dedup-of-NODES, not of values, bound).
//   (3) ORDER: `out` equals the reference preorder/direct-first/declaration-
//       order walk `Flatten(dag, root)`. This is the single load-bearing order
//       statement; it subsumes "direct before children" and "declaration order
//       among children".
// NOTE the deliberate absence of a value-level NoDup: the walk does NOT dedup
// element values, only nodes. Asserting NoDup(out) would be FALSE for a DAG
// where two distinct nodes share an element-ID — see the module header.
// ---------------------------------------------------------------------------
predicate ValidFlatten(dag: seq<Node>, root: int, out: seq<int>)
  requires InBounds(dag)
{
  && VisitedNodes(dag, root) == ReachNodes(dag, root)
  && |out| == |ReachableOccurrences(dag, root)|
  && out == Flatten(dag, root)
}

// Proof target (to be discharged once the Dafny toolchain is installed): the
// constructive `Flatten` walk satisfies ValidFlatten for any acyclic, in-bounds
// DAG. The lemma body must show (a) the visited set the walk threads equals the
// reachable-node set (each reachable node scheduled exactly once — the
// expand-once / O(unique) property), and (b) the emitted length equals the
// total direct-occurrence count over reachable nodes. The SPEC above is what the
// Creusot contract and the differential oracle bind to.
//
// lemma {:axiom} FlattenIsValid(dag: seq<Node>, root: int)
//   requires InBounds(dag)
//   requires Acyclic(dag, ReachNodes(dag, root))
//   ensures ValidFlatten(dag, root, Flatten(dag, root))
