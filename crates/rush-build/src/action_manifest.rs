// Rush Build - External action-graph import and execution
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Strict import and direct execution of a complete Kbuild action graph.
//!
//! Versions 1 and 2 are deliberately execute-all clean-build protocols. They
//! do not claim depfile, cache, restat, or always-run semantics. The importer
//! validates the entire JSONL stream and DAG before it starts any process.
//!
//! The wrapper/exporter must establish a clean, prepared object tree before
//! handoff. Under the output-root lock, Rush snapshots and freezes that complete
//! tree for the execution. Its entries are ambient read-only inputs available
//! to every action. `preexisting_obj_inputs` identifies only producerless
//! object-tree paths explicitly named as graph inputs; it is not a whitelist of
//! the prepared snapshot. Imported commands are trusted executable input and
//! run without a per-action process sandbox.

use futures::stream::{FuturesUnordered, StreamExt};
use rush_action::action_dag::ActionDag;
use rush_action::action_manifest::{
    ActionManifestAction, ActionManifestActionV2, ActionManifestFinal, ActionManifestHeader,
    ActionManifestHeaderV2, ActionManifestUnsupported, ManifestCommand, ManifestEnvPolicy,
    ManifestOutputKind, ManifestPath, ManifestRecordKind, ManifestResourceClass,
    KBUILD_ACTION_MANIFEST_LATEST_VERSION, KBUILD_ACTION_MANIFEST_SCHEMA,
    KBUILD_ACTION_MANIFEST_VERSION, KBUILD_ACTION_MANIFEST_VERSION_2,
};
use rush_action::ids::{
    ActionDeclarationStatus, ActionEnvPolicy, ArtifactId, ArtifactKey, ArtifactKind,
    ArtifactRegistry, ConfigId, DeclaredAction, DeclaredActionId, MaterializedCommand, TargetId,
};
use rush_exec::local_action_runner::{
    DefaultLocalActionRunner, ExecutionRequest, LocalActionRunner, PermitAcquiringRunner,
};
use serde::{Deserialize, Serialize};
use std::collections::{hash_map::RandomState, BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs;
use std::hash::BuildHasher;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use walkdir::WalkDir;

const ERROR_SCHEMA: &str = "rush.action_manifest.error";
const ERROR_VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_RECORD_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ACTIONS: usize = 5_000_000;
const MAX_COMMANDS_PER_ACTION: usize = 256;
const MAX_INPUTS_PER_ACTION: usize = 1_000_000;
const MAX_OUTPUTS_PER_ACTION: usize = 65_536;
const MAX_ENV_PER_ACTION: usize = 16_384;
const MAX_NAMED_ENVIRONMENTS: usize = 65_536;
const MAX_ID_BYTES: usize = 4_096;
const MAX_MNEMONIC_BYTES: usize = 1_024;
const MAX_ARGUMENT_BYTES: usize = 16 * 1024 * 1024;

/// Stable machine-readable importer and executor error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionManifestErrorCode {
    /// The manifest could not be opened or read.
    Io,
    /// A JSONL record was malformed or contained an unknown field.
    InvalidJson,
    /// Header/action/final records appeared in an invalid order.
    InvalidRecordOrder,
    /// The schema identifier is not supported.
    UnsupportedSchema,
    /// The schema version is not supported.
    UnsupportedVersion,
    /// The exporter emitted an explicit unsupported record.
    ExporterUnsupported,
    /// Supported manifest versions cannot consume depfiles.
    UnsupportedDepfile,
    /// Supported manifest versions cannot consume Kconfig-value dependencies.
    UnsupportedConfigDeps,
    /// Supported manifest versions cannot implement restat semantics.
    UnsupportedRestat,
    /// Supported manifest versions cannot implement always-run semantics.
    UnsupportedAlwaysRun,
    /// Supported manifest versions are intentionally cache-disabled.
    UnsupportedCache,
    /// Supported versions cannot model a directory's dynamic member set.
    UnsupportedDirectoryMembership,
    /// A root declaration is invalid.
    InvalidRoot,
    /// A tagged path names an undeclared root.
    UnknownRoot,
    /// A tagged path is absolute, aliased, or escapes its root.
    PathEscape,
    /// An action or one of its scalar fields is invalid.
    InvalidAction,
    /// Two action records use the same identifier.
    DuplicateAction,
    /// Two actions claim the same output.
    DuplicateProducer,
    /// Declared outputs have an impossible file/directory namespace overlap.
    OutputNamespaceConflict,
    /// An output-tree input has no producer or producerless-path declaration.
    MissingProducer,
    /// The combined normal and order-only dependency graph is cyclic.
    Cycle,
    /// A source, tool, or bootstrap input is missing or has the wrong type.
    MissingInput,
    /// A command executable is invalid or undeclared as a normal input.
    InvalidProgram,
    /// A command working directory is invalid.
    InvalidWorkingDirectory,
    /// A declared output was not created.
    MissingOutput,
    /// A declared output existed before execute-all scheduling began.
    OutputAlreadyExists,
    /// Execution left a persistent obj-tree path absent from the manifest.
    UndeclaredObjWrite,
    /// Execution removed, replaced, or changed a prepared obj-tree entry.
    PreexistingObjMutation,
    /// A declared output has the wrong filesystem type.
    WrongOutputKind,
    /// A process could not start or returned a nonzero status.
    ExecutionFailed,
    /// The requested job cap was zero.
    InvalidJobs,
    /// Another Rush process owns the output-root build lock.
    OutputRootBusy,
    /// An internal Rush invariant failed after strict validation.
    Internal,
}

/// Versioned JSON error emitted by `--error-format=json`.
#[derive(Debug, Clone, Serialize)]
pub struct ActionManifestDiagnostic {
    /// Diagnostic schema identifier.
    pub schema: &'static str,
    /// Diagnostic schema version.
    pub version: u32,
    /// Stable failure classification.
    pub code: ActionManifestErrorCode,
    /// One-based manifest line, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    /// Logical action identifier, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_id: Option<String>,
    /// Stable unsupported-feature identifier, when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feature: Option<String>,
    /// Human-readable detail.
    pub message: String,
}

/// Error returned by strict manifest import or execution.
#[derive(Debug, Clone)]
pub struct ActionManifestError {
    diagnostic: ActionManifestDiagnostic,
}

impl ActionManifestError {
    fn new(code: ActionManifestErrorCode, message: impl Into<String>) -> Self {
        Self {
            diagnostic: ActionManifestDiagnostic {
                schema: ERROR_SCHEMA,
                version: ERROR_VERSION,
                code,
                line: None,
                action_id: None,
                feature: None,
                message: message.into(),
            },
        }
    }

    fn at_line(mut self, line: usize) -> Self {
        self.diagnostic.line = Some(line);
        self
    }

    fn for_action(mut self, action_id: &str) -> Self {
        self.diagnostic.action_id = Some(action_id.to_string());
        self
    }

    fn for_feature(mut self, feature: &str) -> Self {
        self.diagnostic.feature = Some(feature.to_string());
        self
    }

    /// Return the stable error code.
    pub fn code(&self) -> ActionManifestErrorCode {
        self.diagnostic.code
    }

    /// Return the structured diagnostic.
    pub fn diagnostic(&self) -> &ActionManifestDiagnostic {
        &self.diagnostic
    }

    /// Serialize the structured diagnostic as one JSON object.
    pub fn to_json(&self) -> String {
        serde_json::to_string(&self.diagnostic).unwrap_or_else(|_| {
            format!(
                "{{\"schema\":\"{}\",\"version\":{},\"code\":\"internal\",\"message\":\"failed to serialize diagnostic\"}}",
                ERROR_SCHEMA, ERROR_VERSION
            )
        })
    }
}

impl std::fmt::Display for ActionManifestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{:?}: {}",
            self.diagnostic.code, self.diagnostic.message
        )
    }
}

impl std::error::Error for ActionManifestError {}

type ManifestResult<T> = Result<T, ActionManifestError>;

#[derive(Debug, Deserialize)]
struct RecordEnvelope {
    record: ManifestRecordKind,
}

#[derive(Debug, Deserialize)]
struct HeaderVersionEnvelope {
    record: ManifestRecordKind,
    version: u32,
}

#[derive(Debug)]
struct ParsedAction {
    line: usize,
    action: ActionManifestAction,
    env: SharedEnv,
}

#[derive(Debug)]
struct ParsedManifest {
    header: ActionManifestHeader,
    actions: Vec<ParsedAction>,
}

type SharedEnv = Arc<[(String, String)]>;

#[derive(Debug)]
struct EnvPool<S = RandomState> {
    values: HashSet<SharedEnv, S>,
}

impl Default for EnvPool<RandomState> {
    fn default() -> Self {
        Self {
            values: HashSet::new(),
        }
    }
}

impl<S> EnvPool<S>
where
    S: BuildHasher,
{
    #[cfg(test)]
    fn with_hasher(hasher: S) -> Self {
        Self {
            values: HashSet::with_hasher(hasher),
        }
    }

    fn intern(&mut self, env: BTreeMap<String, String>) -> SharedEnv {
        let values = env.into_iter().collect::<Vec<_>>();
        if let Some(existing) = self.values.get(values.as_slice()) {
            return Arc::clone(existing);
        }
        let shared: SharedEnv = values.into();
        let inserted = self.values.insert(Arc::clone(&shared));
        debug_assert!(inserted, "exact env lookup and insertion disagreed");
        shared
    }
}

#[derive(Debug, Clone)]
struct ResolvedRoot {
    declared: PathBuf,
    canonical: PathBuf,
}

#[derive(Debug, Clone)]
struct ResolvedRoots {
    src: ResolvedRoot,
    obj: ResolvedRoot,
    tools: BTreeMap<String, ResolvedRoot>,
}

impl ResolvedRoots {
    fn get(&self, name: &str) -> Option<&ResolvedRoot> {
        match name {
            "src" => Some(&self.src),
            "obj" => Some(&self.obj),
            other => self.tools.get(other),
        }
    }
}

#[derive(Debug, Clone)]
struct ResolvedObjEntry {
    path: PathBuf,
    relative: PathBuf,
    kind: ManifestOutputKind,
}

#[derive(Debug, Clone)]
struct ResolvedCommand {
    executable: PathBuf,
    containment_root: PathBuf,
    argv: Vec<String>,
}

#[derive(Debug, Clone)]
struct ResolvedAction {
    line: usize,
    id: String,
    mnemonic: String,
    commands: Vec<ResolvedCommand>,
    cwd: PathBuf,
    cwd_containment_root: PathBuf,
    env: SharedEnv,
    env_policy: ActionEnvPolicy,
    resource_class: rush_resource::ResourceClass,
    declared_action_id: DeclaredActionId,
    normal_input_ids: Vec<ArtifactId>,
    output_ids: Vec<ArtifactId>,
    outputs: Vec<ResolvedObjEntry>,
}

/// A fully validated execute-all graph.
#[derive(Debug)]
pub struct ImportedActionGraph {
    _registry: ArtifactRegistry,
    obj_declared_root: PathBuf,
    obj_root: PathBuf,
    preexisting_obj_inputs: Vec<ResolvedObjEntry>,
    actions: Vec<ResolvedAction>,
    dependencies: Vec<Vec<usize>>,
    dependents: Vec<Vec<usize>>,
}

/// Successful execution summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActionManifestExecutionSummary {
    /// Number of logical actions executed.
    pub actions_executed: usize,
    /// Number of exact process invocations executed.
    pub commands_executed: usize,
}

/// Routing for successful imported-command output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionManifestOutputMode {
    /// Preserve command stdout on stdout and command stderr on stderr.
    Human,
    /// Route both streams to stderr, reserving stdout for the caller's JSON error.
    JsonDiagnostic,
}

/// Parse and validate a complete JSONL action graph without executing it.
pub fn load_action_manifest(path: &Path) -> ManifestResult<ImportedActionGraph> {
    let start = Instant::now();
    let parsed = parse_manifest(path)?;
    let graph = validate_manifest(parsed)?;
    append_manifest_trace(format!(
        "manifest_load\tduration_us={}\tactions={}",
        start.elapsed().as_micros(),
        graph.actions.len()
    ));
    Ok(graph)
}

