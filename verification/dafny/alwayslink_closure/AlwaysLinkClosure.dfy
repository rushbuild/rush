// alwayslink-closure-v1 — abstract whole-archive/normal bucket-CONSTRUCTION
// model from the typed linker-input DAG (Dafny spec laboratory / golden model)
//
// This is the ADDENDUM layer of rush's formal-verification track (see the MACP
// agentic-maintenance extension, Dafny-addendum section). It is NOT the Rust
// implementation and does NOT verify the Rust binary. It is the abstract,
// independently-verified specification of the bucket PARTITION that must be
// computed from the dependency DAG BEFORE the link-plan assembly runs.
//
// WHY THIS SCOPE EXISTS (the production bug it models). The C++ test link
// assembler (CcTestInstance, crates/rush-rules/src/rules_cc.rs) honors the
// `alwayslink` bit when it walks the DIRECT deps' TYPED CcInfo.linker_inputs
// (≈ line 3294-3309 → whole-archive bucket). But its TRANSITIVE artifact sweep
// (≈ line 3344-3357, `iter_transitive_artifacts`) pulls every reachable `.a`
// into the NORMAL bucket UNCONDITIONALLY — the alwayslink bit does NOT ride on
// plain Artifacts. So a REGISTER_OP/REGISTER_KERNEL static-initializer archive
// (MlirPassthroughOp, ShuffleDataset, NoOp/_Send/_Recv, …) that is reachable
// ONLY through that transitive fallback is NOT whole-archived → the linker
// strips its initializers (nothing references them) → runtime "no registered
// op/kernel". Four corpus tests (api_test, backwards_compatibility_test,
// shuffle_dataset_op_test, rewrite_utils_test) fail this way (sub-agent + Codex
// thread 019eccf7).
//
// The existing link-plan-v1 scope (verification/dafny/link_plan/LinkPlan.dfy)
// is VACUOUS against THIS defect: it proves the ASSEMBLY given correctly-tagged
// buckets (its `deps: seq<DepLib>` arrive already carrying `(path, alwayslink)`
// tags). This defect is one level UP — in BUCKET CONSTRUCTION from the dep DAG.
// The fix/kernel must operate on TYPED linker-input metadata (archiveId +
// alwayslink bit) carried through the WHOLE reachable closure, not raw
// artifacts; an archive's effective alwayslink status is the OR over ALL
// reachable declarations of it.
//
// COMPOSITION WITH link-plan-v1. This kernel is the PRODUCER; LinkPlan.dfy's
// AssembleLinkPlan is the CONSUMER. The (wholeSet, normalSet) partition this
// model computes is EXACTLY the bucket tagging AssembleLinkPlan ASSUMES as
// input: feed each archive in wholeSet as a DepLib(path, alwayslink:=true) and
// each archive in normalSet as DepLib(path, alwayslink:=false), and
// ValidLinkPlan's P2 (alwayslink-wins / no-double-list) is DISCHARGED by THIS
// model's P3 (DISJOINT) + P5 (NORMAL-NOT-ALWAYSLINK). In other words: link-plan
// proved "given correct buckets, the link line is correct"; this scope proves
// "the buckets are correct over the transitive closure" — together they close
// the gap that let the transitive sweep silently lose the alwayslink bit.
//
// The Creusot contract on crates/rush-verified-core/src/alwayslink_closure.rs
// MIRRORS the tractable conjuncts here (the two-set DISJOINTness P3 and a
// membership predicate) on the real Rust over usize index-DAGs; the heavier
// closure-level conjuncts (P1 alwayslink-wins over the TRANSITIVE closure, P2
// completeness, P4 no-spurious, P5 normal-not-alwayslink) are discharged by the
// proptest oracle (tests/alwayslink_closure_prop.rs), the same Pearlite-vs-
// oracle split as link-plan-v1 / nested-set-walk-v1 / topo-roots-v1. Dafny CLI +
// Z3 are not yet installed on the build hosts; `dafny verify` / `dafny build`
// are the documented next layer (see ci/dafny-toolchain.lock). Until then this
// file is the stabilized spec the oracle binds to.
//
// THE ABSTRACTION. The dep graph is a DAG over node-IDs `0 <= n < |dag|`. Node
// `n` is `dag[n] == Node(inputs, children)` where `inputs` is a seq of typed
// linker-inputs `(archiveId, alwayslink)` (a node may declare the same archive
// more than once with DIFFERENT bits, and the SAME archive may be declared by
// MANY nodes with CONFLICTING bits) and `children` is a seq of child node-IDs
// (declaration order). Reachability mirrors NestedSet.dfy's ReachNodesN /
// TopSort.dfy's ReachN fuel pattern.
//
// REAL-SEMANTICS NOTE (a naively-assumed invariant that is FALSE is documented,
// never asserted): the buckets are over ARCHIVE-IDs, NOT over (node, input)
// occurrences. The same archive declared by two reachable nodes contributes a
// SINGLE bucket entry (identity de-dup); its effective alwayslink status is the
// OR over every reachable declaration. So a per-occurrence "no-dup" invariant
// would be wrong — the true property is over the de-duplicated archive SET.

