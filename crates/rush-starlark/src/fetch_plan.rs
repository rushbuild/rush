// Phase 5.C.4 commit 140 (Batch R): FetchPlan data
// model.
//
// Pre-Batch-R the external-repo fetch path made
// decisions one repo at a time from inside
// `BzlLoader::try_fetch_known_repo`. Batch R's goal
// is to reduce the remaining ~55 s of serial
// `bazel sync --only=<repo>` waits via either a
// combined single-invocation fetch (commit 141) or
// bounded parallel fetch (commit 142). Both need
// the SAME source of truth about "what would happen
// for repo X, and which side of the fetch fence is
// it on?" — that's `FetchPlan`.
//
// Commit 140 lands ONLY the data model + a pure
// `classify_repo_for_fetch_plan` predicate + tests.
// No call site is wired yet; no behavior changes.
// Commits 141 / 142 will build the strategy
// executors that consume FetchPlanEntry vectors.

use std::path::{Path, PathBuf};

/// Phase 5.C.4 commit 140 (Batch R): classification
/// outcome for a single external repo at the moment
/// rush is asked to resolve it.
///
/// Mirrors the existing `FetchDecision` enum in
/// `bzl_loader.rs` but is forward-looking ("what we
/// WOULD do") rather than after-the-fact ("what we
/// did"). The two are paired in Batch R: a fetch
/// strategy reads a vector of `FetchPlanEntry`,
/// executes the `NeedsFetch` subset (Serial /
/// Combined / Parallel), and records a
/// `FetchDecision` per repo.
///
/// `FailedProbe` is reserved for commit 141 — when
/// the strategy executor probes bazel's
/// `--help fetch` (or equivalent) and the
/// capability is unavailable, the repo is
/// classified `FailedProbe` so the operator can
/// see why a combined strategy fell back to serial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchClassification {
    /// `RUSH_FETCH_SKIP_IF_MATERIALIZED=1` matched:
    /// vendored `external/<repo>/` exists AND a
    /// BUILD or BUILD.bazel file is present. No
    /// fetch invocation needed.
    SkippedMaterialized,
    /// Pre-existing `SKIP_FETCH_REPOS` allowlist
    /// matched: vendored `external/<repo>/` exists
    /// AND the repo name is in the hand-maintained
    /// list. No fetch invocation needed.
    SkippedKnownRule,
    /// Repo is not materialized; a bazel-fetch
    /// subprocess is required. This is the set
    /// commits 141 / 142 will try to orchestrate
    /// (combined or parallel).
    NeedsFetch,
    /// `RUSH_FETCH_SKIP_IF_MATERIALIZED=1` was set
    /// but the vendored dir didn't exist. Treated
    /// as `NeedsFetch` by the rest of the pipeline
    /// — this variant exists to surface the
    /// distinction in reports.
    MissingDir,
    /// `RUSH_FETCH_SKIP_IF_MATERIALIZED=1` was set,
    /// the vendored dir existed, but no BUILD file
    /// was present. Same outcome as `NeedsFetch`
    /// but distinguishes the diagnostic in
    /// reports.
    MissingBuild,
    /// Commit 141: probing bazel's combined-fetch
    /// capability failed (e.g., wrong bazel version,
    /// command unavailable). The strategy falls
    /// back to serial; this variant records why.
    FailedProbe,
}

impl FetchClassification {
    /// Stable string form for serialization /
    /// human reports. Matches the
    /// `FetchDecision` discriminants used in
    /// commit 135's `FetchReportEntry`.
    pub fn as_str(&self) -> &'static str {
        match self {
            FetchClassification::SkippedMaterialized => {
                "skipped_materialized"
            }
            FetchClassification::SkippedKnownRule => {
                "skipped_known_rule"
            }
            FetchClassification::NeedsFetch => "needs_fetch",
            FetchClassification::MissingDir => {
                "materialized_missing_dir"
            }
            FetchClassification::MissingBuild => {
                "materialized_missing_build"
            }
            FetchClassification::FailedProbe => "failed_probe",
        }
    }

    /// True iff the repo will be passed to a bazel
    /// fetch invocation (serial / combined / parallel
    /// strategy). The orchestrator filters its
    /// `Vec<FetchPlanEntry>` on this when building
    /// the actual `bazel sync` command.
    pub fn needs_fetch(&self) -> bool {
        matches!(
            self,
            FetchClassification::NeedsFetch
                | FetchClassification::MissingDir
                | FetchClassification::MissingBuild
                | FetchClassification::FailedProbe
        )
    }
}

