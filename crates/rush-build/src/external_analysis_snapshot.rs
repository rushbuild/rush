//! Persistent analysis snapshots for graphs produced by an external analyzer.
//!
//! Native Rush analysis retains Rust rule objects in memory. External analysis
//! instead persists a plain action DAG plus the complete
//! [`DependencyManifest`] observed while the analyzer constructed it. Loading
//! a snapshot never runs the analyzer: Rush validates the manifest and either
//! admits the whole graph or rejects it before any action can replay.

use crate::analysis_snapshot::daemon_compat_fingerprint;
use anyhow::{Context, Result};
use rush_action::action_manifest::ManifestResourceClass;
use rush_action::external_action_key::{
    ExternalArgumentKey, ExternalArgumentPart, ExternalCredentialState, ExternalInputKey,
    ExternalInputValue, ExternalOutputKind, ExternalPathKey, ExternalSignalState,
};
use rush_command_core::CommandSemantics;
use rush_incremental::{DependencyManifest, Staleness};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Stable schema name stored in every external analysis snapshot.
pub const EXTERNAL_ANALYSIS_SNAPSHOT_SCHEMA: &str = "org.rushbuild.external-analysis-snapshot";

/// Current external analysis snapshot layout.
pub const EXTERNAL_ANALYSIS_SNAPSHOT_VERSION: u32 = 14;

/// Reserved execution-property key for a fail-closed must-execute policy.
pub(crate) const MANDATORY_EXECUTION_PROPERTY: &str = "rush.mandatory-execution";
/// Policy requiring uncacheable local execution with clean descriptor entry.
pub(crate) const PROCFS_CLEAN_ENTRY_MANDATORY_EXECUTION: &str = "procfs-live-clean-entry-v2";
/// Policy permitting cache reuse after content-keying host-global procfs.
pub(crate) const PROCFS_CLEAN_ENTRY_CACHEABLE_EXECUTION: &str =
    "procfs-content-keyed-clean-entry-v3";
pub(crate) const RUNTIME_DIRECTORY_CHECKPOINT_PREFIX: &str =
    "rush.runtime-directory-checkpoint.v1.";

const GNU_MAKE_JOBSERVER_ENVIRONMENTS: [&[u8]; 2] = [b"MAKEFLAGS", b"MFLAGS"];
const GNU_MAKE_JOBSERVER_AUTH_OPTION: &[u8] = b"--jobserver-auth=";
const GNU_MAKE_JOBSERVER_FDS_OPTION: &[u8] = b"--jobserver-fds=";
const CANONICAL_CLOSED_GNU_MAKE_JOBSERVER: &[u8] = b"3,4";

const KEY_DOMAIN: &[u8] = b"rush.external-analysis-key.v1\n";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ExternalRuntimeDirectoryCheckpoint {
    pub(crate) path: ExternalPathKey,
    pub(crate) members: Vec<Vec<u8>>,
    #[serde(default)]
    pub(crate) observation_ordinal: u64,
    #[serde(default)]
    pub(crate) before_actions: Vec<String>,
    #[serde(default)]
    pub(crate) after_actions: Vec<String>,
}

pub(crate) fn encode_runtime_directory_checkpoint(
    index: usize,
    path: &ExternalPathKey,
    members: &[Vec<u8>],
) -> Result<(String, String)> {
    encode_ordered_runtime_directory_checkpoint(
        index,
        path,
        members,
        u64::try_from(index).context("runtime directory checkpoint index does not fit u64")?,
        &[],
        &[],
    )
}

pub(crate) fn encode_ordered_runtime_directory_checkpoint(
    index: usize,
    path: &ExternalPathKey,
    members: &[Vec<u8>],
    observation_ordinal: u64,
    before_actions: &[String],
    after_actions: &[String],
) -> Result<(String, String)> {
    let checkpoint = ExternalRuntimeDirectoryCheckpoint {
        path: path.clone(),
        members: members.to_vec(),
        observation_ordinal,
        before_actions: before_actions.to_vec(),
        after_actions: after_actions.to_vec(),
    };
    validate_runtime_directory_checkpoint(&checkpoint)?;
    Ok((
        format!("{RUNTIME_DIRECTORY_CHECKPOINT_PREFIX}{index:016x}"),
        serde_json::to_string(&checkpoint)
            .context("encode external runtime directory checkpoint")?,
    ))
}

pub(crate) fn runtime_directory_checkpoints(
    properties: &BTreeMap<String, String>,
) -> Result<Vec<ExternalRuntimeDirectoryCheckpoint>> {
    let mut checkpoints = Vec::new();
    for (name, value) in properties {
        let Some(suffix) = name.strip_prefix(RUNTIME_DIRECTORY_CHECKPOINT_PREFIX) else {
            continue;
        };
        let index = usize::from_str_radix(suffix, 16)
            .context("external runtime directory checkpoint has an invalid index")?;
        if suffix.len() != 16 || index != checkpoints.len() {
            anyhow::bail!("external runtime directory checkpoints are not contiguous");
        }
        let checkpoint: ExternalRuntimeDirectoryCheckpoint =
            serde_json::from_str(value).context("decode external runtime directory checkpoint")?;
        validate_runtime_directory_checkpoint(&checkpoint)?;
        checkpoints.push(checkpoint);
    }
    Ok(checkpoints)
}

fn validate_runtime_directory_checkpoint(
    checkpoint: &ExternalRuntimeDirectoryCheckpoint,
) -> Result<()> {
    if checkpoint.path.root != "obj"
        || checkpoint.members.windows(2).any(|pair| pair[0] >= pair[1])
        || checkpoint
            .members
            .iter()
            .any(|member| member.is_empty() || member.contains(&0) || member.contains(&b'/'))
        || checkpoint
            .before_actions
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
        || checkpoint
            .after_actions
            .windows(2)
            .any(|pair| pair[0] >= pair[1])
        || checkpoint.before_actions.iter().any(String::is_empty)
        || checkpoint.after_actions.iter().any(String::is_empty)
        || checkpoint
            .before_actions
            .iter()
            .any(|action| checkpoint.after_actions.binary_search(action).is_ok())
    {
        anyhow::bail!("external runtime directory checkpoint is malformed");
    }
    Ok(())
}

/// Exact identity of the analyzer invocation used on a snapshot miss.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalAnalyzerIdentity {
    /// Analyzer family, such as `gnu-make`.
    pub kind: String,
    /// Digest of the analyzer executable bytes.
    pub executable_digest: [u8; 32],
    /// Exact analyzer argv, including argv[0].
    pub argv: Vec<Vec<u8>>,
    /// Analyzer working directory.
    pub working_directory: PathBuf,
    /// Explicit environment observed by the capture supervisor.
    pub environment: BTreeMap<Vec<u8>, Vec<u8>>,
    /// Explicit build options not already represented in argv.
    pub build_options: BTreeMap<String, Vec<u8>>,
    /// Platform properties that can change graph construction.
    pub platform: BTreeMap<String, String>,
}

/// Lookup key for one persisted external analysis generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalAnalysisKey {
    /// Canonical source workspace root.
    pub workspace_root: PathBuf,
    /// Canonical output root.
    pub output_root: PathBuf,
    /// Analyzer family.
    pub analyzer_kind: String,
    /// Digest of all analyzer invocation semantics.
    pub analyzer_invocation_digest: [u8; 32],
    /// Requested build goals, in command order.
    pub requested_goals: Vec<Vec<u8>>,
    /// Rush binary and persistent-schema compatibility identity.
    pub rush_compat_fingerprint: [u8; 32],
}

impl ExternalAnalysisKey {
    /// Construct a key and bind it to the running Rush binary.
    pub fn new(
        workspace_root: PathBuf,
        output_root: PathBuf,
        analyzer: &ExternalAnalyzerIdentity,
        requested_goals: Vec<Vec<u8>>,
    ) -> Self {
        Self {
            workspace_root,
            output_root,
            analyzer_kind: analyzer.kind.clone(),
            analyzer_invocation_digest: analyzer_digest(analyzer),
            requested_goals,
            rush_compat_fingerprint: daemon_compat_fingerprint(),
        }
    }

    /// Stable filename hash. The stored full key remains authoritative.
    pub fn stable_hash(&self) -> [u8; 32] {
        let bytes =
            bincode::serialize(self).expect("ExternalAnalysisKey contains only serializable data");
        let mut hasher = blake3::Hasher::new();
        hasher.update(KEY_DOMAIN);
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
        *hasher.finalize().as_bytes()
    }
}

/// How replay must observe a path when calculating an action key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalInputKind {
    /// Hash file bytes and executable mode.
    File,
    /// Hash the symbolic-link target without following it.
    Symlink,
    /// Hash the complete tree shape and values.
    Tree,
    /// Require the path to remain absent.
    Missing,
    /// Hash sorted directory-member names.
    DirectoryListing,
    /// Require a directory and hash its permission mode.
    Directory,
}

/// One path input whose current value participates in an action key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalInput {
    /// Stable rooted path.
    pub path: ExternalPathKey,
    /// Observation and hashing contract.
    pub kind: ExternalInputKind,
}

/// Exact process invocation in an external action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalCommand {
    /// Stable rooted executable path.
    pub executable: ExternalPathKey,
    /// Exact argv[0] presented to the executable.
    pub argv0: ExternalArgumentKey,
    /// Arguments after argv[0], preserving literals and canonical paths.
    pub argv: Vec<ExternalArgumentKey>,
    /// Complete environment override when this command differs from the action default.
    #[serde(default)]
    pub environment: Option<BTreeMap<Vec<u8>, ExternalArgumentKey>>,
    /// Working-directory override when this command differs from the action default.
    #[serde(default)]
    pub working_directory: Option<ExternalPathKey>,
}

