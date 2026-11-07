// Rush verified-core
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! # rush-verified-core
//!
//! The VERIFIED-CORE crate of Rush's "formal-verification + agentic
//! maintenance" second-stage testing track. Everything here is intentionally
//! **pure and deterministic**: no I/O, no filesystem, no clock, no randomness,
//! no `unsafe`, no `extern "C"`. That constraint is what makes the code a
//! tractable target for a verifier (Creusot today, Kani as a later bounded
//! cross-check) and what keeps its trusted computing base (TCB) small.
//!
//! The LLM that proposes changes here is UNTRUSTED. The cage is: the Creusot
//! proof, the proptest oracle (`tests/cc_obj_name_prop.rs`), and the CI
//! spec-review gate (`scripts/verify/spec-gate.sh`). A change that weakens a
//! contract, loosens the oracle, or touches the toolchain is a *spec-sensitive*
//! change and routes through a human Spec-Review (see
//! `ci/spec-owned.tsv` and the MACP agentic-maintenance extension doc).
//!
//! ## Verified scope: `cc-obj-name-v1`
//!
//! The first proof obligation is **object-name injectivity**. Rush compiles a
//! C++ translation unit identified by a `(package, stem, ext)` triple to an
//! object file. If two distinct triples map to the same object-file name, the
//! second compile clobbers the first on a single output path — at link time
//! the archive's object list carries the name twice, definitions are lost, and
//! the build produces a wrong (or non-linking) artifact. This is not
//! hypothetical: the legacy encoding `format!("{}_{}.o", pkg.replace('/', "_"),
//! stem)` collapses `//a/b:x.cc` and `//a_b:x.cc` to the SAME `a_b_x.o`,
//! because the `/`→`_` rewrite is not injective.
//!
//! [`cc_obj_key`] / [`CcObjKey`] model the triple; [`CcObjKey::object_name`]
//! is the injective adapter the production path now delegates to.
//!
//! ## Verified scope: `label-parse-v1`
//!
//! A DAFNY-FIRST scope (the abstract model in `verification/dafny/label/Label.dfy`
//! is authored before the Rust mirror, because Bazel label
//! parse/canonicalization has enough relative/external-context resolution cases
//! to warrant nailing the spec first). [`CanonicalLabel`] / [`parse`] /
//! [`CanonicalLabel::render`] model the `(repo, package, target)` triple and the
//! two-sided round-trip property: RENDER-THEN-PARSE recovers a well-formed label
//! exactly, and PARSE-THEN-RENDER is an idempotent normalization (the
//! implicit-target forms `//pkg` and `@r//pkg` collapse to the explicit
//! `:lastseg` spelling). [`render_label_round_trip`] carries the
//! render-then-parse identity in its Creusot contract.
//!
//! ## Verified scope: `nested-set-walk-v1`
//!
//! A DAFNY-FIRST scope (the abstract model in
//! `verification/dafny/nested_set/NestedSet.dfy` is authored before the Rust
//! mirror, because the Arc-identity dedup + DAG-fixpoint reasoning is the
//! heaviest kernel here — it is the structural control on the documented 82 GB
//! graph-RSS hog). [`IndexDag`] / [`DagNode`] / [`flatten`] model the
//! `NestedSet<T>` DAG and the preorder, direct-before-children,
//! declaration-order walk with NODE-identity dedup (each node expanded once —
//! the O(unique) bound, mirroring `NestedSetIter`'s `Arc`-pointer dedup). The
//! walk dedups NODES, not element VALUES: a value appearing in two distinct
//! reachable nodes is emitted twice (faithful — see the production
//! `ordered_diamond_dedups_nodes_not_values` test). [`flatten`] carries the
//! element-completeness length bound in its Creusot contract; node-completeness
//! (expanded == reachable) and order are oracle-only conjuncts.
//!
//! ## Verified scope: `link-plan-v1`
//!
//! A DAFNY-FIRST scope (the abstract model in
//! `verification/dafny/link_plan/LinkPlan.dfy` is authored before the Rust
//! mirror, because the bucket-partition + alwayslink-wins + three-phase
//! ordering has enough interacting cases to warrant nailing the spec first).
//! This is the C++ test link-PLAN assembly that the `dedup_archive_link_flags`
//! sub-kernel (link-dedup-v1) sits downstream of — the exact code where a
//! latent `-lbenchmark` gap silently lived. [`DepLib`] / [`assemble_link_plan`]
//! model the assembly: given the objects, the dep libs each tagged
//! `(path, alwayslink)`, the pass-through flags, and a `links_test_main` bit,
//! produce the ordered link-arg tail — whole-archive (alwayslink) libs FIRST
//! each wrapped in a `--whole-archive` triple, THEN the normal libs in a single
//! `--start-group`/`--end-group` (emitted iff ≥1 normal lib — the REAL `>= 1`
//! condition), THEN the pass-through flags with the tf_cc_test
//! `-lbenchmark -lpthread` arm appended iff `links_test_main` (and never
//! `-lbenchmark_main`). A path advertised alwayslink by ANY dep is
//! whole-archived only, never double-listed (the production `retain`).
//! [`assemble_link_plan`] carries the whole-archive-before-group ordering edge
//! in its Creusot contract; the bucket-membership (P1/P2), completeness (P5),
//! and group/benchmark biconditionals (P4/P6) are oracle-only conjuncts.
//!
//! ## Verified scope: `cycle-detect-v1`
//!
//! A DAFNY-FIRST scope (the abstract model in
//! `verification/dafny/cycle_detect/CycleDetect.dfy` is authored before the Rust
//! mirror, the DIRECT SIBLING of `topo-roots-v1`). Where `topo-roots-v1` and
//! `nested-set-walk-v1` *assume* acyclic graphs, this scope verifies the cycle
//! DETECTION rush actually performs — completing the graph story.
//! [`detect_cycle`] reuses the `topo-roots-v1` [`IndexGraph`] and reachable
//! scope, mirrors the production KAHN'S RESIDUAL cycle test
//! (`dependency_graph.rs:638`: `sorted.len() != scope.len()`), and walks the
//! residual to extract a WITNESS cycle. It returns `Some(c)` — a non-empty,
//! in-bounds, edge-consecutive, last-edges-to-first cycle within the subgraph
//! reachable from `roots` — iff that subgraph has a cycle, else `None`. A cycle
//! unreachable from `roots` is NOT reported (scope match). [`detect_cycle`]
//! carries the tractable witness invariant (non-empty + in-bounds) in its
//! Creusot contract; the `Some <==> cycle-exists` biconditional and the
//! witness edge-closure are oracle-only conjuncts, and the oracle binds the
//! topo↔cycle complement `detect_cycle(g,r).is_none() ==
//! topological_sort_from_roots(g,r).is_some()`.
//!
//! ## Verified scope: `alwayslink-closure-v1`
//!
//! A DAFNY-FIRST scope (the abstract model in
//! `verification/dafny/alwayslink_closure/AlwaysLinkClosure.dfy` is authored
//! before the Rust mirror, because the alwayslink-wins OR over the TRANSITIVE
//! closure is the heart of the kernel). It is the BUCKET-CONSTRUCTION producer
//! that sits one level UP from `link-plan-v1`: where `assemble_link_plan`
//! assembles a link line from ALREADY-TAGGED `(path, alwayslink)` deps, this
//! scope computes that tagging from the dep DAG. It models the production bug
//! where `CcTestInstance`'s transitive artifact sweep
//! (`crates/rush-rules/src/rules_cc.rs` ≈ 3344-3357) pulls every reachable `.a`
//! into the NORMAL bucket UNCONDITIONALLY — the `alwayslink` bit does not ride
//! on plain `Artifact`s — so a `REGISTER_OP`/`REGISTER_KERNEL` archive reachable
//! only transitively is stripped by the linker (runtime "no registered
//! op/kernel"; api_test, backwards_compatibility_test, shuffle_dataset_op_test,
//! rewrite_utils_test). [`LinkInputDag`] / [`LinkNode`] / [`alwayslink_closure`]
//! model the typed linker-input DAG and the `(whole_archive, normal)` partition
//! with alwayslink-wins over the WHOLE reachable closure (a deep transitive
//! alwayslink declaration still whole-archives the archive — P1, the production
//! bug). The output feeds `assemble_link_plan`: this scope's P3 (disjoint) + P5
//! (normal-not-alwayslink) discharge `link-plan-v1`'s P2 assumption.
//! [`alwayslink_closure`] carries the bucket-disjointness conjunct (P3) in its
//! Creusot contract; the closure-level P1/P2/P4/P5 conjuncts are oracle-only.