datatype LinkInput = LinkInput(archiveId: int, alwayslink: bool)
datatype Node = Node(inputs: seq<LinkInput>, children: seq<int>)

// Every child index of every node is a valid node index.
predicate InBounds(dag: seq<Node>) {
  forall n :: 0 <= n < |dag| ==>
    forall c :: c in dag[n].children ==> 0 <= c < |dag|
}

// ---------------------------------------------------------------------------
// Reachability over the NODE-DAG (mirrors NestedSet.dfy ReachNodesN /
// TopSort.dfy ReachN). ReachNodesN(dag, root, fuel) = the set of node-IDs
// reachable from `root` in at most `fuel` child-hops. |dag| hops saturate (no
// simple path is longer than the node count).
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

// ---------------------------------------------------------------------------
// Archive-id projections over the REACHABLE closure. These are the abstract
// sets the (wholeSet, normalSet) partition must realise — the CONSTRUCTION
// half that LinkPlan.dfy's WholeArchiveSet/NormalSet (over an ALREADY-tagged
// `deps: seq<DepLib>`) assumed as given.
// ---------------------------------------------------------------------------

// Does archive `a` appear as an ALWAYSLINK input on SOME reachable node?
// (the OR over all reachable declarations — alwayslink-wins).
predicate IsAlwayslinkSomewhereReachable(dag: seq<Node>, root: int, a: int) {
  exists n, i :: n in ReachNodes(dag, root)
      && 0 <= i < |dag[n].inputs|
      && dag[n].inputs[i].archiveId == a
      && dag[n].inputs[i].alwayslink
}

// Is archive `a` declared (with EITHER bit) by SOME reachable node?
predicate IsDeclaredReachable(dag: seq<Node>, root: int, a: int) {
  exists n, i :: n in ReachNodes(dag, root)
      && 0 <= i < |dag[n].inputs|
      && dag[n].inputs[i].archiveId == a
}

// The set of archive-ids that MUST be whole-archived: alwayslink-declared by
// ANY reachable node (the OR over the WHOLE transitive closure — THIS is the
// production bug: a deep transitive alwayslink declaration still counts).
function ReachableWholeArchiveSet(dag: seq<Node>, root: int): set<int> {
  set n, i | n in ReachNodes(dag, root)
      && 0 <= i < |dag[n].inputs|
      && dag[n].inputs[i].alwayslink
      :: dag[n].inputs[i].archiveId
}

// All distinct archive-ids declared by any reachable node.
function ReachableArchiveSet(dag: seq<Node>, root: int): set<int> {
  set n, i | n in ReachNodes(dag, root) && 0 <= i < |dag[n].inputs|
      :: dag[n].inputs[i].archiveId
}