fn append_manifest_trace(mut line: String) {
    let Some(path) = std::env::var_os("RUSH_RESOURCE_TRACE").filter(|path| !path.is_empty()) else {
        return;
    };
    line.push('\n');
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

fn action_v2_into_common(action: ActionManifestActionV2) -> ActionManifestAction {
    ActionManifestAction {
        record: action.record,
        id: action.id,
        mnemonic: action.mnemonic,
        commands: action.commands,
        cwd: action.cwd,
        env: BTreeMap::new(),
        env_policy: action.env_policy,
        normal_inputs: action.normal_inputs,
        order_only_inputs: action.order_only_inputs,
        outputs: action.outputs,
        resource: action.resource,
        depfile: action.depfile,
        config_deps: action.config_deps,
        restat: action.restat,
        always_run: action.always_run,
        cacheable: action.cacheable,
    }
}

fn parse_manifest(path: &Path) -> ManifestResult<ParsedManifest> {
    let metadata = fs::metadata(path).map_err(|error| {
        ActionManifestError::new(
            ActionManifestErrorCode::Io,
            format!("cannot stat manifest {}: {}", path.display(), error),
        )
    })?;
    if metadata.len() > MAX_MANIFEST_BYTES {
        return Err(ActionManifestError::new(
            ActionManifestErrorCode::InvalidJson,
            format!(
                "manifest is {} bytes; maximum is {}",
                metadata.len(),
                MAX_MANIFEST_BYTES
            ),
        ));
    }
    let file = fs::File::open(path).map_err(|error| {
        ActionManifestError::new(
            ActionManifestErrorCode::Io,
            format!("cannot open manifest {}: {}", path.display(), error),
        )
    })?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut line_number = 0usize;
    let mut header = None;
    let mut actions = Vec::new();
    let mut env_pool = EnvPool::default();
    let mut named_environments: BTreeMap<String, SharedEnv> = BTreeMap::new();
    let mut used_environments = BTreeSet::new();
    let mut final_seen = false;

    loop {
        line.clear();
        let bytes = Read::by_ref(&mut reader)
            .take(MAX_RECORD_BYTES + 1)
            .read_line(&mut line)
            .map_err(|error| {
                ActionManifestError::new(
                    ActionManifestErrorCode::Io,
                    format!("failed reading manifest: {}", error),
                )
            })?;
        if bytes == 0 {
            break;
        }
        line_number += 1;
        if bytes as u64 > MAX_RECORD_BYTES {
            return Err(ActionManifestError::new(
                ActionManifestErrorCode::InvalidJson,
                format!("record exceeds {} bytes", MAX_RECORD_BYTES),
            )
            .at_line(line_number));
        }
        let record_text = line.trim_end_matches(['\r', '\n']);
        if record_text.trim().is_empty() {
            return Err(ActionManifestError::new(
                ActionManifestErrorCode::InvalidJson,
                "blank JSONL records are not allowed",
            )
            .at_line(line_number));
        }
        if final_seen {
            return Err(ActionManifestError::new(
                ActionManifestErrorCode::InvalidRecordOrder,
                "records are not allowed after final",
            )
            .at_line(line_number));
        }
        let envelope: RecordEnvelope = serde_json::from_str(record_text).map_err(|error| {
            ActionManifestError::new(
                ActionManifestErrorCode::InvalidJson,
                format!("invalid record: {}", error),
            )
            .at_line(line_number)
        })?;
        match envelope.record {
            ManifestRecordKind::Header => {
                if header.is_some() || !actions.is_empty() {
                    return Err(ActionManifestError::new(
                        ActionManifestErrorCode::InvalidRecordOrder,
                        "header must be the first and only header record",
                    )
                    .at_line(line_number));
                }
                let version: HeaderVersionEnvelope = deserialize_record(record_text, line_number)?;
                if version.record != ManifestRecordKind::Header {
                    return Err(record_kind_error(line_number, "header"));
                }
                match version.version {
                    KBUILD_ACTION_MANIFEST_VERSION => {
                        let parsed: ActionManifestHeader =
                            deserialize_record(record_text, line_number)?;
                        if parsed.record != ManifestRecordKind::Header {
                            return Err(record_kind_error(line_number, "header"));
                        }
                        header = Some(parsed);
                    }
                    KBUILD_ACTION_MANIFEST_VERSION_2 => {
                        let parsed: ActionManifestHeaderV2 =
                            deserialize_record(record_text, line_number)?;
                        if parsed.record != ManifestRecordKind::Header {
                            return Err(record_kind_error(line_number, "header"));
                        }
                        if parsed.environments.len() > MAX_NAMED_ENVIRONMENTS {
                            return Err(ActionManifestError::new(
                                ActionManifestErrorCode::InvalidAction,
                                format!(
                                    "header exceeds {} named environments",
                                    MAX_NAMED_ENVIRONMENTS
                                ),
                            )
                            .at_line(line_number));
                        }
                        for named in parsed.environments {
                            validate_identifier(
                                "environment id",
                                &named.id,
                                MAX_ID_BYTES,
                                line_number,
                                None,
                            )?;
                            let shared = env_pool.intern(named.env);
                            validate_environment(
                                shared.as_ref(),
                                line_number,
                                EnvironmentContext::Named(&named.id),
                            )?;
                            if named_environments
                                .insert(named.id.clone(), shared)
                                .is_some()
                            {
                                return Err(ActionManifestError::new(
                                    ActionManifestErrorCode::InvalidAction,
                                    format!("duplicate named environment {:?}", named.id),
                                )
                                .at_line(line_number));
                            }
                        }
                        header = Some(ActionManifestHeader {
                            record: parsed.record,
                            schema: parsed.schema,
                            version: parsed.version,
                            roots: parsed.roots,
                            preexisting_obj_inputs: parsed.preexisting_obj_inputs,
                        });
                    }
                    unsupported => {
                        return Err(ActionManifestError::new(
                            ActionManifestErrorCode::UnsupportedVersion,
                            format!(
                                "unsupported schema version {}; latest is {}",
                                unsupported, KBUILD_ACTION_MANIFEST_LATEST_VERSION
                            ),
                        )
                        .at_line(line_number));
                    }
                }
            }
            ManifestRecordKind::Action => {
                if header.is_none() {
                    return Err(ActionManifestError::new(
                        ActionManifestErrorCode::InvalidRecordOrder,
                        "action appeared before header",
                    )
                    .at_line(line_number));
                }
                if actions.len() >= MAX_ACTIONS {
                    return Err(ActionManifestError::new(
                        ActionManifestErrorCode::InvalidAction,
                        format!("manifest exceeds {} actions", MAX_ACTIONS),
                    )
                    .at_line(line_number));
                }
                let manifest_version = header.as_ref().expect("header checked").version;
                let (action, env) = if manifest_version == KBUILD_ACTION_MANIFEST_VERSION {
                    let mut action: ActionManifestAction =
                        deserialize_record(record_text, line_number)?;
                    let env = env_pool.intern(std::mem::take(&mut action.env));
                    (action, env)
                } else {
                    let action: ActionManifestActionV2 =
                        deserialize_record(record_text, line_number)?;
                    validate_identifier(
                        "environment reference",
                        &action.env_ref,
                        MAX_ID_BYTES,
                        line_number,
                        Some(&action.id),
                    )?;
                    let env = named_environments
                        .get(&action.env_ref)
                        .cloned()
                        .ok_or_else(|| {
                            ActionManifestError::new(
                                ActionManifestErrorCode::InvalidAction,
                                format!("unknown environment reference {:?}", action.env_ref),
                            )
                            .at_line(line_number)
                            .for_action(&action.id)
                        })?;
                    used_environments.insert(action.env_ref.clone());
                    (action_v2_into_common(action), env)
                };
                actions.push(ParsedAction {
                    line: line_number,
                    action,
                    env,
                });
            }
            ManifestRecordKind::Unsupported => {
                if header.is_none() {
                    return Err(ActionManifestError::new(
                        ActionManifestErrorCode::InvalidRecordOrder,
                        "unsupported record appeared before header",
                    )
                    .at_line(line_number));
                }
                let unsupported: ActionManifestUnsupported =
                    deserialize_record(record_text, line_number)?;
                let mut error = ActionManifestError::new(
                    ActionManifestErrorCode::ExporterUnsupported,
                    format!(
                        "exporter cannot represent feature {}: {}",
                        unsupported.feature, unsupported.reason
                    ),
                )
                .at_line(line_number)
                .for_feature(&unsupported.feature);
                if let Some(action_id) = unsupported.action_id.as_deref() {
                    error = error.for_action(action_id);
                }
                return Err(error);
            }
            ManifestRecordKind::Final => {
                if header.is_none() {
                    return Err(ActionManifestError::new(
                        ActionManifestErrorCode::InvalidRecordOrder,
                        "final appeared before header",
                    )
                    .at_line(line_number));
                }
                let final_record: ActionManifestFinal =
                    deserialize_record(record_text, line_number)?;
                if final_record.action_count != actions.len() as u64 {
                    return Err(ActionManifestError::new(
                        ActionManifestErrorCode::InvalidRecordOrder,
                        format!(
                            "final action_count {} does not match {} action records",
                            final_record.action_count,
                            actions.len()
                        ),
                    )
                    .at_line(line_number));
                }
                final_seen = true;
            }
        }
    }

    let header = header.ok_or_else(|| {
        ActionManifestError::new(
            ActionManifestErrorCode::InvalidRecordOrder,
            "manifest is missing its header record",
        )
    })?;
    if !final_seen {
        return Err(ActionManifestError::new(
            ActionManifestErrorCode::InvalidRecordOrder,
            "manifest is missing its final record",
        ));
    }
    if header.version == KBUILD_ACTION_MANIFEST_VERSION_2 {
        if let Some(unused) = named_environments
            .keys()
            .find(|id| !used_environments.contains(*id))
        {
            return Err(ActionManifestError::new(
                ActionManifestErrorCode::InvalidAction,
                format!("unused named environment {:?}", unused),
            )
            .at_line(1));
        }
    }
    Ok(ParsedManifest { header, actions })
}

fn deserialize_record<T>(text: &str, line: usize) -> ManifestResult<T>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_str(text).map_err(|error| {
        ActionManifestError::new(
            ActionManifestErrorCode::InvalidJson,
            format!("invalid record: {}", error),
        )
        .at_line(line)
    })
}

fn record_kind_error(line: usize, expected: &str) -> ActionManifestError {
    ActionManifestError::new(
        ActionManifestErrorCode::InvalidJson,
        format!("record discriminator must be {}", expected),
    )
    .at_line(line)
}

#[derive(Debug)]
struct ActionDraft {
    line: usize,
    id: String,
    mnemonic: String,
    commands: Vec<ResolvedCommand>,
    cwd: PathBuf,
    cwd_containment_root: PathBuf,
    env: SharedEnv,
    env_policy: ActionEnvPolicy,
    resource_class: rush_resource::ResourceClass,
    normal_inputs: Vec<PathBuf>,
    order_only_inputs: Vec<PathBuf>,
    outputs: Vec<ResolvedObjEntry>,
}

