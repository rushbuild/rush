// Rush Action - Action DAG construction
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Action DAG construction (phase 5.C.3).
//!
//! Walks the [`ArtifactRegistry`]'s declared-action arena and
//! derives a producer→consumer dependency graph from the inputs
//! of each [`DeclaredAction`]. For every input `ArtifactId` that
//! has a known producer (`reg.producer(aid).is_some()`), an edge
//! is added from the producer action to the consumer. Source
//! artifacts (whose `producer()` returns `None`) contribute no
//! edges — they're inputs to the build, not outputs of it.
//!
//! The DAG is then topologically sorted via Kahn's algorithm. A
//! sort that doesn't visit every action means the action graph
//! contains a cycle — surfaced as
//! [`ActionDagError::CycleDetected`] with the unvisited actions.
//!
//! ## What this is for
//!
//! Phase 5.C.3 builds the DAG and validates it. Phase 5.C.4 uses
//! the topological order to drive narrow per-action execution
//! (replacing the legacy rule-instance build loop). 5.C.5 removes
//! the legacy path. Until then, the DAG is computed for
//! diagnostics only — `print_summary` emits a per-mnemonic edge
//! count and the longest dependency chain length, useful for
//! confirming the action graph shape before flipping execution
//! over to it.
//!
//! ## What this is NOT
//!
//! - Not a content-addressed action key. That's a separate
//!   `Action` type in `rush_action::lib`; the scheduler will hash
//!   each [`DeclaredAction`] when it picks up execution in 5.C.4.
//! - Not aware of `Incomplete` actions' fallback semantics. The
//!   scheduler in 5.C.4 will refuse to execute Incomplete
//!   actions (they're flagged in `DeclaredAction.status`); the
//!   DAG itself includes them so the diagnostic output reflects
//!   the full declared shape.

use crate::ids::{ArtifactRegistry, DeclaredActionId};
use std::collections::{HashMap, VecDeque};

/// Errors from [`ActionDag::build`].
#[derive(Debug, Clone)]
pub enum ActionDagError {
    /// The action graph contains a cycle — at least one action
    /// transitively consumes one of its own outputs. Carries the
    /// set of actions Kahn's algorithm couldn't reach (one of
    /// them must lie on a cycle, possibly more).
    CycleDetected {
        /// Actions whose in-degree never reached zero. Their
        /// declared inputs reference outputs of actions that, in
        /// turn, transitively depend on them.
        unvisited: Vec<DeclaredActionId>,
    },
}

impl std::fmt::Display for ActionDagError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActionDagError::CycleDetected { unvisited } => write!(
                f,
                "action DAG cycle detected: {} unvisited action(s) — \
                 one or more form a producer/consumer cycle",
                unvisited.len()
            ),
        }
    }
}

impl std::error::Error for ActionDagError {}

/// A topologically-sorted action graph derived from the
/// registry's declared actions. Phase 5.C.3.
#[derive(Debug)]
pub struct ActionDag {
    /// Outgoing edges: producer → list of consumer actions.
    /// Sparse — actions with no consumers don't appear.
    edges: HashMap<DeclaredActionId, Vec<DeclaredActionId>>,
    /// Total edge count across all producers. Cached so
    /// `print_summary` doesn't have to walk the map.
    edge_count: usize,
    /// Topological order: every action appears exactly once;
    /// every action precedes its consumers. Empty registry →
    /// empty order.
    topo_order: Vec<DeclaredActionId>,
}