/// Phase 5.C.4 commit 140 (Batch R): one row of the
/// FetchPlan. Built by
/// `classify_repo_for_fetch_plan` (or a future
/// orchestrator-level builder).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchPlanEntry {
    pub repo_name: String,
    pub classification: FetchClassification,
    /// Why rush was asked to materialize this repo
    /// (e.g., load path of the .bzl that triggered
    /// the lookup). Not populated by the pure
    /// classifier — wired by call sites in
    /// commits 141+ when context is available.
    pub requested_reason: Option<String>,
    /// Vendored `<workspace>/external/<repo>/` path
    /// — populated regardless of whether the dir
    /// exists, so reports can show "we expected to
    /// find it here."
    pub materialized_path: Option<PathBuf>,
    /// Whether `BUILD` or `BUILD.bazel` was present
    /// at `materialized_path`. Only meaningful when
    /// `materialized_path` is `Some`.
    pub has_build_file: bool,
    /// Repo rule kind if known
    /// (`http_archive`, `git_repository`,
    /// `local_repository`, etc.). Not populated by
    /// the pure classifier — wired by call sites
    /// in commits 141+ where the WORKSPACE-scan
    /// result is available.
    pub fetch_rule_kind: Option<String>,
    /// The bazel-fetch command rush WOULD invoke
    /// for this repo if the strategy chose to fetch
    /// it. None for skip variants. Used by
    /// commit 141's combined-strategy to construct
    /// a single multi-repo `bazel sync` command.
    pub expected_command: Option<Vec<String>>,
}

impl FetchPlanEntry {
    /// Bare-bones constructor used by the pure
    /// classifier when the extra context (reason,
    /// rule kind, command) is not yet known.
    pub fn new(
        repo_name: &str,
        classification: FetchClassification,
        materialized_path: Option<PathBuf>,
        has_build_file: bool,
    ) -> Self {
        Self {
            repo_name: repo_name.to_string(),
            classification,
            requested_reason: None,
            materialized_path,
            has_build_file,
            fetch_rule_kind: None,
            expected_command: None,
        }
    }
}

/// Phase 5.C.4 commit 140 (Batch R): full plan —
/// just a typed wrapper around `Vec<FetchPlanEntry>`
/// with helpers for filtering on classification.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchPlan {
    entries: Vec<FetchPlanEntry>,
}

impl FetchPlan {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, entry: FetchPlanEntry) {
        self.entries.push(entry);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[FetchPlanEntry] {
        &self.entries
    }

    /// Repos in this plan that the strategy
    /// executor will actually fetch. Filtered on
    /// `FetchClassification::needs_fetch()`.
    pub fn repos_to_fetch(&self) -> Vec<&FetchPlanEntry> {
        self.entries
            .iter()
            .filter(|e| e.classification.needs_fetch())
            .collect()
    }
}

