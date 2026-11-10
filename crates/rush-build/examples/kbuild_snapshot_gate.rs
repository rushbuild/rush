//! Focused native-capture, Kbuild-preflight, snapshot, and replay diagnostics.

use anyhow::{Context, Result};
#[cfg(unix)]
use rand::{rngs::OsRng, RngCore};
use rush_action::external_action_key::{ExternalCredentialState, ExternalSignalState};
#[cfg(unix)]
use rush_build::external_analysis_snapshot::ExternalAnalyzerIdentity;
use rush_build::external_analysis_snapshot::{
    load_if_fresh, persist, ExternalAnalysisKey, ExternalAnalysisSnapshot,
    ExternalInitialInputValue, ExternalSnapshotAdmission,
};
#[cfg(unix)]
use rush_build::external_controller::{
    ExternalBuildController, ExternalBuildOutcome, ExternalCaptureCompletion,
};
#[cfg(unix)]
use rush_build::external_generation::{
    ExternalCaptureTransactionIdentity, ExternalGenerationStore,
};
use rush_build::external_replay::{ExternalReplayEngine, ExternalReplayStatus};
#[cfg(unix)]
use rush_build::kbuild_snapshot::{
    census_kbuild_generated_inputs, census_kbuild_native_components,
    census_kbuild_procfs_directories, census_kbuild_procfs_reads, derive_kbuild_action_graph,
    diagnose_kbuild_native_union_chain, preflight_kbuild_graph_envelope,
    preflight_managed_kbuild_graph_envelope_with_seed, snapshot_from_kbuild_graph_envelope,
    snapshot_from_managed_kbuild_graph_envelope_with_seed, KbuildActionGraphEnvelope,
    KbuildManagedCaptureSeed, KbuildProcessCapture, KBUILD_GRAPH_ENVELOPE_SCHEMA,
    KBUILD_MANAGED_SCRATCH_DIRECTORY, KBUILD_MANAGED_SCRATCH_MODE,
};
use rush_build::kbuild_snapshot::{snapshot_from_kbuild_capture, KbuildActionCapture};
use rush_cache::cas::CasStore;
use rush_cache::{ActionCache, CacheConfig};
use rush_command_core::CommandSemantics;
#[cfg(unix)]
use rush_observe::{
    capture_process_tree_to_path, certify_process_capture_input_witnesses,
    load_process_capture_stream, visit_process_capture_stream, CapturedChildWaitOutcome,
    CapturedChildWaitSelector, CapturedCredentialRequest, CapturedProcessCreationOutcome,
    CapturedProcessId, CapturedProcessOutcome, ProcessCaptureStreamEvent,
    ProcessCaptureStreamIdentity, ProcessCaptureStreamLimits, ProcessCaptureStreamVisitor,
};
use rush_resource::ResourceManager;
use serde_json::json;
use std::ffi::OsStr;
use std::ffi::OsString;
#[cfg(unix)]
use std::fs::OpenOptions;
#[cfg(unix)]
use std::io::{BufWriter, Read, Write};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
#[cfg(unix)]
use std::time::{SystemTime, UNIX_EPOCH};
#[cfg(unix)]
use std::{collections::BTreeMap, collections::BTreeSet, fs::File};

#[cfg(unix)]
const KBUILD_BUILD_TIMESTAMP_VARIABLE: &str = "KBUILD_BUILD_TIMESTAMP";
#[cfg(unix)]
const SOURCE_DATE_EPOCH_VARIABLE: &str = "SOURCE_DATE_EPOCH";
#[cfg(unix)]
const PRIVATE_OUTPUT_BUFFER_BYTES: usize = 1024 * 1024;
#[cfg(unix)]
const PRIVATE_PROCESS_SLICE_MAX_PROCESSES: usize = 64;
#[cfg(unix)]
const PRIVATE_PROCESS_SLICE_MAX_EVENTS: usize = 100_000;
#[cfg(unix)]
const PRIVATE_PROCESS_SLICE_MAX_BYTES: u64 = 128 * 1024 * 1024;

fn usage() -> ! {
    eprintln!(
        "usage: kbuild_snapshot_gate capture <capture.json> <src> <obj> \
         <blocked-signal-mask-hex> <ignored-signal-mask-hex> \
         <file-creation-mask-octal> <credential-state.json> \
         <snapshot> <key.json> <cache>\n\
         or: kbuild_snapshot_gate observe-process <stream> <summary.json> \
         <max-stream-bytes> <max-records> <max-processes> <max-frame-bytes> \
         <cwd> <program> [arguments...]\n\
         or: kbuild_snapshot_gate managed-observe-process <state> <cache> \
         <stream> <summary.json> <transaction.json> <src> <obj> \
         <seed-config> <goal> <max-stream-bytes> <max-records> \
         <max-processes> <max-frame-bytes> <program> [arguments...]\n\
         or: kbuild_snapshot_gate certify-process <summary.json>\n\
         or: kbuild_snapshot_gate decode-process-private <summary.json> \
         <receipt.json>\n\
         or: kbuild_snapshot_gate slice-processes-private <summary.json> \
         <slice.json> <process-id> [process-id...]\n\
         or: kbuild_snapshot_gate diagnose-union-private <summary.json> \
         <diagnostic.json> <obj> <start-process-id> <finish-process-id>\n\
         or: kbuild_snapshot_gate census-credentials-private <summary.json> \
         <envelope.json> <census.json>\n\
         or: kbuild_snapshot_gate derive-graph <summary.json> <graph.json> \
         <envelope.json> <obj> <goal> [goals...]\n\
         or: kbuild_snapshot_gate census-components <summary.json> <obj>\n\
         or: kbuild_snapshot_gate census-procfs <envelope.json> <src> <obj>\n\
         or: kbuild_snapshot_gate census-procfs-directories <envelope.json> <src> <obj>\n\
         or: kbuild_snapshot_gate census-generated-inputs <envelope.json> \
         <src> <obj>\n\
         or: kbuild_snapshot_gate preflight-graph <envelope.json> <src> <obj>\n\
         or: kbuild_snapshot_gate assemble-graph <envelope.json> <src> <obj> \
         <snapshot> <key.json>\n\
         or: kbuild_snapshot_gate assemble-managed-graph <envelope.json> \
         <transaction.json> <src> <obj> <snapshot> <key.json>\n\
         or: kbuild_snapshot_gate preflight-managed-graph <envelope.json> \
         <transaction.json> <src> <obj>\n\
         or: kbuild_snapshot_gate inspect-snapshot-private <snapshot> <needle>\n\
         or: kbuild_snapshot_gate replay <snapshot> <key.json> <cache>\n\
         or: kbuild_snapshot_gate replay-commit <snapshot> <key.json> <cache>\n\
         or: kbuild_snapshot_gate replay-clean-commit <snapshot> <key.json> <cache>"
    );
    std::process::exit(2);
}

fn argument(arguments: &mut impl Iterator<Item = OsString>) -> PathBuf {
    arguments
        .next()
        .map(PathBuf::from)
        .unwrap_or_else(|| usage())
}

fn signal_mask(arguments: &mut impl Iterator<Item = OsString>, description: &str) -> u64 {
    let argument = arguments.next().unwrap_or_else(|| usage());
    let text = argument.to_str().unwrap_or_else(|| {
        eprintln!("{description} is not UTF-8");
        usage()
    });
    u64::from_str_radix(text.strip_prefix("0x").unwrap_or(text), 16).unwrap_or_else(|error| {
        eprintln!("invalid {description}: {error}");
        usage()
    })
}

fn file_creation_mask(arguments: &mut impl Iterator<Item = OsString>) -> u32 {
    let argument = arguments.next().unwrap_or_else(|| usage());
    let text = argument.to_str().unwrap_or_else(|| {
        eprintln!("file-creation mask is not UTF-8");
        usage()
    });
    let mask =
        u32::from_str_radix(text.strip_prefix("0o").unwrap_or(text), 8).unwrap_or_else(|error| {
            eprintln!("invalid file-creation mask: {error}");
            usage()
        });
    if mask & !0o777 != 0 {
        eprintln!("invalid file-creation mask: value exceeds 0777");
        usage()
    }
    mask
}

#[cfg(unix)]
fn number(arguments: &mut impl Iterator<Item = OsString>, description: &str) -> u64 {
    arguments
        .next()
        .unwrap_or_else(|| usage())
        .to_str()
        .unwrap_or_else(|| {
            eprintln!("{description} is not UTF-8");
            usage()
        })
        .parse::<u64>()
        .unwrap_or_else(|error| {
            eprintln!("invalid {description}: {error}");
            usage()
        })
}