/// Dynamic-dependency data consumed after a successful execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalDepfile {
    /// Parser contract, such as `gcc` or `kbuild_fixdep`.
    pub format: String,
    /// Stable rooted depfile path.
    pub path: ExternalPathKey,
}

/// One output name and required filesystem shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalOutput {
    /// Stable rooted output path.
    pub path: ExternalPathKey,
    /// Output shape used for validation and CAS restoration.
    pub kind: ExternalOutputKind,
    /// Exact permission and special bits required after replay.
    pub mode: u32,
}

/// One action in a persisted external graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalAction {
    /// Exporter-local identity used only for graph edges and diagnostics.
    pub logical_id: String,
    /// Human-readable action family.
    pub mnemonic: String,
    /// Exact process sequence. Rush executes it in order as one cache unit.
    pub commands: Vec<ExternalCommand>,
    /// Stable rooted working directory.
    pub working_directory: ExternalPathKey,
    /// Default replay environment for commands without an override.
    pub environment: BTreeMap<Vec<u8>, ExternalArgumentKey>,
    /// Exact signal mask and ignored dispositions installed for execution.
    pub signal_state: ExternalSignalState,
    /// Exact Linux file-creation mask installed for execution.
    pub file_creation_mask: u32,
    /// Exact Linux credential state required for execution.
    pub credentials: ExternalCredentialState,
    /// Inputs whose current values participate in the action key.
    pub normal_inputs: Vec<ExternalInput>,
    /// Scheduling-only paths. Their values do not enter the action key.
    pub order_only_inputs: Vec<ExternalPathKey>,
    /// Declared outputs.
    pub outputs: Vec<ExternalOutput>,
    /// Logical predecessor actions, independent of output-path inference.
    pub dependencies: Vec<String>,
    /// Named Kconfig symbols whose actual values participate in the key.
    pub config_deps: Vec<String>,
    /// Optional dynamic-dependency descriptor.
    pub depfile: Option<ExternalDepfile>,
    /// Suppress downstream work when rebuilt output values are unchanged.
    pub restat: bool,
    /// Force execution even when the canonical action key has a cache entry.
    pub always_run: bool,
    /// Permit local/shared action-cache lookup and publication.
    pub cacheable: bool,
    /// Keep execution on the local machine.
    pub local_only: bool,
    /// Require a sandbox policy that is part of the canonical action key.
    pub sandbox: bool,
    /// Resource policy identity.
    pub resource: ManifestResourceClass,
    /// Executor properties that can affect output bytes.
    pub execution_properties: BTreeMap<String, String>,
}

/// Source for value-based `CONFIG_*` action inputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalKconfigSource {
    /// Existing Kconfig output, normally `.config` or `auto.conf`.
    pub path: ExternalPathKey,
    /// Canonical bytes used only when the configured output is absent.
    pub captured_bytes: Vec<u8>,
}

/// One analyzer-created path that replay can restore without rerunning the
/// analyzer. Existing paths always remain authoritative and are never
/// overwritten.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalInitialInput {
    /// Stable path below the external output root.
    pub path: ExternalPathKey,
    /// Exact value captured after successful analysis.
    pub value: ExternalInitialInputValue,
}

/// Exact materializable value of one analyzer-created input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalInitialInputValue {
    /// Regular-file bytes and complete permission mode.
    File {
        /// Exact file contents.
        bytes: Vec<u8>,
        /// Permission and special bits.
        mode: u32,
    },
    /// An empty directory and its complete permission mode.
    Directory {
        /// Permission and special bits.
        mode: u32,
    },
    /// A symbolic link and its lossless target bytes.
    Symlink {
        /// Target bytes returned by `readlink`.
        target: Vec<u8>,
    },
}

/// Captured key value used only when replay starts from an empty output root.
///
/// The live value is revalidated before a cache miss may execute, so this can
/// admit a cache hit but can never authorize execution against different
/// filesystem state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalEmptyOutputInput {
    /// Stable observed path below the external output root.
    pub path: ExternalPathKey,
    /// Observation contract used by the consuming action.
    pub kind: ExternalInputKind,
    /// Exact value observed after successful analysis.
    pub value: ExternalInputValue,
}

/// Captured analyzer continuation guarded by an exact producer closure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalAnalysisBarrier {
    /// Complete cumulative set of actions allowed before this checkpoint.
    pub producer_closure: Vec<String>,
    /// Exact generated values observed by the analyzer at this checkpoint.
    pub expected_inputs: Vec<ExternalInputKey>,
    /// Exact complement that cannot execute before the values match.
    pub blocked_actions: Vec<String>,
}

/// Complete serializable external action graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalActionGraph {
    /// Absolute runtime roots addressed by [`ExternalPathKey::root`].
    pub roots: BTreeMap<String, PathBuf>,
    /// Optional canonical Kconfig value source.
    pub kconfig: Option<ExternalKconfigSource>,
    /// Captured bootstrap inputs that can be restored without analysis.
    pub initial_inputs: Vec<ExternalInitialInput>,
    /// Captured key values allowed only for an empty output-root start.
    pub empty_output_inputs: Vec<ExternalEmptyOutputInput>,
    /// Actions in stable capture order.
    pub actions: Vec<ExternalAction>,
    /// Explicit analyzer-continuation barriers for generated graph inputs.
    pub analysis_barriers: Vec<ExternalAnalysisBarrier>,
    /// Outputs requested by the user and materialized before success returns.
    pub requested_outputs: Vec<ExternalPathKey>,
}

/// Atomically persisted external analysis generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalAnalysisSnapshot {
    /// Stable schema name.
    pub schema: String,
    /// On-disk layout version.
    pub version: u32,
    /// Full authenticated lookup key.
    pub key: ExternalAnalysisKey,
    /// Exact analyzer identity for recapture and diagnostics.
    pub analyzer: ExternalAnalyzerIdentity,
    /// Every filesystem, environment, option, and probe input to analysis.
    pub analysis_manifest: DependencyManifest,
    /// Plain replayable action DAG.
    pub graph: ExternalActionGraph,
}

impl ExternalAnalysisSnapshot {
    /// Construct and validate a snapshot using the current Rush identity.
    pub fn new(
        key: ExternalAnalysisKey,
        analyzer: ExternalAnalyzerIdentity,
        analysis_manifest: DependencyManifest,
        graph: ExternalActionGraph,
    ) -> Result<Self> {
        let snapshot = Self {
            schema: EXTERNAL_ANALYSIS_SNAPSHOT_SCHEMA.to_string(),
            version: EXTERNAL_ANALYSIS_SNAPSHOT_VERSION,
            key,
            analyzer,
            analysis_manifest,
            graph,
        };
        snapshot.validate_structure()?;
        Ok(snapshot)
    }

    /// Validate graph structure without consulting mutable filesystem state.
    pub fn validate_structure(&self) -> Result<()> {
        if self.schema != EXTERNAL_ANALYSIS_SNAPSHOT_SCHEMA {
            anyhow::bail!("unsupported external analysis schema {:?}", self.schema);
        }
        if self.version != EXTERNAL_ANALYSIS_SNAPSHOT_VERSION {
            anyhow::bail!("unsupported external analysis version {}", self.version);
        }
        if self.key.rush_compat_fingerprint != daemon_compat_fingerprint() {
            anyhow::bail!("external analysis snapshot has an incompatible Rush identity");
        }
        if self.key.analyzer_kind != self.analyzer.kind
            || self.key.analyzer_invocation_digest != analyzer_digest(&self.analyzer)
        {
            anyhow::bail!("external analyzer identity does not match its analysis key");
        }
        if !self.key.workspace_root.is_absolute()
            || !self.key.output_root.is_absolute()
            || !self.analyzer.working_directory.is_absolute()
        {
            anyhow::bail!("external analysis roots and analyzer cwd must be absolute");
        }
        if self.graph.roots.get("src") != Some(&self.key.workspace_root)
            || self.graph.roots.get("obj") != Some(&self.key.output_root)
        {
            anyhow::bail!("external graph src/obj roots do not match the analysis key");
        }
        validate_graph(&self.graph)
    }
}

/// Stable telemetry decision emitted by external snapshot admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ExternalSnapshotDecision {
    /// Every analysis dependency is certified and no continuation check is needed.
    SnapshotHit,
    /// A producer closure must reproduce captured analysis values before replay.
    SnapshotContinuationRequired,
    /// No compatible persisted generation exists.
    SnapshotMissing,
    /// A recorded analysis observation changed.
    SnapshotStale,
    /// Persisted bytes, structure, ownership, or compatibility are invalid.
    SnapshotInvalid,
    /// The selected external build mode is outside the supported policy.
    SnapshotUnsupported,
}

impl ExternalSnapshotDecision {
    /// Exact stable spelling used by CLI and structured telemetry.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SnapshotHit => "SNAPSHOT_HIT",
            Self::SnapshotContinuationRequired => "SNAPSHOT_CONTINUATION_REQUIRED",
            Self::SnapshotMissing => "SNAPSHOT_MISSING",
            Self::SnapshotStale => "SNAPSHOT_STALE",
            Self::SnapshotInvalid => "SNAPSHOT_INVALID",
            Self::SnapshotUnsupported => "SNAPSHOT_UNSUPPORTED",
        }
    }
}

