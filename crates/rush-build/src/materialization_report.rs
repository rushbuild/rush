// Rush Build - Materialization Completeness Report
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Phase 5.C.4 Batch ABR-2 (commit 215): diagnostic checker
//! that classifies the materialization state of declared
//! source artifacts in a candidate target's transitive
//! closure.
//!
//! ## Why this exists
//!
//! The Batch AB-2 verifier failure on
//! `//tensorflow/core/platform:cpu_feature_guard` exposed a
//! gap in the commit-210 candidate scanner: it gated on
//! scheduler rule-kind eligibility, missing-tool checks, shape
//! support, and action-graph completeness — but **not** on
//! whether the declared source files referenced by loaded BUILD
//! rules actually existed on disk.
//!
//! Specifically: `@snappy//snappy:snappy` had its BUILD file
//! materialized in `.cache/rush/external/snappy/snappy/BUILD`,
//! but the three `.cc` source files the rule declares
//! (`snappy.cc`, `snappy-sinksource.cc`,
//! `snappy-stubs-internal.cc`) were absent. Rush loaded the
//! `cc_library`, dispatched it to the scheduler, and the
//! verifier correctly rejected the dispatch because
//! `build_legacy_command_specs` short-circuits to empty when
//! `self.srcs.is_empty()` (rules_cc.rs:653) — the rule has
//! `srcs` declared but they resolve to nothing because the
//! files don't exist.
//!
//! This is the new blocker class:
//! `ExternalRepoSourceMissing`. It is distinct from
//! `MissingTool` (a host tool is unusable) and
//! `ActionGraphIncomplete` (the action DAG can't find an
//! executable for a declared action). The candidate scanner
//! that consults this report (ABR-3) marks any candidate whose
//! closure contains an entry with status
//! [`MaterializationStatus::ExternalRepoSourceMissing`] (or
//! similar payload-incomplete variants) as **blocked** before
//! the verifier or executor ever sees it.
//!
//! ## Constraints
//!
//! Per Pro's 2026-05-20 AB-revised directive:
//!
//! > Do not repair @snappy in this lane. No fetch changes. No
//! > stubs. No materialized skip changes. No external repo
//! > repair. Only classify, gate, and choose a candidate that
//! > does not require incomplete external sources.
//!
//! This module is therefore **read-only**:
//!
//!   - It MUST NOT call any fetch path
//!     (`fetch_repo_with_bazel`, `register_cached_repo`,
//!     `ensure_known_repo_registered`, etc).
//!   - It MUST NOT create directories or write to the external
//!     repo cache.
//!   - It only inspects (`Path::exists` / `Path::is_dir`) the
//!     declared artifact paths and consults an in-memory
//!     producer lookup for generated inputs.
//!
//! ## How it slots into eligibility
//!
//! Today (ABR-2) this module only produces the report; the
//! orchestrator's
//! [`compute_scheduler_eligibility`](crate::build_orchestrator_ext::compute_scheduler_eligibility)
//! signature is unchanged. ABR-3 wires the report into the
//! candidate scanner by:
//!
//!   1. Collecting declared source artifacts per candidate
//!      (using the rule's `srcs`/`hdrs`/`deps` chain).
//!   2. Running [`check_materialization_completeness`] to
//!      classify each artifact.
//!   3. If [`MaterializationCompletenessReport::is_blocked`]
//!      returns `Some(reason)`, the candidate's
//!      `SchedulerEligibilityReason` becomes
//!      [`ExternalRepoSourceMissing`](crate::build_orchestrator_ext::SchedulerEligibilityReason::ExternalRepoSourceMissing)
//!      with that static reason. The candidate is rejected
//!      from execution.
//!
//! [`ExternalRepoSourceMissing`]: crate::build_orchestrator_ext::SchedulerEligibilityReason::ExternalRepoSourceMissing

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// What kind of input artifact this declaration refers to.
/// Determines how the path is resolved and what
/// materialization invariants apply.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum SourceKind {
    /// In-tree source file (workspace-local, not an external
    /// repo). Resolved against the workspace root. Missing →
    /// `SourceMissing`.
    LocalSource,
    /// External-repo source file (e.g.
    /// `@snappy//snappy:snappy.cc`). Resolved against the
    /// external repo's cache root. Missing → either
    /// `ExternalRepoSourceMissing` (BUILD present, .cc absent)
    /// or `ExternalRepoDirectoryMissing` (no repo dir on disk)
    /// or `ExternalRepoBuildExistsPayloadIncomplete` (mix of
    /// present and absent files within the same repo).
    ExternalRepoSource {
        /// Repo workspace name (e.g. `"snappy"`,
        /// `"com_google_absl"`).
        repo: String,
    },
    /// Generated input — produced by another action in the
    /// build graph, not a source-controlled file. Resolved via
    /// the producer lookup. No producer →
    /// `GeneratedInputMissingProducer`; has producer →
    /// `GeneratedInputWithProducer`.
    GeneratedInput,
}

/// The materialization classification for one declared source
/// artifact. Drives whether a candidate is blocked or
/// admitted.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MaterializationStatus {
    /// Source file exists on disk at the resolved path. The
    /// candidate is fine on this artifact.
    SourceExists,
    /// In-tree source file does NOT exist on disk. Workspace
    /// drift — typically a repo-level bug or a rename that
    /// missed a `srcs` entry.
    SourceMissing,
    /// External-repo source: BUILD file present, but the
    /// declared `.cc`/`.h` source file is absent. This is the
    /// `@snappy` class — what AB-2 hit.
    ExternalRepoSourceMissing,
    /// External-repo directory itself is missing. Distinct
    /// from `ExternalRepoSourceMissing`: the repo cache root
    /// doesn't exist at all (no BUILD, no payload).
    ExternalRepoDirectoryMissing,
    /// External-repo BUILD file exists and at least one
    /// declared source file is present, but at least one
    /// declared source file is absent. Within one repo, the
    /// payload is partial. Reported per-artifact for the
    /// absent files; present files use `SourceExists`.
    ExternalRepoBuildExistsPayloadIncomplete,
    /// Generated input has a producer action registered. Safe.
    GeneratedInputWithProducer,
    /// Generated input has NO producer action. The action DAG
    /// can't fulfill the input. Adjacent class to
    /// `ActionGraphIncomplete` but reported through
    /// materialization because the gap is provenance, not
    /// executable-action wiring.
    GeneratedInputMissingProducer,
    /// AJ2-4: external-repo source whose declared `.cc`/`.h`
    /// file is absent on disk BUT the repo is a known
    /// Rush-generated stub (system-library redirect, header-
    /// only, feature-disabled, etc). The absence is by design —
    /// the stub exists only to satisfy the WORKSPACE-level dep
    /// graph; the rule has no compilable payload. Classified
    /// as ADMITTED so the candidate scanner does not reject
    /// targets whose closure crosses a stub.
    ///
    /// The verifier (AJ2-3) and scheduler support-level (AJ2-2)
    /// independently exempt these targets from action-key and
    /// eligibility validation — this materialization variant
    /// gives the candidate scanner the same picture.
    IntentionalExternalStub {
        /// Stub kind label (e.g. `"system_library"`,
        /// `"header_only"`, `"feature_disabled"`,
        /// `"python_stub"`, `"misc"`). Stable across releases;
        /// matches [`ExternalRepoStubKind::label`] from
        /// `external_repo_materialization`.
        ///
        /// Named `stub_kind` (not `kind`) because the enum's
        /// serde tag is `"kind"`; the inner field cannot share
        /// the discriminator's JSON name.
        stub_kind: String,
    },
}

