// Rush Build - External Repo Materialization Report
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Phase 5.C.4 Batch AJ (commit 225+): structured per-external-repo
//! materialization report. Distinct from the per-artifact
//! [`MaterializationCompletenessReport`](crate::materialization_report)
//! introduced in ABR-2 — that report classifies declared source
//! artifacts; this one classifies external repos themselves.
//!
//! ## Why this exists
//!
//! Batch ABR-4 surfaced the systemic finding: 14+ external repos on
//! this host are in the same "BUILD-present, source-payload-absent"
//! state as `@snappy`. The ABR-revised lane could only act on the
//! finding per-artifact; the External Repo Materialization
//! Correctness Lane (Pro 2026-05-21) needs a per-REPO inventory so
//! that:
//!
//!   - Operators triage by repo (which repo blocks the most
//!     candidates).
//!   - Repair plans select repos to fetch (AL/AM).
//!   - Root-cause classification (AK) joins this report with fetch
//!     attribution + repo-rule identity.
//!   - The CI gate (AO) consumes per-repo counts.
//!
//! ## What this module is NOT
//!
//! - It is NOT a verifier weakening. Per Pro: "Do not weaken verifier
//!   behavior to tolerate missing source files."
//! - It is NOT a stub generator. Per Pro: "No broad/silent external-
//!   repo stubs."
//! - It is NOT a fetch path. The scanner (AJ-2) is read-only.
//! - It is NOT a marker for scheduler safety. Per Pro: "Do not mark
//!   incomplete external repos as scheduler-safe."
//!
//! It is a STRUCTURED INVENTORY of facts already observable on disk
//! and in the analyzed action graph.

use std::collections::HashSet;
use std::path::PathBuf;

/// Schema version for the JSON wire shape of
/// [`ExternalRepoMaterializationReport`]. Bumped on breaking field
/// changes; additive fields keep the version stable.
pub const EXTERNAL_REPO_MATERIALIZATION_SCHEMA_VERSION: u32 = 1;

/// The classification of a single external repo's materialization
/// state on disk. Order is intentional: more-specific failure modes
/// appear before more-general ones so report consumers can present
/// the most actionable triage signal.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MaterializationStatus {
    /// Repo directory missing entirely. No BUILD file, no payload.
    /// Either never fetched, or removed.
    MissingRepoDir,
    /// Repo directory exists but no BUILD / BUILD.bazel anywhere
    /// found by the scanner. This is rare in practice — most
    /// external repos either lack both directory and BUILD, or have
    /// a BUILD plus partial payload.
    MissingBuildFile,
    /// Repo directory and BUILD file both exist, but ZERO declared
    /// source files from the analyzed closure are present on disk.
    /// The classic @snappy pattern.
    BuildOnly,
    /// Repo directory and BUILD file exist, at least one declared
    /// source exists, but at least one declared source is absent.
    /// Partial materialization — distinct from `BuildOnly` because
    /// the repair strategy may differ.
    MissingSources,
    /// Every declared source file from the analyzed closure exists
    /// on disk. The repo is materialization-complete for the scope
    /// the scanner observed.
    Complete,
    /// The scanner saw the repo referenced but could not classify
    /// it (e.g. ambiguous label, no declared sources reachable in
    /// the closure to anchor a yes/no answer). Defensive bucket so
    /// the report always covers every observed repo.
    Unknown,
}

impl MaterializationStatus {
    /// True iff the repo is fully usable for action-bearing scheduler
    /// dispatch from the analyzed closure's perspective.
    pub fn is_complete(&self) -> bool {
        matches!(self, MaterializationStatus::Complete)
    }

    /// True iff the repo blocks at least one declared source in the
    /// closure. Operators consume this for the "needs repair" set.
    pub fn is_blocked(&self) -> bool {
        !matches!(
            self,
            MaterializationStatus::Complete | MaterializationStatus::Unknown
        )
    }

    /// Stable kebab-case label for diagnostic / report output.
    pub fn label(&self) -> &'static str {
        match self {
            MaterializationStatus::MissingRepoDir => "missing_repo_dir",
            MaterializationStatus::MissingBuildFile => "missing_build_file",
            MaterializationStatus::BuildOnly => "build_only",
            MaterializationStatus::MissingSources => "missing_sources",
            MaterializationStatus::Complete => "complete",
            MaterializationStatus::Unknown => "unknown",
        }
    }
}

/// One external repo's structured materialization inventory.
///
/// `repo_name` is the workspace name (`"snappy"`, `"com_google_absl"`,
/// etc.), NOT the `@<repo>` label form. `repo_root` is the absolute
/// on-disk path the scanner inspected. Source counts reference
/// artifacts declared by analyzed rules in the candidate's
/// transitive closure — NOT every file the external repo could
/// theoretically contain.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExternalRepoMaterialization {
    pub repo_name: String,
    pub repo_root: PathBuf,
    pub has_repo_dir: bool,
    pub has_build: bool,
    pub has_build_bazel: bool,
    /// Number of source artifacts declared (via `srcs`/`hdrs`/etc.)
    /// in this repo by analyzed rules in the scanned closure.
    pub declared_source_count: u32,
    /// Of `declared_source_count`, how many exist on disk.
    pub existing_source_count: u32,
    /// `declared_source_count - existing_source_count`. Pre-computed
    /// for JSON consumers that don't want to subtract.
    pub missing_source_count: u32,
    /// Up to a small fixed cap (default 5) of representative missing
    /// source paths for operator triage. Sorted deterministically.
    pub missing_source_examples: Vec<PathBuf>,
    /// Set of rule kinds (e.g. `"cc_library"`,
    /// `"tf_proto_library"`) whose declared sources are absent.
    /// Sorted; deduplicated.
    pub rule_kinds_referencing_missing_sources: Vec<String>,
    /// Set of target labels (e.g. `"@snappy//snappy:snappy"`) whose
    /// declared sources are absent. Sorted; deduplicated.
    pub target_labels_referencing_missing_sources: Vec<String>,
    /// The classification.
    pub materialization_status: MaterializationStatus,
    /// Phase 5.C.4 Batch AK-1 (commit 230): is the BUILD file a
    /// Rush-generated stub (marker pattern detected in the
    /// content)? `None` when the scanner didn't inspect content
    /// (e.g. no BUILD found, or content-reader trait not
    /// configured). `Some(true)` when a stub marker is present.
    /// `Some(false)` when content was read and no stub marker
    /// found. Per AJ-5's finding: all 28 external repos on the
    /// canonical TF host are stubs. This field annotates intent —
    /// orthogonal to materialization_status which describes
    /// on-disk state.
    #[serde(default)]
    pub is_stub_build: Option<bool>,
    /// Phase 5.C.4 Batch AK-1: the matching stub marker line (or
    /// `None` when no stub detected). Helps operators identify
    /// which stub-generator code path produced the BUILD (e.g.
    /// `# Stub BUILD for snappy/snappy` → snappy.BUILD output
    /// path).
    #[serde(default)]
    pub stub_marker: Option<String>,
}

impl ExternalRepoMaterialization {
    /// Build an entry with the counts and examples derived from
    /// declared/existing artifact info. Caller is responsible for
    /// classifying `materialization_status` via [`classify`].
    pub fn from_declarations(
        repo_name: String,
        repo_root: PathBuf,
        has_repo_dir: bool,
        has_build: bool,
        has_build_bazel: bool,
        declared: &[DeclaredArtifact],
        existing_paths: &HashSet<PathBuf>,
    ) -> Self {
        let declared_source_count = declared.len() as u32;
        let mut existing_source_count = 0u32;
        let mut missing_examples: Vec<PathBuf> = Vec::new();
        let mut rule_kinds: HashSet<String> = HashSet::new();
        let mut target_labels: HashSet<String> = HashSet::new();
        for d in declared {
            if existing_paths.contains(&d.artifact_path) {
                existing_source_count += 1;
            } else {
                if missing_examples.len() < MISSING_EXAMPLE_CAP {
                    missing_examples.push(d.artifact_path.clone());
                }
                rule_kinds.insert(d.rule_kind.clone());
                target_labels.insert(d.target_label.clone());
            }
        }
        let missing_source_count = declared_source_count - existing_source_count;
        missing_examples.sort();
        let mut rule_kinds: Vec<String> = rule_kinds.into_iter().collect();
        rule_kinds.sort();
        let mut target_labels: Vec<String> = target_labels.into_iter().collect();
        target_labels.sort();

        let materialization_status = classify(
            has_repo_dir,
            has_build || has_build_bazel,
            declared_source_count,
            existing_source_count,
        );

        Self {
            repo_name,
            repo_root,
            has_repo_dir,
            has_build,
            has_build_bazel,
            declared_source_count,
            existing_source_count,
            missing_source_count,
            missing_source_examples: missing_examples,
            rule_kinds_referencing_missing_sources: rule_kinds,
            target_labels_referencing_missing_sources: target_labels,
            materialization_status,
            is_stub_build: None,
            stub_marker: None,
        }
    }

    /// AK-1 helper: annotate stub detection results onto this
    /// entry. Returns `self` for chaining.
    pub fn with_stub_detection(
        mut self,
        is_stub: bool,
        marker: Option<String>,
    ) -> Self {
        self.is_stub_build = Some(is_stub);
        self.stub_marker = marker;
        self
    }
}

/// Cap on the number of representative missing-source paths included
/// per repo. Keeps the report concise; operators get full detail by
/// scanning manually if they need it.
pub const MISSING_EXAMPLE_CAP: usize = 5;

/// A declared source artifact attributable to one external repo,
/// used as scanner input. The scanner aggregates these per-repo.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeclaredArtifact {
    /// Workspace name (e.g. `"snappy"`, `"com_google_absl"`).
    pub repo_name: String,
    /// Absolute on-disk path the artifact is expected at (e.g.
    /// `.cache/rush/external/snappy/snappy/snappy.cc`).
    pub artifact_path: PathBuf,
    /// Bazel-style label of the declaring target (e.g.
    /// `"@snappy//snappy:snappy"`).
    pub target_label: String,
    /// Rule kind of the declaring target.
    pub rule_kind: String,
}

/// Classify a single repo's materialization status given its
/// observable state. Pure function; called by
/// [`ExternalRepoMaterialization::from_declarations`] but also
/// useful for callers that want the verdict without building the
/// full struct.
pub fn classify(
    has_repo_dir: bool,
    has_any_build_file: bool,
    declared_source_count: u32,
    existing_source_count: u32,
) -> MaterializationStatus {
    if !has_repo_dir {
        return MaterializationStatus::MissingRepoDir;
    }
    if !has_any_build_file {
        return MaterializationStatus::MissingBuildFile;
    }
    if declared_source_count == 0 {
        // The scanner observed the repo (dir + BUILD present) but
        // saw no declared sources from the analyzed closure. This
        // is the BuildOnly case — common for repos that contribute
        // only header includes via dependent rules.
        return MaterializationStatus::BuildOnly;
    }
    if existing_source_count == 0 {
        // Declared sources but none of them on disk. The @snappy
        // case as observed via ABR-4.
        return MaterializationStatus::BuildOnly;
    }
    if existing_source_count == declared_source_count {
        return MaterializationStatus::Complete;
    }
    // At least one present, at least one missing. Partial
    // materialization — distinct from BuildOnly because the repair
    // strategy may differ (re-sync vs full re-fetch).
    MaterializationStatus::MissingSources
}

/// The aggregate inventory across all external repos observed in a
/// candidate's analyzed closure.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExternalRepoMaterializationReport {
    pub schema_version: u32,
    /// Per-repo entries, sorted by repo_name asc.
    pub entries: Vec<ExternalRepoMaterialization>,
    /// Tallies by status. Pre-computed for CI consumers.
    pub counts: StatusCounts,
}

/// Per-status repo counts derived from `entries`.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct StatusCounts {
    pub complete: u32,
    pub build_only: u32,
    pub missing_repo_dir: u32,
    pub missing_build_file: u32,
    pub missing_sources: u32,
    pub unknown: u32,
    /// Phase 5.C.4 Batch AK-1 (commit 230): of the entries, how
    /// many had `is_stub_build == Some(true)`. Orthogonal to the
    /// per-status counts (a stub can be classified as BuildOnly
    /// AND counted here as a stub). Helps operators see the
    /// "intentional stub" vs "accidental incomplete" split at a
    /// glance.
    #[serde(default)]
    pub stub_builds: u32,
}

