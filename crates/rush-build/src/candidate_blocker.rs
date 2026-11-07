// Rush Build - Candidate-Level Blocker Model
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Phase 5.C.4 Batch AX (Pro 2026-05-21 candidate-quarantine
//! directive): structured per-candidate blocker model so the
//! pilot ladder can quarantine a single candidate without
//! stopping the entire lane.
//!
//! ## Why this exists
//!
//! Pro's prior aggressive-stub-layout hard stop #3 ("compile
//! failure requires package installation") was triggering on
//! a single candidate (cpu_feature_guard, blocked by missing
//! ml_dtypes_eigen) and halting the whole pilot ladder. Pro's
//! revised directive splits outcomes into three classes:
//!
//!   - **Class 1**: fix locally and continue (narrow stub /
//!     PASS-A/PASS-B / reporting fixes).
//!   - **Class 2**: quarantine the candidate and continue
//!     (missing vendored dep, missing host package, missing
//!     tooling — affects ONE candidate, not the lane).
//!   - **Class 3**: stop the lane (verifier mismatch not
//!     locally fixable, output validation failure, broad
//!     rewrite required, no safe candidates remain).
//!
//! This module is the data model for Class 2. A
//! [`CandidateBlocker`] describes WHY a single candidate is
//! blocked, with enough structure to: (a) quarantine the
//! candidate, (b) include it in the dry-run report under a
//! distinct bucket, (c) let the candidate scanner emit a
//! human-readable explanation, and (d) be picked up later if
//! the external dependency arrives.
//!
//! ## How it composes with existing types
//!
//! The existing taxonomy already encodes similar information:
//!
//!   - [`SchedulerEligibilityReason`](crate::build_orchestrator_ext::SchedulerEligibilityReason)
//!     — per-target eligibility decision, used by the
//!     orchestrator's dispatch gate.
//!   - [`SchedulerSupportLevel`](rush_rules::SchedulerSupportLevel)
//!     — per-rule-kind support level, used in summary
//!     reporting.
//!   - [`MaterializationStatus`](crate::materialization_report::MaterializationStatus)
//!     — per-artifact materialization classification, used by
//!     the candidate scanner's gate.
//!
//! `CandidateBlocker` is candidate-LEVEL — it composes those
//! lower-level concepts into a single "why is this candidate
//! not running" verdict. A candidate may have one blocker
//! (the typical case) or multiple (when several issues
//! coexist).

use std::collections::BTreeMap;
use std::path::PathBuf;

/// Structured reasons a candidate can be blocked. Each variant
/// carries the specific evidence operators need to either
/// remediate the blocker externally or confirm that the
/// candidate is correctly quarantined.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CandidateBlocker {
    /// A vendored external dependency required by this
    /// candidate is not present on disk. Examples:
    /// `ml_dtypes_eigen` (newer Eigen vendored by TF's
    /// ml_dtypes pin), `llvm-project` clone for a
    /// gentbl-using target.
    ///
    /// Distinct from [`Self::MissingHostPackage`]: vendored
    /// deps are SPECIFIC builds/versions pinned by the
    /// upstream project (e.g., TF), not system packages.
    /// They can't be satisfied by `apt install`.
    MissingVendoredDependency {
        /// Repo/clone name as referenced in the build system
        /// (e.g. `"ml_dtypes_eigen"`).
        name: String,
        /// Free-form evidence string (file path checked,
        /// compile error excerpt, etc.) so operators can
        /// verify the diagnosis.
        evidence: String,
    },
    /// A system-installed package is required and not
    /// present. Examples: `libsnappy-dev`, `libnsync-dev` on
    /// a fresh container.
    ///
    /// Distinct from [`Self::MissingVendoredDependency`]:
    /// host packages CAN be satisfied by `apt install`
    /// (subject to Pro's hard stop on package installation).
    MissingHostPackage {
        /// Package name or header file that's missing.
        package_or_header: String,
        evidence: String,
    },
    /// A toolchain or build tool is missing or unusable.
    /// Examples: `grpc_cpp_plugin`, `mlir-tblgen`,
    /// `protoc`.
    ///
    /// Mirrors [`SchedulerEligibilityReason::MissingTool`]
    /// — the orchestrator may have already classified this
    /// at the eligibility level; we re-encode it at the
    /// candidate level for unified reporting.
    MissingTool {
        /// The tool name as referenced by Rush (e.g.
        /// `"grpc_cpp_plugin"`).
        tool: String,
    },
    /// The candidate's rule shape is on the allowlist but the
    /// scheduler's verified-shape registry does not cover
    /// this specific shape variant. Example: tf_proto_library
    /// with grpc.
    ///
    /// Mirrors
    /// [`SchedulerEligibilityReason::UnsupportedShape`].
    UnsupportedShape {
        /// Short descriptor of the unsupported shape (e.g.
        /// `"tf_proto_library_grpc"`).
        shape: String,
    },
    /// A NON-STUB external repo has missing source files on
    /// disk. Distinct from
    /// [`ExternalSystemStub`](rush_rules::SchedulerSupportLevel::ExternalSystemStub)
    /// (which is intentional and admitted): this is a real
    /// gap and remains a blocker.
    ExternalSourceMissing {
        /// External repo name.
        repo: String,
        /// Declared on-disk path of the missing source.
        path: PathBuf,
    },
    /// An external repo's stub BUILD has a known layout bug
    /// (under-declared includes, wrong package root, etc.).
    /// Used to track stub-layout findings (Batch AQ/AS
    /// pattern). Typically Class 1 (fixable locally) but
    /// recorded here when a fix is queued but not yet
    /// landed.
    ExternalRepoStubLayout {
        repo: String,
        /// Free-form description of the layout issue.
        detail: String,
    },
    /// `--verify-scheduler-commands` reported a mismatch
    /// between legacy and scheduler argv for this candidate.
    /// Typically Class 3 (stop the lane) unless it's a
    /// narrow PASS-A/PASS-B symmetry bug.
    VerifierMismatch {
        /// Target label.
        target: String,
        /// Action mnemonic + output path that diverged.
        action: String,
    },
    /// Output validation rejected an action's declared output
    /// at execution time. Class 3 if the action was
    /// scheduler-ready.
    OutputValidationFailure {
        target: String,
        output: PathBuf,
    },
    /// The action graph is missing executable wiring for a
    /// declared action. Class 3 typically — verified shapes
    /// must have complete action graphs.
    ActionGraphIncomplete {
        target: String,
    },
    /// The candidate's blast radius is too broad for the
    /// current lane (e.g., requires platform_base or full-TF
    /// scale). Class 2 — skip this candidate and continue
    /// with smaller ones.
    TooBroad {
        /// Why the candidate is too broad (e.g.
        /// `"requires platform_base"`).
        reason: String,
    },
    /// Two targets in different packages/workspaces share
    /// `self.name`, producing identical `rush-out/<name>/...`
    /// paths. The second target's `declare_action` fails
    /// with `DuplicateProducer`, leaving its scheduler
    /// action graph empty. The BD-1 case is the canonical
    /// example.
    ///
    /// Class 2: quarantine the candidate; fix requires
    /// non-local artifact-ownership refactor (see Batch BN
    /// `docs/analysis/output-path-collision-audit.md`).
    OutputPathCollision {
        /// Target label that collided.
        target: String,
        /// Conflicting `self.name`.
        name: String,
        /// Other target(s) sharing the name (typically one,
        /// across-package).
        other_targets: Vec<String>,
    },
}

