// Rush Action - Scheduler classification stub
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Phase 5.C.4 step 3: action classification stub.
//!
//! Walks the [`ActionDag`] in topological order and classifies
//! every declared action into one of three buckets:
//!
//!   - **`Ready`** — `status == Complete` AND
//!     `executable_action(id).is_some()`. The eventual phase 5.C.4
//!     scheduler can execute this action directly via the
//!     `ExecutionStrategy` boundary.
//!   - **`LegacyComplete`** — `status == Complete` but no
//!     `ExecutableAction` payload has been set. The action's
//!     declaring rule hasn't migrated yet; the scheduler must
//!     defer to the rule's legacy `build()` for this action's
//!     owner_target.
//!   - **`LegacyIncomplete`** — `status == Incomplete`
//!     (cross-target inputs deferred to phase 5.C, etc.). Always
//!     legacy build() regardless of ExecutableAction presence;
//!     phase 5.C0 P4 designed Incomplete actions to be unschedulable
//!     until they're upgraded.
//!
//! The stub does NOT execute anything. Phase 5.C.4 itself adds
//! the execution path by walking the same plan and dispatching
//! Ready actions through `ExecutionStrategy`. This module is the
//! shape that step 2b's per-rule migrations and step 3-proper's
//! execution path will both consume.
//!
//! ## Migration progress metric
//!
//! `SchedulerPlan` exposes per-bucket counts. As rules' analyze()
//! impls migrate to populate `ExecutableAction`, the `Ready` count
//! grows and `LegacyComplete` shrinks. When `LegacyComplete` hits
//! zero (every Complete action has a payload), the legacy build()
//! fallback can retire — that's the gate for phase 5.C.5.

use crate::action_dag::ActionDag;
use crate::ids::{ActionDeclarationStatus, ArtifactRegistry, DeclaredActionId, TargetId};
use std::collections::{HashMap, HashSet};

/// Classification of a single declared action with respect to the
/// phase 5.C.4 scheduler.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum SchedulerVerdict {
    /// Complete + has ExecutableAction — phase 5.C.4 scheduler runs it.
    Ready,
    /// Complete but no ExecutableAction — legacy `rule.build()`
    /// path. The owning target's rule hasn't been migrated yet.
    LegacyComplete,
    /// Incomplete declaration — always legacy regardless of
    /// payload. Phase 5.C0 P4 marker.
    LegacyIncomplete,
}

impl SchedulerVerdict {
    /// Whether the phase 5.C.4 scheduler can execute this action
    /// without falling through to legacy `build()`.
    pub fn is_ready(self) -> bool {
        matches!(self, SchedulerVerdict::Ready)
    }
}

/// Per-target action bucket counts. Phase 5.C.4 step 4 prep.
/// Returned by [`SchedulerPlan::per_target_buckets`] keyed by
/// `TargetId`. The eligibility predicate is `legacy_complete +
/// legacy_incomplete == 0 && ready > 0`.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct PerTargetBuckets {
    /// Number of this target's Complete actions whose
    /// ExecutableAction is set.
    pub ready: usize,
    /// Number of this target's Complete actions whose
    /// ExecutableAction is unset — needs legacy `build()`.
    pub legacy_complete: usize,
    /// Number of this target's Incomplete actions — always
    /// legacy regardless of payload.
    pub legacy_incomplete: usize,
}

impl PerTargetBuckets {
    /// Total number of actions for this target across all
    /// buckets.
    pub fn total(&self) -> usize {
        self.ready + self.legacy_complete + self.legacy_incomplete
    }

    /// Whether the scheduler can execute every action in this
    /// target. Ineligible if any LegacyComplete or
    /// LegacyIncomplete action is present, or if total == 0.
    pub fn is_scheduler_eligible(&self) -> bool {
        self.legacy_complete == 0 && self.legacy_incomplete == 0 && self.ready > 0
    }
}

/// Phase 5.C.4 commit 47: per-mnemonic legacy breakdown
/// entry with named fields. Mirror of
/// `(String, usize, usize)` from
/// `legacy_breakdown_by_mnemonic`, but with a struct shape so
/// JSON consumers (RUSH_LEGACY_BREAKDOWN_JSON=1) see
/// `.mnemonic`, `.legacy_complete`, `.legacy_incomplete`
/// rather than positional `.[0]`, `.[1]`, `.[2]`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LegacyBreakdownEntry {
    /// Action mnemonic (`CppCompile`, `CcArchive`, etc.).
    pub mnemonic: String,
    /// Number of LegacyComplete actions for this mnemonic
    /// (declared but missing ExecutableAction — needs rule-
    /// side migration to populate).
    pub legacy_complete: usize,
    /// Number of LegacyIncomplete actions for this mnemonic
    /// (status Incomplete — needs rule-side declaration
    /// work).
    pub legacy_incomplete: usize,
}

/// Phase 5.C.4 commit 47: legacy breakdown report. Top-level
/// container for the per-mnemonic entries plus aggregate
/// totals across all mnemonics. Serialized to JSON for CI
/// gating; CI scripts assert
/// `.total_legacy_complete <= threshold` or per-mnemonic
/// regressions.
/// Current legacy-breakdown JSON schema version. Bumped on
/// breaking JSON shape changes; CI consumers gate on equality.
pub const LEGACY_BREAKDOWN_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct LegacyBreakdownReport {
    /// Phase 5.C.4 commit 54: schema version. Bumped on
    /// breaking JSON shape changes.
    pub schema_version: u32,
    /// Per-mnemonic entries, sorted by total legacy count
    /// desc + mnemonic name asc tie-break (same order as
    /// `legacy_breakdown_by_mnemonic`).
    pub entries: Vec<LegacyBreakdownEntry>,
    /// Sum of `legacy_complete` across all entries.
    pub total_legacy_complete: usize,
    /// Sum of `legacy_incomplete` across all entries.
    pub total_legacy_incomplete: usize,
}