fn validate_manifest(parsed: ParsedManifest) -> ManifestResult<ImportedActionGraph> {
    if parsed.header.schema != KBUILD_ACTION_MANIFEST_SCHEMA {
        return Err(ActionManifestError::new(
            ActionManifestErrorCode::UnsupportedSchema,
            format!(
                "unsupported schema {:?}; expected {:?}",
                parsed.header.schema, KBUILD_ACTION_MANIFEST_SCHEMA
            ),
        )
        .at_line(1));
    }
    if parsed.header.version != KBUILD_ACTION_MANIFEST_VERSION
        && parsed.header.version != KBUILD_ACTION_MANIFEST_VERSION_2
    {
        return Err(ActionManifestError::new(
            ActionManifestErrorCode::UnsupportedVersion,
            format!(
                "unsupported schema version {}; expected {} or {}",
                parsed.header.version,
                KBUILD_ACTION_MANIFEST_VERSION,
                KBUILD_ACTION_MANIFEST_VERSION_2
            ),
        )
        .at_line(1));
    }

    let roots = validate_roots(&parsed.header)?;
    let mut preexisting: HashMap<PathBuf, ManifestOutputKind> = HashMap::new();
    let mut preexisting_identities: HashMap<PathBuf, PathBuf> = HashMap::new();
    let mut resolved_preexisting = Vec::new();
    for entry in &parsed.header.preexisting_obj_inputs {
        if entry.path.root != "obj" {
            return Err(ActionManifestError::new(
                ActionManifestErrorCode::InvalidRoot,
                "preexisting_obj_inputs entries must use the obj root",
            )
            .at_line(1));
        }
        let path = resolve_tagged_path(&roots, &entry.path, false, 1, None)?;
        ensure_contained(&roots.obj.canonical, &path, 1, None)?;
        if preexisting.insert(path.clone(), entry.kind).is_some() {
            return Err(ActionManifestError::new(
                ActionManifestErrorCode::InvalidAction,
                format!("duplicate preexisting obj input {}", path.display()),
            )
            .at_line(1));
        }
        verify_existing_kind(
            &path,
            entry.kind,
            ActionManifestErrorCode::MissingInput,
            1,
            None,
        )?;
        let identity = fs::canonicalize(&path).map_err(|error| {
            ActionManifestError::new(
                ActionManifestErrorCode::MissingInput,
                format!(
                    "cannot resolve preexisting obj input {}: {}",
                    path.display(),
                    error
                ),
            )
            .at_line(1)
        })?;
        if let Some(previous) = preexisting_identities.insert(identity, path.clone()) {
            return Err(ActionManifestError::new(
                ActionManifestErrorCode::InvalidAction,
                format!(
                    "preexisting obj inputs {} and {} resolve to the same filesystem object",
                    previous.display(),
                    path.display()
                ),
            )
            .at_line(1));
        }
        resolved_preexisting.push(ResolvedObjEntry {
            path,
            relative: entry.path.path.clone(),
            kind: entry.kind,
        });
    }

    let mut action_ids: HashMap<String, usize> = HashMap::new();
    let mut output_producers: HashMap<PathBuf, usize> = HashMap::new();
    let mut output_kinds: HashMap<PathBuf, ManifestOutputKind> = HashMap::new();
    let mut output_identities: HashMap<PathBuf, PathBuf> = HashMap::new();
    let mut outputs_by_action: Vec<Vec<ResolvedObjEntry>> =
        Vec::with_capacity(parsed.actions.len());
    let validate_inline_environments = parsed.header.version == KBUILD_ACTION_MANIFEST_VERSION;

    for (index, parsed_action) in parsed.actions.iter().enumerate() {
        let action = &parsed_action.action;
        validate_action_scalars(action, parsed_action.line)?;
        if validate_inline_environments {
            validate_environment(
                &parsed_action.env,
                parsed_action.line,
                EnvironmentContext::Action(&action.id),
            )?;
        }
        if let Some(previous) = action_ids.insert(action.id.clone(), index) {
            return Err(ActionManifestError::new(
                ActionManifestErrorCode::DuplicateAction,
                format!(
                    "action id {:?} duplicates action at index {}",
                    action.id, previous
                ),
            )
            .at_line(parsed_action.line)
            .for_action(&action.id));
        }
        let mut local_outputs = HashSet::new();
        let mut resolved_outputs = Vec::with_capacity(action.outputs.len());
        for output in &action.outputs {
            if output.path.root != "obj" {
                return Err(ActionManifestError::new(
                    ActionManifestErrorCode::InvalidRoot,
                    "action outputs must use the obj root",
                )
                .at_line(parsed_action.line)
                .for_action(&action.id));
            }
            let path = resolve_tagged_path(
                &roots,
                &output.path,
                false,
                parsed_action.line,
                Some(&action.id),
            )?;
            ensure_contained(
                &roots.obj.canonical,
                &path,
                parsed_action.line,
                Some(&action.id),
            )?;
            ensure_no_symlinked_output_ancestors(
                &roots.obj.declared,
                &path,
                parsed_action.line,
                Some(&action.id),
            )?;
            if !local_outputs.insert(path.clone()) {
                return Err(ActionManifestError::new(
                    ActionManifestErrorCode::DuplicateProducer,
                    format!("action declares output {} more than once", path.display()),
                )
                .at_line(parsed_action.line)
                .for_action(&action.id));
            }
            if let Some(existing) = output_producers.insert(path.clone(), index) {
                let existing_id = &parsed.actions[existing].action.id;
                return Err(ActionManifestError::new(
                    ActionManifestErrorCode::DuplicateProducer,
                    format!(
                        "output {} is produced by both {:?} and {:?}",
                        path.display(),
                        existing_id,
                        action.id
                    ),
                )
                .at_line(parsed_action.line)
                .for_action(&action.id));
            }
            if preexisting.contains_key(&path) {
                return Err(ActionManifestError::new(
                    ActionManifestErrorCode::DuplicateProducer,
                    format!(
                        "output {} is also a preexisting_obj_inputs entry",
                        path.display()
                    ),
                )
                .at_line(parsed_action.line)
                .for_action(&action.id));
            }
            let identity = output_identity(&roots.obj, &path, &output.path.path);
            if let Some(previous) = output_identities.insert(identity.clone(), path.clone()) {
                return Err(ActionManifestError::new(
                    ActionManifestErrorCode::DuplicateProducer,
                    format!(
                        "outputs {} and {} resolve to the same filesystem location",
                        previous.display(),
                        path.display()
                    ),
                )
                .at_line(parsed_action.line)
                .for_action(&action.id));
            }
            if let Some(preexisting_path) = preexisting_identities.get(&identity) {
                return Err(ActionManifestError::new(
                    ActionManifestErrorCode::DuplicateProducer,
                    format!(
                        "output {} aliases preexisting obj input {}",
                        path.display(),
                        preexisting_path.display()
                    ),
                )
                .at_line(parsed_action.line)
                .for_action(&action.id));
            }
            output_kinds.insert(path.clone(), output.kind);
            resolved_outputs.push(ResolvedObjEntry {
                path,
                relative: output.path.path.clone(),
                kind: output.kind,
            });
        }
        outputs_by_action.push(resolved_outputs);
    }
    validate_output_namespace(&parsed.actions, &roots, &output_producers, &output_kinds)?;

    let action_count = parsed.actions.len();
    let mut drafts = Vec::with_capacity(action_count);
    let mut dependencies = vec![Vec::new(); action_count];
    for (index, (parsed_action, resolved_outputs)) in parsed
        .actions
        .into_iter()
        .zip(outputs_by_action.into_iter())
        .enumerate()
    {
        let ParsedAction {
            line,
            mut action,
            env,
        } = parsed_action;
        let mut normal_seen = HashSet::new();
        let mut normal_inputs = Vec::with_capacity(action.normal_inputs.len());
        for input in &action.normal_inputs {
            let path = validate_input(
                &roots,
                input,
                &preexisting,
                &output_producers,
                &output_kinds,
                true,
                line,
                &action.id,
            )?;
            if !normal_seen.insert(path.clone()) {
                return Err(ActionManifestError::new(
                    ActionManifestErrorCode::InvalidAction,
                    format!("duplicate normal input {}", path.display()),
                )
                .at_line(line)
                .for_action(&action.id));
            }
            if input.root != "obj" && output_producers.contains_key(&path) {
                return Err(ActionManifestError::new(
                    ActionManifestErrorCode::InvalidAction,
                    format!("external input {} aliases an action output", path.display()),
                )
                .at_line(line)
                .for_action(&action.id));
            }
            if let Some(producer) = producer_for_input(input, &path, &output_producers) {
                dependencies[index].push(producer);
            }
            normal_inputs.push(path);
        }

        let mut order_seen = HashSet::new();
        let mut order_only_inputs = Vec::with_capacity(action.order_only_inputs.len());
        for input in &action.order_only_inputs {
            let path = validate_input(
                &roots,
                input,
                &preexisting,
                &output_producers,
                &output_kinds,
                false,
                line,
                &action.id,
            )?;
            if !order_seen.insert(path.clone()) {
                return Err(ActionManifestError::new(
                    ActionManifestErrorCode::InvalidAction,
                    format!("duplicate order-only input {}", path.display()),
                )
                .at_line(line)
                .for_action(&action.id));
            }
            if normal_seen.contains(&path) {
                return Err(ActionManifestError::new(
                    ActionManifestErrorCode::InvalidAction,
                    format!("input {} is both normal and order-only", path.display()),
                )
                .at_line(line)
                .for_action(&action.id));
            }
            if input.root != "obj" && output_producers.contains_key(&path) {
                return Err(ActionManifestError::new(
                    ActionManifestErrorCode::InvalidAction,
                    format!(
                        "external order-only input {} aliases an action output",
                        path.display()
                    ),
                )
                .at_line(line)
                .for_action(&action.id));
            }
            if let Some(producer) = producer_for_input(input, &path, &output_producers) {
                dependencies[index].push(producer);
            }
            order_only_inputs.push(path);
        }

        let raw_commands = std::mem::take(&mut action.commands);
        let commands = validate_commands(
            &action.id,
            &action.normal_inputs,
            raw_commands,
            &roots,
            &preexisting,
            &output_producers,
            &output_kinds,
            line,
        )?;
        let (cwd, cwd_containment_root) = validate_working_directory(
            &action,
            &roots,
            &preexisting,
            &output_producers,
            &output_kinds,
            &normal_seen,
            &order_seen,
            line,
        )?;
        dependencies[index].sort_unstable();
        dependencies[index].dedup();
        drafts.push(ActionDraft {
            line,
            id: action.id,
            mnemonic: action.mnemonic,
            commands,
            cwd,
            cwd_containment_root,
            env,
            env_policy: map_env_policy(action.env_policy),
            resource_class: map_resource_class(action.resource),
            normal_inputs,
            order_only_inputs,
            outputs: resolved_outputs,
        });
    }

    let (registry, actions) = build_artifact_registry(drafts, &output_kinds)?;
    ActionDag::build(&registry).map_err(|error| {
        ActionManifestError::new(
            ActionManifestErrorCode::Cycle,
            format!("normal-input action DAG is cyclic: {}", error),
        )
    })?;
    let dependents = validate_combined_dag(&actions, &dependencies)?;
    Ok(ImportedActionGraph {
        _registry: registry,
        obj_declared_root: roots.obj.declared,
        obj_root: roots.obj.canonical,
        preexisting_obj_inputs: resolved_preexisting,
        actions,
        dependencies,
        dependents,
    })
}

fn validate_roots(header: &ActionManifestHeader) -> ManifestResult<ResolvedRoots> {
    let src = canonical_root("src", &header.roots.src)?;
    let obj = canonical_root("obj", &header.roots.obj)?;
    require_canonical_declared_root("src", &src)?;
    require_canonical_declared_root("obj", &obj)?;
    if roots_overlap(&src.canonical, &obj.canonical) {
        return Err(ActionManifestError::new(
            ActionManifestErrorCode::InvalidRoot,
            "src and obj must resolve to non-overlapping directories",
        )
        .at_line(1));
    }
    let mut tools = BTreeMap::new();
    for (name, path) in &header.roots.tools {
        if name == "src"
            || name == "obj"
            || name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
        {
            return Err(ActionManifestError::new(
                ActionManifestErrorCode::InvalidRoot,
                format!("invalid tool root name {:?}", name),
            )
            .at_line(1));
        }
        let tool = canonical_root(name, path)?;
        if roots_overlap(&tool.canonical, &obj.canonical) {
            return Err(ActionManifestError::new(
                ActionManifestErrorCode::InvalidRoot,
                format!(
                    "tool root {:?} and obj must resolve to non-overlapping directories",
                    name
                ),
            )
            .at_line(1));
        }
        tools.insert(name.clone(), tool);
    }
    Ok(ResolvedRoots { src, obj, tools })
}

fn require_canonical_declared_root(name: &str, root: &ResolvedRoot) -> ManifestResult<()> {
    if root.declared != root.canonical {
        return Err(ActionManifestError::new(
            ActionManifestErrorCode::InvalidRoot,
            format!(
                "root {:?} must use its canonical path {:?}, not {:?}",
                name, root.canonical, root.declared
            ),
        )
        .at_line(1));
    }
    Ok(())
}