#[cfg(unix)]
fn derive_graph(arguments: &mut impl Iterator<Item = OsString>) -> Result<()> {
    let summary_path = argument(arguments);
    let graph_path = argument(arguments);
    let envelope_path = argument(arguments);
    let output_root = std::fs::canonicalize(argument(arguments))
        .context("canonicalize native Kbuild output root")?;
    let goals = arguments.collect::<Vec<_>>();
    if goals.is_empty()
        || !summary_path.is_absolute()
        || !graph_path.is_absolute()
        || !envelope_path.is_absolute()
        || graph_path == envelope_path
    {
        usage();
    }
    let process_capture: KbuildProcessCapture = serde_json::from_slice(
        &std::fs::read(&summary_path)
            .with_context(|| format!("read {}", summary_path.display()))?,
    )
    .context("decode native Kbuild process summary")?;
    let requested_goals = goals
        .iter()
        .map(|goal| goal.as_os_str().as_bytes().to_vec())
        .collect::<Vec<_>>();
    let requested_outputs = goals
        .iter()
        .map(|goal| {
            let path = PathBuf::from(goal);
            if path.is_absolute() {
                path
            } else {
                output_root.join(path)
            }
        })
        .collect::<Vec<_>>();
    let derived = derive_kbuild_action_graph(
        &process_capture,
        &output_root,
        requested_goals,
        requested_outputs,
    )?;
    let mut graph_bytes = serde_json::to_vec_pretty(&derived.graph)?;
    graph_bytes.push(b'\n');
    let graph_digest = *blake3::hash(&graph_bytes).as_bytes();
    let max_graph_bytes =
        u64::try_from(graph_bytes.len()).context("native Kbuild graph size does not fit u64")?;
    let envelope = KbuildActionGraphEnvelope {
        schema: KBUILD_GRAPH_ENVELOPE_SCHEMA.to_string(),
        graph_path: graph_path.clone(),
        graph_digest,
        max_graph_bytes,
        process_capture,
        process_boundaries: derived.process_boundaries,
        coverage: derived.coverage,
    };
    write_private_bytes(&graph_path, &graph_bytes)?;
    write_private_json(&envelope_path, &envelope)?;
    println!(
        "{}",
        serde_json::to_string(&json!({
            "graph_actions": derived.graph.actions.len(),
            "mutation_paths": derived.coverage.persistent_writes_attributed,
            "process_lifecycles": derived.coverage.process_lifecycles_observed,
            "stream_records": derived.coverage.stream_records_observed,
        }))?
    );
    Ok(())
}

#[cfg(unix)]
fn diagnose_union_private(arguments: &mut impl Iterator<Item = OsString>) -> Result<()> {
    let summary_path = argument(arguments);
    let diagnostic_path = argument(arguments);
    let output_root = std::fs::canonicalize(argument(arguments))
        .context("canonicalize native Kbuild output root")?;
    let start = number(arguments, "start process ID");
    let finish = number(arguments, "finish process ID");
    if arguments.next().is_some()
        || !summary_path.is_absolute()
        || !diagnostic_path.is_absolute()
    {
        usage();
    }
    let process_capture: KbuildProcessCapture = serde_json::from_slice(
        &std::fs::read(&summary_path)
            .with_context(|| format!("read {}", summary_path.display()))?,
    )
    .context("decode native Kbuild process summary")?;
    let diagnostic = diagnose_kbuild_native_union_chain(
        &process_capture,
        &output_root,
        start,
        finish,
    )?;
    write_private_json(&diagnostic_path, &diagnostic)?;
    println!(
        "{}",
        serde_json::to_string(&json!({
            "start_process": start,
            "finish_process": finish,
            "component_roots": diagnostic.component_root_count(),
            "recorded_union_edges": diagnostic.recorded_union_edges(),
            "chain_length": diagnostic.chain_length(),
        }))?
    );
    Ok(())
}

#[cfg(unix)]
#[derive(Clone, Copy)]
enum CredentialCensusOwner {
    Analyzer,
    Action(u64),
}

#[cfg(unix)]
struct CredentialCensusVisitor {
    action_roots: BTreeMap<CapturedProcessId, u64>,
    owners: BTreeMap<CapturedProcessId, CredentialCensusOwner>,
    getgroups_by_process: BTreeMap<CapturedProcessId, u64>,
    getgroups_by_action: BTreeMap<u64, u64>,
    analyzer_getgroups: u64,
    stream_ordinal: u64,
}

#[cfg(unix)]
impl CredentialCensusVisitor {
    fn new(action_roots: BTreeMap<CapturedProcessId, u64>) -> Self {
        Self {
            action_roots,
            owners: BTreeMap::new(),
            getgroups_by_process: BTreeMap::new(),
            getgroups_by_action: BTreeMap::new(),
            analyzer_getgroups: 0,
            stream_ordinal: 0,
        }
    }
}

#[cfg(unix)]
impl ProcessCaptureStreamVisitor for CredentialCensusVisitor {
    fn visit(&mut self, event: ProcessCaptureStreamEvent<'_>) -> Result<()> {
        match event {
            ProcessCaptureStreamEvent::ProcessStarted { process, parent } => {
                if self.owners.len() >= 100_000 {
                    anyhow::bail!("private credential census exceeds its process budget");
                }
                let owner = if let Some(action) = self.action_roots.get(&process).copied() {
                    CredentialCensusOwner::Action(action)
                } else if let Some(parent) = parent {
                    *self
                        .owners
                        .get(&parent)
                        .context("credential census child precedes its parent")?
                } else {
                    CredentialCensusOwner::Analyzer
                };
                if self.owners.insert(process, owner).is_some() {
                    anyhow::bail!("credential census repeats a process start");
                }
            }
            ProcessCaptureStreamEvent::CredentialAttempt { process, attempt }
                if matches!(attempt.request, CapturedCredentialRequest::Getgroups { .. }) =>
            {
                let owner = *self
                    .owners
                    .get(&process)
                    .context("credential census request precedes its process")?;
                *self.getgroups_by_process.entry(process).or_default() += 1;
                match owner {
                    CredentialCensusOwner::Analyzer => self.analyzer_getgroups += 1,
                    CredentialCensusOwner::Action(action) => {
                        *self.getgroups_by_action.entry(action).or_default() += 1;
                    }
                }
            }
            _ => {}
        }
        self.stream_ordinal = self
            .stream_ordinal
            .checked_add(1)
            .context("credential census stream ordinal overflowed")?;
        Ok(())
    }
}

#[cfg(unix)]
fn census_credentials_private(arguments: &mut impl Iterator<Item = OsString>) -> Result<()> {
    let summary_path = argument(arguments);
    let envelope_path = argument(arguments);
    let census_path = argument(arguments);
    if arguments.next().is_some()
        || !summary_path.is_absolute()
        || !envelope_path.is_absolute()
        || !census_path.is_absolute()
    {
        usage();
    }
    let capture: KbuildProcessCapture = serde_json::from_slice(
        &std::fs::read(&summary_path)
            .with_context(|| format!("read {}", summary_path.display()))?,
    )
    .context("decode Kbuild process capture")?;
    let envelope: KbuildActionGraphEnvelope = serde_json::from_slice(
        &std::fs::read(&envelope_path)
            .with_context(|| format!("read {}", envelope_path.display()))?,
    )
    .context("decode Kbuild graph envelope")?;
    if envelope.process_capture != capture {
        anyhow::bail!("credential census envelope does not bind the process capture");
    }
    let action_roots = envelope
        .process_boundaries
        .actions
        .iter()
        .flat_map(|action| {
            action
                .commands
                .iter()
                .map(move |command| (command.process, action.action_index))
        })
        .collect::<BTreeMap<_, _>>();
    let (visitor, stream) = visit_process_capture_stream(
        &capture.stream_path,
        capture.summary,
        process_capture_limits(&capture)?,
        CredentialCensusVisitor::new(action_roots),
    )?;
    if visitor.stream_ordinal != stream.record_count() {
        anyhow::bail!("credential census did not visit the complete stream");
    }
    let action_getgroups = visitor
        .getgroups_by_action
        .values()
        .try_fold(0_u64, |total, count| total.checked_add(*count))
        .context("credential census action count overflowed")?;
    write_private_json(
        &census_path,
        &json!({
            "stream_digest": hex::encode(stream.digest()),
            "stream_records": stream.record_count(),
            "process_lifecycles": stream.process_count(),
            "getgroups_attempts": visitor.analyzer_getgroups + action_getgroups,
            "analyzer_getgroups_attempts": visitor.analyzer_getgroups,
            "action_getgroups_attempts": action_getgroups,
            "actions": visitor.getgroups_by_action,
            "processes": visitor.getgroups_by_process,
        }),
    )?;
    println!(
        "{}",
        serde_json::to_string(&json!({
            "stream_records": stream.record_count(),
            "getgroups_attempts": visitor.analyzer_getgroups + action_getgroups,
            "analyzer_getgroups_attempts": visitor.analyzer_getgroups,
            "action_getgroups_attempts": action_getgroups,
            "actions_with_getgroups": visitor.getgroups_by_action.len(),
        }))?
    );
    Ok(())
}

#[cfg(unix)]
fn hash_bytes(hasher: &mut blake3::Hasher, value: &[u8]) {
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value);
}

#[cfg(unix)]
fn capture_context_digest(
    working_directory: &Path,
    program: &OsStr,
    arguments: &[OsString],
    environment: &[(OsString, OsString)],
    limits: ProcessCaptureStreamLimits,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rush.kbuild-diagnostic-capture-context.v1\0");
    hash_bytes(&mut hasher, working_directory.as_os_str().as_bytes());
    hash_bytes(&mut hasher, program.as_bytes());
    hasher.update(&(arguments.len() as u64).to_le_bytes());
    for argument in arguments {
        hash_bytes(&mut hasher, argument.as_bytes());
    }
    hasher.update(&(environment.len() as u64).to_le_bytes());
    for (name, value) in environment {
        hash_bytes(&mut hasher, name.as_bytes());
        hash_bytes(&mut hasher, value.as_bytes());
    }
    hasher.update(&limits.max_stream_bytes().to_le_bytes());
    hasher.update(&limits.max_records().to_le_bytes());
    hasher.update(&limits.max_processes().to_le_bytes());
    hasher.update(&limits.max_frame_bytes().to_le_bytes());
    *hasher.finalize().as_bytes()
}

#[cfg(unix)]
fn maximize_capture_descriptor_limit() -> Result<()> {
    use nix::sys::resource::{getrlimit, setrlimit, Resource};

    let (soft, hard) = getrlimit(Resource::RLIMIT_NOFILE)
        .context("read process-capture descriptor limit")?;
    if soft < hard {
        setrlimit(Resource::RLIMIT_NOFILE, hard, hard)
            .context("raise process-capture descriptor limit to its hard ceiling")?;
    }
    Ok(())
}