/// Topologically-ordered classification of every declared action
/// in the registry, paired with its scheduler verdict.
#[derive(Debug)]
pub struct SchedulerPlan {
    /// (action id, verdict) pairs in topological order. Same
    /// order as `ActionDag::topo_order()` so consumers can walk
    /// the plan and the DAG in lockstep.
    pub entries: Vec<(DeclaredActionId, SchedulerVerdict)>,
    ready: usize,
    legacy_complete: usize,
    legacy_incomplete: usize,
}

impl SchedulerPlan {
    /// Build the plan by walking the DAG's topological order and
    /// classifying each action against the registry's
    /// `ExecutableAction` side table and the action's status.
    pub fn build(reg: &ArtifactRegistry, dag: &ActionDag) -> Self {
        let mut entries: Vec<(DeclaredActionId, SchedulerVerdict)> =
            Vec::with_capacity(dag.action_count());
        let mut ready = 0usize;
        let mut legacy_complete = 0usize;
        let mut legacy_incomplete = 0usize;

        for &id in dag.topo_order() {
            let action = reg.declared_action(id);
            let verdict = match action.status {
                ActionDeclarationStatus::Incomplete { .. } => {
                    legacy_incomplete += 1;
                    SchedulerVerdict::LegacyIncomplete
                }
                ActionDeclarationStatus::Complete => {
                    if reg.executable_action(id).is_some() {
                        ready += 1;
                        SchedulerVerdict::Ready
                    } else {
                        legacy_complete += 1;
                        SchedulerVerdict::LegacyComplete
                    }
                }
            };
            entries.push((id, verdict));
        }

        SchedulerPlan {
            entries,
            ready,
            legacy_complete,
            legacy_incomplete,
        }
    }

    /// Number of actions the phase 5.C.4 scheduler can execute
    /// directly. Migration progress metric — grows toward
    /// `total()` as rules migrate.
    pub fn ready_count(&self) -> usize {
        self.ready
    }

    /// Number of Complete actions still waiting on rule migration.
    /// Phase 5.C.5 fires when this hits zero.
    pub fn legacy_complete_count(&self) -> usize {
        self.legacy_complete
    }

    /// Number of Incomplete actions — always legacy regardless of
    /// payload. Phase 5.C0 P4 marker.
    pub fn legacy_incomplete_count(&self) -> usize {
        self.legacy_incomplete
    }

    /// Total actions covered by this plan. Equals
    /// `ActionDag::action_count()`.
    pub fn total(&self) -> usize {
        self.entries.len()
    }

    /// Format a one-line summary of the plan's bucket counts.
    /// Caller wraps this in their preferred print/log channel.
    pub fn summary_line(&self) -> String {
        format!(
            "🧭 scheduler-plan: {} action(s) — {} ready, {} legacy-complete, {} legacy-incomplete",
            self.total(),
            self.ready,
            self.legacy_complete,
            self.legacy_incomplete,
        )
    }

    /// Phase 5.C.4 commit 47: per-mnemonic legacy breakdown
    /// in a serializable struct shape. Wraps the
    /// (String, usize, usize) tuples from
    /// `legacy_breakdown_by_mnemonic` in named fields so JSON
    /// consumers see `.entries[].mnemonic` rather than `.[0]`.
    /// Used by the orchestrator's RUSH_LEGACY_BREAKDOWN_JSON=1
    /// surface.
    pub fn legacy_breakdown_report(
        &self,
        reg: &ArtifactRegistry,
    ) -> LegacyBreakdownReport {
        let entries: Vec<LegacyBreakdownEntry> = self
            .legacy_breakdown_by_mnemonic(reg)
            .into_iter()
            .map(|(mnemonic, lc, li)| LegacyBreakdownEntry {
                mnemonic,
                legacy_complete: lc,
                legacy_incomplete: li,
            })
            .collect();
        let total_legacy_complete: usize =
            entries.iter().map(|e| e.legacy_complete).sum();
        let total_legacy_incomplete: usize =
            entries.iter().map(|e| e.legacy_incomplete).sum();
        LegacyBreakdownReport {
            schema_version: LEGACY_BREAKDOWN_SCHEMA_VERSION,
            entries,
            total_legacy_complete,
            total_legacy_incomplete,
        }
    }