impl MaterializationStatus {
    /// True if this status admits the artifact for execution.
    /// `SourceExists`, `GeneratedInputWithProducer`, and
    /// `IntentionalExternalStub` admit; all other variants block.
    pub fn is_admitted(&self) -> bool {
        matches!(
            self,
            MaterializationStatus::SourceExists
                | MaterializationStatus::GeneratedInputWithProducer
                | MaterializationStatus::IntentionalExternalStub { .. }
        )
    }

    /// Static reason category for this status. Returns `None`
    /// for admitted variants. Used to populate
    /// `SchedulerEligibilityReason::ExternalRepoSourceMissing(&'static str)`.
    pub fn blocker_reason(&self) -> Option<&'static str> {
        match self {
            MaterializationStatus::SourceExists => None,
            MaterializationStatus::GeneratedInputWithProducer => None,
            MaterializationStatus::IntentionalExternalStub { .. } => None,
            MaterializationStatus::SourceMissing => Some("local source missing on disk"),
            MaterializationStatus::ExternalRepoSourceMissing => {
                Some("external repo source file missing")
            }
            MaterializationStatus::ExternalRepoDirectoryMissing => {
                Some("external repo directory not materialized")
            }
            MaterializationStatus::ExternalRepoBuildExistsPayloadIncomplete => {
                Some("external repo BUILD file present, source payload incomplete")
            }
            MaterializationStatus::GeneratedInputMissingProducer => {
                Some("generated input has no producer in action graph")
            }
        }
    }
}

/// One declared input artifact to be classified by the
/// checker. Inputs are produced by traversing a candidate's
/// transitive closure: for each target, every `srcs`/`hdrs`
/// entry (in-tree or external) becomes one declaration, and
/// every declared generated input not produced by the target
/// itself becomes one declaration with kind `GeneratedInput`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DeclaredSourceArtifact {
    /// Bazel-style label of the target that declared this
    /// input (e.g. `"//tensorflow/core/platform:cpu_feature_guard"`
    /// or `"@snappy//snappy:snappy"`).
    pub target: String,
    /// Repo workspace name. `None` for in-tree workspace;
    /// `Some("snappy")` for external repo entries. Mirrors
    /// [`SourceKind::ExternalRepoSource::repo`] for external
    /// declarations.
    pub repo: Option<String>,
    /// Package path within the repo (e.g.
    /// `"tensorflow/core/platform"` or `"snappy"`).
    pub package: String,
    /// On-disk path the artifact is expected at. For external
    /// repos this is the joined cache path (e.g.
    /// `.cache/rush/external/snappy/snappy/snappy.cc`).
    pub artifact_path: PathBuf,
    /// Rule kind that declared this artifact (e.g.
    /// `"cc_library"`, `"tf_proto_library"`).
    pub rule_kind: String,
    /// What category of input this is (local / external /
    /// generated). Determines resolution rules.
    pub source_kind: SourceKind,
}

/// One classified entry. Pairs a [`DeclaredSourceArtifact`]
/// with the checker's verdict.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MaterializationEntry {
    pub artifact: DeclaredSourceArtifact,
    pub status: MaterializationStatus,
    /// Human-readable detail for the report. For admitted
    /// entries, a short OK summary; for blocked entries, the
    /// blocker reason plus the absolute path the check was
    /// performed against.
    pub detail: String,
}

/// Schema version for the JSON wire shape of
/// [`MaterializationCompletenessReport`]. Bumped on breaking
/// changes (field renames, removals). Additive fields keep the
/// version stable.
pub const MATERIALIZATION_REPORT_SCHEMA_VERSION: u32 = 1;

/// The completeness verdict for a candidate's full transitive
/// closure of declared inputs.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MaterializationCompletenessReport {
    pub schema_version: u32,
    pub entries: Vec<MaterializationEntry>,
}

impl MaterializationCompletenessReport {
    pub fn new() -> Self {
        Self {
            schema_version: MATERIALIZATION_REPORT_SCHEMA_VERSION,
            entries: Vec::new(),
        }
    }

    /// True iff every entry is admitted. Equivalent to
    /// `self.is_blocked().is_none()`.
    pub fn all_admitted(&self) -> bool {
        self.entries.iter().all(|e| e.status.is_admitted())
    }

    /// If any entry is blocked, return a static reason
    /// suitable for
    /// `SchedulerEligibilityReason::ExternalRepoSourceMissing`.
    /// The first blocker's reason wins (entries are scanned in
    /// declaration order, so this is stable). Returns `None`
    /// when all entries are admitted.
    ///
    /// For pluralized blocker mixes the reason names the
    /// *first* blocker — operators get the full list via
    /// [`Self::blocking_entries`].
    pub fn is_blocked(&self) -> Option<&'static str> {
        self.entries
            .iter()
            .find_map(|e| e.status.blocker_reason())
    }

    /// Every entry whose status is not admitted, in declaration
    /// order.
    pub fn blocking_entries(&self) -> impl Iterator<Item = &MaterializationEntry> {
        self.entries.iter().filter(|e| !e.status.is_admitted())
    }

    /// Count of entries grouped by repo name (`None` for
    /// in-tree). Used by the dry-run report (AD-revised) to
    /// rank which external repos block the most candidates.
    pub fn blockers_by_repo(&self) -> HashMap<Option<String>, usize> {
        let mut out: HashMap<Option<String>, usize> = HashMap::new();
        for e in self.blocking_entries() {
            *out.entry(e.artifact.repo.clone()).or_insert(0) += 1;
        }
        out
    }
}