fn canonical_root(name: &str, path: &Path) -> ManifestResult<ResolvedRoot> {
    if !path.is_absolute() || path.parent().is_none() {
        return Err(ActionManifestError::new(
            ActionManifestErrorCode::InvalidRoot,
            format!(
                "root {:?} must be an absolute non-filesystem-root path",
                name
            ),
        )
        .at_line(1));
    }
    let text = path.to_str().ok_or_else(|| {
        ActionManifestError::new(
            ActionManifestErrorCode::InvalidRoot,
            format!("root {:?} must be UTF-8", name),
        )
        .at_line(1)
    })?;
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir | Component::ParentDir => {
                return Err(ActionManifestError::new(
                    ActionManifestErrorCode::InvalidRoot,
                    format!("root {:?} is not lexically normalized", name),
                )
                .at_line(1));
            }
        }
    }
    if text.chars().any(char::is_control) || normalized.as_os_str() != path.as_os_str() {
        return Err(ActionManifestError::new(
            ActionManifestErrorCode::InvalidRoot,
            format!("root {:?} is not a normalized, control-free path", name),
        )
        .at_line(1));
    }
    let metadata = fs::metadata(path).map_err(|error| {
        ActionManifestError::new(
            ActionManifestErrorCode::InvalidRoot,
            format!("root {:?} does not exist: {}", name, error),
        )
        .at_line(1)
    })?;
    if !metadata.is_dir() {
        return Err(ActionManifestError::new(
            ActionManifestErrorCode::InvalidRoot,
            format!("root {:?} is not a directory", name),
        )
        .at_line(1));
    }
    let canonical = fs::canonicalize(path).map_err(|error| {
        ActionManifestError::new(
            ActionManifestErrorCode::InvalidRoot,
            format!("cannot canonicalize root {:?}: {}", name, error),
        )
        .at_line(1)
    })?;
    if canonical.parent().is_none() {
        return Err(ActionManifestError::new(
            ActionManifestErrorCode::InvalidRoot,
            format!("root {:?} resolves to the filesystem root", name),
        )
        .at_line(1));
    }
    Ok(ResolvedRoot {
        declared: path.to_path_buf(),
        canonical,
    })
}

fn roots_overlap(first: &Path, second: &Path) -> bool {
    first.starts_with(second) || second.starts_with(first)
}

fn output_identity(root: &ResolvedRoot, path: &Path, relative: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| root.canonical.join(relative))
}

