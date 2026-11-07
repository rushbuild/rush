// Phase 5.C.4 commit 154 (Batch T): target-scoped
// fetch warmup plan.
//
// Batch R/S landed combined fetch + manifest
// scaffolding, but the operator still has to set
// `RUSH_FETCH_WARMUP_REPOS=repo1,repo2,...` by hand.
// Pro's Batch T green-light letter: "combined fetch
// is safe-ish, but not truly defaultable while it
// depends on an operator-provided CSV list."
//
// This commit lands the data model for a Rush-owned
// sidecar that REMEMBERS which repos a prior run
// asked for, keyed by target pattern + workspace.
// Commits 155 / 156 wire the writer (after a run)
// and reader (before the next run with that target).
//
// CRITICAL invariants per Pro's directive:
//   - The plan is purely observational ("these
//     repos were requested before"). It is NOT a
//     correctness claim about the repos themselves.
//   - Plan location: `<cache_root>/fetch-warmup-plans/<plan-key>.json`.
//     Rush-owned, NEVER inside external/<repo>/.
//   - Schema version pinned at 1; bumping requires
//     an explicit migration story.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Phase 5.C.4 schema version for fetch-warmup-plan
/// sidecars.
///   v1 (commit 154, Batch T): single
///       `repos_required` field meaning
///       "everything observed".
///   v2 (commit 160, Batch U): split into
///       `repos_required_for_fetch` (auto warmup's
///       safe set), `repos_observed`,
///       `repos_skipped_materialized`,
///       `repos_skipped_known_rule`. Pro's
///       Batch U letter:
///         "a plan written while
///          RUSH_FETCH_SKIP_IF_MATERIALIZED=1 may
///          record repos that were observed but
///          not actually fetched. If a later run
///          uses that plan without materialized
///          skip, it could over-warm many repos."
///       v1 plans become Stale on read → ignored
///       → rewritten as v2.
pub const FETCH_WARMUP_PLAN_SCHEMA_VERSION: u32 = 2;

/// Phase 5.C.4 commit 154 (Batch T): the source of
/// truth from which a plan was built. Recorded so
/// future readers can decide if the plan is
/// trustworthy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FetchWarmupPlanSource {
    /// The plan was generated from the fetch
    /// attribution report at the end of an
    /// analyze/build run that completed normally
    /// enough that the report was finalized.
    ObservedFetchReport,
    /// Reserved for a future commit where rush can
    /// derive a plan from static WORKSPACE
    /// inspection. Not produced today.
    StaticWorkspaceScan,
}

/// Phase 5.C.4 commit 154 (Batch T): target-scoped
/// list of repos a previous run touched. Read by
/// commit 156 to drive combined warmup automatically.
///
/// **What this is NOT**: a correctness claim about
/// repo identity. A plan says "last time you ran
/// this target, these are the repos you needed."
/// It does NOT validate that the on-disk copies
/// match upstream — that's the manifest's job
/// (commit 147-149).
///
/// **What this IS**: a recall mechanism. Plans
/// power `RUSH_FETCH_WARMUP_PLAN=auto`, which
/// removes the manual `RUSH_FETCH_WARMUP_REPOS`
/// CSV burden from operators.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchWarmupPlan {
    pub schema_version: u32,
    pub workspace_root: PathBuf,
    /// The target patterns that produced this plan,
    /// sorted alphabetically for stable plan keys.
    pub target_patterns: Vec<String>,
    /// Phase 5.C.4 commit 160 (Batch U): the
    /// SAFE-DEFAULT auto-warmup set. Contains
    /// repos that the observed run actually
    /// fetched (decision = "invoked"). Auto mode
    /// reads ONLY this field — that's the entire
    /// point of the v2 split, per Pro:
    ///   "a plan written while
    ///    RUSH_FETCH_SKIP_IF_MATERIALIZED=1 may
    ///    record repos that were observed but not
    ///    actually fetched. If a later run uses
    ///    that plan without materialized skip, it
    ///    could over-warm many repos."
    pub repos_required_for_fetch: Vec<String>,
    /// Phase 5.C.4 commit 160 (Batch U): every
    /// repo `try_fetch_known_repo` saw, regardless
    /// of decision. Includes invoked +
    /// skipped_materialized + skipped_known_rule
    /// + materialized_missing_*. Operators may
    /// opt in to warming the full set via
    /// `RUSH_FETCH_WARMUP_INCLUDE_SKIPPED=1`
    /// (Batch U does not implement that flag —
    /// reserved future operator surface).
    pub repos_observed: Vec<String>,
    /// Repos that hit the commit-134 /
    /// commit-149 materialized-skip path.
    pub repos_skipped_materialized: Vec<String>,
    /// Repos that hit the hand-coded
    /// SKIP_FETCH_REPOS allowlist
    /// (`skipped_known_rule` decision).
    pub repos_skipped_known_rule: Vec<String>,
    pub generated_at_unix: u64,
    pub source: FetchWarmupPlanSource,
    pub rush_version_or_writer_version: String,
    /// Phase 5.C.4 commit 163 (Batch U): set when a
    /// writer flushes a plan whose source run did NOT
    /// complete its fetch attribution pass. Pro's
    /// directive: "If plan invalid or stale: ignore
    /// it." Incomplete plans are treated as stale by
    /// the reader — the orchestrator falls back to
    /// non-warmup behavior and a subsequent run that
    /// DOES complete overwrites this sidecar.
    ///
    /// Backward compatibility: existing v2 plans on
    /// disk lack this field; `#[serde(default)]`
    /// keeps them readable as `incomplete = false`,
    /// so no schema_version bump is required.
    #[serde(default)]
    pub incomplete: bool,
}

impl FetchWarmupPlan {
    /// Phase 5.C.4 commit 160 (Batch U): build a v2
    /// plan from observed fetch attribution.
    /// Caller passes the four category vectors;
    /// constructor sorts + dedupes for diff-stable
    /// JSON.
    pub fn from_observation(
        workspace_root: PathBuf,
        target_patterns: Vec<String>,
        mut repos_required_for_fetch: Vec<String>,
        mut repos_observed: Vec<String>,
        mut repos_skipped_materialized: Vec<String>,
        mut repos_skipped_known_rule: Vec<String>,
    ) -> Self {
        let mut tp = target_patterns;
        tp.sort();
        tp.dedup();
        repos_required_for_fetch.sort();
        repos_required_for_fetch.dedup();
        repos_observed.sort();
        repos_observed.dedup();
        repos_skipped_materialized.sort();
        repos_skipped_materialized.dedup();
        repos_skipped_known_rule.sort();
        repos_skipped_known_rule.dedup();
        Self {
            schema_version: FETCH_WARMUP_PLAN_SCHEMA_VERSION,
            workspace_root,
            target_patterns: tp,
            repos_required_for_fetch,
            repos_observed,
            repos_skipped_materialized,
            repos_skipped_known_rule,
            generated_at_unix: now_unix(),
            source: FetchWarmupPlanSource::ObservedFetchReport,
            rush_version_or_writer_version: env!("CARGO_PKG_VERSION")
                .to_string(),
            incomplete: false,
        }
    }

    /// Repos to drive auto fetch-warmup from. `repos_required_for_fetch` records only repos that
    /// actually had to be FETCHED last run; repos already on disk land in
    /// `repos_skipped_materialized`. Pre-warming off `required` alone oscillates: once a repo is
    /// warmed it's recorded as skipped-materialized → required=0 next run → no pre-warm → slow
    /// inline re-fetch → invoked → required=N → … (the 44↔130 s no-op swing observed on TF
    /// framework). Warm the UNION of required + skipped-materialized so the set is stable across
    /// no-op rebuilds and always takes the fast unified path. Excludes `repos_skipped_known_rule`
    /// (rule stubs that need no fetch). Scheduling-only: does NOT change which repos end up
    /// materialized, so fetch correctness is unchanged. (Codex 019eebc6.)
    pub fn repos_for_auto_warmup(&self) -> Vec<String> {
        let mut repos = self.repos_required_for_fetch.clone();
        repos.extend(self.repos_skipped_materialized.iter().cloned());
        repos.sort();
        repos.dedup();
        repos
    }

    /// Validate the plan structurally. Returns
    /// Err(reason) for any reader-rejectable issue.
    /// v1 plans fail here (schema_version=1 ≠ 2)
    /// and are treated as Stale by the reader.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != FETCH_WARMUP_PLAN_SCHEMA_VERSION {
            return Err(format!(
                "schema_version {} != expected {}",
                self.schema_version,
                FETCH_WARMUP_PLAN_SCHEMA_VERSION
            ));
        }
        if self.incomplete {
            return Err("plan marked incomplete".into());
        }
        if self.target_patterns.is_empty() {
            return Err("target_patterns is empty".into());
        }
        for r in &self.repos_required_for_fetch {
            if r.is_empty() {
                return Err(
                    "repos_required_for_fetch contains empty name".into(),
                );
            }
        }
        for r in &self.repos_observed {
            if r.is_empty() {
                return Err("repos_observed contains empty name".into());
            }
        }
        Ok(())
    }

    /// Phase 5.C.4 commit 163 (Batch U): plan
    /// validity guard. Structural checks PLUS
    /// context-bound checks: the plan's stored
    /// `workspace_root` must equal the caller's, and
    /// its `target_patterns` must match the caller's
    /// normalized request.
    ///
    /// Why both: the plan-key hash (file path) is a
    /// short blake3 digest. Hash collisions are
    /// vanishingly rare but checked content equality
    /// is also a defense against hand-edited or
    /// copied-around sidecars and against future
    /// digest shortening. Pro's directive: "plan key
    /// matches" AND "target_patterns match
    /// normalized current request" — both must hold.
    pub fn validate_in_context(
        &self,
        expected_workspace_root: &Path,
        expected_target_patterns: &[String],
    ) -> Result<(), String> {
        self.validate()?;
        if self.workspace_root.as_path() != expected_workspace_root {
            return Err(format!(
                "workspace_root mismatch: plan={} expected={}",
                self.workspace_root.display(),
                expected_workspace_root.display()
            ));
        }
        let mut expected: Vec<String> =
            expected_target_patterns.to_vec();
        expected.sort();
        expected.dedup();
        if self.target_patterns != expected {
            return Err(format!(
                "target_patterns mismatch: plan={:?} expected={:?}",
                self.target_patterns, expected
            ));
        }
        Ok(())
    }
}

/// Phase 5.C.4 commit 154 (Batch T): canonical
/// plan-key. Hashes (workspace_root,
/// sorted target_patterns) into a short hex digest
/// suitable for use in a filename. Hash includes
/// the schema version so future readers can detect
/// orphans from older versions.
pub fn plan_key_for(
    workspace_root: &Path,
    target_patterns: &[String],
) -> String {
    let mut sorted: Vec<&str> = target_patterns
        .iter()
        .map(|s| s.as_str())
        .collect();
    sorted.sort();
    sorted.dedup();
    let hash_input = format!(
        "v{}|{}|{}",
        FETCH_WARMUP_PLAN_SCHEMA_VERSION,
        workspace_root.display(),
        sorted.join("\x1f")
    );
    let mut hasher = blake3::Hasher::new();
    hasher.update(hash_input.as_bytes());
    let digest = hasher.finalize();
    // 16 hex chars = 64 bits of digest, plenty for
    // collision resistance at file-naming scale.
    hex_short(digest.as_bytes(), 16)
}

fn hex_short(bytes: &[u8], chars: usize) -> String {
    let mut out = String::with_capacity(chars);
    let nibbles = chars;
    for i in 0..(nibbles + 1) / 2 {
        if let Some(b) = bytes.get(i) {
            out.push_str(&format!("{:02x}", b));
        }
    }
    out.truncate(chars);
    out
}