/// Result of authenticating and validating a persisted snapshot.
///
/// The categories deliberately preserve why replay was not admitted. A
/// controller may recapture on `Missing`, `Stale`, or `Invalid`, but must not
/// silently turn `Unsupported` into a reusable generation. A continuation
/// carries the authenticated snapshot so its producer closure can revalidate
/// the captured analyzer values before any blocked action runs.
#[derive(Debug)]
pub enum ExternalSnapshotAdmission {
    /// The entire graph is admitted for replay without invoking its analyzer.
    Hit(ExternalAnalysisSnapshot),
    /// Analysis inputs are fresh, but captured continuation values need replay validation.
    ContinuationRequired(ExternalAnalysisSnapshot),
    /// No compatible snapshot exists at the selected generation path.
    Missing(String),
    /// The snapshot existed, but an analysis observation changed.
    Stale(String),
    /// Snapshot decoding or structural validation failed closed.
    Invalid(String),
    /// The controller selected a build mode that policy does not support.
    Unsupported(String),
}

impl ExternalSnapshotAdmission {
    /// Stable decision for CLI output and structured telemetry.
    pub const fn decision(&self) -> ExternalSnapshotDecision {
        match self {
            Self::Hit(_) => ExternalSnapshotDecision::SnapshotHit,
            Self::ContinuationRequired(_) => ExternalSnapshotDecision::SnapshotContinuationRequired,
            Self::Missing(_) => ExternalSnapshotDecision::SnapshotMissing,
            Self::Stale(_) => ExternalSnapshotDecision::SnapshotStale,
            Self::Invalid(_) => ExternalSnapshotDecision::SnapshotInvalid,
            Self::Unsupported(_) => ExternalSnapshotDecision::SnapshotUnsupported,
        }
    }

    /// Construct the policy result used before a snapshot path is consulted.
    pub fn unsupported(reason: impl Into<String>) -> Self {
        Self::Unsupported(reason.into())
    }

    /// Diagnostic reason for decisions that do not carry a snapshot.
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Missing(reason)
            | Self::Stale(reason)
            | Self::Invalid(reason)
            | Self::Unsupported(reason) => Some(reason),
            Self::Hit(_) | Self::ContinuationRequired(_) => None,
        }
    }
}

/// Atomically persist a fully validated snapshot.
pub fn persist(path: &Path, snapshot: &ExternalAnalysisSnapshot) -> Result<()> {
    snapshot.validate_structure()?;
    let parent = path
        .parent()
        .context("external analysis snapshot path has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("create external snapshot directory {}", parent.display()))?;
    let bytes = bincode::serialize(snapshot).context("serialize external analysis snapshot")?;
    let temporary = parent.join(format!(
        ".external-analysis-{}-{}.tmp",
        std::process::id(),
        unique_suffix()
    ));
    let write_result = (|| -> Result<()> {
        let mut file = std::fs::File::create(&temporary)
            .with_context(|| format!("create {}", temporary.display()))?;
        file.write_all(&bytes)
            .with_context(|| format!("write {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", temporary.display()))?;
        std::fs::rename(&temporary, path).with_context(|| {
            format!(
                "publish external analysis snapshot {} -> {}",
                temporary.display(),
                path.display()
            )
        })?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    write_result?;
    if let Ok(directory) = std::fs::File::open(parent) {
        let _ = directory.sync_all();
    }
    Ok(())
}

/// Load a snapshot and admit it only when the full key and analysis manifest
/// remain fresh. Every rejection is a miss; no partially valid graph escapes.
pub fn load_if_fresh(
    path: &Path,
    expected_key: &ExternalAnalysisKey,
    semantics: &CommandSemantics,
) -> ExternalSnapshotAdmission {
    let diagnostics = std::env::var("RUSH_EXTERNAL_OBSERVATION_STATS").as_deref() == Ok("1");
    let started = Instant::now();
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return ExternalSnapshotAdmission::Missing(format!("snapshot read failed: {error}"))
        }
        Err(error) => {
            return ExternalSnapshotAdmission::Invalid(format!("snapshot read failed: {error}"))
        }
    };
    let read_elapsed = started.elapsed();
    let admission = load_serialized_if_fresh(&bytes, expected_key, semantics);
    if diagnostics {
        eprintln!(
            "external snapshot load: bytes={} read_us={} total_us={}",
            bytes.len(),
            read_elapsed.as_micros(),
            started.elapsed().as_micros(),
        );
    }
    admission
}

/// Admit already authenticated snapshot bytes without reading them again.
///
/// Generation stores use this after checking the content digest named by an
/// atomic commit marker. Keeping decoding and freshness policy here ensures a
/// direct snapshot path and a generation-selected snapshot fail closed in the
/// same way.
pub(crate) fn load_serialized_if_fresh(
    bytes: &[u8],
    expected_key: &ExternalAnalysisKey,
    semantics: &CommandSemantics,
) -> ExternalSnapshotAdmission {
    let diagnostics = std::env::var("RUSH_EXTERNAL_OBSERVATION_STATS").as_deref() == Ok("1");
    let started = Instant::now();
    let mut snapshot: ExternalAnalysisSnapshot = match bincode::deserialize(&bytes) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return ExternalSnapshotAdmission::Invalid(format!("snapshot decode failed: {error}"))
        }
    };
    let decode_elapsed = started.elapsed();
    if let Err(error) = snapshot.validate_structure() {
        return ExternalSnapshotAdmission::Invalid(error.to_string());
    }
    let structure_elapsed = started.elapsed() - decode_elapsed;
    if &snapshot.key != expected_key {
        return ExternalSnapshotAdmission::Missing(
            "no snapshot generation matches the external analysis key".to_string(),
        );
    }
    let manifest_started = Instant::now();
    let validated = snapshot.analysis_manifest.validate(semantics);
    let manifest_elapsed = manifest_started.elapsed();
    if diagnostics {
        eprintln!(
            "external snapshot admission: dependencies={} decode_us={} structure_us={} manifest_us={} total_us={}",
            snapshot.analysis_manifest.len(),
            decode_elapsed.as_micros(),
            structure_elapsed.as_micros(),
            manifest_elapsed.as_micros(),
            started.elapsed().as_micros(),
        );
    }
    match validated.staleness {
        Staleness::Fresh => {
            snapshot.analysis_manifest = validated.refreshed;
            if snapshot.graph.analysis_barriers.is_empty() {
                ExternalSnapshotAdmission::Hit(snapshot)
            } else {
                ExternalSnapshotAdmission::ContinuationRequired(snapshot)
            }
        }
        Staleness::Stale { reason, .. } => ExternalSnapshotAdmission::Stale(reason),
    }
}