/// Phase 5.C.4 commit 140 (Batch R): pure
/// classifier — given a workspace root, a repo
/// name, and the relevant flags, return the
/// `FetchPlanEntry` rush *would* produce.
///
/// No side effects. No mutex. No env var reads — the
/// caller passes `env_skip_materialized` and
/// `is_known_skip` explicitly so unit tests stay
/// hermetic.
///
/// The "vendored" path matches
/// `bzl_loader::vendored_external_repo_dir`:
/// `<workspace_root>/external/<repo_name>/`.
///
/// This function does NOT attempt to detect
/// `FailedProbe` — that classification is produced
/// by commit 141's combined-strategy executor when
/// a bazel capability probe fails. Pre-fetch
/// classification doesn't know what bazel can do;
/// it only knows what's on disk.
pub fn classify_repo_for_fetch_plan(
    workspace_root: &Path,
    repo_name: &str,
    env_skip_materialized: bool,
    is_known_skip: bool,
) -> FetchPlanEntry {
    let external_path =
        workspace_root.join("external").join(repo_name);
    let dir_exists = external_path.exists();
    let has_build = external_path.join("BUILD").exists()
        || external_path.join("BUILD.bazel").exists();

    let (classification, materialized_path) =
        if is_known_skip && dir_exists {
            // Pre-existing allowlist behavior:
            // dir-exists is enough; BUILD presence
            // is NOT required here because the
            // hand-maintained allowlist was tuned
            // for repos where rush provides its
            // own stub.
            (
                FetchClassification::SkippedKnownRule,
                Some(external_path.clone()),
            )
        } else if env_skip_materialized {
            if !dir_exists {
                (
                    FetchClassification::MissingDir,
                    Some(external_path.clone()),
                )
            } else if has_build {
                (
                    FetchClassification::SkippedMaterialized,
                    Some(external_path.clone()),
                )
            } else {
                (
                    FetchClassification::MissingBuild,
                    Some(external_path.clone()),
                )
            }
        } else {
            // Env off, not in allowlist → caller
            // will run a real bazel-fetch.
            (
                FetchClassification::NeedsFetch,
                Some(external_path.clone()),
            )
        };

    FetchPlanEntry::new(
        repo_name,
        classification,
        materialized_path,
        has_build,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build a temp workspace with optional
    /// external/<repo>/[BUILD] shape.
    fn make_tmp_workspace(
        repo: Option<(&str, bool)>,
    ) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = dir.path().to_path_buf();
        if let Some((name, has_build)) = repo {
            let p = ws.join("external").join(name);
            std::fs::create_dir_all(&p).expect("mkdir");
            if has_build {
                std::fs::write(p.join("BUILD"), b"# stub\n")
                    .expect("BUILD");
            }
        }
        (dir, ws)
    }

    #[test]
    fn materialized_repo_classified_correctly() {
        let (_g, ws) = make_tmp_workspace(Some(("foo", true)));
        let e = classify_repo_for_fetch_plan(&ws, "foo", true, false);
        assert_eq!(
            e.classification,
            FetchClassification::SkippedMaterialized
        );
        assert_eq!(e.repo_name, "foo");
        assert_eq!(
            e.materialized_path,
            Some(ws.join("external").join("foo"))
        );
        assert!(e.has_build_file);
        assert!(!e.classification.needs_fetch());
    }

    #[test]
    fn missing_repo_classified_needs_fetch() {
        let (_g, ws) = make_tmp_workspace(None);
        // env off, not in allowlist → NeedsFetch
        let e = classify_repo_for_fetch_plan(&ws, "absent", false, false);
        assert_eq!(e.classification, FetchClassification::NeedsFetch);
        assert!(e.classification.needs_fetch());
        assert!(!e.has_build_file);

        // env on, dir missing → MissingDir (also
        // categorized as needs_fetch by the
        // strategy executor)
        let e2 = classify_repo_for_fetch_plan(&ws, "absent", true, false);
        assert_eq!(e2.classification, FetchClassification::MissingDir);
        assert!(e2.classification.needs_fetch());
    }

    #[test]
    fn missing_build_classified_correctly() {
        // dir exists but no BUILD file, env on
        let (_g, ws) = make_tmp_workspace(Some(("foo", false)));
        let e = classify_repo_for_fetch_plan(&ws, "foo", true, false);
        assert_eq!(
            e.classification,
            FetchClassification::MissingBuild
        );
        assert!(!e.has_build_file);
        assert!(e.classification.needs_fetch());
    }

    #[test]
    fn known_skip_classified_correctly() {
        // dir exists, in allowlist, env irrelevant
        let (_g, ws) = make_tmp_workspace(Some(("foo", false)));
        let e = classify_repo_for_fetch_plan(&ws, "foo", false, true);
        assert_eq!(
            e.classification,
            FetchClassification::SkippedKnownRule
        );
        // SkippedKnownRule does NOT require BUILD —
        // the hand-maintained allowlist was tuned
        // for repos where rush provides its own
        // stub.
        assert!(!e.has_build_file);
        assert!(!e.classification.needs_fetch());

        // Same scenario with env on still goes
        // SkippedKnownRule (allowlist wins, it's
        // checked first).
        let e2 = classify_repo_for_fetch_plan(&ws, "foo", true, true);
        assert_eq!(
            e2.classification,
            FetchClassification::SkippedKnownRule
        );
    }

    #[test]
    fn fetch_plan_repos_to_fetch_filters_correctly() {
        let mut plan = FetchPlan::new();
        let p1 = PathBuf::from("/tmp/a");
        let p2 = PathBuf::from("/tmp/b");
        plan.push(FetchPlanEntry::new(
            "skip",
            FetchClassification::SkippedMaterialized,
            Some(p1.clone()),
            true,
        ));
        plan.push(FetchPlanEntry::new(
            "fetch",
            FetchClassification::NeedsFetch,
            Some(p2.clone()),
            false,
        ));
        plan.push(FetchPlanEntry::new(
            "missing",
            FetchClassification::MissingDir,
            Some(p1.clone()),
            false,
        ));
        let to_fetch = plan.repos_to_fetch();
        assert_eq!(to_fetch.len(), 2);
        let names: Vec<&str> = to_fetch
            .iter()
            .map(|e| e.repo_name.as_str())
            .collect();
        assert!(names.contains(&"fetch"));
        assert!(names.contains(&"missing"));
        assert!(!names.contains(&"skip"));
    }

    #[test]
    fn classification_as_str_matches_fetch_decision_discriminants() {
        // The string forms must match the discriminants
        // used by FetchDecision's report (commit 135) so
        // operators see the same vocabulary.
        assert_eq!(
            FetchClassification::SkippedMaterialized.as_str(),
            "skipped_materialized"
        );
        assert_eq!(
            FetchClassification::SkippedKnownRule.as_str(),
            "skipped_known_rule"
        );
        assert_eq!(
            FetchClassification::MissingDir.as_str(),
            "materialized_missing_dir"
        );
        assert_eq!(
            FetchClassification::MissingBuild.as_str(),
            "materialized_missing_build"
        );
        assert_eq!(
            FetchClassification::NeedsFetch.as_str(),
            "needs_fetch"
        );
        assert_eq!(
            FetchClassification::FailedProbe.as_str(),
            "failed_probe"
        );
    }
}