// The normal bucket: reachable archives NOT alwayslink anywhere reachable.
function ReachableNormalSet(dag: seq<Node>, root: int): set<int> {
  ReachableArchiveSet(dag, root) - ReachableWholeArchiveSet(dag, root)
}

// ---------------------------------------------------------------------------
// ValidBucketing — the golden property. `(wholeSet, normalSet)` is a valid
// bucket partition of the archives reachable from `root` iff P1..P5 hold.
// ---------------------------------------------------------------------------
predicate ValidBucketing(
    dag: seq<Node>, root: int, wholeSet: set<int>, normalSet: set<int>)
{
  // P1 ALWAYSLINK-WINS (the KEY property, over the TRANSITIVE closure): every
  // archive alwayslink-declared by ANY reachable node is in wholeSet. An
  // archive reachable ONLY via a deep transitive path still gets whole-archived
  // — that is exactly the production bug (the transitive sweep dropped its bit).
  && (forall a :: IsAlwayslinkSomewhereReachable(dag, root, a) ==> a in wholeSet)

  // P2 COMPLETENESS: every archive declared by ANY reachable node lands in
  // wholeSet ∪ normalSet (no reachable archive is dropped from the link line).
  && (forall a :: IsDeclaredReachable(dag, root, a) ==> a in wholeSet + normalSet)

  // P3 DISJOINT: the two buckets do not overlap (no double-list — the
  // multiple-definition guard). This is the conjunct LinkPlan.dfy's P2 ASSUMED.
  && (wholeSet * normalSet == {})

  // P4 NO-SPURIOUS: every archive in either bucket is declared by some reachable
  // node (no invented archives — soundness of the closure).
  && (forall a :: a in wholeSet + normalSet ==> IsDeclaredReachable(dag, root, a))

  // P5 NORMAL-NOT-ALWAYSLINK: no archive in normalSet is alwayslink-declared
  // anywhere reachable (the contrapositive of P1 within the normal bucket — a
  // normal-bucket archive is provably safe to lazily-link).
  && (forall a :: a in normalSet ==> !IsAlwayslinkSomewhereReachable(dag, root, a))
}

// The canonical partition computed from the closure projections satisfies
// ValidBucketing by construction — the SPEC the Creusot contract and the
// differential oracle bind to.
//   wholeSet  := ReachableWholeArchiveSet(dag, root)
//   normalSet := ReachableNormalSet(dag, root)

// Proof target (to be discharged once the Dafny toolchain is installed): a
// constructive AlwaysLinkClosure (a DFS over the node-DAG that, for every
// reachable node, ORs each input's alwayslink bit into a per-archive accumulator
// and finally splits archives by the accumulated bit) whose result satisfies
// ValidBucketing for any in-bounds DAG. The lemmas the body needs mirror
// LinkPlan.dfy's BucketDisjoint/BucketCover, lifted to the TRANSITIVE closure:
//
//   * ClosureWholeSound: every archive whose accumulated bit is set was
//     alwayslink-declared by some reachable node (==> wholeSet ⊆ the
//     alwayslink-reachable set) — soundness of the OR accumulator (P4/P5).
//   * ClosureWholeComplete: every alwayslink-reachable archive has its
//     accumulated bit set (the OR visits EVERY reachable declaration, including
//     deep transitive ones — THIS lemma is the one the production code violated
//     by stopping the alwayslink walk at depth-1; P1).
//   * BucketDisjoint: wholeSet * normalSet == {} (normalSet is built as
//     ReachableArchiveSet - wholeSet; P3).
//   * BucketCover:   wholeSet + normalSet == ReachableArchiveSet (every reachable
//     archive is alwayslink-somewhere or not; P2).
//
// lemma {:axiom} ClosureIsValid(dag: seq<Node>, root: int)
//   requires InBounds(dag)
//   ensures ValidBucketing(dag, root,
//             ReachableWholeArchiveSet(dag, root),
//             ReachableNormalSet(dag, root))