fn validate_graph(graph: &ExternalActionGraph) -> Result<()> {
    if graph.roots.is_empty() {
        anyhow::bail!("external action graph has no declared roots");
    }
    for (name, root) in &graph.roots {
        if name.is_empty() || root.as_os_str().is_empty() || !root.is_absolute() {
            anyhow::bail!("external action graph has invalid root {name:?}");
        }
    }

    let mut action_index = HashMap::with_capacity(graph.actions.len());
    let mut output_paths = BTreeSet::new();
    let mut output_producers = BTreeMap::new();
    for (index, action) in graph.actions.iter().enumerate() {
        if action.logical_id.is_empty() || action.commands.is_empty() || action.outputs.is_empty() {
            anyhow::bail!("external action {index} is missing identity, commands, or outputs");
        }
        action
            .signal_state
            .validate()
            .with_context(|| format!("action {:?} has invalid signal state", action.logical_id))?;
        if action.file_creation_mask & !0o777 != 0 {
            anyhow::bail!(
                "action {:?} has an invalid file-creation mask",
                action.logical_id
            );
        }
        action.credentials.validate().with_context(|| {
            format!(
                "action {:?} has invalid credential state",
                action.logical_id
            )
        })?;
        if let Some(policy) = action
            .execution_properties
            .get(MANDATORY_EXECUTION_PROPERTY)
        {
            match policy.as_str() {
                PROCFS_CLEAN_ENTRY_MANDATORY_EXECUTION
                    if !action.cacheable
                        && !action.always_run
                        && action.local_only
                        && action.sandbox => {}
                PROCFS_CLEAN_ENTRY_CACHEABLE_EXECUTION
                    if action.cacheable
                        && !action.always_run
                        && action.local_only
                        && action.sandbox => {}
                PROCFS_CLEAN_ENTRY_MANDATORY_EXECUTION | PROCFS_CLEAN_ENTRY_CACHEABLE_EXECUTION => {
                    anyhow::bail!(
                        "action {:?} has an invalid procfs execution policy",
                        action.logical_id
                    );
                }
                _ => {
                    anyhow::bail!(
                        "action {:?} has an unsupported mandatory-execution policy",
                        action.logical_id
                    );
                }
            }
        }
        let runtime_directory_checkpoints =
            runtime_directory_checkpoints(&action.execution_properties)?;
        if let Some(policy) = action
            .execution_properties
            .get("rush.kbuild.directory-materialization")
        {
            let [command] = action.commands.as_slice() else {
                anyhow::bail!(
                    "action {:?} has invalid directory materialization commands",
                    action.logical_id
                );
            };
            let valid_argv0 = matches!(
                &command.argv0,
                ExternalArgumentKey::Literal(value) if value == b"mkdir"
            );
            let valid_flag = matches!(
                command.argv.first(),
                Some(ExternalArgumentKey::Literal(value)) if value == b"-p"
            );
            let normalize_relative_path = |value: &[u8]| -> Option<Vec<Vec<u8>>> {
                if value.is_empty() || value.starts_with(b"/") {
                    return None;
                }
                let components = value
                    .split(|byte| *byte == b'/')
                    .filter(|component| !component.is_empty())
                    .map(|component| component.to_vec())
                    .collect::<Vec<_>>();
                (!components.is_empty()
                    && components
                        .iter()
                        .all(|component| component != b"." && component != b".."))
                .then_some(components)
            };
            let literal_targets = command
                .argv
                .iter()
                .skip(1)
                .map(|argument| match argument {
                    ExternalArgumentKey::Literal(value) if !value.starts_with(b"-") => {
                        normalize_relative_path(value)
                    }
                    _ => None,
                })
                .collect::<Option<Vec<_>>>();
            let valid_targets = literal_targets
                .as_ref()
                .is_some_and(|targets| !targets.is_empty());
            let outputs_match_targets = literal_targets.as_ref().is_some_and(|targets| {
                targets.iter().all(|target| {
                    action.outputs.iter().any(|output| {
                        output.kind == ExternalOutputKind::Tree
                            && output.path.root == "obj"
                            && normalize_relative_path(&output.path.path).as_ref() == Some(target)
                    })
                }) && action.outputs.iter().all(|output| {
                    output.path.root == "obj"
                        && normalize_relative_path(&output.path.path).is_some_and(|output_path| {
                            targets.iter().any(|target| target.starts_with(&output_path))
                        })
                })
            });
            if policy != "declared-mkdir-p-v1"
                || action.cacheable
                || action.always_run
                || !action.local_only
                || !action.restat
                || action.working_directory.root != "obj"
                || action.working_directory.path != b"."
                || command.executable.root != "host"
                || command.executable.path != b"usr/bin/mkdir"
                || !valid_argv0
                || !valid_flag
                || !valid_targets
                || !outputs_match_targets
                || command.argv.len() < 2
                || command.environment.is_some()
                || command.working_directory.is_some()
                || action.outputs.is_empty()
                || action
                    .outputs
                    .iter()
                    .any(|output| output.kind != ExternalOutputKind::Tree)
                || !runtime_directory_checkpoints.is_empty()
            {
                anyhow::bail!(
                    "action {:?} has an invalid directory materialization policy",
                    action.logical_id
                );
            }
        }
        for checkpoint in &runtime_directory_checkpoints {
            validate_path(&checkpoint.path, &graph.roots).with_context(|| {
                format!(
                    "validate action {:?} runtime directory checkpoint",
                    action.logical_id
                )
            })?;
        }
        if !runtime_directory_checkpoints.is_empty()
            && !action
                .execution_properties
                .contains_key(MANDATORY_EXECUTION_PROPERTY)
        {
            anyhow::bail!(
                "action {:?} has runtime checkpoints without mandatory execution",
                action.logical_id
            );
        }
        if action_index
            .insert(action.logical_id.as_str(), index)
            .is_some()
        {
            anyhow::bail!(
                "duplicate external logical action id {:?}",
                action.logical_id
            );
        }
        validate_path(&action.working_directory, &graph.roots)?;
        validate_environment(&action.environment, &action.logical_id, &graph.roots)?;
        if action
            .execution_properties
            .contains_key(MANDATORY_EXECUTION_PROPERTY)
        {
            validate_standard_descriptor_environment(
                &action.environment,
                &action.logical_id,
            )?;
        }
        for command in &action.commands {
            validate_path(&command.executable, &graph.roots)?;
            validate_argument(&command.argv0, &action.logical_id, &graph.roots)?;
            for argument in &command.argv {
                validate_argument(argument, &action.logical_id, &graph.roots)?;
            }
            if let Some(environment) = &command.environment {
                validate_environment(environment, &action.logical_id, &graph.roots)?;
                if action
                    .execution_properties
                    .contains_key(MANDATORY_EXECUTION_PROPERTY)
                {
                    validate_standard_descriptor_environment(environment, &action.logical_id)?;
                }
            }
            if let Some(working_directory) = &command.working_directory {
                validate_path(working_directory, &graph.roots)?;
            }
        }
        let mut input_paths = BTreeSet::new();
        for input in &action.normal_inputs {
            validate_path(&input.path, &graph.roots)?;
            if !input_paths.insert(&input.path) {
                anyhow::bail!("action {:?} repeats a normal input", action.logical_id);
            }
        }
        let mut order_only_paths = BTreeSet::new();
        for input in &action.order_only_inputs {
            validate_path(input, &graph.roots)?;
            if input_paths.contains(input) || !order_only_paths.insert(input) {
                anyhow::bail!(
                    "action {:?} repeats or overlaps an order-only input",
                    action.logical_id
                );
            }
        }
        let mut local_outputs = BTreeSet::new();
        for output in &action.outputs {
            validate_path(&output.path, &graph.roots)?;
            if output.mode & !0o7777 != 0 {
                anyhow::bail!(
                    "action {:?} has an invalid output permission mode",
                    action.logical_id
                );
            }
            if output.path.root != "obj" {
                anyhow::bail!("action {:?} has an output outside obj", action.logical_id);
            }
            if !local_outputs.insert(&output.path) || !output_paths.insert(&output.path) {
                anyhow::bail!(
                    "duplicate external output path in action {:?}",
                    action.logical_id
                );
            }
            output_producers.insert(&output.path, index);
        }
        let mut config_deps = action.config_deps.clone();
        config_deps.sort();
        if config_deps.windows(2).any(|pair| pair[0] == pair[1]) {
            anyhow::bail!(
                "duplicate Kconfig dependency in action {:?}",
                action.logical_id
            );
        }
        if action.config_deps.iter().any(|symbol| {
            !symbol.starts_with("CONFIG_")
                || symbol
                    .bytes()
                    .any(|byte| !(byte.is_ascii_alphanumeric() || byte == b'_'))
        }) {
            anyhow::bail!(
                "action {:?} has an invalid Kconfig symbol",
                action.logical_id
            );
        }
        if !action.config_deps.is_empty() && graph.kconfig.is_none() {
            anyhow::bail!(
                "action {:?} has config_deps without a Kconfig source",
                action.logical_id
            );
        }
        if let Some(depfile) = &action.depfile {
            validate_path(&depfile.path, &graph.roots)?;
            if depfile.format.is_empty() {
                anyhow::bail!("action {:?} has an empty depfile format", action.logical_id);
            }
            if !local_outputs.iter().any(|output| *output == &depfile.path) {
                anyhow::bail!(
                    "action {:?} has a depfile that is not a declared output",
                    action.logical_id
                );
            }
        }
    }
    for action in &graph.actions {
        for checkpoint in runtime_directory_checkpoints(&action.execution_properties)? {
            for ordered in checkpoint
                .before_actions
                .iter()
                .chain(&checkpoint.after_actions)
            {
                if ordered == &action.logical_id || !action_index.contains_key(ordered.as_str()) {
                    anyhow::bail!(
                        "action {:?} has a runtime directory checkpoint with an invalid ordering action {:?}",
                        action.logical_id,
                        ordered,
                    );
                }
            }
        }
    }
    if let Some(kconfig) = &graph.kconfig {
        validate_path(&kconfig.path, &graph.roots)?;
        if kconfig.path.root != "obj" {
            anyhow::bail!("external Kconfig source is outside obj");
        }
    }
    validate_initial_inputs(graph, &output_producers)?;
    validate_empty_output_inputs(graph, &output_paths)?;
    for (index, action) in graph.actions.iter().enumerate() {
        let declared = action.dependencies.iter().collect::<BTreeSet<_>>();
        for input in action
            .normal_inputs
            .iter()
            .map(|input| &input.path)
            .chain(action.order_only_inputs.iter())
        {
            let Some(&producer) = output_producers.get(input) else {
                continue;
            };
            if producer == index {
                anyhow::bail!(
                    "action {:?} consumes its own declared output",
                    action.logical_id
                );
            }
            let producer_id = &graph.actions[producer].logical_id;
            if !declared.contains(producer_id) {
                anyhow::bail!(
                    "action {:?} consumes output of {:?} without a dependency",
                    action.logical_id,
                    producer_id
                );
            }
        }
    }
    for output in &graph.requested_outputs {
        validate_path(output, &graph.roots)?;
        if output.root != "obj" {
            anyhow::bail!("requested external output is outside obj");
        }
        if !output_paths.contains(output) {
            anyhow::bail!("requested external output has no producer");
        }
    }
    validate_analysis_barriers(graph, &action_index)?;

    let mut indegree = vec![0usize; graph.actions.len()];
    let mut dependents = vec![Vec::new(); graph.actions.len()];
    for (index, action) in graph.actions.iter().enumerate() {
        let mut seen = BTreeSet::new();
        for dependency in &action.dependencies {
            let Some(&producer) = action_index.get(dependency.as_str()) else {
                anyhow::bail!(
                    "action {:?} references unknown dependency {:?}",
                    action.logical_id,
                    dependency
                );
            };
            if producer == index {
                anyhow::bail!("action {:?} depends on itself", action.logical_id);
            }
            if seen.insert(producer) {
                indegree[index] += 1;
                dependents[producer].push(index);
            }
        }
    }
    let mut ready = VecDeque::new();
    for (index, count) in indegree.iter().enumerate() {
        if *count == 0 {
            ready.push_back(index);
        }
    }
    let mut visited = 0usize;
    while let Some(index) = ready.pop_front() {
        visited += 1;
        for &dependent in &dependents[index] {
            indegree[dependent] -= 1;
            if indegree[dependent] == 0 {
                ready.push_back(dependent);
            }
        }
    }
    if visited != graph.actions.len() {
        let mut cursor = indegree
            .iter()
            .position(|count| *count != 0)
            .context("cyclic external action graph has no retained action")?;
        let mut positions = HashMap::new();
        let mut chain = Vec::new();
        let cycle_start = loop {
            if let Some(start) = positions.insert(cursor, chain.len()) {
                break start;
            }
            chain.push(cursor);
            cursor = graph.actions[cursor]
                .dependencies
                .iter()
                .filter_map(|dependency| action_index.get(dependency.as_str()).copied())
                .find(|dependency| indegree[*dependency] != 0)
                .context("cyclic external action has no retained dependency")?;
        };
        let cycle = &chain[cycle_start..];
        let mut witness = cycle
            .iter()
            .take(16)
            .map(|index| format!("{index}:{:?}", graph.actions[*index].logical_id))
            .collect::<Vec<_>>();
        if cycle.len() <= 16 {
            let index = cycle[0];
            witness.push(format!("{index}:{:?}", graph.actions[index].logical_id));
        } else {
            witness.push("...".to_string());
        }
        anyhow::bail!(
            "external action graph contains a dependency cycle: {}",
            witness.join(" -> ")
        );
    }
    Ok(())
}