impl StatusCounts {
    /// Total number of incomplete (blocked) repos: everything except
    /// `complete` and `unknown`.
    pub fn blocked_total(&self) -> u32 {
        self.build_only
            + self.missing_repo_dir
            + self.missing_build_file
            + self.missing_sources
    }
}

impl ExternalRepoMaterializationReport {
    pub fn new() -> Self {
        Self {
            schema_version: EXTERNAL_REPO_MATERIALIZATION_SCHEMA_VERSION,
            entries: Vec::new(),
            counts: StatusCounts::default(),
        }
    }

    /// Build a report from per-repo entries. Sorts entries by
    /// repo_name and pre-computes the per-status counts.
    pub fn from_entries(mut entries: Vec<ExternalRepoMaterialization>) -> Self {
        entries.sort_by(|a, b| a.repo_name.cmp(&b.repo_name));
        let mut counts = StatusCounts::default();
        for e in &entries {
            match e.materialization_status {
                MaterializationStatus::Complete => counts.complete += 1,
                MaterializationStatus::BuildOnly => counts.build_only += 1,
                MaterializationStatus::MissingRepoDir => {
                    counts.missing_repo_dir += 1
                }
                MaterializationStatus::MissingBuildFile => {
                    counts.missing_build_file += 1
                }
                MaterializationStatus::MissingSources => counts.missing_sources += 1,
                MaterializationStatus::Unknown => counts.unknown += 1,
            }
            if e.is_stub_build == Some(true) {
                counts.stub_builds += 1;
            }
        }
        Self {
            schema_version: EXTERNAL_REPO_MATERIALIZATION_SCHEMA_VERSION,
            entries,
            counts,
        }
    }

    /// Iterator over only the blocked (incomplete) entries, in the
    /// same order as `entries`.
    pub fn blocked(&self) -> impl Iterator<Item = &ExternalRepoMaterialization> {
        self.entries
            .iter()
            .filter(|e| e.materialization_status.is_blocked())
    }
}

impl Default for ExternalRepoMaterializationReport {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// AJ2-1: Typed stub identity model
// ============================================================================

/// Per Pro's 2026-05-21 stub-aware-lane directive: a typed
/// classification of Rush-generated external stub BUILD files
/// by their purpose / system-library shape. The categories
/// come from AJ-5's manual enumeration of the 28 stubbed
/// external repos found on the canonical TF host.
///
/// Distinct from [`MaterializationStatus`] — that enum
/// describes ON-DISK state (Complete vs BuildOnly vs
/// MissingSources etc.); this enum describes the INTENT
/// behind a Rush-generated stub.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExternalRepoStubKind {
    /// Stub redirecting to a system-installed library via
    /// `linkopts = ["-lXXX"]`. Example: `@snappy` (-lsnappy),
    /// `@zlib` (-lz). The Rush scheduler should treat these
    /// as provider-only `system_cc_library`-equivalents.
    SystemLibrary,
    /// Stub providing only header includes via
    /// `includes = ["/usr/include/..."]` with no `linkopts`.
    /// Example: `@eigen_archive` (eigen3 headers), `@pybind11`
    /// (header-only). The link path is empty; consumers get
    /// include paths only.
    HeaderOnly,
    /// Stub for a feature explicitly disabled in this build.
    /// Marker often says "CUDA disabled" / "NCCL disabled" /
    /// "ROCm disabled" / "TensorRT disabled". `srcs=[]`,
    /// `hdrs=[]`, no `linkopts`, no system includes — the rule
    /// exists only to satisfy a dependency that won't be
    /// used at runtime.
    FeatureDisabled,
    /// Stub for a Python library. `@absl_py` is the canonical
    /// example. These are typically `py_library` aliases to
    /// system Python packages, not cc_library, but the stub
    /// generator produces them through the same path.
    PythonStub,
    /// Generic stub that doesn't fit the other categories.
    /// Catch-all for stubs with no `linkopts`, no system
    /// includes, no feature-disabled marker. These are
    /// usually "loaded but not actually used" deps — present
    /// to satisfy the WORKSPACE-level dep graph without
    /// providing functionality.
    Misc,
    /// Stub-marker detected but the sub-classifier couldn't
    /// determine which of the above kinds applies. Should be
    /// rare; if it shows up consistently for some repo class,
    /// extend the classifier.
    Unknown,
}

impl ExternalRepoStubKind {
    /// Stable kebab-case label for diagnostic/report output.
    pub fn label(&self) -> &'static str {
        match self {
            ExternalRepoStubKind::SystemLibrary => "system_library",
            ExternalRepoStubKind::HeaderOnly => "header_only",
            ExternalRepoStubKind::FeatureDisabled => "feature_disabled",
            ExternalRepoStubKind::PythonStub => "python_stub",
            ExternalRepoStubKind::Misc => "misc",
            ExternalRepoStubKind::Unknown => "unknown",
        }
    }
}

/// Per Pro's AJ2-1 spec: richer per-repo stub metadata.
/// Pairs with `ExternalRepoMaterialization.is_stub_build` —
/// when that field is `Some(true)`, this struct can be
/// populated to describe HOW the stub is shaped (system
/// library? header-only? feature-disabled?).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExternalRepoStubInfo {
    pub repo_name: String,
    pub build_file_path: PathBuf,
    pub kind: ExternalRepoStubKind,
    /// System include paths the stub references (e.g.
    /// `["/usr/include"]`). Empty for stubs that don't
    /// provide includes (FeatureDisabled, Misc).
    pub system_includes: Vec<String>,
    /// Link options the stub references (e.g.
    /// `["-lsnappy"]`). Empty for HeaderOnly and
    /// FeatureDisabled stubs.
    pub linkopts: Vec<String>,
    /// Did the stub declare any sources in `srcs`?
    /// For Rush stubs this is virtually always `false`
    /// because the whole point is to skip building.
    pub has_sources: bool,
    /// Did the stub declare any headers in `hdrs`?
    pub has_headers: bool,
    /// The marker line that triggered stub detection (e.g.
    /// `"# Stub BUILD for snappy/snappy - Rush uses system snappy"`).
    pub marker_detected: String,
}

/// Classify the kind of a Rush-generated stub by inspecting
/// the stub marker line + the BUILD content's surface
/// attributes. Pure function; no I/O.
///
/// Algorithm:
///   - FeatureDisabled if the marker line contains "disabled"
///     (case-insensitive).
///   - PythonStub if the repo_name starts with `absl_py` or
///     the marker line mentions `"py_library"`.
///   - SystemLibrary if linkopts is non-empty.
///   - HeaderOnly if linkopts is empty but system_includes
///     is non-empty.
///   - Misc otherwise.
///
/// Returns `Unknown` only when caller passes empty marker and
/// empty content (defensive).
pub fn classify_stub_kind(
    repo_name: &str,
    marker_line: &str,
    linkopts: &[String],
    system_includes: &[String],
) -> ExternalRepoStubKind {
    if marker_line.is_empty() && linkopts.is_empty() && system_includes.is_empty() {
        return ExternalRepoStubKind::Unknown;
    }
    let marker_lower = marker_line.to_lowercase();
    if marker_lower.contains("disabled") {
        return ExternalRepoStubKind::FeatureDisabled;
    }
    if repo_name.starts_with("absl_py") || marker_lower.contains("py_library") {
        return ExternalRepoStubKind::PythonStub;
    }
    if !linkopts.is_empty() {
        return ExternalRepoStubKind::SystemLibrary;
    }
    if !system_includes.is_empty() {
        return ExternalRepoStubKind::HeaderOnly;
    }
    ExternalRepoStubKind::Misc
}

/// Extract `linkopts` and `system_includes` lists from a BUILD
/// file's content via lightweight regex-shaped parsing. The
/// stub BUILD files have a very predictable structure (see
/// crates/rush-build/src/external_deps.rs); we don't need a
/// full Starlark parser to extract these two attributes.
///
/// Returns `(linkopts, system_includes)`. Both default to
/// empty when the patterns don't match.
///
/// This is pattern-matching, not parsing — it works because
/// the stub generator's output is uniform. For non-stub BUILD
/// files, the patterns may match accidentally; callers should
/// gate this on having already detected a stub marker.
pub fn extract_stub_attrs(content: &str) -> (Vec<String>, Vec<String>) {
    let mut linkopts = Vec::new();
    let mut system_includes = Vec::new();
    // linkopts = ["-lXXX"] or linkopts = ["-lXXX", "-lYYY"]
    if let Some(opts) = extract_list_attr(content, "linkopts") {
        linkopts = opts;
    }
    // includes = ["/usr/include"] or similar
    if let Some(incs) = extract_list_attr(content, "includes") {
        for inc in incs {
            if inc.starts_with('/') {
                system_includes.push(inc);
            }
        }
    }
    (linkopts, system_includes)
}

/// Extract a list-valued attribute from BUILD content using a
/// pattern-match approach. Returns `None` if not found.
fn extract_list_attr(content: &str, attr: &str) -> Option<Vec<String>> {
    let needle = format!("{} = [", attr);
    let start = content.find(&needle)?;
    let after = &content[start + needle.len()..];
    let end = after.find(']')?;
    let inside = &after[..end];
    let mut out = Vec::new();
    for raw in inside.split(',') {
        let s = raw.trim().trim_matches('"').trim_matches('\'').trim();
        if !s.is_empty() {
            out.push(s.to_string());
        }
    }
    Some(out)
}

/// Build an `ExternalRepoStubInfo` from BUILD content + repo
/// name + marker. Returns `None` if `marker_line` is empty
/// (i.e. the BUILD isn't a stub).
pub fn build_stub_info(
    repo_name: String,
    build_file_path: PathBuf,
    content: &str,
    marker_line: String,
) -> Option<ExternalRepoStubInfo> {
    if marker_line.is_empty() {
        return None;
    }
    let (linkopts, system_includes) = extract_stub_attrs(content);
    // Cheap heuristics for has_sources / has_headers via the
    // `srcs = [` and `hdrs = [` patterns. A non-empty list
    // appears as `srcs = ["foo.cc",`; an empty list appears
    // as `srcs = [],`.
    let has_sources = content.contains("srcs = [")
        && !content.contains("srcs = [],")
        && !content.contains("srcs = [ ],");
    let has_headers = content.contains("hdrs = [")
        && !content.contains("hdrs = [],")
        && !content.contains("hdrs = [ ],");
    let kind = classify_stub_kind(&repo_name, &marker_line, &linkopts, &system_includes);
    Some(ExternalRepoStubInfo {
        repo_name,
        build_file_path,
        kind,
        system_includes,
        linkopts,
        has_sources,
        has_headers,
        marker_detected: marker_line,
    })
}

// ============================================================================
// AK-1: Stub BUILD detection
// ============================================================================

/// Stub-marker patterns the Rush stub generator (in
/// `crates/rush-build/src/external_deps.rs`) emits at the head
/// of generated BUILD files. Per AJ-5's enumeration, every
/// stubbed external repo on the canonical TF host carries one
/// of these markers in its first three lines.
///
/// Order matters: the longer / more-specific patterns come first
/// so a longer-form marker gets returned before a shorter prefix.
const STUB_MARKER_PATTERNS: &[&str] = &[
    "# Stub BUILD",
    "# Rush stub",
    "# Auto-generated BUILD",
    "# Auto-generated",
    "Stub BUILD",
    "Rush stub",
    "Rush uses",
    "Rush uses system",
    "Auto-generated BUILD",
];

/// Inspect the head of a BUILD file's content for a stub marker.
/// Returns the matching marker string when present.
///
/// Inspects up to the first ~256 bytes / first 3 lines of the
/// content, whichever comes first. Stubs put their marker at the
/// top of the file by convention; this bound keeps the inspection
/// cheap.
pub fn detect_stub_marker(content: &str) -> Option<String> {
    let head: String = content.chars().take(256).collect();
    let head: String = head.lines().take(3).collect::<Vec<_>>().join("\n");
    for pat in STUB_MARKER_PATTERNS {
        if head.contains(pat) {
            // Return the line containing the marker for operator
            // triage (the full line is more useful than just the
            // pattern).
            for line in head.lines() {
                if line.contains(pat) {
                    return Some(line.trim().to_string());
                }
            }
            return Some(pat.to_string());
        }
    }
    None
}

