// Phase 5.C.4 commit 147 (Batch S): external repo
// materialization manifest.
//
// Batch Q's `RUSH_FETCH_SKIP_IF_MATERIALIZED=1`
// trusts "dir exists + BUILD exists" as proof a
// repo is usable. Pro's commit-145 policy doc and
// the Batch S green-light letter both note this is
// a *trust* mode, not a verified-cache protocol.
// This commit lays the data model for a Rush-owned
// sidecar manifest that, once populated by commit
// 148's writer, gives the skip path a real "I
// fetched this myself, here is the proof" record.
//
// CRITICAL per Pro's correction: the manifest does
// NOT live inside `external/<repo>/`. Third-party
// vendored directories must not be mutated by
// rush. The sidecar lives at
//   `<workspace>/.cache/rush/external-manifests/<repo>.json`
// — sibling to rush's own external cache
// (`workspace_external_dir`), separate from
// bazel's output_base and from the vendored
// external/ dir.
//
// This commit lands ONLY the types + serde wiring +
// path helper + tests. No call sites are wired;
// commit 148 will add the writer.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Phase 5.C.4 commit 147 (Batch S): schema version
/// for the external-repo manifest. Bumped only on
/// breaking changes to field shape. Additive changes
/// (new Optional fields) do NOT bump.
pub const EXTERNAL_REPO_MANIFEST_SCHEMA_VERSION: u32 = 1;

// =============================================================
// Phase 5.C.4 commit 176 (Batch W): typed external-repo
// identity model. Separate from the manifest sidecar so the
// trust decision is typed instead of "well it looked like a
// URL." Commits 177+ wire this into metadata extraction +
// fingerprinting + Valid-criteria.
// =============================================================

/// Phase 5.C.4 commit 176 (Batch W): typed identity
/// for an external repository. The variant + its
/// fields are the trust-relevant declared metadata
/// for the rule that produced this repo. Used by
/// commit 178's fingerprint and commit 179's
/// Valid-criteria reader.
///
/// Per Pro's commit-176 spec: do NOT shove raw
/// strings into a single field. Typed variants
/// make completeness rules explicit at the type
/// level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ExternalRepoIdentity {
    HttpArchive {
        urls: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        sha256: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        strip_prefix: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        patches: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        build_file: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        canonical_id: Option<String>,
    },
    GitRepository {
        remote: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        commit: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tag: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        shallow_since: Option<String>,
    },
    LocalRepository {
        path: String,
    },
    NewLocalRepository {
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        build_file: Option<String>,
    },
    Unknown {
        #[serde(skip_serializing_if = "Option::is_none")]
        rule_kind: Option<String>,
    },
}

/// Phase 5.C.4 commit 176 (Batch W): typed
/// completeness verdict for an
/// `ExternalRepoIdentity`. The reader (commit 179)
/// uses this to decide whether the manifest can be
/// `Valid` for materialized-skip purposes.
///
/// Per Pro's commit-176 rules:
///   - HttpArchive: CompleteImmutable iff sha256
///     is present. Otherwise Incomplete.
///   - GitRepository: CompleteImmutable iff commit
///     is pinned. tag / branch / shallow_since
///     alone are NOT enough — they're explicitly
///     Incomplete because "stable tag" is a
///     promise the universe doesn't keep.
///   - LocalRepository / NewLocalRepository:
///     CompleteLocal. Not immutable (the directory
///     can change), but the identity is the path
///     itself — there's nothing more to verify.
///   - Unknown: Unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityCompleteness {
    /// Fully-pinned immutable identity (sha256 for
    /// http_archive, commit for git_repository).
    /// Safest skip-eligibility.
    CompleteImmutable,
    /// Identity is a local-path reference. Not
    /// immutable, but operator-controlled and the
    /// rush-owned scope cannot detect changes
    /// without re-fetch anyway.
    CompleteLocal,
    /// Identity is present but not fully pinned
    /// (e.g. http_archive without sha256, git
    /// repository with branch/tag only).
    Incomplete,
    /// Identity is missing entirely or rule_kind
    /// is unknown to rush.
    Unknown,
}

impl IdentityCompleteness {
    /// Operator-visible string for diagnostics.
    pub fn as_str(&self) -> &'static str {
        match self {
            IdentityCompleteness::CompleteImmutable => {
                "complete_immutable"
            }
            IdentityCompleteness::CompleteLocal => "complete_local",
            IdentityCompleteness::Incomplete => "incomplete",
            IdentityCompleteness::Unknown => "unknown",
        }
    }
    /// True iff the completeness rises to a level
    /// that supports default-on materialized skip.
    /// Today: CompleteImmutable. Per Pro's
    /// commit-145 + commit-180 policy, even
    /// CompleteImmutable still requires explicit
    /// opt-in via `RUSH_FETCH_SKIP_IF_MATERIALIZED=1`.
    /// Default-on remains blocked.
    pub fn is_immutable(&self) -> bool {
        matches!(self, IdentityCompleteness::CompleteImmutable)
    }
}

/// Phase 5.C.4 commit 177 (Batch W): look up a
/// repo's declared identity in the
/// already-discovered WORKSPACE metadata.
/// `discovered` is whatever the existing
/// workspace_loader.rs populated; this function is
/// a mechanical mapping, NOT a fresh parser pass.
///
/// Per Pro's commit-177 spec:
///   - "Use the existing WORKSPACE / external
///     repo parsing path if one exists."
///   - "Do not write a broad new Starlark parser."
///   - "Do not scan all WORKSPACE repos blindly."
///   - "Do not evaluate unrelated repo rules just
///     to fill metadata."
///
/// If the caller already has a
/// `DiscoveredRepositories` (the rush-build
/// `WorkspaceLoader`'s output) it can pass it
/// here. If the repo isn't in any of the three
/// known buckets, we return
/// `Unknown { rule_kind: None }` — we don't try
/// to guess.
///
/// This function is intentionally NOT given the
/// raw bazel_output_base or workspace path. Its
/// only inputs are (already-parsed metadata,
/// repo_name). That keeps it cheap and side-
/// effect free.
pub fn identity_from_discovered(
    discovered: &crate::workspace_loader::DiscoveredRepositories,
    repo_name: &str,
) -> ExternalRepoIdentity {
    // http_archive lookup
    if let Some(a) = discovered
        .http_archives
        .iter()
        .find(|h| h.name == repo_name)
    {
        return ExternalRepoIdentity::HttpArchive {
            urls: a.urls.clone(),
            sha256: a.sha256.clone(),
            strip_prefix: a.strip_prefix.clone(),
            // rush-parse's HttpArchive doesn't model
            // patches as a separate vec today; keep
            // Vec::new() until parser support
            // exists. Future commits can extend.
            patches: Vec::new(),
            build_file: a.build_file.clone(),
            canonical_id: None,
        };
    }
    // git_repository lookup
    if let Some(g) = discovered
        .git_repositories
        .iter()
        .find(|r| r.name == repo_name)
    {
        return ExternalRepoIdentity::GitRepository {
            remote: g.remote.clone(),
            commit: g.commit.clone(),
            tag: g.tag.clone(),
            branch: g.branch.clone(),
            shallow_since: None,
        };
    }
    // local_repository lookup
    if let Some(l) = discovered
        .local_repositories
        .iter()
        .find(|r| r.name == repo_name)
    {
        return ExternalRepoIdentity::LocalRepository {
            path: l.path.clone(),
        };
    }
    // Not found in any bucket -> Unknown without
    // a rule_kind. The reader treats this as
    // Unknown completeness (different from
    // Incomplete-but-known-shape).
    ExternalRepoIdentity::Unknown { rule_kind: None }
}

/// Phase 5.C.4 commit 178 (Batch W): compute a
/// manifest identity fingerprint from
/// (repo_name, identity). Per Pro's spec:
///
/// > This is NOT AC/CAS. This is NOT action cache
/// > hashing. This is a manifest identity
/// > fingerprint over declared repo metadata.
///
/// What goes in:
///   - schema version (`MANIFEST_FINGERPRINT_SCHEMA_VERSION`)
///   - repo name
///   - rule kind string
///   - identity-relevant fields (variant-specific)
///
/// What does NOT go in:
///   - repo contents
///   - action inputs
///   - any filesystem state
///
/// The output is a 32-char hex digest suitable for
/// the `fetch_rule_fingerprint` field of
/// `ExternalRepoManifest`. Length is enough for
/// collision resistance at manifest scale (one
/// per repo per workspace) but not so long it
/// crowds operator log lines.
pub const MANIFEST_FINGERPRINT_SCHEMA_VERSION: u32 = 1;