impl CandidateBlocker {
    /// Stable kebab-case label for the blocker kind.
    /// Used in human-readable reports.
    pub fn kind_label(&self) -> &'static str {
        match self {
            CandidateBlocker::MissingVendoredDependency { .. } => {
                "missing_vendored_dependency"
            }
            CandidateBlocker::MissingHostPackage { .. } => "missing_host_package",
            CandidateBlocker::MissingTool { .. } => "missing_tool",
            CandidateBlocker::UnsupportedShape { .. } => "unsupported_shape",
            CandidateBlocker::ExternalSourceMissing { .. } => "external_source_missing",
            CandidateBlocker::ExternalRepoStubLayout { .. } => {
                "external_repo_stub_layout"
            }
            CandidateBlocker::VerifierMismatch { .. } => "verifier_mismatch",
            CandidateBlocker::OutputValidationFailure { .. } => {
                "output_validation_failure"
            }
            CandidateBlocker::ActionGraphIncomplete { .. } => "action_graph_incomplete",
            CandidateBlocker::TooBroad { .. } => "too_broad",
            CandidateBlocker::OutputPathCollision { .. } => "output_path_collision",
        }
    }

    /// Pro's class taxonomy. Class 1 = fix locally and
    /// continue; Class 2 = quarantine and continue; Class 3 =
    /// stop the lane.
    ///
    /// This classification is the CANONICAL mapping. The
    /// pilot ladder consults it to decide whether to stop or
    /// keep going.
    pub fn class(&self) -> CandidateBlockerClass {
        match self {
            // Class 2: quarantine — single-candidate
            // blocker, lane continues.
            CandidateBlocker::MissingVendoredDependency { .. }
            | CandidateBlocker::MissingHostPackage { .. }
            | CandidateBlocker::MissingTool { .. }
            | CandidateBlocker::UnsupportedShape { .. }
            | CandidateBlocker::ExternalSourceMissing { .. }
            | CandidateBlocker::TooBroad { .. }
            | CandidateBlocker::OutputPathCollision { .. } => {
                CandidateBlockerClass::Quarantine
            }
            // Class 1: fixable narrowly — landed via a
            // narrow stub/PASS-symmetry commit.
            CandidateBlocker::ExternalRepoStubLayout { .. } => {
                CandidateBlockerClass::FixLocally
            }
            // Class 3: stop the lane — these are real
            // correctness gaps that can't be quarantined.
            CandidateBlocker::VerifierMismatch { .. }
            | CandidateBlocker::OutputValidationFailure { .. }
            | CandidateBlocker::ActionGraphIncomplete { .. } => {
                CandidateBlockerClass::StopLane
            }
        }
    }

    /// External action (if any) that would unblock this
    /// candidate. Returns `None` when the blocker is
    /// internal (e.g. verifier mismatch).
    ///
    /// Example: a `MissingVendoredDependency("ml_dtypes_eigen", ...)`
    /// returns `Some("provide TF-pinned ml_dtypes_eigen
    /// checkout or compatible Eigen package")`.
    pub fn external_action(&self) -> Option<String> {
        match self {
            CandidateBlocker::MissingVendoredDependency { name, .. } => Some(format!(
                "provide vendored {} checkout (typically via TF's pinned clone)",
                name
            )),
            CandidateBlocker::MissingHostPackage {
                package_or_header, ..
            } => Some(format!(
                "install host package providing {}",
                package_or_header
            )),
            CandidateBlocker::MissingTool { tool } => {
                Some(format!("install or configure {} build tool", tool))
            }
            CandidateBlocker::UnsupportedShape { shape } => Some(format!(
                "add {} to VerifiedSchedulerShape registry once shape semantics \
                 are pinned",
                shape
            )),
            CandidateBlocker::ExternalSourceMissing { repo, .. } => Some(format!(
                "fetch or vendor the @{} external repo with declared sources",
                repo
            )),
            CandidateBlocker::ExternalRepoStubLayout { repo, .. } => Some(format!(
                "land a narrow stub-layout fix for @{} in external_deps.rs",
                repo
            )),
            CandidateBlocker::TooBroad { .. } => None,
            CandidateBlocker::OutputPathCollision { name, .. } => Some(format!(
                "package-qualify output paths for '{}' (or apply BN audit \
                 Option A/B/C/D); see docs/analysis/output-path-collision-audit.md",
                name
            )),
            CandidateBlocker::VerifierMismatch { .. }
            | CandidateBlocker::OutputValidationFailure { .. }
            | CandidateBlocker::ActionGraphIncomplete { .. } => None,
        }
    }
}