#[cfg(unix)]
fn observe_process(arguments: &mut impl Iterator<Item = OsString>) -> Result<()> {
    maximize_capture_descriptor_limit()?;
    let stream_path = argument(arguments);
    let summary_path = argument(arguments);
    if !stream_path.is_absolute() || !summary_path.is_absolute() {
        anyhow::bail!("process capture artifacts must use absolute paths");
    }
    if stream_path == summary_path || stream_path.parent() != summary_path.parent() {
        anyhow::bail!("process stream and summary must be distinct sibling paths");
    }
    let max_stream_bytes = number(arguments, "maximum stream bytes");
    let max_records = number(arguments, "maximum record count");
    let max_processes = number(arguments, "maximum process count");
    let max_frame_bytes = number(arguments, "maximum frame bytes");
    let limits = ProcessCaptureStreamLimits::new(
        max_stream_bytes,
        max_records,
        max_processes,
        max_frame_bytes,
    )?;
    let working_directory = std::fs::canonicalize(argument(arguments))
        .context("canonicalize process capture working directory")?;
    let program = arguments.next().unwrap_or_else(|| usage());
    let command_arguments = arguments.collect::<Vec<_>>();
    let mut environment = std::env::vars_os().collect::<Vec<_>>();
    environment.sort_unstable();

    let context_digest = capture_context_digest(
        &working_directory,
        &program,
        &command_arguments,
        &environment,
        limits,
    );
    let mut capture_id = [0u8; 16];
    OsRng.fill_bytes(&mut capture_id);
    let identity = ProcessCaptureStreamIdentity::new(capture_id, context_digest);
    let mut command = std::process::Command::new(&program);
    command
        .args(&command_arguments)
        .current_dir(&working_directory)
        .env_clear()
        .envs(environment);
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&summary_path)
        .with_context(|| format!("create {}", summary_path.display()))?;
    let summary = capture_process_tree_to_path(&mut command, &stream_path, identity, limits)?;
    let process_capture = KbuildProcessCapture {
        stream_path,
        summary,
        max_stream_bytes,
        max_records,
        max_processes,
        max_frame_bytes,
    };
    let bytes = serde_json::to_vec_pretty(&process_capture)?;
    output
        .write_all(&bytes)
        .with_context(|| format!("write {}", summary_path.display()))?;
    output
        .sync_all()
        .with_context(|| format!("sync {}", summary_path.display()))?;
    println!("{}", serde_json::to_string(&process_capture)?);
    Ok(())
}

#[cfg(unix)]
fn digest_file(path: &Path) -> Result<[u8; 32]> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(*hasher.finalize().as_bytes())
}

#[cfg(unix)]
fn canonical_future_directory(path: PathBuf, description: &str) -> Result<PathBuf> {
    if !path.is_absolute() {
        anyhow::bail!("{description} must be absolute");
    }
    let parent = path.parent().context("managed output path has no parent")?;
    let name = path
        .file_name()
        .context("managed output path has no final component")?;
    let canonical_parent = std::fs::canonicalize(parent)
        .with_context(|| format!("canonicalize {description} parent"))?;
    let canonical = canonical_parent.join(name);
    if canonical.exists() {
        let actual = std::fs::canonicalize(&canonical)
            .with_context(|| format!("canonicalize {description}"))?;
        if actual != canonical || !std::fs::metadata(&actual)?.is_dir() {
            anyhow::bail!("{description} is not one canonical directory path");
        }
    }
    Ok(canonical)
}

#[cfg(unix)]
fn private_output(path: &Path) -> Result<BufWriter<File>> {
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    Ok(BufWriter::with_capacity(PRIVATE_OUTPUT_BUFFER_BYTES, output))
}

#[cfg(unix)]
fn finish_private_output(mut output: BufWriter<File>, path: &Path) -> Result<()> {
    output
        .flush()
        .with_context(|| format!("flush {}", path.display()))?;
    output
        .get_ref()
        .sync_all()
        .with_context(|| format!("sync {}", path.display()))
}

#[cfg(unix)]
fn write_private_bytes(path: &Path, value: &[u8]) -> Result<()> {
    let mut output = private_output(path)?;
    output
        .write_all(value)
        .with_context(|| format!("write {}", path.display()))?;
    finish_private_output(output, path)
}

#[cfg(unix)]
fn write_private_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let mut output = private_output(path)?;
    serde_json::to_writer_pretty(&mut output, value)
        .with_context(|| format!("write {}", path.display()))?;
    output
        .write_all(b"\n")
        .with_context(|| format!("write {}", path.display()))?;
    finish_private_output(output, path)
}

#[cfg(unix)]
#[derive(Default)]
struct RootOutcomeVisitor {
    root: Option<CapturedProcessId>,
    outcome: Option<CapturedProcessOutcome>,
}

#[cfg(unix)]
impl ProcessCaptureStreamVisitor for RootOutcomeVisitor {
    fn visit(&mut self, event: ProcessCaptureStreamEvent<'_>) -> Result<()> {
        match event {
            ProcessCaptureStreamEvent::ProcessStarted {
                process,
                parent: None,
            } => {
                if self.root.replace(process).is_some() {
                    anyhow::bail!("process capture stream repeats its root");
                }
            }
            ProcessCaptureStreamEvent::ProcessFinished { process, outcome }
                if Some(process) == self.root =>
            {
                if self.outcome.replace(outcome).is_some() {
                    anyhow::bail!("process capture stream repeats its root outcome");
                }
            }
            _ => {}
        }
        Ok(())
    }
}

#[cfg(unix)]
impl RootOutcomeVisitor {
    fn finish(self) -> Result<CapturedProcessOutcome> {
        self.root.context("process capture stream has no root")?;
        self.outcome
            .context("process capture stream has no root outcome")
    }
}

#[cfg(unix)]
fn managed_capture_environment(
    output_root: &Path,
    capture_epoch_seconds: u64,
    mut environment: Vec<(OsString, OsString)>,
) -> Result<Vec<(OsString, OsString)>> {
    if !environment
        .iter()
        .any(|(name, _)| name == OsStr::new(KBUILD_BUILD_TIMESTAMP_VARIABLE))
    {
        let source_date_epoch = environment
            .iter()
            .find(|(name, _)| name == OsStr::new(SOURCE_DATE_EPOCH_VARIABLE))
            .map(|(_, value)| {
                value
                    .to_str()
                    .context("SOURCE_DATE_EPOCH is not UTF-8")?
                    .parse::<u64>()
                    .context("SOURCE_DATE_EPOCH is not an unsigned integer")
            })
            .transpose()?;
        let epoch = source_date_epoch.unwrap_or(capture_epoch_seconds);
        environment.push((
            OsString::from(KBUILD_BUILD_TIMESTAMP_VARIABLE),
            OsString::from(format!("@{epoch}")),
        ));
    }
    environment.retain(|(name, _)| name != OsStr::new("TMPDIR"));
    environment.push((
        OsString::from("TMPDIR"),
        output_root
            .join(KBUILD_MANAGED_SCRATCH_DIRECTORY)
            .into_os_string(),
    ));
    environment.sort_unstable();
    Ok(environment)
}

