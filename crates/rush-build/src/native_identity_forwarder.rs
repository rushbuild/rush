// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Structural admission for native, non-writing identity aliases.
//! A rule name grants nothing. Admitted execution skips the rule body and
//! scheduler and forwards the actual target's current exact-key state.

use crate::analysis_snapshot::AnalysisSnapshot;
use crate::dependency_graph::{DependencyGraph, Label};
use anyhow::{ensure, Context, Result};
use rush_action::ids::{ArtifactId, ArtifactRegistry, ConfigId, TargetId};
use rush_rules::{providers::ProviderSet, RuleInstance};
use std::collections::HashSet;
use std::sync::Arc;

pub(crate) fn resolve_actual(
    graph: &DependencyGraph,
    label: &Label,
    rule: &dyn RuleInstance,
) -> Result<Option<Label>> {
    let Some(raw) = rule.identity_artifact_forwarder() else {
        return Ok(None);
    };
    ensure!(
        rule.dependencies() == vec![raw.to_owned()],
        "identity forwarder must have exactly its literal actual dependency: {label}"
    );
    ensure!(
        rule.analysis_only_dependencies().is_empty()
            && rule.exec_only_dependencies().is_empty()
            && rule.declared_outputs().is_empty()
            && rule.undeclared_source_inputs().is_empty(),
        "identity forwarder has additional inputs or outputs: {label}"
    );
    let actual = graph.canonicalize_label(Label::parse(
        raw,
        label.workspace.as_deref(),
        Some(&label.package),
    )?);
    let node = graph
        .get(label)
        .context("identity forwarder is outside target graph")?;
    ensure!(
        actual != *label
            && graph.get(&actual).is_some()
            && node.dependencies.len() == 1
            && node.dependencies[0] == actual,
        "identity forwarder actual does not match its exact graph dependency: {label}"
    );
    Ok(Some(actual))
}

fn same_slot<T>(left: &Option<Arc<T>>, right: &Option<Arc<T>>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => Arc::ptr_eq(left, right),
        _ => false,
    }
}

fn same_providers(left: &ProviderSet, right: &ProviderSet) -> bool {
    // Deliberately exhaustive: adding a provider field must update admission.
    let ProviderSet {
        cc_info,
        cc_shared,
        files,
        proto_files,
        default_info,
        runfiles,
        custom,
    } = left;
    same_slot(cc_info, &right.cc_info)
        && same_slot(cc_shared, &right.cc_shared)
        && same_slot(files, &right.files)
        && same_slot(proto_files, &right.proto_files)
        && same_slot(default_info, &right.default_info)
        && same_slot(runfiles, &right.runfiles)
        && custom.len() == right.custom.len()
        && custom.iter().all(|(key, value)| {
            right
                .custom
                .get(key)
                .is_some_and(|other| Arc::ptr_eq(value, other))
        })
}

/// This executor currently analyzes only the explicit legacy native profile.
/// Configured/exec graph entries cannot satisfy its exact Label lookups.
pub(crate) fn admit(
    snapshot: &AnalysisSnapshot,
    label: &Label,
    registry: &ArtifactRegistry,
    pending_targets: &HashSet<TargetId>,
    config: ConfigId,
) -> Result<Option<Label>> {
    let node = snapshot
        .graph
        .get(label)
        .context("missing forwarder node")?;
    let Some(actual) = resolve_actual(&snapshot.graph, label, node.rule_instance.as_ref())? else {
        return Ok(None);
    };
    ensure!(
        config == ConfigId::DEFAULT,
        "identity forwarding requires the explicit native target configuration"
    );
    let owner_index = snapshot
        .build_order
        .iter()
        .position(|entry| entry == label)
        .context("identity forwarder is outside scoped execution order")?;
    let actual_index = snapshot
        .build_order
        .iter()
        .position(|entry| entry == &actual)
        .context("identity forwarder actual is outside scoped execution order")?;
    ensure!(
        actual_index < owner_index,
        "identity forwarder actual must precede it in the scoped execution order: {label}"
    );
    let target = *snapshot
        .target_id_by_label
        .get(&label.to_string())
        .context("identity forwarder has no exact TargetId")?;
    let actual_target = *snapshot
        .target_id_by_label
        .get(&actual.to_string())
        .context("identity forwarder actual has no exact TargetId")?;
    ensure!(
        target != actual_target && !pending_targets.contains(&target),
        "identity forwarder has a pending action or aliased owner identity: {label}"
    );
    ensure!(
        !registry
            .declared_actions()
            .any(|(_, action)| action.owner_target == target)
            && !(0..registry.artifact_count())
                .any(|index| registry.artifact(ArtifactId(index as u32)).owner_target == target),
        "identity forwarder owns actions or artifacts: {label}"
    );
    ensure!(
        node.rule_instance.providers().is_empty(),
        "identity forwarder exposes providers of its own: {label}"
    );
    let own = snapshot
        .analysis_providers
        .get(&label.to_string())
        .context("identity forwarder has no analyzed providers")?;
    let forwarded = snapshot
        .analysis_providers
        .get(&actual.to_string())
        .context("identity forwarder actual has no analyzed providers")?;
    ensure!(
        same_providers(own, forwarded),
        "identity forwarder does not retain its actual provider identities: {label}"
    );
    Ok(Some(actual))
}