impl Default for MaterializationCompletenessReport {
    fn default() -> Self {
        Self::new()
    }
}

/// Phase 5.C.4 Batch ABR-3 (commit 216): canonical
/// "materialization blocker reason" for one candidate.
///
/// This is the bridge between the
/// [`MaterializationCompletenessReport`] (a per-artifact
/// classification) and the candidate scanner's eligibility
/// gate (a per-target verdict). The candidate scanner calls
/// this on the report it built for the candidate's transitive
/// closure; the returned `Option<&'static str>` flows
/// directly into
/// [`compute_scheduler_eligibility_with_materialization`](crate::build_orchestrator_ext::compute_scheduler_eligibility_with_materialization)'s
/// `external_source_missing` parameter.
///
/// Returns `None` when every entry in the report is
/// admitted; `Some(reason)` when at least one entry blocks.
///
/// The returned reason is the FIRST blocker's static
/// category (entries are scanned in declaration order). For
/// reports with multiple distinct blocker classes, operators
/// still see the full enumeration via
/// [`MaterializationCompletenessReport::blocking_entries`]
/// in the dry-run report (AD-revised).
pub fn candidate_materialization_blocker(
    report: &MaterializationCompletenessReport,
) -> Option<&'static str> {
    report.is_blocked()
}

/// Trait abstracting file-system existence checks so the
/// checker is unit-testable without touching real disk. The
/// production implementation [`RealFs`] is a thin wrapper
/// over `Path::exists` / `Path::is_dir`.
pub trait MaterializationFs {
    fn path_exists(&self, path: &Path) -> bool;
    fn dir_exists(&self, path: &Path) -> bool;
}

/// Production filesystem accessor.
pub struct RealFs;

impl MaterializationFs for RealFs {
    fn path_exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn dir_exists(&self, path: &Path) -> bool {
        path.is_dir()
    }
}

/// Trait abstracting "does any action produce this artifact"
/// lookup. The production implementation walks
/// [`rush_action::ids::ArtifactRegistry::producer`]; tests use
/// a simple `HashSet<PathBuf>` of "known producers."
pub trait ProducerLookup {
    fn has_producer(&self, artifact_path: &Path) -> bool;
}

/// `HashMap`-backed [`ProducerLookup`] for tests and dry-run
/// callers that have already enumerated the producer set.
pub struct PathSetProducers<'a>(pub &'a std::collections::HashSet<PathBuf>);

impl<'a> ProducerLookup for PathSetProducers<'a> {
    fn has_producer(&self, artifact_path: &Path) -> bool {
        self.0.contains(artifact_path)
    }
}

/// AJ2-4: trait abstracting "is this external repo a known
/// Rush-generated stub?" lookup. Implementations resolve a
/// workspace repo name (e.g. `"snappy"`, `"com_google_absl"`)
/// to its stub kind label if and only if the repo's BUILD file
/// carries one of the stub markers detected by
/// [`detect_stub_marker`](crate::external_repo_materialization::detect_stub_marker).
///
/// Returning `Some(kind)` causes
/// [`check_materialization_completeness_with_stubs`] to emit
/// [`MaterializationStatus::IntentionalExternalStub`] (admitted)
/// in place of [`MaterializationStatus::ExternalRepoSourceMissing`]
/// (blocked) for declared sources that are absent because the
/// repo is a stub redirecting to a system library / disabled
/// feature / header-only path.
pub trait StubLookup {
    /// Returns the stub kind label (e.g. `"system_library"`,
    /// `"header_only"`, `"feature_disabled"`) for `repo` when
    /// it is a known stub; otherwise `None`.
    fn repo_stub_kind(&self, repo: &str) -> Option<String>;
}

/// No-op [`StubLookup`] used by the original
/// [`check_materialization_completeness`] signature and by
/// callers that do not yet have stub knowledge plumbed in.
/// Behaves as if no repo is a stub.
pub struct NoStubLookup;

impl StubLookup for NoStubLookup {
    fn repo_stub_kind(&self, _repo: &str) -> Option<String> {
        None
    }
}

/// `HashMap`-backed [`StubLookup`] for tests and callers that
/// have already classified a set of repos. Keys are repo
/// workspace names (e.g. `"snappy"`); values are stub kind
/// labels.
pub struct MapStubLookup<'a>(pub &'a HashMap<String, String>);

impl<'a> StubLookup for MapStubLookup<'a> {
    fn repo_stub_kind(&self, repo: &str) -> Option<String> {
        self.0.get(repo).cloned()
    }
}

/// Classify all declared source artifacts in `artifacts`.
/// Returns a [`MaterializationCompletenessReport`] whose
/// entries appear in the same order as the input slice.
///
/// Resolution rules (per [`SourceKind`]):
///
///   - `LocalSource`: the artifact's `artifact_path` is
///     expected to exist as a file. Missing → `SourceMissing`.
///   - `ExternalRepoSource { repo }`: look up `repo` in
///     `repo_paths`. If the repo entry is absent or its
///     directory doesn't exist on disk →
///     `ExternalRepoDirectoryMissing`. Otherwise check the
///     artifact path. Missing → `ExternalRepoSourceMissing`.
///   - `GeneratedInput`: consult `producers`. Has producer →
///     `GeneratedInputWithProducer`; otherwise →
///     `GeneratedInputMissingProducer`.
///
/// **Important:** this function never calls fetch or
/// otherwise mutates external-repo state. It only inspects
/// what is already on disk and what the producer lookup
/// already knows.
///
/// The post-pass at the end of this function inspects each
/// repo's set of external entries and, when a repo contains
/// both present and absent declared sources, upgrades the
/// absent entries' status from `ExternalRepoSourceMissing` to
/// `ExternalRepoBuildExistsPayloadIncomplete`. This
/// distinction matters for operator triage: a
/// fully-unmaterialized repo (no BUILD, nothing) is a
/// different remediation than a partially-materialized one
/// (BUILD plus some files but missing others).
pub fn check_materialization_completeness(
    artifacts: &[DeclaredSourceArtifact],
    repo_paths: &HashMap<String, PathBuf>,
    fs: &dyn MaterializationFs,
    producers: &dyn ProducerLookup,
) -> MaterializationCompletenessReport {
    check_materialization_completeness_with_stubs(
        artifacts,
        repo_paths,
        fs,
        producers,
        &NoStubLookup,
    )
}

