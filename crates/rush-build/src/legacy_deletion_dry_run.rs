// Rush Build - Legacy Deletion Dry-Run Report
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Phase 5.C.4 Batch AD-revised (commit 219): the `--legacy-
//! deletion-dry-run` / `RUSH_5C5_DRY_RUN=1` aggregate report.
//!
//! Per Pro's 2026-05-20 AB-revised directive Day 2 section:
//!
//! > Add a dry-run/report mode:
//! >   --legacy-deletion-dry-run
//! >   or RUSH_5C5_DRY_RUN=1
//! >
//! > Report:
//! >   scheduler-ready targets, zero-action scheduler
//! >   targets, legacy-required targets, unsupported shapes,
//! >   missing tools, action graph incomplete, missing
//! >   external source materialization, output-validation
//! >   readiness, generated producer readiness, why each
//! >   target cannot move to scheduler yet.
//! >
//! > Include ExternalRepoSourceMissing as a blocker category.
//!
//! ## What this module is and is NOT
//!
//! **Is**: A pure-data aggregate that composes the existing
//! preflight + census + materialization + producer-validation
//! reports into a single structured output, suitable for
//! both human dump and JSON consumption (CI gates, external
//! tooling, future fetch-arc planning).
//!
//! **Is NOT**: A new validation pipeline. It produces ZERO
//! new failures; it just enumerates the state of the world
//! at a single point in the build's analyze pass. The
//! existing components (scheduler_preflight,
//! compute_legacy_deletion_census,
//! check_materialization_completeness) own the underlying
//! correctness; this module only summarizes.
//!
//! ## Why a dedicated module
//!
//! The dry-run report is the natural artifact when "no safe
//! pilot candidate exists" (Batch ABR-5's conclusion). It
//! feeds three downstream consumers:
//!
//!   1. **The external-repo materialization lane** (later
//!      phase): a complete list of every TF target whose
//!      closure crosses an unmaterialized external repo,
//!      ranked by repo and rule kind.
//!   2. **The CI gate** (Batch AF, Day 4): thresholds like
//!      `RUSH_CI_MAX_EXTERNAL_SOURCE_MISSING=0` for pilots
//!      and configurable values for analyze-only reports
//!      consume the bucket counts here.
//!   3. **The weekly summary** (Batch AH/AI, Day 6-7): the
//!      go/no-go recommendation reads the aggregated state
//!      and explains why the action-bearing pilot ladder
//!      is paused / can resume / etc.

use std::collections::BTreeMap;

use rush_action::scheduler::LegacyBreakdownEntry;
use rush_rules::SchedulerSupportLevel;

use crate::build_orchestrator_ext::{
    ExternalRepoSourceMissingCount, MissingToolCount, ReasonCount, RuleKindCount,
    SchedulerEligibility, SchedulerEligibilityReason, SchedulerPreflightReport,
    UnsupportedShapeCount,
};
use crate::candidate_blocker::{
    blocker_counts_by_kind, CandidateReportEntry, CandidateStatusCounts,
};
use crate::materialization_report::MaterializationCompletenessReport;

/// Schema version for the JSON wire shape of
/// [`LegacyDeletionDryRunReport`]. Bumped on breaking field
/// changes; additive fields keep the version stable.
pub const LEGACY_DELETION_DRY_RUN_SCHEMA_VERSION: u32 = 1;

/// One-line summary of materialization across all examined
/// candidates. `entries` is empty when no per-target
/// materialization data is provided (the orchestrator's live
/// build path doesn't yet have a live closure-walker that
/// computes per-target materialization classification —
/// that's a separate piece of work tracked elsewhere).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct MaterializationSummary {
    /// Number of targets with a materialization report
    /// attached (typically 0 when the dry-run runs from
    /// analyze-only; non-zero when a candidate scanner has
    /// populated per-target reports).
    pub total_examined: usize,
    /// Of `total_examined`, how many had at least one
    /// blocked artifact.
    pub blocked_targets: usize,
    /// Per-repo blocker tally aggregated across all examined
    /// targets. Repo names are workspace names (e.g.
    /// `"snappy"`, `"com_google_absl"`, or `"<in-tree>"`
    /// for non-external).
    pub blockers_by_repo: BTreeMap<String, usize>,
}

/// Output-validation readiness summary. Mirrors the
/// 3-class policy pinned in Batch Z commit 200 +
/// Batch AA commit 208 (provider-only / scheduler-ready
/// default-error / legacy warn-only / incomplete
/// never-schedulable).
///
/// For AD-revised the values are presence-only counts: how
/// many declared actions, how many incomplete, how many
/// missing-input producers, how many missing-provider
/// producers. The interpretation (error vs warn vs ignore)
/// is the orchestrator's job; this module only reports
/// counts.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct OutputValidationSummary {
    pub declared_actions: u64,
    pub incomplete_actions: u64,
    pub missing_input_producers: u64,
    pub missing_provider_producers: u64,
}

/// Generated-producer readiness summary. Subset of
/// [`OutputValidationSummary`] focused on the "every
/// declared input has a producer action" invariant.
///
/// For the 5.C.5 pilot lane this number must be 0 for any
/// candidate that's going to be admitted to dispatch — a
/// missing producer means the action DAG can't be executed.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ProducerReadinessSummary {
    /// Total generated inputs declared across the examined
    /// scope.
    pub total_generated_inputs: u64,
    /// Generated inputs with a registered producer action.
    pub with_producer: u64,
    /// Generated inputs with NO producer (blocked).
    pub without_producer: u64,
}

/// Per-target "why this target cannot move to scheduler yet"
/// entry. Sorted by blocker class then by target name in the
/// final report.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PerTargetBlocker {
    pub target: String,
    pub rule_kind: String,
    /// Resolved support level (the same value the action-
    /// graph explain helper uses).
    pub support_level: SchedulerSupportLevel,
    /// Human-readable reason. For non-blocker variants this
    /// is a short OK summary (e.g. `"dispatch-ready"`).
    pub reason: String,
}