#[cfg(unix)]
async fn managed_observe_process(arguments: &mut impl Iterator<Item = OsString>) -> Result<()> {
    maximize_capture_descriptor_limit()?;
    let state_root = argument(arguments);
    let cache_root = argument(arguments);
    let stream_path = argument(arguments);
    let summary_path = argument(arguments);
    let transaction_path = argument(arguments);
    let source_root = std::fs::canonicalize(argument(arguments))
        .context("canonicalize managed capture source root")?;
    let output_root = canonical_future_directory(argument(arguments), "managed output root")?;
    let seed_path = std::fs::canonicalize(argument(arguments))
        .context("canonicalize explicit configuration seed")?;
    let goal = arguments.next().unwrap_or_else(|| usage());
    let max_stream_bytes = number(arguments, "maximum stream bytes");
    let max_records = number(arguments, "maximum record count");
    let max_processes = number(arguments, "maximum process count");
    let max_frame_bytes = number(arguments, "maximum frame bytes");
    let limits = ProcessCaptureStreamLimits::new(
        max_stream_bytes,
        max_records,
        max_processes,
        max_frame_bytes,
    )?;
    let program = std::fs::canonicalize(argument(arguments))
        .context("canonicalize managed capture analyzer executable")?;
    let command_arguments = arguments.collect::<Vec<_>>();

    let seed_receipt_path = transaction_path.with_extension("seed.json");
    let artifacts = [
        &stream_path,
        &summary_path,
        &transaction_path,
        &seed_receipt_path,
    ];
    if artifacts.iter().any(|path| !path.is_absolute())
        || artifacts.iter().any(|path| path.starts_with(&output_root))
        || stream_path.parent() != summary_path.parent()
        || stream_path.parent() != transaction_path.parent()
        || stream_path == summary_path
        || stream_path == transaction_path
        || summary_path == transaction_path
    {
        anyhow::bail!("managed capture artifacts must be distinct private siblings outside output");
    }
    if !std::fs::metadata(&seed_path)?.is_file() || seed_path.starts_with(&output_root) {
        anyhow::bail!("configuration seed must be a regular file outside managed output");
    }
    let config_bytes = std::fs::read(&seed_path).context("read explicit configuration seed")?;
    let goal_bytes = goal.as_os_str().as_bytes().to_vec();
    if goal_bytes.is_empty()
        || !command_arguments
            .iter()
            .any(|argument| argument.as_os_str().as_bytes() == goal_bytes)
    {
        anyhow::bail!("managed Kbuild command omits its exact requested goal");
    }
    let mut output_assignment = b"O=".to_vec();
    output_assignment.extend_from_slice(output_root.as_os_str().as_bytes());
    if !command_arguments
        .iter()
        .any(|argument| argument.as_os_str().as_bytes() == output_assignment)
    {
        anyhow::bail!("managed Kbuild command omits its exact output-root assignment");
    }

    let capture_epoch_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("managed capture clock precedes the Unix epoch")?
        .as_secs();
    let environment = managed_capture_environment(
        &output_root,
        capture_epoch_seconds,
        std::env::vars_os().collect::<Vec<_>>(),
    )?;
    let analyzer_environment = environment
        .iter()
        .map(|(name, value)| {
            (
                name.as_os_str().as_bytes().to_vec(),
                value.as_os_str().as_bytes().to_vec(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let mut analyzer_argv = Vec::with_capacity(command_arguments.len() + 1);
    analyzer_argv.push(program.as_os_str().as_bytes().to_vec());
    analyzer_argv.extend(
        command_arguments
            .iter()
            .map(|argument| argument.as_os_str().as_bytes().to_vec()),
    );
    let analyzer = ExternalAnalyzerIdentity {
        kind: "gnu-make-kbuild".to_string(),
        executable_digest: digest_file(&program)?,
        argv: analyzer_argv,
        working_directory: source_root.clone(),
        environment: analyzer_environment,
        build_options: BTreeMap::new(),
        platform: BTreeMap::from([("os".to_string(), std::env::consts::OS.to_string())]),
    };
    let key = ExternalAnalysisKey::new(
        source_root.clone(),
        output_root.clone(),
        &analyzer,
        vec![goal_bytes],
    );
    let store = ExternalGenerationStore::new(&state_root, key);
    let (actions, cas) = cache(&cache_root)?;
    let controller =
        ExternalBuildController::new(store, actions, cas, CommandSemantics::from_process());

    let captured_output = output_root.clone();
    let outcome = controller
        .build(move |request| async move {
            anyhow::ensure!(
                request.output_root == captured_output,
                "controller substituted the managed output root"
            );
              let scratch_path = request.output_root.join(KBUILD_MANAGED_SCRATCH_DIRECTORY);
              let mut scratch = std::fs::DirBuilder::new();
              std::os::unix::fs::DirBuilderExt::mode(
                  &mut scratch,
                  KBUILD_MANAGED_SCRATCH_MODE,
              );
              scratch
                  .create(&scratch_path)
                  .context("create managed capture scratch")?;
            let config_path = request.output_root.join(".config");
            let mut config = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o644)
                .open(&config_path)
                .context("create managed configuration seed")?;
            config
                .write_all(&config_bytes)
                .context("write managed configuration seed")?;
            config
                .sync_all()
                .context("sync managed configuration seed")?;
            File::open(&request.output_root)?
                .sync_all()
                .context("sync managed output directory")?;

            use std::os::unix::fs::MetadataExt as _;
            let source_mode = std::fs::metadata(&seed_path)
                .context("stat explicit configuration seed")?
                .mode();
            let output_mode = std::fs::metadata(&config_path)
                .context("stat managed configuration seed")?
                .mode();
            write_private_json(
                &seed_receipt_path,
                &KbuildManagedCaptureSeed {
                    schema: KbuildManagedCaptureSeed::SCHEMA.to_string(),
                    transaction: request.transaction_identity.clone(),
                    source_path: seed_path.clone(),
                    output_path: config_path.clone(),
                    bytes: config_bytes.clone(),
                    output_mode,
                    source_mode,
                },
            )?;

            let identity = request.process_capture_identity();
            let mut command = std::process::Command::new(&program);
            command
                .args(&command_arguments)
                .current_dir(&source_root)
                .env_clear()
                .envs(environment)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            let summary =
                capture_process_tree_to_path(&mut command, &stream_path, identity, limits)?;
            std::fs::remove_dir(&scratch_path)
                .context("remove empty managed capture scratch")?;
            let (visitor, seal) = visit_process_capture_stream(
                &stream_path,
                summary,
                limits,
                RootOutcomeVisitor::default(),
            )?;
            anyhow::ensure!(
                seal.identity() == identity,
                "managed observer stream returned a different transaction identity"
            );
            let root_outcome = visitor.finish()?;
            let process_capture = KbuildProcessCapture {
                stream_path,
                summary,
                max_stream_bytes,
                max_records,
                max_processes,
                max_frame_bytes,
            };
            write_private_json(&summary_path, &process_capture)?;
            write_private_json(&transaction_path, &request.transaction_identity)?;
            Ok(match root_outcome {
                CapturedProcessOutcome::Exited(0) => ExternalCaptureCompletion::Uncertified {
                    result: root_outcome,
                    reason: "managed Kbuild observation completed without namespace provenance \
                             or uncaptured-writer exclusion"
                        .to_string(),
                },
                _ => ExternalCaptureCompletion::Failed(root_outcome),
            })
        })
        .await?;

    let (report, successful) = match outcome {
        ExternalBuildOutcome::Replayed {
            decision,
            recovery,
            replay,
        } => (
            json!({
                "outcome": "replayed",
                "decision": decision.as_str(),
                "recovery": format!("{recovery:?}"),
                "replay": format!("{replay:?}"),
            }),
            true,
        ),
        ExternalBuildOutcome::CaptureFailed {
            decision,
            admission_reason,
            recovery,
            result,
        } => (
            json!({
                "outcome": "capture_failed",
                "decision": decision.as_str(),
                "admission_reason": admission_reason,
                "recovery": format!("{recovery:?}"),
                "result": result,
            }),
            false,
        ),
        ExternalBuildOutcome::CaptureUncertified {
            decision,
            admission_reason,
            recovery,
            result,
            reason,
        } => (
            json!({
                "outcome": "capture_uncertified",
                "decision": decision.as_str(),
                "admission_reason": admission_reason,
                "recovery": format!("{recovery:?}"),
                "result": result,
                "reason": reason,
            }),
            true,
        ),
        ExternalBuildOutcome::Captured { .. } => {
            anyhow::bail!("diagnostic managed observer unexpectedly published a snapshot")
        }
        ExternalBuildOutcome::Unsupported { recovery, reason } => (
            json!({
                "outcome": "unsupported",
                "recovery": format!("{recovery:?}"),
                "reason": reason,
            }),
            false,
        ),
    };
    println!("{}", serde_json::to_string(&report)?);
    if !successful {
        anyhow::bail!("managed analyzer capture did not complete successfully");
    }
    Ok(())
}

#[cfg(unix)]
fn process_capture_limits(capture: &KbuildProcessCapture) -> Result<ProcessCaptureStreamLimits> {
    ProcessCaptureStreamLimits::new(
        capture.max_stream_bytes,
        capture.max_records,
        capture.max_processes,
        capture.max_frame_bytes,
    )
}

#[cfg(unix)]
fn certify_process(arguments: &mut impl Iterator<Item = OsString>) -> Result<()> {
    let summary_path = argument(arguments);
    if arguments.next().is_some() {
        usage();
    }
    let capture: KbuildProcessCapture = serde_json::from_slice(
        &std::fs::read(&summary_path)
            .with_context(|| format!("read {}", summary_path.display()))?,
    )
    .context("decode Kbuild process capture")?;
    let seal = certify_process_capture_input_witnesses(
        &capture.stream_path,
        capture.summary,
        process_capture_limits(&capture)?,
    )?;
    let stream = seal.stream();
    let census = seal.census();
    println!(
        "{}",
        serde_json::to_string(&json!({
            "stream_digest": hex::encode(stream.digest()),
            "stream_bytes": stream.stream_bytes(),
            "stream_records": stream.record_count(),
            "process_lifecycles": stream.process_count(),
            "file_mappings": census.file_mappings(),
            "witnessed_file_mappings": census.witnessed_file_mappings(),
            "regular_file_reads": census.regular_file_reads(),
            "witnessed_regular_file_reads": census.witnessed_regular_file_reads(),
            "directory_enumerations": census.directory_enumerations(),
            "witnessed_directory_enumerations":
                census.witnessed_directory_enumerations(),
        }))?
    );
    Ok(())
}

#[cfg(unix)]
fn decode_process_private(arguments: &mut impl Iterator<Item = OsString>) -> Result<()> {
    let summary_path = argument(arguments);
    let receipt_path = argument(arguments);
    if arguments.next().is_some() {
        usage();
    }
    if !summary_path.is_absolute()
        || !receipt_path.is_absolute()
        || summary_path == receipt_path
        || summary_path.parent() != receipt_path.parent()
    {
        anyhow::bail!("process summary and receipt must be distinct absolute sibling paths");
    }
    let capture: KbuildProcessCapture = serde_json::from_slice(
        &std::fs::read(&summary_path)
            .with_context(|| format!("read {}", summary_path.display()))?,
    )
    .context("decode Kbuild process capture")?;
    let mut output = private_output(&receipt_path)?;
    let (receipt, stream) = load_process_capture_stream(
        &capture.stream_path,
        capture.summary,
        process_capture_limits(&capture)?,
    )?;
    serde_json::to_writer(&mut output, &receipt)
        .with_context(|| format!("write {}", receipt_path.display()))?;
    output
        .write_all(b"\n")
        .with_context(|| format!("write {}", receipt_path.display()))?;
    finish_private_output(output, &receipt_path)?;
    println!(
        "{}",
        serde_json::to_string(&json!({
            "stream_digest": hex::encode(stream.digest()),
            "stream_bytes": stream.stream_bytes(),
            "stream_records": stream.record_count(),
            "process_lifecycles": stream.process_count(),
        }))?
    );
    Ok(())
}

#[cfg(unix)]
#[derive(Default)]
struct ProcessSliceVisitor {
    selected: BTreeSet<CapturedProcessId>,
    started: BTreeSet<CapturedProcessId>,
    finished: BTreeSet<CapturedProcessId>,
    stream_ordinal: u64,
    retained_bytes: u64,
    events: Vec<serde_json::Value>,
}

#[cfg(unix)]
impl ProcessSliceVisitor {
    fn new(selected: BTreeSet<CapturedProcessId>) -> Self {
        Self {
            selected,
            ..Self::default()
        }
    }

    fn retain(
        &mut self,
        kind: &str,
        process: CapturedProcessId,
        value: serde_json::Value,
    ) -> Result<()> {
        let event = json!({
            "stream_ordinal": self.stream_ordinal,
            "kind": kind,
            "process": process,
            "value": value,
        });
        let event_bytes = u64::try_from(serde_json::to_vec(&event)?.len())
            .context("private process-slice event size does not fit u64")?;
        self.retained_bytes = self
            .retained_bytes
            .checked_add(event_bytes)
            .context("private process-slice byte count overflowed")?;
        if self.events.len() >= PRIVATE_PROCESS_SLICE_MAX_EVENTS
            || self.retained_bytes > PRIVATE_PROCESS_SLICE_MAX_BYTES
        {
            anyhow::bail!("private process slice exceeds its retention budget");
        }
        self.events.push(event);
        Ok(())
    }
}

#[cfg(unix)]
impl ProcessCaptureStreamVisitor for ProcessSliceVisitor {
    fn visit(&mut self, event: ProcessCaptureStreamEvent<'_>) -> Result<()> {
        match event {
            ProcessCaptureStreamEvent::ProcessStarted { process, parent }
                if self.selected.contains(&process) =>
            {
                self.started.insert(process);
                self.retain("process_started", process, json!({ "parent": parent }))?;
            }
            ProcessCaptureStreamEvent::Invocation {
                process,
                invocation,
            } if self.selected.contains(&process) => {
                self.retain("invocation", process, json!(invocation))?;
            }
            ProcessCaptureStreamEvent::ProcessCreationAttempt { process, attempt }
                if matches!(
                    attempt.outcome,
                    CapturedProcessCreationOutcome::Created(child)
                        if self.selected.contains(&child)
                ) =>
            {
                self.retain("process_creation", process, json!(attempt))?;
            }
            ProcessCaptureStreamEvent::ChildWaitAttempt { process, attempt }
                if matches!(
                    attempt.request.selector,
                    CapturedChildWaitSelector::Child(child)
                        if self.selected.contains(&child)
                ) || matches!(
                    attempt.outcome,
                    CapturedChildWaitOutcome::Reaped { child, .. }
                        if self.selected.contains(&child)
                ) =>
            {
                self.retain("child_wait", process, json!(attempt))?;
            }
            ProcessCaptureStreamEvent::OpenAttempt { process, attempt }
                if self.selected.contains(&process) =>
            {
                self.retain("open", process, json!(attempt))?;
            }
            ProcessCaptureStreamEvent::PathRemovalAttempt { process, attempt }
                if self.selected.contains(&process) =>
            {
                self.retain("path_removal", process, json!(attempt))?;
            }
            ProcessCaptureStreamEvent::PathRenameAttempt { process, attempt }
                if self.selected.contains(&process) =>
            {
                self.retain("path_rename", process, json!(attempt))?;
            }
            ProcessCaptureStreamEvent::Getdents64Attempt { process, attempt }
                if self.selected.contains(&process) =>
            {
                self.retain("getdents64", process, json!(attempt))?;
            }
            ProcessCaptureStreamEvent::ProcessFinished { process, outcome }
                if self.selected.contains(&process) =>
            {
                self.finished.insert(process);
                self.retain("process_finished", process, json!(outcome))?;
            }
            _ => {}
        }
        self.stream_ordinal = self
            .stream_ordinal
            .checked_add(1)
            .context("private process-slice stream ordinal overflowed")?;
        Ok(())
    }
}

#[cfg(unix)]
fn slice_processes_private(arguments: &mut impl Iterator<Item = OsString>) -> Result<()> {
    let summary_path = argument(arguments);
    let slice_path = argument(arguments);
    let selected = arguments
        .map(|argument| {
            argument
                .to_str()
                .context("process identity is not UTF-8")?
                .parse::<CapturedProcessId>()
                .context("process identity is not an unsigned integer")
        })
        .collect::<Result<BTreeSet<_>>>()?;
    if selected.is_empty()
        || selected.len() > PRIVATE_PROCESS_SLICE_MAX_PROCESSES
        || selected.contains(&0)
        || !summary_path.is_absolute()
        || !slice_path.is_absolute()
        || summary_path == slice_path
        || summary_path.parent() != slice_path.parent()
    {
        usage();
    }
    let capture: KbuildProcessCapture = serde_json::from_slice(
        &std::fs::read(&summary_path)
            .with_context(|| format!("read {}", summary_path.display()))?,
    )
    .context("decode Kbuild process capture")?;
    let (visitor, stream) = visit_process_capture_stream(
        &capture.stream_path,
        capture.summary,
        process_capture_limits(&capture)?,
        ProcessSliceVisitor::new(selected.clone()),
    )?;
    if visitor.stream_ordinal != stream.record_count()
        || visitor.started != selected
        || visitor.finished != selected
    {
        anyhow::bail!("private process slice has incomplete selected lifecycles");
    }
    let mut output = private_output(&slice_path)?;
    serde_json::to_writer_pretty(
        &mut output,
        &json!({
            "stream_digest": hex::encode(stream.digest()),
            "stream_records": stream.record_count(),
            "selected_processes": selected,
            "events": visitor.events,
        }),
    )
    .with_context(|| format!("write {}", slice_path.display()))?;
    output
        .write_all(b"\n")
        .with_context(|| format!("write {}", slice_path.display()))?;
    finish_private_output(output, &slice_path)?;
    println!(
        "{}",
        serde_json::to_string(&json!({
            "stream_digest": hex::encode(stream.digest()),
            "stream_records": stream.record_count(),
            "selected_processes": visitor.started.len(),
            "retained_events": visitor.events.len(),
        }))?
    );
    Ok(())
}

fn cache(cache_root: &Path) -> Result<(Arc<ActionCache>, Arc<CasStore>)> {
    let actions = ActionCache::with_disk(CacheConfig {
        cache_root: cache_root.join("actions"),
        max_size: 0,
        ttl_seconds: 0,
    })?;
    Ok((Arc::new(actions), Arc::new(CasStore::new(cache_root)?)))
}

async fn replay(
    snapshot_path: &Path,
    key_path: &Path,
    cache_root: &Path,
    commit_clean_result: bool,
    force_clean: bool,
) -> Result<ExternalReplayStatus> {
    let diagnostics = std::env::var("RUSH_EXTERNAL_OBSERVATION_STATS").as_deref() == Ok("1");
    let total_started = Instant::now();
    let admission_started = Instant::now();
    let key: ExternalAnalysisKey = serde_json::from_slice(
        &std::fs::read(key_path).with_context(|| format!("read {}", key_path.display()))?,
    )
    .context("decode external analysis key")?;
    let (snapshot, continuation) =
        match load_if_fresh(snapshot_path, &key, &CommandSemantics::from_process()) {
            ExternalSnapshotAdmission::Hit(snapshot) => (snapshot, false),
            ExternalSnapshotAdmission::ContinuationRequired(snapshot) => (snapshot, true),
            admission => {
                let decision = admission.decision().as_str();
                let reason = admission.reason().unwrap_or("continuation pending");
                anyhow::bail!("external snapshot admission {decision}: {reason}")
            }
        };
    let admission_elapsed = admission_started.elapsed();
    let cache_started = Instant::now();
    let (cache, cas) = cache(cache_root)?;
    let cache_elapsed = cache_started.elapsed();
    let output_root = snapshot.key.output_root.clone();
    let clean_started = Instant::now();
    let clean = begin_replay_tree(&output_root, continuation, force_clean)?;
    let owned_scratch = snapshot.graph.initial_inputs.iter().any(|input| {
        input.path.root == "obj"
            && input.path.path == KBUILD_MANAGED_SCRATCH_DIRECTORY.as_bytes()
            && matches!(input.value, ExternalInitialInputValue::Directory { mode }
                if mode == KBUILD_MANAGED_SCRATCH_MODE)
    });
    let mut scratch = if clean.is_none() {
        ManagedReplayScratch::begin(&output_root, owned_scratch)?
    } else {
        None
    };
    let clean_elapsed = clean_started.elapsed();
    let engine_started = Instant::now();
    let replay = ExternalReplayEngine::with_cas(snapshot, cache, ResourceManager::new()?, cas)?;
    if let Some(scratch) = &mut scratch {
        scratch.observe_materialization()?;
    }
    let engine_elapsed = engine_started.elapsed();
    let replay_started = Instant::now();
    let status = replay.replay().await;
    let replay_elapsed = replay_started.elapsed();
    let rollback_started = Instant::now();
    if matches!(status, Ok(ExternalReplayStatus::Complete { .. })) {
        if let Some(scratch) = scratch {
            scratch.finish()?;
        }
    }
    if let Some(clean) = clean {
        if commit_clean_result
            && matches!(status, Ok(ExternalReplayStatus::Complete { .. }))
        {
            clean.commit()?;
        } else {
            clean.rollback()?;
        }
    }
    let rollback_elapsed = rollback_started.elapsed();
    if diagnostics {
        eprintln!(
            "external replay gate: admission_us={} cache_us={} clean_us={} engine_us={} replay_us={} rollback_us={} total_us={}",
            admission_elapsed.as_micros(),
            cache_elapsed.as_micros(),
            clean_elapsed.as_micros(),
            engine_elapsed.as_micros(),
            replay_elapsed.as_micros(),
            rollback_elapsed.as_micros(),
            total_started.elapsed().as_micros(),
        );
    }
    status
}

#[cfg(unix)]
fn begin_replay_tree(
    output: &Path,
    continuation: bool,
    force_clean: bool,
) -> Result<Option<CleanReplayTree>> {
    (continuation || force_clean)
        .then(|| CleanReplayTree::begin(output))
        .transpose()
}

/// Own only a newly materialized, snapshot-declared managed scratch directory.
/// Pre-existing directories belong to the caller; unexpected shapes or writes
/// refuse cleanup rather than deleting arbitrary output-tree contents.
#[cfg(unix)]
struct ManagedReplayScratch {
    path: PathBuf,
    identity: Option<(u64, u64, u32, u32, u32)>,
}

#[cfg(unix)]
impl ManagedReplayScratch {
    fn begin(output: &Path, owned_by_snapshot: bool) -> Result<Option<Self>> {
        if !owned_by_snapshot {
            return Ok(None);
        }
        let path = output.join(KBUILD_MANAGED_SCRATCH_DIRECTORY);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_dir() => Ok(None),
            Ok(_) => anyhow::bail!("pre-existing managed replay scratch is not a directory"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(Some(Self { path, identity: None }))
            }
            Err(error) => Err(error).context("inspect managed replay scratch"),
        }
    }

    fn current_identity(&self) -> Result<(u64, u64, u32, u32, u32)> {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::symlink_metadata(&self.path)
            .context("inspect materialized managed replay scratch")?;
        if !metadata.file_type().is_dir() || metadata.mode() & 0o7777 != KBUILD_MANAGED_SCRATCH_MODE
        {
            anyhow::bail!("materialized managed replay scratch has an unexpected shape or mode");
        }
        Ok((
            metadata.dev(),
            metadata.ino(),
            metadata.uid(),
            metadata.gid(),
            metadata.mode(),
        ))
    }

    fn observe_materialization(&mut self) -> Result<()> {
        self.identity = Some(self.current_identity()?);
        Ok(())
    }

    fn finish(self) -> Result<()> {
        if self.identity.is_none() || Some(self.current_identity()?) != self.identity {
            anyhow::bail!("managed replay scratch identity changed before cleanup");
        }
        // rmdir refuses unexpected contents, including files left by a writer.
        // This is lifecycle cleanup, never a parity exclusion.
        std::fs::remove_dir(&self.path).context("remove new empty managed replay scratch")
    }
}

#[cfg(unix)]
struct CleanReplayTree {
    output: PathBuf,
    backup: PathBuf,
    finished: bool,
}

#[cfg(unix)]
impl CleanReplayTree {
    fn begin(output: &Path) -> Result<Self> {
        let parent = output.parent().context("replay output has no parent")?;
        let mut backup_name = output
            .file_name()
            .context("replay output has no basename")?
            .to_os_string();
        backup_name.push(format!(".rush-replay-backup-{}", std::process::id()));
        let backup = parent.join(backup_name);
        if backup.exists() {
            anyhow::bail!("clean replay backup already exists");
        }
        let permissions = std::fs::metadata(output)
            .context("inspect replay output tree")?
            .permissions();
        std::fs::rename(output, &backup).context("stash replay output tree")?;
        let initialize = || -> std::io::Result<()> {
            std::fs::create_dir(output)?;
            std::fs::set_permissions(output, permissions)?;

            // Match `make clean`: generated objects disappear, but the kernel
            // configuration remains an external input to the clean build.
            for seed in [".config", ".gitignore"] {
                let saved = backup.join(seed);
                if saved.is_file() {
                    std::fs::copy(saved, output.join(seed))?;
                }
            }
            Ok(())
        };
        if let Err(error) = initialize() {
            let _ = std::fs::remove_dir_all(output);
            let _ = std::fs::rename(&backup, output);
            return Err(error).context("create clean replay output tree");
        }
        Ok(Self {
            output: output.to_path_buf(),
            backup,
            finished: false,
        })
    }

    fn rollback(mut self) -> Result<()> {
        self.restore()?;
        self.finished = true;
        Ok(())
    }

    fn commit(mut self) -> Result<()> {
        let private_state = self.output.join(".rush-tmp");
        match std::fs::symlink_metadata(&private_state) {
            Ok(metadata) if metadata.file_type().is_dir() => {
                std::fs::remove_dir_all(&private_state)
                    .context("remove committed replay private state")?;
            }
            Ok(_) => anyhow::bail!("committed replay private state is not a directory"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).context("inspect committed replay private state");
            }
        }
        std::fs::remove_dir_all(&self.backup).context("discard pre-replay output tree")?;
        self.finished = true;
        Ok(())
    }

    fn restore(&self) -> Result<()> {
        let mut discarded_name = self
            .output
            .file_name()
            .context("replay output has no basename")?
            .to_os_string();
        discarded_name.push(format!(".rush-replay-discard-{}", std::process::id()));
        let discarded = self
            .output
            .parent()
            .context("replay output has no parent")?
            .join(discarded_name);
        if discarded.exists() {
            anyhow::bail!("clean replay discard path already exists");
        }
        std::fs::rename(&self.output, &discarded).context("stash clean replay result")?;
        if let Err(error) = std::fs::rename(&self.backup, &self.output) {
            let _ = std::fs::rename(&discarded, &self.output);
            return Err(error).context("restore replay output tree");
        }
        std::fs::remove_dir_all(&discarded).context("discard clean replay result")?;
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for CleanReplayTree {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.restore();
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut arguments = std::env::args_os().skip(1);
    let mode = arguments.next().unwrap_or_else(|| usage());
    if mode == OsStr::new("observe-process") {
        #[cfg(not(unix))]
        anyhow::bail!("process capture is unsupported on this platform");
        #[cfg(unix)]
        return observe_process(&mut arguments);
    }
    if mode == OsStr::new("managed-observe-process") {
        #[cfg(not(unix))]
        anyhow::bail!("managed process capture is unsupported on this platform");
        #[cfg(unix)]
        return managed_observe_process(&mut arguments).await;
    }
    if mode == OsStr::new("certify-process") {
        #[cfg(not(unix))]
        anyhow::bail!("process capture is unsupported on this platform");
        #[cfg(unix)]
        return certify_process(&mut arguments);
    }
    if mode == OsStr::new("decode-process-private") {
        #[cfg(not(unix))]
        anyhow::bail!("process capture is unsupported on this platform");
        #[cfg(unix)]
        return decode_process_private(&mut arguments);
    }
    if mode == OsStr::new("slice-processes-private") {
        #[cfg(not(unix))]
        anyhow::bail!("process capture is unsupported on this platform");
        #[cfg(unix)]
        return slice_processes_private(&mut arguments);
    }
    if mode == OsStr::new("diagnose-union-private") {
        #[cfg(not(unix))]
        anyhow::bail!("native Kbuild union diagnosis is unsupported on this platform");
        #[cfg(unix)]
        return diagnose_union_private(&mut arguments);
    }
    if mode == OsStr::new("census-credentials-private") {
        #[cfg(not(unix))]
        anyhow::bail!("native Kbuild credential census is unsupported on this platform");
        #[cfg(unix)]
        return census_credentials_private(&mut arguments);
    }
    if mode == OsStr::new("derive-graph") {
        #[cfg(not(unix))]
        anyhow::bail!("native Kbuild graph derivation is unsupported on this platform");
        #[cfg(unix)]
        return derive_graph(&mut arguments);
    }
    if mode == OsStr::new("census-components") {
        #[cfg(not(unix))]
        anyhow::bail!("native Kbuild component census is unsupported on this platform");
        #[cfg(unix)]
        {
            let summary_path = argument(&mut arguments);
            let output_root = std::fs::canonicalize(argument(&mut arguments))
                .context("canonicalize native Kbuild census output root")?;
            if arguments.next().is_some() || !summary_path.is_absolute() {
                usage();
            }
            let process_capture: KbuildProcessCapture = serde_json::from_slice(
                &std::fs::read(&summary_path)
                    .with_context(|| format!("read {}", summary_path.display()))?,
            )
            .context("decode native Kbuild process summary")?;
            let census = census_kbuild_native_components(&process_capture, &output_root)?;
            println!("{}", serde_json::to_string(&census)?);
            return Ok(());
        }
    }
    if mode == OsStr::new("census-procfs") {
        #[cfg(not(unix))]
        anyhow::bail!("Kbuild procfs census is unsupported on this platform");
        #[cfg(unix)]
        {
            let envelope_path = argument(&mut arguments);
            let source_root = argument(&mut arguments);
            let output_root = argument(&mut arguments);
            if arguments.next().is_some() {
                usage();
            }
            let envelope: KbuildActionGraphEnvelope = serde_json::from_slice(
                &std::fs::read(&envelope_path)
                    .with_context(|| format!("read {}", envelope_path.display()))?,
            )
            .context("decode observed Kbuild action graph envelope")?;
            let census = census_kbuild_procfs_reads(envelope, &source_root, &output_root)?;
            println!("{}", serde_json::to_string(&census)?);
            return Ok(());
        }
    }
    if mode == OsStr::new("census-procfs-directories") {
        #[cfg(not(unix))]
        anyhow::bail!("Kbuild procfs directory census is unsupported on this platform");
        #[cfg(unix)]
        {
            let envelope_path = argument(&mut arguments);
            let source_root = argument(&mut arguments);
            let output_root = argument(&mut arguments);
            if arguments.next().is_some() {
                usage();
            }
            let envelope: KbuildActionGraphEnvelope = serde_json::from_slice(
                &std::fs::read(&envelope_path)
                    .with_context(|| format!("read {}", envelope_path.display()))?,
            )
            .context("decode observed Kbuild action graph envelope")?;
            let census = census_kbuild_procfs_directories(envelope, &source_root, &output_root)?;
            println!("{}", serde_json::to_string(&census)?);
            return Ok(());
        }
    }
    if mode == OsStr::new("census-generated-inputs") {
        #[cfg(not(unix))]
        anyhow::bail!("Kbuild generated-input census is unsupported on this platform");
        #[cfg(unix)]
        {
            let envelope_path = argument(&mut arguments);
            let source_root = argument(&mut arguments);
            let output_root = argument(&mut arguments);
            if arguments.next().is_some() {
                usage();
            }
            let envelope: KbuildActionGraphEnvelope = serde_json::from_slice(
                &std::fs::read(&envelope_path)
                    .with_context(|| format!("read {}", envelope_path.display()))?,
            )
            .context("decode observed Kbuild action graph envelope")?;
            let census = census_kbuild_generated_inputs(envelope, &source_root, &output_root)?;
            println!("{}", serde_json::to_string(&census)?);
            return Ok(());
        }
    }
    if mode == OsStr::new("preflight-graph") {
        #[cfg(not(unix))]
        anyhow::bail!("Kbuild graph preflight is unsupported on this platform");
        #[cfg(unix)]
        {
            let envelope_path = argument(&mut arguments);
            let source_root = argument(&mut arguments);
            let output_root = argument(&mut arguments);
            if arguments.next().is_some() {
                usage();
            }
            let envelope: KbuildActionGraphEnvelope = serde_json::from_slice(
                &std::fs::read(&envelope_path)
                    .with_context(|| format!("read {}", envelope_path.display()))?,
            )
            .context("decode observed Kbuild action graph envelope")?;
            let preflight = preflight_kbuild_graph_envelope(envelope, &source_root, &output_root)?;
            println!("{}", serde_json::to_string(&preflight)?);
            return Ok(());
        }
    }
    if mode == OsStr::new("assemble-graph") {
        #[cfg(not(unix))]
        anyhow::bail!("Kbuild graph assembly is unsupported on this platform");
        #[cfg(unix)]
        {
            let envelope_path = argument(&mut arguments);
            let source_root = argument(&mut arguments);
            let output_root = argument(&mut arguments);
            let snapshot_path = argument(&mut arguments);
            let key_path = argument(&mut arguments);
            if arguments.next().is_some() {
                usage();
            }
            let envelope: KbuildActionGraphEnvelope = serde_json::from_slice(
                &std::fs::read(&envelope_path)
                    .with_context(|| format!("read {}", envelope_path.display()))?,
            )
            .context("decode observed Kbuild action graph envelope")?;
            let snapshot =
                snapshot_from_kbuild_graph_envelope(envelope, &source_root, &output_root)?;
            persist(&snapshot_path, &snapshot)?;
            std::fs::write(&key_path, serde_json::to_vec_pretty(&snapshot.key)?)
                .with_context(|| format!("write {}", key_path.display()))?;
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "actions": snapshot.graph.actions.len(),
                    "analysis_barriers": snapshot.graph.analysis_barriers.len(),
                }))?
            );
            return Ok(());
        }
    }
    if mode == OsStr::new("preflight-managed-graph") {
        #[cfg(not(unix))]
        anyhow::bail!("managed Kbuild graph preflight is unsupported on this platform");
        #[cfg(unix)]
        {
            let envelope_path = argument(&mut arguments);
            let transaction_path = argument(&mut arguments);
            let source_root = argument(&mut arguments);
            let output_root = argument(&mut arguments);
            if arguments.next().is_some() {
                usage();
            }
            let envelope: KbuildActionGraphEnvelope = serde_json::from_slice(
                &std::fs::read(&envelope_path)
                    .with_context(|| format!("read {}", envelope_path.display()))?,
            )
            .context("decode managed Kbuild action graph envelope")?;
            let transaction: ExternalCaptureTransactionIdentity = serde_json::from_slice(
                &std::fs::read(&transaction_path)
                    .with_context(|| format!("read {}", transaction_path.display()))?,
            )
            .context("decode managed Kbuild capture transaction")?;
            let seed_path = transaction_path.with_extension("seed.json");
            let managed_seed: KbuildManagedCaptureSeed = serde_json::from_slice(
                &std::fs::read(&seed_path)
                    .with_context(|| format!("read {}", seed_path.display()))?,
            )
            .context("decode managed Kbuild capture seed")?;
            let preflight = preflight_managed_kbuild_graph_envelope_with_seed(
                envelope,
                &source_root,
                &output_root,
                transaction,
                managed_seed,
            )?;
            println!("{}", serde_json::to_string(&preflight)?);
            return Ok(());
        }
    }
    if mode == OsStr::new("assemble-managed-graph") {
        #[cfg(not(unix))]
        anyhow::bail!("managed Kbuild graph assembly is unsupported on this platform");
        #[cfg(unix)]
        {
            let envelope_path = argument(&mut arguments);
            let transaction_path = argument(&mut arguments);
            let source_root = argument(&mut arguments);
            let output_root = argument(&mut arguments);
            let snapshot_path = argument(&mut arguments);
            let key_path = argument(&mut arguments);
            if arguments.next().is_some() {
                usage();
            }
            let envelope: KbuildActionGraphEnvelope = serde_json::from_slice(
                &std::fs::read(&envelope_path)
                    .with_context(|| format!("read {}", envelope_path.display()))?,
            )
            .context("decode managed Kbuild action graph envelope")?;
            let transaction: ExternalCaptureTransactionIdentity = serde_json::from_slice(
                &std::fs::read(&transaction_path)
                    .with_context(|| format!("read {}", transaction_path.display()))?,
            )
            .context("decode managed Kbuild capture transaction")?;
            let seed_path = transaction_path.with_extension("seed.json");
            let managed_seed: KbuildManagedCaptureSeed = serde_json::from_slice(
                &std::fs::read(&seed_path)
                    .with_context(|| format!("read {}", seed_path.display()))?,
            )
            .context("decode managed Kbuild capture seed")?;
            let snapshot = snapshot_from_managed_kbuild_graph_envelope_with_seed(
                envelope,
                &source_root,
                &output_root,
                transaction,
                managed_seed,
            )?;
            persist(&snapshot_path, &snapshot)?;
            std::fs::write(&key_path, serde_json::to_vec_pretty(&snapshot.key)?)
                .with_context(|| format!("write {}", key_path.display()))?;
            println!(
                "{}",
                serde_json::to_string(&json!({
                    "actions": snapshot.graph.actions.len(),
                    "analysis_barriers": snapshot.graph.analysis_barriers.len(),
                }))?
            );
            return Ok(());
        }
    }
    if mode == OsStr::new("inspect-snapshot-private") {
        let snapshot_path = argument(&mut arguments);
        let needle = arguments.next().unwrap_or_else(|| usage());
        if arguments.next().is_some() || !snapshot_path.is_absolute() {
            usage();
        }
        #[cfg(not(unix))]
        anyhow::bail!("private snapshot inspection is unsupported on this platform");
        #[cfg(unix)]
        {
            let needle = needle.as_os_str().as_bytes();
            if needle.is_empty() {
                usage();
            }
            let snapshot = rush_build::external_analysis_snapshot::decode_serialized_snapshot(
                &std::fs::read(&snapshot_path)
                    .with_context(|| format!("read {}", snapshot_path.display()))?,
            )
            .context("decode external analysis snapshot")?;
            let actions = snapshot
                .graph
                .actions
                .iter()
                .filter(|action| {
                    action.logical_id.as_bytes().windows(needle.len()).any(|part| part == needle)
                        || action.outputs.iter().any(|output| {
                            output.path.path.windows(needle.len()).any(|part| part == needle)
                        })
                })
                .map(|action| json!({
                    "logical_id": action.logical_id,
                    "mnemonic": action.mnemonic,
                    "normal_inputs": action.normal_inputs,
                    "order_only_inputs": action.order_only_inputs,
                    "outputs": action.outputs,
                    "dependencies": action.dependencies,
                    "commands": action.commands.iter().map(|command| json!({
                        "executable": command.executable,
                        "argv0": command.argv0,
                        "argv": command.argv,
                        "environment": command.environment.as_ref().map(|environment| {
                            environment.iter().map(|(name, value)| json!({
                                "name": name,
                                "value": value,
                            })).collect::<Vec<_>>()
                        }),
                        "working_directory": command.working_directory,
                    })).collect::<Vec<_>>(),
                    "environment": action.environment.iter().map(|(name, value)| json!({
                        "name": name,
                        "value": value,
                    })).collect::<Vec<_>>(),
                    "always_run": action.always_run,
                    "cacheable": action.cacheable,
                    "local_only": action.local_only,
                    "sandbox": action.sandbox,
                    "execution_properties": action.execution_properties,
                }))
                .collect::<Vec<_>>();
            println!("{}", serde_json::to_string_pretty(&json!({
                "actions": actions,
                "requested_outputs": snapshot.graph.requested_outputs,
                "analysis_barriers": snapshot.graph.analysis_barriers.iter().filter(|barrier| {
                    barrier.producer_closure.iter().chain(&barrier.blocked_actions).any(|action| {
                        action.as_bytes().windows(needle.len()).any(|part| part == needle)
                    }) || barrier.expected_inputs.iter().any(|input| {
                        input.path.path.windows(needle.len()).any(|part| part == needle)
                    })
                }).collect::<Vec<_>>(),
            }))?);
            return Ok(());
        }
    }
    let force_clean = mode == OsStr::new("replay-clean-commit");
    let commit_clean_result = mode == OsStr::new("replay-commit") || force_clean;
    let (snapshot_path, key_path, cache_root) = match mode.to_str() {
        Some("capture") => {
            let capture_path = argument(&mut arguments);
            let source_root = argument(&mut arguments);
            let output_root = argument(&mut arguments);
            let signal_state = ExternalSignalState {
                blocked: signal_mask(&mut arguments, "blocked signal mask"),
                ignored: signal_mask(&mut arguments, "ignored signal mask"),
            };
            signal_state.validate()?;
            let file_creation_mask = file_creation_mask(&mut arguments);
            let credential_path = argument(&mut arguments);
            let credentials: ExternalCredentialState = serde_json::from_slice(
                &std::fs::read(&credential_path)
                    .with_context(|| format!("read {}", credential_path.display()))?,
            )
            .context("decode captured credential state")?;
            credentials.validate()?;
            let snapshot_path = argument(&mut arguments);
            let key_path = argument(&mut arguments);
            let cache_root = argument(&mut arguments);
            if arguments.next().is_some() {
                usage();
            }
            let capture_bytes = std::fs::read(&capture_path)
                .with_context(|| format!("read {}", capture_path.display()))?;
            let capture: KbuildActionCapture =
                serde_json::from_slice(&capture_bytes).context("decode observed Kbuild action")?;
            let snapshot = snapshot_from_kbuild_capture(
                capture,
                &source_root,
                &output_root,
                signal_state,
                file_creation_mask,
                credentials,
            )?;
            persist(&snapshot_path, &snapshot)?;
            std::fs::write(&key_path, serde_json::to_vec_pretty(&snapshot.key)?)
                .with_context(|| format!("write {}", key_path.display()))?;
            (snapshot_path, key_path, cache_root)
        }
        Some("replay") | Some("replay-commit") | Some("replay-clean-commit") => {
            let snapshot_path = argument(&mut arguments);
            let key_path = argument(&mut arguments);
            let cache_root = argument(&mut arguments);
            if arguments.next().is_some() {
                usage();
            }
            (snapshot_path, key_path, cache_root)
        }
        _ => usage(),
    };

    let status = replay(
        &snapshot_path,
        &key_path,
        &cache_root,
        commit_clean_result,
        force_clean,
    )
    .await?;
    let value = match status {
        ExternalReplayStatus::Complete {
            cache_hits,
            executed,
        } => json!({
            "status": "complete",
            "cache_hits": cache_hits,
            "executed": executed,
        }),
        ExternalReplayStatus::AnalysisMismatch {
            barrier_index,
            cache_hits,
            executed,
        } => json!({
            "status": "analysis_mismatch",
            "barrier_index": barrier_index,
            "cache_hits": cache_hits,
            "executed": executed,
        }),
    };
    println!("{}", serde_json::to_string(&value)?);
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn managed_capture_pins_scratch_below_the_output_root() {
        let output_root = Path::new("/managed-output");
        let environment = vec![
            (OsString::from("Z"), OsString::from("last")),
            (OsString::from("TMPDIR"), OsString::from("/outside-one")),
            (OsString::from("A"), OsString::from("first")),
            (OsString::from("TMPDIR"), OsString::from("/outside-two")),
        ];

        let environment = managed_capture_environment(output_root, 1_790_154_919, environment)
            .expect("managed environment");

        assert_eq!(
            environment,
            vec![
                (OsString::from("A"), OsString::from("first")),
                (
                    OsString::from(KBUILD_BUILD_TIMESTAMP_VARIABLE),
                    OsString::from("@1790154919"),
                ),
                (
                    OsString::from("TMPDIR"),
                    output_root
                        .join(KBUILD_MANAGED_SCRATCH_DIRECTORY)
                        .into_os_string(),
                ),
                (OsString::from("Z"), OsString::from("last")),
            ]
        );
    }

    #[test]
    fn managed_capture_preserves_explicit_kbuild_timestamp() {
        let environment = managed_capture_environment(
            Path::new("/managed-output"),
            1_790_154_919,
            vec![(
                OsString::from(KBUILD_BUILD_TIMESTAMP_VARIABLE),
                OsString::from("explicit timestamp"),
            )],
        )
        .expect("managed environment");

        assert!(environment.contains(&(
            OsString::from(KBUILD_BUILD_TIMESTAMP_VARIABLE),
            OsString::from("explicit timestamp"),
        )));
        assert!(!environment.contains(&(
            OsString::from(KBUILD_BUILD_TIMESTAMP_VARIABLE),
            OsString::from("@1790154919"),
        )));
    }

    #[test]
    fn managed_capture_uses_source_date_epoch() {
        let environment = managed_capture_environment(
            Path::new("/managed-output"),
            1_790_154_919,
            vec![(
                OsString::from(SOURCE_DATE_EPOCH_VARIABLE),
                OsString::from("42"),
            )],
        )
        .expect("managed environment");

        assert!(environment.contains(&(
            OsString::from(KBUILD_BUILD_TIMESTAMP_VARIABLE),
            OsString::from("@42"),
        )));
    }

    #[test]
    fn managed_capture_rejects_invalid_source_date_epoch() {
        let error = managed_capture_environment(
            Path::new("/managed-output"),
            1_790_154_919,
            vec![(
                OsString::from(SOURCE_DATE_EPOCH_VARIABLE),
                OsString::from("not-an-epoch"),
            )],
        )
        .expect_err("invalid epoch must fail closed");

        assert!(error
            .to_string()
            .contains("SOURCE_DATE_EPOCH is not an unsigned integer"));
    }

    #[test]
    fn forced_clean_replay_starts_empty_without_analysis_barriers() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("obj");
        std::fs::create_dir(&output).unwrap();
        std::fs::write(output.join(".config"), b"seed").unwrap();
        std::fs::write(output.join("old"), b"old").unwrap();
        assert!(begin_replay_tree(&output, false, false).unwrap().is_none());
        let clean = begin_replay_tree(&output, false, true).unwrap().unwrap();
        assert_eq!(std::fs::read(output.join(".config")).unwrap(), b"seed");
        assert!(!output.join("old").exists());
        std::fs::write(output.join("result"), b"new").unwrap();
        clean.commit().unwrap();
        assert_eq!(std::fs::read(output.join("result")).unwrap(), b"new");
        assert!(!output.join("old").exists());
    }

    #[test]
    fn forced_clean_replay_error_restores_preexisting_tree() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("obj");
        std::fs::create_dir(&output).unwrap();
        std::fs::write(output.join("old"), b"old").unwrap();
        let clean = begin_replay_tree(&output, false, true).unwrap().unwrap();
        std::fs::write(
            output.join(KBUILD_MANAGED_SCRATCH_DIRECTORY),
            b"unexpected shape",
        )
        .unwrap();
        assert!(clean.commit().is_err());
        assert_eq!(std::fs::read(output.join("old")).unwrap(), b"old");
        assert!(!output.join(KBUILD_MANAGED_SCRATCH_DIRECTORY).exists());
    }

    #[test]
    fn managed_replay_scratch_cleans_only_new_owned_empty_directory() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path();
        let path = output.join(KBUILD_MANAGED_SCRATCH_DIRECTORY);
        assert!(ManagedReplayScratch::begin(output, false)
            .unwrap()
            .is_none());
        let mut scratch = ManagedReplayScratch::begin(output, true).unwrap().unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(
            &path,
            std::fs::Permissions::from_mode(KBUILD_MANAGED_SCRATCH_MODE),
        )
        .unwrap();
        scratch.observe_materialization().unwrap();
        scratch.finish().unwrap();
        assert!(!path.exists());

        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("preexisting"), b"owned by caller").unwrap();
        assert!(ManagedReplayScratch::begin(output, true).unwrap().is_none());
        assert_eq!(
            std::fs::read(path.join("preexisting")).unwrap(),
            b"owned by caller"
        );
    }

    #[test]
    fn managed_replay_scratch_refuses_unexpected_contents_or_replacement() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path();
        let path = output.join(KBUILD_MANAGED_SCRATCH_DIRECTORY);
        let mut scratch = ManagedReplayScratch::begin(output, true).unwrap().unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(
            &path,
            std::fs::Permissions::from_mode(KBUILD_MANAGED_SCRATCH_MODE),
        )
        .unwrap();
        scratch.observe_materialization().unwrap();
        std::fs::write(path.join("unexpected"), b"keep").unwrap();
        assert!(scratch.finish().is_err());
        assert_eq!(std::fs::read(path.join("unexpected")).unwrap(), b"keep");
        std::fs::remove_file(path.join("unexpected")).unwrap();
        std::fs::remove_dir(&path).unwrap();

        let mut scratch = ManagedReplayScratch::begin(output, true).unwrap().unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(
            &path,
            std::fs::Permissions::from_mode(KBUILD_MANAGED_SCRATCH_MODE),
        )
        .unwrap();
        scratch.observe_materialization().unwrap();
        let saved = output.join("saved-scratch");
        std::fs::rename(&path, &saved).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(
            &path,
            std::fs::Permissions::from_mode(KBUILD_MANAGED_SCRATCH_MODE),
        )
        .unwrap();
        assert!(scratch.finish().is_err());
        assert!(path.is_dir() && saved.is_dir());
        std::fs::remove_dir(&path).unwrap();
        symlink(&saved, &path).unwrap();
        assert!(ManagedReplayScratch::begin(output, true).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"keep file").unwrap();
        assert!(ManagedReplayScratch::begin(output, true).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"keep file");
    }

    #[test]
    fn clean_replay_commit_keeps_the_completed_tree() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("obj");
        std::fs::create_dir(&output).unwrap();
        std::fs::write(output.join("old"), b"old").unwrap();

        let clean = CleanReplayTree::begin(&output).unwrap();
        std::fs::write(output.join("vmlinux"), b"new").unwrap();
        std::fs::create_dir(output.join(".rush-tmp")).unwrap();
        std::fs::write(output.join(".rush-tmp/private"), b"private").unwrap();
        clean.commit().unwrap();

        assert!(!output.join("old").exists());
        assert!(!output.join(".rush-tmp").exists());
        assert_eq!(std::fs::read(output.join("vmlinux")).unwrap(), b"new");
    }

    #[test]
    fn clean_replay_rollback_restores_the_previous_tree() {
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("obj");
        std::fs::create_dir(&output).unwrap();
        std::fs::write(output.join("old"), b"old").unwrap();

        let clean = CleanReplayTree::begin(&output).unwrap();
        std::fs::write(output.join("vmlinux"), b"new").unwrap();
        clean.rollback().unwrap();

        assert_eq!(std::fs::read(output.join("old")).unwrap(), b"old");
        assert!(!output.join("vmlinux").exists());
    }
}