/// The three-class taxonomy Pro pinned in the 2026-05-21
/// candidate-quarantine directive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateBlockerClass {
    /// Class 1: fixable with a narrow local commit (stub
    /// layout, PASS-A/PASS-B symmetry, reporting).
    FixLocally,
    /// Class 2: quarantine the candidate, continue with the
    /// next.
    Quarantine,
    /// Class 3: stop the lane.
    StopLane,
}

impl CandidateBlockerClass {
    /// Stable label for the class.
    pub fn label(&self) -> &'static str {
        match self {
            CandidateBlockerClass::FixLocally => "fix_locally",
            CandidateBlockerClass::Quarantine => "quarantine",
            CandidateBlockerClass::StopLane => "stop_lane",
        }
    }
}

/// Per-candidate lifecycle status. Tracks the candidate
/// through scan → block/quarantine → execute → done.
///
/// Struct variants (rather than newtype tuple variants) are
/// used so the serde `tag = "kind"` discriminator can sit
/// alongside the payload field — newtype variants containing
/// a sequence are not compatible with internally tagged enums.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CandidateStatus {
    /// Candidate passed all gates and is ready to execute.
    Ready,
    /// Candidate has one or more blockers. `Blocked` is used
    /// during the initial scan when the candidate hasn't yet
    /// been quarantined (the lane may still attempt a narrow
    /// fix for Class 1 blockers).
    Blocked { blockers: Vec<CandidateBlocker> },
    /// Candidate has Class 2 blockers and has been
    /// quarantined. The lane continues with other candidates.
    /// May be reconsidered if the external action is taken
    /// later.
    Quarantined { blockers: Vec<CandidateBlocker> },
    /// Candidate executed successfully under
    /// `--use-scheduler`.
    Executed,
    /// Candidate executed but failed at runtime. Distinct
    /// from `Blocked`/`Quarantined`: the action graph was
    /// scheduler-dispatched but the action itself failed.
    Failed { blockers: Vec<CandidateBlocker> },
}