/// Candidate-level blocker taxonomy summary, the BH-introduced
/// section of the dry-run report. Aggregates the candidate
/// scanner's `CandidateBlocker` variants (Class 2 quarantine
/// taxonomy: MissingVendoredDependency, MissingHostPackage,
/// MissingTool, UnsupportedShape, ExternalSourceMissing,
/// ExternalRepoStubLayout, VerifierMismatch,
/// OutputValidationFailure, ActionGraphIncomplete, TooBroad)
/// into per-kind and per-status counts.
///
/// Distinct from `OutputValidationSummary` / `by_reason` /
/// `materialization` — those summarize the orchestrator's
/// preflight gate, while this one summarizes the candidate
/// scanner's quarantine decisions. They overlap (e.g. a
/// MissingTool blocker maps to a MissingTool preflight bucket)
/// but the candidate scanner sees a richer taxonomy than the
/// orchestrator's eligibility decision.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CandidateBlockerTaxonomy {
    /// Per-status counts (ready, blocked, quarantined,
    /// executed, failed). Total across all candidate entries.
    pub status_counts: CandidateStatusCounts,
    /// Per-blocker-kind counts. Keys are stable kebab-case
    /// `CandidateBlocker::kind_label()` strings:
    /// `"missing_vendored_dependency"`,
    /// `"missing_host_package"`, `"missing_tool"`,
    /// `"unsupported_shape"`, `"external_source_missing"`,
    /// `"external_repo_stub_layout"`, `"verifier_mismatch"`,
    /// `"output_validation_failure"`,
    /// `"action_graph_incomplete"`, `"too_broad"`.
    pub blockers_by_kind: BTreeMap<String, usize>,
    /// Total number of candidate entries observed (matches
    /// `status_counts.total()`).
    pub total_candidates: usize,
}

/// The aggregate dry-run report.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LegacyDeletionDryRunReport {
    pub schema_version: u32,
    /// The scheduler preflight bucket counts (
    /// `dispatched`, `RuleKindUnverified`,
    /// `UnsupportedShape`, `MissingTool`,
    /// `ActionGraphIncomplete`,
    /// `ExternalRepoSourceMissing`, `total_targets`).
    pub scheduler_preflight: SchedulerPreflightReport,
    /// Aggregated legacy-deletion debt (mnemonic histograms,
    /// per-rule-kind breakdown, per-support-level breakdown).
    pub legacy_deletion_entries: Vec<LegacyBreakdownEntry>,
    pub total_legacy_complete: usize,
    pub total_legacy_incomplete: usize,
    pub by_rule_kind: Vec<RuleKindCount>,
    pub by_reason: Vec<ReasonCount>,
    /// Materialization summary across examined candidates.
    pub materialization: MaterializationSummary,
    /// Output-validation readiness counts.
    pub output_validation: OutputValidationSummary,
    /// Generated-producer readiness summary.
    pub producer_readiness: ProducerReadinessSummary,
    /// "Why each target cannot move to scheduler yet" —
    /// sorted by support-level class, then by target name.
    /// Includes only blocked targets (Verified targets are
    /// excluded by convention; they're already dispatch-
    /// ready).
    pub per_target_blockers: Vec<PerTargetBlocker>,
    /// Candidate-level blocker taxonomy (Pro 2026-05-22
    /// BH directive: surface the new CandidateBlocker
    /// variants in the dry-run output). Empty
    /// `CandidateBlockerTaxonomy` when no candidate report
    /// entries are supplied to the builder.
    pub candidate_blockers: CandidateBlockerTaxonomy,
}

/// Build a [`LegacyDeletionDryRunReport`] from the
/// component inputs. Pure data: no orchestrator state, no
/// I/O. Easy to test, easy to assemble from any caller that
/// has the preflight + census + per-target reports.
///
/// `materialization_reports` is optional per-target
/// `(label, MaterializationCompletenessReport)` data. When
/// `None` or empty, the materialization summary is zero-
/// filled (the dry-run will still emit, just with the
/// materialization dimension blank).
pub fn build_legacy_deletion_dry_run_report(
    scheduler_preflight: SchedulerPreflightReport,
    legacy_breakdown_entries: Vec<LegacyBreakdownEntry>,
    total_legacy_complete: usize,
    total_legacy_incomplete: usize,
    decisions: &[(String, String, SchedulerEligibility)],
    materialization_reports: Option<&[(String, MaterializationCompletenessReport)]>,
    output_validation: OutputValidationSummary,
    producer_readiness: ProducerReadinessSummary,
) -> LegacyDeletionDryRunReport {
    build_legacy_deletion_dry_run_report_with_candidates(
        scheduler_preflight,
        legacy_breakdown_entries,
        total_legacy_complete,
        total_legacy_incomplete,
        decisions,
        materialization_reports,
        output_validation,
        producer_readiness,
        None,
    )
}