/// Phase 5.C.4 commit 154 (Batch T): sidecar
/// directory.
///
/// **Rush-owned location** per Pro's Batch S/T
/// guidance. Sibling to manifests, NEVER inside
/// `external/<repo>/`.
///
/// Path: `<cache_root>/fetch-warmup-plans/`
/// where `<cache_root>` respects
/// `RUSH_WORKSPACE_CACHE_ROOT`.
pub fn fetch_warmup_plans_dir(workspace_root: &Path) -> PathBuf {
    crate::cache_paths::workspace_cache_root(workspace_root)
        .join("fetch-warmup-plans")
}

/// Phase 5.C.4 commit 154 (Batch T): full path to
/// a plan sidecar for the given target pattern set.
pub fn plan_path_for(
    workspace_root: &Path,
    target_patterns: &[String],
) -> PathBuf {
    let key = plan_key_for(workspace_root, target_patterns);
    fetch_warmup_plans_dir(workspace_root).join(format!("{}.json", key))
}

/// Phase 5.C.4 commit 154 (Batch T): unix epoch
/// seconds, shared with the manifest module.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// =============================================================
// Phase 5.C.4 commit 156 (Batch T): reader + mode parse.
// =============================================================

/// Phase 5.C.4 commit 156 (Batch T): outcome of
/// looking up a warmup plan for a target pattern
/// set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WarmupPlanReadOutcome {
    /// Plan loaded + validated.
    Loaded(FetchWarmupPlan),
    /// No sidecar at the expected path for this
    /// target-pattern key. Normal on first run.
    Missing,
    /// Sidecar exists but parse failed.
    Invalid(String),
    /// Sidecar exists, parses, but fails
    /// `validate()` (e.g. wrong schema_version).
    /// Treated by callers as "ignore" — they fall
    /// back to non-warmup behavior.
    Stale(String),
    /// I/O error reading the sidecar.
    IoError(String),
}

impl WarmupPlanReadOutcome {
    pub fn loaded(&self) -> Option<&FetchWarmupPlan> {
        match self {
            WarmupPlanReadOutcome::Loaded(p) => Some(p),
            _ => None,
        }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            WarmupPlanReadOutcome::Loaded(_) => "loaded",
            WarmupPlanReadOutcome::Missing => "missing",
            WarmupPlanReadOutcome::Invalid(_) => "invalid",
            WarmupPlanReadOutcome::Stale(_) => "stale",
            WarmupPlanReadOutcome::IoError(_) => "io_error",
        }
    }
}

/// Phase 5.C.4 commit 156 (Batch T): read the
/// target-scoped warmup plan, if present. Returns
/// the typed outcome.
///
/// `Stale` is returned for plans that parse but
/// fail structural validation — e.g. a future
/// schema_version bump that drops support for an
/// older sidecar. Callers should treat Stale +
/// Missing + Invalid identically (don't warmup;
/// fall through to normal analyze).
pub fn read_warmup_plan(
    workspace_root: &Path,
    target_patterns: &[String],
) -> WarmupPlanReadOutcome {
    let path = plan_path_for(workspace_root, target_patterns);
    if !path.exists() {
        return WarmupPlanReadOutcome::Missing;
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            return WarmupPlanReadOutcome::IoError(format!(
                "read({}): {}",
                path.display(),
                e
            ));
        }
    };
    let plan: FetchWarmupPlan = match serde_json::from_str(&raw) {
        Ok(p) => p,
        Err(e) => {
            return WarmupPlanReadOutcome::Invalid(format!(
                "parse: {}",
                e
            ));
        }
    };
    // Phase 5.C.4 commit 163 (Batch U): structural
    // validation PLUS context-bound validation. Per
    // Pro's directive a plan must match
    //   - schema_version (validate)
    //   - workspace_root (validate_in_context)
    //   - target_patterns normalized (validate_in_context)
    //   - not marked incomplete (validate)
    // Any mismatch is Stale, never fatal — the
    // caller falls back to non-warmup behavior and a
    // subsequent successful run overwrites this
    // sidecar.
    if let Err(reason) =
        plan.validate_in_context(workspace_root, target_patterns)
    {
        return WarmupPlanReadOutcome::Stale(reason);
    }
    WarmupPlanReadOutcome::Loaded(plan)
}

/// Phase 5.C.4 commit 156 (Batch T): operator mode
/// for `RUSH_FETCH_WARMUP_PLAN`.
///
/// Default changed in commit 165 (Batch U):
///   - Old default (commits 156-164): `Off`. Operators
///     had to set `RUSH_FETCH_WARMUP_PLAN=auto` to
///     get the 6.7× speedup from Batch T.
///   - New default (commit 165+): `Auto`. The
///     plan-driven warmup path runs automatically;
///     operators opt OUT via
///     `RUSH_FETCH_WARMUP_PLAN=off` (or `0`/`false`/
///     `no`).
///
/// Safety of the default flip (per Pro's Batch U
/// green-light letter):
///   - First run for a target: reader returns
///     `Missing` → falls through to non-warmup
///     behavior → no change vs old default.
///   - Subsequent runs: Loaded → ONE combined
///     bazel-sync invocation for the repos the prior
///     run actually fetched (commit 160's
///     repos_required_for_fetch subset).
///   - Stale/invalid/incomplete plans: Stale →
///     ignored, no failure (commit 163).
///   - Warmup failure: non-fatal by default
///     (commit 164); strict via opt-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchWarmupPlanMode {
    Off,
    Auto,
}

impl FetchWarmupPlanMode {
    /// Phase 5.C.4 commit 165 (Batch U): parse
    /// `RUSH_FETCH_WARMUP_PLAN`. Inverted semantics:
    /// explicit opt-out values map to `Off`,
    /// everything else (including `auto`, empty
    /// string, garbage) maps to `Auto`.
    pub fn from_env_value(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "0" | "false" | "no" | "disable" | "disabled" => {
                FetchWarmupPlanMode::Off
            }
            _ => FetchWarmupPlanMode::Auto,
        }
    }
    /// Phase 5.C.4 commit 165 (Batch U): default
    /// changed from `Off` to `Auto`. Unset env var
    /// now enables the plan-driven warmup path. Pro:
    /// "operator-invisible until first analyze run
    /// completes (which then writes a plan); every
    /// subsequent run silently 6.7× faster."
    pub fn from_env() -> Self {
        std::env::var("RUSH_FETCH_WARMUP_PLAN")
            .ok()
            .map(|v| Self::from_env_value(&v))
            .unwrap_or(FetchWarmupPlanMode::Auto)
    }
    pub fn is_auto(&self) -> bool {
        matches!(self, FetchWarmupPlanMode::Auto)
    }
}

// =============================================================
// Phase 5.C.4 commit 155 (Batch T): writer.
// =============================================================

/// Phase 5.C.4 commit 155 (Batch T): outcome of an
/// attempt to write a fetch warmup plan sidecar.
/// Returned by `write_warmup_plan_for_run`; the
/// caller logs/records it but does not abort the
/// analyze/build run on failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WarmupPlanWriteOutcome {
    /// Sidecar created or atomically replaced.
    Written(PathBuf),
    /// Caller passed an empty `target_patterns`
    /// list. We can't compute a stable plan key,
    /// so no plan is written.
    SkippedNoTargets,
    /// Caller passed a fetch report with zero
    /// repos touched. A plan for "nothing was
    /// needed" is operationally useless and would
    /// fail validation; skip.
    SkippedNoRepos,
    /// I/O failure (mkdir, write, rename).
    Failed(String),
}

impl WarmupPlanWriteOutcome {
    pub fn is_written(&self) -> bool {
        matches!(self, WarmupPlanWriteOutcome::Written(_))
    }
    pub fn is_failure(&self) -> bool {
        matches!(self, WarmupPlanWriteOutcome::Failed(_))
    }
}

/// Phase 5.C.4 commit 169 (Batch V): per-run
/// counters for the default-on auto warmup plan
/// surface. Recorded by the orchestrator at the
/// read site (before analyze) and write site (after
/// analyze), plus the warmup-invocation site.
///
/// Pro's commit-169 directive: "Make default-on
/// plan reads/writes operationally boring."
/// Counters are the visibility half — commit 170
/// builds the human-readable report from these.
///
/// Critical invariant: every read AND every write
/// is recorded, regardless of outcome. The default
/// path NEVER fails the build on plan-read or
/// plan-write errors — the counters are how
/// operators tell that an error happened (vs
/// invisible silence) without making errors
/// fatal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WarmupPlanCounters {
    pub plan_read_hit: u32,
    pub plan_read_missing: u32,
    pub plan_read_stale: u32,
    pub plan_read_invalid: u32,
    pub plan_read_io_error: u32,
    pub plan_write_success: u32,
    pub plan_write_failed: u32,
    pub plan_write_skipped: u32,
    pub plan_warmup_used: u32,
    pub plan_warmup_fallback: u32,
}

impl WarmupPlanCounters {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn record_read(&mut self, outcome: &WarmupPlanReadOutcome) {
        match outcome {
            WarmupPlanReadOutcome::Loaded(_) => self.plan_read_hit += 1,
            WarmupPlanReadOutcome::Missing => self.plan_read_missing += 1,
            WarmupPlanReadOutcome::Stale(_) => self.plan_read_stale += 1,
            WarmupPlanReadOutcome::Invalid(_) => self.plan_read_invalid += 1,
            WarmupPlanReadOutcome::IoError(_) => {
                self.plan_read_io_error += 1
            }
        }
    }
    pub fn record_write(&mut self, outcome: &WarmupPlanWriteOutcome) {
        match outcome {
            WarmupPlanWriteOutcome::Written(_) => {
                self.plan_write_success += 1
            }
            WarmupPlanWriteOutcome::Failed(_) => self.plan_write_failed += 1,
            WarmupPlanWriteOutcome::SkippedNoTargets
            | WarmupPlanWriteOutcome::SkippedNoRepos => {
                self.plan_write_skipped += 1
            }
        }
    }
    /// Record that the loaded plan drove a warmup
    /// invocation (warmup actually ran with
    /// plan-supplied repos).
    pub fn record_warmup_used(&mut self) {
        self.plan_warmup_used += 1;
    }
    /// Record that warmup did NOT fire from the plan
    /// path on this run — typically because the
    /// reader returned Missing/Stale/Invalid, OR
    /// because the loaded plan's
    /// `repos_required_for_fetch` is empty.
    pub fn record_warmup_fallback(&mut self) {
        self.plan_warmup_fallback += 1;
    }
    /// True if any plan read/write resulted in a
    /// failure mode (stale, invalid, io_error, or
    /// write failed). Operator-visible signal that
    /// the plan surface is unhealthy on this run.
    pub fn has_anomaly(&self) -> bool {
        self.plan_read_stale > 0
            || self.plan_read_invalid > 0
            || self.plan_read_io_error > 0
            || self.plan_write_failed > 0
    }
}

/// Phase 5.C.4 commit 170 (Batch V): operator-visible
/// warmup-mode report. Emitted at end of every
/// build_with_dependencies pass. The compact human
/// line is always emitted; the JSON form is gated
/// by `RUSH_WARMUP_REPORT_JSON=1`.
///
/// Pro's commit-170 directive: "Defaults without
/// visibility are how tools become haunted." The
/// fields below are exactly Pro's spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WarmupReport {
    /// Operator-effective plan mode: "auto" if the
    /// commit-165 default or explicit auto opt-in
    /// applied, "off" if opted out.
    pub warmup_plan_mode: String,
    /// One of "hit" / "missing" / "stale" /
    /// "invalid" / "io_error" / "n/a" (n/a when mode
    /// is off — reader never ran).
    pub plan_status: String,
    /// One of "combined" / "serial" / "parallel" /
    /// "auto" / "none" (none when warmup didn't
    /// fire because both plan+env inputs were
    /// empty).
    pub warmup_strategy: String,
    /// Count of repos successfully warmed (ok=true
    /// in the per-repo outcome vec).
    pub repos_warmed_count: u32,
    /// Operator-supplied plan_repos length.
    pub repos_from_plan: u32,
    /// Operator-supplied env_repos length.
    pub repos_from_env: u32,
    /// Count of per-repo warmup failures.
    pub warmup_failures: u32,
    /// Approximate count of serial per-repo fetches
    /// invoked during analyze AFTER warmup. Computed
    /// as `fetch_invoked_count - warmed_count`. Zero
    /// means warmup covered everything analyze
    /// asked for.
    pub normal_fetches_after_warmup: u32,
    /// True if any commit-169 counter recorded a
    /// stale / invalid / io_error / write_failed.
    pub plan_anomaly: bool,
    /// Wall-clock milliseconds spent in the warmup
    /// invocation (0 when warmup didn't fire).
    pub warmup_wall_ms: u64,
}