fn validate_analysis_barriers(
    graph: &ExternalActionGraph,
    action_index: &HashMap<&str, usize>,
) -> Result<()> {
    let all_actions = action_index.keys().copied().collect::<BTreeSet<_>>();
    let mut previous_producers = BTreeSet::new();
    for barrier in &graph.analysis_barriers {
        let producers = barrier
            .producer_closure
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        let blocked = barrier
            .blocked_actions
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if producers.is_empty()
            || blocked.is_empty()
            || barrier.expected_inputs.is_empty()
            || producers.len() != barrier.producer_closure.len()
            || blocked.len() != barrier.blocked_actions.len()
            || barrier
                .producer_closure
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
            || barrier
                .blocked_actions
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
            || barrier
                .expected_inputs
                .windows(2)
                .any(|pair| pair[0].path >= pair[1].path)
            || !producers.is_disjoint(&blocked)
            || producers.union(&blocked).copied().collect::<BTreeSet<_>>() != all_actions
        {
            anyhow::bail!("external analysis barrier has an incomplete action partition");
        }
        if !previous_producers.is_subset(&producers) || previous_producers == producers {
            anyhow::bail!("external analysis barrier producer closures are not strictly nested");
        }
        for logical_id in &producers {
            let Some(&index) = action_index.get(logical_id) else {
                anyhow::bail!("external analysis barrier references an unknown producer action");
            };
            if graph.actions[index]
                .dependencies
                .iter()
                .any(|dependency| !producers.contains(dependency.as_str()))
            {
                anyhow::bail!("external analysis barrier producer set is not dependency closed");
            }
        }

        let mut seen_inputs = BTreeSet::new();
        for input in &barrier.expected_inputs {
            validate_path(&input.path, &graph.roots)?;
            if input.path.root != "obj" || !seen_inputs.insert(&input.path) {
                anyhow::bail!("external analysis barrier input is not a unique obj path");
            }
            let supported = match &input.value {
                ExternalInputValue::File { .. } => barrier_has_direct_output(
                    graph,
                    action_index,
                    &producers,
                    &input.path,
                    ExternalOutputKind::File,
                ),
                ExternalInputValue::Symlink { target } if !target.contains(&0) => {
                    barrier_has_direct_output(
                        graph,
                        action_index,
                        &producers,
                        &input.path,
                        ExternalOutputKind::Symlink,
                    )
                }
                ExternalInputValue::Tree { .. } => barrier_has_direct_output(
                    graph,
                    action_index,
                    &producers,
                    &input.path,
                    ExternalOutputKind::Tree,
                ),
                ExternalInputValue::DirectoryListing { .. }
                | ExternalInputValue::Directory { mode: 0..=0o7777 } => {
                    producers.iter().any(|logical_id| {
                        graph.actions[action_index[logical_id]]
                            .outputs
                            .iter()
                            .any(|output| {
                                path_is_below(&output.path, &input.path)
                                    || (output.path == input.path
                                        && output.kind == ExternalOutputKind::Tree)
                            })
                    })
                }
                ExternalInputValue::Missing
                | ExternalInputValue::Virtual { .. }
                | ExternalInputValue::Directory { .. } => false,
                ExternalInputValue::Symlink { .. } => false,
            };
            if !supported {
                anyhow::bail!(
                    "external analysis barrier input has no typed producer in its closure"
                );
            }
        }
        previous_producers = producers;
    }
    Ok(())
}

fn barrier_has_direct_output(
    graph: &ExternalActionGraph,
    action_index: &HashMap<&str, usize>,
    producers: &BTreeSet<&str>,
    path: &ExternalPathKey,
    kind: ExternalOutputKind,
) -> bool {
    producers.iter().any(|logical_id| {
        graph.actions[action_index[logical_id]]
            .outputs
            .iter()
            .any(|output| output.path == *path && output.kind == kind)
    })
}

fn path_is_below(path: &ExternalPathKey, directory: &ExternalPathKey) -> bool {
    if path.root != directory.root || path.path.len() <= directory.path.len() {
        return false;
    }
    if directory.path == b"." {
        return true;
    }
    path.path.starts_with(&directory.path)
        && path.path.get(directory.path.len()).copied() == Some(b'/')
}

fn validate_initial_inputs(
    graph: &ExternalActionGraph,
    _output_producers: &BTreeMap<&ExternalPathKey, usize>,
) -> Result<()> {
    let mut entries = BTreeMap::new();
    for input in &graph.initial_inputs {
        validate_path(&input.path, &graph.roots)?;
        if input.path.root != "obj" || input.path.path == b"." {
            anyhow::bail!("external initial input is not below obj");
        }
        if entries.insert(&input.path, &input.value).is_some() {
            anyhow::bail!("external initial input path is repeated");
        }
        match &input.value {
            ExternalInitialInputValue::File { mode, .. }
            | ExternalInitialInputValue::Directory { mode }
                if mode & !0o7777 != 0 =>
            {
                anyhow::bail!("external initial input has an invalid permission mode");
            }
            ExternalInitialInputValue::Symlink { target } if target.contains(&0) => {
                anyhow::bail!("external initial symlink target contains NUL");
            }
            _ => {}
        }
        // A controller-owned initial value may intentionally be replaced by a
        // later action. Replay materializes it before any action is scheduled.
    }

    for input in &graph.initial_inputs {
        let mut parent = parent_path(&input.path.path);
        while let Some(path) = parent {
            let parent_key = ExternalPathKey {
                root: "obj".to_string(),
                path: path.to_vec(),
            };
            match entries.get(&parent_key) {
                Some(ExternalInitialInputValue::Directory { .. }) => {}
                Some(_) => anyhow::bail!("external initial input has a non-directory ancestor"),
                None => anyhow::bail!(
                    "external initial input {:?} omits an ancestor directory {:?}",
                    input.path,
                    parent_key
                ),
            }
            parent = parent_path(path);
        }
    }
    Ok(())
}

fn validate_empty_output_inputs(
    graph: &ExternalActionGraph,
    output_paths: &BTreeSet<&ExternalPathKey>,
) -> Result<()> {
    let mut seen = BTreeSet::new();
    for captured in &graph.empty_output_inputs {
        validate_path(&captured.path, &graph.roots)?;
        if captured.path.root != "obj" || !seen.insert(&captured.path) {
            anyhow::bail!("empty-output input must be a unique obj path");
        }
        if captured.kind != ExternalInputKind::DirectoryListing
            || !matches!(captured.value, ExternalInputValue::DirectoryListing { .. })
        {
            anyhow::bail!("empty-output input must capture a directory listing");
        }
        if output_paths.iter().any(|output| *output == &captured.path) {
            anyhow::bail!("empty-output input is also a declared output");
        }
        let consumed = graph.actions.iter().any(|action| {
            action
                .normal_inputs
                .iter()
                .any(|input| input.path == captured.path && input.kind == captured.kind)
        });
        if !consumed {
            anyhow::bail!("empty-output input has no matching action input");
        }
    }
    Ok(())
}

fn parent_path(path: &[u8]) -> Option<&[u8]> {
    let separator = path.iter().rposition(|byte| *byte == b'/')?;
    Some(&path[..separator])
}

fn validate_path(path: &ExternalPathKey, roots: &BTreeMap<String, PathBuf>) -> Result<()> {
    if !roots.contains_key(&path.root) {
        anyhow::bail!("external path references unknown root {:?}", path.root);
    }
    if path.path.is_empty() || path.path[0] == b'/' || path.path.contains(&0) {
        anyhow::bail!(
            "external path under {:?} is not a relative byte path: {:?}",
            path.root,
            path.path
        );
    }
    if path
        .path
        .split(|byte| *byte == b'/')
        .any(|part| part == b"..")
    {
        anyhow::bail!("external path under {:?} escapes its root", path.root);
    }
    Ok(())
}

fn validate_environment(
    environment: &BTreeMap<Vec<u8>, ExternalArgumentKey>,
    logical_id: &str,
    roots: &BTreeMap<String, PathBuf>,
) -> Result<()> {
    for (name, value) in environment {
        if name.is_empty() || name.contains(&0) || name.contains(&b'=') {
            anyhow::bail!("action {logical_id:?} has an invalid explicit environment");
        }
        validate_argument(value, logical_id, roots)?;
    }
    Ok(())
}

fn validate_standard_descriptor_environment(
    environment: &BTreeMap<Vec<u8>, ExternalArgumentKey>,
    logical_id: &str,
) -> Result<()> {
    for name in GNU_MAKE_JOBSERVER_ENVIRONMENTS {
        if let Some(argument) = environment.get(name) {
            validate_standard_descriptor_make_flags(argument).with_context(|| {
                format!(
                    "action {logical_id:?} has unsupported {} under standard-only mandatory execution",
                    String::from_utf8_lossy(name)
                )
            })?;
        }
    }
    Ok(())
}