    /// Phase 5.C.4 commit 44: per-mnemonic legacy breakdown.
    ///
    /// Walks the plan's entries and groups Legacy* verdicts
    /// by action mnemonic. Returns Vec of (mnemonic, legacy_
    /// complete_count, legacy_incomplete_count) sorted by
    /// total legacy count descending — operators see the
    /// biggest debt buckets first.
    ///
    /// Used by the analyze-only / explain-action-keys path
    /// to answer "which rule families dominate the migration
    /// gap." Today's per-target census (commit 40) tells
    /// operators WHICH targets aren't dispatched; this tells
    /// them WHICH mnemonics within those targets need
    /// ExecutableAction populated. The two views are
    /// complementary: target-level for "which rule kind to
    /// migrate next," action-level for "which action shapes
    /// are most expensive to leave un-migrated."
    pub fn legacy_breakdown_by_mnemonic(
        &self,
        reg: &ArtifactRegistry,
    ) -> Vec<(String, usize, usize)> {
        let mut by_mn: HashMap<String, (usize, usize)> = HashMap::new();
        for &(id, verdict) in self.entries.iter() {
            let (lc, li) = match verdict {
                SchedulerVerdict::LegacyComplete => (1usize, 0usize),
                SchedulerVerdict::LegacyIncomplete => (0usize, 1usize),
                SchedulerVerdict::Ready => continue,
            };
            let action = reg.declared_action(id);
            let mn = reg.strings().str(action.mnemonic).to_string();
            let entry = by_mn.entry(mn).or_insert((0, 0));
            entry.0 += lc;
            entry.1 += li;
        }
        let mut out: Vec<(String, usize, usize)> = by_mn
            .into_iter()
            .map(|(mn, (lc, li))| (mn, lc, li))
            .collect();
        // Sort by total legacy count descending; stable
        // tiebreak on mnemonic name for determinism.
        out.sort_by(|a, b| {
            (b.1 + b.2)
                .cmp(&(a.1 + a.2))
                .then_with(|| a.0.cmp(&b.0))
        });
        out
    }

    /// Phase 5.C.4 commit 44: format the legacy-mnemonic
    /// breakdown as multi-line human-readable output.
    ///
    /// Empty buckets return empty Vec (caller skips the
    /// dump). Otherwise:
    ///
    /// ```text
    /// Legacy-action breakdown by mnemonic (5 mnemonics):
    ///   CppCompile: 23 legacy-complete, 0 legacy-incomplete
    ///   CcArchive: 12 legacy-complete, 0 legacy-incomplete
    ///   ProtocCompile: 0 legacy-complete, 4 legacy-incomplete
    /// ```
    pub fn format_legacy_breakdown(
        &self,
        reg: &ArtifactRegistry,
    ) -> Vec<String> {
        let entries = self.legacy_breakdown_by_mnemonic(reg);
        if entries.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(entries.len() + 1);
        out.push(format!(
            "Legacy-action breakdown by mnemonic ({} mnemonics):",
            entries.len()
        ));
        for (mn, lc, li) in entries {
            out.push(format!(
                "  {}: {} legacy-complete, {} legacy-incomplete",
                mn, lc, li
            ));
        }
        out
    }

    /// Phase 5.C.4 step 4 prep: per-target action bucket counts.
    ///
    /// Walks the plan's entries and groups them by
    /// `DeclaredAction.owner_target`, returning (ready,
    /// legacy_complete, legacy_incomplete) per target. Step 4
    /// proper uses this to decide per-target whether the
    /// scheduler can execute (all Complete actions Ready) or
    /// whether the target stays on the legacy `build()` path.
    /// Mixed targets — some Ready, some LegacyComplete — fall
    /// to legacy because executing only some of a target's
    /// actions would leave its other declared outputs missing.
    pub fn per_target_buckets(
        &self,
        reg: &ArtifactRegistry,
    ) -> HashMap<TargetId, PerTargetBuckets> {
        let mut by_target: HashMap<TargetId, PerTargetBuckets> = HashMap::new();
        for &(id, verdict) in self.entries.iter() {
            let action = reg.declared_action(id);
            let entry = by_target
                .entry(action.owner_target)
                .or_default();
            match verdict {
                SchedulerVerdict::Ready => entry.ready += 1,
                SchedulerVerdict::LegacyComplete => entry.legacy_complete += 1,
                SchedulerVerdict::LegacyIncomplete => entry.legacy_incomplete += 1,
            }
        }
        by_target
    }

    /// Phase 5.C.4 step 4 prep: targets the scheduler may execute.
    ///
    /// A target is scheduler-eligible iff ALL of its Complete
    /// actions are Ready (have an ExecutableAction payload). A
    /// target with even one LegacyComplete action is ineligible
    /// because the legacy `build()` would re-run the target's
    /// full work, duplicating the scheduler's effort and
    /// overwriting outputs in nondeterministic ways. A target
    /// with LegacyIncomplete actions is also ineligible — the
    /// Incomplete marker is the deferred-to-phase-5.C signal.
    ///
    /// Targets with zero declared actions are NOT included
    /// (nothing to schedule). The orchestrator's PASS B
    /// continues to handle them via legacy `build()` since
    /// build() may still produce artifacts even with zero
    /// declared actions (some metadata-only rules do this).
    pub fn scheduler_eligible_targets(
        &self,
        reg: &ArtifactRegistry,
    ) -> HashSet<TargetId> {
        let buckets = self.per_target_buckets(reg);
        buckets
            .into_iter()
            .filter_map(|(target, b)| {
                let total = b.ready + b.legacy_complete + b.legacy_incomplete;
                if total == 0 {
                    None
                } else if b.legacy_complete == 0 && b.legacy_incomplete == 0 {
                    Some(target)
                } else {
                    None
                }
            })
            .collect()
    }