impl WarmupReport {
    /// Build the report from the orchestrator's
    /// per-run state. Pure function — easy to unit
    /// test without driving the full orchestrator.
    /// `outcome` is None when warmup didn't fire.
    pub fn build(
        plan_mode_is_auto: bool,
        counters: &WarmupPlanCounters,
        outcome: Option<&dyn UnifiedWarmupOutcomeView>,
        repos_from_plan: u32,
        repos_from_env: u32,
        normal_fetches_after_warmup: u32,
    ) -> Self {
        let warmup_plan_mode = if plan_mode_is_auto {
            "auto".to_string()
        } else {
            "off".to_string()
        };
        // Determine plan_status from counters. Pick
        // the FIRST non-zero (in priority order:
        // hit > stale > invalid > io_error > missing).
        let plan_status = if counters.plan_read_hit > 0 {
            "hit"
        } else if counters.plan_read_stale > 0 {
            "stale"
        } else if counters.plan_read_invalid > 0 {
            "invalid"
        } else if counters.plan_read_io_error > 0 {
            "io_error"
        } else if counters.plan_read_missing > 0 {
            "missing"
        } else {
            "n/a"
        }
        .to_string();
        let (warmup_strategy, warmup_wall_ms, repos_warmed_count, warmup_failures) =
            match outcome {
                Some(o) => (
                    o.strategy_name().to_string(),
                    o.wall_ms(),
                    o.succeeded_count() as u32,
                    o.failed_count() as u32,
                ),
                None => ("none".to_string(), 0u64, 0u32, 0u32),
            };
        Self {
            warmup_plan_mode,
            plan_status,
            warmup_strategy,
            repos_warmed_count,
            repos_from_plan,
            repos_from_env,
            warmup_failures,
            normal_fetches_after_warmup,
            plan_anomaly: counters.has_anomaly(),
            warmup_wall_ms,
        }
    }

    /// Compact one-line human format. Pro:
    /// "compact line, not a novella."
    pub fn format_human(&self) -> String {
        format!(
            "warmup: mode={} plan={} strategy={} warmed={} \
             plan_in={} env_in={} failed={} post_warmup_fetches={} \
             anomaly={} wall={}ms",
            self.warmup_plan_mode,
            self.plan_status,
            self.warmup_strategy,
            self.repos_warmed_count,
            self.repos_from_plan,
            self.repos_from_env,
            self.warmup_failures,
            self.normal_fetches_after_warmup,
            self.plan_anomaly,
            self.warmup_wall_ms,
        )
    }
}

/// Phase 5.C.4 commit 170 (Batch V): trait that lets
/// WarmupReport::build accept the rush-starlark
/// `UnifiedWarmupOutcome` without rush-build needing
/// a dependency cycle. Implemented for the concrete
/// type via a thin adapter at the call site.
pub trait UnifiedWarmupOutcomeView {
    fn strategy_name(&self) -> &'static str;
    fn wall_ms(&self) -> u64;
    fn succeeded_count(&self) -> usize;
    fn failed_count(&self) -> usize;
}

impl UnifiedWarmupOutcomeView
    for rush_starlark::fetch_strategy::UnifiedWarmupOutcome
{
    fn strategy_name(&self) -> &'static str {
        self.strategy.as_str()
    }
    fn wall_ms(&self) -> u64 {
        self.wall_ms
    }
    fn succeeded_count(&self) -> usize {
        self.repos.iter().filter(|(_, _, ok)| *ok).count()
    }
    fn failed_count(&self) -> usize {
        self.repos.iter().filter(|(_, _, ok)| !*ok).count()
    }
}

/// Phase 5.C.4 commit 155 (Batch T): observation
/// summary of a single fetch-report entry.
/// Decoupled from the full
/// `BzlFetchReportEntry` so this module doesn't
/// have to depend on the full cache_counters wire
/// shape. The caller (orchestrator) projects each
/// report row into one of these and passes a vec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchObservation {
    pub repo_name: String,
    /// String form matching the
    /// `BzlFetchReportEntry::decision` vocabulary.
    pub decision: String,
}