fn validate_standard_descriptor_make_flags(argument: &ExternalArgumentKey) -> Result<()> {
    let value = match argument {
        ExternalArgumentKey::Literal(value) => value.clone(),
        ExternalArgumentKey::Path(_) => {
            anyhow::bail!("path-bearing GNU Make flags cannot prove descriptor topology")
        }
        ExternalArgumentKey::Joined(parts) => {
            let mut value = Vec::new();
            for part in parts {
                match part {
                    ExternalArgumentPart::Literal(bytes) => value.extend(bytes),
                    ExternalArgumentPart::Path(_) => {
                        anyhow::bail!(
                            "path-bearing GNU Make flags cannot prove descriptor topology"
                        )
                    }
                }
            }
            value
        }
    };

    let mut canonical_auth_count = 0usize;
    for token in value.split(|byte| byte.is_ascii_whitespace()) {
        if let Some(auth) = token.strip_prefix(GNU_MAKE_JOBSERVER_AUTH_OPTION) {
            if auth != CANONICAL_CLOSED_GNU_MAKE_JOBSERVER {
                anyhow::bail!(
                    "GNU Make jobserver authentication is not the canonical closed 3,4 pair"
                );
            }
            canonical_auth_count += 1;
        } else if token.starts_with(GNU_MAKE_JOBSERVER_FDS_OPTION) {
            anyhow::bail!("legacy GNU Make jobserver descriptors are unsupported");
        }
    }

    if canonical_auth_count > 1 {
        anyhow::bail!("GNU Make jobserver authentication is duplicated");
    }
    Ok(())
}

fn validate_argument(
    argument: &ExternalArgumentKey,
    logical_id: &str,
    roots: &BTreeMap<String, PathBuf>,
) -> Result<()> {
    match argument {
        ExternalArgumentKey::Literal(value) if value.contains(&0) => {
            anyhow::bail!("action {logical_id:?} has an argument containing NUL");
        }
        ExternalArgumentKey::Path(path) => validate_path(path, roots)?,
        ExternalArgumentKey::Joined(parts) if parts.is_empty() => {
            anyhow::bail!("action {logical_id:?} has an empty joined argument");
        }
        ExternalArgumentKey::Joined(parts) => {
            for part in parts {
                match part {
                    ExternalArgumentPart::Literal(value) if value.contains(&0) => {
                        anyhow::bail!("action {logical_id:?} has an argument containing NUL");
                    }
                    ExternalArgumentPart::Path(path) => validate_path(path, roots)?,
                    ExternalArgumentPart::Literal(_) => {}
                }
            }
        }
        ExternalArgumentKey::Literal(_) => {}
    }
    Ok(())
}