// ============================================================================
// AJ-3: External-repo label parsing helper
// ============================================================================

/// Parse a Bazel-style external-repo label into its `(repo_name,
/// package, file)` components. Returns `None` for non-external
/// labels (in-tree paths, `:name` labels, `//pkg:name` labels).
///
/// External label shapes recognized:
///   `@snappy//snappy:snappy.cc`           → ("snappy", "snappy", "snappy.cc")
///   `@com_google_absl//absl/base:atomic_hook.h`
///        → ("com_google_absl", "absl/base", "atomic_hook.h")
///   `@snappy//:snappy.cc`                  → ("snappy", "", "snappy.cc")
///   `@local_tsl//tsl/platform:macros.h`    → ("local_tsl", "tsl/platform", "macros.h")
///
/// Bazel allows file references without a package separator
/// (`@repo//path/to/file` with no colon — the last segment is
/// implicitly the target). This parser supports that too:
///   `@snappy//snappy/snappy.cc`            → ("snappy", "snappy", "snappy.cc")
///
/// The returned `package` is empty when the file is at the repo
/// root. `file` is never empty for a valid input.
pub fn parse_external_repo_label(label: &str) -> Option<(String, String, String)> {
    let after_at = label.strip_prefix('@')?;
    let (repo_name, rest) = after_at.split_once("//")?;
    if repo_name.is_empty() {
        return None;
    }
    // Two shapes for the remainder: "pkg:file" or "pkg/path/file"
    // (no colon, target name implied to be the last path segment).
    let (package, file) = if let Some((pkg, name)) = rest.split_once(':') {
        (pkg.to_string(), name.to_string())
    } else if let Some(idx) = rest.rfind('/') {
        let (pkg, name) = rest.split_at(idx);
        // `name` includes the leading '/'.
        (pkg.to_string(), name[1..].to_string())
    } else {
        // Bare `@repo//file` — file at the repo root, no package.
        ("".to_string(), rest.to_string())
    };
    if file.is_empty() {
        return None;
    }
    Some((repo_name.to_string(), package, file))
}

/// Resolve an external-repo label to its expected on-disk path
/// under the repo's cache root, following the layout Rush uses
/// (`<repo_root>/<package>/<file>`).
///
/// Returns `None` for non-external labels (use
/// [`parse_external_repo_label`] for the parse-only path).
pub fn resolve_external_repo_label_path(
    label: &str,
    repo_root: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let (_repo, package, file) = parse_external_repo_label(label)?;
    let mut out = repo_root.to_path_buf();
    if !package.is_empty() {
        out.push(&package);
    }
    out.push(&file);
    Some(out)
}

// ============================================================================
// AJ-2: Inventory scanner
// ============================================================================

/// Trait abstracting filesystem-existence checks so the scanner is
/// unit-testable without touching real disk. The production
/// implementation [`RealScannerFs`] is a thin wrapper over
/// `Path::is_file` / `Path::is_dir`.
///
/// Mirrors the [`MaterializationFs`](crate::materialization_report::MaterializationFs)
/// trait shape from ABR-2 — kept separate so the two modules don't
/// take a hard dependency on each other.
pub trait ScannerFs {
    fn file_exists(&self, path: &std::path::Path) -> bool;
    fn dir_exists(&self, path: &std::path::Path) -> bool;
    /// AK-1 extension: read up to `max_bytes` of file content for
    /// stub-marker detection. Returns `None` if the file doesn't
    /// exist or can't be read. Default impl returns `None` so
    /// pre-AK ScannerFs impls keep working (they simply won't
    /// populate `is_stub_build`).
    fn read_head(&self, _path: &std::path::Path, _max_bytes: usize) -> Option<String> {
        None
    }
}

/// Production filesystem accessor for the scanner.
pub struct RealScannerFs;

impl ScannerFs for RealScannerFs {
    fn file_exists(&self, p: &std::path::Path) -> bool {
        p.is_file()
    }
    fn dir_exists(&self, p: &std::path::Path) -> bool {
        p.is_dir()
    }
    fn read_head(&self, p: &std::path::Path, max_bytes: usize) -> Option<String> {
        use std::io::Read;
        let mut f = std::fs::File::open(p).ok()?;
        let mut buf = vec![0u8; max_bytes];
        let n = f.read(&mut buf).ok()?;
        buf.truncate(n);
        String::from_utf8(buf).ok()
    }
}

/// Walk a list of declared source artifacts (one per declared
/// `srcs`/`hdrs` entry in the analyzed closure) and emit a per-repo
/// [`ExternalRepoMaterializationReport`].
///
/// `repo_paths` maps repo workspace names to their on-disk roots
/// (typically populated by the orchestrator's
/// `external_repo_paths` cache). Artifacts whose repo isn't in the
/// map get a `MissingRepoDir` classification.
///
/// **Scope discipline (per Pro 2026-05-21):**
///
///   - Read-only. The scanner MUST NOT call fetch, write files,
///     create directories, or otherwise mutate state.
///   - Only repos referenced by the input artifacts are inspected.
///     The scanner MUST NOT enumerate every WORKSPACE repo on disk.
///   - Generated inputs (artifacts produced by other targets'
///     actions) must be filtered out by the caller — this scanner
///     treats every artifact as a declared source.
///
/// Per-repo `has_repo_dir`, `has_build`, and `has_build_bazel` are
/// resolved by stat-ing repo_root, repo_root/BUILD, and
/// repo_root/BUILD.bazel respectively.
pub fn scan_external_repos(
    artifacts: &[DeclaredArtifact],
    repo_paths: &std::collections::HashMap<String, std::path::PathBuf>,
    fs: &dyn ScannerFs,
) -> ExternalRepoMaterializationReport {
    use std::collections::HashMap;

    // Group artifacts by repo_name. BTreeMap → deterministic order.
    let mut per_repo: std::collections::BTreeMap<String, Vec<DeclaredArtifact>> =
        std::collections::BTreeMap::new();
    for art in artifacts {
        per_repo
            .entry(art.repo_name.clone())
            .or_default()
            .push(art.clone());
    }

    // Pre-compute existence for every unique artifact path. This
    // batches the fs calls so the same path isn't stat()ed twice if
    // two artifacts reference it (rare but possible with shared
    // headers).
    let mut existing_paths: std::collections::HashSet<std::path::PathBuf> =
        std::collections::HashSet::new();
    let mut seen: std::collections::HashSet<std::path::PathBuf> =
        std::collections::HashSet::new();
    for art in artifacts {
        if seen.insert(art.artifact_path.clone()) && fs.file_exists(&art.artifact_path) {
            existing_paths.insert(art.artifact_path.clone());
        }
    }

    // Per-repo: cache the repo-dir/BUILD/BUILD.bazel checks so we
    // don't re-stat for every artifact of the same repo.
    let mut repo_dir_cache: HashMap<String, (bool, bool, bool, std::path::PathBuf)> =
        HashMap::new();
    let mut entries: Vec<ExternalRepoMaterialization> =
        Vec::with_capacity(per_repo.len());

    for (repo_name, declared) in per_repo.iter() {
        let (has_repo_dir, has_build, has_build_bazel, repo_root) = match repo_dir_cache
            .get(repo_name)
        {
            Some(t) => t.clone(),
            None => {
                let repo_root = repo_paths
                    .get(repo_name)
                    .cloned()
                    .unwrap_or_else(|| std::path::PathBuf::from(""));
                let has_repo_dir = !repo_root.as_os_str().is_empty()
                    && fs.dir_exists(&repo_root);
                let has_build = has_repo_dir && fs.file_exists(&repo_root.join("BUILD"));
                let has_build_bazel =
                    has_repo_dir && fs.file_exists(&repo_root.join("BUILD.bazel"));
                let t = (has_repo_dir, has_build, has_build_bazel, repo_root);
                repo_dir_cache.insert(repo_name.clone(), t.clone());
                t
            }
        };

        let entry = ExternalRepoMaterialization::from_declarations(
            repo_name.clone(),
            repo_root.clone(),
            has_repo_dir,
            has_build,
            has_build_bazel,
            declared,
            &existing_paths,
        );
        // AK-1: stub detection. Read the head of whichever BUILD
        // file exists and look for a marker. Read failures yield
        // is_stub_build = None (vs Some(false), which signals
        // content-was-read-but-no-marker).
        let entry = if has_build || has_build_bazel {
            let build_path = if has_build_bazel {
                repo_root.join("BUILD.bazel")
            } else {
                repo_root.join("BUILD")
            };
            match fs.read_head(&build_path, 256) {
                Some(content) => {
                    let marker = detect_stub_marker(&content);
                    entry.with_stub_detection(marker.is_some(), marker)
                }
                None => entry,
            }
        } else {
            entry
        };
        entries.push(entry);
    }

    ExternalRepoMaterializationReport::from_entries(entries)
}