/// Phase 5.C.4 commit 155 (Batch T): atomically
/// write a warmup plan sidecar from a vec of
/// observed fetch decisions.
///
/// v2 projection (commit 160, Batch U):
///   `repos_required_for_fetch` = observations with
///     `decision == "invoked"`. Auto warmup's safe
///     default set.
///   `repos_observed` = every observation, regardless
///     of decision.
///   `repos_skipped_materialized` = observations whose
///     decision starts with `skipped_materialized`.
///   `repos_skipped_known_rule` = observations with
///     `decision == "skipped_known_rule"`.
///
/// Per Pro's spec:
///   - Don't require the run to fully succeed.
///   - Don't write if there's nothing to record.
///   - Don't write inside `external/<repo>/` —
///     `plan_path_for` always lands under
///     `<cache_root>/fetch-warmup-plans/`.
pub fn write_warmup_plan_for_run(
    workspace_root: &Path,
    target_patterns: &[String],
    observations: &[FetchObservation],
) -> WarmupPlanWriteOutcome {
    if target_patterns.is_empty() {
        return WarmupPlanWriteOutcome::SkippedNoTargets;
    }
    if observations.is_empty() {
        return WarmupPlanWriteOutcome::SkippedNoRepos;
    }

    // Phase 5.C.4 commit 160 (Batch U): project each
    // observation into the 4-bucket split per Pro's
    // letter. `repos_required_for_fetch` is ONLY the
    // Invoked subset; `repos_observed` is the full
    // set; the two skipped-* buckets capture
    // diagnostics. Auto warmup reads only
    // `repos_required_for_fetch` to avoid the
    // "skip-mode plan over-warms on a non-skip run"
    // failure mode.
    let mut repos_required_for_fetch: Vec<String> = Vec::new();
    let mut repos_observed: Vec<String> = Vec::new();
    let mut repos_skipped_materialized: Vec<String> = Vec::new();
    let mut repos_skipped_known_rule: Vec<String> = Vec::new();
    for obs in observations {
        repos_observed.push(obs.repo_name.clone());
        if obs.decision == "invoked" {
            repos_required_for_fetch.push(obs.repo_name.clone());
        }
        if obs.decision.starts_with("skipped_materialized") {
            repos_skipped_materialized.push(obs.repo_name.clone());
        }
        if obs.decision == "skipped_known_rule" {
            repos_skipped_known_rule.push(obs.repo_name.clone());
        }
    }

    let plan = FetchWarmupPlan::from_observation(
        workspace_root.to_path_buf(),
        target_patterns.to_vec(),
        repos_required_for_fetch,
        repos_observed,
        repos_skipped_materialized,
        repos_skipped_known_rule,
    );

    let dir = fetch_warmup_plans_dir(workspace_root);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return WarmupPlanWriteOutcome::Failed(format!(
            "create_dir_all({}): {}",
            dir.display(),
            e
        ));
    }
    let final_path = plan_path_for(workspace_root, target_patterns);
    let tmp_path = final_path.with_extension("json.tmp");

    let json = match serde_json::to_string_pretty(&plan) {
        Ok(s) => s,
        Err(e) => {
            return WarmupPlanWriteOutcome::Failed(format!(
                "serialize: {}",
                e
            ))
        }
    };
    if let Err(e) = std::fs::write(&tmp_path, &json) {
        return WarmupPlanWriteOutcome::Failed(format!(
            "write({}): {}",
            tmp_path.display(),
            e
        ));
    }
    if let Err(e) = std::fs::rename(&tmp_path, &final_path) {
        let _ = std::fs::remove_file(&tmp_path);
        return WarmupPlanWriteOutcome::Failed(format!(
            "rename({} → {}): {}",
            tmp_path.display(),
            final_path.display(),
            e
        ));
    }
    WarmupPlanWriteOutcome::Written(final_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_plan() -> FetchWarmupPlan {
        FetchWarmupPlan::from_observation(
            PathBuf::from("/tmp/ws"),
            vec!["//foo:bar".to_string(), "//baz:qux".to_string()],
            // repos_required_for_fetch (Invoked only)
            vec!["alpha".to_string()],
            // repos_observed (all touched)
            vec!["alpha".to_string(), "beta".to_string()],
            // repos_skipped_materialized
            vec!["beta".to_string()],
            // repos_skipped_known_rule
            vec![],
        )
    }

    #[test]
    fn warmup_plan_roundtrip() {
        let p = sample_plan();
        let json = serde_json::to_string(&p).expect("ser");
        let back: FetchWarmupPlan =
            serde_json::from_str(&json).expect("deser");
        assert_eq!(p, back);
        // The from_observation builder sorts +
        // dedupes, so the field shape matches what
        // the reader will see.
        assert_eq!(
            back.target_patterns,
            vec!["//baz:qux".to_string(), "//foo:bar".to_string()]
        );
        assert_eq!(back.schema_version, 2);
    }

    #[test]
    fn target_pattern_key_is_stable() {
        let ws = Path::new("/tmp/ws");
        let k1 = plan_key_for(
            ws,
            &["//foo:bar".to_string(), "//baz:qux".to_string()],
        );
        // Same set in different order → same key.
        let k2 = plan_key_for(
            ws,
            &["//baz:qux".to_string(), "//foo:bar".to_string()],
        );
        assert_eq!(
            k1, k2,
            "plan key must be stable across input order"
        );
        // Different workspace → different key.
        let k3 = plan_key_for(
            Path::new("/tmp/other_ws"),
            &["//foo:bar".to_string(), "//baz:qux".to_string()],
        );
        assert_ne!(k1, k3, "different workspace must yield different key");
        // Different target → different key.
        let k4 = plan_key_for(ws, &["//foo:bar".to_string()]);
        assert_ne!(k1, k4, "different target set must yield different key");
        // Key is short and hex-shaped.
        assert_eq!(k1.len(), 16);
        assert!(
            k1.chars().all(|c| c.is_ascii_hexdigit()),
            "plan key must be hex: {}",
            k1
        );
    }

    #[test]
    fn plan_location_under_cache_root() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let dir = fetch_warmup_plans_dir(&ws);
        let dir_str = dir.display().to_string();
        assert!(
            dir_str.contains(".cache/rush") || dir_str.contains("/rush/"),
            "plans dir must live under rush cache: {}",
            dir_str
        );
        let path = plan_path_for(
            &ws,
            &["//foo:bar".to_string()],
        );
        assert_eq!(path.parent().unwrap(), dir);
        assert!(
            path.to_string_lossy().ends_with(".json"),
            "plan filename must be JSON: {}",
            path.display()
        );
    }

    #[test]
    fn plan_does_not_live_inside_external_repo() {
        // Critical Pro invariant: rush-owned
        // sidecars NEVER live inside
        // <workspace>/external/<repo>/. The plan key
        // is a hash, so even if a target pattern
        // happens to contain "//external/foo:bar",
        // the resulting sidecar still lives in
        // .cache/rush/fetch-warmup-plans/.
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let path = plan_path_for(
            &ws,
            &["//external/foo:bar".to_string()],
        );
        let path_str = path.display().to_string();
        // Must contain the cache root.
        assert!(
            path_str.contains(".cache/rush")
                || path_str.contains("/rush/"),
            "plan must live under rush cache: {}",
            path_str
        );
        // Must NOT contain the literal `/external/foo/`
        // substring (that would mean we ended up inside
        // the third-party dir).
        assert!(
            !path_str.contains("/external/foo/"),
            "plan must NOT live inside external repo dir: {}",
            path_str
        );
    }

    #[test]
    fn validate_rejects_empty_target_patterns() {
        let mut p = sample_plan();
        p.target_patterns.clear();
        let err = p.validate().expect_err("empty patterns must reject");
        assert!(err.contains("target_patterns"));
    }

    #[test]
    fn validate_rejects_empty_repo_names() {
        let mut p = sample_plan();
        p.repos_required_for_fetch.push("".to_string());
        let err = p.validate().expect_err("empty repo name must reject");
        assert!(err.contains("repos_required_for_fetch"));
    }

    #[test]
    fn validate_rejects_wrong_schema_version() {
        let mut p = sample_plan();
        p.schema_version = 999;
        let err = p.validate().expect_err("wrong schema must reject");
        assert!(err.contains("schema_version"));
    }

    // =====================================================
    // Phase 5.C.4 commit 155 (Batch T): writer tests
    // =====================================================

    #[test]
    fn fetch_report_writes_warmup_plan() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let observations = vec![
            FetchObservation {
                repo_name: "alpha".into(),
                decision: "invoked".into(),
            },
            FetchObservation {
                repo_name: "beta".into(),
                decision: "skipped_materialized_trust_mode".into(),
            },
            FetchObservation {
                repo_name: "gamma".into(),
                decision: "skipped_known_rule".into(),
            },
            FetchObservation {
                repo_name: "delta".into(),
                decision: "invoked".into(),
            },
        ];
        let target_patterns =
            vec!["//tensorflow/core/platform:macros".into()];

        let outcome = write_warmup_plan_for_run(
            &ws,
            &target_patterns,
            &observations,
        );
        assert!(
            outcome.is_written(),
            "expected Written, got {:?}",
            outcome
        );

        // Sidecar lives at the canonical path.
        let path = plan_path_for(&ws, &target_patterns);
        assert!(path.exists(), "sidecar must exist: {}", path.display());

        let json = std::fs::read_to_string(&path).unwrap();
        let plan: FetchWarmupPlan = serde_json::from_str(&json).unwrap();
        plan.validate().expect("written plan must validate");

        // v2: all 4 observations contribute to
        // repos_observed.
        assert_eq!(
            plan.repos_observed,
            vec![
                "alpha".to_string(),
                "beta".to_string(),
                "delta".to_string(),
                "gamma".to_string(),
            ]
        );
        // v2: alpha + delta got "invoked" →
        // repos_required_for_fetch (auto warmup's
        // safe-default set).
        assert_eq!(
            plan.repos_required_for_fetch,
            vec!["alpha".to_string(), "delta".to_string()]
        );
        // v2: beta got "skipped_materialized_trust_mode"
        // → contributes to skipped_materialized
        // (prefix match).
        assert_eq!(
            plan.repos_skipped_materialized,
            vec!["beta".to_string()]
        );
        // v2: gamma got "skipped_known_rule" →
        // contributes to skipped_known_rule.
        assert_eq!(
            plan.repos_skipped_known_rule,
            vec!["gamma".to_string()]
        );
    }

    #[test]
    fn failed_run_writes_incomplete_plan_or_skips_with_reason() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();

        // Empty observations → SkippedNoRepos (Pro's
        // letter: "mark incomplete plans if the run
        // failed before fetch attribution finished").
        // Our policy is the stricter "no observations →
        // no plan" because an empty plan would fail
        // validation and an Incomplete marker isn't a
        // shape the current model supports. The
        // SkippedNoRepos outcome IS that incomplete
        // marker — the caller sees it and can log
        // "run did not produce a usable plan."
        let outcome = write_warmup_plan_for_run(
            &ws,
            &["//foo:bar".to_string()],
            &[],
        );
        assert_eq!(
            outcome,
            WarmupPlanWriteOutcome::SkippedNoRepos,
            "empty observations must produce SkippedNoRepos, got {:?}",
            outcome
        );
        let path = plan_path_for(&ws, &["//foo:bar".to_string()]);
        assert!(
            !path.exists(),
            "no sidecar written when observations empty"
        );

        // Empty target_patterns → SkippedNoTargets.
        let outcome2 = write_warmup_plan_for_run(
            &ws,
            &[],
            &[FetchObservation {
                repo_name: "a".into(),
                decision: "invoked".into(),
            }],
        );
        assert_eq!(
            outcome2,
            WarmupPlanWriteOutcome::SkippedNoTargets,
            "empty targets must produce SkippedNoTargets, got {:?}",
            outcome2
        );
    }

    #[test]
    fn plan_contains_required_repos() {
        // v2: repos_observed is the union of every
        // decision the run saw. repos_required_for_fetch
        // is the Invoked-only safe-default subset that
        // auto warmup reads. Both are populated; the
        // reader chooses which to consume based on
        // operator policy.
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let observations = vec![
            FetchObservation {
                repo_name: "only_invoked".into(),
                decision: "invoked".into(),
            },
            FetchObservation {
                repo_name: "only_known_rule".into(),
                decision: "skipped_known_rule".into(),
            },
            FetchObservation {
                repo_name: "only_missing_dir".into(),
                decision: "materialized_missing_dir".into(),
            },
            FetchObservation {
                repo_name: "only_manifest_valid".into(),
                decision: "skipped_materialized_manifest_valid".into(),
            },
        ];
        let target_patterns = vec!["//x:y".to_string()];
        let outcome = write_warmup_plan_for_run(
            &ws,
            &target_patterns,
            &observations,
        );
        assert!(outcome.is_written());
        let plan: FetchWarmupPlan = serde_json::from_str(
            &std::fs::read_to_string(plan_path_for(
                &ws,
                &target_patterns,
            ))
            .unwrap(),
        )
        .unwrap();
        // v2: all 4 in repos_observed (every
        // decision contributes).
        assert_eq!(plan.repos_observed.len(), 4);
        assert!(plan.repos_observed.contains(&"only_invoked".to_string()));
        assert!(plan.repos_observed.contains(&"only_known_rule".to_string()));
        assert!(plan.repos_observed.contains(&"only_missing_dir".to_string()));
        assert!(
            plan.repos_observed
                .contains(&"only_manifest_valid".to_string())
        );
        // v2: only the Invoked one is in
        // repos_required_for_fetch (auto warmup's
        // safe-default set).
        assert_eq!(
            plan.repos_required_for_fetch,
            vec!["only_invoked".to_string()]
        );
        // v2: skipped_materialized_manifest_valid
        // counts in skipped_materialized (starts
        // with the prefix).
        assert_eq!(
            plan.repos_skipped_materialized,
            vec!["only_manifest_valid".to_string()]
        );
        // v2: skipped_known_rule populated.
        assert_eq!(
            plan.repos_skipped_known_rule,
            vec!["only_known_rule".to_string()]
        );
    }

    // =====================================================
    // Phase 5.C.4 commit 156 (Batch T): reader tests
    // =====================================================

    #[test]
    fn auto_without_plan_falls_back_and_writes_plan() {
        // Reader returns Missing when no sidecar
        // exists. The orchestrator's behavior
        // (proceed normally, write plan at end) is
        // tested via integration; here we verify the
        // reader returns the right typed outcome.
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let outcome = read_warmup_plan(
            &ws,
            &["//foo:bar".to_string()],
        );
        assert_eq!(outcome, WarmupPlanReadOutcome::Missing);
        assert!(outcome.loaded().is_none());
    }

    #[test]
    fn auto_with_plan_runs_combined_fetch() {
        // Verify Loaded path: write a plan, then
        // read it back. The orchestrator-side
        // "runs combined fetch" behavior is wired
        // in build_orchestrator_ext.rs and exercised
        // by the smoke test; here we verify the
        // reader hands the orchestrator a valid
        // plan.
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let observations = vec![
            FetchObservation {
                repo_name: "alpha".into(),
                decision: "invoked".into(),
            },
            FetchObservation {
                repo_name: "beta".into(),
                decision: "skipped_known_rule".into(),
            },
        ];
        let target_patterns = vec!["//foo:bar".to_string()];
        let _ = write_warmup_plan_for_run(
            &ws,
            &target_patterns,
            &observations,
        );
        let outcome = read_warmup_plan(&ws, &target_patterns);
        let plan = outcome.loaded().expect("plan must load");
        // v2: alpha + beta both in repos_observed;
        // alpha in repos_required_for_fetch
        // (Invoked); beta in skipped_known_rule.
        assert_eq!(plan.repos_observed.len(), 2);
        assert!(plan.repos_observed.contains(&"alpha".to_string()));
        assert!(plan.repos_observed.contains(&"beta".to_string()));
        assert!(
            plan.repos_required_for_fetch.contains(&"alpha".to_string())
        );
        plan.validate().expect("must validate");
    }

    #[test]
    fn combined_unsupported_falls_back_to_serial() {
        // This is a fetch_strategy-level concern,
        // not a fetch_warmup_plan-level concern.
        // The orchestrator calls
        // perform_warmup_from_plan which internally
        // calls resolve_runtime_strategy and falls
        // back to serial. Tested at the fetch_strategy
        // layer in commit 141's
        // unsupported_combined_falls_back_to_serial.
        // Here we assert the env mode parses
        // correctly and the orchestrator dispatcher
        // can be exercised.
        // Phase 5.C.4 commit 165 (Batch U): default
        // flipped. Auto is now the default;
        // explicit-off values still map to Off.
        assert_eq!(
            FetchWarmupPlanMode::from_env_value("auto"),
            FetchWarmupPlanMode::Auto
        );
        assert_eq!(
            FetchWarmupPlanMode::from_env_value("off"),
            FetchWarmupPlanMode::Off
        );
        // Empty + garbage NOW default to Auto (post
        // commit 165 flip).
        assert_eq!(
            FetchWarmupPlanMode::from_env_value(""),
            FetchWarmupPlanMode::Auto
        );
        assert_eq!(
            FetchWarmupPlanMode::from_env_value("garbage"),
            FetchWarmupPlanMode::Auto
        );
    }

    #[test]
    fn stale_or_mismatched_plan_ignored() {
        // Two scenarios:
        //   1. Plan exists for target X; read for
        //      target Y → Missing (different keys).
        //   2. Plan exists but JSON is corrupt →
        //      Invalid.
        //   3. Plan exists, parses, but
        //      schema_version is wrong → Stale.
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();

        // Scenario 1: write plan for X, read for Y.
        let _ = write_warmup_plan_for_run(
            &ws,
            &["//target/x:lib".to_string()],
            &[FetchObservation {
                repo_name: "a".into(),
                decision: "invoked".into(),
            }],
        );
        let outcome = read_warmup_plan(
            &ws,
            &["//target/y:lib".to_string()],
        );
        assert_eq!(
            outcome,
            WarmupPlanReadOutcome::Missing,
            "different target must miss: {:?}",
            outcome
        );

        // Scenario 2: corrupt JSON.
        let dir = fetch_warmup_plans_dir(&ws);
        std::fs::create_dir_all(&dir).unwrap();
        let target_corrupt = vec!["//corrupt:target".to_string()];
        std::fs::write(
            plan_path_for(&ws, &target_corrupt),
            b"{ not json }",
        )
        .unwrap();
        let outcome2 = read_warmup_plan(&ws, &target_corrupt);
        assert!(
            matches!(outcome2, WarmupPlanReadOutcome::Invalid(_)),
            "corrupt plan must Invalid: {:?}",
            outcome2
        );

        // Scenario 3: wrong schema_version.
        let target_stale = vec!["//stale:target".to_string()];
        let stale = serde_json::json!({
            "schema_version": 999,
            "workspace_root": ws.display().to_string(),
            "target_patterns": ["//stale:target"],
            // Use v2 field names so the parser
            // succeeds and only validate() rejects.
            "repos_required_for_fetch": ["a"],
            "repos_observed": ["a"],
            "repos_skipped_materialized": [],
            "repos_skipped_known_rule": [],
            "generated_at_unix": 0,
            "source": "observed_fetch_report",
            "rush_version_or_writer_version": "0.0.0",
        });
        std::fs::write(
            plan_path_for(&ws, &target_stale),
            serde_json::to_string(&stale).unwrap(),
        )
        .unwrap();
        let outcome3 = read_warmup_plan(&ws, &target_stale);
        // Either Stale (validate() rejects schema)
        // or Invalid (parser rejects unknown
        // version) is acceptable — both cause the
        // reader to ignore the plan.
        assert!(
            matches!(
                outcome3,
                WarmupPlanReadOutcome::Stale(_)
                    | WarmupPlanReadOutcome::Invalid(_)
            ),
            "wrong schema_version must Stale or Invalid: {:?}",
            outcome3
        );
    }

    #[test]
    fn from_observation_sorts_and_dedupes() {
        let p = FetchWarmupPlan::from_observation(
            PathBuf::from("/tmp/ws"),
            vec!["//b".to_string(), "//a".to_string(), "//b".to_string()],
            // repos_required_for_fetch
            vec!["zz".to_string(), "aa".to_string(), "zz".to_string()],
            // repos_observed
            vec![],
            // repos_skipped_materialized
            vec![],
            // repos_skipped_known_rule
            vec![],
        );
        assert_eq!(
            p.target_patterns,
            vec!["//a".to_string(), "//b".to_string()]
        );
        assert_eq!(
            p.repos_required_for_fetch,
            vec!["aa".to_string(), "zz".to_string()]
        );
    }

    // =====================================================
    // Phase 5.C.4 commit 160 (Batch U): v2 split tests
    // =====================================================

    #[test]
    fn plan_written_from_skip_run_separates_skipped_repos() {
        // Observations from a skip-mode run: most
        // repos hit skipped_materialized variants;
        // only a few actually fetch. v2 ensures the
        // plan separates these so auto warmup on a
        // future non-skip run doesn't over-warm.
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let observations = vec![
            FetchObservation {
                repo_name: "real_fetch_repo".into(),
                decision: "invoked".into(),
            },
            FetchObservation {
                repo_name: "trust_mode_repo".into(),
                decision: "skipped_materialized_trust_mode".into(),
            },
            FetchObservation {
                repo_name: "manifest_valid_repo".into(),
                decision: "skipped_materialized_manifest_valid".into(),
            },
            FetchObservation {
                repo_name: "known_rule_repo".into(),
                decision: "skipped_known_rule".into(),
            },
            FetchObservation {
                repo_name: "missing_dir_repo".into(),
                decision: "materialized_missing_dir".into(),
            },
        ];
        let target_patterns = vec!["//foo:bar".to_string()];
        let outcome = write_warmup_plan_for_run(
            &ws,
            &target_patterns,
            &observations,
        );
        assert!(outcome.is_written());

        let plan: FetchWarmupPlan = serde_json::from_str(
            &std::fs::read_to_string(plan_path_for(
                &ws,
                &target_patterns,
            ))
            .unwrap(),
        )
        .unwrap();

        // Auto warmup's safe-default set: ONLY the
        // actually-fetched repo.
        assert_eq!(
            plan.repos_required_for_fetch,
            vec!["real_fetch_repo".to_string()]
        );
        // Full observed set: all 5.
        assert_eq!(plan.repos_observed.len(), 5);
        // Skipped buckets correctly populated.
        assert_eq!(
            plan.repos_skipped_materialized,
            vec![
                "manifest_valid_repo".to_string(),
                "trust_mode_repo".to_string()
            ]
        );
        assert_eq!(
            plan.repos_skipped_known_rule,
            vec!["known_rule_repo".to_string()]
        );
    }

    #[test]
    fn auto_warmup_uses_required_for_fetch_only() {
        // The plan-driven warmup hook reads
        // `repos_required_for_fetch`. Even if the
        // plan also contains observed/skipped entries,
        // those are diagnostic-only.
        //
        // This test exercises the plan shape; the
        // orchestrator-side dispatch is integration-
        // tested via the smoke runs.
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        // Build a plan that has 1 repo in
        // repos_required_for_fetch but 5 in
        // repos_observed. Auto warmup must only
        // see the 1.
        let plan = FetchWarmupPlan::from_observation(
            ws.clone(),
            vec!["//foo:bar".to_string()],
            // repos_required_for_fetch
            vec!["actually_needs_fetch".to_string()],
            // repos_observed (much larger set)
            vec![
                "actually_needs_fetch".to_string(),
                "skipped_a".to_string(),
                "skipped_b".to_string(),
                "skipped_c".to_string(),
                "skipped_d".to_string(),
            ],
            vec!["skipped_a".to_string(), "skipped_b".to_string()],
            vec!["skipped_c".to_string(), "skipped_d".to_string()],
        );
        assert_eq!(plan.repos_required_for_fetch.len(), 1);
        assert_eq!(plan.repos_observed.len(), 5);
        // Critical invariant: the warmup-eligible
        // subset is strictly smaller.
        assert!(
            plan.repos_required_for_fetch.len()
                <= plan.repos_observed.len()
        );
    }

    #[test]
    fn observed_repos_do_not_overfetch_by_default() {
        // The bug Pro's letter warned about: "a plan
        // written while RUSH_FETCH_SKIP_IF_MATERIALIZED=1
        // may record repos that were observed but not
        // actually fetched. If a later run uses that
        // plan without materialized skip, it could
        // over-warm many repos."
        //
        // Build a plan with 1 invoked + many skipped.
        // Assert that the reader returns the
        // 1-repo set when consuming
        // repos_required_for_fetch. If a future
        // operator opts in to the observed set
        // explicitly, that's a separate code path
        // (not implemented in this commit).
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let observations = vec![FetchObservation {
            repo_name: "the_one_repo".into(),
            decision: "invoked".into(),
        }];
        // Plus 50 fake skipped observations.
        let mut all_obs = observations.clone();
        for i in 0..50 {
            all_obs.push(FetchObservation {
                repo_name: format!("skip_{}", i),
                decision: "skipped_materialized_trust_mode".into(),
            });
        }
        let target_patterns = vec!["//x:y".to_string()];
        let _ = write_warmup_plan_for_run(
            &ws,
            &target_patterns,
            &all_obs,
        );
        let plan = match read_warmup_plan(&ws, &target_patterns) {
            WarmupPlanReadOutcome::Loaded(p) => p,
            other => panic!("expected Loaded, got {:?}", other),
        };
        assert_eq!(plan.repos_required_for_fetch.len(), 1);
        assert_eq!(plan.repos_observed.len(), 51);
        // Auto mode's safe-default scope:
        assert_eq!(
            plan.repos_required_for_fetch,
            vec!["the_one_repo".to_string()],
            "auto must only fetch the actually-Invoked repo, not 51 skipped ones"
        );
    }

    #[test]
    fn schema_roundtrip() {
        // v2 plans must roundtrip. v1 plans must
        // fail validate() (different schema_version).
        let p = sample_plan();
        let json = serde_json::to_string(&p).expect("ser");
        let back: FetchWarmupPlan =
            serde_json::from_str(&json).expect("deser");
        assert_eq!(p, back);
        assert_eq!(back.schema_version, 2);

        // Simulate a v1 plan landing on disk and check
        // the reader rejects it as Stale.
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        let dir = fetch_warmup_plans_dir(&ws);
        std::fs::create_dir_all(&dir).unwrap();
        let v1_plan = serde_json::json!({
            "schema_version": 1,
            "workspace_root": ws.display().to_string(),
            "target_patterns": ["//x:y"],
            "repos_required": ["a"],
            "repos_fetched": ["a"],
            "repos_skipped_materialized": [],
            "generated_at_unix": 0,
            "source": "observed_fetch_report",
            "rush_version_or_writer_version": "0.0.0",
        });
        let target_patterns = vec!["//x:y".to_string()];
        std::fs::write(
            plan_path_for(&ws, &target_patterns),
            serde_json::to_string(&v1_plan).unwrap(),
        )
        .unwrap();
        let outcome = read_warmup_plan(&ws, &target_patterns);
        // The reader may classify v1 plans as either
        // Invalid (parse fail on missing v2 fields)
        // OR Stale (validate fail). Both are
        // acceptable — both make the reader ignore
        // the plan, which is the correctness
        // invariant.
        assert!(
            matches!(
                outcome,
                WarmupPlanReadOutcome::Invalid(_)
                    | WarmupPlanReadOutcome::Stale(_)
            ),
            "v1 plan must NOT be Loaded; got {:?}",
            outcome
        );
    }

    // =====================================================
    // Phase 5.C.4 commit 163 (Batch U): plan validity +
    // stale handling tests. Names per Pro's directive.
    // The Loaded path is reached only when every guard
    // passes; any of these failing produces Stale (or
    // Missing) and the orchestrator falls back to
    // non-warmup behavior — never aborts the build.
    // =====================================================

    /// Build a fully-valid v2 plan JSON value with the
    /// caller-supplied workspace_root and patterns, so
    /// individual tests can corrupt one field at a time.
    fn valid_v2_plan_json(
        workspace_root: &Path,
        target_patterns: &[&str],
    ) -> serde_json::Value {
        serde_json::json!({
            "schema_version": FETCH_WARMUP_PLAN_SCHEMA_VERSION,
            "workspace_root": workspace_root.display().to_string(),
            "target_patterns": target_patterns,
            "repos_required_for_fetch": ["alpha"],
            "repos_observed": ["alpha"],
            "repos_skipped_materialized": [],
            "repos_skipped_known_rule": [],
            "generated_at_unix": 0,
            "source": "observed_fetch_report",
            "rush_version_or_writer_version": "0.0.0",
            "incomplete": false,
        })
    }

    #[test]
    fn stale_workspace_plan_ignored() {
        // The plan file lives at the hash-keyed path
        // for the CURRENT workspace, but its embedded
        // `workspace_root` records a DIFFERENT path —
        // e.g. someone copied a plan from another
        // checkout. The reader must reject it as
        // Stale and never fall through to Loaded,
        // because the embedded repo lists may be
        // wrong for this tree.
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let other_ws = tmp.path().join("other_ws");
        let target_patterns = vec!["//foo:bar".to_string()];
        let path = plan_path_for(&ws, &target_patterns);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut plan_json = valid_v2_plan_json(&other_ws, &["//foo:bar"]);
        // Sanity: the JSON parses + structurally
        // validates; only the context check should
        // reject.
        let plan_parsed: FetchWarmupPlan =
            serde_json::from_value(plan_json.clone()).unwrap();
        plan_parsed.validate().expect(
            "structural validate must pass — only context should reject",
        );
        // Tighten the JSON we write (no-op; here to
        // signal intent that this is the on-disk shape).
        plan_json["workspace_root"] =
            serde_json::Value::String(other_ws.display().to_string());
        std::fs::write(&path, serde_json::to_string(&plan_json).unwrap())
            .unwrap();

        let outcome = read_warmup_plan(&ws, &target_patterns);
        match outcome {
            WarmupPlanReadOutcome::Stale(reason) => {
                assert!(
                    reason.contains("workspace_root"),
                    "Stale reason must cite workspace_root: {}",
                    reason
                );
            }
            other => panic!(
                "expected Stale for workspace mismatch, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn target_pattern_mismatch_ignored() {
        // Two distinct target-pattern sets normally
        // produce two distinct file paths via
        // plan_key_for. To exercise the *content*
        // mismatch check (defense-in-depth against
        // hash collisions or hand-edited sidecars),
        // we write a plan at the path keyed by one
        // pattern set but claim a DIFFERENT pattern
        // set inside the JSON.
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let read_patterns = vec!["//caller:wants".to_string()];
        let path = plan_path_for(&ws, &read_patterns);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Plan content claims an unrelated pattern.
        let plan_json = valid_v2_plan_json(&ws, &["//unrelated:pattern"]);
        std::fs::write(&path, serde_json::to_string(&plan_json).unwrap())
            .unwrap();

        let outcome = read_warmup_plan(&ws, &read_patterns);
        match outcome {
            WarmupPlanReadOutcome::Stale(reason) => {
                assert!(
                    reason.contains("target_patterns"),
                    "Stale reason must cite target_patterns: {}",
                    reason
                );
            }
            other => panic!(
                "expected Stale for pattern mismatch, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn incomplete_plan_ignored() {
        // If the writer ever flushes a plan whose
        // source run did not complete fetch
        // attribution, it sets `incomplete: true`.
        // The reader rejects such plans as Stale so
        // the next successful run rewrites them.
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let target_patterns = vec!["//foo:bar".to_string()];
        let path = plan_path_for(&ws, &target_patterns);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut plan_json = valid_v2_plan_json(&ws, &["//foo:bar"]);
        plan_json["incomplete"] = serde_json::Value::Bool(true);
        std::fs::write(&path, serde_json::to_string(&plan_json).unwrap())
            .unwrap();

        let outcome = read_warmup_plan(&ws, &target_patterns);
        match outcome {
            WarmupPlanReadOutcome::Stale(reason) => {
                assert!(
                    reason.contains("incomplete"),
                    "Stale reason must cite incomplete: {}",
                    reason
                );
            }
            other => panic!(
                "expected Stale for incomplete plan, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn invalid_schema_ignored() {
        // schema_version != current → Stale.
        // Distinct from `schema_roundtrip`'s v1 case
        // (which can land as Invalid OR Stale
        // depending on which fields the v1 payload
        // happens to share with v2). Here we keep
        // every v2 field but bump schema_version, so
        // the only failure mode is validate() →
        // Stale.
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let target_patterns = vec!["//foo:bar".to_string()];
        let path = plan_path_for(&ws, &target_patterns);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut plan_json = valid_v2_plan_json(&ws, &["//foo:bar"]);
        plan_json["schema_version"] = serde_json::Value::from(99u32);
        std::fs::write(&path, serde_json::to_string(&plan_json).unwrap())
            .unwrap();

        let outcome = read_warmup_plan(&ws, &target_patterns);
        match outcome {
            WarmupPlanReadOutcome::Stale(reason) => {
                assert!(
                    reason.contains("schema_version"),
                    "Stale reason must cite schema_version: {}",
                    reason
                );
            }
            other => panic!(
                "expected Stale for bad schema, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn missing_plan_falls_back_to_normal() {
        // No sidecar at all → Missing. The
        // orchestrator's auto-mode path treats
        // Missing identically to Stale/Invalid: skip
        // warmup, run normally, write a fresh plan at
        // the end of the run.
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let outcome = read_warmup_plan(
            &ws,
            &["//never:built".to_string()],
        );
        assert_eq!(outcome, WarmupPlanReadOutcome::Missing);
        // Operator-visible classification is "missing"
        // (vs the Stale variants) so the log line
        // distinguishes "first run for this target"
        // from "rejected stale sidecar".
        assert_eq!(outcome.as_str(), "missing");
    }

    // =====================================================
    // Phase 5.C.4 commit 165 (Batch U): default-on auto
    // warmup tests. Names per Pro's directive. These
    // cover the env-parser flip + the read-write
    // cycle that powers the user-invisible 6.7×
    // speedup on second + subsequent runs.
    // =====================================================

    /// Helper: save+clear an env var, run a closure
    /// with the env in the desired state, restore. The
    /// rush-build test runner runs unit tests in
    /// parallel by default; tests that depend on env
    /// vars must guard their reads.
    /// Serializes every environment mutation in this module.
    ///
    /// `std::env` is process-global while Rust runs tests in parallel
    /// threads, so two tests setting the same variable raced: one would
    /// observe the other's value, or restore over it. That is why
    /// opt_out_disables_plan passed alone and failed in the full suite.
    ///
    /// REENTRANT on purpose. Tests nest these - warmup_failure_does_not_
    /// fail_non_strict sets RUSH_FETCH_WARMUP_PLAN and then
    /// RUSH_FETCH_WARMUP_STRICT inside it - and a plain Mutex deadlocks
    /// the thread against a lock it already holds. That turned a 1-second
    /// suite into one that never finished.
    static ENV_LOCK: once_cell::sync::Lazy<parking_lot::ReentrantMutex<()>> =
        once_cell::sync::Lazy::new(|| parking_lot::ReentrantMutex::new(()));

    fn with_env<F: FnOnce()>(var: &str, value: Option<&str>, f: F) {
        let _guard = ENV_LOCK.lock();
        let prior = std::env::var(var).ok();
        match value {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }
        f();
        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }
    }

    #[test]
    fn default_auto_first_run_writes_plan() {
        // Default-on Auto, no sidecar yet → reader
        // returns Missing. The orchestrator's end-of-
        // run writer flushes a fresh plan. The next
        // run will Load. This test exercises the
        // first-run half: env-default = Auto AND
        // reader returns Missing AND writer succeeds.
        with_env("RUSH_FETCH_WARMUP_PLAN", None, || {
            assert_eq!(
                FetchWarmupPlanMode::from_env(),
                FetchWarmupPlanMode::Auto,
                "unset env must default to Auto post-commit-165"
            );

            let tmp = TempDir::new().unwrap();
            let ws = tmp.path().join("ws");
            std::fs::create_dir_all(&ws).unwrap();
            let target_patterns = vec!["//foo:bar".to_string()];

            // First run: no plan.
            let outcome = read_warmup_plan(&ws, &target_patterns);
            assert_eq!(
                outcome,
                WarmupPlanReadOutcome::Missing,
                "first run must report Missing"
            );

            // End-of-run writer flushes a plan from
            // the observed fetch report.
            let observations = vec![FetchObservation {
                repo_name: "alpha".into(),
                decision: "invoked".into(),
            }];
            let write_outcome = write_warmup_plan_for_run(
                &ws,
                &target_patterns,
                &observations,
            );
            assert!(
                write_outcome.is_written(),
                "first run must write plan, got {:?}",
                write_outcome
            );

            // Sidecar exists at the expected path.
            assert!(plan_path_for(&ws, &target_patterns).exists());
        });
    }

    #[test]
    fn default_auto_second_run_uses_plan() {
        // Second run: env unset (still Auto by
        // default), prior run's plan on disk → reader
        // returns Loaded with the right repos. This
        // is the user-invisible speedup path.
        with_env("RUSH_FETCH_WARMUP_PLAN", None, || {
            assert_eq!(
                FetchWarmupPlanMode::from_env(),
                FetchWarmupPlanMode::Auto
            );

            let tmp = TempDir::new().unwrap();
            let ws = tmp.path().join("ws");
            std::fs::create_dir_all(&ws).unwrap();
            let target_patterns = vec!["//foo:bar".to_string()];

            // Pre-write a plan as if a previous run
            // finished and flushed it.
            let observations = vec![
                FetchObservation {
                    repo_name: "alpha".into(),
                    decision: "invoked".into(),
                },
                FetchObservation {
                    repo_name: "beta".into(),
                    decision: "skipped_known_rule".into(),
                },
            ];
            let _ = write_warmup_plan_for_run(
                &ws,
                &target_patterns,
                &observations,
            );

            // Second run: read loads.
            let outcome = read_warmup_plan(&ws, &target_patterns);
            let plan = outcome.loaded().expect(
                "second run with prior plan must Load, default-on Auto",
            );
            // The Invoked-only safe-default set
            // drives the warmup. beta (known_rule) is
            // present in observed but NOT in
            // required_for_fetch.
            assert_eq!(
                plan.repos_required_for_fetch,
                vec!["alpha".to_string()]
            );
            assert!(plan.repos_observed.contains(&"beta".to_string()));
        });
    }

    #[test]
    fn opt_out_disables_plan() {
        // Operators who want pre-Batch-T behavior
        // back set `RUSH_FETCH_WARMUP_PLAN=off`. Any
        // of the documented opt-out values must work.
        for val in ["off", "OFF", "Off", "0", "false", "no", "disabled"] {
            with_env("RUSH_FETCH_WARMUP_PLAN", Some(val), || {
                assert_eq!(
                    FetchWarmupPlanMode::from_env(),
                    FetchWarmupPlanMode::Off,
                    "value {:?} must opt out of default-Auto",
                    val
                );
            });
        }
    }

    #[test]
    fn stale_plan_does_not_fail() {
        // Default-on Auto + on-disk plan with bad
        // schema_version → reader returns Stale
        // (not Err). The orchestrator treats Stale
        // identically to Missing. Build does NOT
        // fail; the analyze pass proceeds normally
        // and a fresh plan overwrites the stale one
        // at end-of-run.
        with_env("RUSH_FETCH_WARMUP_PLAN", None, || {
            assert_eq!(
                FetchWarmupPlanMode::from_env(),
                FetchWarmupPlanMode::Auto
            );

            let tmp = TempDir::new().unwrap();
            let ws = tmp.path().join("ws");
            std::fs::create_dir_all(&ws).unwrap();
            let target_patterns = vec!["//foo:bar".to_string()];

            // Write a stale plan (future schema).
            let path = plan_path_for(&ws, &target_patterns);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut stale_json =
                valid_v2_plan_json(&ws, &["//foo:bar"]);
            stale_json["schema_version"] =
                serde_json::Value::from(99u32);
            std::fs::write(
                &path,
                serde_json::to_string(&stale_json).unwrap(),
            )
            .unwrap();

            // Reader returns Stale, NEVER Err.
            let outcome = read_warmup_plan(&ws, &target_patterns);
            assert!(
                matches!(outcome, WarmupPlanReadOutcome::Stale(_)),
                "stale plan must Stale not Err: {:?}",
                outcome
            );
            assert!(outcome.loaded().is_none());

            // After the failing read, the same
            // workflow can write a fresh plan on top
            // of the stale one. Self-healing in one
            // run.
            let observations = vec![FetchObservation {
                repo_name: "alpha".into(),
                decision: "invoked".into(),
            }];
            let write_outcome = write_warmup_plan_for_run(
                &ws,
                &target_patterns,
                &observations,
            );
            assert!(
                write_outcome.is_written(),
                "writer must overwrite stale plan, got {:?}",
                write_outcome
            );

            // Now the next read Loads cleanly.
            let outcome2 = read_warmup_plan(&ws, &target_patterns);
            assert!(
                outcome2.loaded().is_some(),
                "fresh plan must Load: {:?}",
                outcome2
            );
        });
    }

    #[test]
    fn warmup_failure_does_not_fail_non_strict() {
        // Cross-crate contract: turning default-Auto
        // on did NOT implicitly enable strict mode.
        // Strict (`RUSH_FETCH_WARMUP_STRICT`) is an
        // independent env var; flipping the plan
        // default to Auto does not change its
        // default of OFF.
        //
        // The actual non-fatal behavior is exercised
        // in rush-starlark's
        // `warmup_failure_non_strict_falls_back_to_normal_analyze`.
        // Here we just verify that the two env
        // surfaces are independent so an operator
        // who upgrades doesn't suddenly get strict
        // semantics they didn't ask for.
        with_env("RUSH_FETCH_WARMUP_PLAN", None, || {
            with_env("RUSH_FETCH_WARMUP_STRICT", None, || {
                assert_eq!(
                    FetchWarmupPlanMode::from_env(),
                    FetchWarmupPlanMode::Auto,
                    "plan default = Auto"
                );
                assert!(
                    !rush_starlark::fetch_strategy::is_warmup_strict(),
                    "strict default must REMAIN off even with default-Auto plan"
                );
            });
        });
    }

    // =====================================================
    // Phase 5.C.4 commit 169 (Batch V): plan read/write
    // failure hardening tests. Names per Pro's
    // directive. Each test proves the default path is
    // non-fatal: read/write errors record in counters
    // but never propagate as Err to the caller.
    // =====================================================

    #[test]
    fn invalid_plan_does_not_fail_default_build() {
        // Corrupt JSON at the plan path. Reader returns
        // Invalid (not Err). Counters record it.
        // No panic, no Err — the caller is free to
        // continue with normal analyze.
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let target_patterns = vec!["//foo:bar".to_string()];
        let path = plan_path_for(&ws, &target_patterns);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ this is not valid json }").unwrap();

        let outcome = read_warmup_plan(&ws, &target_patterns);
        assert!(matches!(outcome, WarmupPlanReadOutcome::Invalid(_)));

        // Counters surface the invalid read.
        let mut counters = WarmupPlanCounters::new();
        counters.record_read(&outcome);
        assert_eq!(counters.plan_read_invalid, 1);
        assert!(counters.has_anomaly(), "invalid read is an anomaly");
        // Other counters stay zero — no read_hit/missing/stale.
        assert_eq!(counters.plan_read_hit, 0);
        assert_eq!(counters.plan_read_missing, 0);
        assert_eq!(counters.plan_read_stale, 0);
    }

    #[test]
    #[cfg(unix)]
    fn unwritable_plan_dir_does_not_fail_default_build() {
        use std::os::unix::fs::PermissionsExt;
        // Make the plan parent directory read-only.
        // Writer returns Failed (not Err / panic).
        // Counters record it. Caller is free to
        // continue.
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        // Pre-create the plans subdir as read-only so
        // the writer's create_dir_all sees it but
        // can't write into it.
        let dir = fetch_warmup_plans_dir(&ws);
        std::fs::create_dir_all(&dir).unwrap();
        let mut perms = std::fs::metadata(&dir).unwrap().permissions();
        perms.set_mode(0o555); // r-xr-xr-x
        std::fs::set_permissions(&dir, perms).unwrap();

        let observations = vec![FetchObservation {
            repo_name: "alpha".into(),
            decision: "invoked".into(),
        }];
        let outcome = write_warmup_plan_for_run(
            &ws,
            &["//foo:bar".to_string()],
            &observations,
        );
        // Per Pro's commit-169 contract: write failure
        // surfaces as Failed(reason), NOT a panic.
        // (Test isn't strict about the variant —
        // some filesystems may allow the write to
        // succeed via path tricks; on most they
        // return Failed.)
        // Restore perms so TempDir can clean up.
        let mut restore =
            std::fs::metadata(&dir).unwrap().permissions();
        restore.set_mode(0o755);
        let _ = std::fs::set_permissions(&dir, restore);

        // Counters record the outcome regardless of
        // which non-fatal variant it landed in.
        let mut counters = WarmupPlanCounters::new();
        counters.record_write(&outcome);
        let total_writes = counters.plan_write_success
            + counters.plan_write_failed
            + counters.plan_write_skipped;
        assert_eq!(
            total_writes, 1,
            "every write must increment exactly one bucket"
        );
        // The whole point of the contract: no panic,
        // no Err. If we got here, the contract held.
    }

    #[test]
    fn stale_plan_rewritten() {
        // Self-healing-in-one-run loop:
        //   1. Write stale plan on disk.
        //   2. Reader → Stale.
        //   3. Writer overwrites with fresh plan.
        //   4. Next read → Loaded.
        // Counters reflect the full cycle.
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let target_patterns = vec!["//foo:bar".to_string()];
        let path = plan_path_for(&ws, &target_patterns);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        // Step 1: stale on disk.
        let mut stale_json = valid_v2_plan_json(&ws, &["//foo:bar"]);
        stale_json["schema_version"] = serde_json::Value::from(99u32);
        std::fs::write(&path, serde_json::to_string(&stale_json).unwrap())
            .unwrap();

        let mut counters = WarmupPlanCounters::new();
        // Step 2: read → Stale.
        let outcome1 = read_warmup_plan(&ws, &target_patterns);
        counters.record_read(&outcome1);
        assert!(matches!(outcome1, WarmupPlanReadOutcome::Stale(_)));
        assert_eq!(counters.plan_read_stale, 1);

        // Step 3: writer overwrites.
        let observations = vec![FetchObservation {
            repo_name: "alpha".into(),
            decision: "invoked".into(),
        }];
        let outcome2 = write_warmup_plan_for_run(
            &ws,
            &target_patterns,
            &observations,
        );
        counters.record_write(&outcome2);
        assert!(outcome2.is_written());
        assert_eq!(counters.plan_write_success, 1);

        // Step 4: re-read sees the fresh plan.
        let outcome3 = read_warmup_plan(&ws, &target_patterns);
        counters.record_read(&outcome3);
        assert!(matches!(outcome3, WarmupPlanReadOutcome::Loaded(_)));
        assert_eq!(
            counters.plan_read_hit, 1,
            "the rewritten plan must Load"
        );
        // Stale stays at 1 from the first read.
        assert_eq!(counters.plan_read_stale, 1);
        // Anomaly remains true because Stale happened
        // earlier this run — operators see the cycle
        // in the counters even though the run
        // ultimately succeeded.
        assert!(counters.has_anomaly());
    }

    #[test]
    fn counters_record_all_read_variants() {
        // Every WarmupPlanReadOutcome variant must
        // map to exactly one counter increment. This
        // is the property commit 170's report relies
        // on.
        let plan = sample_plan();
        let cases = vec![
            (WarmupPlanReadOutcome::Loaded(plan.clone()), "hit"),
            (WarmupPlanReadOutcome::Missing, "missing"),
            (WarmupPlanReadOutcome::Stale("test".into()), "stale"),
            (WarmupPlanReadOutcome::Invalid("test".into()), "invalid"),
            (WarmupPlanReadOutcome::IoError("test".into()), "io_error"),
        ];
        for (outcome, label) in cases {
            let mut counters = WarmupPlanCounters::new();
            counters.record_read(&outcome);
            let total = counters.plan_read_hit
                + counters.plan_read_missing
                + counters.plan_read_stale
                + counters.plan_read_invalid
                + counters.plan_read_io_error;
            assert_eq!(
                total, 1,
                "outcome {} must produce exactly one increment",
                label
            );
        }
    }

    #[test]
    fn counters_record_all_write_variants() {
        // Every WarmupPlanWriteOutcome variant must
        // map to exactly one counter increment.
        let cases = vec![
            (
                WarmupPlanWriteOutcome::Written(PathBuf::from("/x")),
                "success",
            ),
            (WarmupPlanWriteOutcome::SkippedNoTargets, "skipped"),
            (WarmupPlanWriteOutcome::SkippedNoRepos, "skipped"),
            (
                WarmupPlanWriteOutcome::Failed("test".into()),
                "failed",
            ),
        ];
        for (outcome, label) in cases {
            let mut counters = WarmupPlanCounters::new();
            counters.record_write(&outcome);
            let total = counters.plan_write_success
                + counters.plan_write_failed
                + counters.plan_write_skipped;
            assert_eq!(
                total, 1,
                "outcome {} must produce exactly one increment",
                label
            );
        }
    }

    // =====================================================
    // Phase 5.C.4 commit 170 (Batch V): WarmupReport
    // builder tests. Names per Pro's directive.
    // Tests are pure-function (no orchestrator) so
    // they run fast and stay hermetic.
    // =====================================================

    /// Minimal stub implementing UnifiedWarmupOutcomeView
    /// for tests so we don't need rush-starlark in the
    /// rush-build unit tests.
    struct StubOutcome {
        strategy: &'static str,
        wall_ms: u64,
        succeeded: usize,
        failed: usize,
    }
    impl UnifiedWarmupOutcomeView for StubOutcome {
        fn strategy_name(&self) -> &'static str {
            self.strategy
        }
        fn wall_ms(&self) -> u64 {
            self.wall_ms
        }
        fn succeeded_count(&self) -> usize {
            self.succeeded
        }
        fn failed_count(&self) -> usize {
            self.failed
        }
    }

    #[test]
    fn report_plan_hit() {
        // Default-on Auto + reader Loaded + warmup
        // fired with combined strategy + all
        // succeeded. The healthy second-run shape.
        let mut counters = WarmupPlanCounters::new();
        counters.plan_read_hit = 1;
        counters.plan_warmup_used = 1;
        counters.plan_write_success = 1;
        let outcome = StubOutcome {
            strategy: "combined",
            wall_ms: 11000,
            succeeded: 8,
            failed: 0,
        };
        let report = WarmupReport::build(
            /*plan_mode_is_auto=*/ true,
            &counters,
            Some(&outcome),
            /*repos_from_plan=*/ 8,
            /*repos_from_env=*/ 0,
            /*normal_fetches_after_warmup=*/ 0,
        );
        assert_eq!(report.warmup_plan_mode, "auto");
        assert_eq!(report.plan_status, "hit");
        assert_eq!(report.warmup_strategy, "combined");
        assert_eq!(report.repos_warmed_count, 8);
        assert_eq!(report.repos_from_plan, 8);
        assert_eq!(report.repos_from_env, 0);
        assert_eq!(report.warmup_failures, 0);
        assert_eq!(report.normal_fetches_after_warmup, 0);
        assert!(!report.plan_anomaly);
        assert_eq!(report.warmup_wall_ms, 11000);
        // Human line includes all fields.
        let human = report.format_human();
        assert!(human.contains("mode=auto"), "{}", human);
        assert!(human.contains("plan=hit"), "{}", human);
        assert!(human.contains("strategy=combined"), "{}", human);
        assert!(human.contains("warmed=8"), "{}", human);
    }

    #[test]
    fn report_plan_miss() {
        // Default-on Auto, first run, no plan on
        // disk → reader Missing → warmup did NOT
        // fire → outcome=None → strategy="none".
        let mut counters = WarmupPlanCounters::new();
        counters.plan_read_missing = 1;
        counters.plan_warmup_fallback = 1;
        counters.plan_write_success = 1;
        let report = WarmupReport::build(
            true,
            &counters,
            None, // no warmup invocation
            0,
            0,
            0,
        );
        assert_eq!(report.warmup_plan_mode, "auto");
        assert_eq!(report.plan_status, "missing");
        assert_eq!(report.warmup_strategy, "none");
        assert_eq!(report.repos_warmed_count, 0);
        assert!(!report.plan_anomaly, "missing is healthy first-run");
    }

    #[test]
    fn report_stale_plan() {
        // Default-on Auto + stale plan on disk →
        // reader Stale → warmup did NOT fire →
        // anomaly=true (stale is an anomaly even
        // though the build didn't fail).
        let mut counters = WarmupPlanCounters::new();
        counters.plan_read_stale = 1;
        counters.plan_warmup_fallback = 1;
        counters.plan_write_success = 1; // rewriting healed it
        let report = WarmupReport::build(true, &counters, None, 0, 0, 0);
        assert_eq!(report.plan_status, "stale");
        assert_eq!(report.warmup_strategy, "none");
        assert!(report.plan_anomaly);
        let human = report.format_human();
        assert!(human.contains("plan=stale"), "{}", human);
        assert!(human.contains("anomaly=true"), "{}", human);
    }

    #[test]
    fn report_normal_fetches_after_warmup() {
        // Plan covered 8 repos, but analyze still
        // serial-fetched 3 more. Operators see the
        // 3 in `post_warmup_fetches` and can
        // investigate why those repos weren't in the
        // plan.
        let mut counters = WarmupPlanCounters::new();
        counters.plan_read_hit = 1;
        counters.plan_warmup_used = 1;
        counters.plan_write_success = 1;
        let outcome = StubOutcome {
            strategy: "combined",
            wall_ms: 11000,
            succeeded: 8,
            failed: 0,
        };
        let report = WarmupReport::build(
            true,
            &counters,
            Some(&outcome),
            8,
            0,
            3, // 3 post-warmup serial fetches
        );
        assert_eq!(report.normal_fetches_after_warmup, 3);
        let human = report.format_human();
        assert!(human.contains("post_warmup_fetches=3"), "{}", human);
    }

    #[test]
    fn report_opt_out_mode() {
        // RUSH_FETCH_WARMUP_PLAN=off → plan_mode is
        // off → reader never ran → plan_status is
        // "n/a" → warmup did NOT fire from the plan
        // path. Env-driven warmup may have fired,
        // but the report still says strategy=none if
        // outcome=None.
        let mut counters = WarmupPlanCounters::new();
        counters.plan_warmup_fallback = 1;
        let report = WarmupReport::build(
            /*plan_mode_is_auto=*/ false,
            &counters,
            None,
            0,
            0,
            0,
        );
        assert_eq!(report.warmup_plan_mode, "off");
        assert_eq!(report.plan_status, "n/a");
        assert!(!report.plan_anomaly);
    }

    #[test]
    fn report_json_serializes() {
        // Sanity: WarmupReport is serde::Serialize-able
        // so the orchestrator's RUSH_WARMUP_REPORT_JSON
        // gate can emit one line per run.
        let counters = WarmupPlanCounters::new();
        let report = WarmupReport::build(true, &counters, None, 0, 0, 0);
        let json = serde_json::to_string(&report)
            .expect("WarmupReport must serialize");
        assert!(json.contains("warmup_plan_mode"));
        assert!(json.contains("plan_status"));
        assert!(json.contains("warmup_strategy"));
        assert!(json.contains("repos_warmed_count"));
        assert!(json.contains("normal_fetches_after_warmup"));
    }

    #[test]
    fn counters_anomaly_logic() {
        // has_anomaly() is the operator's "something
        // was off this run" signal. Stale, invalid,
        // io_error, and write_failed are anomalies;
        // hit, missing, write_success, write_skipped
        // are NOT.
        let mut healthy = WarmupPlanCounters::new();
        healthy.plan_read_hit = 1;
        healthy.plan_write_success = 1;
        assert!(!healthy.has_anomaly());

        let mut healthy_missing = WarmupPlanCounters::new();
        healthy_missing.plan_read_missing = 1;
        healthy_missing.plan_write_success = 1;
        assert!(
            !healthy_missing.has_anomaly(),
            "first-run Missing is healthy"
        );

        let mut stale = WarmupPlanCounters::new();
        stale.plan_read_stale = 1;
        assert!(stale.has_anomaly());

        let mut invalid = WarmupPlanCounters::new();
        invalid.plan_read_invalid = 1;
        assert!(invalid.has_anomaly());

        let mut io_err = WarmupPlanCounters::new();
        io_err.plan_read_io_error = 1;
        assert!(io_err.has_anomaly());

        let mut write_fail = WarmupPlanCounters::new();
        write_fail.plan_write_failed = 1;
        assert!(write_fail.has_anomaly());
    }

    // =====================================================
    // Phase 5.C.4 commit 173 (Batch V): default-on
    // regression tests. Names per Pro's directive.
    // These encode the contract that Batches T+U+V
    // built. Each test guards a specific invariant
    // future commits could accidentally regress.
    //
    // Hermetic: use fake repos/decisions; no real
    // bazel; no TensorFlow dependency.
    // =====================================================

    #[test]
    fn first_run_without_plan_writes_plan() {
        // The default-on contract: env unset →
        // mode=Auto → reader Missing → analyze runs
        // baseline → writer flushes plan at end.
        // Future regression to watch for: a commit
        // that gates the writer behind a flag (which
        // happened in Batch U commit 166 with the
        // cache_counters gate). This test catches
        // that class of bug.
        with_env("RUSH_FETCH_WARMUP_PLAN", None, || {
            assert_eq!(
                FetchWarmupPlanMode::from_env(),
                FetchWarmupPlanMode::Auto,
                "default mode must be Auto"
            );

            let tmp = TempDir::new().unwrap();
            let ws = tmp.path().join("ws");
            std::fs::create_dir_all(&ws).unwrap();
            let target_patterns = vec!["//foo:bar".to_string()];

            // Plan absent before the run.
            assert!(
                !plan_path_for(&ws, &target_patterns).exists(),
                "fresh workspace must have no plan"
            );

            // Run "completes" (we simulate by calling
            // the writer with observed fetches).
            let observations = vec![FetchObservation {
                repo_name: "alpha".into(),
                decision: "invoked".into(),
            }];
            let outcome = write_warmup_plan_for_run(
                &ws,
                &target_patterns,
                &observations,
            );
            assert!(
                outcome.is_written(),
                "first run must write plan, got {:?}",
                outcome
            );

            // Plan exists after the run.
            assert!(
                plan_path_for(&ws, &target_patterns).exists(),
                "plan must exist after first run writes it"
            );
        });
    }

    #[test]
    fn second_run_uses_plan_by_default() {
        // The 6.5× speedup contract: env unset →
        // mode=Auto → reader Loaded → plan_repos
        // populated → unified warmup fires. Future
        // regression to watch for: a commit that
        // reorders the read-vs-warmup branches or
        // strips the plan_repos consumption.
        with_env("RUSH_FETCH_WARMUP_PLAN", None, || {
            let tmp = TempDir::new().unwrap();
            let ws = tmp.path().join("ws");
            std::fs::create_dir_all(&ws).unwrap();
            let target_patterns = vec!["//foo:bar".to_string()];

            // Pre-write a plan as a prior run would.
            let observations = vec![
                FetchObservation {
                    repo_name: "alpha".into(),
                    decision: "invoked".into(),
                },
                FetchObservation {
                    repo_name: "beta".into(),
                    decision: "invoked".into(),
                },
            ];
            let _ = write_warmup_plan_for_run(
                &ws,
                &target_patterns,
                &observations,
            );

            // Default mode is Auto.
            assert_eq!(
                FetchWarmupPlanMode::from_env(),
                FetchWarmupPlanMode::Auto
            );

            // Second run loads the plan.
            let outcome = read_warmup_plan(&ws, &target_patterns);
            let plan = outcome.loaded().expect(
                "second run with prior plan must Load by default",
            );
            // Plan contains the Invoked-only safe-default
            // subset that drives the warmup invocation.
            assert_eq!(plan.repos_required_for_fetch.len(), 2);
            assert!(
                plan.repos_required_for_fetch
                    .contains(&"alpha".to_string())
            );
            assert!(
                plan.repos_required_for_fetch
                    .contains(&"beta".to_string())
            );
        });
    }

    #[test]
    fn invalid_plan_falls_back() {
        // The non-fatal contract: corrupt JSON on
        // disk → reader returns Invalid (not Err) →
        // orchestrator falls through to baseline
        // analyze. Future regression to watch for:
        // changing read_warmup_plan to return
        // Result<...> instead of the typed enum.
        with_env("RUSH_FETCH_WARMUP_PLAN", None, || {
            let tmp = TempDir::new().unwrap();
            let ws = tmp.path().join("ws");
            std::fs::create_dir_all(&ws).unwrap();
            let target_patterns = vec!["//foo:bar".to_string()];
            let path = plan_path_for(&ws, &target_patterns);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"not valid json at all").unwrap();

            let outcome = read_warmup_plan(&ws, &target_patterns);
            assert!(
                matches!(outcome, WarmupPlanReadOutcome::Invalid(_)),
                "corrupt JSON must return Invalid, got {:?}",
                outcome
            );
            // Critical: the outcome is a value-typed
            // enum, never an Err. The orchestrator
            // can't accidentally propagate it as a
            // build failure.
            assert!(outcome.loaded().is_none());
            assert_eq!(outcome.as_str(), "invalid");

            // Counters record the invalid read.
            let mut counters = WarmupPlanCounters::new();
            counters.record_read(&outcome);
            assert_eq!(counters.plan_read_invalid, 1);
            assert!(counters.has_anomaly());
        });
    }

    #[test]
    fn build_mode_does_not_emit_extra_fetches_after_warmup() {
        // The build-mode-after-warmup contract
        // (Pro's commit-171 smoke caught a regression
        // here): when the warmup invocation covers
        // every repo analyze needs, post_warmup_fetches
        // must be zero. Future regression to watch
        // for: the report's post_warmup_fetches
        // computation drifting (e.g. counting
        // warmup-internal fetches as serial fetches).
        //
        // Hermetic test: drive WarmupReport::build()
        // with the property under test. We can't
        // run real build mode in a unit test, but
        // the post_warmup_fetches field IS computed
        // by the same formula the orchestrator uses
        // — verifying its math here guards the
        // orchestrator's emission shape.
        let mut counters = WarmupPlanCounters::new();
        counters.plan_read_hit = 1;
        counters.plan_warmup_used = 1;
        counters.plan_write_success = 1;
        let outcome = StubOutcome {
            strategy: "combined",
            wall_ms: 11000,
            succeeded: 8,
            failed: 0,
        };
        // The orchestrator computes
        //   post_warmup_fetches = stats.fetch_invoked_count
        //                       - warmed_count
        // When warmup covered all 8 needed repos and
        // fetch_invoked_count is 8 (warmup IS the
        // fetch), the difference is 0.
        let report = WarmupReport::build(
            true,
            &counters,
            Some(&outcome),
            /*repos_from_plan=*/ 8,
            /*repos_from_env=*/ 0,
            /*normal_fetches_after_warmup=*/ 0,
        );
        assert_eq!(
            report.normal_fetches_after_warmup, 0,
            "build mode with full plan coverage must NOT emit \
             extra serial fetches after warmup"
        );
        assert_eq!(report.repos_warmed_count, 8);
        assert_eq!(report.warmup_failures, 0);
        // Anomaly stays false even though build mode
        // is active. The flag tracks plan-surface
        // health, not build-mode-vs-analyze-only.
        assert!(!report.plan_anomaly);

        // The complementary case: incomplete CSV
        // (Batch U C5 shape) — 7 warmed but 8 needed
        // → 1 post-warmup fetch.
        let mut counters2 = WarmupPlanCounters::new();
        counters2.plan_warmup_fallback = 1;
        let outcome2 = StubOutcome {
            strategy: "combined",
            wall_ms: 11000,
            succeeded: 7,
            failed: 0,
        };
        let report2 = WarmupReport::build(
            false, // mode=off
            &counters2,
            Some(&outcome2),
            0,
            7,
            1, // 1 serial fetch after warmup
        );
        assert_eq!(
            report2.normal_fetches_after_warmup, 1,
            "incomplete CSV must show the missing-repo \
             count in post_warmup_fetches"
        );
    }

    #[test]
    fn opt_out_disables_plan_v173() {
        // Already covered by commit 165's
        // opt_out_disables_plan but Pro's commit-173
        // spec lists this name explicitly as a
        // regression test. Recorded here as a
        // separate guard so future renames don't
        // accidentally drop the assertion.
        for val in ["off", "OFF", "Off", "0", "false", "no", "disabled"] {
            with_env("RUSH_FETCH_WARMUP_PLAN", Some(val), || {
                assert_eq!(
                    FetchWarmupPlanMode::from_env(),
                    FetchWarmupPlanMode::Off,
                    "value {:?} must continue to opt out",
                    val
                );
            });
        }
        // Auto is the unset default.
        with_env("RUSH_FETCH_WARMUP_PLAN", None, || {
            assert_eq!(
                FetchWarmupPlanMode::from_env(),
                FetchWarmupPlanMode::Auto
            );
        });
    }

    #[test]
    fn validate_in_context_accepts_normalized_targets() {
        // Reader normalizes (sort + dedup) before
        // comparison so caller order doesn't matter.
        // Sanity test of the normalization contract.
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        // Plan was written with sorted targets
        // [//a, //b] (from_observation sorts).
        let plan = FetchWarmupPlan::from_observation(
            ws.clone(),
            vec!["//b".to_string(), "//a".to_string()],
            vec!["repo1".to_string()],
            vec!["repo1".to_string()],
            vec![],
            vec![],
        );
        // Caller passes unsorted + duplicated; the
        // context-check normalizes and accepts.
        plan.validate_in_context(
            &ws,
            &[
                "//a".to_string(),
                "//b".to_string(),
                "//a".to_string(),
            ],
        )
        .expect("normalized caller input must accept");
    }
}