    /// Phase 5.C.4 dry-run: walk Ready entries and format each
    /// as a one-line "would execute" report by materializing the
    /// `ExecutableAction` through the registry. Returns the
    /// lines so the caller can print, log, or capture for tests
    /// without implying a print policy.
    ///
    /// The lines look like:
    /// ```text
    /// 🧭 scheduler-dry-run: <mnemonic> /usr/bin/ar rcs lib/foo.a obj/foo.o
    /// ```
    ///
    /// Path components are space-joined; quoting is the caller's
    /// problem (the dry-run is for human inspection, not shell
    /// execution). The eventual scheduler hands the materialized
    /// command to ExecutionStrategy without a shell, so this
    /// formatter doesn't need to re-quote either.
    ///
    /// Skips Legacy* entries because their `ExecutableAction` is
    /// either absent (LegacyComplete) or pathologically present
    /// alongside an Incomplete marker (LegacyIncomplete) — neither
    /// is what the scheduler will run.
    pub fn dry_run_ready(&self, reg: &ArtifactRegistry) -> Vec<String> {
        let mut lines = Vec::new();
        for &(id, verdict) in self.entries.iter() {
            if !verdict.is_ready() {
                continue;
            }
            let action = reg.declared_action(id);
            let mnemonic = reg.strings().str(action.mnemonic);
            // Ready by definition means the registry has an
            // ExecutableAction for this id. The unwrap is
            // structurally safe; the alternative would be a
            // panic-free skip-with-warning, but Ready→None is a
            // bug elsewhere not a runtime condition.
            let exec = reg
                .executable_action(id)
                .expect("Ready verdict implies executable_action is set");
            let mat = exec.materialize(reg);
            let argv_joined = mat.argv.join(" ");
            let line = if argv_joined.is_empty() {
                format!(
                    "🧭 scheduler-dry-run: {} {}",
                    mnemonic,
                    mat.executable.display(),
                )
            } else {
                format!(
                    "🧭 scheduler-dry-run: {} {} {}",
                    mnemonic,
                    mat.executable.display(),
                    argv_joined,
                )
            };
            lines.push(line);
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{
        ActionDeclarationStatus, ArtifactKey, ArtifactKind, ConfigId, DeclaredAction,
        ExecutableAction, TargetId,
    };
    use std::path::Path;

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

    fn declare(
        reg: &mut ArtifactRegistry,
        mnemonic: &str,
        inputs: Vec<crate::ids::ArtifactId>,
        outputs: Vec<crate::ids::ArtifactId>,
        status: ActionDeclarationStatus,
    ) -> DeclaredActionId {
        let m = reg.intern_string(mnemonic);
        reg.declare_action(DeclaredAction {
            owner_target: TargetId(1),
            mnemonic: m,
            inputs: inputs.into_boxed_slice(),
            outputs: outputs.into_boxed_slice(),
            status,
        })
        .expect("declare should succeed in test setup")
    }

    #[test]
    fn empty_registry_yields_empty_plan() {
        let reg = ArtifactRegistry::new();
        let dag = ActionDag::build(&reg).unwrap();
        let plan = SchedulerPlan::build(&reg, &dag);
        assert_eq!(plan.total(), 0);
        assert_eq!(plan.ready_count(), 0);
        assert_eq!(plan.legacy_complete_count(), 0);
        assert_eq!(plan.legacy_incomplete_count(), 0);
        assert!(plan.entries.is_empty());
    }

    #[test]
    fn complete_with_executable_is_ready() {
        let mut reg = ArtifactRegistry::new();
        let src = mk_src(&mut reg, "src.cc");
        let obj = mk_obj(&mut reg, "obj.o");
        let id = declare(
            &mut reg,
            "CppCompile",
            vec![src],
            vec![obj],
            ActionDeclarationStatus::Complete,
        );
        let exe = reg.intern_path(Path::new("/usr/bin/g++"));
        reg.set_executable_action(
            id,
            ExecutableAction {
                executable: exe,
                argv: Box::new([]),
                env: Box::new([]),
                working_dir: None,
                env_policy: crate::ids::ActionEnvPolicy::InheritHost,
            },
        );

        let dag = ActionDag::build(&reg).unwrap();
        let plan = SchedulerPlan::build(&reg, &dag);
        assert_eq!(plan.total(), 1);
        assert_eq!(plan.ready_count(), 1);
        assert_eq!(plan.legacy_complete_count(), 0);
        assert_eq!(plan.legacy_incomplete_count(), 0);
        assert_eq!(plan.entries[0], (id, SchedulerVerdict::Ready));
    }

    #[test]
    fn complete_without_executable_is_legacy_complete() {
        let mut reg = ArtifactRegistry::new();
        let src = mk_src(&mut reg, "src.cc");
        let obj = mk_obj(&mut reg, "obj.o");
        let id = declare(
            &mut reg,
            "CppCompile",
            vec![src],
            vec![obj],
            ActionDeclarationStatus::Complete,
        );
        // Note: no set_executable_action call.

        let dag = ActionDag::build(&reg).unwrap();
        let plan = SchedulerPlan::build(&reg, &dag);
        assert_eq!(plan.ready_count(), 0);
        assert_eq!(plan.legacy_complete_count(), 1);
        assert_eq!(plan.legacy_incomplete_count(), 0);
        assert_eq!(plan.entries[0], (id, SchedulerVerdict::LegacyComplete));
    }

    #[test]
    fn incomplete_is_legacy_incomplete_even_with_executable() {
        // Pathological: rule populates ExecutableAction but
        // marks the action Incomplete. Plan should still bucket
        // it as legacy-incomplete — the Incomplete marker wins
        // because phase 5.C0 P4 designed it as the unschedulable
        // signal.
        let mut reg = ArtifactRegistry::new();
        let obj = mk_obj(&mut reg, "obj.o");
        let id = declare(
            &mut reg,
            "BogusLink",
            vec![],
            vec![obj],
            ActionDeclarationStatus::Incomplete {
                reason: "test reason",
            },
        );
        let exe = reg.intern_path(Path::new("/bin/true"));
        reg.set_executable_action(
            id,
            ExecutableAction {
                executable: exe,
                argv: Box::new([]),
                env: Box::new([]),
                working_dir: None,
                env_policy: crate::ids::ActionEnvPolicy::InheritHost,
            },
        );

        let dag = ActionDag::build(&reg).unwrap();
        let plan = SchedulerPlan::build(&reg, &dag);
        assert_eq!(plan.ready_count(), 0);
        assert_eq!(plan.legacy_complete_count(), 0);
        assert_eq!(plan.legacy_incomplete_count(), 1);
        assert_eq!(plan.entries[0], (id, SchedulerVerdict::LegacyIncomplete));
    }

    #[test]
    fn mixed_plan_classifies_each_bucket() {
        // Three actions:
        //   1. Complete with ExecutableAction → Ready
        //   2. Complete without ExecutableAction → LegacyComplete
        //   3. Incomplete → LegacyIncomplete
        // Verify per-bucket counts and that entries are in DAG
        // topological order.
        let mut reg = ArtifactRegistry::new();
        let src = mk_src(&mut reg, "src.cc");
        let o1 = mk_obj(&mut reg, "o1.o");
        let o2 = mk_obj(&mut reg, "o2.o");
        let o3 = mk_obj(&mut reg, "o3.o");

        let a1 = declare(
            &mut reg,
            "Ready",
            vec![src],
            vec![o1],
            ActionDeclarationStatus::Complete,
        );
        let a2 = declare(
            &mut reg,
            "LegacyComplete",
            vec![src],
            vec![o2],
            ActionDeclarationStatus::Complete,
        );
        let a3 = declare(
            &mut reg,
            "LegacyIncomplete",
            vec![src],
            vec![o3],
            ActionDeclarationStatus::Incomplete {
                reason: "deferred",
            },
        );

        let exe = reg.intern_path(Path::new("/bin/true"));
        reg.set_executable_action(
            a1,
            ExecutableAction {
                executable: exe,
                argv: Box::new([]),
                env: Box::new([]),
                working_dir: None,
                env_policy: crate::ids::ActionEnvPolicy::InheritHost,
            },
        );

        let dag = ActionDag::build(&reg).unwrap();
        let plan = SchedulerPlan::build(&reg, &dag);
        assert_eq!(plan.total(), 3);
        assert_eq!(plan.ready_count(), 1);
        assert_eq!(plan.legacy_complete_count(), 1);
        assert_eq!(plan.legacy_incomplete_count(), 1);

        // Entries are in DAG order, which for three independent
        // actions matches registration order. Classification
        // matches the per-action setup.
        let verdicts: Vec<_> = plan.entries.iter().map(|(_, v)| *v).collect();
        assert_eq!(
            verdicts,
            vec![
                SchedulerVerdict::Ready,
                SchedulerVerdict::LegacyComplete,
                SchedulerVerdict::LegacyIncomplete,
            ]
        );

        let ids: Vec<_> = plan.entries.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![a1, a2, a3]);
    }

    #[test]
    fn summary_line_formats_counts() {
        let mut reg = ArtifactRegistry::new();
        let src = mk_src(&mut reg, "src.cc");
        let obj = mk_obj(&mut reg, "obj.o");
        declare(
            &mut reg,
            "X",
            vec![src],
            vec![obj],
            ActionDeclarationStatus::Complete,
        );
        let dag = ActionDag::build(&reg).unwrap();
        let plan = SchedulerPlan::build(&reg, &dag);
        let line = plan.summary_line();
        assert!(line.contains("1 action(s)"));
        assert!(line.contains("0 ready"));
        assert!(line.contains("1 legacy-complete"));
    }

    #[test]
    fn verdict_is_ready_predicate() {
        assert!(SchedulerVerdict::Ready.is_ready());
        assert!(!SchedulerVerdict::LegacyComplete.is_ready());
        assert!(!SchedulerVerdict::LegacyIncomplete.is_ready());
    }

    fn declare_for_target(
        reg: &mut ArtifactRegistry,
        target: u32,
        mnemonic: &str,
        outputs: Vec<crate::ids::ArtifactId>,
        status: ActionDeclarationStatus,
    ) -> DeclaredActionId {
        let m = reg.intern_string(mnemonic);
        reg.declare_action(DeclaredAction {
            owner_target: TargetId(target),
            mnemonic: m,
            inputs: Vec::new().into_boxed_slice(),
            outputs: outputs.into_boxed_slice(),
            status,
        })
        .expect("declare should succeed in test setup")
    }

    #[test]
    fn per_target_buckets_split_actions_by_owner() {
        // Two targets, each with two actions in mixed buckets:
        //   target 1: 1 Ready + 1 LegacyComplete
        //   target 2: 2 Ready
        // per_target_buckets must return separate entries with
        // correct counts.
        let mut reg = ArtifactRegistry::new();
        let o1 = mk_obj(&mut reg, "t1a.o");
        let o2 = mk_obj(&mut reg, "t1b.o");
        let o3 = mk_obj(&mut reg, "t2a.o");
        let o4 = mk_obj(&mut reg, "t2b.o");

        let a1 = declare_for_target(
            &mut reg,
            1,
            "Compile",
            vec![o1],
            ActionDeclarationStatus::Complete,
        );
        let _a2 = declare_for_target(
            &mut reg,
            1,
            "Compile",
            vec![o2],
            ActionDeclarationStatus::Complete,
        );
        let a3 = declare_for_target(
            &mut reg,
            2,
            "Compile",
            vec![o3],
            ActionDeclarationStatus::Complete,
        );
        let a4 = declare_for_target(
            &mut reg,
            2,
            "Compile",
            vec![o4],
            ActionDeclarationStatus::Complete,
        );

        let exe = reg.intern_path(Path::new("/bin/true"));
        for id in [a1, a3, a4] {
            reg.set_executable_action(
                id,
                ExecutableAction {
                    executable: exe,
                    argv: Box::new([]),
                    env: Box::new([]),
                    working_dir: None,
                    env_policy: crate::ids::ActionEnvPolicy::InheritHost,
                },
            );
        }

        let dag = ActionDag::build(&reg).unwrap();
        let plan = SchedulerPlan::build(&reg, &dag);
        let buckets = plan.per_target_buckets(&reg);

        let t1 = buckets.get(&TargetId(1)).copied().unwrap_or_default();
        assert_eq!(t1.ready, 1);
        assert_eq!(t1.legacy_complete, 1);
        assert_eq!(t1.legacy_incomplete, 0);
        assert_eq!(t1.total(), 2);
        assert!(!t1.is_scheduler_eligible());

        let t2 = buckets.get(&TargetId(2)).copied().unwrap_or_default();
        assert_eq!(t2.ready, 2);
        assert_eq!(t2.legacy_complete, 0);
        assert_eq!(t2.legacy_incomplete, 0);
        assert_eq!(t2.total(), 2);
        assert!(t2.is_scheduler_eligible());
    }

    #[test]
    fn scheduler_eligible_targets_filters_mixed() {
        // Target 1: mixed (Ready + Legacy) → ineligible.
        // Target 2: all Ready → eligible.
        // Target 3: has Incomplete → ineligible.
        let mut reg = ArtifactRegistry::new();
        let o1 = mk_obj(&mut reg, "t1.o");
        let o2 = mk_obj(&mut reg, "t1b.o");
        let o3 = mk_obj(&mut reg, "t2.o");
        let o4 = mk_obj(&mut reg, "t3.o");

        // Target 1: one Ready + one LegacyComplete.
        let a1 = declare_for_target(
            &mut reg,
            1,
            "X",
            vec![o1],
            ActionDeclarationStatus::Complete,
        );
        declare_for_target(
            &mut reg,
            1,
            "X",
            vec![o2],
            ActionDeclarationStatus::Complete,
        );
        // Target 2: one Ready (only one action total).
        let a3 = declare_for_target(
            &mut reg,
            2,
            "X",
            vec![o3],
            ActionDeclarationStatus::Complete,
        );
        // Target 3: one Incomplete with ExecutableAction (still
        // ineligible because Incomplete is the unschedulable
        // marker).
        let a4 = declare_for_target(
            &mut reg,
            3,
            "X",
            vec![o4],
            ActionDeclarationStatus::Incomplete {
                reason: "deferred",
            },
        );
        let exe = reg.intern_path(Path::new("/bin/true"));
        for id in [a1, a3, a4] {
            reg.set_executable_action(
                id,
                ExecutableAction {
                    executable: exe,
                    argv: Box::new([]),
                    env: Box::new([]),
                    working_dir: None,
                    env_policy: crate::ids::ActionEnvPolicy::InheritHost,
                },
            );
        }

        let dag = ActionDag::build(&reg).unwrap();
        let plan = SchedulerPlan::build(&reg, &dag);
        let eligible = plan.scheduler_eligible_targets(&reg);

        assert!(!eligible.contains(&TargetId(1))); // mixed
        assert!(eligible.contains(&TargetId(2))); // all Ready
        assert!(!eligible.contains(&TargetId(3))); // Incomplete present
        assert_eq!(eligible.len(), 1);
    }

    #[test]
    fn empty_target_is_not_eligible() {
        // No declared actions → no targets in the buckets map.
        // scheduler_eligible_targets returns empty set.
        let reg = ArtifactRegistry::new();
        let dag = ActionDag::build(&reg).unwrap();
        let plan = SchedulerPlan::build(&reg, &dag);
        assert!(plan.scheduler_eligible_targets(&reg).is_empty());
    }

    #[test]
    fn dry_run_emits_only_ready_entries() {
        // Three actions: one Ready with a meaningful argv, one
        // LegacyComplete, one LegacyIncomplete. Dry-run output
        // must include only the Ready entry's materialized
        // command, naming both the mnemonic and the resolved
        // exe + argv strings.
        let mut reg = ArtifactRegistry::new();
        let src = mk_src(&mut reg, "src.cc");
        let o1 = mk_obj(&mut reg, "o1.o");
        let o2 = mk_obj(&mut reg, "o2.o");
        let o3 = mk_obj(&mut reg, "o3.o");

        let a1 = declare(
            &mut reg,
            "CcArchive",
            vec![src],
            vec![o1],
            ActionDeclarationStatus::Complete,
        );
        let _a2 = declare(
            &mut reg,
            "CppCompileLegacy",
            vec![src],
            vec![o2],
            ActionDeclarationStatus::Complete,
        );
        let _a3 = declare(
            &mut reg,
            "CcLinkSharedDeferred",
            vec![src],
            vec![o3],
            ActionDeclarationStatus::Incomplete {
                reason: "deferred to phase 5.C",
            },
        );

        let exe = reg.intern_path(Path::new("/usr/bin/ar"));
        let arg_rcs = reg.intern_string("rcs");
        let arg_lib = reg.intern_string("lib/foo.a");
        reg.set_executable_action(
            a1,
            ExecutableAction {
                executable: exe,
                argv: Box::new([arg_rcs, arg_lib]),
                env: Box::new([]),
                working_dir: None,
                env_policy: crate::ids::ActionEnvPolicy::InheritHost,
            },
        );

        let dag = ActionDag::build(&reg).unwrap();
        let plan = SchedulerPlan::build(&reg, &dag);
        let lines = plan.dry_run_ready(&reg);
        assert_eq!(lines.len(), 1);
        let line = &lines[0];
        assert!(line.contains("CcArchive"));
        assert!(line.contains("/usr/bin/ar"));
        assert!(line.contains("rcs"));
        assert!(line.contains("lib/foo.a"));
        // Make sure the legacy mnemonics didn't leak in.
        assert!(!line.contains("CppCompileLegacy"));
        assert!(!line.contains("CcLinkSharedDeferred"));
    }

    // Phase 5.C.4 commit 44: legacy_breakdown_by_mnemonic +
    // format_legacy_breakdown tests. Pin sort order
    // (descending by total legacy count, ties broken on
    // mnemonic name), bucket counts (legacy_complete +
    // legacy_incomplete tracked separately), and the
    // format-empty-when-no-legacy contract.

    #[test]
    fn legacy_breakdown_buckets_by_mnemonic_with_correct_counts() {
        let mut reg = ArtifactRegistry::new();
        // Three CppCompile LegacyComplete + one CcArchive
        // LegacyComplete + one CcLink LegacyIncomplete.
        for i in 0..3 {
            let src = mk_src(&mut reg, &format!("c{}.cc", i));
            let obj = mk_obj(&mut reg, &format!("c{}.o", i));
            declare(
                &mut reg,
                "CppCompile",
                vec![src],
                vec![obj],
                ActionDeclarationStatus::Complete,
            );
        }
        let arc_src = mk_src(&mut reg, "arc.cc");
        let arc_o = mk_obj(&mut reg, "arc.o");
        declare(
            &mut reg,
            "CcArchive",
            vec![arc_src],
            vec![arc_o],
            ActionDeclarationStatus::Complete,
        );
        let lk_src = mk_src(&mut reg, "lk.cc");
        let lk_o = mk_obj(&mut reg, "lk.o");
        declare(
            &mut reg,
            "CcLink",
            vec![lk_src],
            vec![lk_o],
            ActionDeclarationStatus::Incomplete {
                reason: "test",
            },
        );
        let dag = ActionDag::build(&reg).unwrap();
        let plan = SchedulerPlan::build(&reg, &dag);
        let breakdown = plan.legacy_breakdown_by_mnemonic(&reg);
        // Three entries, sorted by total legacy count desc.
        assert_eq!(breakdown.len(), 3);
        assert_eq!(breakdown[0], ("CppCompile".to_string(), 3, 0));
        // CcArchive (1 LC) and CcLink (1 LI) tie at 1; tie
        // broken by mnemonic name → CcArchive first.
        assert_eq!(breakdown[1], ("CcArchive".to_string(), 1, 0));
        assert_eq!(breakdown[2], ("CcLink".to_string(), 0, 1));
    }

    #[test]
    fn legacy_breakdown_excludes_ready_entries() {
        // Set up one Ready (LegacyComplete with
        // ExecutableAction populated) + one LegacyComplete.
        // breakdown should report only the legacy one.
        let mut reg = ArtifactRegistry::new();
        let r_src = mk_src(&mut reg, "r.cc");
        let r_obj = mk_obj(&mut reg, "r.o");
        let r_id = declare(
            &mut reg,
            "ReadyMnemonic",
            vec![r_src],
            vec![r_obj],
            ActionDeclarationStatus::Complete,
        );
        let exe = reg.intern_path(Path::new("/usr/bin/ar"));
        reg.set_executable_action(
            r_id,
            ExecutableAction {
                executable: exe,
                argv: Box::new([]),
                env: Box::new([]),
                working_dir: None,
                env_policy: crate::ids::ActionEnvPolicy::InheritHost,
            },
        );
        let lc_src = mk_src(&mut reg, "lc.cc");
        let lc_obj = mk_obj(&mut reg, "lc.o");
        declare(
            &mut reg,
            "LegacyMnemonic",
            vec![lc_src],
            vec![lc_obj],
            ActionDeclarationStatus::Complete,
        );
        let dag = ActionDag::build(&reg).unwrap();
        let plan = SchedulerPlan::build(&reg, &dag);
        let breakdown = plan.legacy_breakdown_by_mnemonic(&reg);
        assert_eq!(breakdown.len(), 1);
        assert_eq!(breakdown[0], ("LegacyMnemonic".to_string(), 1, 0));
    }

    #[test]
    fn legacy_breakdown_returns_empty_when_all_ready() {
        // Single action with ExecutableAction set → Ready.
        // Breakdown is empty.
        let mut reg = ArtifactRegistry::new();
        let src = mk_src(&mut reg, "x.cc");
        let obj = mk_obj(&mut reg, "x.o");
        let id = declare(
            &mut reg,
            "CppCompile",
            vec![src],
            vec![obj],
            ActionDeclarationStatus::Complete,
        );
        let exe = reg.intern_path(Path::new("/usr/bin/g++"));
        reg.set_executable_action(
            id,
            ExecutableAction {
                executable: exe,
                argv: Box::new([]),
                env: Box::new([]),
                working_dir: None,
                env_policy: crate::ids::ActionEnvPolicy::InheritHost,
            },
        );
        let dag = ActionDag::build(&reg).unwrap();
        let plan = SchedulerPlan::build(&reg, &dag);
        let breakdown = plan.legacy_breakdown_by_mnemonic(&reg);
        assert!(breakdown.is_empty());
    }

    #[test]
    fn format_legacy_breakdown_returns_empty_when_breakdown_empty() {
        // Mirror of the empty-breakdown test for the format
        // function. Caller (orchestrator) decides whether to
        // print based on Vec emptiness.
        let mut reg = ArtifactRegistry::new();
        let src = mk_src(&mut reg, "x.cc");
        let obj = mk_obj(&mut reg, "x.o");
        let id = declare(
            &mut reg,
            "CppCompile",
            vec![src],
            vec![obj],
            ActionDeclarationStatus::Complete,
        );
        let exe = reg.intern_path(Path::new("/usr/bin/g++"));
        reg.set_executable_action(
            id,
            ExecutableAction {
                executable: exe,
                argv: Box::new([]),
                env: Box::new([]),
                working_dir: None,
                env_policy: crate::ids::ActionEnvPolicy::InheritHost,
            },
        );
        let dag = ActionDag::build(&reg).unwrap();
        let plan = SchedulerPlan::build(&reg, &dag);
        let lines = plan.format_legacy_breakdown(&reg);
        assert!(lines.is_empty());
    }

    #[test]
    fn legacy_breakdown_report_struct_carries_named_fields() {
        // Build a SchedulerPlan with one CppCompile LC + one
        // CcArchive LI + one Ready entry. Report struct must
        // contain only the legacy entries with named fields,
        // and totals must sum across all entries.
        let mut reg = ArtifactRegistry::new();
        let s1 = mk_src(&mut reg, "a.cc");
        let o1 = mk_obj(&mut reg, "a.o");
        declare(
            &mut reg,
            "CppCompile",
            vec![s1],
            vec![o1],
            ActionDeclarationStatus::Complete,
        );
        let s2 = mk_src(&mut reg, "b.cc");
        let o2 = mk_obj(&mut reg, "b.o");
        declare(
            &mut reg,
            "CcArchive",
            vec![s2],
            vec![o2],
            ActionDeclarationStatus::Incomplete { reason: "test" },
        );
        let s3 = mk_src(&mut reg, "c.cc");
        let o3 = mk_obj(&mut reg, "c.o");
        let id3 = declare(
            &mut reg,
            "Ready",
            vec![s3],
            vec![o3],
            ActionDeclarationStatus::Complete,
        );
        let exe = reg.intern_path(Path::new("/usr/bin/ar"));
        reg.set_executable_action(
            id3,
            ExecutableAction {
                executable: exe,
                argv: Box::new([]),
                env: Box::new([]),
                working_dir: None,
                env_policy: crate::ids::ActionEnvPolicy::InheritHost,
            },
        );
        let dag = ActionDag::build(&reg).unwrap();
        let plan = SchedulerPlan::build(&reg, &dag);
        let report = plan.legacy_breakdown_report(&reg);
        // Phase 5.C.4 commit 54: schema_version pinned.
        assert_eq!(report.schema_version, LEGACY_BREAKDOWN_SCHEMA_VERSION);
        // Two legacy entries (CppCompile + CcArchive), Ready
        // excluded. Sorted by total legacy count desc + name
        // asc tie-break.
        assert_eq!(report.entries.len(), 2);
        assert_eq!(report.entries[0].mnemonic, "CcArchive");
        assert_eq!(report.entries[0].legacy_complete, 0);
        assert_eq!(report.entries[0].legacy_incomplete, 1);
        assert_eq!(report.entries[1].mnemonic, "CppCompile");
        assert_eq!(report.entries[1].legacy_complete, 1);
        assert_eq!(report.entries[1].legacy_incomplete, 0);
        // Totals across all entries.
        assert_eq!(report.total_legacy_complete, 1);
        assert_eq!(report.total_legacy_incomplete, 1);
    }

    #[test]
    fn legacy_breakdown_report_empty_when_all_ready() {
        let mut reg = ArtifactRegistry::new();
        let src = mk_src(&mut reg, "x.cc");
        let obj = mk_obj(&mut reg, "x.o");
        let id = declare(
            &mut reg,
            "CppCompile",
            vec![src],
            vec![obj],
            ActionDeclarationStatus::Complete,
        );
        let exe = reg.intern_path(Path::new("/usr/bin/g++"));
        reg.set_executable_action(
            id,
            ExecutableAction {
                executable: exe,
                argv: Box::new([]),
                env: Box::new([]),
                working_dir: None,
                env_policy: crate::ids::ActionEnvPolicy::InheritHost,
            },
        );
        let dag = ActionDag::build(&reg).unwrap();
        let plan = SchedulerPlan::build(&reg, &dag);
        let report = plan.legacy_breakdown_report(&reg);
        assert!(report.entries.is_empty());
        assert_eq!(report.total_legacy_complete, 0);
        assert_eq!(report.total_legacy_incomplete, 0);
    }

    #[test]
    fn format_legacy_breakdown_includes_header_and_per_mnemonic_lines() {
        let mut reg = ArtifactRegistry::new();
        let src = mk_src(&mut reg, "x.cc");
        let obj = mk_obj(&mut reg, "x.o");
        declare(
            &mut reg,
            "CppCompile",
            vec![src],
            vec![obj],
            ActionDeclarationStatus::Complete,
        );
        let dag = ActionDag::build(&reg).unwrap();
        let plan = SchedulerPlan::build(&reg, &dag);
        let lines = plan.format_legacy_breakdown(&reg);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("Legacy-action breakdown"));
        assert!(lines[0].contains("1 mnemonics"));
        assert!(lines[1].contains("CppCompile"));
        assert!(lines[1].contains("1 legacy-complete"));
        assert!(lines[1].contains("0 legacy-incomplete"));
    }
}