impl ActionDag {
    /// Build the action DAG from a fully-populated artifact
    /// registry. Call after every rule's analyze() has run, so
    /// every declared action's outputs are in the producer slot
    /// (otherwise the consumer's input → producer lookup will
    /// undercount edges).
    ///
    /// Returns `ActionDagError::CycleDetected` if the declared
    /// actions form a producer/consumer cycle. With phase 5.C0
    /// pass 1 (no duplicate producer) and pass 4 (sources have
    /// no producer) enforced at registration time, cycles can
    /// only arise from a rule declaring an action whose input
    /// transitively depends on its output — which is itself a
    /// rule bug, but the DAG construction surfaces it
    /// structurally rather than letting the scheduler hang.
    pub fn build(reg: &ArtifactRegistry) -> Result<Self, ActionDagError> {
        let action_count = reg.declared_action_count();
        let mut edges: HashMap<DeclaredActionId, Vec<DeclaredActionId>> = HashMap::new();
        let mut in_degree: HashMap<DeclaredActionId, usize> =
            HashMap::with_capacity(action_count);
        let mut edge_count: usize = 0;

        // Initialize every action with in_degree 0. We need this
        // even for actions with no producer-side edges, so the
        // Kahn queue seeds correctly.
        for (id, _) in reg.declared_actions() {
            in_degree.insert(id, 0);
        }

        // Walk every action's inputs. For each input that has a
        // declared producer, add a producer→consumer edge.
        for (consumer, action) in reg.declared_actions() {
            for &input_aid in action.inputs.iter() {
                if let Some(producer) = reg.producer(input_aid) {
                    if producer == consumer {
                        // An action consuming its own output is
                        // a self-loop; treat as a 1-element
                        // cycle. Surface it the same way as a
                        // multi-action cycle so the error path
                        // is uniform.
                        // Add the edge anyway; Kahn will fail to
                        // visit `consumer` and report it as
                        // unvisited.
                    }
                    edges.entry(producer).or_default().push(consumer);
                    *in_degree.entry(consumer).or_insert(0) += 1;
                    edge_count += 1;
                }
            }
        }

        // Kahn's algorithm. Seed queue with every zero-in-degree
        // action (those whose inputs are either source artifacts
        // or otherwise un-produced). Iteration order is the
        // registry's allocation order, so the topological output
        // is deterministic across runs of the same build.
        let mut queue: VecDeque<DeclaredActionId> = VecDeque::new();
        for (id, _) in reg.declared_actions() {
            if in_degree.get(&id).copied().unwrap_or(0) == 0 {
                queue.push_back(id);
            }
        }

        let mut topo_order: Vec<DeclaredActionId> = Vec::with_capacity(action_count);
        while let Some(id) = queue.pop_front() {
            topo_order.push(id);
            if let Some(consumers) = edges.get(&id) {
                for &consumer in consumers.iter() {
                    let entry = in_degree.entry(consumer).or_insert(0);
                    if *entry > 0 {
                        *entry -= 1;
                        if *entry == 0 {
                            queue.push_back(consumer);
                        }
                    }
                }
            }
        }

        if topo_order.len() != action_count {
            // Some actions never reached in_degree 0 → cycle.
            // Collect them for the error message; the caller can
            // map back to action mnemonics + owner_target via
            // the registry for a useful diagnostic.
            let visited: std::collections::HashSet<DeclaredActionId> =
                topo_order.iter().copied().collect();
            let unvisited: Vec<DeclaredActionId> = reg
                .declared_actions()
                .filter_map(|(id, _)| if visited.contains(&id) { None } else { Some(id) })
                .collect();
            return Err(ActionDagError::CycleDetected { unvisited });
        }

        Ok(ActionDag {
            edges,
            edge_count,
            topo_order,
        })
    }

    /// Topological order of declared actions. Producers precede
    /// consumers. Length equals
    /// `ArtifactRegistry::declared_action_count()`.
    pub fn topo_order(&self) -> &[DeclaredActionId] {
        &self.topo_order
    }

