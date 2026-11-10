// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Per-command execution demand for the closed C++ metadata projections.
//! Keep the complete analysis graph and providers. Only runtime ordering and
//! supported action selection change; no retained freshness proof is minted.

use crate::analysis_snapshot::AnalysisSnapshot;
use anyhow::{ensure, Context, Result};
use rush_action::ids::{ArtifactRegistry, DeclaredActionId, TargetId};
use crate::dependency_graph::Label;
use rush_rules::rules_cc_projection::CcProjectionKind;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

pub(crate) fn has_projections(snapshot: &AnalysisSnapshot) -> bool {
    snapshot.build_order.iter().any(|label| {
        snapshot
            .graph
            .get(label)
            .is_some_and(|node| node.rule_instance.cc_projection_kind().is_some())
    })
}

pub(crate) struct NativeExecutionDemand {
    pub active: Vec<bool>,
    pub dependencies: Vec<HashSet<usize>>,
    pub selected_actions: HashMap<usize, Arc<HashSet<DeclaredActionId>>>,
}

impl NativeExecutionDemand {
    pub fn build(
        snapshot: &AnalysisSnapshot,
        roots: &[Label],
        registry: &ArtifactRegistry,
        pending_producers: &HashMap<TargetId, Vec<DeclaredActionId>>,
    ) -> Result<Option<Self>> {
        if !has_projections(snapshot) {
            return Ok(None);
        }
        // Reject cycles and duplicate/invalid producer relations before using
        // any producer as an execution edge.
        rush_action::action_dag::ActionDag::build(registry)?;
        let indices: HashMap<_, _> = snapshot
            .build_order
            .iter()
            .enumerate()
            .map(|(index, label)| (label.to_string(), index))
            .collect();
        let owners: HashMap<TargetId, usize> = snapshot
            .target_id_by_label
            .iter()
            .filter_map(|(label, target)| indices.get(label).map(|index| (*target, *index)))
            .collect();
        let nt = snapshot.build_order.len();
        let mut plan = Self {
            active: vec![false; nt],
            dependencies: vec![HashSet::new(); nt],
            selected_actions: HashMap::new(),
        };
        let mut full = vec![false; nt];
        let mut targets = VecDeque::new();
        let mut actions = VecDeque::new();
        let mut demanded_actions = HashSet::new();
        let mut selected: HashMap<usize, HashSet<DeclaredActionId>> = HashMap::new();
        for root in roots {
            targets.push_back(
                *indices.get(&root.to_string()).with_context(|| {
                    format!("requested root is outside execution closure: {root}")
                })?,
            );
        }
        ensure!(
            !targets.is_empty(),
            "C++ projection execution has no requested roots"
        );

        while !targets.is_empty() || !actions.is_empty() {
            while let Some(index) = targets.pop_front() {
                if full[index] {
                    continue;
                }
                full[index] = true;
                plan.active[index] = true;
                selected.remove(&index);
                let label = &snapshot.build_order[index];
                let node = snapshot
                    .graph
                    .get(label)
                    .context("execution target disappeared")?;
                if let Some(kind) = node.rule_instance.cc_projection_kind() {
                    ensure!(
                        node.rule_instance.analysis_only_dependencies()
                            == node.rule_instance.dependencies(),
                        "C++ projection has unclassified runtime dependencies: {label}"
                    );
                    if kind == CcProjectionKind::HeadersV1 {
                        let files = snapshot
                            .analysis_providers
                            .get(&label.to_string())
                            .and_then(|providers| providers.get_default_info())
                            .context("header projection has no analyzed DefaultInfo")?;
                        for &artifact in &files.files {
                            if let Some(producer) = registry.producer(artifact) {
                                let owner = *owners
                                    .get(&registry.declared_action(producer).owner_target)
                                    .context("header producer is outside the execution closure")?;
                                if owner != index {
                                    plan.dependencies[index].insert(owner);
                                }
                                actions.push_back(producer);
                            } else {
                                ensure!(
                                    registry.artifact(artifact).is_source,
                                    "projected generated header has no real producer: {}",
                                    registry.artifact_path(artifact).display()
                                );
                            }
                        }
                    }
                    continue;
                }
                if let Some(target) = snapshot.target_id_by_label.get(&label.to_string()) {
                    if let Some(producers) = pending_producers.get(target) {
                        for &producer in producers {
                            let producer_index = *owners
                                .get(&registry.declared_action(producer).owner_target)
                                .context("pending input producer is outside execution closure")?;
                            if producer_index != index {
                                plan.dependencies[index].insert(producer_index);
                            }
                            actions.push_back(producer);
                        }
                    }
                }
                for dependency in &node.dependencies {
                    let dependency_index =
                        *indices.get(&dependency.to_string()).with_context(|| {
                            format!("runtime dependency is outside execution closure: {dependency}")
                        })?;
                    ensure!(
                        dependency_index != index,
                        "self dependency in execution closure: {label}"
                    );
                    plan.dependencies[index].insert(dependency_index);
                    targets.push_back(dependency_index);
                }
            }
            while let Some(action_id) = actions.pop_front() {
                if !demanded_actions.insert(action_id) {
                    continue;
                }
                let action = registry.declared_action(action_id);
                let index = *owners
                    .get(&action.owner_target)
                    .context("demanded action owner is outside execution closure")?;
                plan.active[index] = true;
                let node = snapshot
                    .graph
                    .get(&snapshot.build_order[index])
                    .context("demanded action owner disappeared")?;
                let mnemonic = registry.strings().str(action.mnemonic);
                // Only the declared TF proto path supports partial execution.
                // Other producers run through their complete owned lifecycle.
                if node.rule_instance.rule_type() == "tf_proto_library"
                    && mnemonic == "ProtocCompile"
                {
                    if !full[index] {
                        selected.entry(index).or_default().insert(action_id);
                    }
                    ensure!(
                        registry.executable_action(action_id).is_some(),
                        "demanded protoc action has no executable payload"
                    );
                } else {
                    targets.push_back(index);
                }
                for &input in action.inputs.iter() {
                    if let Some(producer) = registry.producer(input) {
                        let producer_owner = *owners
                            .get(&registry.declared_action(producer).owner_target)
                            .context("demanded input producer is outside execution closure")?;
                        if producer_owner != index {
                            plan.dependencies[index].insert(producer_owner);
                        }
                        actions.push_back(producer);
                    } else {
                        ensure!(
                            registry.artifact(input).is_source,
                            "demanded generated input has no real producer: {}",
                            registry.artifact_path(input).display()
                        );
                    }
                }
            }
        }
        for (index, ids) in selected {
            if !full[index] {
                ensure!(!ids.is_empty(), "empty partial execution selection");
                plan.selected_actions.insert(index, Arc::new(ids));
            }
        }
        // A target condensation can cycle even when the action DAG does not.
        // Reject rather than silently broadening this demand to unrelated work.
        let mut indegree: Vec<_> = plan.dependencies.iter().map(HashSet::len).collect();
        let mut reverse = vec![Vec::new(); nt];
        for (consumer, dependencies) in plan.dependencies.iter().enumerate() {
            for &producer in dependencies {
                ensure!(plan.active[producer], "inactive execution producer");
                reverse[producer].push(consumer);
            }
        }
        let mut ready: VecDeque<_> = (0..nt)
            .filter(|i| plan.active[*i] && indegree[*i] == 0)
            .collect();
        let mut completed = 0;
        while let Some(index) = ready.pop_front() {
            completed += 1;
            for &consumer in &reverse[index] {
                indegree[consumer] -= 1;
                if indegree[consumer] == 0 {
                    ready.push_back(consumer);
                }
            }
        }
        ensure!(
            completed == plan.active.iter().filter(|active| **active).count(),
            "C++ projection execution demand has a target ordering cycle"
        );
        Ok(Some(plan))
    }
}