fn analyzer_digest(analyzer: &ExternalAnalyzerIdentity) -> [u8; 32] {
    let bytes = bincode::serialize(analyzer)
        .expect("ExternalAnalyzerIdentity contains only serializable data");
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rush.external-analyzer-identity.v1\n");
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(&bytes);
    *hasher.finalize().as_bytes()
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use rush_incremental::{content_digest_of_regular_file, Dependency};
    use rush_protocol::command::v1::{EnvironmentVariable, RunRequest};
    use rush_protocol::{PROTOCOL_MAJOR, PROTOCOL_MINOR};

    fn rooted(root: &str, path: &str) -> ExternalPathKey {
        ExternalPathKey {
            root: root.to_string(),
            path: path.as_bytes().to_vec(),
        }
    }

    fn semantics(cwd: &Path) -> CommandSemantics {
        CommandSemantics::from_run_request(&RunRequest {
            auth_cookie: Bytes::new(),
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: PROTOCOL_MINOR,
            argv: vec![Bytes::from_static(b"rush"), Bytes::from_static(b"build")],
            working_directory: Bytes::copy_from_slice(cwd.as_os_str().as_encoded_bytes()),
            environment: vec![EnvironmentVariable {
                name: Bytes::from_static(b"LANG"),
                value: Bytes::from_static(b"C"),
            }],
            block_for_lock: true,
            client_build_id: "external-snapshot-test".to_string(),
            workspace_config: Vec::new(),
        })
        .unwrap()
    }

    fn snapshot(temp: &Path, analysis_input: &Path) -> ExternalAnalysisSnapshot {
        let analyzer = ExternalAnalyzerIdentity {
            kind: "test-analyzer".to_string(),
            executable_digest: [8; 32],
            argv: vec![b"analyzer".to_vec(), b"build".to_vec()],
            working_directory: temp.to_path_buf(),
            environment: BTreeMap::from([(b"LANG".to_vec(), b"C".to_vec())]),
            build_options: BTreeMap::new(),
            platform: BTreeMap::from([("os".to_string(), "linux".to_string())]),
        };
        let src = temp.join("src");
        let obj = temp.join("obj");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&obj).unwrap();
        let key =
            ExternalAnalysisKey::new(src.clone(), obj.clone(), &analyzer, vec![b"all".to_vec()]);
        let manifest = DependencyManifest::from_deps(vec![Dependency::FileContent {
            path: analysis_input.to_path_buf(),
            digest: content_digest_of_regular_file(analysis_input, 1 << 20).unwrap(),
        }]);
        ExternalAnalysisSnapshot::new(
            key,
            analyzer,
            manifest,
            ExternalActionGraph {
                roots: BTreeMap::from([("src".to_string(), src), ("obj".to_string(), obj)]),
                kconfig: None,
                initial_inputs: Vec::new(),
                empty_output_inputs: Vec::new(),
                actions: vec![ExternalAction {
                    logical_id: "compile".to_string(),
                    mnemonic: "Compile".to_string(),
                    commands: vec![ExternalCommand {
                        executable: rooted("src", "tool"),
                        argv0: ExternalArgumentKey::Path(rooted("src", "tool")),
                        argv: vec![
                            ExternalArgumentKey::Literal(b"-c".to_vec()),
                            ExternalArgumentKey::Path(rooted("src", "input.c")),
                        ],
                        environment: None,
                        working_directory: None,
                    }],
                    working_directory: rooted("obj", "."),
                    environment: BTreeMap::from([(
                        b"LANG".to_vec(),
                        ExternalArgumentKey::Literal(b"C".to_vec()),
                    )]),
                    signal_state: ExternalSignalState::default(),
                    file_creation_mask: 0o022,
                    credentials: ExternalCredentialState::default(),
                    normal_inputs: vec![ExternalInput {
                        path: rooted("src", "input.c"),
                        kind: ExternalInputKind::File,
                    }],
                    order_only_inputs: Vec::new(),
                    outputs: vec![ExternalOutput {
                        path: rooted("obj", "input.o"),
                        kind: ExternalOutputKind::File,
                        mode: 0o644,
                    }],
                    dependencies: Vec::new(),
                    config_deps: Vec::new(),
                    depfile: None,
                    restat: true,
                    always_run: false,
                    cacheable: true,
                    local_only: false,
                    sandbox: true,
                    resource: ManifestResourceClass::Compile,
                    execution_properties: BTreeMap::new(),
                }],
                analysis_barriers: Vec::new(),
                requested_outputs: vec![rooted("obj", "input.o")],
            },
        )
        .unwrap()
    }

    #[test]
    fn persisted_snapshot_round_trips_and_admits_without_analyzer() {
        let temp = tempfile::tempdir().unwrap();
        let analysis_input = temp.path().join("Makefile");
        std::fs::write(&analysis_input, b"all:\n").unwrap();
        let snapshot = snapshot(temp.path(), &analysis_input);
        let destination = temp.path().join("snapshot.bin");
        persist(&destination, &snapshot).unwrap();

        match load_if_fresh(&destination, &snapshot.key, &semantics(temp.path())) {
            ExternalSnapshotAdmission::Hit(loaded) => assert_eq!(loaded, snapshot),
            admission => panic!("unexpected admission: {}", admission.decision().as_str()),
        }
    }

    #[test]
    fn persisted_mandatory_execution_policy_round_trips_and_admits() {
        let temp = tempfile::tempdir().unwrap();
        let analysis_input = temp.path().join("Makefile");
        std::fs::write(&analysis_input, b"all:\n").unwrap();
        let mut snapshot = snapshot(temp.path(), &analysis_input);
        let action = &mut snapshot.graph.actions[0];
        action.cacheable = false;
        action.local_only = true;
        action.execution_properties.insert(
            MANDATORY_EXECUTION_PROPERTY.to_string(),
            PROCFS_CLEAN_ENTRY_MANDATORY_EXECUTION.to_string(),
        );
        let checkpoint_path = rooted("obj", "generated");
        let checkpoint_members = vec![b"first".to_vec(), b"second".to_vec()];
        let (name, value) =
            encode_runtime_directory_checkpoint(0, &checkpoint_path, &checkpoint_members).unwrap();
        action.execution_properties.insert(name, value);
        assert_eq!(
            runtime_directory_checkpoints(&action.execution_properties).unwrap(),
            vec![ExternalRuntimeDirectoryCheckpoint {
                path: checkpoint_path,
                members: checkpoint_members,
                observation_ordinal: 0,
                before_actions: Vec::new(),
                after_actions: Vec::new(),
            }]
        );
        snapshot.validate_structure().unwrap();

        let destination = temp.path().join("snapshot.bin");
        persist(&destination, &snapshot).unwrap();
        match load_if_fresh(&destination, &snapshot.key, &semantics(temp.path())) {
            ExternalSnapshotAdmission::Hit(loaded) => assert_eq!(loaded, snapshot),
            admission => panic!("unexpected admission: {}", admission.decision().as_str()),
        }
    }

    #[test]
    fn mandatory_execution_validates_gnu_make_jobserver_descriptors() {
        let temp = tempfile::tempdir().unwrap();
        let analysis_input = temp.path().join("Makefile");
        std::fs::write(&analysis_input, b"all:\n").unwrap();

        for (command_override, name, makeflags) in [
            (
                false,
                b"MAKEFLAGS".as_slice(),
                ExternalArgumentKey::Literal(
                    b"rRs -j96 --jobserver-auth=4,5 --no-print-directory".to_vec(),
                ),
            ),
            (
                true,
                b"MAKEFLAGS".as_slice(),
                ExternalArgumentKey::Joined(vec![
                    ExternalArgumentPart::Literal(b"rRs --jobserver-".to_vec()),
                    ExternalArgumentPart::Literal(b"fds=4,5".to_vec()),
                ]),
            ),
            (
                false,
                b"MFLAGS".as_slice(),
                ExternalArgumentKey::Literal(b"-j96 --jobserver-auth=3,5".to_vec()),
            ),
            (
                true,
                b"MAKEFLAGS".as_slice(),
                ExternalArgumentKey::Literal(
                    b"-j96 --jobserver-auth=3,4 --jobserver-auth=3,4".to_vec(),
                ),
            ),
        ] {
            let mut snapshot = snapshot(temp.path(), &analysis_input);
            let action = &mut snapshot.graph.actions[0];
            action.cacheable = false;
            action.local_only = true;
            action.execution_properties.insert(
                MANDATORY_EXECUTION_PROPERTY.to_string(),
                PROCFS_CLEAN_ENTRY_MANDATORY_EXECUTION.to_string(),
            );
            if command_override {
                action.commands[0].environment =
                    Some(BTreeMap::from([(name.to_vec(), makeflags)]));
            } else {
                action.environment.insert(name.to_vec(), makeflags);
            }

            let error = snapshot.validate_structure().unwrap_err().to_string();
            assert!(
                error.contains("unsupported")
                    && error.contains("standard-only mandatory execution"),
                "unexpected validation error: {error}"
            );
        }

        let mut snapshot = snapshot(temp.path(), &analysis_input);
        let action = &mut snapshot.graph.actions[0];
        action.cacheable = false;
        action.local_only = true;
        action.execution_properties.insert(
            MANDATORY_EXECUTION_PROPERTY.to_string(),
            PROCFS_CLEAN_ENTRY_MANDATORY_EXECUTION.to_string(),
        );
        action.environment.insert(
            b"MAKEFLAGS".to_vec(),
            ExternalArgumentKey::Joined(vec![
                ExternalArgumentPart::Literal(b"rRs -j96 --jobserver-".to_vec()),
                ExternalArgumentPart::Literal(b"auth=3,4 --no-print-directory".to_vec()),
            ]),
        );
        action.environment.insert(
            b"MFLAGS".to_vec(),
            ExternalArgumentKey::Literal(b"-j96 --jobserver-auth=3,4".to_vec()),
        );
        snapshot.validate_structure().unwrap();
    }

    #[test]
    fn changed_analysis_input_rejects_the_whole_graph() {
        let temp = tempfile::tempdir().unwrap();
        let analysis_input = temp.path().join("Makefile");
        std::fs::write(&analysis_input, b"all:\n").unwrap();
        let snapshot = snapshot(temp.path(), &analysis_input);
        let destination = temp.path().join("snapshot.bin");
        persist(&destination, &snapshot).unwrap();
        std::fs::write(&analysis_input, b"all: changed\n").unwrap();

        match load_if_fresh(&destination, &snapshot.key, &semantics(temp.path())) {
            ExternalSnapshotAdmission::Stale(reason) => {
                assert!(reason.contains("content changed"));
            }
            admission => panic!("unexpected admission: {}", admission.decision().as_str()),
        }
    }

    #[test]
    fn wrong_key_and_corruption_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let analysis_input = temp.path().join("Makefile");
        std::fs::write(&analysis_input, b"all:\n").unwrap();
        let snapshot = snapshot(temp.path(), &analysis_input);
        let destination = temp.path().join("snapshot.bin");
        persist(&destination, &snapshot).unwrap();

        let mut wrong_key = snapshot.key.clone();
        wrong_key.requested_goals = vec![b"other".to_vec()];
        assert!(matches!(
            load_if_fresh(&destination, &wrong_key, &semantics(temp.path())),
            ExternalSnapshotAdmission::Missing(_)
        ));

        let mut old_version = snapshot.clone();
        old_version.version = 1;
        std::fs::write(&destination, bincode::serialize(&old_version).unwrap()).unwrap();
        match load_if_fresh(&destination, &snapshot.key, &semantics(temp.path())) {
            ExternalSnapshotAdmission::Invalid(reason) => {
                assert!(reason.contains("unsupported external analysis version 1"));
            }
            admission => panic!("unexpected admission: {}", admission.decision().as_str()),
        }

        std::fs::write(&destination, b"not a snapshot").unwrap();
        assert!(matches!(
            load_if_fresh(&destination, &snapshot.key, &semantics(temp.path())),
            ExternalSnapshotAdmission::Invalid(_)
        ));
    }

    #[test]
    fn malformed_argv_zero_fails_closed() {
        let temp = tempfile::tempdir().unwrap();
        let analysis_input = temp.path().join("Makefile");
        std::fs::write(&analysis_input, b"all:\n").unwrap();
        let mut snapshot = snapshot(temp.path(), &analysis_input);
        snapshot.graph.actions[0].commands[0].argv0 =
            ExternalArgumentKey::Literal(b"bad\0argv-zero".to_vec());

        let error = snapshot.validate_structure().unwrap_err().to_string();
        assert!(error.contains("argument containing NUL"), "{error}");
    }

    #[test]
    fn missing_continuation_and_unsupported_are_distinct_decisions() {
        let temp = tempfile::tempdir().unwrap();
        let analysis_input = temp.path().join("Makefile");
        std::fs::write(&analysis_input, b"all:\n").unwrap();
        let mut snapshot = snapshot(temp.path(), &analysis_input);

        let missing = load_if_fresh(
            &temp.path().join("absent.bin"),
            &snapshot.key,
            &semantics(temp.path()),
        );
        assert_eq!(
            missing.decision(),
            ExternalSnapshotDecision::SnapshotMissing
        );

        let mut consumer = snapshot.graph.actions[0].clone();
        consumer.logical_id = "consume".to_string();
        consumer.normal_inputs = vec![ExternalInput {
            path: rooted("obj", "input.o"),
            kind: ExternalInputKind::File,
        }];
        consumer.outputs = vec![ExternalOutput {
            path: rooted("obj", "final"),
            kind: ExternalOutputKind::File,
            mode: 0o644,
        }];
        consumer.dependencies = vec!["compile".to_string()];
        snapshot.graph.actions.push(consumer);
        snapshot.graph.requested_outputs = vec![rooted("obj", "final")];
        snapshot
            .graph
            .analysis_barriers
            .push(ExternalAnalysisBarrier {
                producer_closure: vec!["compile".to_string()],
                expected_inputs: vec![ExternalInputKey {
                    path: rooted("obj", "input.o"),
                    value: ExternalInputValue::File {
                        digest: [7; 32],
                        executable: false,
                    },
                }],
                blocked_actions: vec!["consume".to_string()],
            });
        snapshot.validate_structure().unwrap();
        let destination = temp.path().join("snapshot.bin");
        persist(&destination, &snapshot).unwrap();
        let continuation = load_if_fresh(&destination, &snapshot.key, &semantics(temp.path()));
        assert!(matches!(
            &continuation,
            ExternalSnapshotAdmission::ContinuationRequired(_)
        ));
        assert_eq!(
            continuation.decision(),
            ExternalSnapshotDecision::SnapshotContinuationRequired
        );

        let unsupported = ExternalSnapshotAdmission::unsupported("signed build policy");
        assert_eq!(
            unsupported.decision(),
            ExternalSnapshotDecision::SnapshotUnsupported
        );
        assert_eq!(
            serde_json::to_string(&unsupported.decision()).unwrap(),
            "\"SNAPSHOT_UNSUPPORTED\""
        );
    }

    #[test]
    fn cycles_unknown_roots_and_config_without_source_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let analysis_input = temp.path().join("Makefile");
        std::fs::write(&analysis_input, b"all:\n").unwrap();

        let mut cycle = snapshot(temp.path(), &analysis_input);
        cycle.graph.actions[0].dependencies = vec!["compile".to_string()];
        assert!(cycle
            .validate_structure()
            .unwrap_err()
            .to_string()
            .contains("itself"));

        let mut unknown_root = snapshot(temp.path(), &analysis_input);
        unknown_root.graph.actions[0].commands[0].executable = rooted("missing", "cc");
        assert!(unknown_root
            .validate_structure()
            .unwrap_err()
            .to_string()
            .contains("unknown root"));

        let mut unknown_command_root = snapshot(temp.path(), &analysis_input);
        unknown_command_root.graph.actions[0].commands[0].working_directory =
            Some(rooted("missing", "."));
        assert!(unknown_command_root
            .validate_structure()
            .unwrap_err()
            .to_string()
            .contains("unknown root"));

        let mut config = snapshot(temp.path(), &analysis_input);
        config.graph.actions[0].config_deps = vec!["CONFIG_TEST".to_string()];
        assert!(config
            .validate_structure()
            .unwrap_err()
            .to_string()
            .contains("without a Kconfig source"));
    }

    #[test]
    fn directory_listing_barrier_accepts_direct_tree_producer() {
        let temp = tempfile::tempdir().unwrap();
        let analysis_input = temp.path().join("Makefile");
        std::fs::write(&analysis_input, b"all:\n").unwrap();
        let mut snapshot = snapshot(temp.path(), &analysis_input);
        let mut consumer = snapshot.graph.actions[0].clone();
        snapshot.graph.actions[0].outputs = vec![ExternalOutput {
            path: rooted("obj", "generated"),
            kind: ExternalOutputKind::Tree,
            mode: 0o755,
        }];
        consumer.logical_id = "consume".to_string();
        consumer.outputs = vec![ExternalOutput {
            path: rooted("obj", "final"),
            kind: ExternalOutputKind::File,
            mode: 0o644,
        }];
        consumer.dependencies = vec!["compile".to_string()];
        snapshot.graph.actions.push(consumer);
        snapshot.graph.requested_outputs = vec![rooted("obj", "final")];
        snapshot
            .graph
            .analysis_barriers
            .push(ExternalAnalysisBarrier {
                producer_closure: vec!["compile".to_string()],
                expected_inputs: vec![ExternalInputKey {
                    path: rooted("obj", "generated"),
                    value: ExternalInputValue::DirectoryListing { digest: [7; 32] },
                }],
                blocked_actions: vec!["consume".to_string()],
            });

        snapshot.validate_structure().unwrap();
    }

    #[test]
    fn ambiguous_inputs_environment_and_barriers_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let analysis_input = temp.path().join("Makefile");
        std::fs::write(&analysis_input, b"all:\n").unwrap();

        let mut duplicate_input = snapshot(temp.path(), &analysis_input);
        let repeated_input = duplicate_input.graph.actions[0].normal_inputs[0].clone();
        duplicate_input.graph.actions[0]
            .normal_inputs
            .push(repeated_input);
        assert!(duplicate_input
            .validate_structure()
            .unwrap_err()
            .to_string()
            .contains("repeats a normal input"));

        let mut invalid_environment = snapshot(temp.path(), &analysis_input);
        invalid_environment.graph.actions[0].environment.insert(
            b"BAD=NAME".to_vec(),
            ExternalArgumentKey::Literal(b"value".to_vec()),
        );
        assert!(invalid_environment
            .validate_structure()
            .unwrap_err()
            .to_string()
            .contains("invalid explicit environment"));

        let mut invalid_command_environment = snapshot(temp.path(), &analysis_input);
        invalid_command_environment.graph.actions[0].commands[0].environment =
            Some(BTreeMap::from([(
                b"BAD=NAME".to_vec(),
                ExternalArgumentKey::Literal(b"value".to_vec()),
            )]));
        assert!(invalid_command_environment
            .validate_structure()
            .unwrap_err()
            .to_string()
            .contains("invalid explicit environment"));

        let mut invalid_output_mode = snapshot(temp.path(), &analysis_input);
        invalid_output_mode.graph.actions[0].outputs[0].mode = 0o10000;
        assert!(invalid_output_mode
            .validate_structure()
            .unwrap_err()
            .to_string()
            .contains("invalid output permission mode"));

        let mut invalid_signal_state = snapshot(temp.path(), &analysis_input);
        invalid_signal_state.graph.actions[0].signal_state.blocked = 1_u64 << (9 - 1);
        assert!(invalid_signal_state
            .validate_structure()
            .unwrap_err()
            .to_string()
            .contains("invalid signal state"));

        let mut invalid_file_creation_mask = snapshot(temp.path(), &analysis_input);
        invalid_file_creation_mask.graph.actions[0].file_creation_mask = 0o1000;
        assert!(invalid_file_creation_mask
            .validate_structure()
            .unwrap_err()
            .to_string()
            .contains("invalid file-creation mask"));

        let mut invalid_credentials = snapshot(temp.path(), &analysis_input);
        invalid_credentials.graph.actions[0]
            .credentials
            .supplementary_groups = vec![20, 10];
        assert!(invalid_credentials
            .validate_structure()
            .unwrap_err()
            .to_string()
            .contains("invalid credential state"));

        let mut invalid_barrier = snapshot(temp.path(), &analysis_input);
        let mut consumer = invalid_barrier.graph.actions[0].clone();
        consumer.logical_id = "consume".to_string();
        consumer.outputs[0].path = rooted("obj", "final");
        consumer.dependencies = vec!["compile".to_string()];
        invalid_barrier.graph.actions.push(consumer);
        invalid_barrier.graph.requested_outputs = vec![rooted("obj", "final")];
        invalid_barrier
            .graph
            .analysis_barriers
            .push(ExternalAnalysisBarrier {
                producer_closure: vec!["compile".to_string()],
                expected_inputs: vec![ExternalInputKey {
                    path: rooted("obj", "not-produced"),
                    value: ExternalInputValue::File {
                        digest: [7; 32],
                        executable: false,
                    },
                }],
                blocked_actions: vec!["consume".to_string()],
            });
        assert!(invalid_barrier
            .validate_structure()
            .unwrap_err()
            .to_string()
            .contains("no typed producer"));

        let mut unclosed_barrier = snapshot(temp.path(), &analysis_input);
        let mut consumer = unclosed_barrier.graph.actions[0].clone();
        consumer.logical_id = "consume".to_string();
        consumer.normal_inputs = vec![ExternalInput {
            path: rooted("obj", "input.o"),
            kind: ExternalInputKind::File,
        }];
        consumer.outputs[0].path = rooted("obj", "final");
        consumer.dependencies = vec!["compile".to_string()];
        unclosed_barrier.graph.actions.push(consumer);
        unclosed_barrier.graph.requested_outputs = vec![rooted("obj", "final")];
        unclosed_barrier
            .graph
            .analysis_barriers
            .push(ExternalAnalysisBarrier {
                producer_closure: vec!["consume".to_string()],
                expected_inputs: vec![ExternalInputKey {
                    path: rooted("obj", "final"),
                    value: ExternalInputValue::File {
                        digest: [7; 32],
                        executable: false,
                    },
                }],
                blocked_actions: vec!["compile".to_string()],
            });
        assert!(unclosed_barrier
            .validate_structure()
            .unwrap_err()
            .to_string()
            .contains("not dependency closed"));
    }

    #[test]
    fn generated_inputs_require_their_exact_producer_edge() {
        let temp = tempfile::tempdir().unwrap();
        let analysis_input = temp.path().join("Makefile");
        std::fs::write(&analysis_input, b"all:\n").unwrap();
        let mut snapshot = snapshot(temp.path(), &analysis_input);
        let mut consumer = snapshot.graph.actions[0].clone();
        consumer.logical_id = "consume".to_string();
        consumer.normal_inputs = vec![ExternalInput {
            path: rooted("obj", "input.o"),
            kind: ExternalInputKind::File,
        }];
        consumer.outputs = vec![ExternalOutput {
            path: rooted("obj", "final"),
            kind: ExternalOutputKind::File,
            mode: 0o644,
        }];
        consumer.dependencies.clear();
        snapshot.graph.actions.push(consumer);
        snapshot.graph.requested_outputs = vec![rooted("obj", "final")];

        assert!(snapshot
            .validate_structure()
            .unwrap_err()
            .to_string()
            .contains("without a dependency"));
        snapshot.graph.actions[1].dependencies = vec!["compile".to_string()];
        snapshot.validate_structure().unwrap();
    }

    #[test]
    fn dependency_cycle_reports_action_witness() {
        let temp = tempfile::tempdir().unwrap();
        let analysis_input = temp.path().join("Makefile");
        std::fs::write(&analysis_input, b"all:\n").unwrap();
        let mut snapshot = snapshot(temp.path(), &analysis_input);
        let mut link = snapshot.graph.actions[0].clone();
        link.logical_id = "link".to_string();
        link.normal_inputs.clear();
        link.outputs[0].path = rooted("obj", "final");
        link.dependencies = vec!["compile".to_string()];
        snapshot.graph.actions[0].dependencies = vec!["link".to_string()];
        snapshot.graph.actions.push(link);
        snapshot.graph.requested_outputs = vec![rooted("obj", "final")];

        let error = snapshot.validate_structure().unwrap_err().to_string();
        assert!(error.contains("dependency cycle"));
        assert!(error.contains("compile"));
        assert!(error.contains("link"));
    }

    #[test]
    fn initial_state_requires_owned_shape_and_matching_observation() {
        let temp = tempfile::tempdir().unwrap();
        let analysis_input = temp.path().join("Makefile");
        std::fs::write(&analysis_input, b"all:\n").unwrap();

        let mut missing_parent = snapshot(temp.path(), &analysis_input);
        missing_parent.graph.initial_inputs = vec![ExternalInitialInput {
            path: rooted("obj", "generated/state"),
            value: ExternalInitialInputValue::File {
                bytes: b"state\n".to_vec(),
                mode: 0o644,
            },
        }];
        assert!(missing_parent
            .validate_structure()
            .unwrap_err()
            .to_string()
            .contains("omits an ancestor"));

        let mut output_overlap = snapshot(temp.path(), &analysis_input);
        output_overlap.graph.initial_inputs = vec![ExternalInitialInput {
            path: rooted("obj", "input.o"),
            value: ExternalInitialInputValue::File {
                bytes: Vec::new(),
                mode: 0o644,
            },
        }];
        // Controller-owned seeds may be replaced by the declared producer.
        // The seed shape/ownership checks still apply; overlap alone is valid.
        output_overlap.validate_structure().unwrap();

        let mut wrong_observation = snapshot(temp.path(), &analysis_input);
        wrong_observation.graph.actions[0]
            .normal_inputs
            .push(ExternalInput {
                path: rooted("obj", "."),
                kind: ExternalInputKind::DirectoryListing,
            });
        wrong_observation.graph.empty_output_inputs = vec![ExternalEmptyOutputInput {
            path: rooted("obj", "."),
            kind: ExternalInputKind::DirectoryListing,
            value: ExternalInputValue::Missing,
        }];
        assert!(wrong_observation
            .validate_structure()
            .unwrap_err()
            .to_string()
            .contains("must capture a directory listing"));
    }
}