    /// Consumers of a given action — the actions whose declared
    /// inputs include one of this action's outputs. Returns an
    /// empty slice if the action has no downstream consumers.
    pub fn consumers_of(&self, id: DeclaredActionId) -> &[DeclaredActionId] {
        self.edges.get(&id).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Total action count (== `topo_order().len()`).
    pub fn action_count(&self) -> usize {
        self.topo_order.len()
    }

    /// Total producer→consumer edge count across the DAG.
    pub fn edge_count(&self) -> usize {
        self.edge_count
    }

    /// Compute the longest dependency chain length — the height
    /// of the DAG. Useful for understanding the parallelism
    /// ceiling: a build with N actions and longest-chain L can
    /// theoretically run with ~N/L parallelism. Returns 0 for an
    /// empty DAG.
    pub fn longest_chain(&self) -> usize {
        if self.topo_order.is_empty() {
            return 0;
        }
        // Walk in topo order; chain[v] = 1 + max(chain[u]) over
        // predecessors u of v. We stored outgoing edges, so we
        // update consumers on each step instead.
        let mut chain: HashMap<DeclaredActionId, usize> = HashMap::new();
        for &id in self.topo_order.iter() {
            let here = chain.get(&id).copied().unwrap_or(1);
            chain.insert(id, here);
            if let Some(consumers) = self.edges.get(&id) {
                for &consumer in consumers.iter() {
                    let next = chain.get(&consumer).copied().unwrap_or(1).max(here + 1);
                    chain.insert(consumer, next);
                }
            }
        }
        chain.values().copied().max().unwrap_or(0)
    }

    /// Edge count grouped by producer-action mnemonic. Lets the
    /// orchestrator print a histogram like
    /// `CppCompile → 142 edges, CcArchive → 18 edges, …` so a
    /// reader can sanity-check that the action graph's shape
    /// matches expectations for a workload.
    pub fn edges_by_mnemonic(&self, reg: &ArtifactRegistry) -> Vec<(String, usize)> {
        let mut by_mnemonic: HashMap<String, usize> = HashMap::new();
        for (producer, consumers) in self.edges.iter() {
            let action = reg.declared_action(*producer);
            let mnemonic = reg.strings().str(action.mnemonic).to_string();
            *by_mnemonic.entry(mnemonic).or_insert(0) += consumers.len();
        }
        let mut rows: Vec<(String, usize)> = by_mnemonic.into_iter().collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{
        ActionDeclarationStatus, ArtifactKey, ArtifactKind, ConfigId, DeclaredAction, TargetId,
    };
    use std::path::Path;

    fn mk_src(reg: &mut ArtifactRegistry, name: &str) -> crate::ids::ArtifactId {
        let p = reg.intern_path(Path::new(name));
        reg.intern_artifact(
            ArtifactKey {
                config: ConfigId::DEFAULT,
                exec_path: p,
            },
            ArtifactKind::Source,
            true,
            TargetId(1),
            None,
        )
    }

    fn mk_obj(reg: &mut ArtifactRegistry, name: &str) -> crate::ids::ArtifactId {
        let p = reg.intern_path(Path::new(name));
        reg.intern_artifact(
            ArtifactKey {
                config: ConfigId::DEFAULT,
                exec_path: p,
            },
            ArtifactKind::Object,
            false,
            TargetId(1),
            None,
        )
    }

    fn declare(
        reg: &mut ArtifactRegistry,
        mnemonic: &str,
        inputs: Vec<crate::ids::ArtifactId>,
        outputs: Vec<crate::ids::ArtifactId>,
    ) -> DeclaredActionId {
        let m = reg.intern_string(mnemonic);
        reg.declare_action(DeclaredAction {
            owner_target: TargetId(1),
            mnemonic: m,
            inputs: inputs.into_boxed_slice(),
            outputs: outputs.into_boxed_slice(),
            status: ActionDeclarationStatus::Complete,
        })
        .expect("declare should succeed in test setup")
    }

    #[test]
    fn empty_registry_yields_empty_dag() {
        let reg = ArtifactRegistry::new();
        let dag = ActionDag::build(&reg).expect("empty registry has no cycles");
        assert_eq!(dag.action_count(), 0);
        assert_eq!(dag.edge_count(), 0);
        assert!(dag.topo_order().is_empty());
        assert_eq!(dag.longest_chain(), 0);
    }

    #[test]
    fn linear_compile_archive_dag() {
        // src.cc -> CppCompile -> obj.o -> CcArchive -> lib.a
        //
        // Two-action chain: archive depends on compile via the
        // intermediate object file. Topo order must place compile
        // before archive; longest chain is 2.
        let mut reg = ArtifactRegistry::new();
        let src = mk_src(&mut reg, "src.cc");
        let obj = mk_obj(&mut reg, "obj.o");
        let lib = {
            let p = reg.intern_path(Path::new("lib.a"));
            reg.intern_artifact(
                ArtifactKey {
                    config: ConfigId::DEFAULT,
                    exec_path: p,
                },
                ArtifactKind::StaticLibrary,
                false,
                TargetId(1),
                None,
            )
        };
        let compile = declare(&mut reg, "CppCompile", vec![src], vec![obj]);
        let archive = declare(&mut reg, "CcArchive", vec![obj], vec![lib]);

        let dag = ActionDag::build(&reg).expect("acyclic chain");
        assert_eq!(dag.action_count(), 2);
        assert_eq!(dag.edge_count(), 1);
        assert_eq!(dag.topo_order(), &[compile, archive]);
        assert_eq!(dag.consumers_of(compile), &[archive]);
        assert_eq!(dag.consumers_of(archive), &[]);
        assert_eq!(dag.longest_chain(), 2);
    }

    #[test]
    fn diamond_dag_is_acyclic() {
        // src.cc -> [compile_a, compile_b] -> [obj_a, obj_b]
        // [obj_a, obj_b] -> link -> bin
        //
        // Diamond: two parallel compiles, both feeding one link.
        // Topo order must place link last; both compiles are at
        // depth 1, link at depth 2; longest chain is 2.
        let mut reg = ArtifactRegistry::new();
        let src = mk_src(&mut reg, "src.cc");
        let obj_a = mk_obj(&mut reg, "obj_a.o");
        let obj_b = mk_obj(&mut reg, "obj_b.o");
        let bin = {
            let p = reg.intern_path(Path::new("bin"));
            reg.intern_artifact(
                ArtifactKey {
                    config: ConfigId::DEFAULT,
                    exec_path: p,
                },
                ArtifactKind::Executable,
                false,
                TargetId(1),
                None,
            )
        };
        let compile_a = declare(&mut reg, "CppCompile", vec![src], vec![obj_a]);
        let compile_b = declare(&mut reg, "CppCompile", vec![src], vec![obj_b]);
        let link = declare(&mut reg, "CcLink", vec![obj_a, obj_b], vec![bin]);

        let dag = ActionDag::build(&reg).expect("diamond is acyclic");
        assert_eq!(dag.action_count(), 3);
        assert_eq!(dag.edge_count(), 2); // both compiles → link
        // link must come after both compiles. The two compiles
        // can appear in either registry order; what matters is
        // that link is last.
        let order = dag.topo_order();
        assert_eq!(order.len(), 3);
        assert_eq!(order[2], link);
        assert!(order[..2].contains(&compile_a));
        assert!(order[..2].contains(&compile_b));
        assert_eq!(dag.longest_chain(), 2);
    }

    #[test]
    fn source_only_inputs_have_no_producer_edges() {
        // Standalone action consuming only source files: no
        // incoming edges, in_degree 0 from the start.
        let mut reg = ArtifactRegistry::new();
        let src = mk_src(&mut reg, "src.cc");
        let obj = mk_obj(&mut reg, "obj.o");
        let compile = declare(&mut reg, "CppCompile", vec![src], vec![obj]);

        let dag = ActionDag::build(&reg).unwrap();
        assert_eq!(dag.edge_count(), 0);
        assert_eq!(dag.topo_order(), &[compile]);
    }

    #[test]
    fn longest_chain_walks_full_height() {
        // Linear chain of 4 actions: longest_chain == 4.
        let mut reg = ArtifactRegistry::new();
        let a0 = mk_src(&mut reg, "src.cc");
        let a1 = mk_obj(&mut reg, "a1.o");
        let a2 = mk_obj(&mut reg, "a2.o");
        let a3 = mk_obj(&mut reg, "a3.o");
        let a4 = mk_obj(&mut reg, "a4.o");
        let _act1 = declare(&mut reg, "Step1", vec![a0], vec![a1]);
        let _act2 = declare(&mut reg, "Step2", vec![a1], vec![a2]);
        let _act3 = declare(&mut reg, "Step3", vec![a2], vec![a3]);
        let _act4 = declare(&mut reg, "Step4", vec![a3], vec![a4]);

        let dag = ActionDag::build(&reg).unwrap();
        assert_eq!(dag.action_count(), 4);
        assert_eq!(dag.edge_count(), 3);
        assert_eq!(dag.longest_chain(), 4);
    }

    #[test]
    fn cycle_in_action_graph_is_detected() {
        // Construct a 2-action cycle:
        //   action A: outputs=[X], inputs=[Y]
        //   action B: outputs=[Y], inputs=[X]
        // A consumes B's output, B consumes A's output.
        // Registry-time checks (pass 1 / pass 4) don't catch
        // this — they only enforce uniqueness of producers and
        // non-source-as-output. Cycle detection is the DAG's
        // responsibility.
        let mut reg = ArtifactRegistry::new();
        let x = mk_obj(&mut reg, "x.o");
        let y = mk_obj(&mut reg, "y.o");
        // Order matters: A must declare outputs[x] before B
        // declares outputs[y], otherwise B would see a phantom
        // X with no producer when it consumes [x]. We're testing
        // cycle detection, not registration ordering.
        let _a = declare(&mut reg, "ActionA", vec![y], vec![x]);
        let _b = declare(&mut reg, "ActionB", vec![x], vec![y]);

        let err = ActionDag::build(&reg).expect_err("cycle should be detected");
        match err {
            ActionDagError::CycleDetected { unvisited } => {
                // Both actions form the cycle; both should be
                // unvisited (each has in_degree > 0).
                assert_eq!(unvisited.len(), 2);
            }
        }
    }

    #[test]
    fn edges_by_mnemonic_groups_and_sorts() {
        // 2 CppCompile -> 1 CcArchive: edges_by_mnemonic should
        // report CppCompile = 2 (count of producer→consumer
        // edges originating at CppCompile actions). CcArchive
        // has no consumers in this graph, so it's absent.
        let mut reg = ArtifactRegistry::new();
        let src1 = mk_src(&mut reg, "src1.cc");
        let src2 = mk_src(&mut reg, "src2.cc");
        let obj1 = mk_obj(&mut reg, "o1.o");
        let obj2 = mk_obj(&mut reg, "o2.o");
        let lib = {
            let p = reg.intern_path(Path::new("lib.a"));
            reg.intern_artifact(
                ArtifactKey {
                    config: ConfigId::DEFAULT,
                    exec_path: p,
                },
                ArtifactKind::StaticLibrary,
                false,
                TargetId(1),
                None,
            )
        };
        let _c1 = declare(&mut reg, "CppCompile", vec![src1], vec![obj1]);
        let _c2 = declare(&mut reg, "CppCompile", vec![src2], vec![obj2]);
        let _a = declare(&mut reg, "CcArchive", vec![obj1, obj2], vec![lib]);

        let dag = ActionDag::build(&reg).unwrap();
        let rows = dag.edges_by_mnemonic(&reg);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "CppCompile");
        assert_eq!(rows[0].1, 2);
    }
}