#[cfg(test)]
mod scanner_tests {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};

    /// Fake filesystem oracle. `files` are paths that respond true
    /// to `file_exists`; `dirs` respond true to `dir_exists`. Paths
    /// not listed return false for both.
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

    impl ScannerFs for FakeFs {
        fn file_exists(&self, p: &Path) -> bool {
            self.files.contains(p)
        }
        fn dir_exists(&self, p: &Path) -> bool {
            self.dirs.contains(p)
        }
    }

    fn art(repo: &str, path: &str, target: &str, rule: &str) -> DeclaredArtifact {
        DeclaredArtifact {
            repo_name: repo.to_string(),
            artifact_path: PathBuf::from(path),
            target_label: target.to_string(),
            rule_kind: rule.to_string(),
        }
    }

    /// AJ-2 directive test: synthetic fixture — repo with BUILD and
    /// all sources present classifies as Complete.
    #[test]
    fn scan_repo_with_build_and_all_sources_present_is_complete() {
        let fs = FakeFs::new()
            .with_dir("/cache/external/zlib")
            .with_file("/cache/external/zlib/BUILD")
            .with_file("/cache/external/zlib/zlib.c")
            .with_file("/cache/external/zlib/zlib.h");
        let mut repo_paths = HashMap::new();
        repo_paths.insert("zlib".to_string(), PathBuf::from("/cache/external/zlib"));
        let artifacts = vec![
            art("zlib", "/cache/external/zlib/zlib.c", "@zlib//zlib:zlib", "cc_library"),
            art("zlib", "/cache/external/zlib/zlib.h", "@zlib//zlib:zlib", "cc_library"),
        ];
        let report = scan_external_repos(&artifacts, &repo_paths, &fs);
        assert_eq!(report.entries.len(), 1);
        assert_eq!(report.entries[0].repo_name, "zlib");
        assert_eq!(
            report.entries[0].materialization_status,
            MaterializationStatus::Complete
        );
        assert_eq!(report.counts.complete, 1);
        assert_eq!(report.counts.blocked_total(), 0);
    }

    /// AJ-2 directive test: synthetic fixture — repo with BUILD and
    /// missing source classifies as MissingSources / BuildOnly.
    /// (Here: all sources missing → BuildOnly.)
    #[test]
    fn scan_repo_with_build_only_classifies_correctly() {
        let fs = FakeFs::new()
            .with_dir("/cache/external/snappy")
            .with_file("/cache/external/snappy/BUILD");
        let mut repo_paths = HashMap::new();
        repo_paths
            .insert("snappy".to_string(), PathBuf::from("/cache/external/snappy"));
        let artifacts = vec![
            art(
                "snappy",
                "/cache/external/snappy/snappy.cc",
                "@snappy//snappy:snappy",
                "cc_library",
            ),
            art(
                "snappy",
                "/cache/external/snappy/snappy-sinksource.cc",
                "@snappy//snappy:snappy",
                "cc_library",
            ),
        ];
        let report = scan_external_repos(&artifacts, &repo_paths, &fs);
        assert_eq!(
            report.entries[0].materialization_status,
            MaterializationStatus::BuildOnly
        );
        assert_eq!(report.counts.build_only, 1);
    }

    /// AJ-2 directive test: synthetic fixture — repo dir missing
    /// classifies as MissingRepoDir.
    #[test]
    fn scan_repo_dir_missing_classifies_correctly() {
        let fs = FakeFs::new();
        let mut repo_paths = HashMap::new();
        repo_paths.insert(
            "nonexistent".to_string(),
            PathBuf::from("/cache/external/nonexistent"),
        );
        let artifacts = vec![art(
            "nonexistent",
            "/cache/external/nonexistent/x.cc",
            "@nonexistent//pkg:x",
            "cc_library",
        )];
        let report = scan_external_repos(&artifacts, &repo_paths, &fs);
        assert_eq!(
            report.entries[0].materialization_status,
            MaterializationStatus::MissingRepoDir
        );
        assert_eq!(report.counts.missing_repo_dir, 1);
    }

    /// AJ-2 directive test: repo not in repo_paths map (orchestrator
    /// never resolved it) also classifies as MissingRepoDir.
    #[test]
    fn scan_repo_not_in_paths_map_classifies_as_missing_repo_dir() {
        let fs = FakeFs::new();
        let repo_paths: HashMap<String, PathBuf> = HashMap::new();
        let artifacts = vec![art(
            "unmapped",
            "/cache/external/unmapped/x.cc",
            "@unmapped//pkg:x",
            "cc_library",
        )];
        let report = scan_external_repos(&artifacts, &repo_paths, &fs);
        assert_eq!(
            report.entries[0].materialization_status,
            MaterializationStatus::MissingRepoDir
        );
    }

    /// AJ-2 directive test: partial materialization (some sources
    /// present, some absent) classifies as MissingSources.
    #[test]
    fn scan_partial_materialization_is_missing_sources() {
        let fs = FakeFs::new()
            .with_dir("/cache/external/absl")
            .with_file("/cache/external/absl/BUILD")
            .with_file("/cache/external/absl/strings.h");
        let mut repo_paths = HashMap::new();
        repo_paths.insert("absl".to_string(), PathBuf::from("/cache/external/absl"));
        let artifacts = vec![
            art(
                "absl",
                "/cache/external/absl/strings.cc",
                "@absl//strings:strings",
                "cc_library",
            ),
            art(
                "absl",
                "/cache/external/absl/strings.h",
                "@absl//strings:strings",
                "cc_library",
            ),
        ];
        let report = scan_external_repos(&artifacts, &repo_paths, &fs);
        assert_eq!(
            report.entries[0].materialization_status,
            MaterializationStatus::MissingSources
        );
        assert_eq!(report.entries[0].existing_source_count, 1);
        assert_eq!(report.entries[0].missing_source_count, 1);
    }

    /// AJ-2 directive test: BUILD.bazel counts equivalently to BUILD.
    /// Scanner stats both.
    #[test]
    fn scan_build_bazel_counts_equivalently() {
        let fs = FakeFs::new()
            .with_dir("/cache/external/foo")
            .with_file("/cache/external/foo/BUILD.bazel")
            .with_file("/cache/external/foo/foo.cc");
        let mut repo_paths = HashMap::new();
        repo_paths.insert("foo".to_string(), PathBuf::from("/cache/external/foo"));
        let artifacts = vec![art(
            "foo",
            "/cache/external/foo/foo.cc",
            "@foo//pkg:foo",
            "cc_library",
        )];
        let report = scan_external_repos(&artifacts, &repo_paths, &fs);
        assert!(report.entries[0].has_build_bazel);
        assert!(!report.entries[0].has_build);
        assert_eq!(
            report.entries[0].materialization_status,
            MaterializationStatus::Complete
        );
    }

    /// AJ-2 directive test: scanner aggregates across MULTIPLE
    /// external repos, sorts by name, counts per-status correctly.
    /// This is the cpu_feature_guard-class scenario in miniature.
    #[test]
    fn scan_multi_repo_closure_aggregates_correctly() {
        let fs = FakeFs::new()
            .with_dir("/cache/external/snappy")
            .with_file("/cache/external/snappy/BUILD")
            .with_dir("/cache/external/zlib")
            .with_file("/cache/external/zlib/BUILD")
            .with_file("/cache/external/zlib/zlib.c");
        let mut repo_paths = HashMap::new();
        repo_paths.insert(
            "snappy".to_string(),
            PathBuf::from("/cache/external/snappy"),
        );
        repo_paths.insert("zlib".to_string(), PathBuf::from("/cache/external/zlib"));
        repo_paths.insert(
            "absent".to_string(),
            PathBuf::from("/cache/external/absent"),
        );
        let artifacts = vec![
            art(
                "snappy",
                "/cache/external/snappy/snappy.cc",
                "@snappy//snappy:snappy",
                "cc_library",
            ),
            art(
                "zlib",
                "/cache/external/zlib/zlib.c",
                "@zlib//zlib:zlib",
                "cc_library",
            ),
            art(
                "absent",
                "/cache/external/absent/x.cc",
                "@absent//pkg:x",
                "cc_library",
            ),
        ];
        let report = scan_external_repos(&artifacts, &repo_paths, &fs);
        let names: Vec<&str> = report
            .entries
            .iter()
            .map(|e| e.repo_name.as_str())
            .collect();
        // Sorted by repo_name asc.
        assert_eq!(names, vec!["absent", "snappy", "zlib"]);
        assert_eq!(report.counts.complete, 1);
        assert_eq!(report.counts.build_only, 1);
        assert_eq!(report.counts.missing_repo_dir, 1);
        assert_eq!(report.counts.blocked_total(), 2);
    }

    /// AJ-2 directive test: the scanner has ZERO filesystem side
    /// effects. Pre-build the fs state, run the scanner, verify the
    /// fake-fs containers are byte-identical after the scan.
    #[test]
    fn scan_has_no_filesystem_side_effects() {
        // The FakeFs is read-only by construction (no &mut self
        // methods), but we additionally pin that the scanner
        // doesn't try to call anything that doesn't fit the trait
        // surface.
        let files_before: HashSet<PathBuf> = [
            PathBuf::from("/cache/external/snappy/BUILD"),
            PathBuf::from("/cache/external/snappy/snappy.cc"),
        ]
        .into_iter()
        .collect();
        let dirs_before: HashSet<PathBuf> =
            [PathBuf::from("/cache/external/snappy")].into_iter().collect();
        let fs = FakeFs {
            files: files_before.clone(),
            dirs: dirs_before.clone(),
        };
        let mut repo_paths = HashMap::new();
        repo_paths.insert(
            "snappy".to_string(),
            PathBuf::from("/cache/external/snappy"),
        );
        let artifacts = vec![art(
            "snappy",
            "/cache/external/snappy/snappy.cc",
            "@snappy//snappy:snappy",
            "cc_library",
        )];
        let _report = scan_external_repos(&artifacts, &repo_paths, &fs);
        assert_eq!(fs.files, files_before);
        assert_eq!(fs.dirs, dirs_before);
    }

    /// AJ-2 directive test: duplicate artifacts (same path declared
    /// by two targets) don't inflate counts. Pin de-duplication
    /// implicit in the scanner's HashSet-backed existence cache.
    #[test]
    fn scan_duplicate_artifact_paths_dedup_for_existence_check() {
        let fs = FakeFs::new()
            .with_dir("/cache/external/shared")
            .with_file("/cache/external/shared/BUILD")
            .with_file("/cache/external/shared/header.h");
        let mut repo_paths = HashMap::new();
        repo_paths.insert(
            "shared".to_string(),
            PathBuf::from("/cache/external/shared"),
        );
        let artifacts = vec![
            art(
                "shared",
                "/cache/external/shared/header.h",
                "@shared//pkg:a",
                "cc_library",
            ),
            art(
                "shared",
                "/cache/external/shared/header.h",
                "@shared//pkg:b",
                "cc_library",
            ),
        ];
        let report = scan_external_repos(&artifacts, &repo_paths, &fs);
        // 2 declared (both target labels), 2 existing.
        assert_eq!(report.entries[0].declared_source_count, 2);
        assert_eq!(report.entries[0].existing_source_count, 2);
        assert_eq!(
            report.entries[0].materialization_status,
            MaterializationStatus::Complete
        );
    }

    /// AJ-2 directive test: an empty artifact list produces an empty
    /// report (no panics, no spurious entries).
    #[test]
    fn scan_empty_input_is_empty_report() {
        let fs = FakeFs::new();
        let repo_paths: HashMap<String, PathBuf> = HashMap::new();
        let report = scan_external_repos(&[], &repo_paths, &fs);
        assert!(report.entries.is_empty());
        assert_eq!(report.counts.blocked_total(), 0);
        assert_eq!(report.schema_version, 1);
    }

    // AJ-3 label parser tests

    /// Standard external label: @repo//pkg:file.
    #[test]
    fn parse_external_label_standard_form() {
        let p = parse_external_repo_label("@snappy//snappy:snappy.cc");
        assert_eq!(
            p,
            Some(("snappy".to_string(), "snappy".to_string(), "snappy.cc".to_string()))
        );
    }

    /// Multi-segment package: @repo//pkg/sub:file.
    #[test]
    fn parse_external_label_multi_segment_package() {
        let p = parse_external_repo_label("@com_google_absl//absl/base:atomic_hook.h");
        assert_eq!(
            p,
            Some((
                "com_google_absl".to_string(),
                "absl/base".to_string(),
                "atomic_hook.h".to_string()
            ))
        );
    }

    /// Root file: @repo//:file (no package).
    #[test]
    fn parse_external_label_root_package() {
        let p = parse_external_repo_label("@snappy//:snappy.cc");
        assert_eq!(
            p,
            Some(("snappy".to_string(), "".to_string(), "snappy.cc".to_string()))
        );
    }

    /// Path-form (no colon): @repo//pkg/file.
    #[test]
    fn parse_external_label_path_form_no_colon() {
        let p = parse_external_repo_label("@snappy//snappy/snappy.cc");
        assert_eq!(
            p,
            Some(("snappy".to_string(), "snappy".to_string(), "snappy.cc".to_string()))
        );
    }

    /// Path-form, single segment after //: bare file at root.
    #[test]
    fn parse_external_label_bare_file_at_root() {
        let p = parse_external_repo_label("@foo//bar.cc");
        assert_eq!(
            p,
            Some(("foo".to_string(), "".to_string(), "bar.cc".to_string()))
        );
    }

    /// Reject: in-tree workspace label.
    #[test]
    fn parse_external_label_rejects_workspace_label() {
        assert_eq!(parse_external_repo_label("//tsl/platform:mutex.h"), None);
    }

    /// Reject: same-package label.
    #[test]
    fn parse_external_label_rejects_relative_label() {
        assert_eq!(parse_external_repo_label(":local_target"), None);
    }

    /// Reject: bare relative path.
    #[test]
    fn parse_external_label_rejects_bare_path() {
        assert_eq!(parse_external_repo_label("foo.cc"), None);
    }

    /// Reject: empty repo name.
    #[test]
    fn parse_external_label_rejects_empty_repo() {
        assert_eq!(parse_external_repo_label("@//pkg:file"), None);
    }

    /// Reject: empty file.
    #[test]
    fn parse_external_label_rejects_empty_file() {
        assert_eq!(parse_external_repo_label("@snappy//snappy:"), None);
    }

    /// Resolve label to on-disk path: standard layout.
    #[test]
    fn resolve_external_label_path_layout() {
        let p = resolve_external_repo_label_path(
            "@snappy//snappy:snappy.cc",
            std::path::Path::new("/cache/external/snappy"),
        );
        assert_eq!(
            p,
            Some(std::path::PathBuf::from(
                "/cache/external/snappy/snappy/snappy.cc"
            ))
        );
    }

    /// Resolve label to on-disk path: root package (no pkg dir).
    #[test]
    fn resolve_external_label_path_root_package() {
        let p = resolve_external_repo_label_path(
            "@snappy//:snappy.cc",
            std::path::Path::new("/cache/external/snappy"),
        );
        assert_eq!(
            p,
            Some(std::path::PathBuf::from("/cache/external/snappy/snappy.cc"))
        );
    }

    /// Resolve label to on-disk path: returns None for non-external.
    #[test]
    fn resolve_external_label_path_rejects_workspace() {
        let p = resolve_external_repo_label_path(
            "//tsl/platform:mutex.h",
            std::path::Path::new("/cache/external/snappy"),
        );
        assert_eq!(p, None);
    }

    // AJ-4 directive: synthetic external repo fixture tests
    // that materialize REAL directory structures on disk via
    // tempfile and run the scanner with RealScannerFs. Pins
    // that the scanner classifies correctly against actual
    // filesystem state, not just the FakeFs oracle.

    use tempfile::TempDir;

    fn write_file(dir: &std::path::Path, rel: &str, content: &str) -> PathBuf {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, content).unwrap();
        path
    }

    /// AJ-4 fixture #1 (Pro spec): repo with BUILD and all
    /// declared sources present → Complete.
    #[test]
    fn aj4_fixture_repo_with_build_and_all_sources_present() {
        let tmp = TempDir::new().unwrap();
        let repo_root = tmp.path().join("zlib");
        std::fs::create_dir_all(&repo_root).unwrap();
        write_file(&repo_root, "BUILD", "# fixture");
        let zlib_c = write_file(&repo_root, "zlib.c", "/* fixture */");
        let zlib_h = write_file(&repo_root, "zlib.h", "/* fixture */");
        let mut repo_paths = HashMap::new();
        repo_paths.insert("zlib".to_string(), repo_root.clone());
        let artifacts = vec![
            DeclaredArtifact {
                repo_name: "zlib".to_string(),
                artifact_path: zlib_c.clone(),
                target_label: "@zlib//zlib:zlib".to_string(),
                rule_kind: "cc_library".to_string(),
            },
            DeclaredArtifact {
                repo_name: "zlib".to_string(),
                artifact_path: zlib_h.clone(),
                target_label: "@zlib//zlib:zlib".to_string(),
                rule_kind: "cc_library".to_string(),
            },
        ];
        let report = scan_external_repos(&artifacts, &repo_paths, &RealScannerFs);
        assert_eq!(report.entries.len(), 1);
        assert_eq!(
            report.entries[0].materialization_status,
            MaterializationStatus::Complete
        );
        assert_eq!(report.counts.complete, 1);
    }

    /// AJ-4 fixture #2 (Pro spec): repo with BUILD and missing
    /// declared source → BuildOnly (all missing) or
    /// MissingSources (partial). The "all-missing" case is the
    /// @snappy reproduction.
    #[test]
    fn aj4_fixture_repo_with_build_only_no_sources_present() {
        let tmp = TempDir::new().unwrap();
        let repo_root = tmp.path().join("snappy");
        std::fs::create_dir_all(&repo_root).unwrap();
        write_file(&repo_root, "snappy/BUILD", "# fixture");
        let mut repo_paths = HashMap::new();
        repo_paths.insert("snappy".to_string(), repo_root.clone());
        let declared_path = repo_root.join("snappy").join("snappy.cc");
        // Note: do NOT create snappy.cc on disk.
        let artifacts = vec![DeclaredArtifact {
            repo_name: "snappy".to_string(),
            artifact_path: declared_path.clone(),
            target_label: "@snappy//snappy:snappy".to_string(),
            rule_kind: "cc_library".to_string(),
        }];
        let report = scan_external_repos(&artifacts, &repo_paths, &RealScannerFs);
        // BUILD lives under snappy/snappy/BUILD; the scanner
        // checks repo_root/BUILD which doesn't exist here.
        // That should classify as MissingBuildFile.
        assert_eq!(report.entries[0].has_repo_dir, true);
        assert_eq!(report.entries[0].has_build, false);
        assert_eq!(report.entries[0].has_build_bazel, false);
        assert_eq!(
            report.entries[0].materialization_status,
            MaterializationStatus::MissingBuildFile
        );
    }

    /// AJ-4 fixture #2-variant: BUILD at repo_root, declared
    /// .cc missing → BuildOnly. The realistic @snappy shape.
    #[test]
    fn aj4_fixture_build_only_repo_root_layout() {
        let tmp = TempDir::new().unwrap();
        let repo_root = tmp.path().join("snappy");
        std::fs::create_dir_all(&repo_root).unwrap();
        write_file(&repo_root, "BUILD", "# fixture");
        let mut repo_paths = HashMap::new();
        repo_paths.insert("snappy".to_string(), repo_root.clone());
        let declared_path = repo_root.join("snappy.cc");
        // Do NOT create snappy.cc.
        let artifacts = vec![DeclaredArtifact {
            repo_name: "snappy".to_string(),
            artifact_path: declared_path,
            target_label: "@snappy//snappy:snappy".to_string(),
            rule_kind: "cc_library".to_string(),
        }];
        let report = scan_external_repos(&artifacts, &repo_paths, &RealScannerFs);
        assert_eq!(report.entries[0].has_repo_dir, true);
        assert_eq!(report.entries[0].has_build, true);
        assert_eq!(
            report.entries[0].materialization_status,
            MaterializationStatus::BuildOnly
        );
    }

    /// AJ-4 fixture #3 (Pro spec): repo dir missing entirely
    /// → MissingRepoDir.
    #[test]
    fn aj4_fixture_repo_dir_missing() {
        let tmp = TempDir::new().unwrap();
        // No directory created under tmp; the repo_root path
        // doesn't exist.
        let repo_root = tmp.path().join("nonexistent");
        let mut repo_paths = HashMap::new();
        repo_paths.insert("nonexistent".to_string(), repo_root.clone());
        let artifacts = vec![DeclaredArtifact {
            repo_name: "nonexistent".to_string(),
            artifact_path: repo_root.join("src.cc"),
            target_label: "@nonexistent//pkg:src".to_string(),
            rule_kind: "cc_library".to_string(),
        }];
        let report = scan_external_repos(&artifacts, &repo_paths, &RealScannerFs);
        assert_eq!(
            report.entries[0].materialization_status,
            MaterializationStatus::MissingRepoDir
        );
        assert_eq!(report.counts.missing_repo_dir, 1);
    }

    /// AJ-4 fixture #4 (Pro spec): partial materialization
    /// → MissingSources (some present, some absent).
    #[test]
    fn aj4_fixture_partial_materialization() {
        let tmp = TempDir::new().unwrap();
        let repo_root = tmp.path().join("absl");
        std::fs::create_dir_all(&repo_root).unwrap();
        write_file(&repo_root, "BUILD", "# fixture");
        let strings_h = write_file(&repo_root, "strings/strings.h", "/* */");
        // Note: do NOT create strings.cc.
        let mut repo_paths = HashMap::new();
        repo_paths.insert("absl".to_string(), repo_root.clone());
        let artifacts = vec![
            DeclaredArtifact {
                repo_name: "absl".to_string(),
                artifact_path: repo_root.join("strings/strings.cc"),
                target_label: "@absl//strings:strings".to_string(),
                rule_kind: "cc_library".to_string(),
            },
            DeclaredArtifact {
                repo_name: "absl".to_string(),
                artifact_path: strings_h.clone(),
                target_label: "@absl//strings:strings".to_string(),
                rule_kind: "cc_library".to_string(),
            },
        ];
        let report = scan_external_repos(&artifacts, &repo_paths, &RealScannerFs);
        assert_eq!(report.entries[0].existing_source_count, 1);
        assert_eq!(report.entries[0].missing_source_count, 1);
        assert_eq!(
            report.entries[0].materialization_status,
            MaterializationStatus::MissingSources
        );
    }

    /// AJ-4 fixture #5 (Pro spec): generated files with
    /// producers are NOT counted as missing sources by this
    /// report. The contract is upstream: the orchestrator-side
    /// adapter filters generated artifacts (is_source == false)
    /// before passing them to scan_external_repos. The scanner
    /// itself never sees generated inputs. This test pins the
    /// contract by demonstrating that if the caller correctly
    /// omits the generated input, the report reflects only the
    /// sources (the on-disk truth).
    #[test]
    fn aj4_fixture_generated_files_not_in_scan() {
        let tmp = TempDir::new().unwrap();
        let repo_root = tmp.path().join("proto_repo");
        std::fs::create_dir_all(&repo_root).unwrap();
        write_file(&repo_root, "BUILD", "# fixture");
        let real_src = write_file(&repo_root, "real.cc", "/* fixture */");
        let mut repo_paths = HashMap::new();
        repo_paths.insert("proto_repo".to_string(), repo_root.clone());
        // The caller (orchestrator) is expected to filter
        // is_source==false artifacts. Here we simulate by NOT
        // passing the generated input at all.
        let artifacts = vec![DeclaredArtifact {
            repo_name: "proto_repo".to_string(),
            artifact_path: real_src.clone(),
            target_label: "@proto_repo//pkg:lib".to_string(),
            rule_kind: "cc_library".to_string(),
        }];
        let report = scan_external_repos(&artifacts, &repo_paths, &RealScannerFs);
        // The real source counts. The generated input (had it
        // been passed) would have been counted too — the
        // filter is the caller's responsibility, validated at
        // the orchestrator-side adapter.
        assert_eq!(report.entries[0].declared_source_count, 1);
        assert_eq!(report.entries[0].existing_source_count, 1);
        assert_eq!(
            report.entries[0].materialization_status,
            MaterializationStatus::Complete
        );
    }

    // AJ2-1 stub identity model tests

    const SNAPPY_STUB_CONTENT: &str = r#"# Stub BUILD for snappy/snappy - Rush uses system snappy