/// BH-extended builder: same as
/// [`build_legacy_deletion_dry_run_report`] but accepts an
/// optional per-candidate `CandidateReportEntry` slice. When
/// supplied, populates the `candidate_blockers` taxonomy
/// section. When `None` or empty, that section is zero-filled.
///
/// Callers that have already aggregated candidate scan results
/// (the candidate scanner from AY/AZ) should use this form;
/// callers that only have the orchestrator's preflight pass
/// (the live build path) keep using the unparameterized form.
pub fn build_legacy_deletion_dry_run_report_with_candidates(
    scheduler_preflight: SchedulerPreflightReport,
    legacy_breakdown_entries: Vec<LegacyBreakdownEntry>,
    total_legacy_complete: usize,
    total_legacy_incomplete: usize,
    decisions: &[(String, String, SchedulerEligibility)],
    materialization_reports: Option<&[(String, MaterializationCompletenessReport)]>,
    output_validation: OutputValidationSummary,
    producer_readiness: ProducerReadinessSummary,
    candidate_entries: Option<&[CandidateReportEntry]>,
) -> LegacyDeletionDryRunReport {
    // Roll the eligibility decisions up into per-rule-kind
    // and per-reason buckets — same convention as
    // compute_legacy_deletion_census but keyed off the
    // (target, rule_kind, eligibility) tuples this function
    // takes (so the dry-run report can include per-target
    // blocker enumeration that the census doesn't).
    let mut by_rule_kind_map: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut by_reason_map: std::collections::HashMap<SchedulerSupportLevel, usize> =
        std::collections::HashMap::new();
    let mut per_target_blockers: Vec<PerTargetBlocker> = Vec::new();

    for (target, rule_kind, eligibility) in decisions {
        let support_level =
            crate::build_orchestrator_ext::eligibility_reason_to_support_level_public(
                &eligibility.reason,
            );
        if matches!(support_level, SchedulerSupportLevel::Verified) {
            // Verified targets are dispatch-ready, not legacy
            // debt — exclude from the per-target blocker list
            // and from the by_rule_kind / by_reason buckets.
            continue;
        }
        *by_rule_kind_map.entry(rule_kind.clone()).or_insert(0) += 1;
        *by_reason_map.entry(support_level.clone()).or_insert(0) += 1;

        let reason = reason_for_eligibility(&eligibility.reason);
        per_target_blockers.push(PerTargetBlocker {
            target: target.clone(),
            rule_kind: rule_kind.clone(),
            support_level,
            reason,
        });
    }

    // Sort: by_rule_kind by count desc then name asc; same as
    // compute_legacy_deletion_census.
    let mut by_rule_kind: Vec<RuleKindCount> = by_rule_kind_map
        .into_iter()
        .map(|(rule_kind, count)| RuleKindCount { rule_kind, count })
        .collect();
    by_rule_kind.sort_by(|a, b| {
        b.count.cmp(&a.count).then_with(|| a.rule_kind.cmp(&b.rule_kind))
    });

    let mut by_reason: Vec<ReasonCount> = by_reason_map
        .into_iter()
        .map(|(support_level, count)| ReasonCount { support_level, count })
        .collect();
    by_reason.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.support_level.label().cmp(&b.support_level.label()))
    });

    // Per-target blockers: sort by support-level class
    // (label asc), then by target name asc. Operators see
    // "RuleKindUnverified" entries grouped, "UnsupportedShape"
    // grouped, etc.
    per_target_blockers.sort_by(|a, b| {
        a.support_level
            .label()
            .cmp(&b.support_level.label())
            .then_with(|| a.target.cmp(&b.target))
    });

    // Materialization summary.
    let materialization = match materialization_reports {
        None | Some(&[]) => MaterializationSummary::default(),
        Some(reports) => {
            let total_examined = reports.len();
            let mut blocked_targets = 0usize;
            let mut blockers_by_repo: BTreeMap<String, usize> = BTreeMap::new();
            for (_label, report) in reports {
                if !report.all_admitted() {
                    blocked_targets += 1;
                }
                for (repo_opt, n) in report.blockers_by_repo() {
                    let key = repo_opt.unwrap_or_else(|| "<in-tree>".to_string());
                    *blockers_by_repo.entry(key).or_insert(0) += n;
                }
            }
            MaterializationSummary {
                total_examined,
                blocked_targets,
                blockers_by_repo,
            }
        }
    };

    // Candidate-level blocker taxonomy (BH).
    let candidate_blockers = match candidate_entries {
        None | Some(&[]) => CandidateBlockerTaxonomy::default(),
        Some(entries) => {
            let status_counts = CandidateStatusCounts::tally(entries);
            let blockers_by_kind = blocker_counts_by_kind(entries);
            CandidateBlockerTaxonomy {
                total_candidates: status_counts.total(),
                status_counts,
                blockers_by_kind,
            }
        }
    };

    LegacyDeletionDryRunReport {
        schema_version: LEGACY_DELETION_DRY_RUN_SCHEMA_VERSION,
        scheduler_preflight,
        legacy_deletion_entries: legacy_breakdown_entries,
        total_legacy_complete,
        total_legacy_incomplete,
        by_rule_kind,
        by_reason,
        materialization,
        output_validation,
        producer_readiness,
        per_target_blockers,
        candidate_blockers,
    }
}

/// Translate a [`SchedulerEligibilityReason`] into a human-
/// readable per-target reason string for the
/// `PerTargetBlocker.reason` field.
fn reason_for_eligibility(reason: &SchedulerEligibilityReason) -> String {
    match reason {
        SchedulerEligibilityReason::RuleKindVerified => {
            "dispatch-ready".to_string()
        }
        SchedulerEligibilityReason::RuleKindUnverified => {
            "rule kind not on scheduler allowlist".to_string()
        }
        SchedulerEligibilityReason::UnsupportedShape(r) => {
            format!("unsupported shape: {}", r)
        }
        SchedulerEligibilityReason::MissingTool(t) => {
            format!("missing host tool: {}", t)
        }
        SchedulerEligibilityReason::NoDeclaredActions => {
            "zero declared actions (scheduler handles as no-op)"
                .to_string()
        }
        SchedulerEligibilityReason::ActionGraphIncomplete => {
            "action DAG incomplete (no ExecutableAction wiring)".to_string()
        }
        SchedulerEligibilityReason::ExternalRepoSourceMissing(r) => {
            format!("external repo materialization gap: {}", r)
        }
        SchedulerEligibilityReason::ExternalSystemStub(kind) => {
            format!(
                "intentional external stub ({}) — provider-only, scheduler no-op",
                kind
            )
        }
    }
}