impl CandidateStatus {
    /// True iff status is `Ready`.
    pub fn is_ready(&self) -> bool {
        matches!(self, CandidateStatus::Ready)
    }

    /// True iff status is `Quarantined`.
    pub fn is_quarantined(&self) -> bool {
        matches!(self, CandidateStatus::Quarantined { .. })
    }

    /// True iff status is terminal: Executed, Failed, or
    /// Quarantined (no further work in this lane).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            CandidateStatus::Executed
                | CandidateStatus::Failed { .. }
                | CandidateStatus::Quarantined { .. }
        )
    }

    /// Stable kebab-case label for the status variant.
    pub fn kind_label(&self) -> &'static str {
        match self {
            CandidateStatus::Ready => "ready",
            CandidateStatus::Blocked { .. } => "blocked",
            CandidateStatus::Quarantined { .. } => "quarantined",
            CandidateStatus::Executed => "executed",
            CandidateStatus::Failed { .. } => "failed",
        }
    }

    /// All blockers attached to this status, in declaration
    /// order. Returns an empty slice for `Ready` /
    /// `Executed`.
    pub fn blockers(&self) -> &[CandidateBlocker] {
        match self {
            CandidateStatus::Ready | CandidateStatus::Executed => &[],
            CandidateStatus::Blocked { blockers }
            | CandidateStatus::Quarantined { blockers }
            | CandidateStatus::Failed { blockers } => blockers,
        }
    }
}

/// One row of the candidate report: a target label paired
/// with its current [`CandidateStatus`]. Used by the
/// candidate scanner and the 5.C.5 dry-run report.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CandidateReportEntry {
    /// Target label (e.g. `"//tensorflow/core/platform:cpu_feature_guard"`).
    pub target: String,
    /// Status as of the last scan/run.
    pub status: CandidateStatus,
    /// Optional rule kind for display grouping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_kind: Option<String>,
    /// Optional action count (when known from analyze pass).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_count: Option<u32>,
}

/// Aggregate counts by status, used by the dry-run report's
/// candidate-status section.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct CandidateStatusCounts {
    pub ready: usize,
    pub blocked: usize,
    pub quarantined: usize,
    pub executed: usize,
    pub failed: usize,
}

impl CandidateStatusCounts {
    /// Tally a slice of entries into status counts.
    pub fn tally(entries: &[CandidateReportEntry]) -> Self {
        let mut counts = Self::default();
        for e in entries {
            match &e.status {
                CandidateStatus::Ready => counts.ready += 1,
                CandidateStatus::Blocked { .. } => counts.blocked += 1,
                CandidateStatus::Quarantined { .. } => counts.quarantined += 1,
                CandidateStatus::Executed => counts.executed += 1,
                CandidateStatus::Failed { .. } => counts.failed += 1,
            }
        }
        counts
    }

    /// Total count across all status variants.
    pub fn total(&self) -> usize {
        self.ready + self.blocked + self.quarantined + self.executed + self.failed
    }
}