/// AJ2-4 variant of [`check_materialization_completeness`] that
/// also consults a [`StubLookup`]. When an external-repo
/// source's `.cc`/`.h` is absent on disk **and** the repo is a
/// known Rush-generated stub, the entry is classified as
/// [`MaterializationStatus::IntentionalExternalStub`] (admitted)
/// instead of [`MaterializationStatus::ExternalRepoSourceMissing`]
/// (blocked).
///
/// The candidate scanner uses this variant so it does not
/// reject targets whose closure crosses a stub. The original
/// signature (without `stubs`) is preserved for callers that
/// have not yet plumbed stub knowledge — it behaves as if every
/// repo is non-stub.
///
/// Resolution rules match the original function with one
/// addition: at the external-source-absent branch, if
/// `stubs.repo_stub_kind(repo)` returns `Some(kind)`, the
/// classification is `IntentionalExternalStub { stub_kind }` and the
/// per-repo absent counter is NOT incremented (so the payload-
/// incomplete post-pass treats the stub neutrally).
pub fn check_materialization_completeness_with_stubs(
    artifacts: &[DeclaredSourceArtifact],
    repo_paths: &HashMap<String, PathBuf>,
    fs: &dyn MaterializationFs,
    producers: &dyn ProducerLookup,
    stubs: &dyn StubLookup,
) -> MaterializationCompletenessReport {
    let mut entries: Vec<MaterializationEntry> = Vec::with_capacity(artifacts.len());
    // Track per-repo presence/absence so we can post-pass
    // upgrade absent-but-some-present entries to
    // ExternalRepoBuildExistsPayloadIncomplete.
    let mut per_repo_present: HashMap<String, bool> = HashMap::new();
    let mut per_repo_absent: HashMap<String, bool> = HashMap::new();

    for art in artifacts {
        let (status, detail) = match &art.source_kind {
            SourceKind::LocalSource => {
                if fs.path_exists(&art.artifact_path) {
                    (
                        MaterializationStatus::SourceExists,
                        format!(
                            "local source present at {}",
                            art.artifact_path.display()
                        ),
                    )
                } else {
                    (
                        MaterializationStatus::SourceMissing,
                        format!(
                            "local source missing at {}",
                            art.artifact_path.display()
                        ),
                    )
                }
            }
            SourceKind::ExternalRepoSource { repo } => {
                let repo_root = repo_paths.get(repo);
                match repo_root {
                    None => (
                        MaterializationStatus::ExternalRepoDirectoryMissing,
                        format!(
                            "external repo `{}` not registered in repo_paths \
                             (declared artifact path: {})",
                            repo,
                            art.artifact_path.display()
                        ),
                    ),
                    Some(root) if !fs.dir_exists(root) => (
                        MaterializationStatus::ExternalRepoDirectoryMissing,
                        format!(
                            "external repo `{}` directory absent at {} \
                             (declared artifact path: {})",
                            repo,
                            root.display(),
                            art.artifact_path.display()
                        ),
                    ),
                    Some(_) => {
                        if fs.path_exists(&art.artifact_path) {
                            *per_repo_present.entry(repo.clone()).or_insert(false) =
                                true;
                            (
                                MaterializationStatus::SourceExists,
                                format!(
                                    "external repo `{}` source present at {}",
                                    repo,
                                    art.artifact_path.display()
                                ),
                            )
                        } else if let Some(kind) = stubs.repo_stub_kind(repo) {
                            // AJ2-4: known Rush stub. Absence is
                            // by design — admit the entry and
                            // do NOT contribute to per-repo
                            // absent tracking (the payload-
                            // incomplete upgrade is irrelevant
                            // for stubs).
                            (
                                MaterializationStatus::IntentionalExternalStub {
                                    stub_kind: kind.clone(),
                                },
                                format!(
                                    "external repo `{}` is a Rush stub ({}); \
                                     declared source legitimately absent at {}",
                                    repo,
                                    kind,
                                    art.artifact_path.display()
                                ),
                            )
                        } else {
                            *per_repo_absent.entry(repo.clone()).or_insert(false) =
                                true;
                            (
                                MaterializationStatus::ExternalRepoSourceMissing,
                                format!(
                                    "external repo `{}` BUILD/dir present, \
                                     source absent at {}",
                                    repo,
                                    art.artifact_path.display()
                                ),
                            )
                        }
                    }
                }
            }
            SourceKind::GeneratedInput => {
                if producers.has_producer(&art.artifact_path) {
                    (
                        MaterializationStatus::GeneratedInputWithProducer,
                        format!(
                            "generated input {} has producer action",
                            art.artifact_path.display()
                        ),
                    )
                } else {
                    (
                        MaterializationStatus::GeneratedInputMissingProducer,
                        format!(
                            "generated input {} has NO producer action",
                            art.artifact_path.display()
                        ),
                    )
                }
            }
        };
        entries.push(MaterializationEntry {
            artifact: art.clone(),
            status,
            detail,
        });
    }

    // Post-pass: where a repo has both present AND absent
    // declared sources, upgrade the absent entries from
    // ExternalRepoSourceMissing to
    // ExternalRepoBuildExistsPayloadIncomplete. Operator
    // triage uses the distinction to decide whether the repo
    // needs a full re-fetch or a partial payload fix.
    for entry in entries.iter_mut() {
        if let MaterializationStatus::ExternalRepoSourceMissing = entry.status {
            if let SourceKind::ExternalRepoSource { repo } = &entry.artifact.source_kind {
                let has_present = per_repo_present.get(repo).copied().unwrap_or(false);
                let has_absent = per_repo_absent.get(repo).copied().unwrap_or(false);
                if has_present && has_absent {
                    entry.status =
                        MaterializationStatus::ExternalRepoBuildExistsPayloadIncomplete;
                    entry.detail = format!(
                        "external repo `{}` payload incomplete \
                         (some declared sources present, this one absent at {})",
                        repo,
                        entry.artifact.artifact_path.display()
                    );
                }
            }
        }
    }

    MaterializationCompletenessReport {
        schema_version: MATERIALIZATION_REPORT_SCHEMA_VERSION,
        entries,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;

    /// In-memory filesystem oracle: a path "exists" iff the
    /// caller listed it. `dirs` is the set of paths that
    /// answer `true` to `dir_exists`.
    struct FakeFs {
        files: HashSet<PathBuf>,
        dirs: HashSet<PathBuf>,
    }

    impl FakeFs {
        fn new() -> Self {
            Self {
                files: HashSet::new(),
                dirs: HashSet::new(),
            }
        }
        fn with_file(mut self, p: impl Into<PathBuf>) -> Self {
            self.files.insert(p.into());
            self
        }
        fn with_dir(mut self, p: impl Into<PathBuf>) -> Self {
            self.dirs.insert(p.into());
            self
        }
    }

    impl MaterializationFs for FakeFs {
        fn path_exists(&self, p: &Path) -> bool {
            self.files.contains(p) || self.dirs.contains(p)
        }
        fn dir_exists(&self, p: &Path) -> bool {
            self.dirs.contains(p)
        }
    }

    fn art_local(target: &str, path: &str) -> DeclaredSourceArtifact {
        DeclaredSourceArtifact {
            target: target.to_string(),
            repo: None,
            package: "pkg".to_string(),
            artifact_path: PathBuf::from(path),
            rule_kind: "cc_library".to_string(),
            source_kind: SourceKind::LocalSource,
        }
    }

    fn art_external(
        target: &str,
        repo: &str,
        path: &str,
    ) -> DeclaredSourceArtifact {
        DeclaredSourceArtifact {
            target: target.to_string(),
            repo: Some(repo.to_string()),
            package: format!("{}/pkg", repo),
            artifact_path: PathBuf::from(path),
            rule_kind: "cc_library".to_string(),
            source_kind: SourceKind::ExternalRepoSource {
                repo: repo.to_string(),
            },
        }
    }

    fn art_generated(target: &str, path: &str) -> DeclaredSourceArtifact {
        DeclaredSourceArtifact {
            target: target.to_string(),
            repo: None,
            package: "pkg".to_string(),
            artifact_path: PathBuf::from(path),
            rule_kind: "cc_library".to_string(),
            source_kind: SourceKind::GeneratedInput,
        }
    }

    /// ABR-2 directive test #1 (Pro's spec): an external repo
    /// whose BUILD file is materialized but whose declared
    /// `.cc` sources are absent must be classified as
    /// `ExternalRepoSourceMissing` and the report must
    /// report a blocker. This is the @snappy case.
    #[test]
    fn external_repo_build_exists_but_source_missing_is_blocked() {
        let mut repo_paths = HashMap::new();
        let repo_root = PathBuf::from("/cache/external/snappy");
        repo_paths.insert("snappy".to_string(), repo_root.clone());
        // The repo dir exists (BUILD present), but the
        // declared source file does not.
        let fs = FakeFs::new().with_dir(repo_root);
        let producers_set: HashSet<PathBuf> = HashSet::new();
        let producers = PathSetProducers(&producers_set);

        let artifacts = vec![art_external(
            "@snappy//snappy:snappy",
            "snappy",
            "/cache/external/snappy/snappy/snappy.cc",
        )];

        let report = check_materialization_completeness(
            &artifacts,
            &repo_paths,
            &fs,
            &producers,
        );
        assert!(
            !report.all_admitted(),
            "report should be blocked when external .cc is missing"
        );
        let blocker = report.is_blocked().expect("expected a blocker reason");
        assert_eq!(blocker, "external repo source file missing");
        assert_eq!(report.entries.len(), 1);
        assert_eq!(
            report.entries[0].status,
            MaterializationStatus::ExternalRepoSourceMissing
        );
        // Per-repo breakdown should attribute the blocker to
        // snappy.
        let by_repo = report.blockers_by_repo();
        assert_eq!(by_repo.get(&Some("snappy".to_string())).copied(), Some(1));
    }

    /// ABR-2 directive test #2 (Pro's spec): in-tree source
    /// that exists on disk passes the checker.
    #[test]
    fn local_source_exists_passes() {
        let path = "/workspace/tf/core/platform/cpu_feature_guard.cc";
        let fs = FakeFs::new().with_file(path);
        let producers_set: HashSet<PathBuf> = HashSet::new();
        let producers = PathSetProducers(&producers_set);
        let repo_paths: HashMap<String, PathBuf> = HashMap::new();

        let artifacts = vec![art_local(
            "//tensorflow/core/platform:cpu_feature_guard",
            path,
        )];
        let report = check_materialization_completeness(
            &artifacts,
            &repo_paths,
            &fs,
            &producers,
        );
        assert!(report.all_admitted(), "in-tree present source should pass");
        assert!(report.is_blocked().is_none());
        assert_eq!(
            report.entries[0].status,
            MaterializationStatus::SourceExists
        );
    }

    /// ABR-2 directive test #3 (Pro's spec): a generated input
    /// that has a registered producer action passes.
    #[test]
    fn generated_input_with_producer_passes() {
        let path = "/genfiles/proto/foo.pb.cc";
        let fs = FakeFs::new();
        let mut producers_set: HashSet<PathBuf> = HashSet::new();
        producers_set.insert(PathBuf::from(path));
        let producers = PathSetProducers(&producers_set);
        let repo_paths: HashMap<String, PathBuf> = HashMap::new();

        let artifacts = vec![art_generated("//pkg:bar", path)];
        let report = check_materialization_completeness(
            &artifacts,
            &repo_paths,
            &fs,
            &producers,
        );
        assert!(report.all_admitted());
        assert!(report.is_blocked().is_none());
        assert_eq!(
            report.entries[0].status,
            MaterializationStatus::GeneratedInputWithProducer
        );
    }

    /// ABR-2 directive test #4 (Pro's spec): a generated input
    /// with no registered producer fails the checker with the
    /// `GeneratedInputMissingProducer` blocker.
    #[test]
    fn missing_generated_producer_fails() {
        let path = "/genfiles/proto/missing.pb.cc";
        let fs = FakeFs::new();
        let producers_set: HashSet<PathBuf> = HashSet::new();
        let producers = PathSetProducers(&producers_set);
        let repo_paths: HashMap<String, PathBuf> = HashMap::new();

        let artifacts = vec![art_generated("//pkg:bar", path)];
        let report = check_materialization_completeness(
            &artifacts,
            &repo_paths,
            &fs,
            &producers,
        );
        assert!(!report.all_admitted());
        let blocker = report.is_blocked().expect("expected blocker");
        assert_eq!(
            blocker,
            "generated input has no producer in action graph"
        );
        assert_eq!(
            report.entries[0].status,
            MaterializationStatus::GeneratedInputMissingProducer
        );
    }

    /// External repo directory missing entirely (no BUILD, no
    /// payload) is reported distinctly from the
    /// "BUILD-present-source-absent" case.
    #[test]
    fn external_repo_directory_missing_is_distinct_from_source_missing() {
        let repo_paths: HashMap<String, PathBuf> = HashMap::new();
        let fs = FakeFs::new();
        let producers_set: HashSet<PathBuf> = HashSet::new();
        let producers = PathSetProducers(&producers_set);

        let artifacts = vec![art_external(
            "@nonexistent//pkg:src",
            "nonexistent",
            "/cache/external/nonexistent/pkg/src.cc",
        )];
        let report = check_materialization_completeness(
            &artifacts,
            &repo_paths,
            &fs,
            &producers,
        );
        assert!(!report.all_admitted());
        assert_eq!(
            report.entries[0].status,
            MaterializationStatus::ExternalRepoDirectoryMissing
        );
        assert_eq!(
            report.is_blocked(),
            Some("external repo directory not materialized")
        );
    }

    /// When a repo has BOTH a present and an absent declared
    /// source, the absent entry must be upgraded from
    /// `ExternalRepoSourceMissing` to
    /// `ExternalRepoBuildExistsPayloadIncomplete`. Operator
    /// triage relies on the distinction: a fully-absent repo
    /// needs re-fetch, a partially-present one needs payload
    /// reconciliation.
    #[test]
    fn payload_incomplete_upgrade_when_repo_has_both_present_and_absent() {
        let mut repo_paths = HashMap::new();
        let root = PathBuf::from("/cache/external/snappy");
        repo_paths.insert("snappy".to_string(), root.clone());
        let present = "/cache/external/snappy/snappy/snappy.cc";
        let absent = "/cache/external/snappy/snappy/snappy-sinksource.cc";
        let fs = FakeFs::new().with_dir(root).with_file(present);
        let producers_set: HashSet<PathBuf> = HashSet::new();
        let producers = PathSetProducers(&producers_set);

        let artifacts = vec![
            art_external("@snappy//snappy:snappy", "snappy", present),
            art_external("@snappy//snappy:snappy", "snappy", absent),
        ];
        let report = check_materialization_completeness(
            &artifacts,
            &repo_paths,
            &fs,
            &producers,
        );
        // First entry is present.
        assert_eq!(
            report.entries[0].status,
            MaterializationStatus::SourceExists
        );
        // Second entry must be upgraded to payload-incomplete.
        assert_eq!(
            report.entries[1].status,
            MaterializationStatus::ExternalRepoBuildExistsPayloadIncomplete
        );
        assert_eq!(
            report.is_blocked(),
            Some("external repo BUILD file present, source payload incomplete")
        );
    }

    /// Report shape: schema version is stable at 1 and the
    /// JSON has the expected top-level fields.
    #[test]
    fn report_schema_version_is_stable() {
        let report = MaterializationCompletenessReport::new();
        assert_eq!(report.schema_version, 1);
        let json = serde_json::to_string(&report).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["schema_version"], 1);
        assert!(parsed["entries"].is_array());
    }

    /// In-tree source missing on disk reports as
    /// `SourceMissing` (not `ExternalRepoSourceMissing`).
    /// Workspace drift class.
    #[test]
    fn local_source_missing_reports_distinctly() {
        let fs = FakeFs::new();
        let producers_set: HashSet<PathBuf> = HashSet::new();
        let producers = PathSetProducers(&producers_set);
        let repo_paths: HashMap<String, PathBuf> = HashMap::new();
        let artifacts =
            vec![art_local("//pkg:missing_local", "/workspace/missing.cc")];
        let report = check_materialization_completeness(
            &artifacts,
            &repo_paths,
            &fs,
            &producers,
        );
        assert_eq!(
            report.entries[0].status,
            MaterializationStatus::SourceMissing
        );
        assert_eq!(report.is_blocked(), Some("local source missing on disk"));
    }

    /// Phase 5.C.4 Batch ABR-3 (commit 216) directive test #1
    /// (Pro's spec): a candidate whose closure contains an
    /// external repo source whose payload is missing is
    /// REJECTED by the materialization gate. The gate returns
    /// a blocker reason that flows into the
    /// SchedulerEligibilityReason::ExternalRepoSourceMissing
    /// variant. This is the @snappy / cpu_feature_guard case.
    #[test]
    fn candidate_with_missing_external_source_rejected() {
        let mut repo_paths = HashMap::new();
        let repo_root = PathBuf::from("/cache/external/snappy");
        repo_paths.insert("snappy".to_string(), repo_root.clone());
        let fs = FakeFs::new().with_dir(repo_root);
        let producers_set: HashSet<PathBuf> = HashSet::new();
        let producers = PathSetProducers(&producers_set);
        // Simulate the closure for cpu_feature_guard: one local
        // source (cpu_feature_guard.cc, present) + three
        // external snappy sources (.cc files, NOT present on
        // disk). The candidate is the local target; the
        // external dep is the source of the blocker.
        let artifacts = vec![
            art_local(
                "//tensorflow/core/platform:cpu_feature_guard",
                "/workspace/tf/core/platform/cpu_feature_guard.cc",
            ),
            art_external(
                "@snappy//snappy:snappy",
                "snappy",
                "/cache/external/snappy/snappy/snappy.cc",
            ),
            art_external(
                "@snappy//snappy:snappy",
                "snappy",
                "/cache/external/snappy/snappy/snappy-sinksource.cc",
            ),
            art_external(
                "@snappy//snappy:snappy",
                "snappy",
                "/cache/external/snappy/snappy/snappy-stubs-internal.cc",
            ),
        ];
        // The local cpu_feature_guard.cc isn't actually present
        // in this test (fs only has /cache/external/snappy as a
        // dir), so the first entry will be SourceMissing too;
        // that's fine — it still demonstrates the gate firing
        // on missing-source. To isolate the external-source
        // case, add the local file to the fs.
        let fs = fs.with_file("/workspace/tf/core/platform/cpu_feature_guard.cc");
        let report = check_materialization_completeness(
            &artifacts,
            &repo_paths,
            &fs,
            &producers,
        );
        // The local source admits.
        assert_eq!(
            report.entries[0].status,
            MaterializationStatus::SourceExists
        );
        // All three external sources are blocked.
        for i in 1..=3 {
            assert_eq!(
                report.entries[i].status,
                MaterializationStatus::ExternalRepoSourceMissing,
                "entry {} should be ExternalRepoSourceMissing",
                i
            );
        }
        // Candidate gate returns a blocker reason — exactly
        // what `compute_scheduler_eligibility_with_materialization`'s
        // `external_source_missing` parameter consumes.
        let blocker = candidate_materialization_blocker(&report)
            .expect("candidate should be blocked");
        assert_eq!(blocker, "external repo source file missing");
        // Per-repo breakdown attributes 3 blockers to snappy.
        let by_repo = report.blockers_by_repo();
        assert_eq!(by_repo.get(&Some("snappy".to_string())).copied(), Some(3));
        // No blockers attributed to local (None) repo.
        assert_eq!(by_repo.get(&None).copied(), None);
    }

    /// Phase 5.C.4 Batch ABR-3 (commit 216) directive test #2
    /// (Pro's spec): a candidate whose closure has all source
    /// artifacts materialized (in-tree present, external repo
    /// present, generated inputs with producers) is ACCEPTED
    /// by the materialization gate. The gate returns None,
    /// which signals
    /// `compute_scheduler_eligibility_with_materialization` to
    /// continue down the priority chain (NoDeclaredActions,
    /// ActionGraphIncomplete, RuleKindVerified).
    #[test]
    fn candidate_with_complete_sources_accepted() {
        let mut repo_paths = HashMap::new();
        let repo_root = PathBuf::from("/cache/external/zlib");
        repo_paths.insert("zlib".to_string(), repo_root.clone());
        // Generated producer set.
        let mut producers_set: HashSet<PathBuf> = HashSet::new();
        producers_set.insert(PathBuf::from("/genfiles/proto/foo.pb.cc"));
        let producers = PathSetProducers(&producers_set);
        // Build the fs with every declared file present.
        let local_path = "/workspace/tf/core/platform/byte_order.cc";
        let external_path = "/cache/external/zlib/zlib/zlib.c";
        let generated_path = "/genfiles/proto/foo.pb.cc";
        let fs = FakeFs::new()
            .with_dir(repo_root)
            .with_file(local_path)
            .with_file(external_path)
            .with_file(generated_path);
        let artifacts = vec![
            art_local("//pkg:byte_order", local_path),
            art_external("@zlib//zlib:zlib", "zlib", external_path),
            art_generated("//pkg:byte_order", generated_path),
        ];
        let report = check_materialization_completeness(
            &artifacts,
            &repo_paths,
            &fs,
            &producers,
        );
        assert!(
            report.all_admitted(),
            "all three artifacts should be admitted"
        );
        assert_eq!(report.entries[0].status, MaterializationStatus::SourceExists);
        assert_eq!(report.entries[1].status, MaterializationStatus::SourceExists);
        assert_eq!(
            report.entries[2].status,
            MaterializationStatus::GeneratedInputWithProducer
        );
        // Candidate gate returns None — no blocker, candidate
        // continues down the eligibility chain.
        assert_eq!(candidate_materialization_blocker(&report), None);
    }

    // ====================================================================
    // AJ2-4: stub-aware candidate scanner
    // ====================================================================

    /// AJ2-4: when the external repo is a known stub, an absent
    /// declared source is classified as
    /// `IntentionalExternalStub` (admitted) instead of
    /// `ExternalRepoSourceMissing` (blocked). The candidate
    /// scanner therefore admits the closure.
    #[test]
    fn aj2_4_stub_repo_absent_source_admitted_via_stub_lookup() {
        let mut repo_paths = HashMap::new();
        let repo_root = PathBuf::from("/cache/external/snappy");
        repo_paths.insert("snappy".to_string(), repo_root.clone());
        // BUILD-only repo: dir exists, source missing.
        let fs = FakeFs::new().with_dir(repo_root);
        let producers_set: HashSet<PathBuf> = HashSet::new();
        let producers = PathSetProducers(&producers_set);
        // Stub lookup knows @snappy is a system_library stub.
        let mut stub_map: HashMap<String, String> = HashMap::new();
        stub_map.insert("snappy".to_string(), "system_library".to_string());
        let stubs = MapStubLookup(&stub_map);

        let artifacts = vec![art_external(
            "@snappy//snappy:snappy",
            "snappy",
            "/cache/external/snappy/snappy/snappy.cc",
        )];

        let report = check_materialization_completeness_with_stubs(
            &artifacts,
            &repo_paths,
            &fs,
            &producers,
            &stubs,
        );

        assert!(
            report.all_admitted(),
            "stub repo absence should be admitted, not blocked"
        );
        assert!(report.is_blocked().is_none());
        assert_eq!(
            report.entries[0].status,
            MaterializationStatus::IntentionalExternalStub {
                stub_kind: "system_library".to_string()
            }
        );
        // Candidate gate sees no blocker.
        assert_eq!(candidate_materialization_blocker(&report), None);
        // The stub does NOT show up in the per-repo blocker
        // aggregation.
        let by_repo = report.blockers_by_repo();
        assert_eq!(by_repo.get(&Some("snappy".to_string())).copied(), None);
    }

    /// AJ2-4: the original (non-`_with_stubs`) signature stays
    /// strict — without a stub lookup, an absent stub-repo
    /// source still classifies as
    /// `ExternalRepoSourceMissing`. Existing callers that do
    /// not yet plumb stub knowledge see no behavior change.
    #[test]
    fn aj2_4_original_signature_unchanged_no_stub_lookup() {
        let mut repo_paths = HashMap::new();
        let repo_root = PathBuf::from("/cache/external/snappy");
        repo_paths.insert("snappy".to_string(), repo_root.clone());
        let fs = FakeFs::new().with_dir(repo_root);
        let producers_set: HashSet<PathBuf> = HashSet::new();
        let producers = PathSetProducers(&producers_set);

        let artifacts = vec![art_external(
            "@snappy//snappy:snappy",
            "snappy",
            "/cache/external/snappy/snappy/snappy.cc",
        )];

        let report = check_materialization_completeness(
            &artifacts,
            &repo_paths,
            &fs,
            &producers,
        );
        assert!(!report.all_admitted());
        assert_eq!(
            report.entries[0].status,
            MaterializationStatus::ExternalRepoSourceMissing
        );
    }

    /// AJ2-4: NoStubLookup behaves like a no-op — every repo is
    /// classified as non-stub. Equivalent to the original
    /// signature.
    #[test]
    fn aj2_4_no_stub_lookup_is_equivalent_to_strict_path() {
        let mut repo_paths = HashMap::new();
        let repo_root = PathBuf::from("/cache/external/snappy");
        repo_paths.insert("snappy".to_string(), repo_root.clone());
        let fs = FakeFs::new().with_dir(repo_root);
        let producers_set: HashSet<PathBuf> = HashSet::new();
        let producers = PathSetProducers(&producers_set);

        let artifacts = vec![art_external(
            "@snappy//snappy:snappy",
            "snappy",
            "/cache/external/snappy/snappy/snappy.cc",
        )];

        let report = check_materialization_completeness_with_stubs(
            &artifacts,
            &repo_paths,
            &fs,
            &producers,
            &NoStubLookup,
        );
        assert!(!report.all_admitted());
        assert_eq!(
            report.entries[0].status,
            MaterializationStatus::ExternalRepoSourceMissing
        );
    }

    /// AJ2-4: when one repo is a stub and another is not, the
    /// non-stub repo's absent source is still a blocker. The
    /// stub-aware path is precisely scoped — only repos the
    /// caller has classified as stubs get the admission.
    #[test]
    fn aj2_4_stub_aware_does_not_weaken_non_stub_repos() {
        let mut repo_paths = HashMap::new();
        let snappy_root = PathBuf::from("/cache/external/snappy");
        let real_root = PathBuf::from("/cache/external/real_dep");
        repo_paths.insert("snappy".to_string(), snappy_root.clone());
        repo_paths.insert("real_dep".to_string(), real_root.clone());
        let fs = FakeFs::new().with_dir(snappy_root).with_dir(real_root);
        let producers_set: HashSet<PathBuf> = HashSet::new();
        let producers = PathSetProducers(&producers_set);
        let mut stub_map: HashMap<String, String> = HashMap::new();
        stub_map.insert("snappy".to_string(), "system_library".to_string());
        let stubs = MapStubLookup(&stub_map);

        let artifacts = vec![
            art_external(
                "@snappy//snappy:snappy",
                "snappy",
                "/cache/external/snappy/snappy/snappy.cc",
            ),
            art_external(
                "@real_dep//pkg:real",
                "real_dep",
                "/cache/external/real_dep/pkg/real.cc",
            ),
        ];

        let report = check_materialization_completeness_with_stubs(
            &artifacts,
            &repo_paths,
            &fs,
            &producers,
            &stubs,
        );
        // First entry is the snappy stub — admitted.
        assert_eq!(
            report.entries[0].status,
            MaterializationStatus::IntentionalExternalStub {
                stub_kind: "system_library".to_string()
            }
        );
        // Second entry is the non-stub real_dep — blocked.
        assert_eq!(
            report.entries[1].status,
            MaterializationStatus::ExternalRepoSourceMissing
        );
        // Overall report is blocked (because real_dep is
        // missing).
        assert!(!report.all_admitted());
        // Blocker reason comes from the non-stub entry.
        assert_eq!(
            report.is_blocked(),
            Some("external repo source file missing")
        );
        // Per-repo aggregation: only real_dep contributes.
        let by_repo = report.blockers_by_repo();
        assert_eq!(by_repo.get(&Some("snappy".to_string())).copied(), None);
        assert_eq!(by_repo.get(&Some("real_dep".to_string())).copied(), Some(1));
    }

    /// AJ2-4: stub kinds are preserved verbatim through the
    /// classification. The status carries the kind label so the
    /// dry-run report (AM2) can group by stub kind.
    #[test]
    fn aj2_4_stub_kind_label_is_preserved() {
        let mut repo_paths = HashMap::new();
        let root = PathBuf::from("/cache/external/eigen_archive");
        repo_paths.insert("eigen_archive".to_string(), root.clone());
        let fs = FakeFs::new().with_dir(root);
        let producers_set: HashSet<PathBuf> = HashSet::new();
        let producers = PathSetProducers(&producers_set);
        let mut stub_map: HashMap<String, String> = HashMap::new();
        stub_map.insert("eigen_archive".to_string(), "header_only".to_string());
        let stubs = MapStubLookup(&stub_map);

        let artifacts = vec![art_external(
            "@eigen_archive//:eigen3",
            "eigen_archive",
            "/cache/external/eigen_archive/Eigen/Core",
        )];

        let report = check_materialization_completeness_with_stubs(
            &artifacts,
            &repo_paths,
            &fs,
            &producers,
            &stubs,
        );

        assert!(report.all_admitted());
        match &report.entries[0].status {
            MaterializationStatus::IntentionalExternalStub { stub_kind } => {
                assert_eq!(stub_kind, "header_only");
            }
            other => panic!("expected IntentionalExternalStub, got {:?}", other),
        }
    }

    /// AJ2-4: the IntentionalExternalStub variant's
    /// `is_admitted()` returns true and `blocker_reason()`
    /// returns None.
    #[test]
    fn aj2_4_intentional_external_stub_admits_and_has_no_blocker_reason() {
        let s = MaterializationStatus::IntentionalExternalStub {
            stub_kind: "system_library".to_string(),
        };
        assert!(s.is_admitted());
        assert_eq!(s.blocker_reason(), None);
    }

    /// AJ2-4: NoStubLookup returns None for every input.
    #[test]
    fn aj2_4_no_stub_lookup_returns_none() {
        let n = NoStubLookup;
        assert_eq!(n.repo_stub_kind("snappy"), None);
        assert_eq!(n.repo_stub_kind(""), None);
        assert_eq!(n.repo_stub_kind("anything"), None);
    }

    /// AJ2-4: MapStubLookup returns the mapped kind exactly.
    #[test]
    fn aj2_4_map_stub_lookup_returns_mapped_kind() {
        let mut map: HashMap<String, String> = HashMap::new();
        map.insert("snappy".to_string(), "system_library".to_string());
        map.insert("zlib_disabled".to_string(), "feature_disabled".to_string());
        let l = MapStubLookup(&map);
        assert_eq!(l.repo_stub_kind("snappy"), Some("system_library".to_string()));
        assert_eq!(
            l.repo_stub_kind("zlib_disabled"),
            Some("feature_disabled".to_string())
        );
        assert_eq!(l.repo_stub_kind("not_in_map"), None);
    }

    /// AJ2-4: a stub repo's IntentionalExternalStub status
    /// serializes to JSON with the kind label embedded.
    #[test]
    fn aj2_4_intentional_external_stub_serializes_with_kind() {
        let entry = MaterializationEntry {
            artifact: art_external(
                "@snappy//snappy:snappy",
                "snappy",
                "/cache/external/snappy/snappy/snappy.cc",
            ),
            status: MaterializationStatus::IntentionalExternalStub {
                stub_kind: "system_library".to_string(),
            },
            detail: "stub".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        // The variant tag goes to `"kind"` (per serde tag attr).
        assert_eq!(parsed["status"]["kind"], "intentional_external_stub");
        // The inner field is named `stub_kind` to avoid a tag
        // collision; it carries the classifier label.
        assert_eq!(parsed["status"]["stub_kind"], "system_library");
    }
}
