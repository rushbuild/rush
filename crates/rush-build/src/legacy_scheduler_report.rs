// Rush Build - Legacy/Scheduler Invocation Report
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Phase 5.C.4 commit 205 (Batch AA): per-target accounting of
//! which targets ran through the scheduler dispatch path versus
//! the legacy `rule.build()` path during a single
//! `rush build --use-scheduler` invocation.
//!
//! The report is the operator-visible answer to "what actually
//! happened?" for the 5.C.5 first-target pilot. Targets the
//! scheduler dispatched, targets that fell back to legacy
//! `build()`, and the subset of scheduler-dispatched targets
//! that had zero declared actions (header-only / provider-only
//! shapes) are all named so an operator can confirm the
//! invocation report matches the preflight census.
//!
//! Pro's commit-205 spec: zero-action scheduler-dispatched
//! targets are NOT a failure; they're the expected outcome for
//! header-only `cc_library` targets. The report names them in
//! their own bucket so downstream tooling doesn't have to
//! reason about "is this a regression?"

use std::collections::BTreeMap;

/// One scheduler-dispatched target's per-run accounting.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct DispatchedEntry {
    /// Bazel-style label of the target (e.g. `//foo:bar`).
    pub label: String,
    /// Rule kind (e.g. `cc_library`, `tf_cc_library`).
    pub rule_kind: String,
    /// Number of declared actions the scheduler dispatched for
    /// this target. Zero for header-only / provider-only shapes.
    pub action_count: u32,
}

/// One legacy-invoked target's per-run accounting.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct LegacyEntry {
    pub label: String,
    pub rule_kind: String,
    /// Why this target fell to the legacy path. Mirrors a
    /// `SchedulerEligibilityReason` variant name when
    /// available, or a free-form string for other paths
    /// (`use_scheduler=0`, etc.).
    pub reason: String,
}

/// Full per-run report. Built incrementally during PASS B and
/// emitted at the build-completion postlude.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct LegacyVsSchedulerInvocationReport {
    pub scheduler_dispatched: Vec<DispatchedEntry>,
    pub legacy_invoked: Vec<LegacyEntry>,
    /// Labels of scheduler-dispatched targets with `action_count == 0`.
    /// Mirrors a filtered view of `scheduler_dispatched`; surfaced
    /// at the top level so consumers don't have to filter.
    pub zero_action_scheduler_targets: Vec<String>,

    /// `scheduler_dispatched.len()` precomputed for downstream
    /// JSON consumers that don't want to walk the vec.
    pub scheduler_count: u32,
    pub legacy_count: u32,
    pub zero_action_count: u32,

    /// Counts grouped by `rule_kind`. `BTreeMap` for stable
    /// serialization order in JSON.
    pub scheduler_by_rule_kind: BTreeMap<String, u32>,
    pub legacy_by_rule_kind: BTreeMap<String, u32>,
}