fn ensure_no_symlinked_output_ancestors(
    declared_root: &Path,
    output: &Path,
    line: usize,
    action_id: Option<&str>,
) -> ManifestResult<()> {
    let relative = output.strip_prefix(declared_root).map_err(|_| {
        contextual_error(
            ActionManifestErrorCode::PathEscape,
            format!(
                "output {} is not lexically beneath obj root {}",
                output.display(),
                declared_root.display()
            ),
            line,
            action_id,
        )
    })?;
    let mut current = declared_root.to_path_buf();
    let Some(parent) = relative.parent() else {
        return Ok(());
    };
    for component in parent.components() {
        let Component::Normal(part) = component else {
            return Err(contextual_error(
                ActionManifestErrorCode::PathEscape,
                format!(
                    "output {} has an invalid parent component",
                    output.display()
                ),
                line,
                action_id,
            ));
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(contextual_error(
                    ActionManifestErrorCode::PathEscape,
                    format!(
                        "output {} traverses symbolic-link ancestor {}",
                        output.display(),
                        current.display()
                    ),
                    line,
                    action_id,
                ));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(contextual_error(
                    ActionManifestErrorCode::OutputNamespaceConflict,
                    format!(
                        "output {} has non-directory ancestor {}",
                        output.display(),
                        current.display()
                    ),
                    line,
                    action_id,
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(contextual_error(
                    ActionManifestErrorCode::PathEscape,
                    format!(
                        "cannot inspect output ancestor {}: {}",
                        current.display(),
                        error
                    ),
                    line,
                    action_id,
                ));
            }
        }
    }
    Ok(())
}

fn validate_output_namespace(
    parsed_actions: &[ParsedAction],
    roots: &ResolvedRoots,
    output_producers: &HashMap<PathBuf, usize>,
    output_kinds: &HashMap<PathBuf, ManifestOutputKind>,
) -> ManifestResult<()> {
    for (producer, action) in parsed_actions.iter().enumerate() {
        for declared_output in &action.action.outputs {
            let output = resolve_tagged_path(
                roots,
                &declared_output.path,
                false,
                action.line,
                Some(&action.action.id),
            )?;
            let mut ancestor = output.parent();
            while let Some(path) = ancestor {
                if path == roots.obj.declared {
                    break;
                }
                let Some(&ancestor_producer) = output_producers.get(path) else {
                    ancestor = path.parent();
                    continue;
                };
                let ancestor_action = &parsed_actions[ancestor_producer];
                match output_kinds.get(path) {
                    Some(ManifestOutputKind::File) => {
                        return Err(ActionManifestError::new(
                            ActionManifestErrorCode::OutputNamespaceConflict,
                            format!(
                                "file output {} from {:?} is an ancestor of output {}",
                                path.display(),
                                ancestor_action.action.id,
                                output.display()
                            ),
                        )
                        .at_line(action.line)
                        .for_action(&action.action.id));
                    }
                    Some(ManifestOutputKind::Directory) if ancestor_producer != producer => {
                        let mut ordered = false;
                        for input in &action.action.order_only_inputs {
                            if input.root == "obj"
                                && resolve_tagged_path(
                                    roots,
                                    input,
                                    false,
                                    action.line,
                                    Some(&action.action.id),
                                )? == path
                            {
                                ordered = true;
                                break;
                            }
                        }
                        if !ordered {
                            return Err(ActionManifestError::new(
                                ActionManifestErrorCode::OutputNamespaceConflict,
                                format!(
                                    "output {} must order on directory output {} from {:?}",
                                    output.display(),
                                    path.display(),
                                    ancestor_action.action.id
                                ),
                            )
                            .at_line(action.line)
                            .for_action(&action.action.id));
                        }
                    }
                    Some(ManifestOutputKind::Directory) => {}
                    None => {
                        return Err(ActionManifestError::new(
                            ActionManifestErrorCode::Internal,
                            format!("output kind is missing for {}", path.display()),
                        ));
                    }
                }
                ancestor = path.parent();
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum EnvironmentContext<'a> {
    Action(&'a str),
    Named(&'a str),
}

fn environment_error(
    context: EnvironmentContext<'_>,
    line: usize,
    message: impl Into<String>,
) -> ActionManifestError {
    let message = message.into();
    match context {
        EnvironmentContext::Action(action_id) => {
            ActionManifestError::new(ActionManifestErrorCode::InvalidAction, message)
                .at_line(line)
                .for_action(action_id)
        }
        EnvironmentContext::Named(environment_id) => ActionManifestError::new(
            ActionManifestErrorCode::InvalidAction,
            format!("named environment {:?}: {}", environment_id, message),
        )
        .at_line(line),
    }
}

fn validate_environment(
    env: &[(String, String)],
    line: usize,
    context: EnvironmentContext<'_>,
) -> ManifestResult<()> {
    if env.len() > MAX_ENV_PER_ACTION {
        return Err(environment_error(
            context,
            line,
            format!("environment exceeds {} entries", MAX_ENV_PER_ACTION),
        ));
    }
    for (key, value) in env {
        if key.is_empty() || key.contains('=') || key.contains('\0') {
            return Err(environment_error(
                context,
                line,
                format!("invalid environment key {:?}", key),
            ));
        }
        if value.contains('\0') {
            return Err(environment_error(
                context,
                line,
                format!("environment value for {:?} contains NUL", key),
            ));
        }
    }
    Ok(())
}

fn validate_action_scalars(action: &ActionManifestAction, line: usize) -> ManifestResult<()> {
    if action.record != ManifestRecordKind::Action {
        return Err(record_kind_error(line, "action"));
    }
    validate_identifier("action id", &action.id, MAX_ID_BYTES, line, None)?;
    validate_identifier(
        "mnemonic",
        &action.mnemonic,
        MAX_MNEMONIC_BYTES,
        line,
        Some(&action.id),
    )?;
    if action.commands.is_empty() || action.commands.len() > MAX_COMMANDS_PER_ACTION {
        return Err(ActionManifestError::new(
            ActionManifestErrorCode::InvalidAction,
            format!("commands count must be 1..={}", MAX_COMMANDS_PER_ACTION),
        )
        .at_line(line)
        .for_action(&action.id));
    }
    if action.normal_inputs.len() > MAX_INPUTS_PER_ACTION
        || action.order_only_inputs.len() > MAX_INPUTS_PER_ACTION
    {
        return Err(ActionManifestError::new(
            ActionManifestErrorCode::InvalidAction,
            format!("an input list exceeds {} entries", MAX_INPUTS_PER_ACTION),
        )
        .at_line(line)
        .for_action(&action.id));
    }
    if action.outputs.is_empty() || action.outputs.len() > MAX_OUTPUTS_PER_ACTION {
        return Err(ActionManifestError::new(
            ActionManifestErrorCode::InvalidAction,
            format!("outputs count must be 1..={}", MAX_OUTPUTS_PER_ACTION),
        )
        .at_line(line)
        .for_action(&action.id));
    }
    if action.depfile.is_some() {
        return Err(unsupported_error(
            ActionManifestErrorCode::UnsupportedDepfile,
            "depfile",
            line,
            &action.id,
        ));
    }
    if !action.config_deps.is_empty() {
        return Err(unsupported_error(
            ActionManifestErrorCode::UnsupportedConfigDeps,
            "config_deps",
            line,
            &action.id,
        ));
    }
    if action.restat {
        return Err(unsupported_error(
            ActionManifestErrorCode::UnsupportedRestat,
            "restat",
            line,
            &action.id,
        ));
    }
    if action.always_run {
        return Err(unsupported_error(
            ActionManifestErrorCode::UnsupportedAlwaysRun,
            "always_run",
            line,
            &action.id,
        ));
    }
    if action.cacheable {
        return Err(unsupported_error(
            ActionManifestErrorCode::UnsupportedCache,
            "cacheable",
            line,
            &action.id,
        ));
    }
    Ok(())
}

fn validate_identifier(
    what: &str,
    value: &str,
    max_bytes: usize,
    line: usize,
    action_id: Option<&str>,
) -> ManifestResult<()> {
    if value.is_empty() || value.len() > max_bytes || value.chars().any(char::is_control) {
        let mut error = ActionManifestError::new(
            ActionManifestErrorCode::InvalidAction,
            format!(
                "{} must be nonempty, control-free, and at most {} bytes",
                what, max_bytes
            ),
        )
        .at_line(line);
        if let Some(action_id) = action_id {
            error = error.for_action(action_id);
        }
        return Err(error);
    }
    Ok(())
}

fn unsupported_error(
    code: ActionManifestErrorCode,
    feature: &str,
    line: usize,
    action_id: &str,
) -> ActionManifestError {
    ActionManifestError::new(
        code,
        format!("manifest does not implement {} semantics", feature),
    )
    .at_line(line)
    .for_action(action_id)
    .for_feature(feature)
}

fn resolve_tagged_path(
    roots: &ResolvedRoots,
    tagged: &ManifestPath,
    allow_dot: bool,
    line: usize,
    action_id: Option<&str>,
) -> ManifestResult<PathBuf> {
    let root = roots.get(&tagged.root).ok_or_else(|| {
        contextual_error(
            ActionManifestErrorCode::UnknownRoot,
            format!("unknown path root {:?}", tagged.root),
            line,
            action_id,
        )
    })?;
    validate_relative_path(&tagged.path, allow_dot, line, action_id)?;
    Ok(root.declared.join(&tagged.path))
}

fn validate_relative_path(
    path: &Path,
    allow_dot: bool,
    line: usize,
    action_id: Option<&str>,
) -> ManifestResult<()> {
    let text = path.to_str().ok_or_else(|| {
        contextual_error(
            ActionManifestErrorCode::PathEscape,
            "manifest paths must be UTF-8",
            line,
            action_id,
        )
    })?;
    if text.chars().any(char::is_control) || path.is_absolute() || text.is_empty() {
        return Err(contextual_error(
            ActionManifestErrorCode::PathEscape,
            format!(
                "path {:?} must be a nonempty, control-free relative path",
                text
            ),
            line,
            action_id,
        ));
    }
    if text == "." {
        if allow_dot {
            return Ok(());
        }
        return Err(contextual_error(
            ActionManifestErrorCode::PathEscape,
            "the root path '.' is not valid here",
            line,
            action_id,
        ));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(contextual_error(
                    ActionManifestErrorCode::PathEscape,
                    format!("path {:?} is not normalized beneath its root", text),
                    line,
                    action_id,
                ));
            }
        }
    }
    if normalized.as_os_str() != path.as_os_str() {
        return Err(contextual_error(
            ActionManifestErrorCode::PathEscape,
            format!("path {:?} contains a lexical alias", text),
            line,
            action_id,
        ));
    }
    Ok(())
}

fn ensure_contained(
    root: &Path,
    candidate: &Path,
    line: usize,
    action_id: Option<&str>,
) -> ManifestResult<()> {
    let existing = fs::symlink_metadata(candidate).is_ok();
    let mut probe = candidate;
    let resolved = loop {
        match fs::canonicalize(probe) {
            Ok(path) => break path,
            Err(error) if existing && probe == candidate => {
                return Err(contextual_error(
                    ActionManifestErrorCode::PathEscape,
                    format!(
                        "existing path {} cannot be resolved safely: {}",
                        candidate.display(),
                        error
                    ),
                    line,
                    action_id,
                ));
            }
            Err(_) => {
                probe = probe.parent().ok_or_else(|| {
                    contextual_error(
                        ActionManifestErrorCode::PathEscape,
                        format!("cannot find a safe ancestor for {}", candidate.display()),
                        line,
                        action_id,
                    )
                })?;
            }
        }
    };
    if !resolved.starts_with(root) {
        return Err(contextual_error(
            ActionManifestErrorCode::PathEscape,
            format!(
                "path {} escapes canonical root {}",
                candidate.display(),
                root.display()
            ),
            line,
            action_id,
        ));
    }
    Ok(())
}

fn validate_input(
    roots: &ResolvedRoots,
    tagged: &ManifestPath,
    preexisting: &HashMap<PathBuf, ManifestOutputKind>,
    output_producers: &HashMap<PathBuf, usize>,
    output_kinds: &HashMap<PathBuf, ManifestOutputKind>,
    normal: bool,
    line: usize,
    action_id: &str,
) -> ManifestResult<PathBuf> {
    let path = resolve_tagged_path(roots, tagged, false, line, Some(action_id))?;
    let root = roots.get(&tagged.root).expect("root was resolved");
    ensure_contained(&root.canonical, &path, line, Some(action_id))?;
    if tagged.root == "obj" {
        if output_producers.contains_key(&path) {
            reject_normal_directory_membership(
                &path,
                output_kinds.get(&path).copied(),
                normal,
                line,
                action_id,
            )?;
            return Ok(path);
        }
        if let Some(kind) = preexisting.get(&path).copied() {
            reject_normal_directory_membership(&path, Some(kind), normal, line, action_id)?;
            return Ok(path);
        }
        return Err(ActionManifestError::new(
            ActionManifestErrorCode::MissingProducer,
            format!(
                "obj input {} has no manifest producer or preexisting_obj_inputs entry",
                path.display()
            ),
        )
        .at_line(line)
        .for_action(action_id));
    }
    fs::symlink_metadata(&path).map_err(|error| {
        ActionManifestError::new(
            ActionManifestErrorCode::MissingInput,
            format!("input {} is unavailable: {}", path.display(), error),
        )
        .at_line(line)
        .for_action(action_id)
    })?;
    if normal
        && fs::metadata(&path)
            .map(|metadata| metadata.is_dir())
            .unwrap_or(false)
    {
        return Err(directory_membership_error(&path, line, action_id));
    }
    Ok(path)
}

fn reject_normal_directory_membership(
    path: &Path,
    kind: Option<ManifestOutputKind>,
    normal: bool,
    line: usize,
    action_id: &str,
) -> ManifestResult<()> {
    if normal && kind == Some(ManifestOutputKind::Directory) {
        return Err(directory_membership_error(path, line, action_id));
    }
    Ok(())
}

fn directory_membership_error(path: &Path, line: usize, action_id: &str) -> ActionManifestError {
    ActionManifestError::new(
        ActionManifestErrorCode::UnsupportedDirectoryMembership,
        format!(
            "normal input {} requires unsupported directory-membership semantics; enumerate member files or use an order-only directory edge",
            path.display()
        ),
    )
    .at_line(line)
    .for_action(action_id)
    .for_feature("directory_membership")
}

fn producer_for_input(
    tagged: &ManifestPath,
    path: &Path,
    output_producers: &HashMap<PathBuf, usize>,
) -> Option<usize> {
    if tagged.root == "obj" {
        output_producers.get(path).copied()
    } else {
        None
    }
}

fn validate_commands(
    action_id: &str,
    normal_inputs: &[ManifestPath],
    raw_commands: Vec<ManifestCommand>,
    roots: &ResolvedRoots,
    preexisting: &HashMap<PathBuf, ManifestOutputKind>,
    output_producers: &HashMap<PathBuf, usize>,
    output_kinds: &HashMap<PathBuf, ManifestOutputKind>,
    line: usize,
) -> ManifestResult<Vec<ResolvedCommand>> {
    let mut commands = Vec::with_capacity(raw_commands.len());
    for (command_index, command) in raw_commands.into_iter().enumerate() {
        let executable =
            resolve_tagged_path(roots, &command.program, false, line, Some(action_id))?;
        let root = roots
            .get(&command.program.root)
            .expect("program root was resolved");
        ensure_contained(&root.canonical, &executable, line, Some(action_id))?;
        if !normal_inputs.iter().any(|input| input == &command.program) {
            return Err(ActionManifestError::new(
                ActionManifestErrorCode::InvalidProgram,
                format!(
                    "command {} program {} must also be a normal input",
                    command_index,
                    executable.display()
                ),
            )
            .at_line(line)
            .for_action(action_id));
        }
        if command.program.root == "obj" && output_producers.contains_key(&executable) {
            if output_kinds.get(&executable) != Some(&ManifestOutputKind::File) {
                return Err(ActionManifestError::new(
                    ActionManifestErrorCode::InvalidProgram,
                    format!(
                        "generated program {} must be declared as a file output",
                        executable.display()
                    ),
                )
                .at_line(line)
                .for_action(action_id));
            }
        } else {
            if command.program.root == "obj" && !preexisting.contains_key(&executable) {
                return Err(ActionManifestError::new(
                    ActionManifestErrorCode::MissingProducer,
                    format!("obj program {} has no producer", executable.display()),
                )
                .at_line(line)
                .for_action(action_id));
            }
            verify_executable(&executable, line, action_id)?;
        }
        for argument in &command.argv {
            if argument.len() > MAX_ARGUMENT_BYTES || argument.contains('\0') {
                return Err(ActionManifestError::new(
                    ActionManifestErrorCode::InvalidAction,
                    format!(
                        "command {} has an argument containing NUL or exceeding {} bytes",
                        command_index, MAX_ARGUMENT_BYTES
                    ),
                )
                .at_line(line)
                .for_action(action_id));
            }
        }
        commands.push(ResolvedCommand {
            executable,
            containment_root: root.canonical.clone(),
            argv: command.argv,
        });
    }
    Ok(commands)
}

fn verify_executable(path: &Path, line: usize, action_id: &str) -> ManifestResult<()> {
    let metadata = fs::metadata(path).map_err(|error| {
        ActionManifestError::new(
            ActionManifestErrorCode::InvalidProgram,
            format!("program {} is unavailable: {}", path.display(), error),
        )
        .at_line(line)
        .for_action(action_id)
    })?;
    if !metadata.is_file() {
        return Err(ActionManifestError::new(
            ActionManifestErrorCode::InvalidProgram,
            format!("program {} is not a file", path.display()),
        )
        .at_line(line)
        .for_action(action_id));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(ActionManifestError::new(
                ActionManifestErrorCode::InvalidProgram,
                format!("program {} is not executable", path.display()),
            )
            .at_line(line)
            .for_action(action_id));
        }
    }
    Ok(())
}

fn validate_working_directory(
    action: &ActionManifestAction,
    roots: &ResolvedRoots,
    preexisting: &HashMap<PathBuf, ManifestOutputKind>,
    output_producers: &HashMap<PathBuf, usize>,
    output_kinds: &HashMap<PathBuf, ManifestOutputKind>,
    normal_inputs: &HashSet<PathBuf>,
    order_only_inputs: &HashSet<PathBuf>,
    line: usize,
) -> ManifestResult<(PathBuf, PathBuf)> {
    let cwd = resolve_tagged_path(roots, &action.cwd, true, line, Some(&action.id))?;
    let root = roots.get(&action.cwd.root).expect("cwd root was resolved");
    ensure_contained(&root.canonical, &cwd, line, Some(&action.id))?;
    if action.cwd.root != "obj" || action.cwd.path == Path::new(".") {
        let metadata = fs::metadata(&cwd).map_err(|error| {
            ActionManifestError::new(
                ActionManifestErrorCode::InvalidWorkingDirectory,
                format!(
                    "working directory {} is unavailable: {}",
                    cwd.display(),
                    error
                ),
            )
            .at_line(line)
            .for_action(&action.id)
        })?;
        if !metadata.is_dir() {
            return Err(ActionManifestError::new(
                ActionManifestErrorCode::InvalidWorkingDirectory,
                format!("working directory {} is not a directory", cwd.display()),
            )
            .at_line(line)
            .for_action(&action.id));
        }
        return Ok((cwd, root.canonical.clone()));
    }
    let declared_input = normal_inputs.contains(&cwd) || order_only_inputs.contains(&cwd);
    let generated_directory = output_producers.contains_key(&cwd)
        && output_kinds.get(&cwd) == Some(&ManifestOutputKind::Directory);
    let bootstrap_directory = preexisting.get(&cwd) == Some(&ManifestOutputKind::Directory);
    if !declared_input || (!generated_directory && !bootstrap_directory) {
        return Err(ActionManifestError::new(
            ActionManifestErrorCode::InvalidWorkingDirectory,
            format!(
                "obj working directory {} must be a declared input produced as a directory or listed as a preexisting directory",
                cwd.display()
            ),
        )
        .at_line(line)
        .for_action(&action.id));
    }
    Ok((cwd, root.canonical.clone()))
}

fn verify_existing_kind(
    path: &Path,
    kind: ManifestOutputKind,
    code: ActionManifestErrorCode,
    line: usize,
    action_id: Option<&str>,
) -> ManifestResult<()> {
    let symlink_metadata = fs::symlink_metadata(path).map_err(|error| {
        contextual_error(
            code,
            format!("{} is unavailable: {}", path.display(), error),
            line,
            action_id,
        )
    })?;
    let matches = match kind {
        ManifestOutputKind::File => {
            symlink_metadata.is_file()
                || (symlink_metadata.file_type().is_symlink()
                    && fs::metadata(path)
                        .map(|metadata| metadata.is_file())
                        .unwrap_or(false))
        }
        ManifestOutputKind::Directory => symlink_metadata.is_dir(),
    };
    if !matches {
        return Err(contextual_error(
            code,
            format!("{} does not have declared kind {:?}", path.display(), kind),
            line,
            action_id,
        ));
    }
    Ok(())
}

fn build_artifact_registry(
    drafts: Vec<ActionDraft>,
    output_kinds: &HashMap<PathBuf, ManifestOutputKind>,
) -> ManifestResult<(ArtifactRegistry, Vec<ResolvedAction>)> {
    let owner = TargetId(0);
    let mut registry = ArtifactRegistry::new();
    let mut artifact_ids: HashMap<PathBuf, ArtifactId> = HashMap::new();

    for draft in &drafts {
        for output in &draft.outputs {
            let kind = match output.kind {
                ManifestOutputKind::File => ArtifactKind::Generated,
                ManifestOutputKind::Directory => ArtifactKind::Directory,
            };
            intern_artifact(
                &mut registry,
                &mut artifact_ids,
                &output.path,
                kind,
                false,
                owner,
            );
        }
    }
    for draft in &drafts {
        for input in draft
            .normal_inputs
            .iter()
            .chain(draft.order_only_inputs.iter())
        {
            if artifact_ids.contains_key(input) {
                continue;
            }
            let kind = match output_kinds.get(input) {
                Some(ManifestOutputKind::Directory) => ArtifactKind::Directory,
                Some(ManifestOutputKind::File) => ArtifactKind::Generated,
                None => ArtifactKind::Source,
            };
            intern_artifact(
                &mut registry,
                &mut artifact_ids,
                input,
                kind,
                output_kinds.get(input).is_none(),
                owner,
            );
        }
    }

    let mut resolved = Vec::with_capacity(drafts.len());
    for draft in drafts {
        let normal_input_ids: Vec<ArtifactId> = draft
            .normal_inputs
            .iter()
            .map(|path| artifact_ids[path])
            .collect();
        let output_ids: Vec<ArtifactId> = draft
            .outputs
            .iter()
            .map(|output| artifact_ids[&output.path])
            .collect();
        let mnemonic = registry.intern_string(&draft.mnemonic);
        let declared_action_id = registry
            .declare_action(DeclaredAction {
                owner_target: owner,
                mnemonic,
                inputs: normal_input_ids.clone().into_boxed_slice(),
                outputs: output_ids.clone().into_boxed_slice(),
                status: ActionDeclarationStatus::Complete,
            })
            .map_err(|error| {
                ActionManifestError::new(
                    ActionManifestErrorCode::Internal,
                    format!("artifact registry rejected validated action: {}", error),
                )
                .at_line(draft.line)
                .for_action(&draft.id)
            })?;
        resolved.push(ResolvedAction {
            line: draft.line,
            id: draft.id,
            mnemonic: draft.mnemonic,
            commands: draft.commands,
            cwd: draft.cwd,
            cwd_containment_root: draft.cwd_containment_root,
            env: draft.env,
            env_policy: draft.env_policy,
            resource_class: draft.resource_class,
            declared_action_id,
            normal_input_ids,
            output_ids,
            outputs: draft.outputs,
        });
    }
    Ok((registry, resolved))
}

fn intern_artifact(
    registry: &mut ArtifactRegistry,
    ids: &mut HashMap<PathBuf, ArtifactId>,
    path: &Path,
    kind: ArtifactKind,
    is_source: bool,
    owner: TargetId,
) {
    let exec_path = registry.intern_path(path);
    let id = registry.intern_artifact(
        ArtifactKey {
            config: ConfigId::DEFAULT,
            exec_path,
        },
        kind,
        is_source,
        owner,
        None,
    );
    ids.insert(path.to_path_buf(), id);
}

fn validate_combined_dag(
    actions: &[ResolvedAction],
    dependencies: &[Vec<usize>],
) -> ManifestResult<Vec<Vec<usize>>> {
    let mut in_degree = dependencies.iter().map(Vec::len).collect::<Vec<_>>();
    let mut dependents = vec![Vec::new(); dependencies.len()];
    for (consumer, producers) in dependencies.iter().enumerate() {
        for &producer in producers {
            dependents[producer].push(consumer);
        }
    }
    let mut ready = VecDeque::new();
    for (index, degree) in in_degree.iter().enumerate() {
        if *degree == 0 {
            ready.push_back(index);
        }
    }
    let mut visited = 0usize;
    while let Some(index) = ready.pop_front() {
        visited += 1;
        for &dependent in &dependents[index] {
            in_degree[dependent] -= 1;
            if in_degree[dependent] == 0 {
                ready.push_back(dependent);
            }
        }
    }
    if visited != dependencies.len() {
        let cycle_ids: Vec<&str> = in_degree
            .iter()
            .enumerate()
            .filter_map(|(index, degree)| {
                if *degree == 0 {
                    None
                } else {
                    Some(actions[index].id.as_str())
                }
            })
            .take(16)
            .collect();
        return Err(ActionManifestError::new(
            ActionManifestErrorCode::Cycle,
            format!(
                "combined normal/order-only graph is cyclic near {:?}",
                cycle_ids
            ),
        ));
    }
    Ok(dependents)
}

fn map_env_policy(policy: ManifestEnvPolicy) -> ActionEnvPolicy {
    match policy {
        ManifestEnvPolicy::ExplicitOnly => ActionEnvPolicy::ExplicitOnly,
        ManifestEnvPolicy::InheritHost => ActionEnvPolicy::InheritHost,
    }
}

fn map_resource_class(class: ManifestResourceClass) -> rush_resource::ResourceClass {
    match class {
        ManifestResourceClass::Compile => rush_resource::ResourceClass::CppCompile,
        ManifestResourceClass::KernelCompile => rush_resource::ResourceClass::KernelCompile,
        ManifestResourceClass::Link => rush_resource::ResourceClass::Link,
        ManifestResourceClass::KernelLink => rush_resource::ResourceClass::KernelLink,
        ManifestResourceClass::Archive => rush_resource::ResourceClass::Archive,
        ManifestResourceClass::Codegen => rush_resource::ResourceClass::CodegenProtoc,
        ManifestResourceClass::KernelCodegen => rush_resource::ResourceClass::KernelCodegen,
        ManifestResourceClass::Other => rush_resource::ResourceClass::Other,
    }
}

fn contextual_error(
    code: ActionManifestErrorCode,
    message: impl Into<String>,
    line: usize,
    action_id: Option<&str>,
) -> ActionManifestError {
    let mut error = ActionManifestError::new(code, message).at_line(line);
    if let Some(action_id) = action_id {
        error = error.for_action(action_id);
    }
    error
}

type ManifestRunner = PermitAcquiringRunner<DefaultLocalActionRunner>;
type RunningAction =
    std::pin::Pin<Box<dyn std::future::Future<Output = (usize, ManifestResult<usize>)> + Send>>;

#[derive(Debug, Default)]
struct ManifestSchedulerProfile {
    wall_us: u128,
    saturated_wait_us: u128,
    graph_limited_wait_us: u128,
    dispatches: usize,
    initial_ready: usize,
    max_ready: usize,
    max_running: usize,
    actions_completed: usize,
    commands_completed: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SchedulerWaitKind {
    Saturated,
    GraphLimited,
}

fn scheduler_wait_kind(running: usize, jobs: usize) -> SchedulerWaitKind {
    if running >= jobs {
        SchedulerWaitKind::Saturated
    } else {
        SchedulerWaitKind::GraphLimited
    }
}

#[cfg(unix)]
struct OutputRootLock {
    _directory: fs::File,
}

#[cfg(unix)]
impl OutputRootLock {
    // Use the raw flock operation deliberately. nix::fcntl::Flock unlocks on
    // Drop, which would also unlock an inherited open-file description still
    // held by the bootstrap wrapper. Closing this duplicate leaves the
    // wrapper's reference—and therefore its original lock—intact.
    #[allow(deprecated)]
    fn acquire(root: &Path, inherited_fd: Option<i32>) -> ManifestResult<Self> {
        use nix::fcntl::{flock, FlockArg};
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::fs::MetadataExt;

        let directory = match inherited_fd {
            Some(fd) if fd >= 0 => {
                // Duplicate the inherited open-file description so this owner
                // can close its handle without depending on the CLI's fd
                // lifetime. F_DUPFD preserves the underlying flock identity.
                let duplicate = unsafe { nix::libc::fcntl(fd, nix::libc::F_DUPFD_CLOEXEC, 0) };
                if duplicate < 0 {
                    return Err(ActionManifestError::new(
                        ActionManifestErrorCode::OutputRootBusy,
                        format!(
                            "cannot duplicate inherited output-lock fd {}: {}",
                            fd,
                            std::io::Error::last_os_error()
                        ),
                    ));
                }
                // SAFETY: fcntl returned a new owned descriptor above.
                unsafe { fs::File::from_raw_fd(duplicate) }
            }
            Some(fd) => {
                return Err(ActionManifestError::new(
                    ActionManifestErrorCode::OutputRootBusy,
                    format!("output-lock fd must be nonnegative, got {}", fd),
                ));
            }
            None => fs::File::open(root).map_err(|error| {
                ActionManifestError::new(
                    ActionManifestErrorCode::OutputRootBusy,
                    format!("cannot open output root for locking: {}", error),
                )
            })?,
        };
        let fd_metadata = directory.metadata().map_err(|error| {
            ActionManifestError::new(
                ActionManifestErrorCode::OutputRootBusy,
                format!("cannot stat output-lock fd: {}", error),
            )
        })?;
        let root_metadata = fs::metadata(root).map_err(|error| {
            ActionManifestError::new(
                ActionManifestErrorCode::OutputRootBusy,
                format!("cannot stat canonical output root: {}", error),
            )
        })?;
        if !fd_metadata.is_dir()
            || fd_metadata.dev() != root_metadata.dev()
            || fd_metadata.ino() != root_metadata.ino()
        {
            return Err(ActionManifestError::new(
                ActionManifestErrorCode::OutputRootBusy,
                format!(
                    "inherited output-lock fd does not refer to canonical obj root {}",
                    root.display()
                ),
            ));
        }
        if let Some(fd) = inherited_fd {
            let flags = unsafe { nix::libc::fcntl(fd, nix::libc::F_GETFD) };
            if flags < 0
                || unsafe {
                    nix::libc::fcntl(fd, nix::libc::F_SETFD, flags | nix::libc::FD_CLOEXEC)
                } < 0
            {
                return Err(ActionManifestError::new(
                    ActionManifestErrorCode::OutputRootBusy,
                    format!(
                        "cannot mark inherited output-lock fd {} close-on-exec: {}",
                        fd,
                        std::io::Error::last_os_error()
                    ),
                ));
            }
        }
        flock(directory.as_raw_fd(), FlockArg::LockExclusiveNonblock).map_err(|error| {
            ActionManifestError::new(
                ActionManifestErrorCode::OutputRootBusy,
                format!(
                    "cannot acquire exclusive lock on output root {}: {}",
                    root.display(),
                    error
                ),
            )
        })?;
        let locked_metadata = directory.metadata().map_err(|error| {
            ActionManifestError::new(
                ActionManifestErrorCode::OutputRootBusy,
                format!("cannot re-stat locked output-root fd: {}", error),
            )
        })?;
        let current_root_metadata = fs::metadata(root).map_err(|error| {
            ActionManifestError::new(
                ActionManifestErrorCode::OutputRootBusy,
                format!("cannot re-stat output root after locking: {}", error),
            )
        })?;
        if locked_metadata.dev() != current_root_metadata.dev()
            || locked_metadata.ino() != current_root_metadata.ino()
        {
            return Err(ActionManifestError::new(
                ActionManifestErrorCode::OutputRootBusy,
                format!(
                    "canonical obj root {} changed while acquiring its lock",
                    root.display()
                ),
            ));
        }
        Ok(Self {
            _directory: directory,
        })
    }
}

#[cfg(not(unix))]
struct OutputRootLock;

#[cfg(not(unix))]
impl OutputRootLock {
    fn acquire(_root: &Path, _inherited_fd: Option<i32>) -> ManifestResult<Self> {
        Err(ActionManifestError::new(
            ActionManifestErrorCode::OutputRootBusy,
            "external action manifests require a kernel-enforced Unix output-root lock",
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObjEntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct UnixObjEntryMetadata {
    dev: u64,
    ino: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    rdev: u64,
    non_directory: Option<UnixNonDirectoryMetadata>,
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct UnixNonDirectoryMetadata {
    nlink: u64,
    len: u64,
    mtime: i64,
    mtime_nsec: i64,
    ctime: i64,
    ctime_nsec: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ObjEntryMetadata {
    kind: ObjEntryKind,
    symlink_target: Option<PathBuf>,
    #[cfg(unix)]
    unix: UnixObjEntryMetadata,
    #[cfg(not(unix))]
    readonly: bool,
    #[cfg(not(unix))]
    non_directory_len: Option<u64>,
    #[cfg(not(unix))]
    non_directory_modified: Option<std::time::SystemTime>,
}

#[derive(Debug)]
struct ObjEntrySnapshot {
    metadata: ObjEntryMetadata,
}

type ObjTreeSnapshot = BTreeMap<PathBuf, ObjEntrySnapshot>;

#[derive(Debug, Clone)]
struct DeclaredObjOutput {
    path: PathBuf,
    kind: ManifestOutputKind,
    line: usize,
    action_id: String,
}

#[derive(Debug)]
struct ObjTreePolicy {
    outputs: BTreeMap<PathBuf, DeclaredObjOutput>,
    allowed_new_directories: BTreeSet<PathBuf>,
}

impl ObjTreePolicy {
    fn from_graph(graph: &ImportedActionGraph) -> Self {
        let mut outputs = BTreeMap::new();
        let mut allowed_new_directories = BTreeSet::new();
        for action in &graph.actions {
            for output in &action.outputs {
                outputs.insert(
                    output.relative.clone(),
                    DeclaredObjOutput {
                        path: output.path.clone(),
                        kind: output.kind,
                        line: action.line,
                        action_id: action.id.clone(),
                    },
                );
                let mut ancestor = output.relative.parent();
                while let Some(path) = ancestor {
                    if path.as_os_str().is_empty() {
                        break;
                    }
                    allowed_new_directories.insert(path.to_path_buf());
                    ancestor = path.parent();
                }
            }
        }
        Self {
            outputs,
            allowed_new_directories,
        }
    }
}

fn obj_entry_kind(metadata: &fs::Metadata) -> ObjEntryKind {
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        ObjEntryKind::Symlink
    } else if file_type.is_file() {
        ObjEntryKind::File
    } else if file_type.is_dir() {
        ObjEntryKind::Directory
    } else {
        ObjEntryKind::Other
    }
}

fn obj_entry_metadata(path: &Path, metadata: &fs::Metadata) -> ManifestResult<ObjEntryMetadata> {
    let kind = obj_entry_kind(metadata);
    let symlink_target = if kind == ObjEntryKind::Symlink {
        Some(fs::read_link(path).map_err(|error| {
            ActionManifestError::new(
                ActionManifestErrorCode::Io,
                format!("cannot read obj-tree symbolic link {:?}: {}", path, error),
            )
        })?)
    } else {
        None
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let non_directory = (kind != ObjEntryKind::Directory).then(|| UnixNonDirectoryMetadata {
            nlink: metadata.nlink(),
            len: metadata.len(),
            // Do not compare st_blocks. Delayed-allocation filesystems may
            // assign physical blocks after close without changing the file's
            // logical contents or other inode metadata.
            mtime: metadata.mtime(),
            mtime_nsec: metadata.mtime_nsec(),
            ctime: metadata.ctime(),
            ctime_nsec: metadata.ctime_nsec(),
        });
        Ok(ObjEntryMetadata {
            kind,
            symlink_target,
            unix: UnixObjEntryMetadata {
                dev: metadata.dev(),
                ino: metadata.ino(),
                mode: metadata.mode(),
                uid: metadata.uid(),
                gid: metadata.gid(),
                rdev: metadata.rdev(),
                non_directory,
            },
        })
    }

    #[cfg(not(unix))]
    {
        let is_directory = kind == ObjEntryKind::Directory;
        Ok(ObjEntryMetadata {
            kind,
            symlink_target,
            readonly: metadata.permissions().readonly(),
            non_directory_len: (!is_directory).then_some(metadata.len()),
            non_directory_modified: if is_directory {
                None
            } else {
                metadata.modified().ok()
            },
        })
    }
}

fn walk_entry_error(root: &Path, error: walkdir::Error) -> ActionManifestError {
    let path = error.path().unwrap_or(root);
    ActionManifestError::new(
        ActionManifestErrorCode::Io,
        format!("cannot inventory obj-tree entry {:?}: {}", path, error),
    )
}

fn relative_obj_path(root: &Path, path: &Path) -> ManifestResult<PathBuf> {
    path.strip_prefix(root).map(Path::to_path_buf).map_err(|_| {
        ActionManifestError::new(
            ActionManifestErrorCode::Internal,
            format!("obj-tree inventory path {:?} escaped root {:?}", path, root),
        )
    })
}

fn capture_obj_tree(root: &Path) -> ManifestResult<ObjTreeSnapshot> {
    let mut snapshot = BTreeMap::new();
    for entry in WalkDir::new(root).follow_links(false).sort_by_file_name() {
        let entry = entry.map_err(|error| walk_entry_error(root, error))?;
        let path = entry.path();
        let relative = relative_obj_path(root, path)?;
        let before_metadata = fs::symlink_metadata(path).map_err(|error| {
            ActionManifestError::new(
                ActionManifestErrorCode::Io,
                format!("cannot inspect obj-tree entry {:?}: {}", path, error),
            )
        })?;
        let before = obj_entry_metadata(path, &before_metadata)?;
        let after_metadata = fs::symlink_metadata(path).map_err(|error| {
            ActionManifestError::new(
                ActionManifestErrorCode::Io,
                format!(
                    "obj-tree entry {:?} disappeared while taking the initial snapshot: {}",
                    path, error
                ),
            )
        })?;
        let after = obj_entry_metadata(path, &after_metadata)?;
        if before != after {
            return Err(ActionManifestError::new(
                ActionManifestErrorCode::Io,
                format!(
                    "obj-tree entry {:?} changed while taking the initial snapshot",
                    path
                ),
            ));
        }
        snapshot.insert(relative, ObjEntrySnapshot { metadata: after });
    }
    Ok(snapshot)
}

fn validate_preexisting_obj_entry(
    path: &Path,
    relative: &Path,
    before: ObjEntrySnapshot,
) -> ManifestResult<()> {
    let current_metadata = fs::symlink_metadata(path).map_err(|error| {
        ActionManifestError::new(
            ActionManifestErrorCode::PreexistingObjMutation,
            format!(
                "preexisting obj-tree entry {:?} is unavailable after execution: {}",
                relative, error
            ),
        )
    })?;
    let current = obj_entry_metadata(path, &current_metadata).map_err(|error| {
        ActionManifestError::new(
            ActionManifestErrorCode::PreexistingObjMutation,
            format!(
                "cannot validate preexisting obj-tree entry {:?}: {}",
                relative, error
            ),
        )
    })?;
    if before.metadata != current {
        return Err(ActionManifestError::new(
            ActionManifestErrorCode::PreexistingObjMutation,
            format!(
                "preexisting obj-tree entry {:?} was removed, replaced, or changed metadata",
                relative
            ),
        ));
    }
    let after_metadata = fs::symlink_metadata(path).map_err(|error| {
        ActionManifestError::new(
            ActionManifestErrorCode::PreexistingObjMutation,
            format!(
                "preexisting obj-tree entry {:?} disappeared during final validation: {}",
                relative, error
            ),
        )
    })?;
    let after = obj_entry_metadata(path, &after_metadata).map_err(|error| {
        ActionManifestError::new(
            ActionManifestErrorCode::PreexistingObjMutation,
            format!(
                "cannot revalidate preexisting obj-tree entry {:?}: {}",
                relative, error
            ),
        )
    })?;
    if current != after {
        return Err(ActionManifestError::new(
            ActionManifestErrorCode::PreexistingObjMutation,
            format!(
                "preexisting obj-tree entry {:?} changed during final validation",
                relative
            ),
        ));
    }
    Ok(())
}

fn validate_obj_tree_after_execution(
    graph: &ImportedActionGraph,
    policy: &ObjTreePolicy,
    mut before: ObjTreeSnapshot,
) -> ManifestResult<()> {
    for entry in WalkDir::new(&graph.obj_root)
        .follow_links(false)
        .sort_by_file_name()
    {
        let entry = entry.map_err(|error| walk_entry_error(&graph.obj_root, error))?;
        let path = entry.path();
        let relative = relative_obj_path(&graph.obj_root, path)?;
        if let Some(snapshot) = before.remove(&relative) {
            validate_preexisting_obj_entry(path, &relative, snapshot)?;
            continue;
        }

        if policy.outputs.contains_key(&relative) {
            continue;
        }
        let metadata = fs::symlink_metadata(path).map_err(|error| {
            ActionManifestError::new(
                ActionManifestErrorCode::Io,
                format!(
                    "cannot inspect new obj-tree entry {:?}: {}",
                    relative, error
                ),
            )
        })?;
        if obj_entry_kind(&metadata) == ObjEntryKind::Directory
            && policy.allowed_new_directories.contains(&relative)
        {
            continue;
        }
        return Err(ActionManifestError::new(
            ActionManifestErrorCode::UndeclaredObjWrite,
            format!(
                "manifest execution left undeclared obj-tree entry {:?}",
                relative
            ),
        ));
    }

    if let Some((relative, _)) = before.first_key_value() {
        return Err(ActionManifestError::new(
            ActionManifestErrorCode::PreexistingObjMutation,
            format!(
                "preexisting obj-tree entry {:?} was removed during manifest execution",
                relative
            ),
        ));
    }

    for output in policy.outputs.values() {
        ensure_contained(
            &graph.obj_root,
            &output.path,
            output.line,
            Some(&output.action_id),
        )?;
        ensure_no_symlinked_output_ancestors(
            &graph.obj_declared_root,
            &output.path,
            output.line,
            Some(&output.action_id),
        )?;
        let code = if fs::symlink_metadata(&output.path).is_ok() {
            ActionManifestErrorCode::WrongOutputKind
        } else {
            ActionManifestErrorCode::MissingOutput
        };
        verify_existing_kind(
            &output.path,
            output.kind,
            code,
            output.line,
            Some(&output.action_id),
        )?;
    }
    Ok(())
}

/// Load, fully validate, and execute one manifest with an explicit job cap.
pub async fn execute_action_manifest(
    path: &Path,
    jobs: usize,
) -> ManifestResult<ActionManifestExecutionSummary> {
    execute_action_manifest_with_lock_fd(path, jobs, None).await
}

/// Load and execute a manifest while retaining an inherited obj-root lock fd.
pub async fn execute_action_manifest_with_lock_fd(
    path: &Path,
    jobs: usize,
    output_lock_fd: Option<i32>,
) -> ManifestResult<ActionManifestExecutionSummary> {
    execute_action_manifest_with_lock_fd_and_output_mode(
        path,
        jobs,
        output_lock_fd,
        ActionManifestOutputMode::Human,
    )
    .await
}

/// Load and execute with an inherited lock and explicit output routing.
pub async fn execute_action_manifest_with_lock_fd_and_output_mode(
    path: &Path,
    jobs: usize,
    output_lock_fd: Option<i32>,
    output_mode: ActionManifestOutputMode,
) -> ManifestResult<ActionManifestExecutionSummary> {
    let graph = load_action_manifest(path)?;
    execute_imported_action_graph_with_lock_fd_and_output_mode(
        graph,
        jobs,
        output_lock_fd,
        output_mode,
    )
    .await
}

/// Execute an already validated graph once, without cache or incremental skips.
pub async fn execute_imported_action_graph(
    graph: ImportedActionGraph,
    jobs: usize,
) -> ManifestResult<ActionManifestExecutionSummary> {
    execute_imported_action_graph_with_lock_fd(graph, jobs, None).await
}

/// Execute a validated graph while retaining an inherited obj-root lock fd.
pub async fn execute_imported_action_graph_with_lock_fd(
    graph: ImportedActionGraph,
    jobs: usize,
    output_lock_fd: Option<i32>,
) -> ManifestResult<ActionManifestExecutionSummary> {
    execute_imported_action_graph_with_lock_fd_and_output_mode(
        graph,
        jobs,
        output_lock_fd,
        ActionManifestOutputMode::Human,
    )
    .await
}

async fn execute_imported_action_graph_with_lock_fd_and_output_mode(
    graph: ImportedActionGraph,
    jobs: usize,
    output_lock_fd: Option<i32>,
    output_mode: ActionManifestOutputMode,
) -> ManifestResult<ActionManifestExecutionSummary> {
    let total_start = Instant::now();
    if jobs == 0 {
        return Err(ActionManifestError::new(
            ActionManifestErrorCode::InvalidJobs,
            "jobs must be greater than zero",
        ));
    }
    let lock_start = Instant::now();
    let output_root_lock = OutputRootLock::acquire(&graph.obj_root, output_lock_fd)?;
    validate_locked_graph_state(&graph)?;
    let lock_and_validation_us = lock_start.elapsed().as_micros();
    let inventory_start = Instant::now();
    let obj_tree_policy = ObjTreePolicy::from_graph(&graph);
    let obj_tree_before = capture_obj_tree(&graph.obj_root)?;
    let initial_inventory_us = inventory_start.elapsed().as_micros();
    let resource_manager = rush_resource::ResourceManager::new().map_err(|error| {
        ActionManifestError::new(
            ActionManifestErrorCode::Internal,
            format!("cannot initialize ResourceManager: {}", error),
        )
    })?;
    resource_manager.set_max_cpu_slots(jobs).await;
    let runner = Arc::new(PermitAcquiringRunner::new(
        DefaultLocalActionRunner,
        resource_manager,
    ));
    let graph = Arc::new(graph);
    let mut scheduler_profile = ManifestSchedulerProfile::default();
    let execution_result = execute_with_runner(
        Arc::clone(&graph),
        jobs,
        runner,
        output_mode,
        &mut scheduler_profile,
    )
    .await;
    let final_inventory_start = Instant::now();
    let inventory_result =
        validate_obj_tree_after_execution(&graph, &obj_tree_policy, obj_tree_before);
    let final_inventory_us = final_inventory_start.elapsed().as_micros();
    let result = match execution_result {
        Err(error) => Err(error),
        Ok(summary) => inventory_result.map(|()| summary),
    };
    drop(output_root_lock);
    append_manifest_trace(format!(
        "manifest_execute\ttotal_us={}\tlock_validation_us={}\tinitial_inventory_us={}\texecution_us={}\tfinal_inventory_us={}\tactions={}\tcompleted_actions={}\tcompleted_commands={}\tjobs={}\tinitial_ready={}\tmax_ready={}\tmax_running={}\tdispatches={}\tsaturated_wait_us={}\tgraph_limited_wait_us={}\toutcome={}",
        total_start.elapsed().as_micros(),
        lock_and_validation_us,
        initial_inventory_us,
        scheduler_profile.wall_us,
        final_inventory_us,
        graph.actions.len(),
        scheduler_profile.actions_completed,
        scheduler_profile.commands_completed,
        jobs,
        scheduler_profile.initial_ready,
        scheduler_profile.max_ready,
        scheduler_profile.max_running,
        scheduler_profile.dispatches,
        scheduler_profile.saturated_wait_us,
        scheduler_profile.graph_limited_wait_us,
        if result.is_ok() { "success" } else { "error" },
    ));
    result
}

fn validate_locked_graph_state(graph: &ImportedActionGraph) -> ManifestResult<()> {
    for input in &graph.preexisting_obj_inputs {
        ensure_contained(&graph.obj_root, &input.path, 1, None)?;
        verify_existing_kind(
            &input.path,
            input.kind,
            ActionManifestErrorCode::MissingInput,
            1,
            None,
        )?;
    }
    for action in &graph.actions {
        validate_action_outputs_absent(action, &graph.obj_declared_root, &graph.obj_root)?;
    }
    Ok(())
}

fn validate_action_outputs_absent(
    action: &ResolvedAction,
    obj_declared_root: &Path,
    obj_root: &Path,
) -> ManifestResult<()> {
    for output in &action.outputs {
        ensure_contained(obj_root, &output.path, action.line, Some(&action.id))?;
        ensure_no_symlinked_output_ancestors(
            obj_declared_root,
            &output.path,
            action.line,
            Some(&action.id),
        )?;
        match fs::symlink_metadata(&output.path) {
            Ok(_) => {
                return Err(ActionManifestError::new(
                    ActionManifestErrorCode::OutputAlreadyExists,
                    format!(
                        "declared output {} already exists before action execution",
                        output.path.display()
                    ),
                )
                .at_line(action.line)
                .for_action(&action.id));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(ActionManifestError::new(
                    ActionManifestErrorCode::Io,
                    format!(
                        "cannot inspect declared output {} before execution: {}",
                        output.path.display(),
                        error
                    ),
                )
                .at_line(action.line)
                .for_action(&action.id));
            }
        }
    }
    Ok(())
}

async fn execute_with_runner(
    graph: Arc<ImportedActionGraph>,
    jobs: usize,
    runner: Arc<ManifestRunner>,
    output_mode: ActionManifestOutputMode,
    profile: &mut ManifestSchedulerProfile,
) -> ManifestResult<ActionManifestExecutionSummary> {
    let scheduler_start = Instant::now();
    let mut remaining = graph.dependencies.iter().map(Vec::len).collect::<Vec<_>>();
    let mut ready = VecDeque::new();
    for (index, count) in remaining.iter().enumerate() {
        if *count == 0 {
            ready.push_back(index);
        }
    }
    profile.initial_ready = ready.len();
    profile.max_ready = ready.len();
    let mut running: FuturesUnordered<RunningAction> = FuturesUnordered::new();
    let output_lock = Arc::new(tokio::sync::Mutex::new(()));
    let mut first_failure = None;
    let mut actions_executed = 0usize;
    let mut commands_executed = 0usize;

    loop {
        while first_failure.is_none() && running.len() < jobs {
            let Some(index) = ready.pop_front() else {
                break;
            };
            running.push(run_action_future(
                Arc::clone(&runner),
                Arc::clone(&graph),
                Arc::clone(&output_lock),
                index,
                output_mode,
            ));
            profile.dispatches += 1;
            profile.max_running = profile.max_running.max(running.len());
        }
        let wait_kind = scheduler_wait_kind(running.len(), jobs);
        let wait_start = Instant::now();
        let Some((index, result)) = running.next().await else {
            break;
        };
        let waited_us = wait_start.elapsed().as_micros();
        match wait_kind {
            SchedulerWaitKind::Saturated => profile.saturated_wait_us += waited_us,
            SchedulerWaitKind::GraphLimited => profile.graph_limited_wait_us += waited_us,
        }
        match result {
            Ok(command_count) => {
                actions_executed += 1;
                commands_executed += command_count;
                profile.actions_completed = actions_executed;
                profile.commands_completed = commands_executed;
                if first_failure.is_none() {
                    for &dependent in &graph.dependents[index] {
                        remaining[dependent] -= 1;
                        if remaining[dependent] == 0 {
                            ready.push_back(dependent);
                        }
                    }
                    profile.max_ready = profile.max_ready.max(ready.len());
                }
            }
            Err(error) => {
                if first_failure.is_none() {
                    first_failure = Some(error);
                }
            }
        }
    }
    if let Some(error) = first_failure {
        profile.wall_us = scheduler_start.elapsed().as_micros();
        return Err(error);
    }
    if actions_executed != graph.actions.len() {
        profile.wall_us = scheduler_start.elapsed().as_micros();
        return Err(ActionManifestError::new(
            ActionManifestErrorCode::Internal,
            format!(
                "scheduler completed {} of {} validated actions",
                actions_executed,
                graph.actions.len()
            ),
        ));
    }
    profile.wall_us = scheduler_start.elapsed().as_micros();
    Ok(ActionManifestExecutionSummary {
        actions_executed,
        commands_executed,
    })
}

fn run_action_future(
    runner: Arc<ManifestRunner>,
    graph: Arc<ImportedActionGraph>,
    output_lock: Arc<tokio::sync::Mutex<()>>,
    index: usize,
    output_mode: ActionManifestOutputMode,
) -> RunningAction {
    Box::pin(async move {
        let result = execute_one_action(
            &runner,
            &graph.actions[index],
            &graph.obj_declared_root,
            &graph.obj_root,
            &output_lock,
            output_mode,
        )
        .await;
        (index, result)
    })
}

async fn execute_one_action(
    runner: &ManifestRunner,
    action: &ResolvedAction,
    obj_declared_root: &Path,
    obj_root: &Path,
    output_lock: &tokio::sync::Mutex<()>,
    output_mode: ActionManifestOutputMode,
) -> ManifestResult<usize> {
    validate_action_outputs_absent(action, obj_declared_root, obj_root)?;

    for (command_index, command) in action.commands.iter().enumerate() {
        ensure_contained(
            &command.containment_root,
            &command.executable,
            action.line,
            Some(&action.id),
        )?;
        verify_executable(&command.executable, action.line, &action.id)?;
        let request = ExecutionRequest {
            declared_action_id: action.declared_action_id,
            mnemonic: action.mnemonic.clone(),
            resource_class: Some(action.resource_class),
            owner_target: TargetId(0),
            command: MaterializedCommand {
                executable: command.executable.clone(),
                argv0: None,
                argv: command.argv.clone(),
                env: action.env.iter().cloned().collect(),
                working_dir: Some(action.cwd.clone()),
                env_policy: action.env_policy,
            },
            declared_inputs: action.normal_input_ids.clone(),
            declared_outputs: action.output_ids.clone(),
            timeout: None,
            log_command_index: Some(command_index as u32),
            remove_stale_archive_before_execute: false,
            terminate_process_group_on_drop: true,
            stdin_null: false,
            signal_state: None,
            file_creation_mask: None,
            credentials: None,
            isolation: None,
        };
        validate_working_directory_at_execution(action)?;
        let result = runner.execute(request).await.map_err(|error| {
            ActionManifestError::new(
                ActionManifestErrorCode::ExecutionFailed,
                format!("command {} could not execute: {}", command_index, error),
            )
            .at_line(action.line)
            .for_action(&action.id)
        })?;
        if result.exit_code != 0 {
            let stderr = String::from_utf8_lossy(&result.stderr_tail);
            let stdout = String::from_utf8_lossy(&result.stdout_tail);
            return Err(ActionManifestError::new(
                ActionManifestErrorCode::ExecutionFailed,
                format!(
                    "command {} exited {} (stdout tail: {:?}, stderr tail: {:?})",
                    command_index, result.exit_code, stdout, stderr
                ),
            )
            .at_line(action.line)
            .for_action(&action.id));
        }
        if !result.stdout_tail.is_empty() || !result.stderr_tail.is_empty() {
            let _guard = output_lock.lock().await;
            if !result.stdout_tail.is_empty() {
                let to_stderr = output_mode == ActionManifestOutputMode::JsonDiagnostic;
                forward_command_tail(&result.stdout_tail, to_stderr).map_err(|error| {
                    ActionManifestError::new(
                        ActionManifestErrorCode::ExecutionFailed,
                        format!("cannot forward command {} stdout: {}", command_index, error),
                    )
                    .at_line(action.line)
                    .for_action(&action.id)
                })?;
            }
            if !result.stderr_tail.is_empty() {
                forward_command_tail(&result.stderr_tail, true).map_err(|error| {
                    ActionManifestError::new(
                        ActionManifestErrorCode::ExecutionFailed,
                        format!("cannot forward command {} stderr: {}", command_index, error),
                    )
                    .at_line(action.line)
                    .for_action(&action.id)
                })?;
            }
        }
    }

    for output in &action.outputs {
        ensure_contained(obj_root, &output.path, action.line, Some(&action.id))?;
        ensure_no_symlinked_output_ancestors(
            obj_declared_root,
            &output.path,
            action.line,
            Some(&action.id),
        )?;
        let code = if fs::symlink_metadata(&output.path).is_ok() {
            ActionManifestErrorCode::WrongOutputKind
        } else {
            ActionManifestErrorCode::MissingOutput
        };
        verify_existing_kind(
            &output.path,
            output.kind,
            code,
            action.line,
            Some(&action.id),
        )?;
    }
    Ok(action.commands.len())
}

fn validate_working_directory_at_execution(action: &ResolvedAction) -> ManifestResult<()> {
    ensure_contained(
        &action.cwd_containment_root,
        &action.cwd,
        action.line,
        Some(&action.id),
    )?;
    let cwd_metadata = fs::metadata(&action.cwd).map_err(|error| {
        ActionManifestError::new(
            ActionManifestErrorCode::InvalidWorkingDirectory,
            format!(
                "working directory {} is unavailable at execution: {}",
                action.cwd.display(),
                error
            ),
        )
        .at_line(action.line)
        .for_action(&action.id)
    })?;
    if !cwd_metadata.is_dir() {
        return Err(ActionManifestError::new(
            ActionManifestErrorCode::InvalidWorkingDirectory,
            format!(
                "working directory {} is not a directory at execution",
                action.cwd.display()
            ),
        )
        .at_line(action.line)
        .for_action(&action.id));
    }
    Ok(())
}

fn forward_command_tail(tail: &[u8], to_stderr: bool) -> std::io::Result<()> {
    use std::io::Write;

    if to_stderr {
        let mut stderr = std::io::stderr().lock();
        stderr.write_all(tail)?;
        stderr.flush()
    } else {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(tail)?;
        stdout.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::Hasher;

    #[derive(Clone, Default)]
    struct ConstantBuildHasher;

    struct ConstantHasher;

    impl BuildHasher for ConstantBuildHasher {
        type Hasher = ConstantHasher;

        fn build_hasher(&self) -> Self::Hasher {
            ConstantHasher
        }
    }

    impl Hasher for ConstantHasher {
        fn finish(&self) -> u64 {
            0
        }

        fn write(&mut self, _bytes: &[u8]) {}
    }

    fn env(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn env_pool_interns_only_exact_values_under_forced_hash_collisions() {
        let mut pool = EnvPool::with_hasher(ConstantBuildHasher);
        let first = pool.intern(env(&[("A", "one"), ("SHARED", "same")]));
        let second = pool.intern(env(&[("A", "two"), ("SHARED", "same")]));
        let first_again = pool.intern(env(&[("SHARED", "same"), ("A", "one")]));

        assert!(!Arc::ptr_eq(&first, &second));
        assert!(Arc::ptr_eq(&first, &first_again));
        assert_eq!(first[0], ("A".to_string(), "one".to_string()));
        assert_eq!(second[0], ("A".to_string(), "two".to_string()));
        assert_eq!(pool.values.len(), 2);
    }

    #[test]
    fn manifest_kernel_classes_preserve_kernel_identity() {
        assert_eq!(
            map_resource_class(ManifestResourceClass::KernelCompile),
            rush_resource::ResourceClass::KernelCompile
        );
        assert_eq!(
            map_resource_class(ManifestResourceClass::KernelLink),
            rush_resource::ResourceClass::KernelLink
        );
        assert_eq!(
            map_resource_class(ManifestResourceClass::KernelCodegen),
            rush_resource::ResourceClass::KernelCodegen
        );
    }

    #[test]
    fn scheduler_waits_distinguish_full_capacity_from_graph_width() {
        assert_eq!(scheduler_wait_kind(8, 8), SchedulerWaitKind::Saturated);
        assert_eq!(scheduler_wait_kind(3, 8), SchedulerWaitKind::GraphLimited);
    }
}