pub fn fingerprint_from_identity(
    repo_name: &str,
    identity: &ExternalRepoIdentity,
) -> String {
    let mut hasher = blake3::Hasher::new();
    // Schema version first so future bumps are
    // explicit and don't collide with v1 hashes.
    hasher.update(b"manifest_identity_v");
    hasher.update(
        MANIFEST_FINGERPRINT_SCHEMA_VERSION.to_string().as_bytes(),
    );
    hasher.update(b"\x1f");
    hasher.update(repo_name.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(identity.rule_kind().as_bytes());
    hasher.update(b"\x1f");

    match identity {
        ExternalRepoIdentity::HttpArchive {
            urls,
            sha256,
            strip_prefix,
            patches,
            build_file,
            canonical_id,
        } => {
            // urls: order matters (mirrors). Include
            // every entry.
            hasher.update(b"urls=");
            for u in urls {
                hasher.update(u.as_bytes());
                hasher.update(b",");
            }
            hasher.update(b"\x1f");
            hasher.update(b"sha256=");
            hasher.update(sha256.as_deref().unwrap_or("").as_bytes());
            hasher.update(b"\x1f");
            hasher.update(b"strip_prefix=");
            hasher
                .update(strip_prefix.as_deref().unwrap_or("").as_bytes());
            hasher.update(b"\x1f");
            hasher.update(b"patches=");
            for p in patches {
                hasher.update(p.as_bytes());
                hasher.update(b",");
            }
            hasher.update(b"\x1f");
            hasher.update(b"build_file=");
            hasher.update(build_file.as_deref().unwrap_or("").as_bytes());
            hasher.update(b"\x1f");
            hasher.update(b"canonical_id=");
            hasher.update(
                canonical_id.as_deref().unwrap_or("").as_bytes(),
            );
        }
        ExternalRepoIdentity::GitRepository {
            remote,
            commit,
            tag,
            branch,
            shallow_since,
        } => {
            hasher.update(b"remote=");
            hasher.update(remote.as_bytes());
            hasher.update(b"\x1f");
            hasher.update(b"commit=");
            hasher.update(commit.as_deref().unwrap_or("").as_bytes());
            hasher.update(b"\x1f");
            hasher.update(b"tag=");
            hasher.update(tag.as_deref().unwrap_or("").as_bytes());
            hasher.update(b"\x1f");
            hasher.update(b"branch=");
            hasher.update(branch.as_deref().unwrap_or("").as_bytes());
            hasher.update(b"\x1f");
            hasher.update(b"shallow_since=");
            hasher
                .update(shallow_since.as_deref().unwrap_or("").as_bytes());
        }
        ExternalRepoIdentity::LocalRepository { path } => {
            hasher.update(b"path=");
            hasher.update(path.as_bytes());
        }
        ExternalRepoIdentity::NewLocalRepository { path, build_file } => {
            hasher.update(b"path=");
            hasher.update(path.as_bytes());
            hasher.update(b"\x1f");
            hasher.update(b"build_file=");
            hasher.update(build_file.as_deref().unwrap_or("").as_bytes());
        }
        ExternalRepoIdentity::Unknown { rule_kind } => {
            hasher.update(b"rule_kind=");
            hasher.update(rule_kind.as_deref().unwrap_or("").as_bytes());
        }
    }

    let digest = hasher.finalize();
    // 32 hex chars = 128 bits. More than enough for
    // manifest-scale collision resistance; short
    // enough to fit cleanly in a log line.
    let bytes = digest.as_bytes();
    let mut out = String::with_capacity(32);
    for b in &bytes[..16] {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

impl ExternalRepoIdentity {
    /// Phase 5.C.4 commit 176 (Batch W): per-variant
    /// completeness verdict per Pro's spec.
    pub fn completeness(&self) -> IdentityCompleteness {
        match self {
            ExternalRepoIdentity::HttpArchive { sha256, .. } => {
                if sha256
                    .as_ref()
                    .map(|s| !s.is_empty())
                    .unwrap_or(false)
                {
                    IdentityCompleteness::CompleteImmutable
                } else {
                    IdentityCompleteness::Incomplete
                }
            }
            ExternalRepoIdentity::GitRepository { commit, .. } => {
                // tag / branch / shallow_since alone
                // are explicitly Incomplete per Pro.
                // Only a pinned commit qualifies.
                if commit
                    .as_ref()
                    .map(|s| !s.is_empty())
                    .unwrap_or(false)
                {
                    IdentityCompleteness::CompleteImmutable
                } else {
                    IdentityCompleteness::Incomplete
                }
            }
            ExternalRepoIdentity::LocalRepository { .. } => {
                IdentityCompleteness::CompleteLocal
            }
            ExternalRepoIdentity::NewLocalRepository { .. } => {
                IdentityCompleteness::CompleteLocal
            }
            ExternalRepoIdentity::Unknown { rule_kind } => {
                if rule_kind.is_some() {
                    // We know the rule type but no
                    // identity fields. That's
                    // Incomplete (we recognize the
                    // shape but lack pin data).
                    IdentityCompleteness::Incomplete
                } else {
                    IdentityCompleteness::Unknown
                }
            }
        }
    }

    /// Operator-visible rule-kind string (matches
    /// the .bzl rule name).
    pub fn rule_kind(&self) -> &'static str {
        match self {
            ExternalRepoIdentity::HttpArchive { .. } => "http_archive",
            ExternalRepoIdentity::GitRepository { .. } => "git_repository",
            ExternalRepoIdentity::LocalRepository { .. } => "local_repository",
            ExternalRepoIdentity::NewLocalRepository { .. } => {
                "new_local_repository"
            }
            ExternalRepoIdentity::Unknown { .. } => "unknown",
        }
    }
}

/// Phase 5.C.4 commit 147 (Batch S): one sidecar
/// manifest entry for a repository that rush has
/// fetched (or, for `success_marker = false`,
/// attempted to fetch).
///
/// Field invariants:
///   - `schema_version` MUST equal
///     `EXTERNAL_REPO_MANIFEST_SCHEMA_VERSION` for
///     the reader to accept it.
///   - `repo_name` matches the bazel repo name
///     (without `@`).
///   - `external_path` is the bazel-output-base
///     path where the repo was materialized (e.g.
///     `<cache_root>/bazel_output_base/external/<repo>`).
///   - `fetch_rule_kind` is best-effort. If rush
///     does not know what rule produced the repo,
///     this is `"unknown"`.
///   - `fetch_rule_fingerprint` is the conservative
///     identity Pro asked for: a hash or canonical
///     string built from whatever fetch-rule
///     metadata IS available. If nothing is known,
///     this is `"unknown"`.
///   - `commit_or_sha256` is Some only when the
///     fetch rule provides a verifiable identity
///     (sha256= for http_archive, commit= for
///     git_repository).
///   - `materialized_at_unix` is the writer's
///     system clock at success time.
///   - `success_marker = true` is the verified-
///     success indicator. A reader treats
///     `false` as Invalid for skip purposes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalRepoManifest {
    pub schema_version: u32,
    pub repo_name: String,
    pub external_path: PathBuf,
    pub fetch_rule_kind: String,
    pub fetch_rule_fingerprint: String,
    pub workspace_root: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url_or_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_or_sha256: Option<String>,
    pub materialized_at_unix: u64,
    pub bazel_command_shape: String,
    pub rush_version_or_manifest_writer_version: String,
    pub success_marker: bool,
    /// Phase 5.C.4 commit 179 (Batch W): typed
    /// declared identity (commit 176 enum). When
    /// present, the reader uses it for the Valid
    /// criteria. When absent (older manifest or
    /// rush-parse couldn't extract metadata), the
    /// reader treats this repo as Incomplete.
    /// #[serde(default)] keeps the field
    /// backward-compatible with v1 sidecars on
    /// disk — no schema bump required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<ExternalRepoIdentity>,
}

impl ExternalRepoManifest {
    /// Construct a conservative manifest with
    /// best-effort metadata. `Unknown` fields are
    /// explicit strings so commit-149's reader can
    /// tell "this manifest does NOT give us
    /// fingerprint identity" apart from "fingerprint
    /// is empty for some other reason".
    pub fn new_conservative(
        repo_name: &str,
        external_path: PathBuf,
        workspace_root: PathBuf,
        bazel_command_shape: &str,
    ) -> Self {
        Self {
            schema_version: EXTERNAL_REPO_MANIFEST_SCHEMA_VERSION,
            repo_name: repo_name.to_string(),
            external_path,
            fetch_rule_kind: "unknown".to_string(),
            fetch_rule_fingerprint: "unknown".to_string(),
            workspace_root,
            source_url_or_path: None,
            commit_or_sha256: None,
            materialized_at_unix: now_unix(),
            bazel_command_shape: bazel_command_shape.to_string(),
            rush_version_or_manifest_writer_version:
                env!("CARGO_PKG_VERSION").to_string(),
            success_marker: true,
            // Phase 5.C.4 commit 179 (Batch W):
            // conservative writers don't have an
            // identity yet — commit 180 wires the
            // metadata-extraction path. Until then,
            // the reader sees None and classifies
            // as Incomplete.
            identity: None,
        }
    }

    /// Phase 5.C.4 commit 175 (Batch W): structural
    /// equivalence check that IGNORES the
    /// `materialized_at_unix` timestamp. Two
    /// manifests are equivalent iff every other
    /// field matches. Used by the writer to decide
    /// whether the on-disk manifest is still
    /// current (skip rewrite) or different (rewrite
    /// atomically).
    ///
    /// Why ignore the timestamp: every run produces
    /// a fresh `materialized_at_unix`, so byte-
    /// comparing the full JSON would always force a
    /// rewrite. The timestamp is for diagnostics
    /// only — the reader doesn't gate trust on it.
    pub fn content_equivalent_to(&self, other: &Self) -> bool {
        self.schema_version == other.schema_version
            && self.repo_name == other.repo_name
            && self.external_path == other.external_path
            && self.fetch_rule_kind == other.fetch_rule_kind
            && self.fetch_rule_fingerprint == other.fetch_rule_fingerprint
            && self.workspace_root == other.workspace_root
            && self.source_url_or_path == other.source_url_or_path
            && self.commit_or_sha256 == other.commit_or_sha256
            && self.bazel_command_shape == other.bazel_command_shape
            && self.rush_version_or_manifest_writer_version
                == other.rush_version_or_manifest_writer_version
            && self.success_marker == other.success_marker
            // Phase 5.C.4 commit 179 (Batch W):
            // identity is identity-relevant — a
            // change in the typed identity must
            // trigger a rewrite. Stays out of the
            // diagnostic-only bucket alongside
            // materialized_at_unix.
            && self.identity == other.identity
    }

    /// Path-validation predicate. Called by the
    /// commit-149 reader to decide whether a
    /// freshly-deserialized manifest can be trusted.
    /// Returns Err(reason) on any structural
    /// problem.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != EXTERNAL_REPO_MANIFEST_SCHEMA_VERSION
        {
            return Err(format!(
                "schema_version {} != expected {}",
                self.schema_version,
                EXTERNAL_REPO_MANIFEST_SCHEMA_VERSION
            ));
        }
        if self.repo_name.is_empty() {
            return Err("repo_name is empty".into());
        }
        if !self.success_marker {
            return Err(
                "success_marker is false — not a verified materialization"
                    .into(),
            );
        }
        if self.external_path.as_os_str().is_empty() {
            return Err("external_path is empty".into());
        }
        Ok(())
    }
}