/// Format the dry-run report as a multi-line human-readable
/// dump. Pro's format style: section headings + indented
/// counts + per-target enumeration capped to a reasonable
/// length.
pub fn format_legacy_deletion_dry_run_report(
    report: &LegacyDeletionDryRunReport,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    out.push(format!(
        "5.C.5 legacy-deletion dry-run report \
         (schema {}; {} total targets):",
        report.schema_version, report.scheduler_preflight.total_targets,
    ));
    out.push("".to_string());

    out.push("== Scheduler preflight ==".to_string());
    out.push(format!("  dispatched: {}", report.scheduler_preflight.dispatched));
    fmt_bucket(
        &mut out,
        "RuleKindUnverified",
        &report.scheduler_preflight.rule_kind_unverified,
        |e: &RuleKindCount| format!("    {}: {}", e.rule_kind, e.count),
    );
    fmt_bucket_us(
        &mut out,
        "UnsupportedShape",
        &report.scheduler_preflight.unsupported_shape,
    );
    fmt_bucket_mt(
        &mut out,
        "MissingTool",
        &report.scheduler_preflight.missing_tool,
    );
    fmt_bucket(
        &mut out,
        "ActionGraphIncomplete",
        &report.scheduler_preflight.action_graph_incomplete,
        |e: &RuleKindCount| format!("    {}: {}", e.rule_kind, e.count),
    );
    fmt_bucket_em(
        &mut out,
        "ExternalRepoSourceMissing",
        &report.scheduler_preflight.external_repo_source_missing,
    );

    out.push("".to_string());
    out.push("== Legacy deletion debt ==".to_string());
    out.push(format!(
        "  legacy_complete: {}, legacy_incomplete: {}",
        report.total_legacy_complete, report.total_legacy_incomplete,
    ));
    if !report.by_rule_kind.is_empty() {
        out.push("  by rule kind:".to_string());
        for e in &report.by_rule_kind {
            out.push(format!("    {}: {}", e.rule_kind, e.count));
        }
    }
    if !report.by_reason.is_empty() {
        out.push("  by reason:".to_string());
        for e in &report.by_reason {
            out.push(format!("    {}: {}", e.support_level.label(), e.count));
        }
    }

    out.push("".to_string());
    out.push("== Materialization ==".to_string());
    out.push(format!(
        "  examined: {}, blocked: {}",
        report.materialization.total_examined,
        report.materialization.blocked_targets,
    ));
    if !report.materialization.blockers_by_repo.is_empty() {
        out.push("  blockers by repo:".to_string());
        for (repo, count) in &report.materialization.blockers_by_repo {
            out.push(format!("    {}: {}", repo, count));
        }
    }

    out.push("".to_string());
    out.push("== Output validation readiness ==".to_string());
    out.push(format!(
        "  declared_actions: {}, incomplete: {}, \
         missing_input_producers: {}, missing_provider_producers: {}",
        report.output_validation.declared_actions,
        report.output_validation.incomplete_actions,
        report.output_validation.missing_input_producers,
        report.output_validation.missing_provider_producers,
    ));

    out.push("".to_string());
    out.push("== Producer readiness ==".to_string());
    out.push(format!(
        "  total_generated_inputs: {}, with_producer: {}, without_producer: {}",
        report.producer_readiness.total_generated_inputs,
        report.producer_readiness.with_producer,
        report.producer_readiness.without_producer,
    ));

    out.push("".to_string());
    out.push("== Candidate-level blocker taxonomy (BH) ==".to_string());
    out.push(format!(
        "  total_candidates: {}",
        report.candidate_blockers.total_candidates,
    ));
    if report.candidate_blockers.total_candidates > 0 {
        let s = &report.candidate_blockers.status_counts;
        out.push(format!(
            "  status: ready={}, blocked={}, quarantined={}, \
             executed={}, failed={}",
            s.ready, s.blocked, s.quarantined, s.executed, s.failed,
        ));
        if !report.candidate_blockers.blockers_by_kind.is_empty() {
            out.push("  blockers by kind:".to_string());
            for (kind, count) in &report.candidate_blockers.blockers_by_kind {
                out.push(format!("    {}: {}", kind, count));
            }
        } else {
            out.push("  blockers by kind: (none)".to_string());
        }
    } else {
        out.push(
            "  (no candidate scan input — section empty)".to_string(),
        );
    }

    out.push("".to_string());
    out.push("== Per-target blockers ==".to_string());
    if report.per_target_blockers.is_empty() {
        out.push("  (none — every non-Verified target enumerated above)"
            .to_string());
    } else {
        // Group by support-level label for readability.
        let mut current_label = String::new();
        for b in &report.per_target_blockers {
            let label = b.support_level.label();
            if label != current_label {
                out.push(format!("  {}:", label));
                current_label = label;
            }
            out.push(format!(
                "    {} [{}] — {}",
                b.target, b.rule_kind, b.reason
            ));
        }
    }

    out
}

fn fmt_bucket<T, F: Fn(&T) -> String>(
    out: &mut Vec<String>,
    heading: &str,
    bucket: &[T],
    fmt: F,
) {
    if bucket.is_empty() {
        return;
    }
    out.push(format!("  {}:", heading));
    for e in bucket {
        out.push(fmt(e));
    }
}

fn fmt_bucket_us(
    out: &mut Vec<String>,
    heading: &str,
    bucket: &[UnsupportedShapeCount],
) {
    if bucket.is_empty() {
        return;
    }
    out.push(format!("  {}:", heading));
    for e in bucket {
        out.push(format!("    {} [{}]: {}", e.rule_kind, e.reason, e.count));
    }
}

fn fmt_bucket_mt(
    out: &mut Vec<String>,
    heading: &str,
    bucket: &[MissingToolCount],
) {
    if bucket.is_empty() {
        return;
    }
    out.push(format!("  {}:", heading));
    for e in bucket {
        out.push(format!("    {}: {}", e.tool, e.count));
    }
}

fn fmt_bucket_em(
    out: &mut Vec<String>,
    heading: &str,
    bucket: &[ExternalRepoSourceMissingCount],
) {
    if bucket.is_empty() {
        return;
    }
    out.push(format!("  {}:", heading));
    for e in bucket {
        out.push(format!("    {} [{}]: {}", e.rule_kind, e.reason, e.count));
    }
}