// The verified core must not silently grow a trusted base. Deny the escape
// hatches the agentic-maintenance gate forbids in verified-core source
// (`#[trusted]`, `unsafe`, FFI). `unsafe` is a hard compile error here.
#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod contract_shim;
mod alwayslink_closure;
mod cc_obj_name;
mod cycle_detect;
mod include_dedup;
mod label_parse;
mod link_dedup;
mod link_plan;
mod nested_set_walk;
mod topo_roots;

pub use alwayslink_closure::{
    alwayslink_closure, in_whole_archive, reachable_nodes as reachable_link_nodes, LinkInputDag,
    LinkNode,
};
pub use cc_obj_name::{cc_obj_key, CcObjKey};
pub use cycle_detect::detect_cycle;
pub use include_dedup::{
    dedup_dep_include_flags, is_include_flag, normalized_include_sequence,
};
pub use label_parse::{parse, render_label_round_trip, CanonicalLabel};
pub use link_dedup::{dedup_archive_link_flags, is_archive_path};
pub use link_plan::{assemble_link_plan, is_group_start, is_whole_archive_open, DepLib};
pub use nested_set_walk::{expanded_nodes, flatten, reachable_nodes, DagNode, IndexDag};
pub use topo_roots::{reachable, topological_sort_from_roots, IndexGraph};