/// Phase 5.C.4 commit 147 (Batch S): typed outcome
/// of looking up a manifest for a given repo. Used
/// by commit-149's reader to drive the
/// SkippedMaterializedManifestValid /
/// SkippedMaterializedTrustMode distinction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalRepoManifestStatus {
    /// Sidecar file present, schema version matches,
    /// success_marker is true, repo_name and
    /// external_path are non-empty, identity
    /// completeness is acceptable for the repo
    /// kind. Skip is manifest-validated.
    Valid,
    /// No sidecar file at the expected path.
    Missing,
    /// Sidecar file exists but failed to parse OR
    /// failed `validate()`. Reader treats as
    /// "do not skip" — fetch instead.
    Invalid(String),
    /// Phase 5.C.4 commit 179 (Batch W): sidecar
    /// parses + validates, but a context-bound check
    /// fails (workspace_root mismatch, external_path
    /// mismatch, etc.). Reader treats identically
    /// to Invalid for trust purposes.
    Stale(String),
    /// Sidecar exists and validates structurally,
    /// but the typed identity is missing or
    /// declared completeness is below the
    /// "Valid" bar (e.g. http_archive without
    /// sha256, git_repository with branch only).
    /// Skip is allowed in trust mode only.
    Incomplete(String),
    /// Sidecar exists but cannot be loaded due to
    /// I/O error (permissions, etc.).
    Unknown(String),
}

impl ExternalRepoManifestStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExternalRepoManifestStatus::Valid => "valid",
            ExternalRepoManifestStatus::Missing => "missing",
            ExternalRepoManifestStatus::Invalid(_) => "invalid",
            ExternalRepoManifestStatus::Stale(_) => "stale",
            ExternalRepoManifestStatus::Incomplete(_) => "incomplete",
            ExternalRepoManifestStatus::Unknown(_) => "unknown",
        }
    }

    pub fn is_valid_for_manifest_skip(&self) -> bool {
        matches!(self, ExternalRepoManifestStatus::Valid)
    }
}

/// Phase 5.C.4 commit 147 (Batch S): canonical
/// sidecar directory under the rush cache root.
///
/// **Rush-owned location.** Does NOT mutate
/// third-party `external/<repo>/` directories. Pro's
/// commit-S green-light letter is explicit about
/// this — manifests live in rush's territory only.
///
/// Path:
///   `<cache_root>/external-manifests/`
///
/// where `<cache_root>` comes from
/// `cache_paths::workspace_cache_root` (respects
/// `RUSH_WORKSPACE_CACHE_ROOT`).
pub fn external_manifests_dir(workspace_root: &Path) -> PathBuf {
    crate::cache_paths::workspace_cache_root(workspace_root)
        .join("external-manifests")
}

/// Phase 5.C.4 commit 147 (Batch S): per-repo
/// manifest file path. One JSON file per repo.
pub fn manifest_path(workspace_root: &Path, repo_name: &str) -> PathBuf {
    external_manifests_dir(workspace_root)
        .join(format!("{}.json", repo_name))
}

/// Phase 5.C.4 commit 147 (Batch S): unix epoch
/// seconds at call time. Used by the writer
/// (commit 148) to stamp `materialized_at_unix`.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// =============================================================
// Phase 5.C.4 commit 149 (Batch S): manifest reader.
// =============================================================

/// Phase 5.C.4 commit 149 (Batch S): load the
/// sidecar manifest for `repo_name` from
/// `<cache_root>/external-manifests/<repo>.json`
/// and classify its status.
///
/// Pure: no env var reads, no side effects. Returns
/// `Missing` when the sidecar file isn't present,
/// `Invalid(reason)` when parsing or structural
/// validation fails, `Incomplete` when the manifest
/// validates but has "unknown" fingerprints / no
/// URL / no commit-sha (commit 148's
/// `new_conservative` writer produces this shape
/// today), and `Valid` only when every identity
/// field is meaningfully populated.
pub fn read_manifest_status(
    workspace_root: &Path,
    repo_name: &str,
) -> ExternalRepoManifestStatus {
    let path = manifest_path(workspace_root, repo_name);
    if !path.exists() {
        return ExternalRepoManifestStatus::Missing;
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            return ExternalRepoManifestStatus::Unknown(format!(
                "read({}): {}",
                path.display(),
                e
            ));
        }
    };
    let manifest: ExternalRepoManifest = match serde_json::from_str(&raw)
    {
        Ok(m) => m,
        Err(e) => {
            return ExternalRepoManifestStatus::Invalid(format!(
                "parse: {}",
                e
            ));
        }
    };
    if let Err(reason) = manifest.validate() {
        return ExternalRepoManifestStatus::Invalid(reason);
    }
    // Phase 5.C.4 commit 179 (Batch W): context-bound
    // validity checks. workspace_root + repo_name
    // must match what the caller asked about.
    // Mismatches produce Stale (parses fine, but
    // not relevant to this run).
    if manifest.workspace_root.as_path() != workspace_root {
        return ExternalRepoManifestStatus::Stale(format!(
            "workspace_root mismatch: manifest={} expected={}",
            manifest.workspace_root.display(),
            workspace_root.display()
        ));
    }
    if manifest.repo_name != repo_name {
        return ExternalRepoManifestStatus::Stale(format!(
            "repo_name mismatch: manifest={} expected={}",
            manifest.repo_name, repo_name
        ));
    }
    // success_marker must be true (failed-fetch
    // manifests are an explicit Incomplete).
    if !manifest.success_marker {
        return ExternalRepoManifestStatus::Incomplete(
            "success_marker is false".to_string(),
        );
    }
    // Identity-completeness checks. Per Pro's
    // commit-179 spec, the reader uses the typed
    // identity (commit 176) to decide whether the
    // manifest meets the Valid bar.
    match &manifest.identity {
        Some(identity) => match identity.completeness() {
            IdentityCompleteness::CompleteImmutable
            | IdentityCompleteness::CompleteLocal => {
                // Both qualify for Valid. Local
                // repos are operator-controlled so
                // the immutability bar doesn't apply.
            }
            IdentityCompleteness::Incomplete => {
                return ExternalRepoManifestStatus::Incomplete(
                    format!(
                        "{} identity is incomplete (missing pin)",
                        identity.rule_kind()
                    ),
                );
            }
            IdentityCompleteness::Unknown => {
                return ExternalRepoManifestStatus::Incomplete(
                    "identity rule_kind is unknown".to_string(),
                );
            }
        },
        None => {
            // No typed identity at all — pre-commit-179
            // conservative manifests or extraction
            // failed. Same backward-compat path as
            // before: surface as Incomplete with a
            // reason.
            let legacy_unknown = manifest.fetch_rule_fingerprint
                == "unknown"
                || manifest.fetch_rule_kind == "unknown"
                || (manifest.source_url_or_path.is_none()
                    && manifest.commit_or_sha256.is_none());
            if legacy_unknown {
                return ExternalRepoManifestStatus::Incomplete(
                    "no typed identity (legacy conservative manifest)"
                        .to_string(),
                );
            }
            // Old manifest with some metadata in the
            // legacy string fields but no typed
            // identity. Treat as Incomplete — the
            // reader doesn't trust string-only
            // metadata for the Valid path.
            return ExternalRepoManifestStatus::Incomplete(
                "no typed identity available".to_string(),
            );
        }
    }
    // Phase 5.C.4 commit 178 (Batch W): cross-check
    // that the stored fetch_rule_fingerprint
    // matches what the identity would produce
    // today. Drift indicates either a manifest
    // hand-edit or a fingerprint-formula change;
    // either way, treat as Stale.
    if let Some(identity) = &manifest.identity {
        let recomputed =
            fingerprint_from_identity(&manifest.repo_name, identity);
        if recomputed != manifest.fetch_rule_fingerprint
            && manifest.fetch_rule_fingerprint != "unknown"
        {
            return ExternalRepoManifestStatus::Stale(format!(
                "fingerprint mismatch: stored={} recomputed={}",
                manifest.fetch_rule_fingerprint, recomputed
            ));
        }
    }
    ExternalRepoManifestStatus::Valid
}

// =============================================================
// Phase 5.C.4 commit 148 (Batch S): manifest writer.
// =============================================================

/// Phase 5.C.4 commit 148 (Batch S): outcome of a
/// manifest-write attempt. The writer never panics
/// and returns a typed result so callers can fold
/// the outcome into operator counters cleanly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestWriteOutcome {
    /// Sidecar file was created (or atomically
    /// replaced).
    Written(PathBuf),
    /// Phase 5.C.4 commit 175 (Batch W): the
    /// existing on-disk manifest is structurally
    /// identical to what we'd write (ignoring
    /// materialized_at_unix). The write is skipped
    /// to avoid pointless rewrites on every
    /// analyze/build run.
    SkippedUnchanged(PathBuf),
    /// Repo dir is missing or has no BUILD /
    /// BUILD.bazel — manifest not written
    /// (writer policy: only stamp materialization
    /// success).
    SkippedNoBuild,
    /// Repo dir does not exist on disk — manifest
    /// not written.
    SkippedNoDir,
    /// Write failed (I/O error, permission, etc.).
    /// The serialized manifest contents and the
    /// underlying error are folded into the
    /// reason string for diagnostics.
    Failed(String),
}