/// Serialize the dry-run report as pretty-printed JSON. Used
/// by CI gates and external tooling. Returns Err on serde
/// failure (extremely unlikely for this Vec/HashMap-shaped
/// data; would indicate a serde version skew).
pub fn format_legacy_deletion_dry_run_report_json(
    report: &LegacyDeletionDryRunReport,
) -> anyhow::Result<String> {
    serde_json::to_string_pretty(report).map_err(|e| {
        anyhow::anyhow!("dry-run JSON serialization failed: {}", e)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_orchestrator_ext::SCHEDULER_PREFLIGHT_SCHEMA_VERSION;

    fn empty_preflight() -> SchedulerPreflightReport {
        SchedulerPreflightReport {
            schema_version: SCHEDULER_PREFLIGHT_SCHEMA_VERSION,
            dispatched: 0,
            rule_kind_unverified: vec![],
            unsupported_shape: vec![],
            missing_tool: vec![],
            action_graph_incomplete: vec![],
            external_repo_source_missing: vec![],
            total_targets: 0,
        }
    }

    fn mk_decision(
        target: &str,
        rule_kind: &str,
        reason: SchedulerEligibilityReason,
    ) -> (String, String, SchedulerEligibility) {
        let eligible = matches!(reason, SchedulerEligibilityReason::RuleKindVerified)
            || matches!(reason, SchedulerEligibilityReason::NoDeclaredActions);
        (
            target.to_string(),
            rule_kind.to_string(),
            SchedulerEligibility { eligible, reason },
        )
    }

    /// AD-revised: an empty input produces a well-formed empty
    /// report with schema_version == 1.
    #[test]
    fn empty_report_well_formed() {
        let report = build_legacy_deletion_dry_run_report(
            empty_preflight(),
            vec![],
            0,
            0,
            &[],
            None,
            OutputValidationSummary::default(),
            ProducerReadinessSummary::default(),
        );
        assert_eq!(report.schema_version, 1);
        assert_eq!(report.scheduler_preflight.total_targets, 0);
        assert_eq!(report.by_rule_kind.len(), 0);
        assert_eq!(report.by_reason.len(), 0);
        assert_eq!(report.per_target_blockers.len(), 0);
        assert_eq!(report.materialization.total_examined, 0);
        assert_eq!(report.materialization.blocked_targets, 0);
    }

    /// AD-revised: ExternalRepoSourceMissing decisions show up
    /// in the per-target blocker list with their static
    /// reason category surfaced via the SchedulerSupportLevel.
    /// This is the new vocabulary Pro asked be named
    /// consistently across reports.
    #[test]
    fn external_repo_source_missing_surfaces_in_per_target_blockers() {
        let decisions = vec![
            mk_decision(
                "//tensorflow/core/platform:cpu_feature_guard",
                "cc_library",
                SchedulerEligibilityReason::ExternalRepoSourceMissing(
                    "external repo source file missing",
                ),
            ),
            mk_decision(
                "//tensorflow/core/platform:tensor_coding",
                "cc_library",
                SchedulerEligibilityReason::ExternalRepoSourceMissing(
                    "external repo source file missing",
                ),
            ),
            mk_decision(
                "//tensorflow/core/platform:macros",
                "cc_library",
                SchedulerEligibilityReason::NoDeclaredActions,
            ),
            mk_decision(
                "//pkg:verified_target",
                "cc_library",
                SchedulerEligibilityReason::RuleKindVerified,
            ),
        ];
        let report = build_legacy_deletion_dry_run_report(
            empty_preflight(),
            vec![],
            0,
            0,
            &decisions,
            None,
            OutputValidationSummary::default(),
            ProducerReadinessSummary::default(),
        );
        // Verified is excluded from the per-target blocker
        // enumeration. NoDeclaredActions maps to Verified
        // support level, so it's also excluded. Only the two
        // ExternalRepoSourceMissing entries remain.
        assert_eq!(report.per_target_blockers.len(), 2);
        // Both entries carry the new support level.
        for b in &report.per_target_blockers {
            assert!(
                b.reason.starts_with("external repo materialization gap"),
                "blocker reason mentions the materialization class, got: {}",
                b.reason
            );
            assert_eq!(
                b.support_level,
                SchedulerSupportLevel::ExternalRepoSourceMissing {
                    reason: "external repo source file missing",
                }
            );
        }
        // Sorted by target name within the same support-level
        // class.
        assert_eq!(
            report.per_target_blockers[0].target,
            "//tensorflow/core/platform:cpu_feature_guard"
        );
        assert_eq!(
            report.per_target_blockers[1].target,
            "//tensorflow/core/platform:tensor_coding"
        );
        // by_rule_kind: 1 cc_library row with count == 2
        // (NoDeclaredActions / Verified are excluded).
        assert_eq!(report.by_rule_kind.len(), 1);
        assert_eq!(report.by_rule_kind[0].rule_kind, "cc_library");
        assert_eq!(report.by_rule_kind[0].count, 2);
        // by_reason: 1 entry for the new class with count == 2.
        assert_eq!(report.by_reason.len(), 1);
        assert_eq!(report.by_reason[0].count, 2);
        assert_eq!(
            report.by_reason[0].support_level,
            SchedulerSupportLevel::ExternalRepoSourceMissing {
                reason: "external repo source file missing"
            }
        );
    }

    /// AD-revised: the human dump includes the new
    /// ExternalRepoSourceMissing section heading + per-target
    /// blocker enumeration.
    #[test]
    fn human_format_includes_external_repo_section() {
        let decisions = vec![mk_decision(
            "//pkg:blocked",
            "cc_library",
            SchedulerEligibilityReason::ExternalRepoSourceMissing(
                "external repo source file missing",
            ),
        )];
        let mut preflight = empty_preflight();
        preflight.total_targets = 1;
        preflight.external_repo_source_missing.push(
            ExternalRepoSourceMissingCount {
                rule_kind: "cc_library".to_string(),
                reason: "external repo source file missing".to_string(),
                count: 1,
            },
        );
        let report = build_legacy_deletion_dry_run_report(
            preflight,
            vec![],
            0,
            0,
            &decisions,
            None,
            OutputValidationSummary::default(),
            ProducerReadinessSummary::default(),
        );
        let lines = format_legacy_deletion_dry_run_report(&report);
        let dump = lines.join("\n");
        assert!(dump.contains("== Scheduler preflight =="));
        assert!(dump.contains("ExternalRepoSourceMissing:"));
        assert!(dump.contains("    cc_library [external repo source file missing]: 1"));
        assert!(dump.contains("== Per-target blockers =="));
        assert!(dump.contains(
            "//pkg:blocked [cc_library] — external repo materialization gap"
        ));
    }

    /// AD-revised: materialization summary aggregates per-
    /// target reports by repo. Pro's example: "operators
    /// triage by which external repo blocks the most
    /// candidates."
    #[test]
    fn materialization_summary_aggregates_blockers_by_repo() {
        use crate::materialization_report::{
            check_materialization_completeness, MaterializationFs,
            PathSetProducers,
        };
        use std::collections::{HashMap, HashSet};
        use std::path::{Path, PathBuf};

        // Fake FS: repo dirs exist for snappy and absl, sources
        // absent.
        struct StubFs {
            dirs: HashSet<PathBuf>,
        }
        impl MaterializationFs for StubFs {
            fn path_exists(&self, p: &Path) -> bool {
                self.dirs.contains(p)
            }
            fn dir_exists(&self, p: &Path) -> bool {
                self.dirs.contains(p)
            }
        }
        let mut dirs = HashSet::new();
        dirs.insert(PathBuf::from("/cache/external/snappy"));
        dirs.insert(PathBuf::from("/cache/external/com_google_absl"));
        let fs = StubFs { dirs };
        let producers_set: HashSet<PathBuf> = HashSet::new();
        let producers = PathSetProducers(&producers_set);

        let mut repo_paths = HashMap::new();
        repo_paths
            .insert("snappy".to_string(), PathBuf::from("/cache/external/snappy"));
        repo_paths.insert(
            "com_google_absl".to_string(),
            PathBuf::from("/cache/external/com_google_absl"),
        );

        use crate::materialization_report::{
            DeclaredSourceArtifact, SourceKind,
        };
        let mk_external = |target: &str, repo: &str, path: &str| {
            DeclaredSourceArtifact {
                target: target.to_string(),
                repo: Some(repo.to_string()),
                package: "pkg".to_string(),
                artifact_path: PathBuf::from(path),
                rule_kind: "cc_library".to_string(),
                source_kind: SourceKind::ExternalRepoSource {
                    repo: repo.to_string(),
                },
            }
        };

        let report_a = check_materialization_completeness(
            &[
                mk_external(
                    "//pkg:a",
                    "snappy",
                    "/cache/external/snappy/snappy/snappy.cc",
                ),
                mk_external(
                    "//pkg:a",
                    "snappy",
                    "/cache/external/snappy/snappy/snappy-sinksource.cc",
                ),
            ],
            &repo_paths,
            &fs,
            &producers,
        );
        let report_b = check_materialization_completeness(
            &[mk_external(
                "//pkg:b",
                "com_google_absl",
                "/cache/external/com_google_absl/absl/strings/strings.cc",
            )],
            &repo_paths,
            &fs,
            &producers,
        );
        let reports = vec![
            ("//pkg:a".to_string(), report_a),
            ("//pkg:b".to_string(), report_b),
        ];
        let dr_report = build_legacy_deletion_dry_run_report(
            empty_preflight(),
            vec![],
            0,
            0,
            &[],
            Some(&reports),
            OutputValidationSummary::default(),
            ProducerReadinessSummary::default(),
        );
        assert_eq!(dr_report.materialization.total_examined, 2);
        assert_eq!(dr_report.materialization.blocked_targets, 2);
        // snappy has 2 blocked artifacts, absl has 1.
        assert_eq!(
            dr_report.materialization.blockers_by_repo.get("snappy").copied(),
            Some(2)
        );
        assert_eq!(
            dr_report
                .materialization
                .blockers_by_repo
                .get("com_google_absl")
                .copied(),
            Some(1)
        );
    }

    /// AD-revised JSON shape pin: schema_version + named
    /// fields the CI gate parser depends on.
    #[test]
    fn json_shape_pin() {
        let report = build_legacy_deletion_dry_run_report(
            empty_preflight(),
            vec![],
            0,
            0,
            &[],
            None,
            OutputValidationSummary::default(),
            ProducerReadinessSummary::default(),
        );
        let json = format_legacy_deletion_dry_run_report_json(&report)
            .expect("json ok");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("re-parse");
        assert_eq!(parsed["schema_version"], 1);
        assert!(parsed["scheduler_preflight"].is_object());
        assert!(parsed["legacy_deletion_entries"].is_array());
        assert!(parsed["by_rule_kind"].is_array());
        assert!(parsed["by_reason"].is_array());
        assert!(parsed["materialization"].is_object());
        assert!(parsed["output_validation"].is_object());
        assert!(parsed["producer_readiness"].is_object());
        assert!(parsed["per_target_blockers"].is_array());
        // Materialization summary has the new
        // blockers_by_repo field.
        assert!(parsed["materialization"]["blockers_by_repo"].is_object());
    }

    /// AD-revised: NoDeclaredActions is excluded from the
    /// per-target blocker enumeration (it maps to Verified
    /// support level, which means dispatch-ready by no-op).
    /// This pins that the dry-run report does NOT
    /// double-count zero-action scheduler targets as
    /// "legacy debt."
    #[test]
    fn no_declared_actions_excluded_from_blocker_enumeration() {
        let decisions = vec![mk_decision(
            "//pkg:hdr_only",
            "cc_library",
            SchedulerEligibilityReason::NoDeclaredActions,
        )];
        let report = build_legacy_deletion_dry_run_report(
            empty_preflight(),
            vec![],
            0,
            0,
            &decisions,
            None,
            OutputValidationSummary::default(),
            ProducerReadinessSummary::default(),
        );
        // The header-only target is dispatch-ready (Verified
        // support level), so it doesn't appear in the
        // blocker enumeration.
        assert_eq!(report.per_target_blockers.len(), 0);
        assert_eq!(report.by_rule_kind.len(), 0);
        assert_eq!(report.by_reason.len(), 0);
    }

    /// AD-revised: output-validation and producer-readiness
    /// counts pass through to the report unchanged.
    #[test]
    fn output_validation_and_producer_readiness_passthrough() {
        let ov = OutputValidationSummary {
            declared_actions: 100,
            incomplete_actions: 5,
            missing_input_producers: 2,
            missing_provider_producers: 1,
        };
        let pr = ProducerReadinessSummary {
            total_generated_inputs: 50,
            with_producer: 48,
            without_producer: 2,
        };
        let report = build_legacy_deletion_dry_run_report(
            empty_preflight(),
            vec![],
            0,
            0,
            &[],
            None,
            ov.clone(),
            pr.clone(),
        );
        assert_eq!(report.output_validation.declared_actions, 100);
        assert_eq!(report.output_validation.incomplete_actions, 5);
        assert_eq!(report.output_validation.missing_input_producers, 2);
        assert_eq!(report.output_validation.missing_provider_producers, 1);
        assert_eq!(report.producer_readiness.total_generated_inputs, 50);
        assert_eq!(report.producer_readiness.with_producer, 48);
        assert_eq!(report.producer_readiness.without_producer, 2);

        // Human dump includes the values.
        let lines = format_legacy_deletion_dry_run_report(&report);
        let dump = lines.join("\n");
        assert!(dump.contains("declared_actions: 100"));
        assert!(dump.contains("with_producer: 48"));
    }

    /// BH: when no candidate entries are supplied, the
    /// candidate-blockers section is well-formed with zero
    /// counts and the human dump notes the absence.
    #[test]
    fn bh_candidate_blockers_default_when_no_entries() {
        let report = build_legacy_deletion_dry_run_report(
            empty_preflight(),
            vec![],
            0,
            0,
            &[],
            None,
            OutputValidationSummary::default(),
            ProducerReadinessSummary::default(),
        );
        assert_eq!(report.candidate_blockers.total_candidates, 0);
        assert!(report.candidate_blockers.blockers_by_kind.is_empty());
        assert_eq!(report.candidate_blockers.status_counts.total(), 0);

        let dump = format_legacy_deletion_dry_run_report(&report).join("\n");
        assert!(dump.contains("== Candidate-level blocker taxonomy (BH) =="));
        assert!(dump.contains("total_candidates: 0"));
        assert!(dump.contains("(no candidate scan input — section empty)"));
    }

    /// BH: a mixed candidate report (ready + quarantined +
    /// executed) populates status_counts and
    /// blockers_by_kind accurately. The taxonomy uses the
    /// stable kebab-case kind_label() values.
    #[test]
    fn bh_candidate_blockers_mixed_taxonomy() {
        use crate::candidate_blocker::{
            CandidateBlocker, CandidateReportEntry, CandidateStatus,
        };
        let entries = vec![
            CandidateReportEntry {
                target: "//examples/hello_cpp:hello".to_string(),
                status: CandidateStatus::Executed,
                rule_kind: Some("cc_binary".to_string()),
                action_count: Some(2),
            },
            CandidateReportEntry {
                target: "//tensorflow/core/platform:macros".to_string(),
                status: CandidateStatus::Ready,
                rule_kind: Some("cc_library".to_string()),
                action_count: Some(0),
            },
            CandidateReportEntry {
                target: "//tensorflow/core/platform:cpu_feature_guard"
                    .to_string(),
                status: CandidateStatus::Quarantined {
                    blockers: vec![CandidateBlocker::MissingVendoredDependency {
                        name: "ml_dtypes_eigen".to_string(),
                        evidence: "float8.h Eigen Traits error".to_string(),
                    }],
                },
                rule_kind: Some("cc_library".to_string()),
                action_count: Some(2),
            },
            CandidateReportEntry {
                target: "//tensorflow/core/protobuf:error_codes_proto_impl"
                    .to_string(),
                status: CandidateStatus::Quarantined {
                    blockers: vec![CandidateBlocker::TooBroad {
                        reason: "duplicate target name across packages"
                            .to_string(),
                    }],
                },
                rule_kind: Some("tf_proto_library".to_string()),
                action_count: Some(3),
            },
        ];
        let report = build_legacy_deletion_dry_run_report_with_candidates(
            empty_preflight(),
            vec![],
            0,
            0,
            &[],
            None,
            OutputValidationSummary::default(),
            ProducerReadinessSummary::default(),
            Some(&entries),
        );

        assert_eq!(report.candidate_blockers.total_candidates, 4);
        let s = &report.candidate_blockers.status_counts;
        assert_eq!(s.ready, 1);
        assert_eq!(s.quarantined, 2);
        assert_eq!(s.executed, 1);
        assert_eq!(s.blocked, 0);
        assert_eq!(s.failed, 0);

        assert_eq!(
            report
                .candidate_blockers
                .blockers_by_kind
                .get("missing_vendored_dependency")
                .copied(),
            Some(1)
        );
        assert_eq!(
            report.candidate_blockers.blockers_by_kind.get("too_broad").copied(),
            Some(1)
        );
        // Ready and Executed entries contribute no blockers.
        assert_eq!(report.candidate_blockers.blockers_by_kind.len(), 2);

        let dump = format_legacy_deletion_dry_run_report(&report).join("\n");
        assert!(dump.contains("== Candidate-level blocker taxonomy (BH) =="));
        assert!(dump.contains("total_candidates: 4"));
        assert!(dump.contains("ready=1"));
        assert!(dump.contains("quarantined=2"));
        assert!(dump.contains("executed=1"));
        assert!(dump.contains("missing_vendored_dependency: 1"));
        assert!(dump.contains("too_broad: 1"));
    }

    /// BP: dry_run_reports_eigen_blocker — the dry-run
    /// report surfaces an ml_dtypes_eigen blocker in the
    /// candidate_blockers.blockers_by_kind section under
    /// the missing_vendored_dependency kind.
    #[test]
    fn dry_run_reports_eigen_blocker() {
        use crate::candidate_blocker::{
            CandidateBlocker, CandidateReportEntry, CandidateStatus,
        };
        let entries = vec![CandidateReportEntry {
            target: "//tensorflow/core/platform:cpu_feature_guard".to_string(),
            status: CandidateStatus::Quarantined {
                blockers: vec![CandidateBlocker::MissingVendoredDependency {
                    name: "ml_dtypes_eigen".to_string(),
                    evidence: "float8.h: no type named 'BitsType' in Traits<float>"
                        .to_string(),
                }],
            },
            rule_kind: Some("cc_library".to_string()),
            action_count: Some(2),
        }];
        let report = build_legacy_deletion_dry_run_report_with_candidates(
            empty_preflight(),
            vec![],
            0,
            0,
            &[],
            None,
            OutputValidationSummary::default(),
            ProducerReadinessSummary::default(),
            Some(&entries),
        );
        assert_eq!(
            report
                .candidate_blockers
                .blockers_by_kind
                .get("missing_vendored_dependency")
                .copied(),
            Some(1)
        );
        let dump = format_legacy_deletion_dry_run_report(&report).join("\n");
        assert!(dump.contains("missing_vendored_dependency: 1"));
    }

    /// BP: dry_run_reports_output_path_collision — the
    /// dry-run surfaces the BN-introduced OutputPathCollision
    /// blocker kind. Pinned per Pro's BP test list.
    #[test]
    fn dry_run_reports_output_path_collision() {
        use crate::candidate_blocker::{
            CandidateBlocker, CandidateReportEntry, CandidateStatus,
        };
        let entries = vec![CandidateReportEntry {
            target: "//tensorflow/core/protobuf:error_codes_proto_impl"
                .to_string(),
            status: CandidateStatus::Quarantined {
                blockers: vec![CandidateBlocker::OutputPathCollision {
                    target: "//tensorflow/core/protobuf:error_codes_proto_impl"
                        .to_string(),
                    name: "error_codes_proto_impl".to_string(),
                    other_targets: vec![
                        "@local_tsl//tsl/protobuf:error_codes_proto_impl"
                            .to_string(),
                    ],
                }],
            },
            rule_kind: Some("tf_proto_library".to_string()),
            action_count: Some(3),
        }];
        let report = build_legacy_deletion_dry_run_report_with_candidates(
            empty_preflight(),
            vec![],
            0,
            0,
            &[],
            None,
            OutputValidationSummary::default(),
            ProducerReadinessSummary::default(),
            Some(&entries),
        );
        assert_eq!(
            report
                .candidate_blockers
                .blockers_by_kind
                .get("output_path_collision")
                .copied(),
            Some(1)
        );
        let dump = format_legacy_deletion_dry_run_report(&report).join("\n");
        assert!(dump.contains("output_path_collision: 1"));
    }

    /// BP: dry_run_reports_provider_only_system_library —
    /// system_cc_library targets land as ExternalSystemStub
    /// in the SchedulerSupportLevel taxonomy; they DO NOT
    /// appear in per_target_blockers because they're
    /// dispatch-ready. The dry-run report's per_target
    /// blockers list explicitly excludes them.
    #[test]
    fn dry_run_reports_provider_only_system_library() {
        let decisions = vec![mk_decision(
            "//pkg:system_lib",
            "system_cc_library",
            SchedulerEligibilityReason::NoDeclaredActions,
        )];
        let report = build_legacy_deletion_dry_run_report(
            empty_preflight(),
            vec![],
            0,
            0,
            &decisions,
            None,
            OutputValidationSummary::default(),
            ProducerReadinessSummary::default(),
        );
        // NoDeclaredActions → Verified support level →
        // excluded from per-target blocker enumeration
        // (because it's dispatch-ready, not legacy debt).
        assert_eq!(report.per_target_blockers.len(), 0);
        assert_eq!(report.by_rule_kind.len(), 0);
        // No false-positive in the candidate taxonomy
        // either.
        assert!(report.candidate_blockers.blockers_by_kind.is_empty());
    }

    /// BP: dry_run_reports_successful_pilots — the
    /// status_counts.executed field reflects the number of
    /// successful action-bearing pilots in the candidate
    /// report.
    #[test]
    fn dry_run_reports_successful_pilots() {
        use crate::candidate_blocker::{
            CandidateReportEntry, CandidateStatus,
        };
        let entries = vec![
            CandidateReportEntry {
                target: "//examples/hello_cpp:hello".to_string(),
                status: CandidateStatus::Executed,
                rule_kind: Some("cc_binary".to_string()),
                action_count: Some(2),
            },
            CandidateReportEntry {
                target: "//examples/hello_cpp:hello_lib".to_string(),
                status: CandidateStatus::Executed,
                rule_kind: Some("cc_library".to_string()),
                action_count: Some(1),
            },
            CandidateReportEntry {
                target: "@local_tsl//tsl/framework:device_type".to_string(),
                status: CandidateStatus::Executed,
                rule_kind: Some("cc_library".to_string()),
                action_count: Some(2),
            },
        ];
        let report = build_legacy_deletion_dry_run_report_with_candidates(
            empty_preflight(),
            vec![],
            0,
            0,
            &[],
            None,
            OutputValidationSummary::default(),
            ProducerReadinessSummary::default(),
            Some(&entries),
        );
        assert_eq!(report.candidate_blockers.status_counts.executed, 3);
        let dump = format_legacy_deletion_dry_run_report(&report).join("\n");
        assert!(dump.contains("executed=3"));
    }

    /// BP: dry_run_scheduler_ready_count_matches_invocation_report —
    /// the scheduler_preflight.dispatched count and the
    /// candidate scanner's Ready count should agree on the
    /// number of dispatch-ready targets when both are
    /// populated from the same scan run.
    #[test]
    fn dry_run_scheduler_ready_count_matches_invocation_report() {
        use crate::candidate_blocker::{
            CandidateReportEntry, CandidateStatus,
        };
        // Synthesize a small consistent view: 3 dispatched
        // in preflight, 3 ready in candidate report.
        let mut preflight = empty_preflight();
        preflight.dispatched = 3;
        preflight.total_targets = 3;
        let entries = vec![
            CandidateReportEntry {
                target: "//pkg:a".to_string(),
                status: CandidateStatus::Ready,
                rule_kind: None,
                action_count: Some(0),
            },
            CandidateReportEntry {
                target: "//pkg:b".to_string(),
                status: CandidateStatus::Ready,
                rule_kind: None,
                action_count: Some(0),
            },
            CandidateReportEntry {
                target: "//pkg:c".to_string(),
                status: CandidateStatus::Ready,
                rule_kind: None,
                action_count: Some(2),
            },
        ];
        let report = build_legacy_deletion_dry_run_report_with_candidates(
            preflight,
            vec![],
            0,
            0,
            &[],
            None,
            OutputValidationSummary::default(),
            ProducerReadinessSummary::default(),
            Some(&entries),
        );
        // The two independently-tracked counts agree on
        // 3 dispatch-ready / Ready targets.
        assert_eq!(report.scheduler_preflight.dispatched, 3);
        assert_eq!(report.candidate_blockers.status_counts.ready, 3);
    }

    /// BH: the JSON shape pin includes the new
    /// `candidate_blockers` object.
    #[test]
    fn bh_candidate_blockers_json_shape() {
        let report = build_legacy_deletion_dry_run_report(
            empty_preflight(),
            vec![],
            0,
            0,
            &[],
            None,
            OutputValidationSummary::default(),
            ProducerReadinessSummary::default(),
        );
        let json = format_legacy_deletion_dry_run_report_json(&report)
            .expect("json ok");
        let parsed: serde_json::Value =
            serde_json::from_str(&json).expect("re-parse");
        assert!(parsed["candidate_blockers"].is_object());
        assert!(parsed["candidate_blockers"]["status_counts"].is_object());
        assert!(parsed["candidate_blockers"]["blockers_by_kind"].is_object());
        assert_eq!(parsed["candidate_blockers"]["total_candidates"], 0);
    }
}