package(default_visibility = ["//visibility:public"])

cc_library(
    name = "snappy",
    srcs = [],
    hdrs = [],
    includes = ["/usr/include"],
    linkopts = ["-lsnappy"],
    visibility = ["//visibility:public"],
)
"#;

    const CUDA_DISABLED_STUB_CONTENT: &str = r#"# Stub BUILD for local_config_cuda (CUDA disabled)
package(default_visibility = ["//visibility:public"])
cc_library(name = "cuda", srcs = [], hdrs = [])
"#;

    const EIGEN_HEADER_ONLY_STUB_CONTENT: &str = r#"# Stub BUILD for eigen_archive - Rush uses system eigen
package(default_visibility = ["//visibility:public"])
cc_library(
    name = "eigen",
    srcs = [],
    hdrs = [],
    includes = ["/usr/include/eigen3"],
    visibility = ["//visibility:public"],
)
"#;

    const REAL_CC_LIBRARY_CONTENT: &str = r#"package(default_visibility = ["//visibility:public"])
licenses(["notice"])

cc_library(
    name = "real",
    srcs = ["real.cc"],
    hdrs = ["real.h"],
)
"#;

    /// AJ2-1: extract_stub_attrs parses linkopts + system
    /// includes from a SystemLibrary stub.
    #[test]
    fn aj2_extract_stub_attrs_system_library() {
        let (linkopts, includes) = extract_stub_attrs(SNAPPY_STUB_CONTENT);
        assert_eq!(linkopts, vec!["-lsnappy"]);
        assert_eq!(includes, vec!["/usr/include"]);
    }

    /// AJ2-1: extract_stub_attrs handles empty linkopts.
    #[test]
    fn aj2_extract_stub_attrs_empty_linkopts() {
        let (linkopts, includes) = extract_stub_attrs(CUDA_DISABLED_STUB_CONTENT);
        assert!(linkopts.is_empty());
        assert!(includes.is_empty());
    }

    /// AJ2-1: extract_stub_attrs filters non-absolute include
    /// paths out (workspace-relative `.` includes don't count
    /// as system includes). Stub BUILDs use the `includes = [...]`
    /// shape with spaces around `=`, matching what
    /// external_deps.rs emits.
    #[test]
    fn aj2_extract_stub_attrs_filters_relative_includes() {
        let content =
            "cc_library(name = \"x\", srcs = [\"x.cc\"], includes = [\".\", \"/usr/include\"])\n";
        let (linkopts, includes) = extract_stub_attrs(content);
        assert!(linkopts.is_empty());
        assert_eq!(includes, vec!["/usr/include"]);
    }

    /// AJ2-1 Pro directive test: snappy stub is classified as
    /// SystemLibrary.
    #[test]
    fn aj2_snappy_stub_detected_as_system_library() {
        let marker = detect_stub_marker(SNAPPY_STUB_CONTENT).unwrap();
        let info = build_stub_info(
            "snappy".to_string(),
            PathBuf::from("/cache/external/snappy/snappy/BUILD"),
            SNAPPY_STUB_CONTENT,
            marker.clone(),
        )
        .unwrap();
        assert_eq!(info.kind, ExternalRepoStubKind::SystemLibrary);
        assert_eq!(info.linkopts, vec!["-lsnappy"]);
        assert_eq!(info.system_includes, vec!["/usr/include"]);
        assert!(!info.has_sources);
        assert!(!info.has_headers);
        assert!(info.marker_detected.contains("snappy"));
    }

    /// AJ2-1 Pro directive test: feature-disabled stub
    /// (CUDA/NCCL/ROCm/TensorRT) is classified as
    /// FeatureDisabled.
    #[test]
    fn aj2_feature_disabled_stub_classified() {
        let marker = detect_stub_marker(CUDA_DISABLED_STUB_CONTENT).unwrap();
        let info = build_stub_info(
            "local_config_cuda".to_string(),
            PathBuf::from("/cache/external/local_config_cuda/BUILD"),
            CUDA_DISABLED_STUB_CONTENT,
            marker,
        )
        .unwrap();
        assert_eq!(info.kind, ExternalRepoStubKind::FeatureDisabled);
        assert!(info.linkopts.is_empty());
        assert!(info.system_includes.is_empty());
        assert!(info.marker_detected.contains("disabled"));
    }

    /// AJ2-1 Pro directive test: header-only stub (no linkopts,
    /// has system includes) is classified as HeaderOnly.
    #[test]
    fn aj2_header_only_stub_classified() {
        let marker = detect_stub_marker(EIGEN_HEADER_ONLY_STUB_CONTENT).unwrap();
        let info = build_stub_info(
            "eigen_archive".to_string(),
            PathBuf::from("/cache/external/eigen_archive/BUILD"),
            EIGEN_HEADER_ONLY_STUB_CONTENT,
            marker,
        )
        .unwrap();
        assert_eq!(info.kind, ExternalRepoStubKind::HeaderOnly);
        assert!(info.linkopts.is_empty());
        assert_eq!(info.system_includes, vec!["/usr/include/eigen3"]);
    }

    /// AJ2-1 Pro directive test: a real (non-stub) BUILD is
    /// NOT classified as a stub. build_stub_info returns None
    /// when the marker line is empty (which is what
    /// detect_stub_marker returns for non-stubs).
    #[test]
    fn aj2_non_stub_build_not_classified() {
        assert_eq!(detect_stub_marker(REAL_CC_LIBRARY_CONTENT), None);
        let info = build_stub_info(
            "real_repo".to_string(),
            PathBuf::from("/repo/BUILD"),
            REAL_CC_LIBRARY_CONTENT,
            String::new(),
        );
        assert!(info.is_none());
    }

    /// AJ2-1: Python stub classifier path (absl_py special-case).
    #[test]
    fn aj2_python_stub_classified() {
        let content = "# Stub BUILD for absl_py - Rush uses system absl_py\n\
                       py_library(name = \"absl_py\", srcs = [])\n";
        let marker = detect_stub_marker(content).unwrap();
        let info = build_stub_info(
            "absl_py".to_string(),
            PathBuf::from("/cache/external/absl_py/BUILD"),
            content,
            marker,
        )
        .unwrap();
        assert_eq!(info.kind, ExternalRepoStubKind::PythonStub);
    }

    /// AJ2-1: misc stub fallback (stub marker present but no
    /// linkopts, no includes, no disabled marker).
    #[test]
    fn aj2_misc_stub_fallback() {
        let content =
            "# Stub BUILD for foobar - placeholder\ncc_library(name = \"foobar\", srcs = [])\n";
        let marker = detect_stub_marker(content).unwrap();
        let info = build_stub_info(
            "foobar".to_string(),
            PathBuf::from("/cache/external/foobar/BUILD"),
            content,
            marker,
        )
        .unwrap();
        assert_eq!(info.kind, ExternalRepoStubKind::Misc);
    }

    /// AJ2-1: classify_stub_kind returns Unknown only for the
    /// fully-empty defensive case.
    #[test]
    fn aj2_classify_stub_unknown_only_when_all_empty() {
        assert_eq!(
            classify_stub_kind("snappy", "", &[], &[]),
            ExternalRepoStubKind::Unknown
        );
        // Any non-empty signal pushes us off Unknown.
        assert_ne!(
            classify_stub_kind(
                "snappy",
                "# Stub BUILD for snappy",
                &[],
                &[]
            ),
            ExternalRepoStubKind::Unknown
        );
    }

    /// AJ2-1: ExternalRepoStubKind labels are stable
    /// kebab-case.
    #[test]
    fn aj2_stub_kind_labels_stable_snake_case() {
        assert_eq!(ExternalRepoStubKind::SystemLibrary.label(), "system_library");
        assert_eq!(ExternalRepoStubKind::HeaderOnly.label(), "header_only");
        assert_eq!(
            ExternalRepoStubKind::FeatureDisabled.label(),
            "feature_disabled"
        );
        assert_eq!(ExternalRepoStubKind::PythonStub.label(), "python_stub");
        assert_eq!(ExternalRepoStubKind::Misc.label(), "misc");
        assert_eq!(ExternalRepoStubKind::Unknown.label(), "unknown");
    }

    /// AJ2-1: ExternalRepoStubInfo serializes with the expected
    /// snake_case JSON shape.
    #[test]
    fn aj2_stub_info_json_shape() {
        let info = ExternalRepoStubInfo {
            repo_name: "snappy".to_string(),
            build_file_path: PathBuf::from("/cache/external/snappy/BUILD"),
            kind: ExternalRepoStubKind::SystemLibrary,
            system_includes: vec!["/usr/include".to_string()],
            linkopts: vec!["-lsnappy".to_string()],
            has_sources: false,
            has_headers: false,
            marker_detected: "# Stub BUILD for snappy/snappy - Rush uses system snappy"
                .to_string(),
        };
        let json = serde_json::to_string(&info).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["repo_name"], "snappy");
        assert_eq!(parsed["kind"]["kind"], "system_library");
        assert_eq!(parsed["system_includes"][0], "/usr/include");
        assert_eq!(parsed["linkopts"][0], "-lsnappy");
        assert_eq!(parsed["has_sources"], false);
        assert_eq!(parsed["has_headers"], false);
        assert!(parsed["marker_detected"]
            .as_str()
            .unwrap()
            .contains("snappy"));
    }

    // AK-1 stub detection tests

    /// AK-1: detect_stub_marker returns the marker line for a
    /// canonical "# Stub BUILD" header.
    #[test]
    fn ak1_detect_stub_marker_canonical_form() {
        let content = "# Stub BUILD for snappy/snappy - Rush uses system snappy\n\
                       package(default_visibility = [\"//visibility:public\"])\n";
        let m = detect_stub_marker(content);
        assert!(m.is_some());
        let m = m.unwrap();
        assert!(m.contains("Stub BUILD"));
        assert!(m.contains("snappy"));
    }

    /// AK-1: detect_stub_marker recognizes the "# Rush stub" form.
    #[test]
    fn ak1_detect_stub_marker_rush_stub_form() {
        let content = "# Rush stub for local_config_cuda (CUDA disabled)\n";
        let m = detect_stub_marker(content);
        assert!(m.is_some());
    }

    /// AK-1: detect_stub_marker recognizes the "Rush uses system"
    /// form even without a `#` comment prefix.
    #[test]
    fn ak1_detect_stub_marker_rush_uses_system_form() {
        let content = "# Some comment\n# Rush uses system protobuf\n";
        let m = detect_stub_marker(content);
        assert!(m.is_some());
    }

    /// AK-1: a non-stub BUILD file returns None.
    #[test]
    fn ak1_detect_stub_marker_non_stub_returns_none() {
        let content = "package(default_visibility = [\"//visibility:public\"])\n\
                       licenses([\"notice\"])\n\
                       cc_library(name = \"real\", srcs = [\"real.cc\"])\n";
        assert_eq!(detect_stub_marker(content), None);
    }

    /// AK-1: empty content returns None (no panic).
    #[test]
    fn ak1_detect_stub_marker_empty_content() {
        assert_eq!(detect_stub_marker(""), None);
    }

    /// AK-1: marker beyond the head window is NOT detected (the
    /// bound is intentional — stubs put markers at the top).
    #[test]
    fn ak1_detect_stub_marker_only_inspects_head() {
        // 600 bytes of non-stub content, then a marker.
        let prefix = "x".repeat(600);
        let content = format!("{}\n# Stub BUILD\n", prefix);
        // The head inspection is capped at 256 chars + 3 lines.
        assert_eq!(detect_stub_marker(&content), None);
    }

    /// AK-1: scanner with a content-providing FakeFs populates
    /// is_stub_build = Some(true) for stub BUILDs.
    #[test]
    fn ak1_scanner_populates_is_stub_build_true() {
        struct StubAwareFs {
            files: HashSet<PathBuf>,
            dirs: HashSet<PathBuf>,
            contents: HashMap<PathBuf, String>,
        }
        impl ScannerFs for StubAwareFs {
            fn file_exists(&self, p: &Path) -> bool {
                self.files.contains(p)
            }
            fn dir_exists(&self, p: &Path) -> bool {
                self.dirs.contains(p)
            }
            fn read_head(&self, p: &Path, _max: usize) -> Option<String> {
                self.contents.get(p).cloned()
            }
        }
        let mut files = HashSet::new();
        files.insert(PathBuf::from("/cache/external/snappy/BUILD"));
        let mut dirs = HashSet::new();
        dirs.insert(PathBuf::from("/cache/external/snappy"));
        let mut contents = HashMap::new();
        contents.insert(
            PathBuf::from("/cache/external/snappy/BUILD"),
            "# Stub BUILD for snappy - Rush uses system snappy\n".to_string(),
        );
        let fs = StubAwareFs { files, dirs, contents };
        let mut repo_paths = HashMap::new();
        repo_paths
            .insert("snappy".to_string(), PathBuf::from("/cache/external/snappy"));
        let artifacts = vec![art(
            "snappy",
            "/cache/external/snappy/snappy.cc",
            "@snappy//snappy:snappy",
            "cc_library",
        )];
        let report = scan_external_repos(&artifacts, &repo_paths, &fs);
        assert_eq!(report.entries[0].is_stub_build, Some(true));
        assert!(report.entries[0]
            .stub_marker
            .as_ref()
            .unwrap()
            .contains("Stub BUILD"));
        assert_eq!(report.counts.stub_builds, 1);
    }

    /// BUILD.bazel controls stub classification when both package files exist.
    #[test]
    fn ak1_scanner_prefers_real_build_bazel_over_stub_build() {
        struct StubAwareFs {
            files: HashSet<PathBuf>,
            dirs: HashSet<PathBuf>,
            contents: HashMap<PathBuf, String>,
        }
        impl ScannerFs for StubAwareFs {
            fn file_exists(&self, p: &Path) -> bool {
                self.files.contains(p)
            }
            fn dir_exists(&self, p: &Path) -> bool {
                self.dirs.contains(p)
            }
            fn read_head(&self, p: &Path, _max: usize) -> Option<String> {
                self.contents.get(p).cloned()
            }
        }
        let mut files = HashSet::new();
        files.insert(PathBuf::from("/repo/BUILD"));
        files.insert(PathBuf::from("/repo/BUILD.bazel"));
        files.insert(PathBuf::from("/repo/foo.cc"));
        let mut dirs = HashSet::new();
        dirs.insert(PathBuf::from("/repo"));
        let mut contents = HashMap::new();
        contents.insert(
            PathBuf::from("/repo/BUILD"),
            "# Stub BUILD generated by Rush\n".to_string(),
        );
        contents.insert(
            PathBuf::from("/repo/BUILD.bazel"),
            "package(default_visibility = [\"//visibility:public\"])\n\
             cc_library(name = \"real\", srcs = [\"foo.cc\"])\n"
                .to_string(),
        );
        let fs = StubAwareFs { files, dirs, contents };
        let mut repo_paths = HashMap::new();
        repo_paths.insert("real_repo".to_string(), PathBuf::from("/repo"));
        let artifacts = vec![art(
            "real_repo",
            "/repo/foo.cc",
            "@real_repo//:real",
            "cc_library",
        )];
        let report = scan_external_repos(&artifacts, &repo_paths, &fs);
        assert_eq!(report.entries[0].is_stub_build, Some(false));
        assert_eq!(report.entries[0].stub_marker, None);
        assert_eq!(report.counts.stub_builds, 0);
    }

    /// AK-1: scanner with the default ScannerFs (no read_head
    /// override) leaves is_stub_build = None.
    #[test]
    fn ak1_scanner_default_fs_leaves_stub_detection_none() {
        let fs = FakeFs::new()
            .with_dir("/repo")
            .with_file("/repo/BUILD")
            .with_file("/repo/foo.cc");
        let mut repo_paths = HashMap::new();
        repo_paths.insert("repo".to_string(), PathBuf::from("/repo"));
        let artifacts = vec![art(
            "repo",
            "/repo/foo.cc",
            "@repo//:foo",
            "cc_library",
        )];
        let report = scan_external_repos(&artifacts, &repo_paths, &fs);
        // FakeFs uses the default read_head impl → None.
        assert_eq!(report.entries[0].is_stub_build, None);
        assert_eq!(report.entries[0].stub_marker, None);
        assert_eq!(report.counts.stub_builds, 0);
    }

    /// AK-1: report counts stub_builds correctly across mixed
    /// entries.
    #[test]
    fn ak1_report_aggregates_stub_builds_count() {
        let e1 = ExternalRepoMaterialization::from_declarations(
            "stub1".to_string(),
            PathBuf::from("/r1"),
            true,
            true,
            false,
            &[],
            &HashSet::new(),
        )
        .with_stub_detection(true, Some("# Stub".to_string()));
        let e2 = ExternalRepoMaterialization::from_declarations(
            "stub2".to_string(),
            PathBuf::from("/r2"),
            true,
            true,
            false,
            &[],
            &HashSet::new(),
        )
        .with_stub_detection(true, Some("# Rush stub".to_string()));
        let e3 = ExternalRepoMaterialization::from_declarations(
            "real".to_string(),
            PathBuf::from("/r3"),
            true,
            true,
            false,
            &[],
            &HashSet::new(),
        )
        .with_stub_detection(false, None);
        let report = ExternalRepoMaterializationReport::from_entries(vec![e1, e2, e3]);
        assert_eq!(report.counts.stub_builds, 2);
    }

    /// AK-1: stub_marker JSON shape (snake_case fields, optional).
    #[test]
    fn ak1_stub_fields_serialize_with_snake_case() {
        let mut e = ExternalRepoMaterialization::from_declarations(
            "snappy".to_string(),
            PathBuf::from("/cache/external/snappy"),
            true,
            true,
            false,
            &[],
            &HashSet::new(),
        );
        e = e.with_stub_detection(
            true,
            Some("# Stub BUILD for snappy".to_string()),
        );
        let json = serde_json::to_string(&e).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["is_stub_build"], true);
        assert_eq!(parsed["stub_marker"], "# Stub BUILD for snappy");
    }

    /// AJ-4 end-to-end fixture: multiple repos with mixed
    /// states (Complete + BuildOnly + MissingSources +
    /// MissingRepoDir) all present in one scan. Mirrors the
    /// shape AJ-5 will see when run on a real TF target whose
    /// closure crosses many repos.
    #[test]
    fn aj4_fixture_mixed_multi_repo_scan() {
        let tmp = TempDir::new().unwrap();
        // zlib: complete (BUILD + all sources)
        let zlib_root = tmp.path().join("zlib");
        std::fs::create_dir_all(&zlib_root).unwrap();
        write_file(&zlib_root, "BUILD", "# fixture");
        let zlib_c = write_file(&zlib_root, "zlib.c", "/* */");
        // snappy: build only (BUILD, no sources)
        let snappy_root = tmp.path().join("snappy");
        std::fs::create_dir_all(&snappy_root).unwrap();
        write_file(&snappy_root, "BUILD", "# fixture");
        // absl: partial (BUILD + 1 of 2 sources)
        let absl_root = tmp.path().join("absl");
        std::fs::create_dir_all(&absl_root).unwrap();
        write_file(&absl_root, "BUILD", "# fixture");
        let absl_h = write_file(&absl_root, "strings.h", "/* */");
        // nonexistent: dir missing entirely
        let nonexistent_root = tmp.path().join("nonexistent");
        // No mkdir for nonexistent.

        let mut repo_paths = HashMap::new();
        repo_paths.insert("zlib".to_string(), zlib_root.clone());
        repo_paths.insert("snappy".to_string(), snappy_root.clone());
        repo_paths.insert("absl".to_string(), absl_root.clone());
        repo_paths.insert("nonexistent".to_string(), nonexistent_root.clone());

        let artifacts = vec![
            DeclaredArtifact {
                repo_name: "zlib".to_string(),
                artifact_path: zlib_c.clone(),
                target_label: "@zlib//zlib:zlib".to_string(),
                rule_kind: "cc_library".to_string(),
            },
            DeclaredArtifact {
                repo_name: "snappy".to_string(),
                artifact_path: snappy_root.join("snappy.cc"),
                target_label: "@snappy//snappy:snappy".to_string(),
                rule_kind: "cc_library".to_string(),
            },
            DeclaredArtifact {
                repo_name: "absl".to_string(),
                artifact_path: absl_root.join("strings.cc"),
                target_label: "@absl//strings:strings".to_string(),
                rule_kind: "cc_library".to_string(),
            },
            DeclaredArtifact {
                repo_name: "absl".to_string(),
                artifact_path: absl_h.clone(),
                target_label: "@absl//strings:strings".to_string(),
                rule_kind: "cc_library".to_string(),
            },
            DeclaredArtifact {
                repo_name: "nonexistent".to_string(),
                artifact_path: nonexistent_root.join("x.cc"),
                target_label: "@nonexistent//pkg:x".to_string(),
                rule_kind: "cc_library".to_string(),
            },
        ];
        let report = scan_external_repos(&artifacts, &repo_paths, &RealScannerFs);
        assert_eq!(report.counts.complete, 1);
        assert_eq!(report.counts.build_only, 1);
        assert_eq!(report.counts.missing_sources, 1);
        assert_eq!(report.counts.missing_repo_dir, 1);
        assert_eq!(report.counts.blocked_total(), 3);
        // Sorted by repo_name asc.
        let names: Vec<&str> = report.entries.iter().map(|e| e.repo_name.as_str()).collect();
        assert_eq!(names, vec!["absl", "nonexistent", "snappy", "zlib"]);
    }

    /// AJ-2 directive test: rule_kinds and target_labels for missing
    /// sources aggregate distinct values across declarations.
    #[test]
    fn scan_aggregates_rule_kinds_and_target_labels_for_missing() {
        let fs = FakeFs::new()
            .with_dir("/cache/external/multi")
            .with_file("/cache/external/multi/BUILD");
        let mut repo_paths = HashMap::new();
        repo_paths
            .insert("multi".to_string(), PathBuf::from("/cache/external/multi"));
        let artifacts = vec![
            art(
                "multi",
                "/cache/external/multi/a.cc",
                "@multi//pkg:a",
                "cc_library",
            ),
            art(
                "multi",
                "/cache/external/multi/b.cc",
                "@multi//pkg:b",
                "cc_binary",
            ),
            art(
                "multi",
                "/cache/external/multi/c.proto",
                "@multi//pkg:c",
                "proto_library",
            ),
        ];
        let report = scan_external_repos(&artifacts, &repo_paths, &fs);
        let e = &report.entries[0];
        assert_eq!(e.declared_source_count, 3);
        assert_eq!(e.missing_source_count, 3);
        // 3 distinct rule kinds, sorted.
        assert_eq!(
            e.rule_kinds_referencing_missing_sources,
            vec!["cc_binary", "cc_library", "proto_library"]
        );
        // 3 distinct target labels, sorted.
        assert_eq!(
            e.target_labels_referencing_missing_sources,
            vec!["@multi//pkg:a", "@multi//pkg:b", "@multi//pkg:c"]
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::PathBuf;

    #[test]
    fn classify_missing_repo_dir() {
        assert_eq!(
            classify(false, false, 0, 0),
            MaterializationStatus::MissingRepoDir
        );
        // Repo missing trumps build-file state.
        assert_eq!(
            classify(false, true, 3, 0),
            MaterializationStatus::MissingRepoDir
        );
    }

    #[test]
    fn classify_missing_build_file() {
        // Dir present but no BUILD anywhere.
        assert_eq!(
            classify(true, false, 0, 0),
            MaterializationStatus::MissingBuildFile
        );
    }

    #[test]
    fn classify_build_only_zero_declared() {
        // Dir + BUILD present, no declared sources in scope (the
        // scanner saw the repo but the closure didn't anchor any
        // sources there).
        assert_eq!(
            classify(true, true, 0, 0),
            MaterializationStatus::BuildOnly
        );
    }

    #[test]
    fn classify_build_only_declared_but_none_present() {
        // The @snappy case from AB-2: dir + BUILD present, 3 sources
        // declared, 0 on disk.
        assert_eq!(
            classify(true, true, 3, 0),
            MaterializationStatus::BuildOnly
        );
    }

    #[test]
    fn classify_complete() {
        // 3/3 present.
        assert_eq!(
            classify(true, true, 3, 3),
            MaterializationStatus::Complete
        );
    }

    #[test]
    fn classify_partial_materialization() {
        // 2/3 present.
        assert_eq!(
            classify(true, true, 3, 2),
            MaterializationStatus::MissingSources
        );
        // 1/3 present.
        assert_eq!(
            classify(true, true, 3, 1),
            MaterializationStatus::MissingSources
        );
    }

    #[test]
    fn status_is_complete_is_blocked_invariants() {
        assert!(MaterializationStatus::Complete.is_complete());
        assert!(!MaterializationStatus::Complete.is_blocked());
        assert!(!MaterializationStatus::Unknown.is_blocked());
        assert!(!MaterializationStatus::Unknown.is_complete());
        for s in [
            MaterializationStatus::BuildOnly,
            MaterializationStatus::MissingRepoDir,
            MaterializationStatus::MissingBuildFile,
            MaterializationStatus::MissingSources,
        ] {
            assert!(s.is_blocked(), "{:?} should be blocked", s);
            assert!(!s.is_complete(), "{:?} should not be complete", s);
        }
    }

    #[test]
    fn label_stable_kebab_case() {
        assert_eq!(MaterializationStatus::Complete.label(), "complete");
        assert_eq!(MaterializationStatus::BuildOnly.label(), "build_only");
        assert_eq!(
            MaterializationStatus::MissingRepoDir.label(),
            "missing_repo_dir"
        );
        assert_eq!(
            MaterializationStatus::MissingBuildFile.label(),
            "missing_build_file"
        );
        assert_eq!(
            MaterializationStatus::MissingSources.label(),
            "missing_sources"
        );
        assert_eq!(MaterializationStatus::Unknown.label(), "unknown");
    }

    fn art(repo: &str, path: &str, target: &str, rule: &str) -> DeclaredArtifact {
        DeclaredArtifact {
            repo_name: repo.to_string(),
            artifact_path: PathBuf::from(path),
            target_label: target.to_string(),
            rule_kind: rule.to_string(),
        }
    }

    /// AJ-1 directive shape: an external repo with BUILD present and
    /// all declared sources missing is classified BuildOnly and the
    /// entry exposes the missing examples + rule kinds + target
    /// labels for operator triage.
    #[test]
    fn entry_snappy_class_build_only() {
        let declared = vec![
            art(
                "snappy",
                "/cache/external/snappy/snappy/snappy.cc",
                "@snappy//snappy:snappy",
                "cc_library",
            ),
            art(
                "snappy",
                "/cache/external/snappy/snappy/snappy-sinksource.cc",
                "@snappy//snappy:snappy",
                "cc_library",
            ),
            art(
                "snappy",
                "/cache/external/snappy/snappy/snappy-stubs-internal.cc",
                "@snappy//snappy:snappy",
                "cc_library",
            ),
        ];
        // Existing paths: none.
        let existing: HashSet<PathBuf> = HashSet::new();
        let entry = ExternalRepoMaterialization::from_declarations(
            "snappy".to_string(),
            PathBuf::from("/cache/external/snappy"),
            true,  // has_repo_dir
            true,  // has_build
            false, // has_build_bazel
            &declared,
            &existing,
        );
        assert_eq!(entry.repo_name, "snappy");
        assert_eq!(entry.has_repo_dir, true);
        assert_eq!(entry.has_build, true);
        assert_eq!(entry.declared_source_count, 3);
        assert_eq!(entry.existing_source_count, 0);
        assert_eq!(entry.missing_source_count, 3);
        assert_eq!(
            entry.materialization_status,
            MaterializationStatus::BuildOnly
        );
        assert_eq!(entry.missing_source_examples.len(), 3);
        // Examples sorted.
        let s: Vec<&str> = entry
            .missing_source_examples
            .iter()
            .map(|p| p.to_str().unwrap())
            .collect();
        assert!(s.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(entry.rule_kinds_referencing_missing_sources, vec!["cc_library"]);
        assert_eq!(
            entry.target_labels_referencing_missing_sources,
            vec!["@snappy//snappy:snappy"]
        );
    }

    /// AJ-1 directive shape: a fully-materialized repo classifies as
    /// Complete and reports zero missing.
    #[test]
    fn entry_complete_repo() {
        let declared = vec![
            art(
                "zlib",
                "/cache/external/zlib/zlib/zlib.c",
                "@zlib//zlib:zlib",
                "cc_library",
            ),
            art(
                "zlib",
                "/cache/external/zlib/zlib/zlib.h",
                "@zlib//zlib:zlib",
                "cc_library",
            ),
        ];
        let mut existing = HashSet::new();
        existing.insert(PathBuf::from("/cache/external/zlib/zlib/zlib.c"));
        existing.insert(PathBuf::from("/cache/external/zlib/zlib/zlib.h"));
        let entry = ExternalRepoMaterialization::from_declarations(
            "zlib".to_string(),
            PathBuf::from("/cache/external/zlib"),
            true,
            true,
            false,
            &declared,
            &existing,
        );
        assert_eq!(entry.declared_source_count, 2);
        assert_eq!(entry.existing_source_count, 2);
        assert_eq!(entry.missing_source_count, 0);
        assert_eq!(
            entry.materialization_status,
            MaterializationStatus::Complete
        );
        assert!(entry.missing_source_examples.is_empty());
        assert!(entry.rule_kinds_referencing_missing_sources.is_empty());
        assert!(entry.target_labels_referencing_missing_sources.is_empty());
    }

    /// AJ-1 directive shape: a partially-materialized repo classifies
    /// as MissingSources (distinct from BuildOnly).
    #[test]
    fn entry_partial_materialization() {
        let declared = vec![
            art(
                "absl",
                "/cache/external/absl/strings.cc",
                "@absl//strings:strings",
                "cc_library",
            ),
            art(
                "absl",
                "/cache/external/absl/strings.h",
                "@absl//strings:strings",
                "cc_library",
            ),
            art(
                "absl",
                "/cache/external/absl/internal.cc",
                "@absl//strings:internal",
                "cc_library",
            ),
        ];
        let mut existing = HashSet::new();
        existing.insert(PathBuf::from("/cache/external/absl/strings.h"));
        let entry = ExternalRepoMaterialization::from_declarations(
            "absl".to_string(),
            PathBuf::from("/cache/external/absl"),
            true,
            true,
            false,
            &declared,
            &existing,
        );
        assert_eq!(entry.declared_source_count, 3);
        assert_eq!(entry.existing_source_count, 1);
        assert_eq!(entry.missing_source_count, 2);
        assert_eq!(
            entry.materialization_status,
            MaterializationStatus::MissingSources
        );
        assert_eq!(entry.missing_source_examples.len(), 2);
        // Two distinct target labels.
        assert_eq!(
            entry.target_labels_referencing_missing_sources,
            vec!["@absl//strings:internal", "@absl//strings:strings"]
        );
    }

    /// AJ-1 directive shape: missing_source_examples is capped at
    /// MISSING_EXAMPLE_CAP entries even when more are missing.
    #[test]
    fn entry_missing_examples_capped() {
        let declared: Vec<DeclaredArtifact> = (0..20)
            .map(|i| {
                art(
                    "bigrepo",
                    &format!("/cache/external/bigrepo/file{:02}.cc", i),
                    "@bigrepo//pkg:bigrepo",
                    "cc_library",
                )
            })
            .collect();
        let existing: HashSet<PathBuf> = HashSet::new();
        let entry = ExternalRepoMaterialization::from_declarations(
            "bigrepo".to_string(),
            PathBuf::from("/cache/external/bigrepo"),
            true,
            true,
            false,
            &declared,
            &existing,
        );
        assert_eq!(entry.declared_source_count, 20);
        assert_eq!(entry.missing_source_count, 20);
        assert_eq!(entry.missing_source_examples.len(), MISSING_EXAMPLE_CAP);
    }

    /// AJ-1 directive shape: a repo with no declared sources in
    /// scope (header-only, used for include-only deps) classifies as
    /// BuildOnly, not MissingSources.
    #[test]
    fn entry_zero_declared_classifies_as_build_only() {
        let declared: Vec<DeclaredArtifact> = Vec::new();
        let existing: HashSet<PathBuf> = HashSet::new();
        let entry = ExternalRepoMaterialization::from_declarations(
            "header_only_repo".to_string(),
            PathBuf::from("/cache/external/header_only_repo"),
            true,
            true,
            false,
            &declared,
            &existing,
        );
        assert_eq!(entry.declared_source_count, 0);
        assert_eq!(
            entry.materialization_status,
            MaterializationStatus::BuildOnly
        );
        assert!(entry.missing_source_examples.is_empty());
    }

    /// AJ-1 directive shape: a repo whose directory is missing
    /// classifies as MissingRepoDir.
    #[test]
    fn entry_missing_repo_dir() {
        let declared = vec![art(
            "nonexistent",
            "/cache/external/nonexistent/src.cc",
            "@nonexistent//pkg:src",
            "cc_library",
        )];
        let existing: HashSet<PathBuf> = HashSet::new();
        let entry = ExternalRepoMaterialization::from_declarations(
            "nonexistent".to_string(),
            PathBuf::from("/cache/external/nonexistent"),
            false, // has_repo_dir
            false,
            false,
            &declared,
            &existing,
        );
        assert_eq!(
            entry.materialization_status,
            MaterializationStatus::MissingRepoDir
        );
    }

    /// AJ-1 directive shape: BUILD.bazel counts as a BUILD file for
    /// classification.
    #[test]
    fn entry_build_bazel_counts_as_build_file() {
        let declared = vec![art(
            "foo",
            "/cache/external/foo/foo.cc",
            "@foo//pkg:foo",
            "cc_library",
        )];
        let mut existing = HashSet::new();
        existing.insert(PathBuf::from("/cache/external/foo/foo.cc"));
        let entry = ExternalRepoMaterialization::from_declarations(
            "foo".to_string(),
            PathBuf::from("/cache/external/foo"),
            true,
            false, // has_build
            true,  // has_build_bazel
            &declared,
            &existing,
        );
        assert_eq!(
            entry.materialization_status,
            MaterializationStatus::Complete
        );
    }

    /// AJ-1 directive shape: aggregating multiple repo entries
    /// produces sorted-by-name output and pre-computed status counts.
    #[test]
    fn aggregate_report_sorts_and_counts() {
        let snappy_decl = vec![art(
            "snappy",
            "/cache/external/snappy/snappy.cc",
            "@snappy//snappy:snappy",
            "cc_library",
        )];
        let snappy = ExternalRepoMaterialization::from_declarations(
            "snappy".to_string(),
            PathBuf::from("/cache/external/snappy"),
            true,
            true,
            false,
            &snappy_decl,
            &HashSet::new(),
        );
        let zlib_decl = vec![art(
            "zlib",
            "/cache/external/zlib/zlib.c",
            "@zlib//zlib:zlib",
            "cc_library",
        )];
        let mut zlib_existing = HashSet::new();
        zlib_existing.insert(PathBuf::from("/cache/external/zlib/zlib.c"));
        let zlib = ExternalRepoMaterialization::from_declarations(
            "zlib".to_string(),
            PathBuf::from("/cache/external/zlib"),
            true,
            true,
            false,
            &zlib_decl,
            &zlib_existing,
        );
        let absent = ExternalRepoMaterialization::from_declarations(
            "absent".to_string(),
            PathBuf::from("/cache/external/absent"),
            false,
            false,
            false,
            &[art(
                "absent",
                "/cache/external/absent/x.cc",
                "@absent//pkg:x",
                "cc_library",
            )],
            &HashSet::new(),
        );
        let report =
            ExternalRepoMaterializationReport::from_entries(vec![snappy, zlib, absent]);
        // Sorted by repo_name asc.
        let names: Vec<&str> = report.entries.iter().map(|e| e.repo_name.as_str()).collect();
        assert_eq!(names, vec!["absent", "snappy", "zlib"]);
        // Per-status counts.
        assert_eq!(report.counts.complete, 1);
        assert_eq!(report.counts.build_only, 1);
        assert_eq!(report.counts.missing_repo_dir, 1);
        assert_eq!(report.counts.missing_build_file, 0);
        assert_eq!(report.counts.missing_sources, 0);
        assert_eq!(report.counts.unknown, 0);
        assert_eq!(report.counts.blocked_total(), 2);
        // Blocked iterator skips Complete.
        let blocked: Vec<&str> = report
            .blocked()
            .map(|e| e.repo_name.as_str())
            .collect();
        assert_eq!(blocked, vec!["absent", "snappy"]);
    }

    /// AJ-1 directive shape: JSON schema is stable (schema_version
    /// field present, named fields).
    #[test]
    fn report_json_schema_stable() {
        let report = ExternalRepoMaterializationReport::new();
        assert_eq!(report.schema_version, 1);
        let json = serde_json::to_string(&report).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["schema_version"], 1);
        assert!(parsed["entries"].is_array());
        assert!(parsed["counts"].is_object());
        assert_eq!(parsed["counts"]["complete"], 0);
        assert_eq!(parsed["counts"]["build_only"], 0);
        assert_eq!(parsed["counts"]["missing_repo_dir"], 0);
        assert_eq!(parsed["counts"]["missing_build_file"], 0);
        assert_eq!(parsed["counts"]["missing_sources"], 0);
        assert_eq!(parsed["counts"]["unknown"], 0);
    }

    /// AJ-1 directive shape: generated files with producers are NOT
    /// counted as missing sources by this report (those belong to
    /// the existing materialization_report module, not this one).
    /// The scanner caller is responsible for filtering DeclaredArtifact
    /// list down to source files only; this test pins the contract.
    #[test]
    fn entry_only_counts_paths_caller_provides() {
        // If the caller hands the report a DeclaredArtifact, it
        // counts. If the caller filters generated inputs out
        // upstream, they won't appear. Verify the count math is
        // exactly what the caller provides.
        let declared = vec![
            art(
                "foo",
                "/cache/external/foo/src1.cc",
                "@foo//pkg:foo",
                "cc_library",
            ),
            art(
                "foo",
                "/cache/external/foo/src2.cc",
                "@foo//pkg:foo",
                "cc_library",
            ),
        ];
        let mut existing = HashSet::new();
        existing.insert(PathBuf::from("/cache/external/foo/src1.cc"));
        // src2 missing.
        let entry = ExternalRepoMaterialization::from_declarations(
            "foo".to_string(),
            PathBuf::from("/cache/external/foo"),
            true,
            true,
            false,
            &declared,
            &existing,
        );
        assert_eq!(entry.declared_source_count, 2);
        assert_eq!(entry.existing_source_count, 1);
        assert_eq!(
            entry.materialization_status,
            MaterializationStatus::MissingSources
        );
    }
}