/// Aggregate blocker counts grouped by kind. Lets the dry-run
/// report and CI gates answer "how many candidates are
/// blocked by missing vendored deps?" without re-scanning.
pub fn blocker_counts_by_kind(
    entries: &[CandidateReportEntry],
) -> BTreeMap<String, usize> {
    let mut out: BTreeMap<String, usize> = BTreeMap::new();
    for e in entries {
        for b in e.status.blockers() {
            *out.entry(b.kind_label().to_string()).or_insert(0) += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AX-3 directive test: a missing vendored dependency
    /// quarantines the candidate. The lane continues.
    #[test]
    fn missing_vendored_dependency_quarantines_candidate() {
        let blocker = CandidateBlocker::MissingVendoredDependency {
            name: "ml_dtypes_eigen".to_string(),
            evidence: "external/ml_dtypes_eigen/Eigen/Core not present; \
                       float8.h needs newer Eigen with Traits<float>::BitsType"
                .to_string(),
        };
        assert_eq!(blocker.class(), CandidateBlockerClass::Quarantine);
        assert_eq!(blocker.kind_label(), "missing_vendored_dependency");
        assert!(blocker
            .external_action()
            .unwrap()
            .contains("ml_dtypes_eigen"));

        let status = CandidateStatus::Quarantined { blockers: vec![blocker] };
        assert!(status.is_quarantined());
        assert!(status.is_terminal());
        assert!(!status.is_ready());
    }

    /// AX-3 directive test: a quarantined candidate in the
    /// report does NOT stop the scan. A subsequent ready
    /// candidate still gets scanned and ranked.
    #[test]
    fn quarantined_candidate_does_not_stop_scan() {
        let entries = vec![
            CandidateReportEntry {
                target: "//tensorflow/core/platform:cpu_feature_guard".to_string(),
                status: CandidateStatus::Quarantined { blockers: vec![
                    CandidateBlocker::MissingVendoredDependency {
                        name: "ml_dtypes_eigen".to_string(),
                        evidence: "float8.h Eigen Traits error".to_string(),
                    },
                ] },
                rule_kind: Some("cc_library".to_string()),
                action_count: Some(2),
            },
            CandidateReportEntry {
                target: "//tensorflow/core/platform:macros".to_string(),
                status: CandidateStatus::Ready,
                rule_kind: Some("cc_library".to_string()),
                action_count: Some(0),
            },
            CandidateReportEntry {
                target: "//tensorflow/core/platform:tensor_coding".to_string(),
                status: CandidateStatus::Ready,
                rule_kind: Some("cc_library".to_string()),
                action_count: Some(2),
            },
        ];
        let counts = CandidateStatusCounts::tally(&entries);
        assert_eq!(counts.ready, 2);
        assert_eq!(counts.quarantined, 1);
        assert_eq!(counts.total(), 3);
    }

    /// AX-3 directive test: ready candidates are still
    /// counted/ranked in the report even when other
    /// candidates are quarantined or blocked.
    #[test]
    fn ready_candidates_still_ranked() {
        let entries = vec![
            CandidateReportEntry {
                target: "//pkg:quarantined_a".to_string(),
                status: CandidateStatus::Quarantined { blockers: vec![
                    CandidateBlocker::MissingTool {
                        tool: "grpc_cpp_plugin".to_string(),
                    },
                ] },
                rule_kind: None,
                action_count: None,
            },
            CandidateReportEntry {
                target: "//pkg:ready_a".to_string(),
                status: CandidateStatus::Ready,
                rule_kind: None,
                action_count: Some(3),
            },
            CandidateReportEntry {
                target: "//pkg:ready_b".to_string(),
                status: CandidateStatus::Ready,
                rule_kind: None,
                action_count: Some(5),
            },
        ];
        let ready: Vec<_> = entries
            .iter()
            .filter(|e| e.status.is_ready())
            .collect();
        assert_eq!(ready.len(), 2);
        // The lane would pick from these two; quarantined
        // candidates don't enter the selection set.
    }

    /// BN: OutputPathCollision is the dedicated kind for
    /// BD-1-class duplicate output path quarantines. It
    /// classifies as Class 2 (quarantine + continue) and
    /// emits a stable kebab-case label.
    #[test]
    fn bn_output_path_collision_quarantines_with_correct_label() {
        let b = CandidateBlocker::OutputPathCollision {
            target: "//tensorflow/core/protobuf:error_codes_proto_impl".to_string(),
            name: "error_codes_proto_impl".to_string(),
            other_targets: vec![
                "@local_tsl//tsl/protobuf:error_codes_proto_impl".to_string(),
            ],
        };
        assert_eq!(b.class(), CandidateBlockerClass::Quarantine);
        assert_eq!(b.kind_label(), "output_path_collision");
        // external_action describes the fix without
        // implementing it (per Pro D2 directive).
        let action = b.external_action().unwrap();
        assert!(action.contains("error_codes_proto_impl"));
        assert!(action.contains("output-path-collision-audit"));
        // Serde shape pin.
        let json = serde_json::to_string(&b).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["kind"], "output_path_collision");
        assert_eq!(parsed["name"], "error_codes_proto_impl");
        assert!(parsed["other_targets"].is_array());
    }

    /// AX: blocker class taxonomy is canonical. Class 1 fixes
    /// locally; Class 2 quarantines; Class 3 stops the lane.
    #[test]
    fn ax_blocker_class_taxonomy_canonical() {
        // Class 2: quarantine
        assert_eq!(
            CandidateBlocker::MissingVendoredDependency {
                name: "x".to_string(),
                evidence: "y".to_string(),
            }
            .class(),
            CandidateBlockerClass::Quarantine
        );
        assert_eq!(
            CandidateBlocker::MissingHostPackage {
                package_or_header: "libfoo-dev".to_string(),
                evidence: "header not found".to_string(),
            }
            .class(),
            CandidateBlockerClass::Quarantine
        );
        assert_eq!(
            CandidateBlocker::MissingTool {
                tool: "protoc".to_string(),
            }
            .class(),
            CandidateBlockerClass::Quarantine
        );
        assert_eq!(
            CandidateBlocker::TooBroad {
                reason: "requires platform_base".to_string(),
            }
            .class(),
            CandidateBlockerClass::Quarantine
        );
        assert_eq!(
            CandidateBlocker::OutputPathCollision {
                target: "//tf/core/protobuf:error_codes_proto_impl".to_string(),
                name: "error_codes_proto_impl".to_string(),
                other_targets: vec![
                    "@local_tsl//tsl/protobuf:error_codes_proto_impl".to_string(),
                ],
            }
            .class(),
            CandidateBlockerClass::Quarantine
        );

        // Class 1: fix locally
        assert_eq!(
            CandidateBlocker::ExternalRepoStubLayout {
                repo: "ml_dtypes".to_string(),
                detail: "missing vendored subdir in includes".to_string(),
            }
            .class(),
            CandidateBlockerClass::FixLocally
        );

        // Class 3: stop lane
        assert_eq!(
            CandidateBlocker::VerifierMismatch {
                target: "//pkg:t".to_string(),
                action: "CppCompile".to_string(),
            }
            .class(),
            CandidateBlockerClass::StopLane
        );
        assert_eq!(
            CandidateBlocker::OutputValidationFailure {
                target: "//pkg:t".to_string(),
                output: PathBuf::from("foo.o"),
            }
            .class(),
            CandidateBlockerClass::StopLane
        );
    }

    /// AX: serde shape pin. The discriminator is `kind`, the
    /// payload fields are named.
    #[test]
    fn ax_candidate_blocker_serializes_with_kind_discriminator() {
        let b = CandidateBlocker::MissingVendoredDependency {
            name: "ml_dtypes_eigen".to_string(),
            evidence: "float8.h Eigen Traits error".to_string(),
        };
        let json = serde_json::to_string(&b).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["kind"], "missing_vendored_dependency");
        assert_eq!(parsed["name"], "ml_dtypes_eigen");
        assert!(parsed["evidence"]
            .as_str()
            .unwrap()
            .contains("float8.h"));
    }

    /// AX: CandidateStatus serde shape pin.
    #[test]
    fn ax_candidate_status_serializes_with_kind() {
        let s = CandidateStatus::Quarantined {
            blockers: vec![CandidateBlocker::MissingTool {
                tool: "protoc".to_string(),
            }],
        };
        let json = serde_json::to_string(&s).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["kind"], "quarantined");
    }

    /// AX: CandidateStatusCounts::tally aggregates as
    /// expected.
    #[test]
    fn ax_candidate_status_counts_tally() {
        let entries = vec![
            CandidateReportEntry {
                target: "//a:ready".to_string(),
                status: CandidateStatus::Ready,
                rule_kind: None,
                action_count: None,
            },
            CandidateReportEntry {
                target: "//b:ready".to_string(),
                status: CandidateStatus::Ready,
                rule_kind: None,
                action_count: None,
            },
            CandidateReportEntry {
                target: "//c:executed".to_string(),
                status: CandidateStatus::Executed,
                rule_kind: None,
                action_count: None,
            },
            CandidateReportEntry {
                target: "//d:quarantined".to_string(),
                status: CandidateStatus::Quarantined { blockers: vec![
                    CandidateBlocker::MissingVendoredDependency {
                        name: "x".to_string(),
                        evidence: "y".to_string(),
                    },
                ] },
                rule_kind: None,
                action_count: None,
            },
            CandidateReportEntry {
                target: "//e:failed".to_string(),
                status: CandidateStatus::Failed { blockers: vec![
                    CandidateBlocker::ActionGraphIncomplete {
                        target: "//e:failed".to_string(),
                    },
                ] },
                rule_kind: None,
                action_count: None,
            },
        ];
        let counts = CandidateStatusCounts::tally(&entries);
        assert_eq!(counts.ready, 2);
        assert_eq!(counts.executed, 1);
        assert_eq!(counts.quarantined, 1);
        assert_eq!(counts.failed, 1);
        assert_eq!(counts.blocked, 0);
        assert_eq!(counts.total(), 5);
    }

    /// AX: blocker_counts_by_kind aggregates blockers by
    /// kind across all entries.
    #[test]
    fn ax_blocker_counts_by_kind_aggregates() {
        let entries = vec![
            CandidateReportEntry {
                target: "//a".to_string(),
                status: CandidateStatus::Quarantined { blockers: vec![
                    CandidateBlocker::MissingVendoredDependency {
                        name: "ml_dtypes_eigen".to_string(),
                        evidence: "".to_string(),
                    },
                ] },
                rule_kind: None,
                action_count: None,
            },
            CandidateReportEntry {
                target: "//b".to_string(),
                status: CandidateStatus::Quarantined { blockers: vec![
                    CandidateBlocker::MissingVendoredDependency {
                        name: "llvm-project".to_string(),
                        evidence: "".to_string(),
                    },
                ] },
                rule_kind: None,
                action_count: None,
            },
            CandidateReportEntry {
                target: "//c".to_string(),
                status: CandidateStatus::Quarantined { blockers: vec![
                    CandidateBlocker::MissingTool {
                        tool: "grpc_cpp_plugin".to_string(),
                    },
                ] },
                rule_kind: None,
                action_count: None,
            },
        ];
        let counts = blocker_counts_by_kind(&entries);
        assert_eq!(counts.get("missing_vendored_dependency").copied(), Some(2));
        assert_eq!(counts.get("missing_tool").copied(), Some(1));
        assert_eq!(counts.len(), 2);
    }

    /// AX: external_action() returns guidance for the blocker
    /// classes that have a remediation path.
    #[test]
    fn ax_external_action_returns_guidance_for_remediable_blockers() {
        let vendored = CandidateBlocker::MissingVendoredDependency {
            name: "ml_dtypes_eigen".to_string(),
            evidence: "".to_string(),
        };
        assert!(vendored
            .external_action()
            .unwrap()
            .contains("ml_dtypes_eigen"));

        let host = CandidateBlocker::MissingHostPackage {
            package_or_header: "libsnappy-dev".to_string(),
            evidence: "".to_string(),
        };
        assert!(host.external_action().unwrap().contains("libsnappy-dev"));

        let tool = CandidateBlocker::MissingTool {
            tool: "protoc".to_string(),
        };
        assert!(tool.external_action().unwrap().contains("protoc"));

        // Verifier mismatch is not remediable externally.
        let mismatch = CandidateBlocker::VerifierMismatch {
            target: "//pkg:t".to_string(),
            action: "CppCompile".to_string(),
        };
        assert!(mismatch.external_action().is_none());
    }

    /// AX: CandidateStatus::blockers() returns the blocker
    /// vec for non-terminal-success statuses, empty
    /// otherwise.
    #[test]
    fn ax_candidate_status_blockers_returns_attached_blockers() {
        assert!(CandidateStatus::Ready.blockers().is_empty());
        assert!(CandidateStatus::Executed.blockers().is_empty());

        let b = CandidateBlocker::MissingTool {
            tool: "grpc".to_string(),
        };
        let blocked = CandidateStatus::Blocked { blockers: vec![b.clone()] };
        assert_eq!(blocked.blockers().len(), 1);
        let quarantined = CandidateStatus::Quarantined { blockers: vec![b.clone()] };
        assert_eq!(quarantined.blockers().len(), 1);
        let failed = CandidateStatus::Failed { blockers: vec![b] };
        assert_eq!(failed.blockers().len(), 1);
    }
}