impl ManifestWriteOutcome {
    /// True iff a sidecar was actually written.
    /// Counter wiring uses this to decide between
    /// `manifests_written` and `manifest_write_failures`.
    pub fn is_written(&self) -> bool {
        matches!(self, ManifestWriteOutcome::Written(_))
    }

    /// True iff the writer failed (vs deliberately
    /// skipped). Distinguishes "I/O error wrote
    /// nothing" from "no BUILD file so I didn't try".
    pub fn is_failure(&self) -> bool {
        matches!(self, ManifestWriteOutcome::Failed(_))
    }

    /// Phase 5.C.4 commit 175 (Batch W): true if
    /// the writer found an identical on-disk
    /// manifest and skipped the rewrite. Counters
    /// use this to distinguish "we wrote a new
    /// manifest" from "the existing one is still
    /// current."
    pub fn is_skipped_unchanged(&self) -> bool {
        matches!(self, ManifestWriteOutcome::SkippedUnchanged(_))
    }
}

/// Phase 5.C.4 commit 148 (Batch S): atomically
/// write a conservative manifest for a successfully
/// materialized repo. Atomic = tempfile + rename.
///
/// Per Pro's commit-148 spec:
///   - Do NOT write if fetch failed (caller must
///     not invoke this on failed fetches).
///   - Do NOT write if repo dir lacks BUILD /
///     BUILD.bazel.
///   - Do NOT write inside the external repo dir
///     (sidecar location is under
///     `<cache_root>/external-manifests/`).
///
/// Counter wiring is the CALLER's responsibility;
/// this function returns a typed outcome so the
/// rush-starlark fetch path can record manifests_written
/// / manifest_write_failures into its CacheMetrics.
pub fn write_manifest_for_repo(
    workspace_root: &Path,
    repo_name: &str,
    external_path: &Path,
    bazel_command_shape: &str,
) -> ManifestWriteOutcome {
    // Guard 1: repo dir must exist.
    if !external_path.exists() {
        return ManifestWriteOutcome::SkippedNoDir;
    }
    // Guard 2: BUILD or BUILD.bazel must be present.
    // Same predicate as
    // `RUSH_FETCH_SKIP_IF_MATERIALIZED`'s
    // check — manifest writer policy is "I only
    // certify what the skip path will trust".
    let has_build = external_path.join("BUILD").exists()
        || external_path.join("BUILD.bazel").exists();
    if !has_build {
        return ManifestWriteOutcome::SkippedNoBuild;
    }

    let manifest = ExternalRepoManifest::new_conservative(
        repo_name,
        external_path.to_path_buf(),
        workspace_root.to_path_buf(),
        bazel_command_shape,
    );

    let dir = external_manifests_dir(workspace_root);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return ManifestWriteOutcome::Failed(format!(
            "create_dir_all({}): {}",
            dir.display(),
            e
        ));
    }
    let final_path = manifest_path(workspace_root, repo_name);

    // Phase 5.C.4 commit 175 (Batch W): write-if-
    // changed. If the on-disk manifest is
    // structurally identical (ignoring the
    // timestamp), skip the rewrite to avoid
    // pointless I/O churn on every analyze/build
    // run. Read failures, parse failures, and
    // validate failures all fall through to the
    // normal write path — the writer is
    // best-effort.
    if final_path.exists() {
        if let Ok(existing_raw) = std::fs::read_to_string(&final_path) {
            if let Ok(existing) =
                serde_json::from_str::<ExternalRepoManifest>(&existing_raw)
            {
                if existing.content_equivalent_to(&manifest) {
                    return ManifestWriteOutcome::SkippedUnchanged(
                        final_path,
                    );
                }
            }
        }
    }

    let tmp_path = final_path.with_extension("json.tmp");

    let json = match serde_json::to_string_pretty(&manifest) {
        Ok(s) => s,
        Err(e) => {
            return ManifestWriteOutcome::Failed(format!(
                "serialize: {}",
                e
            ));
        }
    };

    if let Err(e) = std::fs::write(&tmp_path, &json) {
        return ManifestWriteOutcome::Failed(format!(
            "write({}): {}",
            tmp_path.display(),
            e
        ));
    }
    if let Err(e) = std::fs::rename(&tmp_path, &final_path) {
        // Best-effort cleanup of the tmpfile on
        // rename failure.
        let _ = std::fs::remove_file(&tmp_path);
        return ManifestWriteOutcome::Failed(format!(
            "rename({} → {}): {}",
            tmp_path.display(),
            final_path.display(),
            e
        ));
    }

    ManifestWriteOutcome::Written(final_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // =====================================================
    // Phase 5.C.4 commit 176 (Batch W): ExternalRepoIdentity
    // completeness tests. Per Pro's spec each variant has
    // explicit rules about what makes it
    // CompleteImmutable / CompleteLocal / Incomplete /
    // Unknown.
    // =====================================================

    #[test]
    fn http_archive_with_sha256_is_complete_immutable() {
        let id = ExternalRepoIdentity::HttpArchive {
            urls: vec!["https://example.com/foo.tar.gz".to_string()],
            sha256: Some("abc123".to_string()),
            strip_prefix: None,
            patches: vec![],
            build_file: None,
            canonical_id: None,
        };
        assert_eq!(
            id.completeness(),
            IdentityCompleteness::CompleteImmutable
        );
        assert!(id.completeness().is_immutable());
        assert_eq!(id.rule_kind(), "http_archive");
    }

    #[test]
    fn http_archive_without_sha256_is_incomplete() {
        let id = ExternalRepoIdentity::HttpArchive {
            urls: vec!["https://example.com/foo.tar.gz".to_string()],
            sha256: None,
            strip_prefix: None,
            patches: vec![],
            build_file: None,
            canonical_id: None,
        };
        assert_eq!(id.completeness(), IdentityCompleteness::Incomplete);
        assert!(!id.completeness().is_immutable());
        // Empty sha256 string also counts as
        // Incomplete (not just None).
        let id_empty = ExternalRepoIdentity::HttpArchive {
            urls: vec!["https://example.com/foo.tar.gz".to_string()],
            sha256: Some("".to_string()),
            strip_prefix: None,
            patches: vec![],
            build_file: None,
            canonical_id: None,
        };
        assert_eq!(
            id_empty.completeness(),
            IdentityCompleteness::Incomplete
        );
    }

    #[test]
    fn git_repository_with_commit_is_complete_immutable() {
        let id = ExternalRepoIdentity::GitRepository {
            remote: "https://github.com/example/repo.git".to_string(),
            commit: Some("deadbeef".to_string()),
            tag: None,
            branch: None,
            shallow_since: None,
        };
        assert_eq!(
            id.completeness(),
            IdentityCompleteness::CompleteImmutable
        );
    }

    #[test]
    fn git_repository_with_tag_only_is_incomplete() {
        // Per Pro's spec: "tag may be acceptable
        // only if you deliberately treat it as
        // stable; safer to call tag Incomplete
        // unless resolved to commit."
        let id = ExternalRepoIdentity::GitRepository {
            remote: "https://github.com/example/repo.git".to_string(),
            commit: None,
            tag: Some("v1.0.0".to_string()),
            branch: None,
            shallow_since: None,
        };
        assert_eq!(id.completeness(), IdentityCompleteness::Incomplete);
    }

    #[test]
    fn git_repository_with_branch_is_incomplete() {
        // Per Pro's spec: "branch is Incomplete."
        let id = ExternalRepoIdentity::GitRepository {
            remote: "https://github.com/example/repo.git".to_string(),
            commit: None,
            tag: None,
            branch: Some("main".to_string()),
            shallow_since: None,
        };
        assert_eq!(id.completeness(), IdentityCompleteness::Incomplete);
    }

    #[test]
    fn local_repository_is_complete_local() {
        // Per Pro: "local_repository is CompleteLocal,
        // not immutable."
        let id = ExternalRepoIdentity::LocalRepository {
            path: "/abs/path/to/repo".to_string(),
        };
        assert_eq!(id.completeness(), IdentityCompleteness::CompleteLocal);
        assert!(!id.completeness().is_immutable());
    }

    #[test]
    fn new_local_repository_is_complete_local() {
        let id = ExternalRepoIdentity::NewLocalRepository {
            path: "/abs/path/to/repo".to_string(),
            build_file: Some("BUILD.repo".to_string()),
        };
        assert_eq!(id.completeness(), IdentityCompleteness::CompleteLocal);
    }

    #[test]
    fn unknown_with_rule_kind_is_incomplete() {
        // We know the shape (some rule we don't yet
        // model) but no identity fields.
        let id = ExternalRepoIdentity::Unknown {
            rule_kind: Some("local_config_cuda".to_string()),
        };
        assert_eq!(id.completeness(), IdentityCompleteness::Incomplete);
        assert_eq!(id.rule_kind(), "unknown");
    }

    #[test]
    fn unknown_without_rule_kind_is_unknown() {
        let id = ExternalRepoIdentity::Unknown { rule_kind: None };
        assert_eq!(id.completeness(), IdentityCompleteness::Unknown);
        assert!(!id.completeness().is_immutable());
    }

    #[test]
    fn identity_roundtrips_via_serde() {
        let cases = vec![
            ExternalRepoIdentity::HttpArchive {
                urls: vec!["https://a.example/x.tgz".to_string()],
                sha256: Some("abc".to_string()),
                strip_prefix: Some("x-1.0".to_string()),
                patches: vec!["patch.diff".to_string()],
                build_file: Some("BUILD.x".to_string()),
                canonical_id: Some("x@1.0".to_string()),
            },
            ExternalRepoIdentity::GitRepository {
                remote: "https://github.com/example/repo.git".to_string(),
                commit: Some("deadbeef".to_string()),
                tag: None,
                branch: None,
                shallow_since: None,
            },
            ExternalRepoIdentity::LocalRepository {
                path: "/abs/p".to_string(),
            },
            ExternalRepoIdentity::NewLocalRepository {
                path: "/abs/p".to_string(),
                build_file: None,
            },
            ExternalRepoIdentity::Unknown {
                rule_kind: Some("custom_rule".to_string()),
            },
        ];
        for id in cases {
            let json = serde_json::to_string(&id).expect("ser");
            let back: ExternalRepoIdentity =
                serde_json::from_str(&json).expect("deser");
            assert_eq!(id, back, "roundtrip mismatch for {:?}", id);
        }
    }

    // =====================================================
    // Phase 5.C.4 commit 177 (Batch W): metadata
    // extraction from existing repo-rule path. Names
    // per Pro's directive. Tests use
    // DiscoveredRepositories constructed in-test;
    // no real WORKSPACE evaluation.
    // =====================================================

    fn discovered_with_repos(
        http: Vec<rush_parse::HttpArchive>,
        git: Vec<rush_parse::GitRepository>,
        local: Vec<rush_parse::LocalRepository>,
    ) -> crate::workspace_loader::DiscoveredRepositories {
        crate::workspace_loader::DiscoveredRepositories {
            workspace_name: None,
            http_archives: http,
            git_repositories: git,
            local_repositories: local,
        }
    }

    #[test]
    fn http_archive_metadata_extracted() {
        let discovered = discovered_with_repos(
            vec![rush_parse::HttpArchive {
                name: "alpha".to_string(),
                urls: vec![
                    "https://example.com/alpha.tar.gz".to_string(),
                    "https://mirror.example.com/alpha.tar.gz".to_string(),
                ],
                sha256: Some("abc123".to_string()),
                strip_prefix: Some("alpha-1.0".to_string()),
                build_file: Some("BUILD.alpha".to_string()),
                build_file_content: None,
                repo_mapping: std::collections::HashMap::new(),
            }],
            vec![],
            vec![],
        );
        let id = identity_from_discovered(&discovered, "alpha");
        match &id {
            ExternalRepoIdentity::HttpArchive {
                urls,
                sha256,
                strip_prefix,
                build_file,
                ..
            } => {
                assert_eq!(urls.len(), 2);
                assert_eq!(sha256.as_deref(), Some("abc123"));
                assert_eq!(strip_prefix.as_deref(), Some("alpha-1.0"));
                assert_eq!(build_file.as_deref(), Some("BUILD.alpha"));
            }
            other => panic!("expected HttpArchive, got {:?}", other),
        }
        // sha256 present -> CompleteImmutable
        assert_eq!(
            id.completeness(),
            IdentityCompleteness::CompleteImmutable
        );
    }

    #[test]
    fn git_repository_metadata_extracted_if_present() {
        let discovered = discovered_with_repos(
            vec![],
            vec![rush_parse::GitRepository {
                name: "beta".to_string(),
                remote: "https://github.com/example/beta.git"
                    .to_string(),
                commit: Some("deadbeef".to_string()),
                tag: None,
                branch: None,
                repo_mapping: std::collections::HashMap::new(),
            }],
            vec![],
        );
        let id = identity_from_discovered(&discovered, "beta");
        match &id {
            ExternalRepoIdentity::GitRepository {
                remote, commit, ..
            } => {
                assert_eq!(
                    remote,
                    "https://github.com/example/beta.git"
                );
                assert_eq!(commit.as_deref(), Some("deadbeef"));
            }
            other => panic!("expected GitRepository, got {:?}", other),
        }
        assert_eq!(
            id.completeness(),
            IdentityCompleteness::CompleteImmutable
        );

        // Branch-only case: Incomplete.
        let discovered2 = discovered_with_repos(
            vec![],
            vec![rush_parse::GitRepository {
                name: "gamma".to_string(),
                remote: "https://github.com/example/gamma.git"
                    .to_string(),
                commit: None,
                tag: None,
                branch: Some("main".to_string()),
                repo_mapping: std::collections::HashMap::new(),
            }],
            vec![],
        );
        let id2 = identity_from_discovered(&discovered2, "gamma");
        assert_eq!(id2.completeness(), IdentityCompleteness::Incomplete);
    }

    #[test]
    fn local_repository_metadata_extracted_if_present() {
        let discovered = discovered_with_repos(
            vec![],
            vec![],
            vec![rush_parse::LocalRepository {
                name: "delta".to_string(),
                path: "/abs/path/to/delta".to_string(),
                repo_mapping: std::collections::HashMap::new(),
            }],
        );
        let id = identity_from_discovered(&discovered, "delta");
        match &id {
            ExternalRepoIdentity::LocalRepository { path } => {
                assert_eq!(path, "/abs/path/to/delta");
            }
            other => panic!("expected LocalRepository, got {:?}", other),
        }
        assert_eq!(
            id.completeness(),
            IdentityCompleteness::CompleteLocal
        );
    }

    #[test]
    fn unknown_rule_stays_incomplete() {
        // Repo not in any DiscoveredRepositories
        // bucket -> Unknown { rule_kind: None }.
        // completeness() returns Unknown (not
        // Incomplete) because we don't even know
        // what kind of rule produced it.
        let discovered = discovered_with_repos(
            vec![rush_parse::HttpArchive {
                name: "alpha".to_string(),
                urls: vec!["https://x.example/a.tgz".to_string()],
                sha256: Some("abc".to_string()),
                strip_prefix: None,
                build_file: None,
                build_file_content: None,
                repo_mapping: std::collections::HashMap::new(),
            }],
            vec![],
            vec![],
        );
        let id = identity_from_discovered(
            &discovered,
            "not_in_workspace",
        );
        match &id {
            ExternalRepoIdentity::Unknown { rule_kind } => {
                assert!(rule_kind.is_none());
            }
            other => panic!("expected Unknown, got {:?}", other),
        }
        // Pro's commit-179 reader will see Unknown
        // completeness and emit a distinct status
        // from Incomplete. The skip path treats
        // both as not-Valid.
        assert_eq!(id.completeness(), IdentityCompleteness::Unknown);
    }

    // =====================================================
    // Phase 5.C.4 commit 178 (Batch W): manifest
    // identity fingerprint tests. Names per Pro's
    // directive. Pure-function tests over the
    // identity model; no filesystem.
    // =====================================================

    #[test]
    fn same_http_archive_identity_same_fingerprint() {
        let id1 = ExternalRepoIdentity::HttpArchive {
            urls: vec!["https://a.example/x.tgz".to_string()],
            sha256: Some("abc123".to_string()),
            strip_prefix: Some("x-1.0".to_string()),
            patches: vec![],
            build_file: None,
            canonical_id: None,
        };
        let id2 = id1.clone();
        let fp1 = fingerprint_from_identity("alpha", &id1);
        let fp2 = fingerprint_from_identity("alpha", &id2);
        assert_eq!(
            fp1, fp2,
            "identical identity must produce identical fingerprint"
        );
        // Hex shape sanity.
        assert_eq!(fp1.len(), 32);
        assert!(fp1.chars().all(|c| c.is_ascii_hexdigit()));

        // Different repo_name -> different fingerprint
        // (the name is in the canonical preimage).
        let fp_other = fingerprint_from_identity("beta", &id1);
        assert_ne!(fp1, fp_other);
    }

    #[test]
    fn sha_change_changes_fingerprint() {
        let id_a = ExternalRepoIdentity::HttpArchive {
            urls: vec!["https://a.example/x.tgz".to_string()],
            sha256: Some("abc123".to_string()),
            strip_prefix: None,
            patches: vec![],
            build_file: None,
            canonical_id: None,
        };
        let id_b = ExternalRepoIdentity::HttpArchive {
            urls: vec!["https://a.example/x.tgz".to_string()],
            sha256: Some("def456".to_string()),
            strip_prefix: None,
            patches: vec![],
            build_file: None,
            canonical_id: None,
        };
        let fp_a = fingerprint_from_identity("alpha", &id_a);
        let fp_b = fingerprint_from_identity("alpha", &id_b);
        assert_ne!(
            fp_a, fp_b,
            "sha256 change must change fingerprint"
        );
    }

    #[test]
    fn url_change_changes_fingerprint() {
        let id_a = ExternalRepoIdentity::HttpArchive {
            urls: vec!["https://a.example/x.tgz".to_string()],
            sha256: Some("abc".to_string()),
            strip_prefix: None,
            patches: vec![],
            build_file: None,
            canonical_id: None,
        };
        let id_b = ExternalRepoIdentity::HttpArchive {
            urls: vec!["https://b.example/x.tgz".to_string()],
            sha256: Some("abc".to_string()),
            strip_prefix: None,
            patches: vec![],
            build_file: None,
            canonical_id: None,
        };
        let fp_a = fingerprint_from_identity("alpha", &id_a);
        let fp_b = fingerprint_from_identity("alpha", &id_b);
        assert_ne!(fp_a, fp_b, "url change must change fingerprint");

        // Adding a mirror URL also changes the
        // fingerprint (order + count matter).
        let id_c = ExternalRepoIdentity::HttpArchive {
            urls: vec![
                "https://a.example/x.tgz".to_string(),
                "https://mirror.example/x.tgz".to_string(),
            ],
            sha256: Some("abc".to_string()),
            strip_prefix: None,
            patches: vec![],
            build_file: None,
            canonical_id: None,
        };
        let fp_c = fingerprint_from_identity("alpha", &id_c);
        assert_ne!(fp_a, fp_c);
    }

    #[test]
    fn branch_git_repo_marked_incomplete() {
        // Pro's spec phrasing is "branch_git_repo_marked_incomplete"
        // — this test is really about the
        // completeness verdict (commit 176's
        // territory) but is named under commit 178
        // because the fingerprint computation
        // includes branch in the preimage and we
        // want to confirm the branch-only case
        // still produces a stable fingerprint
        // (just at Incomplete completeness, not
        // Valid). Different branches -> different
        // fingerprints.
        let id_main = ExternalRepoIdentity::GitRepository {
            remote: "https://github.com/example/r.git".to_string(),
            commit: None,
            tag: None,
            branch: Some("main".to_string()),
            shallow_since: None,
        };
        let id_dev = ExternalRepoIdentity::GitRepository {
            remote: "https://github.com/example/r.git".to_string(),
            commit: None,
            tag: None,
            branch: Some("dev".to_string()),
            shallow_since: None,
        };
        // Both Incomplete per commit 176.
        assert_eq!(
            id_main.completeness(),
            IdentityCompleteness::Incomplete
        );
        assert_eq!(
            id_dev.completeness(),
            IdentityCompleteness::Incomplete
        );
        // Different branches produce different
        // fingerprints — caller can still
        // distinguish runs even though neither is
        // Valid.
        let fp_main = fingerprint_from_identity("r", &id_main);
        let fp_dev = fingerprint_from_identity("r", &id_dev);
        assert_ne!(fp_main, fp_dev);
    }

    #[test]
    fn local_repo_fingerprint_includes_path() {
        let id_a = ExternalRepoIdentity::LocalRepository {
            path: "/abs/path/to/repo".to_string(),
        };
        let id_b = ExternalRepoIdentity::LocalRepository {
            path: "/different/abs/path".to_string(),
        };
        let fp_a = fingerprint_from_identity("local", &id_a);
        let fp_b = fingerprint_from_identity("local", &id_b);
        assert_ne!(fp_a, fp_b, "local-repo path is identity-relevant");

        // Same path, same name -> stable.
        let fp_a2 = fingerprint_from_identity("local", &id_a);
        assert_eq!(fp_a, fp_a2);
    }

    #[test]
    fn completeness_str_labels() {
        assert_eq!(
            IdentityCompleteness::CompleteImmutable.as_str(),
            "complete_immutable"
        );
        assert_eq!(
            IdentityCompleteness::CompleteLocal.as_str(),
            "complete_local"
        );
        assert_eq!(IdentityCompleteness::Incomplete.as_str(), "incomplete");
        assert_eq!(IdentityCompleteness::Unknown.as_str(), "unknown");
    }

    fn sample_manifest() -> ExternalRepoManifest {
        ExternalRepoManifest {
            schema_version: EXTERNAL_REPO_MANIFEST_SCHEMA_VERSION,
            repo_name: "foo".to_string(),
            external_path: PathBuf::from(
                "/tmp/ws/.cache/rush/bazel_output_base/external/foo",
            ),
            fetch_rule_kind: "http_archive".to_string(),
            fetch_rule_fingerprint: "abc123".to_string(),
            workspace_root: PathBuf::from("/tmp/ws"),
            source_url_or_path: Some(
                "https://example.com/foo.tar.gz".to_string(),
            ),
            commit_or_sha256: Some(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string(),
            ),
            materialized_at_unix: 1737128400,
            bazel_command_shape: "bazel sync --only=foo".to_string(),
            rush_version_or_manifest_writer_version: "0.1.0".to_string(),
            success_marker: true,
            identity: None,
        }
    }

    #[test]
    fn manifest_roundtrip_json() {
        let m = sample_manifest();
        let json = serde_json::to_string(&m).expect("serialize");
        // None fields should be skipped — but Some
        // fields stay.
        assert!(
            json.contains("\"source_url_or_path\""),
            "Some url field should be in JSON: {}",
            json
        );
        let back: ExternalRepoManifest =
            serde_json::from_str(&json).expect("deserialize");
        assert_eq!(m, back, "roundtrip must be byte-identical");
    }

    #[test]
    fn manifest_missing_is_missing() {
        // For a tempdir with NO sidecar file at the
        // expected path, the path computation should
        // produce a non-existent file.
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        let path = manifest_path(&ws, "absent_repo");
        assert!(
            !path.exists(),
            "manifest_path for absent repo must not exist: {}",
            path.display()
        );
        // The sidecar dir itself should not be auto-created
        // by the path helper.
        let dir = external_manifests_dir(&ws);
        assert!(
            !dir.exists(),
            "external_manifests_dir should not auto-create: {}",
            dir.display()
        );
    }

    #[test]
    fn manifest_schema_version_pinned() {
        // Schema version must be 1 in this batch. If
        // a future commit bumps it, this test fails
        // and forces an explicit migration story.
        assert_eq!(EXTERNAL_REPO_MANIFEST_SCHEMA_VERSION, 1);
        let m = sample_manifest();
        let mut wrong_version = m.clone();
        wrong_version.schema_version = 999;
        let err = wrong_version.validate().expect_err(
            "wrong schema_version must reject",
        );
        assert!(
            err.contains("schema_version"),
            "validate error should mention schema_version: {}",
            err
        );
    }

    #[test]
    fn manifest_validates_repo_name_and_path() {
        let m = sample_manifest();
        m.validate().expect("baseline must validate");

        // Empty repo_name → invalid.
        let mut bad = m.clone();
        bad.repo_name = "".to_string();
        let err = bad.validate().expect_err("empty repo_name must reject");
        assert!(err.contains("repo_name"));

        // success_marker false → invalid.
        let mut not_ok = m.clone();
        not_ok.success_marker = false;
        let err = not_ok.validate().expect_err("success_marker=false must reject");
        assert!(err.contains("success_marker"));

        // Empty external_path → invalid.
        let mut empty_path = m.clone();
        empty_path.external_path = PathBuf::new();
        let err = empty_path.validate().expect_err("empty path must reject");
        assert!(err.contains("external_path"));
    }

    #[test]
    fn external_manifests_dir_is_under_cache_root() {
        // Pro's correction: manifests live in rush's
        // own territory, NEVER inside external/<repo>/.
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        let dir = external_manifests_dir(&ws);
        let dir_str = dir.display().to_string();
        assert!(
            dir_str.contains(".cache/rush") || dir_str.contains("/rush/"),
            "manifests dir must be under rush cache: {}",
            dir_str
        );
        assert!(
            !dir_str.contains("/external/foo")
                && !dir_str.contains("/external/bar"),
            "manifests dir must NOT live inside an external repo: {}",
            dir_str
        );

        let path = manifest_path(&ws, "myrepo");
        assert_eq!(
            path,
            dir.join("myrepo.json"),
            "per-repo manifest path is dir/<repo>.json"
        );
    }

    // =====================================================
    // Phase 5.C.4 commit 148 (Batch S): writer tests
    // =====================================================

    /// Helper: build a workspace_root + external repo
    /// dir with optional BUILD file.
    fn setup_repo(
        repo_name: &str,
        has_build: bool,
        repo_exists: bool,
    ) -> (TempDir, PathBuf, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        // The "external" path the caller would
        // claim is materialized — does NOT live in
        // .cache/rush; commit 148's writer doesn't
        // care WHERE the external_path is as long
        // as it exists.
        let ext = ws.join("bazel_output").join("external").join(repo_name);
        if repo_exists {
            fs::create_dir_all(&ext).unwrap();
            if has_build {
                fs::write(ext.join("BUILD"), b"# stub\n").unwrap();
            }
        }
        (tmp, ws, ext)
    }

    #[test]
    fn successful_fetch_writes_manifest() {
        let (_tmp, ws, ext) =
            setup_repo("alpha", true, true);
        let outcome = write_manifest_for_repo(
            &ws,
            "alpha",
            &ext,
            "bazel sync --only=alpha",
        );
        assert!(
            outcome.is_written(),
            "expected Written, got {:?}",
            outcome
        );

        // The manifest must be at the canonical
        // sidecar path AND parse back to an
        // ExternalRepoManifest that validates.
        let path = manifest_path(&ws, "alpha");
        assert!(path.exists(), "sidecar must exist at {}", path.display());
        let json = fs::read_to_string(&path).unwrap();
        let parsed: ExternalRepoManifest =
            serde_json::from_str(&json).expect("valid JSON");
        parsed.validate().expect("written manifest must validate");
        assert_eq!(parsed.repo_name, "alpha");
        assert_eq!(parsed.external_path, ext);
        assert_eq!(parsed.bazel_command_shape, "bazel sync --only=alpha");
        assert!(parsed.success_marker);
    }

    #[test]
    fn failed_fetch_does_not_write_manifest() {
        // "Failed fetch" is represented by the
        // CALLER not invoking write_manifest_for_repo
        // at all — the writer itself has no failure
        // injection for the fetch step. But the
        // analogous structural failure ("repo dir
        // not present") is what the writer sees if
        // a fetch failure left no dir on disk.
        let (_tmp, ws, ext) =
            setup_repo("beta", false, false);
        let outcome = write_manifest_for_repo(
            &ws,
            "beta",
            &ext,
            "bazel sync --only=beta",
        );
        assert_eq!(
            outcome,
            ManifestWriteOutcome::SkippedNoDir,
            "missing repo dir must produce SkippedNoDir, got {:?}",
            outcome
        );
        // Sidecar file must NOT exist.
        let path = manifest_path(&ws, "beta");
        assert!(
            !path.exists(),
            "no manifest must be written: {}",
            path.display()
        );
    }

    #[test]
    fn missing_build_does_not_write_manifest() {
        // Repo dir exists, but no BUILD or
        // BUILD.bazel → SkippedNoBuild.
        let (_tmp, ws, ext) =
            setup_repo("gamma", false, true);
        let outcome = write_manifest_for_repo(
            &ws,
            "gamma",
            &ext,
            "bazel sync --only=gamma",
        );
        assert_eq!(
            outcome,
            ManifestWriteOutcome::SkippedNoBuild,
            "missing BUILD must produce SkippedNoBuild, got {:?}",
            outcome
        );
        let path = manifest_path(&ws, "gamma");
        assert!(
            !path.exists(),
            "no manifest must be written: {}",
            path.display()
        );
    }

    #[test]
    fn manifest_write_failure_reported() {
        // Force a write failure by making the
        // sidecar dir a NON-DIRECTORY file. The
        // `create_dir_all` call will fail.
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        let ext = ws.join("external").join("delta");
        fs::create_dir_all(&ext).unwrap();
        fs::write(ext.join("BUILD"), b"# stub").unwrap();
        // Sabotage: make the cache dir's parent a
        // regular file so create_dir_all can't
        // populate it.
        let cache_root = ws.join(".cache").join("rush");
        fs::create_dir_all(&cache_root).unwrap();
        // Put a file where the sidecar dir would go.
        fs::write(
            cache_root.join("external-manifests"),
            b"not a dir",
        )
        .unwrap();

        let outcome = write_manifest_for_repo(
            &ws,
            "delta",
            &ext,
            "bazel sync --only=delta",
        );
        assert!(
            outcome.is_failure(),
            "expected Failed, got {:?}",
            outcome
        );
        // The failure reason should mention the
        // blocking path so operators can diagnose.
        if let ManifestWriteOutcome::Failed(msg) = outcome {
            assert!(
                msg.contains("create_dir_all")
                    || msg.contains("external-manifests"),
                "failure reason should reference the issue: {}",
                msg
            );
        }
    }

    // =====================================================
    // Phase 5.C.4 commit 175 (Batch W): write-if-changed
    // manifest writer tests. Names per Pro's directive.
    // The on-disk manifest only gets rewritten when its
    // content (ignoring timestamp) actually changed.
    // =====================================================

    #[test]
    fn identical_manifest_not_rewritten() {
        // First write populates the sidecar.
        // Second write with the SAME structural
        // content (same repo, path, etc) must
        // return SkippedUnchanged — the timestamp
        // difference alone doesn't justify a
        // rewrite.
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        let ext = ws.join("external").join("alpha");
        fs::create_dir_all(&ext).unwrap();
        fs::write(ext.join("BUILD"), b"# stub").unwrap();

        let first = write_manifest_for_repo(
            &ws,
            "alpha",
            &ext,
            "bazel sync --only=alpha",
        );
        assert!(first.is_written(), "first write: {:?}", first);
        let path = match &first {
            ManifestWriteOutcome::Written(p) => p.clone(),
            other => panic!("expected Written, got {:?}", other),
        };
        let first_bytes = fs::read(&path).unwrap();

        // Sleep one second so a hypothetical
        // timestamp-driven rewrite WOULD produce
        // different bytes — this rules out
        // false-positive "skip because nothing
        // happened in this tick."
        std::thread::sleep(std::time::Duration::from_secs(1));

        let second = write_manifest_for_repo(
            &ws,
            "alpha",
            &ext,
            "bazel sync --only=alpha",
        );
        assert!(
            second.is_skipped_unchanged(),
            "second write must skip unchanged, got {:?}",
            second
        );
        // The on-disk bytes must be byte-for-byte
        // identical (timestamp from first write
        // preserved).
        let second_bytes = fs::read(&path).unwrap();
        assert_eq!(
            first_bytes, second_bytes,
            "skipped write must leave on-disk content unchanged"
        );
    }

    #[test]
    fn changed_manifest_rewritten() {
        // Write a manifest, then change a structural
        // field (e.g. bazel_command_shape via a
        // different caller invocation), and verify
        // the writer rewrites with Written.
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        let ext = ws.join("external").join("beta");
        fs::create_dir_all(&ext).unwrap();
        fs::write(ext.join("BUILD"), b"# stub").unwrap();

        let first = write_manifest_for_repo(
            &ws,
            "beta",
            &ext,
            "bazel sync --only=beta",
        );
        assert!(first.is_written(), "first write: {:?}", first);

        // Same repo, different command shape (a
        // realistic change — operator switched from
        // serial to combined invocations).
        let second = write_manifest_for_repo(
            &ws,
            "beta",
            &ext,
            "bazel sync --only=A --only=B (combined)",
        );
        assert!(
            second.is_written(),
            "structural change must rewrite, got {:?}",
            second
        );
    }

    #[test]
    fn write_failure_remains_non_fatal() {
        // Even with the write-if-changed optimization,
        // an I/O failure must surface as Failed (the
        // commit-169 non-fatal contract) and never
        // panic. Sabotage the sidecar dir like the
        // existing manifest_write_failure_reported
        // test, but verify the failure path even when
        // the on-disk-read short-circuit isn't
        // triggered (because there's nothing to read).
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        let ext = ws.join("external").join("epsilon");
        fs::create_dir_all(&ext).unwrap();
        fs::write(ext.join("BUILD"), b"# stub").unwrap();
        let cache_root = ws.join(".cache").join("rush");
        fs::create_dir_all(&cache_root).unwrap();
        fs::write(cache_root.join("external-manifests"), b"not a dir")
            .unwrap();

        let outcome = write_manifest_for_repo(
            &ws,
            "epsilon",
            &ext,
            "bazel sync --only=epsilon",
        );
        assert!(
            outcome.is_failure(),
            "non-fatal contract: failure surfaces as Failed enum, \
             never panic. Got {:?}",
            outcome
        );
        // is_skipped_unchanged must NOT be true for
        // a failed write (the buckets are
        // disjoint).
        assert!(!outcome.is_skipped_unchanged());
        assert!(!outcome.is_written());
    }

    // =====================================================
    // Phase 5.C.4 commit 149 (Batch S): reader tests
    // =====================================================

    fn write_complete_manifest(ws: &Path, repo_name: &str) {
        let dir = external_manifests_dir(ws);
        fs::create_dir_all(&dir).unwrap();
        // Phase 5.C.4 commit 179 (Batch W): a
        // "complete" manifest now carries a typed
        // ExternalRepoIdentity. The reader uses the
        // identity to decide Valid vs Incomplete;
        // the string fetch_rule_kind /
        // fetch_rule_fingerprint fields remain for
        // backward compatibility.
        let identity = ExternalRepoIdentity::HttpArchive {
            urls: vec!["https://example.com/x.tar.gz".to_string()],
            sha256: Some(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
                    .to_string(),
            ),
            strip_prefix: None,
            patches: vec![],
            build_file: None,
            canonical_id: None,
        };
        let fingerprint = fingerprint_from_identity(repo_name, &identity);
        let m = ExternalRepoManifest {
            schema_version: EXTERNAL_REPO_MANIFEST_SCHEMA_VERSION,
            repo_name: repo_name.to_string(),
            external_path: ws.join("bazel_output_base").join("external").join(repo_name),
            fetch_rule_kind: "http_archive".to_string(),
            fetch_rule_fingerprint: fingerprint,
            workspace_root: ws.to_path_buf(),
            source_url_or_path: Some("https://example.com/x.tar.gz".into()),
            commit_or_sha256: Some(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            ),
            materialized_at_unix: 1737128400,
            bazel_command_shape: "bazel sync --only=x".into(),
            rush_version_or_manifest_writer_version: "0.1.0".into(),
            success_marker: true,
            identity: Some(identity),
        };
        fs::write(
            manifest_path(ws, repo_name),
            serde_json::to_string_pretty(&m).unwrap(),
        )
        .unwrap();
    }

    fn write_conservative_manifest(ws: &Path, repo_name: &str) {
        let dir = external_manifests_dir(ws);
        fs::create_dir_all(&dir).unwrap();
        let m = ExternalRepoManifest::new_conservative(
            repo_name,
            ws.join("bazel_output_base").join("external").join(repo_name),
            ws.to_path_buf(),
            "bazel sync --only=x",
        );
        fs::write(
            manifest_path(ws, repo_name),
            serde_json::to_string_pretty(&m).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn valid_manifest_skips() {
        // A "complete" manifest — http_archive +
        // sha256 + URL all populated → Valid.
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        write_complete_manifest(&ws, "alpha");

        let status = read_manifest_status(&ws, "alpha");
        assert_eq!(status, ExternalRepoManifestStatus::Valid);
        assert!(status.is_valid_for_manifest_skip());
    }

    #[test]
    fn missing_manifest_does_not_skip_in_manifest_required_mode() {
        // No sidecar on disk → Missing. The materialized
        // skip logic in bzl_loader::evaluate_materialized_skip
        // sees Missing and produces SkippedMaterializedTrustMode
        // (with a stderr warning). In "manifest-required
        // mode" — encoded by the caller checking
        // status.is_valid_for_manifest_skip() — this
        // would block the skip. The reader's job here
        // is just to honestly report Missing.
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        let status = read_manifest_status(&ws, "absent_repo");
        assert_eq!(status, ExternalRepoManifestStatus::Missing);
        assert!(!status.is_valid_for_manifest_skip());
    }

    #[test]
    fn invalid_manifest_does_not_skip() {
        // Corrupt JSON → Invalid → not eligible for
        // manifest-validated skip.
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        let dir = external_manifests_dir(&ws);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            manifest_path(&ws, "broken"),
            b"{not json}",
        )
        .unwrap();

        let status = read_manifest_status(&ws, "broken");
        match status {
            ExternalRepoManifestStatus::Invalid(reason) => {
                assert!(
                    reason.contains("parse"),
                    "invalid reason should reference parse: {}",
                    reason
                );
            }
            other => panic!("expected Invalid, got {:?}", other),
        }

        // Schema mismatch is also Invalid.
        let tmp2 = TempDir::new().unwrap();
        let ws2 = tmp2.path().join("ws");
        let dir2 = external_manifests_dir(&ws2);
        fs::create_dir_all(&dir2).unwrap();
        let m_v999 = serde_json::json!({
            "schema_version": 999,
            "repo_name": "x",
            "external_path": "/tmp/x",
            "fetch_rule_kind": "unknown",
            "fetch_rule_fingerprint": "unknown",
            "workspace_root": "/tmp/ws",
            "materialized_at_unix": 0,
            "bazel_command_shape": "x",
            "rush_version_or_manifest_writer_version": "x",
            "success_marker": true,
        });
        fs::write(
            manifest_path(&ws2, "wrong_version"),
            serde_json::to_string(&m_v999).unwrap(),
        )
        .unwrap();
        let status2 = read_manifest_status(&ws2, "wrong_version");
        assert!(
            matches!(status2, ExternalRepoManifestStatus::Invalid(_)),
            "wrong schema_version must be Invalid, got {:?}",
            status2
        );
    }

    #[test]
    fn trust_mode_skips_without_manifest() {
        // A conservative manifest — "unknown" fields
        // throughout. Reader classifies as Incomplete
        // (NOT Valid for manifest-validated skip).
        // The bzl_loader's evaluate_materialized_skip
        // converts Incomplete into a trust-mode
        // skip with stderr warning.
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        write_conservative_manifest(&ws, "conservative");
        let status = read_manifest_status(&ws, "conservative");
        assert!(
            matches!(status, ExternalRepoManifestStatus::Incomplete(_)),
            "expected Incomplete(_), got {:?}",
            status
        );
        assert!(
            !status.is_valid_for_manifest_skip(),
            "Incomplete must not satisfy manifest-validated skip"
        );

        // No manifest at all → Missing, also not
        // eligible for manifest-validated skip.
        let status2 = read_manifest_status(&ws, "totally_missing");
        assert_eq!(status2, ExternalRepoManifestStatus::Missing);
        assert!(!status2.is_valid_for_manifest_skip());
    }

    // =====================================================
    // Phase 5.C.4 commit 179 (Batch W): manifest Valid
    // criteria tests. Names per Pro's directive. The
    // reader now uses typed identity completeness.
    // =====================================================

    fn write_manifest_with_identity(
        ws: &Path,
        repo_name: &str,
        identity: ExternalRepoIdentity,
    ) {
        let dir = external_manifests_dir(ws);
        fs::create_dir_all(&dir).unwrap();
        let fingerprint = fingerprint_from_identity(repo_name, &identity);
        let m = ExternalRepoManifest {
            schema_version: EXTERNAL_REPO_MANIFEST_SCHEMA_VERSION,
            repo_name: repo_name.to_string(),
            external_path: ws
                .join("bazel_output_base")
                .join("external")
                .join(repo_name),
            fetch_rule_kind: identity.rule_kind().to_string(),
            fetch_rule_fingerprint: fingerprint,
            workspace_root: ws.to_path_buf(),
            source_url_or_path: None,
            commit_or_sha256: None,
            materialized_at_unix: 1737128400,
            bazel_command_shape: format!(
                "bazel sync --only={}",
                repo_name
            ),
            rush_version_or_manifest_writer_version: "0.1.0".into(),
            success_marker: true,
            identity: Some(identity),
        };
        fs::write(
            manifest_path(ws, repo_name),
            serde_json::to_string_pretty(&m).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn complete_http_archive_manifest_valid() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        write_manifest_with_identity(
            &ws,
            "alpha",
            ExternalRepoIdentity::HttpArchive {
                urls: vec!["https://example.com/x.tgz".to_string()],
                sha256: Some("abc".to_string()),
                strip_prefix: None,
                patches: vec![],
                build_file: None,
                canonical_id: None,
            },
        );
        let status = read_manifest_status(&ws, "alpha");
        assert_eq!(status, ExternalRepoManifestStatus::Valid);
        assert!(status.is_valid_for_manifest_skip());
    }

    #[test]
    fn http_archive_without_sha_incomplete() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        write_manifest_with_identity(
            &ws,
            "beta",
            ExternalRepoIdentity::HttpArchive {
                urls: vec!["https://example.com/y.tgz".to_string()],
                sha256: None,
                strip_prefix: None,
                patches: vec![],
                build_file: None,
                canonical_id: None,
            },
        );
        let status = read_manifest_status(&ws, "beta");
        match &status {
            ExternalRepoManifestStatus::Incomplete(reason) => {
                assert!(
                    reason.contains("http_archive"),
                    "reason should mention http_archive: {}",
                    reason
                );
            }
            other => panic!("expected Incomplete, got {:?}", other),
        }
        assert!(!status.is_valid_for_manifest_skip());
    }

    #[test]
    fn git_repository_with_branch_incomplete() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        write_manifest_with_identity(
            &ws,
            "gamma",
            ExternalRepoIdentity::GitRepository {
                remote: "https://github.com/example/r.git".to_string(),
                commit: None,
                tag: None,
                branch: Some("main".to_string()),
                shallow_since: None,
            },
        );
        let status = read_manifest_status(&ws, "gamma");
        assert!(matches!(
            status,
            ExternalRepoManifestStatus::Incomplete(_)
        ));
        assert!(!status.is_valid_for_manifest_skip());
    }

    #[test]
    fn git_repository_with_commit_valid() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        write_manifest_with_identity(
            &ws,
            "delta",
            ExternalRepoIdentity::GitRepository {
                remote: "https://github.com/example/r.git".to_string(),
                commit: Some("deadbeef".to_string()),
                tag: None,
                branch: None,
                shallow_since: None,
            },
        );
        let status = read_manifest_status(&ws, "delta");
        assert_eq!(status, ExternalRepoManifestStatus::Valid);
    }

    #[test]
    fn local_repository_valid_local_only() {
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        write_manifest_with_identity(
            &ws,
            "epsilon",
            ExternalRepoIdentity::LocalRepository {
                path: "/abs/path".to_string(),
            },
        );
        let status = read_manifest_status(&ws, "epsilon");
        assert_eq!(
            status,
            ExternalRepoManifestStatus::Valid,
            "CompleteLocal must satisfy Valid"
        );
    }

    #[test]
    fn fingerprint_mismatch_stale() {
        // Write a complete manifest, then hand-edit
        // the fingerprint to something else. Reader
        // detects the drift and returns Stale.
        let tmp = TempDir::new().unwrap();
        let ws = tmp.path().join("ws");
        fs::create_dir_all(&ws).unwrap();
        write_manifest_with_identity(
            &ws,
            "zeta",
            ExternalRepoIdentity::HttpArchive {
                urls: vec!["https://example.com/z.tgz".to_string()],
                sha256: Some("abc".to_string()),
                strip_prefix: None,
                patches: vec![],
                build_file: None,
                canonical_id: None,
            },
        );
        // Hand-corrupt the fingerprint on disk.
        let path = manifest_path(&ws, "zeta");
        let raw = fs::read_to_string(&path).unwrap();
        let modified = raw.replace(
            // Find and replace any "fetch_rule_fingerprint": "..."
            // line with a deliberately-wrong value.
            "\"fetch_rule_fingerprint\":",
            "\"fetch_rule_fingerprint_orig\": \"removed\",\n  \"fetch_rule_fingerprint\":",
        );
        let modified = modified.replace(
            &format!(
                "\"fetch_rule_fingerprint\": \"{}\"",
                fingerprint_from_identity(
                    "zeta",
                    &ExternalRepoIdentity::HttpArchive {
                        urls: vec!["https://example.com/z.tgz".to_string()],
                        sha256: Some("abc".to_string()),
                        strip_prefix: None,
                        patches: vec![],
                        build_file: None,
                        canonical_id: None,
                    },
                )
            ),
            "\"fetch_rule_fingerprint\": \"deliberately_wrong_fingerprint\"",
        );
        fs::write(&path, modified).unwrap();
        let status = read_manifest_status(&ws, "zeta");
        match &status {
            ExternalRepoManifestStatus::Stale(reason) => {
                assert!(
                    reason.contains("fingerprint"),
                    "reason should cite fingerprint: {}",
                    reason
                );
            }
            other => panic!("expected Stale, got {:?}", other),
        }
        assert!(!status.is_valid_for_manifest_skip());
    }

    #[test]
    fn manifest_status_strings() {
        // Stable strings — wire format consumers rely
        // on these.
        assert_eq!(ExternalRepoManifestStatus::Valid.as_str(), "valid");
        assert_eq!(
            ExternalRepoManifestStatus::Missing.as_str(),
            "missing"
        );
        assert_eq!(
            ExternalRepoManifestStatus::Invalid("x".into()).as_str(),
            "invalid"
        );
        assert_eq!(
            ExternalRepoManifestStatus::Stale("x".into()).as_str(),
            "stale"
        );
        assert_eq!(
            ExternalRepoManifestStatus::Incomplete("x".into()).as_str(),
            "incomplete"
        );
        assert_eq!(
            ExternalRepoManifestStatus::Unknown("y".into()).as_str(),
            "unknown"
        );
        // Only Valid permits manifest-validated skip.
        assert!(
            ExternalRepoManifestStatus::Valid
                .is_valid_for_manifest_skip()
        );
        assert!(!ExternalRepoManifestStatus::Missing
            .is_valid_for_manifest_skip());
        assert!(!ExternalRepoManifestStatus::Incomplete("x".into())
            .is_valid_for_manifest_skip());
    }
}