impl LegacyVsSchedulerInvocationReport {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that the scheduler dispatched `label` (of rule
    /// kind `rule_kind`) with `action_count` declared actions.
    pub fn record_scheduler_dispatch(
        &mut self,
        label: &str,
        rule_kind: &str,
        action_count: u32,
    ) {
        self.scheduler_dispatched.push(DispatchedEntry {
            label: label.to_string(),
            rule_kind: rule_kind.to_string(),
            action_count,
        });
        self.scheduler_count += 1;
        *self
            .scheduler_by_rule_kind
            .entry(rule_kind.to_string())
            .or_insert(0) += 1;
        if action_count == 0 {
            self.zero_action_scheduler_targets
                .push(label.to_string());
            self.zero_action_count += 1;
        }
    }

    /// Record that `label` (of rule kind `rule_kind`) ran
    /// through the legacy `rule.build()` path. `reason` should
    /// name the `SchedulerEligibilityReason` variant or a
    /// free-form fallback reason.
    pub fn record_legacy_invocation(
        &mut self,
        label: &str,
        rule_kind: &str,
        reason: &str,
    ) {
        self.legacy_invoked.push(LegacyEntry {
            label: label.to_string(),
            rule_kind: rule_kind.to_string(),
            reason: reason.to_string(),
        });
        self.legacy_count += 1;
        *self
            .legacy_by_rule_kind
            .entry(rule_kind.to_string())
            .or_insert(0) += 1;
    }

    /// Was this build a no-op for the scheduler (zero scheduler
    /// dispatches AND zero legacy invocations)? Useful for the
    /// pilot's analyze-only pre-check.
    pub fn is_empty(&self) -> bool {
        self.scheduler_count == 0 && self.legacy_count == 0
    }

    /// Multiline human-readable summary suitable for stderr.
    /// Lines are prefixed with the existing `📊` glyph used by
    /// other build postlude reports.
    pub fn format_human(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "📊 invocation report: scheduler={} legacy={} zero_action_scheduler={}\n",
            self.scheduler_count, self.legacy_count, self.zero_action_count,
        ));
        if !self.scheduler_by_rule_kind.is_empty() {
            out.push_str("📊   scheduler by rule_kind:\n");
            for (rk, n) in &self.scheduler_by_rule_kind {
                out.push_str(&format!("📊     {} = {}\n", rk, n));
            }
        }
        if !self.legacy_by_rule_kind.is_empty() {
            out.push_str("📊   legacy by rule_kind:\n");
            for (rk, n) in &self.legacy_by_rule_kind {
                out.push_str(&format!("📊     {} = {}\n", rk, n));
            }
        }
        if !self.zero_action_scheduler_targets.is_empty() {
            out.push_str("📊   zero-action scheduler targets:\n");
            for label in &self.zero_action_scheduler_targets {
                out.push_str(&format!("📊     {}\n", label));
            }
        }
        if !self.legacy_invoked.is_empty() {
            out.push_str("📊   legacy fallbacks:\n");
            for entry in &self.legacy_invoked {
                out.push_str(&format!(
                    "📊     {} (rule_kind={} reason={})\n",
                    entry.label, entry.rule_kind, entry.reason
                ));
            }
        }
        out
    }

    /// JSON serialization for tooling that wants the
    /// structured form. Gated by env-var at the call site;
    /// the data struct itself is always available.
    pub fn to_json_string(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_action_target_reported_as_zero_action() {
        let mut r = LegacyVsSchedulerInvocationReport::new();
        r.record_scheduler_dispatch(
            "//tensorflow/core/platform:macros",
            "cc_library",
            0,
        );
        assert_eq!(r.scheduler_count, 1);
        assert_eq!(r.zero_action_count, 1);
        assert_eq!(
            r.zero_action_scheduler_targets,
            vec!["//tensorflow/core/platform:macros".to_string()]
        );
        // And the per-rule-kind bucket got incremented.
        assert_eq!(r.scheduler_by_rule_kind.get("cc_library"), Some(&1));
        // Zero-action targets are NOT in the legacy bucket.
        assert!(r.legacy_invoked.is_empty());
    }

    #[test]
    fn legacy_invocations_group_by_rule_kind() {
        let mut r = LegacyVsSchedulerInvocationReport::new();
        r.record_legacy_invocation(
            "//foo:a",
            "gentbl_cc_library",
            "RuleKindUnverified",
        );
        r.record_legacy_invocation(
            "//foo:b",
            "gentbl_cc_library",
            "RuleKindUnverified",
        );
        r.record_legacy_invocation(
            "//foo:c",
            "tf_proto_library",
            "UnsupportedShape",
        );
        assert_eq!(r.legacy_count, 3);
        assert_eq!(
            r.legacy_by_rule_kind.get("gentbl_cc_library"),
            Some(&2)
        );
        assert_eq!(
            r.legacy_by_rule_kind.get("tf_proto_library"),
            Some(&1)
        );
        // Reasons preserved per-target.
        let reasons: Vec<&str> =
            r.legacy_invoked.iter().map(|e| e.reason.as_str()).collect();
        assert_eq!(
            reasons,
            vec!["RuleKindUnverified", "RuleKindUnverified", "UnsupportedShape"]
        );
    }

    /// Phase 5.C.4 Batch AE (commit 220): an external-repo-
    /// source-missing legacy fallback uses the new variant
    /// name as its reason string, matching the support-level
    /// vocabulary across all reports.
    #[test]
    fn external_repo_source_missing_legacy_reason_is_consistent() {
        let mut r = LegacyVsSchedulerInvocationReport::new();
        // The orchestrator's live path formats reasons via
        // `format!("{:?}", reason)` — for the
        // ExternalRepoSourceMissing variant that produces
        // `ExternalRepoSourceMissing("external repo source
        // file missing")`. Reports should preserve that
        // string verbatim so JSON consumers can match it
        // against the SchedulerSupportLevel enum's snake_case
        // kind tag `external_repo_source_missing`.
        let reason =
            r#"ExternalRepoSourceMissing("external repo source file missing")"#;
        r.record_legacy_invocation(
            "//tensorflow/core/platform:cpu_feature_guard",
            "cc_library",
            reason,
        );
        assert_eq!(r.legacy_count, 1);
        assert_eq!(
            r.legacy_invoked[0].reason,
            reason,
            "external repo source missing reason flows through verbatim"
        );
        // The bucket grouping by rule_kind still works.
        assert_eq!(r.legacy_by_rule_kind.get("cc_library"), Some(&1));
        // The human summary names the new class clearly enough
        // for operators to triage.
        let human = r.format_human();
        assert!(
            human.contains("ExternalRepoSourceMissing"),
            "human summary mentions ExternalRepoSourceMissing: {}",
            human
        );
    }

    #[test]
    fn provider_only_target_not_reported_as_failure() {
        // Pro's commit-205 invariant: a scheduler-dispatched
        // target with zero declared actions is the EXPECTED
        // shape for header-only cc_library. The report puts
        // it in the zero-action bucket — it does NOT appear
        // in the legacy fallback list, and the human summary
        // never describes it as a "failure."
        let mut r = LegacyVsSchedulerInvocationReport::new();
        r.record_scheduler_dispatch(
            "//tensorflow/core/platform:macros",
            "cc_library",
            0,
        );
        // Also dispatch one normal target with actions.
        r.record_scheduler_dispatch("//pkg:lib", "cc_library", 2);

        assert_eq!(r.scheduler_count, 2);
        assert_eq!(r.zero_action_count, 1);
        // The zero-action target is in zero_action_scheduler_targets
        // and in scheduler_dispatched — never in legacy_invoked.
        assert_eq!(r.legacy_count, 0);
        assert!(r.legacy_invoked.is_empty());
        assert!(r
            .zero_action_scheduler_targets
            .contains(&"//tensorflow/core/platform:macros".to_string()));
        // The human format must not use the word "failure" or
        // "error" anywhere when describing zero-action targets.
        let human = r.format_human();
        assert!(!human.to_lowercase().contains("failure"));
        assert!(!human.to_lowercase().contains("error"));
        // Scheduler bucket for cc_library counts both targets.
        assert_eq!(r.scheduler_by_rule_kind.get("cc_library"), Some(&2));
    }

    #[test]
    fn empty_report_is_empty() {
        let r = LegacyVsSchedulerInvocationReport::new();
        assert!(r.is_empty());
        assert_eq!(r.scheduler_count, 0);
        assert_eq!(r.legacy_count, 0);
        assert_eq!(r.zero_action_count, 0);
        // Human summary still produces a header line so
        // operators can see "yes, the report ran and there
        // was nothing to report."
        let human = r.format_human();
        assert!(human.contains("scheduler=0"));
        assert!(human.contains("legacy=0"));
    }

    #[test]
    fn json_roundtrip_preserves_shape() {
        let mut r = LegacyVsSchedulerInvocationReport::new();
        r.record_scheduler_dispatch("//a:b", "cc_library", 2);
        r.record_legacy_invocation("//c:d", "gentbl_cc_library", "RuleKindUnverified");
        let json = r.to_json_string().expect("serialize");
        let round: LegacyVsSchedulerInvocationReport =
            serde_json::from_str(&json).expect("deserialize");
        assert_eq!(round, r);
    }
}
