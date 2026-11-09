//! Native process-stream to Kbuild graph adapter.

use super::*;
use rush_observe::{
    load_process_capture_stream, visit_process_capture_stream, CapturedProcess,
    ProcessCaptureReceipt, ProcessCaptureStreamEvent, ProcessCaptureStreamSeal,
    ProcessCaptureStreamVisitor,
};

/// Sanitized partition of native mutation components before graph admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct KbuildNativeComponentCensus {
    stream_digest: [u8; 32],
    census_digest: [u8; 32],
    mutation_components: u64,
    candidate_path_references: u64,
    unique_candidate_paths: u64,
    multiply_owned_candidate_paths: u64,
    maximum_path_owners: u64,
    receiptless_components: u64,
    single_receipt_components: u64,
    ambiguous_receipt_components: u64,
    transient_components: u64,
    retained_regular_only_components: u64,
    retained_nonregular_components: u64,
    retained_regular_files: u64,
    retained_directories: u64,
    retained_symlinks: u64,
    retained_special_entries: u64,
    missing_paths: u64,
}

impl KbuildNativeComponentCensus {
    /// Exact sealed process stream supplying the mutation candidates.
    pub fn stream_digest(&self) -> [u8; 32] {
        self.stream_digest
    }

    /// Domain-separated identity of the complete component partition.
    pub fn census_digest(&self) -> [u8; 32] {
        self.census_digest
    }

    /// Process components that own at least one mutation candidate.
    pub fn mutation_components(&self) -> u64 {
        self.mutation_components
    }

    /// Candidate-path references summed across all components.
    pub fn candidate_path_references(&self) -> u64 {
        self.candidate_path_references
    }

    /// Distinct candidate paths across all components.
    pub fn unique_candidate_paths(&self) -> u64 {
        self.unique_candidate_paths
    }

    /// Distinct candidate paths referenced by more than one component.
    pub fn multiply_owned_candidate_paths(&self) -> u64 {
        self.multiply_owned_candidate_paths
    }

    /// Largest number of component references to one candidate path.
    pub fn maximum_path_owners(&self) -> u64 {
        self.maximum_path_owners
    }

    /// Components with no retained regular-file command receipt.
    pub fn receiptless_components(&self) -> u64 {
        self.receiptless_components
    }

    /// Components with exactly one retained regular-file command receipt.
    pub fn single_receipt_components(&self) -> u64 {
        self.single_receipt_components
    }

    /// Components with more than one retained regular-file command receipt.
    pub fn ambiguous_receipt_components(&self) -> u64 {
        self.ambiguous_receipt_components
    }

    /// Components whose candidate paths are all absent.
    pub fn transient_components(&self) -> u64 {
        self.transient_components
    }

    /// Components retaining regular files but no non-regular entries.
    pub fn retained_regular_only_components(&self) -> u64 {
        self.retained_regular_only_components
    }

    /// Components retaining at least one non-regular entry.
    pub fn retained_nonregular_components(&self) -> u64 {
        self.retained_nonregular_components
    }

    /// Retained regular-file candidate references across all components.
    pub fn retained_regular_files(&self) -> u64 {
        self.retained_regular_files
    }

    /// Retained directory candidate references across all components.
    pub fn retained_directories(&self) -> u64 {
        self.retained_directories
    }

    /// Retained symbolic-link candidate references across all components.
    pub fn retained_symlinks(&self) -> u64 {
        self.retained_symlinks
    }

    /// Retained special-entry candidate references across all components.
    pub fn retained_special_entries(&self) -> u64 {
        self.retained_special_entries
    }

    /// Absent candidate-path references across all components.
    pub fn missing_paths(&self) -> u64 {
        self.missing_paths
    }
}

struct KbuildNativeComponentContext {
    receipt: ProcessCaptureReceipt,
    stream: ProcessCaptureStreamSeal,
    analyzer: CapturedInvocation,
    analyzer_processes: BTreeSet<CapturedProcessId>,
    rustc_processes: BTreeSet<CapturedProcessId>,
    components: BTreeMap<CapturedProcessId, BTreeSet<PathBuf>>,
    component_members: BTreeMap<CapturedProcessId, BTreeSet<CapturedProcessId>>,
    timeline: KbuildNativeTimeline,
}

#[derive(Debug, Clone, Copy)]
struct KbuildNativeProcessInterval {
    start: u64,
    finish: u64,
}

#[derive(Debug, Clone)]
struct KbuildNativePathEvent {
    root: CapturedProcessId,
    stream_ordinal: u64,
    path: PathBuf,
}

#[derive(Debug, Clone)]
struct KbuildNativeMutationInteraction {
    root: CapturedProcessId,
    stream_ordinal: u64,
    candidate_path: PathBuf,
    interaction_path: PathBuf,
}

#[derive(Default)]
struct KbuildNativeInteractionTimeline {
    stream_ordinal: u64,
    output_root: PathBuf,
    component_roots: BTreeMap<CapturedProcessId, CapturedProcessId>,
    observations: Vec<KbuildNativePathEvent>,
    mutations: Vec<KbuildNativeMutationInteraction>,
}

impl KbuildNativeInteractionTimeline {
    fn new(
        output_root: PathBuf,
        component_members: &BTreeMap<CapturedProcessId, BTreeSet<CapturedProcessId>>,
    ) -> Result<Self> {
        let mut component_roots = BTreeMap::new();
        for (root, members) in component_members {
            for member in members {
                if component_roots.insert(*member, *root).is_some() {
                    anyhow::bail!("native Kbuild process belongs to multiple components");
                }
            }
        }
        Ok(Self {
            stream_ordinal: 0,
            output_root,
            component_roots,
            observations: Vec::new(),
            mutations: Vec::new(),
        })
    }

    fn record_observation(&mut self, process: CapturedProcessId, target: &[u8]) -> Result<()> {
        let Some(root) = self.component_roots.get(&process).copied() else {
            return Ok(());
        };
        let path = normalized_kbuild_input_target(target, "native input interaction")?;
        if path.starts_with(&self.output_root) {
            self.observations.push(KbuildNativePathEvent {
                root,
                stream_ordinal: self.stream_ordinal,
                path,
            });
        }
        Ok(())
    }
}

impl ProcessCaptureStreamVisitor for KbuildNativeInteractionTimeline {
    fn visit(&mut self, event: ProcessCaptureStreamEvent<'_>) -> Result<()> {
        let stream_ordinal = self.stream_ordinal;
        match &event {
            ProcessCaptureStreamEvent::Invocation {
                process,
                invocation,
            } => {
                for mapping in &invocation.file_mappings {
                    self.record_observation(*process, &mapping.identity.target)?;
                }
            }
            ProcessCaptureStreamEvent::OpenAttempt { process, attempt } => {
                if let CapturedOpenOutcome::Opened {
                    identity,
                    file_value: Some(_),
                    ..
                } = &attempt.outcome
                {
                    self.record_observation(*process, &identity.target)?;
                }
            }
            ProcessCaptureStreamEvent::ReadAttempt { process, attempt } => {
                if let CapturedReadOutcome::Read {
                    file_value: Some(_),
                    ..
                } = &attempt.outcome
                {
                    let identity = attempt
                        .request
                        .identity
                        .as_ref()
                        .context("native Kbuild witnessed read has no descriptor identity")?;
                    self.record_observation(*process, &identity.target)?;
                }
            }
            ProcessCaptureStreamEvent::MmapAttempt { process, attempt } => {
                if let CapturedMmapOutcome::Mapped {
                    file_value: Some(_),
                    ..
                } = &attempt.outcome
                {
                    let identity =
                        attempt.request.identity.as_ref().context(
                            "native Kbuild witnessed mapping has no descriptor identity",
                        )?;
                    self.record_observation(*process, &identity.target)?;
                }
            }
            ProcessCaptureStreamEvent::Getdents64Attempt { process, attempt } => {
                if let CapturedGetdents64Outcome::Enumerated {
                    directory_value: CapturedDirectoryValue::Complete { .. },
                    ..
                } = &attempt.outcome
                {
                    let identity = attempt
                        .request
                        .identity
                        .as_ref()
                        .context("native Kbuild directory observation has no identity")?;
                    if identity.kind != CapturedDescriptorKind::Directory {
                        anyhow::bail!(
                            "native Kbuild directory observation has the wrong object kind"
                        );
                    }
                    self.record_observation(*process, &identity.target)?;
                }
            }
            _ => {}
        }
        for mutation in kbuild_mutation_candidates_at(&event, &self.output_root, stream_ordinal)? {
            if let Some(root) = self.component_roots.get(&mutation.process).copied() {
                self.mutations.push(KbuildNativeMutationInteraction {
                    root,
                    stream_ordinal,
                    candidate_path: mutation.path.clone(),
                    interaction_path: mutation.path.clone(),
                });
                if matches!(
                    mutation.kind,
                    KbuildMutationCandidateKind::CreateOrTruncateOpen
                        | KbuildMutationCandidateKind::PathRemoval
                        | KbuildMutationCandidateKind::DirectoryCreation
                        | KbuildMutationCandidateKind::PathRenameSource
                        | KbuildMutationCandidateKind::PathRenameDestination
                        | KbuildMutationCandidateKind::SymbolicLinkCreation
                ) {
                    let parent = mutation
                        .path
                        .parent()
                        .context("native Kbuild membership mutation has no parent directory")?
                        .to_path_buf();
                    if parent.starts_with(&self.output_root) {
                        self.mutations.push(KbuildNativeMutationInteraction {
                            root,
                            stream_ordinal,
                            candidate_path: mutation.path,
                            interaction_path: parent,
                        });
                    }
                }
            }
        }
        self.stream_ordinal = self
            .stream_ordinal
            .checked_add(1)
            .context("native Kbuild interaction ordinal overflowed")?;
        Ok(())
    }
}

#[derive(Default)]
struct KbuildNativeTimeline {
    stream_ordinal: u64,
    starts: BTreeMap<CapturedProcessId, u64>,
    invocation_starts: BTreeMap<CapturedProcessId, u64>,
    finishes: BTreeMap<CapturedProcessId, u64>,
}

impl KbuildNativeTimeline {
    fn interval(
        &self,
        root: CapturedProcessId,
        members: &BTreeSet<CapturedProcessId>,
    ) -> Result<KbuildNativeProcessInterval> {
        let start = *self
            .invocation_starts
            .get(&root)
            .context("native Kbuild component root has no invocation start")?;
        let finish = members
            .iter()
            .map(|member| {
                self.finishes
                    .get(member)
                    .copied()
                    .context("native Kbuild component member has no stream finish")
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .max()
            .context("native Kbuild component has no process members")?;
        if start >= finish {
            anyhow::bail!("native Kbuild component has an invalid stream interval");
        }
        Ok(KbuildNativeProcessInterval { start, finish })
    }
}

impl ProcessCaptureStreamVisitor for KbuildNativeTimeline {
    fn visit(&mut self, event: ProcessCaptureStreamEvent<'_>) -> Result<()> {
        let ordinal = self.stream_ordinal;
        self.stream_ordinal = self
            .stream_ordinal
            .checked_add(1)
            .context("native Kbuild timeline ordinal overflowed")?;
        match event {
            ProcessCaptureStreamEvent::ProcessStarted { process, .. } => {
                if self.starts.insert(process, ordinal).is_some() {
                    anyhow::bail!("native Kbuild timeline repeats a process start");
                }
            }
            ProcessCaptureStreamEvent::Invocation { process, .. } => {
                self.invocation_starts.entry(process).or_insert(ordinal);
            }
            ProcessCaptureStreamEvent::ProcessFinished { process, .. } => {
                if self.finishes.insert(process, ordinal).is_some() {
                    anyhow::bail!("native Kbuild timeline repeats a process finish");
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn increment(count: &mut u64, description: &str) -> Result<()> {
    *count = count
        .checked_add(1)
        .with_context(|| format!("{description} overflowed"))?;
    Ok(())
}

fn checked_sum(counts: &[u64], description: &str) -> Result<u64> {
    counts.iter().try_fold(0_u64, |total, count| {
        total
            .checked_add(*count)
            .with_context(|| format!("{description} overflowed"))
    })
}

/// Private adapter result derived from one complete native process stream.
///
/// The graph and boundary declarations are not replay authority. A caller must
/// persist the graph privately, bind its digest into an envelope, and pass the
/// ordinary managed graph preflight before using any derived value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KbuildNativeGraphDerivation {
    /// Complete graph material derived from the sealed stream.
    pub graph: KbuildActionGraphCapture,
    /// Exact analyzer and action locations in that stream.
    pub process_boundaries: KbuildProcessBoundaryDeclaration,
    /// Complete adapter denominators for the derived generation.
    pub coverage: KbuildGraphCaptureCoverage,
}

fn graph_invocation(invocation: &rush_observe::CapturedInvocation) -> Result<CapturedInvocation> {
    let executable = bytes_path(&invocation.executable);
    let working_directory = bytes_path(&invocation.working_directory);
    if !executable.is_absolute() || !working_directory.is_absolute() || invocation.argv.is_empty() {
        anyhow::bail!("native Kbuild invocation is malformed");
    }
    let mut names = BTreeSet::new();
    let environment = invocation
        .environment
        .iter()
        .map(|entry| {
            let separator = entry
                .iter()
                .position(|byte| *byte == b'=')
                .context("native Kbuild environment entry has no name separator")?;
            let name = entry[..separator].to_vec();
            let value = entry[separator + 1..].to_vec();
            if name.is_empty() || !names.insert(name.clone()) {
                anyhow::bail!("native Kbuild invocation repeats or omits an environment name");
            }
            Ok(CapturedEnvironmentVariable { name, value })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(CapturedInvocation {
        executable,
        argv: invocation.argv.clone(),
        environment,
        working_directory,
    })
}

fn process_mutation_paths(
    process: &CapturedProcess,
    output_root: &Path,
) -> Result<BTreeSet<PathBuf>> {
    let mut candidates = BTreeSet::new();
    let mut record = |event: ProcessCaptureStreamEvent<'_>, ordinal| -> Result<()> {
        for candidate in kbuild_mutation_candidates_at(&event, output_root, ordinal)? {
            candidates.insert(candidate.path);
        }
        Ok(())
    };
    for attempt in &process.open_attempts {
        record(
            ProcessCaptureStreamEvent::OpenAttempt {
                process: process.id,
                attempt,
            },
            attempt.syscall_index,
        )?;
    }
    for attempt in &process.write_attempts {
        record(
            ProcessCaptureStreamEvent::WriteAttempt {
                process: process.id,
                attempt,
            },
            attempt.syscall_index,
        )?;
    }
    for attempt in &process.mmap_attempts {
        record(
            ProcessCaptureStreamEvent::MmapAttempt {
                process: process.id,
                attempt,
            },
            attempt.syscall_index,
        )?;
    }
    for attempt in &process.path_removal_attempts {
        record(
            ProcessCaptureStreamEvent::PathRemovalAttempt {
                process: process.id,
                attempt,
            },
            attempt.syscall_index,
        )?;
    }
    for attempt in &process.directory_creation_attempts {
        record(
            ProcessCaptureStreamEvent::DirectoryCreationAttempt {
                process: process.id,
                attempt,
            },
            attempt.syscall_index,
        )?;
    }
    for attempt in &process.path_rename_attempts {
        record(
            ProcessCaptureStreamEvent::PathRenameAttempt {
                process: process.id,
                attempt,
            },
            attempt.syscall_index,
        )?;
    }
    for attempt in &process.path_mode_change_attempts {
        record(
            ProcessCaptureStreamEvent::PathModeChangeAttempt {
                process: process.id,
                attempt,
            },
            attempt.syscall_index,
        )?;
    }
    for attempt in &process.descriptor_timestamp_change_attempts {
        record(
            ProcessCaptureStreamEvent::DescriptorTimestampChangeAttempt {
                process: process.id,
                attempt,
            },
            attempt.syscall_index,
        )?;
    }
    for attempt in &process.symbolic_link_creation_attempts {
        record(
            ProcessCaptureStreamEvent::SymbolicLinkCreationAttempt {
                process: process.id,
                attempt,
            },
            attempt.syscall_index,
        )?;
    }
    Ok(candidates)
}

fn action_component_root(
    process: CapturedProcessId,
    analyzer_processes: &BTreeSet<CapturedProcessId>,
    parents: &BTreeMap<CapturedProcessId, Option<CapturedProcessId>>,
) -> Result<Option<CapturedProcessId>> {
    if analyzer_processes.contains(&process) {
        return Ok(None);
    }
    let mut current = process;
    loop {
        let parent = parents
            .get(&current)
            .copied()
            .flatten()
            .context("native Kbuild action component does not reach the analyzer")?;
        if analyzer_processes.contains(&parent) {
            return Ok(Some(current));
        }
        current = parent;
    }
}

fn demote_mutating_analyzer_ancestors(
    root: CapturedProcessId,
    analyzer_processes: &mut BTreeSet<CapturedProcessId>,
    mutation_owners: &BTreeSet<CapturedProcessId>,
) {
    analyzer_processes.retain(|process| *process == root || !mutation_owners.contains(process));
}

fn process_executes_named(process: &CapturedProcess, name: &[u8]) -> bool {
    process.invocations.iter().any(|invocation| {
        invocation.executable == name
            || invocation
                .executable
                .strip_suffix(name)
                .is_some_and(|prefix| prefix.ends_with(b"/"))
    })
}

fn signed_i32_argument(raw: u64) -> Option<i32> {
    let low = raw as u32;
    let signed = low as i32;
    (raw == u64::from(low) || raw == signed as i64 as u64).then_some(signed)
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(super) fn supported_rustc_sigusr1_runtime_syscall(
    attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    rustc_process: bool,
) -> bool {
    use rush_observe::CapturedUnmodeledSyscallOutcome;

    let arguments = attempt.arguments;
    attempt.number == libc::SYS_tgkill as u64
        && rustc_process
        && signed_i32_argument(arguments[0]).is_some_and(|tgid| tgid > 0)
        && signed_i32_argument(arguments[1]).is_some_and(|tid| tid > 0)
        && arguments[2] == libc::SIGUSR1 as u64
        && attempt.outcome
            == CapturedUnmodeledSyscallOutcome::Returned {
                return_value: 0,
                is_error: false,
            }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(super) fn make_jobserver_fifo_runtime_syscall_candidate(
    attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    analyzer_process: bool,
    make_process: bool,
) -> bool {
    use rush_observe::CapturedUnmodeledSyscallOutcome;

    let arguments = attempt.arguments;
    analyzer_process
        && make_process
        && attempt.number == libc::SYS_mknodat as u64
        && signed_i32_argument(arguments[0]) == Some(libc::AT_FDCWD)
        && arguments[1] != 0
        && arguments[2] == (libc::S_IFIFO as u64 | 0o600)
        && arguments[3] == 0
        && attempt.outcome
            == (CapturedUnmodeledSyscallOutcome::Returned {
                return_value: 0,
                is_error: false,
            })
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub(super) fn make_jobserver_fifo_runtime_syscall_candidate(
    _attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    _analyzer_process: bool,
    _make_process: bool,
) -> bool {
    false
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(super) fn certified_make_jobserver_fifo_runtime_target(
    attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    analyzer_process: bool,
    make_process: bool,
    open_attempts: &[rush_observe::CapturedOpenAttempt],
    path_removal_attempts: &[rush_observe::CapturedPathRemovalAttempt],
) -> Option<Vec<u8>> {
    if !make_jobserver_fifo_runtime_syscall_candidate(
        attempt,
        analyzer_process,
        make_process,
    ) {
        return None;
    }

    let is_jobserver_target = |target: &[u8]| {
        let mut components = target.rsplit(|byte| *byte == b'/');
        let Some(name) = components.next() else {
            return false;
        };
        let Some(suffix) = name.strip_prefix(b"GMfifo") else {
            return false;
        };
        !suffix.is_empty()
            && suffix.iter().all(u8::is_ascii_digit)
            && components.next() == Some(b".rush-tmp".as_slice())
    };
    let opened_fifo = |open: &rush_observe::CapturedOpenAttempt,
                       descriptor: i32,
                       flags: u64| {
        if open.syscall_index <= attempt.syscall_index
            || open.request.system_call != rush_observe::CapturedOpenSystemCall::OpenAt
            || open.request.directory.is_some()
            || open.request.flags != flags
            || open.request.mode != 0
            || open.request.resolve != 0
        {
            return None;
        }
        let rush_observe::CapturedOpenOutcome::Opened {
            descriptor: opened_descriptor,
            identity,
            file_value: None,
            ..
        } = &open.outcome
        else {
            return None;
        };
        (*opened_descriptor == descriptor
            && identity.kind == rush_observe::CapturedDescriptorKind::Fifo
            && open.request.path == identity.target
            && is_jobserver_target(&identity.target))
        .then(|| identity.clone())
    };

    open_attempts.iter().find_map(|reader| {
        let Some(reader_identity) = opened_fifo(
            reader,
            3,
            (libc::O_RDONLY | libc::O_NONBLOCK) as u64,
        ) else {
            return None;
        };
        open_attempts.iter().find_map(|writer| {
            let Some(writer_identity) = opened_fifo(writer, 4, libc::O_WRONLY as u64) else {
                return None;
            };
            (writer.syscall_index > reader.syscall_index
                && writer_identity == reader_identity
                && path_removal_attempts.iter().any(|removal| {
                    removal.syscall_index > writer.syscall_index
                        && removal.request.system_call
                            == rush_observe::CapturedPathRemovalSystemCall::Unlink
                        && removal.request.directory_descriptor == libc::AT_FDCWD
                        && !removal.request.path_is_null
                        && removal.request.base.is_none()
                        && removal.request.flags == 0
                        && removal.request.path == reader_identity.target
                        && removal.outcome
                            == rush_observe::CapturedPathRemovalOutcome::Removed
                }))
            .then(|| reader_identity.target.clone())
        })
    })
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(super) fn supported_make_jobserver_fifo_runtime_syscall(
    attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    analyzer_process: bool,
    make_process: bool,
    open_attempts: &[rush_observe::CapturedOpenAttempt],
    path_removal_attempts: &[rush_observe::CapturedPathRemovalAttempt],
) -> bool {
    certified_make_jobserver_fifo_runtime_target(
        attempt,
        analyzer_process,
        make_process,
        open_attempts,
        path_removal_attempts,
    )
    .is_some()
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub(super) fn certified_make_jobserver_fifo_runtime_target(
    _attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    _analyzer_process: bool,
    _make_process: bool,
    _open_attempts: &[rush_observe::CapturedOpenAttempt],
    _path_removal_attempts: &[rush_observe::CapturedPathRemovalAttempt],
) -> Option<Vec<u8>> {
    None
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub(super) fn supported_make_jobserver_fifo_runtime_syscall(
    _attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    _analyzer_process: bool,
    _make_process: bool,
    _open_attempts: &[rush_observe::CapturedOpenAttempt],
    _path_removal_attempts: &[rush_observe::CapturedPathRemovalAttempt],
) -> bool {
    false
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(super) fn m4_self_procmap_runtime_contract(
    attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    m4_process: bool,
) -> Option<i32> {
    use rush_observe::CapturedUnmodeledSyscallOutcome;

    // _IOWR(PROCFS_IOCTL_MAGIC, 17, struct procmap_query) on x86-64.
    const PROCMAP_QUERY: u64 = 0xc068_6611;
    let arguments = attempt.arguments;
    let descriptor = signed_i32_argument(arguments[0])?;
    (m4_process
        && descriptor >= 0
        && attempt.number == libc::SYS_ioctl as u64
        && arguments[1] == PROCMAP_QUERY
        && arguments[2] != 0
        && attempt.outcome
            == (CapturedUnmodeledSyscallOutcome::Returned {
                return_value: 0,
                is_error: false,
            }))
    .then_some(descriptor)
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub(super) fn m4_self_procmap_runtime_contract(
    _attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    _m4_process: bool,
) -> Option<i32> {
    None
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(super) fn supported_m4_self_procmap_runtime_syscall(
    attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    m4_process: bool,
    open_attempts: &[rush_observe::CapturedOpenAttempt],
    close_attempts: &[rush_observe::CapturedCloseAttempt],
    dup2_attempts: &[rush_observe::CapturedDup2Attempt],
) -> bool {
    let Some(descriptor) = m4_self_procmap_runtime_contract(attempt, m4_process) else {
        return false;
    };

    let is_resolved_self_maps = |target: &[u8]| {
        target
            .strip_prefix(b"/proc/")
            .and_then(|rest| rest.strip_suffix(b"/maps"))
            .is_some_and(|pid| !pid.is_empty() && pid.iter().all(u8::is_ascii_digit))
    };
    open_attempts.iter().any(|open| {
        if open.syscall_index >= attempt.syscall_index
            || open.request.system_call != rush_observe::CapturedOpenSystemCall::OpenAt
            || open.request.path != b"/proc/self/maps"
            || open.request.directory.is_some()
            || open.request.flags != (libc::O_RDONLY | libc::O_CLOEXEC) as u64
            || open.request.mode != 0
            || open.request.resolve != 0
        {
            return false;
        }
        let rush_observe::CapturedOpenOutcome::Opened {
            descriptor: opened_descriptor,
            identity,
            mount_id,
            file_value: None,
        } = &open.outcome
        else {
            return false;
        };
        if *opened_descriptor != descriptor
            || identity.kind != rush_observe::CapturedDescriptorKind::RegularFile
            || !is_resolved_self_maps(&identity.target)
        {
            return false;
        }
        let descriptor_replaced_before_query = close_attempts.iter().any(|close| {
            close.syscall_index > open.syscall_index
                && close.syscall_index < attempt.syscall_index
                && close.request.descriptor == descriptor
                && matches!(
                    close.outcome,
                    rush_observe::CapturedCloseOutcome::Closed { .. }
                )
        }) || dup2_attempts.iter().any(|duplication| {
            duplication.syscall_index > open.syscall_index
                && duplication.syscall_index < attempt.syscall_index
                && duplication.request.target_descriptor == descriptor
                && matches!(
                    duplication.outcome,
                    rush_observe::CapturedDup2Outcome::Duplicated { .. }
                )
        });
        !descriptor_replaced_before_query
            && close_attempts.iter().any(|close| {
                close.syscall_index > attempt.syscall_index
                    && close.request.descriptor == descriptor
                    && close.request.identity.as_ref() == Some(identity)
                    && close.request.mount_id == Some(*mount_id)
                    && matches!(
                        close.outcome,
                        rush_observe::CapturedCloseOutcome::Closed { .. }
                    )
            })
    })
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub(super) fn supported_m4_self_procmap_runtime_syscall(
    _attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    _m4_process: bool,
    _open_attempts: &[rush_observe::CapturedOpenAttempt],
    _close_attempts: &[rush_observe::CapturedCloseAttempt],
    _dup2_attempts: &[rush_observe::CapturedDup2Attempt],
) -> bool {
    false
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(super) fn cat_copy_file_range_runtime_syscall_candidate(
    attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    cat_process: bool,
) -> bool {
    use rush_observe::CapturedUnmodeledSyscallOutcome;

    let arguments = attempt.arguments;
    cat_process
        && attempt.number == libc::SYS_copy_file_range as u64
        && signed_i32_argument(arguments[0]).is_some_and(|descriptor| descriptor >= 3)
        && arguments[1] == 0
        && signed_i32_argument(arguments[2]) == Some(1)
        && arguments[3] == 0
        && arguments[4] == 0x7ff0_0000
        && arguments[5] == 0
        && matches!(
            attempt.outcome,
            CapturedUnmodeledSyscallOutcome::Returned {
                return_value: -9,
                is_error: true,
            } | CapturedUnmodeledSyscallOutcome::Returned {
                return_value: 0,
                is_error: false,
            } | CapturedUnmodeledSyscallOutcome::Returned {
                return_value: 1..,
                is_error: false,
            }
        )
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub(super) fn cat_copy_file_range_runtime_syscall_candidate(
    _attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    _cat_process: bool,
) -> bool {
    false
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(super) fn supported_cat_copy_file_range_runtime_syscall(
    attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    cat_process: bool,
    open_attempts: &[rush_observe::CapturedOpenAttempt],
    write_attempts: &[rush_observe::CapturedWriteAttempt],
    close_attempts: &[rush_observe::CapturedCloseAttempt],
    dup2_attempts: &[rush_observe::CapturedDup2Attempt],
) -> bool {
    use rush_observe::{CapturedMappedFileValue, CapturedUnmodeledSyscallOutcome};

    let arguments = attempt.arguments;
    if !cat_copy_file_range_runtime_syscall_candidate(attempt, cat_process) {
        return false;
    }
    // A live inherited descriptor (for example, a Make jobserver pipe) can
    // change the descriptor assigned to cat's source open. Prove that exact
    // descriptor's file identity and lifetime instead of assuming the first
    // descriptor after stdin/stdout/stderr is available.
    let Some(source_descriptor) = signed_i32_argument(arguments[0]) else {
        return false;
    };

    open_attempts.iter().any(|open| {
        if open.syscall_index >= attempt.syscall_index
            || open.request.flags != libc::O_RDONLY as u64
            || open.request.mode != 0
            || open.request.resolve != 0
        {
            return false;
        }
        let rush_observe::CapturedOpenOutcome::Opened {
            descriptor,
            identity: source_identity,
            mount_id: source_mount_id,
            file_value: Some(source_value @ CapturedMappedFileValue::WholeFile { .. }),
        } = &open.outcome
        else {
            return false;
        };
        if *descriptor != source_descriptor {
            return false;
        }
        let source_replaced_before_copy = close_attempts.iter().any(|close| {
            close.syscall_index > open.syscall_index
                && close.syscall_index < attempt.syscall_index
                && close.request.descriptor == source_descriptor
                && matches!(
                    close.outcome,
                    rush_observe::CapturedCloseOutcome::Closed { .. }
                )
        }) || dup2_attempts.iter().any(|duplication| {
            duplication.syscall_index > open.syscall_index
                && duplication.syscall_index < attempt.syscall_index
                && duplication.request.target_descriptor == source_descriptor
                && matches!(
                    duplication.outcome,
                    rush_observe::CapturedDup2Outcome::Duplicated { .. }
                )
        });
        if source_replaced_before_copy
            || !close_attempts.iter().any(|close| {
                close.syscall_index > attempt.syscall_index
                    && close.request.descriptor == source_descriptor
                    && close.request.identity.as_ref() == Some(source_identity)
                    && close.request.mount_id == Some(*source_mount_id)
                    && matches!(
                        close.outcome,
                        rush_observe::CapturedCloseOutcome::Closed { .. }
                    )
            })
        {
            return false;
        }

        match attempt.outcome {
            CapturedUnmodeledSyscallOutcome::Returned {
                return_value: -9,
                is_error: true,
            } => write_attempts.iter().any(|write| {
                let Some(destination_identity) = write.request.identity.as_ref() else {
                    return false;
                };
                let (CapturedMappedFileValue::WholeFile {
                    bytes: source_bytes,
                    digest: source_digest,
                    ..
                }, rush_observe::CapturedWriteOutcome::Wrote {
                    bytes: written_bytes,
                    digest: written_digest,
                }) = (source_value, write.outcome)
                else {
                    return false;
                };
                write.syscall_index > attempt.syscall_index
                    && write.request.descriptor == 1
                    && write.request.append
                    && destination_identity.kind
                        == rush_observe::CapturedDescriptorKind::RegularFile
                                && written_bytes == *source_bytes
                                && written_digest == *source_digest
                    && close_attempts.iter().any(|close| {
                        close.syscall_index > write.syscall_index
                            && close.request.descriptor == 1
                            && close.request.identity.as_ref() == Some(destination_identity)
                            && matches!(
                                close.outcome,
                                rush_observe::CapturedCloseOutcome::Closed {
                                    file_value: Some(_)
                                }
                            )
                    })
            }),
            CapturedUnmodeledSyscallOutcome::Returned {
                return_value: 0,
                is_error: false,
            } => true,
            CapturedUnmodeledSyscallOutcome::Returned {
                return_value,
                is_error: false,
            } if return_value > 0 && return_value as u64 <= arguments[4] => {
                matches!(
                    source_value,
                    CapturedMappedFileValue::WholeFile { bytes, .. }
                        if *bytes == return_value as u64
                ) && close_attempts.iter().any(|close| {
                        if close.syscall_index <= attempt.syscall_index
                            || close.request.descriptor != 1
                            || close.request.identity.as_ref().is_none_or(|identity| {
                                identity.kind
                                    != rush_observe::CapturedDescriptorKind::RegularFile
                            })
                            || dup2_attempts.iter().any(|duplication| {
                                duplication.syscall_index > attempt.syscall_index
                                    && duplication.syscall_index < close.syscall_index
                                    && duplication.request.target_descriptor == 1
                                    && matches!(
                                        duplication.outcome,
                                        rush_observe::CapturedDup2Outcome::Duplicated { .. }
                                    )
                            })
                        {
                            return false;
                        }
                        matches!(
                            close.outcome,
                            rush_observe::CapturedCloseOutcome::Closed {
                                file_value: Some(_)
                            }
                        )
                    })
            }
            _ => false,
        }
    })
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub(super) fn supported_cat_copy_file_range_runtime_syscall(
    _attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    _cat_process: bool,
    _open_attempts: &[rush_observe::CapturedOpenAttempt],
    _write_attempts: &[rush_observe::CapturedWriteAttempt],
    _close_attempts: &[rush_observe::CapturedCloseAttempt],
    _dup2_attempts: &[rush_observe::CapturedDup2Attempt],
) -> bool {
    false
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(super) fn supported_kallsyms_ls_xattr_runtime_syscall(
    attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    exact_kallsyms_ls_process: bool,
) -> bool {
    use rush_observe::CapturedUnmodeledSyscallOutcome;

    exact_kallsyms_ls_process
        && attempt.number == libc::SYS_llistxattr as u64
        && attempt.arguments[0] != 0
        && attempt.arguments[1] != 0
        && attempt.arguments[2] == 152
        && attempt.outcome
            == CapturedUnmodeledSyscallOutcome::Returned {
                return_value: 0,
                is_error: false,
            }
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub(super) fn supported_kallsyms_ls_xattr_runtime_syscall(
    _attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    _exact_kallsyms_ls_process: bool,
) -> bool {
    false
}

pub(super) fn exact_kallsyms_ls_invocation(
    invocation: &rush_observe::CapturedInvocation,
) -> bool {
    invocation.executable == b"/usr/bin/ls"
        && invocation.argv.len() == 3
        && invocation.argv[0] == b"ls"
        && invocation.argv[1] == b"-dn"
        && matches!(
            invocation.argv[2].as_slice(),
            b".tmp_vmlinux1.kallsyms.o" | b".tmp_vmlinux2.kallsyms.o"
        )
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(super) fn supported_kbuild_runtime_syscall(
    attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    analyzer_process: bool,
    rustc_process: bool,
    make_process: bool,
    _xargs_process: bool,
) -> bool {
    use rush_observe::CapturedUnmodeledSyscallOutcome;

    let returned = |value, is_error| {
        attempt.outcome
            == CapturedUnmodeledSyscallOutcome::Returned {
                return_value: value,
                is_error,
            }
    };
    let arguments = attempt.arguments;
    if attempt.number == libc::SYS_madvise as u64 {
        const MADV_GUARD_INSTALL: u64 = 102;
        return arguments[0] != 0
            && arguments[0] % 4096 == 0
            && arguments[1] != 0
            && arguments[1] % 4096 == 0
            && matches!(
                arguments[2],
                advice if advice == libc::MADV_DONTNEED as u64
                    || advice == libc::MADV_FREE as u64
                    || advice == MADV_GUARD_INSTALL
            )
            && returned(0, false);
    }
    if attempt.number == libc::SYS_futex as u64 {
        let operation = arguments[1] as u32;
        let value_mismatched = returned(-libc::EAGAIN as i64, true);
        let supported_wait = match operation {
            128 => arguments[3] == 0,
            137 | 265 => arguments[5] as u32 == u32::MAX && (arguments[3] == 0 || value_mismatched),
            _ => false,
        };
        return arguments[0] != 0
            && arguments[0] % std::mem::align_of::<u32>() as u64 == 0
            && supported_wait
            && (returned(0, false)
                || returned(-libc::EAGAIN as i64, true)
                || returned(-libc::EINTR as i64, true)
                || returned(-512, true)
                || attempt.outcome == CapturedUnmodeledSyscallOutcome::Terminated);
    }
    if attempt.number == libc::SYS_sched_yield as u64 {
        return returned(0, false);
    }
    if attempt.number == libc::SYS_sched_getscheduler as u64 {
        return signed_i32_argument(arguments[0]) == Some(0)
            && returned(libc::SCHED_OTHER as i64, false);
    }
    if attempt.number == libc::SYS_fsync as u64 {
        return signed_i32_argument(arguments[0]).is_some_and(|fd| fd >= 0) && returned(0, false);
    }
    if attempt.number == libc::SYS_mremap as u64 {
        return arguments[0] != 0
            && arguments[0] % 4096 == 0
            && arguments[1] != 0
            && arguments[1] % 4096 == 0
            && arguments[2] != 0
            && arguments[2] % 4096 == 0
            && arguments[3] == libc::MREMAP_MAYMOVE as u64
            && arguments[4] == 0
            && matches!(
                attempt.outcome,
                CapturedUnmodeledSyscallOutcome::Returned {
                    return_value,
                    is_error: false,
                } if return_value > 0 && return_value % 4096 == 0
            );
    }
    if attempt.number == libc::SYS_ioctl as u64 {
        // GNU make and common build utilities probe whether inherited
        // descriptors are terminals with the termios2 variant of TCGETS.
        // ENOTTY guarantees that no terminal state was returned and the
        // request has no filesystem or process-global effect.
        const TCGETS2: u64 = 0x802c_542a;
        return signed_i32_argument(arguments[0]).is_some_and(|descriptor| descriptor >= 0)
            && arguments[1] == TCGETS2
            && arguments[2] != 0
            && returned(-libc::ENOTTY as i64, true);
    }
    if supported_rustc_sigusr1_runtime_syscall(attempt, rustc_process) {
        return true;
    }
    if attempt.number == libc::SYS_pselect6 as u64 {
        const ERESTARTNOHAND: i64 = 514;
        return ((analyzer_process && arguments[0] == 4)
            || (make_process && matches!(arguments[0], 4 | 5)))
            && arguments[1] != 0
            && arguments[2] == 0
            && arguments[3] == 0
            && arguments[4] == 0
            && arguments[5] != 0
            && (returned(1, false) || returned(-ERESTARTNOHAND, true));
    }
    if analyzer_process {
        return false;
    }
    match attempt.number {
        // Poll admission requires the typed descriptor identities and event
        // masks. Raw argument words cannot prove a readiness-probe profile.
        number if number == libc::SYS_sigaltstack as u64 => {
            rustc_process && (arguments[0] == 0) != (arguments[1] == 0) && returned(0, false)
        }
        number if number == libc::SYS_prctl as u64 => {
            rustc_process
                && arguments[0] == libc::PR_SET_NAME as u64
                && arguments[1] != 0
                && returned(0, false)
        }
        number if number == libc::SYS_sched_getaffinity as u64 => {
            rustc_process
                && signed_i32_argument(arguments[0]).is_some_and(|process| process > 0)
                && arguments[1] == 32
                && arguments[2] != 0
                && returned(16, false)
        }
        _ => false,
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(super) fn supported_objtool_signal_stack_runtime_syscall(
    attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    objtool_process: bool,
) -> bool {
    use rush_observe::CapturedUnmodeledSyscallOutcome;

    objtool_process
        && attempt.number == libc::SYS_sigaltstack as u64
        && (attempt.arguments[0] == 0) != (attempt.arguments[1] == 0)
        && attempt.outcome
            == CapturedUnmodeledSyscallOutcome::Returned {
                return_value: 0,
                is_error: false,
            }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(super) fn objtool_ftruncate_runtime_contract(
    attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    objtool_process: bool,
) -> Option<(i32, u64)> {
    use rush_observe::CapturedUnmodeledSyscallOutcome;

    if !objtool_process || attempt.number != libc::SYS_ftruncate as u64 {
        return None;
    }
    let descriptor = signed_i32_argument(attempt.arguments[0])?;
    let length = attempt.arguments[1];
    (descriptor >= 0
        && length != 0
        && attempt.outcome
            == CapturedUnmodeledSyscallOutcome::Returned {
                return_value: 0,
                is_error: false,
            })
    .then_some((descriptor, length))
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn supported_objtool_runtime_syscall(
    attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    objtool_process: bool,
    mmap_attempts: &[rush_observe::CapturedMmapAttempt],
    write_attempts: &[rush_observe::CapturedWriteAttempt],
    close_attempts: &[rush_observe::CapturedCloseAttempt],
    dup2_attempts: &[rush_observe::CapturedDup2Attempt],
) -> bool {
    if supported_objtool_signal_stack_runtime_syscall(attempt, objtool_process) {
        return true;
    }
    if !objtool_process {
        return false;
    }
    if let Some((descriptor, length)) = objtool_ftruncate_runtime_contract(attempt, objtool_process)
    {
        let descriptor_is_continuous_through = |witness_index| {
            !close_attempts.iter().any(|close| {
                close.syscall_index > attempt.syscall_index
                    && close.syscall_index < witness_index
                    && close.request.descriptor == descriptor
                    && matches!(
                        close.outcome,
                        rush_observe::CapturedCloseOutcome::Closed { .. }
                    )
            }) && !dup2_attempts.iter().any(|duplication| {
                duplication.syscall_index > attempt.syscall_index
                    && duplication.syscall_index < witness_index
                    && duplication.request.target_descriptor == descriptor
                    && matches!(
                        duplication.outcome,
                        rush_observe::CapturedDup2Outcome::Duplicated { .. }
                    )
            })
        };
        return mmap_attempts.iter().any(|mapping| {
            mapping.syscall_index > attempt.syscall_index
                && mapping.request.descriptor == descriptor
                && mapping.request.length == length
                && mapping.request.offset == 0
                && mapping.request.flags & 0xf == libc::MAP_SHARED as u64
                && mapping.request.protection & libc::PROT_WRITE as u64 != 0
                && mapping.request.identity.as_ref().is_some_and(|identity| {
                    identity.kind == rush_observe::CapturedDescriptorKind::RegularFile
                })
                && matches!(
                    mapping.outcome,
                    rush_observe::CapturedMmapOutcome::Mapped { .. }
                )
                && descriptor_is_continuous_through(mapping.syscall_index)
        }) || write_attempts.iter().any(|write| {
            write.syscall_index > attempt.syscall_index
                && write.request.descriptor == descriptor
                && write.request.identity.as_ref().is_some_and(|identity| {
                    identity.kind == rush_observe::CapturedDescriptorKind::RegularFile
                })
                && matches!(
                    write.outcome,
                    rush_observe::CapturedWriteOutcome::Wrote { bytes, .. } if bytes != 0
                )
                && descriptor_is_continuous_through(write.syscall_index)
        });
    }
    false
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub(super) fn sed_fchown_runtime_contract(
    attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    sed_process: bool,
    credentials: Option<&rush_observe::CapturedCredentialState>,
) -> Option<i32> {
    use rush_observe::CapturedUnmodeledSyscallOutcome;

    if !sed_process || attempt.number != libc::SYS_fchown as u64 {
        return None;
    }
    let descriptor = signed_i32_argument(attempt.arguments[0])?;
    let credentials = credentials?;
    (descriptor >= 0
        && attempt.arguments[1] == u64::from(credentials.filesystem_user)
        && attempt.arguments[2] == u64::from(credentials.filesystem_group)
        && attempt.outcome
            == CapturedUnmodeledSyscallOutcome::Returned {
                return_value: 0,
                is_error: false,
            })
    .then_some(descriptor)
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn supported_sed_runtime_syscall(
    attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    sed_process: bool,
    credentials: Option<&rush_observe::CapturedCredentialState>,
    write_attempts: &[rush_observe::CapturedWriteAttempt],
    close_attempts: &[rush_observe::CapturedCloseAttempt],
    dup2_attempts: &[rush_observe::CapturedDup2Attempt],
) -> bool {
    let Some(descriptor) = sed_fchown_runtime_contract(attempt, sed_process, credentials) else {
        return false;
    };
    write_attempts.iter().any(|write| {
        let first = attempt.syscall_index.min(write.syscall_index);
        let last = attempt.syscall_index.max(write.syscall_index);
        write.request.descriptor == descriptor
            && write.request.identity.as_ref().is_some_and(|identity| {
                identity.kind == rush_observe::CapturedDescriptorKind::RegularFile
            })
            && matches!(
                write.outcome,
                rush_observe::CapturedWriteOutcome::Wrote { bytes, .. } if bytes != 0
            )
            && !close_attempts.iter().any(|close| {
                close.syscall_index > first
                    && close.syscall_index < last
                    && close.request.descriptor == descriptor
                    && matches!(
                        close.outcome,
                        rush_observe::CapturedCloseOutcome::Closed { .. }
                    )
            })
            && !dup2_attempts.iter().any(|duplication| {
                duplication.syscall_index > first
                    && duplication.syscall_index < last
                    && duplication.request.target_descriptor == descriptor
                    && matches!(
                        duplication.outcome,
                        rush_observe::CapturedDup2Outcome::Duplicated { .. }
                    )
            })
    })
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub(super) fn sed_fchown_runtime_contract(
    _attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    _sed_process: bool,
    _credentials: Option<&rush_observe::CapturedCredentialState>,
) -> Option<i32> {
    None
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn supported_sed_runtime_syscall(
    _attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    _sed_process: bool,
    _credentials: Option<&rush_observe::CapturedCredentialState>,
    _write_attempts: &[rush_observe::CapturedWriteAttempt],
    _close_attempts: &[rush_observe::CapturedCloseAttempt],
    _dup2_attempts: &[rush_observe::CapturedDup2Attempt],
) -> bool {
    false
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub(super) fn supported_objtool_signal_stack_runtime_syscall(
    _attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    _objtool_process: bool,
) -> bool {
    false
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub(super) fn objtool_ftruncate_runtime_contract(
    _attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    _objtool_process: bool,
) -> Option<(i32, u64)> {
    None
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn supported_objtool_runtime_syscall(
    _attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    _objtool_process: bool,
    _mmap_attempts: &[rush_observe::CapturedMmapAttempt],
    _write_attempts: &[rush_observe::CapturedWriteAttempt],
    _close_attempts: &[rush_observe::CapturedCloseAttempt],
    _dup2_attempts: &[rush_observe::CapturedDup2Attempt],
) -> bool {
    false
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub(super) fn supported_rustc_sigusr1_runtime_syscall(
    _attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    _rustc_process: bool,
) -> bool {
    false
}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub(super) fn supported_kbuild_runtime_syscall(
    _attempt: &rush_observe::CapturedUnmodeledSyscallAttempt,
    _analyzer_process: bool,
    _rustc_process: bool,
    _make_process: bool,
    _xargs_process: bool,
) -> bool {
    false
}

pub(super) fn is_command_receipt(path: &Path) -> bool {
    path.file_name()
        .map(os_bytes)
        .is_some_and(|name| name.starts_with(b".") && name.ends_with(b".cmd"))
}

fn command_receipt_match_indices(commands: &[CapturedInvocation], evidence: &[u8]) -> Vec<usize> {
    commands
        .iter()
        .enumerate()
        .filter_map(|(index, command)| {
            command
                .argv
                .iter()
                .any(|argument| contains_subslice(argument, evidence))
                .then_some(index)
        })
        .collect()
}

fn command_receipt_match_index(commands: &[CapturedInvocation], evidence: &[u8]) -> Option<usize> {
    let matches = command_receipt_match_indices(commands, evidence);
    let final_index = commands.len().checked_sub(1)?;
    if matches.contains(&final_index) {
        Some(final_index)
    } else if matches.len() == 1 {
        matches.first().copied()
    } else {
        None
    }
}

fn load_kbuild_native_components(
    process_capture: &KbuildProcessCapture,
    output_root: &Path,
) -> Result<KbuildNativeComponentContext> {
    let limits = process_capture.limits()?;
    let (receipt, stream) = load_process_capture_stream(
        &process_capture.stream_path,
        process_capture.summary,
        limits,
    )
    .context("load native Kbuild process stream for component discovery")?;
    let (timeline, timeline_stream) = visit_process_capture_stream(
        &process_capture.stream_path,
        process_capture.summary,
        limits,
        KbuildNativeTimeline::default(),
    )
    .context("load native Kbuild process timeline for component discovery")?;
    if timeline_stream != stream || timeline.stream_ordinal != stream.record_count() {
        anyhow::bail!("native Kbuild process timeline differs from its loaded stream");
    }
    let root_process = receipt
        .processes()
        .iter()
        .find(|process| process.id == receipt.root())
        .context("native Kbuild process stream omits its root")?;
    if root_process.parent.is_some() || root_process.outcome != CapturedProcessOutcome::Exited(0) {
        anyhow::bail!("native Kbuild analyzer root did not exit successfully");
    }
    let root_invocation = root_process
        .invocations
        .first()
        .context("native Kbuild analyzer root has no invocation")?;
    let analyzer = graph_invocation(root_invocation)?;

    let parents = receipt
        .processes()
        .iter()
        .map(|process| (process.id, process.parent))
        .collect::<BTreeMap<_, _>>();
    let mut analyzer_processes = receipt
        .processes()
        .iter()
        .filter(|process| {
            process
                .invocations
                .iter()
                .any(|invocation| invocation.executable == root_invocation.executable)
        })
        .map(|process| process.id)
        .collect::<BTreeSet<_>>();
    analyzer_processes.insert(receipt.root());
    let analyzer_roots = analyzer_processes.iter().copied().collect::<Vec<_>>();
    for process in analyzer_roots {
        let mut current = parents.get(&process).copied().flatten();
        while let Some(ancestor) = current {
            analyzer_processes.insert(ancestor);
            current = parents.get(&ancestor).copied().flatten();
        }
    }
    let mut mutation_paths_by_process = BTreeMap::new();
    for process in receipt.processes() {
        let paths = process_mutation_paths(process, output_root)?;
        if !paths.is_empty() {
            mutation_paths_by_process.insert(process.id, paths);
        }
    }
    let mutation_owners = mutation_paths_by_process
        .keys()
        .copied()
        .collect::<BTreeSet<_>>();
    demote_mutating_analyzer_ancestors(receipt.root(), &mut analyzer_processes, &mutation_owners);
    let mut rustc_processes = receipt
        .processes()
        .iter()
        .filter(|process| process_executes_named(process, b"rustc"))
        .map(|process| process.id)
        .collect::<BTreeSet<_>>();
    loop {
        let inherited = receipt
            .processes()
            .iter()
            .filter(|process| {
                process.invocations.is_empty()
                    && process
                        .parent
                        .is_some_and(|parent| rustc_processes.contains(&parent))
            })
            .map(|process| process.id)
            .collect::<Vec<_>>();
        let previous = rustc_processes.len();
        rustc_processes.extend(inherited);
        if rustc_processes.len() == previous {
            break;
        }
    }

    let mut components = BTreeMap::<CapturedProcessId, BTreeSet<PathBuf>>::new();
    for process in receipt.processes() {
        let Some(paths) = mutation_paths_by_process.get(&process.id) else {
            continue;
        };
        let Some(root) = action_component_root(process.id, &analyzer_processes, &parents)? else {
            // The analyzer root remains analyzer-owned even when GNU Make uses
            // TMPDIR for a transient FIFO jobserver. The downstream mutation
            // seal admits these candidates only after proving same-owner
            // creation, final removal, and final absence.
            continue;
        };
        components
            .entry(root)
            .or_default()
            .extend(paths.iter().cloned());
    }
    let mut component_members = BTreeMap::<CapturedProcessId, BTreeSet<CapturedProcessId>>::new();
    for process in receipt.processes() {
        if let Some(root) = action_component_root(process.id, &analyzer_processes, &parents)? {
            if components.contains_key(&root) {
                component_members
                    .entry(root)
                    .or_default()
                    .insert(process.id);
            }
        }
    }
    if component_members.len() != components.len() {
        anyhow::bail!("native Kbuild component membership is incomplete");
    }
    Ok(KbuildNativeComponentContext {
        receipt,
        stream,
        analyzer,
        analyzer_processes,
        rustc_processes,
        components,
        component_members,
        timeline,
    })
}

#[derive(Debug)]
struct KbuildNativeLogicalComponent {
    roots: Vec<CapturedProcessId>,
    candidate_paths: BTreeSet<PathBuf>,
}

fn native_mutation_path_coverage_count(
    owned_paths: &BTreeMap<PathBuf, usize>,
    certified_runtime_paths: &BTreeSet<PathBuf>,
) -> Result<u64> {
    let mut paths = owned_paths.keys().cloned().collect::<BTreeSet<_>>();
    paths.extend(certified_runtime_paths.iter().cloned());
    u64::try_from(paths.len()).context("native Kbuild mutation-path count does not fit u64")
}

fn component_representative(
    parents: &BTreeMap<CapturedProcessId, CapturedProcessId>,
    mut process: CapturedProcessId,
) -> Result<CapturedProcessId> {
    let mut traversed = 0_usize;
    loop {
        let parent = *parents
            .get(&process)
            .context("native Kbuild component union omits a process")?;
        if parent == process {
            return Ok(process);
        }
        process = parent;
        traversed = traversed
            .checked_add(1)
            .context("native Kbuild component union traversal overflowed")?;
        if traversed > parents.len() {
            anyhow::bail!("native Kbuild component union contains a cycle");
        }
    }
}

fn union_components(
    parents: &mut BTreeMap<CapturedProcessId, CapturedProcessId>,
    left: CapturedProcessId,
    right: CapturedProcessId,
) -> Result<()> {
    let left = component_representative(parents, left)?;
    let right = component_representative(parents, right)?;
    if left != right {
        let (representative, child) = if left < right {
            (left, right)
        } else {
            (right, left)
        };
        parents.insert(child, representative);
    }
    Ok(())
}

#[derive(Debug, Clone)]
enum KbuildNativeUnionReason {
    SharedCandidate {
        path: PathBuf,
    },
    InterleavedObservation {
        path: PathBuf,
        observation: u64,
        first_mutation: u64,
        final_mutation: u64,
    },
}

/// Private diagnostic for one pair of roots coalesced by native derivation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KbuildNativeUnionChainDiagnostic {
    start: CapturedProcessId,
    finish: CapturedProcessId,
    component_roots: Vec<CapturedProcessId>,
    recorded_union_edges: u64,
    chain: Vec<String>,
}

impl KbuildNativeUnionChainDiagnostic {
    /// Number of process roots in the resulting logical component.
    pub fn component_root_count(&self) -> usize {
        self.component_roots.len()
    }

    /// Number of union edges recorded across this derivation.
    pub fn recorded_union_edges(&self) -> u64 {
        self.recorded_union_edges
    }

    /// Number of edges in the shortest recorded chain between the roots.
    pub fn chain_length(&self) -> usize {
        self.chain.len()
    }
}

#[derive(Debug, Clone)]
struct KbuildNativeUnionEdge {
    left: CapturedProcessId,
    right: CapturedProcessId,
    reason: KbuildNativeUnionReason,
}

fn native_union_chain(
    edges: &[KbuildNativeUnionEdge],
    start: CapturedProcessId,
    finish: CapturedProcessId,
) -> Vec<String> {
    let mut adjacency = BTreeMap::<CapturedProcessId, Vec<(CapturedProcessId, usize)>>::new();
    for (index, edge) in edges.iter().enumerate() {
        adjacency
            .entry(edge.left)
            .or_default()
            .push((edge.right, index));
        adjacency
            .entry(edge.right)
            .or_default()
            .push((edge.left, index));
    }
    let mut queue = std::collections::VecDeque::from([start]);
    let mut predecessors = BTreeMap::<CapturedProcessId, (CapturedProcessId, usize)>::new();
    while let Some(current) = queue.pop_front() {
        if current == finish {
            break;
        }
        for (next, edge) in adjacency.get(&current).into_iter().flatten() {
            if *next == start || predecessors.contains_key(next) {
                continue;
            }
            predecessors.insert(*next, (current, *edge));
            queue.push_back(*next);
        }
    }
    if start != finish && !predecessors.contains_key(&finish) {
        return vec!["no recorded union chain".to_string()];
    }
    let mut chain = Vec::new();
    let mut current = finish;
    while current != start {
        let Some((previous, edge_index)) = predecessors.get(&current).copied() else {
            return vec!["recorded union chain is incomplete".to_string()];
        };
        let edge = &edges[edge_index];
        let reason = match &edge.reason {
            KbuildNativeUnionReason::SharedCandidate { path } => {
                format!("shared-candidate path={path:?}")
            }
            KbuildNativeUnionReason::InterleavedObservation {
                path,
                observation,
                first_mutation,
                final_mutation,
            } => format!(
                "interleaved-observation path={path:?} observation={observation} producer_epoch=[{first_mutation},{final_mutation}]"
            ),
        };
        chain.push(format!("{previous:?}->{current:?} {reason}"));
        current = previous;
    }
    chain.reverse();
    chain
}

#[cfg(test)]
fn coalesce_native_components(
    components: &BTreeMap<CapturedProcessId, BTreeSet<PathBuf>>,
    component_members: &BTreeMap<CapturedProcessId, BTreeSet<CapturedProcessId>>,
    timeline: &KbuildNativeTimeline,
    interactions: &KbuildNativeInteractionTimeline,
) -> Result<Vec<KbuildNativeLogicalComponent>> {
    let interaction_roots = components.keys().copied().collect();
    coalesce_native_components_for_roots(
        components,
        component_members,
        timeline,
        interactions,
        &interaction_roots,
    )
}

fn coalesce_native_components_for_roots(
    components: &BTreeMap<CapturedProcessId, BTreeSet<PathBuf>>,
    component_members: &BTreeMap<CapturedProcessId, BTreeSet<CapturedProcessId>>,
    timeline: &KbuildNativeTimeline,
    interactions: &KbuildNativeInteractionTimeline,
    interaction_roots: &BTreeSet<CapturedProcessId>,
) -> Result<Vec<KbuildNativeLogicalComponent>> {
    let retained_candidate_paths = interaction_roots
        .iter()
        .filter_map(|root| components.get(root))
        .flatten()
        .cloned()
        .collect::<BTreeSet<_>>();
    Ok(coalesce_native_components_with_union_edges_for_roots(
        components,
        component_members,
        timeline,
        interactions,
        interaction_roots,
        &retained_candidate_paths,
    )?
    .0)
}

fn coalesce_native_components_with_union_edges_for_roots(
    components: &BTreeMap<CapturedProcessId, BTreeSet<PathBuf>>,
    component_members: &BTreeMap<CapturedProcessId, BTreeSet<CapturedProcessId>>,
    timeline: &KbuildNativeTimeline,
    interactions: &KbuildNativeInteractionTimeline,
    interaction_roots: &BTreeSet<CapturedProcessId>,
    retained_candidate_paths: &BTreeSet<PathBuf>,
) -> Result<(
    Vec<KbuildNativeLogicalComponent>,
    Vec<KbuildNativeUnionEdge>,
)> {
    if !interaction_roots
        .iter()
        .all(|root| components.contains_key(root))
    {
        anyhow::bail!("native Kbuild interaction root has no mutation component");
    }
    if !components
        .keys()
        .all(|root| component_members.contains_key(root))
    {
        anyhow::bail!("native Kbuild mutation component has no process membership");
    }
    let mut interaction_intervals = BTreeMap::<CapturedProcessId, KbuildNativeProcessInterval>::new();
    for (root, stream_ordinal) in interactions
        .observations
        .iter()
        .map(|event| (event.root, event.stream_ordinal))
        .chain(
            interactions
                .mutations
                .iter()
                .map(|event| (event.root, event.stream_ordinal)),
        )
    {
        interaction_intervals
            .entry(root)
            .and_modify(|interval| {
                interval.start = interval.start.min(stream_ordinal);
                interval.finish = interval.finish.max(stream_ordinal);
            })
            .or_insert(KbuildNativeProcessInterval {
                start: stream_ordinal,
                finish: stream_ordinal,
            });
    }
    if !components
        .keys()
        .all(|root| interaction_intervals.contains_key(root))
    {
        anyhow::bail!("native Kbuild mutation component has no filesystem interaction");
    }
    let mut unions = components
        .keys()
        .map(|root| (*root, *root))
        .collect::<BTreeMap<_, _>>();
    let mut union_edges = Vec::<KbuildNativeUnionEdge>::new();
    let mut first_owner = BTreeMap::<&Path, CapturedProcessId>::new();
    for (root, paths) in components {
        for path in paths {
            if let Some(owner) = first_owner.insert(path, *root) {
                union_edges.push(KbuildNativeUnionEdge {
                    left: owner,
                    right: *root,
                    reason: KbuildNativeUnionReason::SharedCandidate { path: path.clone() },
                });
                union_components(&mut unions, owner, *root)?;
            }
        }
    }
    let interaction_representatives = interaction_roots
        .iter()
        .map(|root| component_representative(&unions, *root))
        .collect::<Result<BTreeSet<_>>>()?;
    let mut interaction_members = BTreeSet::new();
    for root in components.keys() {
        let representative = component_representative(&unions, *root)?;
        if interaction_representatives.contains(&representative) {
            interaction_members.insert(*root);
        }
    }

    let mut mutation_epochs = BTreeMap::<&Path, BTreeMap<CapturedProcessId, (u64, u64)>>::new();
    for mutation in &interactions.mutations {
        if !components
            .get(&mutation.root)
            .is_some_and(|paths| paths.contains(&mutation.candidate_path))
        {
            anyhow::bail!("native Kbuild interaction has an unknown mutation component");
        }
        if !interaction_members.contains(&mutation.root) {
            continue;
        }
        let producer = if retained_candidate_paths.contains(&mutation.candidate_path) {
            component_representative(&unions, mutation.root)?
        } else {
            mutation.root
        };
        let epoch = mutation_epochs
            .entry(&mutation.interaction_path)
            .or_default()
            .entry(producer)
            .or_insert((mutation.stream_ordinal, mutation.stream_ordinal));
        epoch.0 = epoch.0.min(mutation.stream_ordinal);
        epoch.1 = epoch.1.max(mutation.stream_ordinal);
    }
    for observation in &interactions.observations {
        if !interaction_members.contains(&observation.root) {
            continue;
        }
        let Some(producers) = mutation_epochs.get(observation.path.as_path()) else {
            continue;
        };
        for (producer, (first_mutation, final_mutation)) in producers {
            if *first_mutation < observation.stream_ordinal
                && observation.stream_ordinal < *final_mutation
                && component_representative(&unions, observation.root)?
                    != component_representative(&unions, *producer)?
            {
                // Replaying a producer cluster with mutation epochs on both
                // sides of this consumer as one contiguous command sequence
                // would change the observed order. Preserve that order by
                // making the interacting roots one compound action. The
                // interval check below still rejects concurrent roots.
                union_edges.push(KbuildNativeUnionEdge {
                    left: observation.root,
                    right: *producer,
                    reason: KbuildNativeUnionReason::InterleavedObservation {
                        path: observation.path.clone(),
                        observation: observation.stream_ordinal,
                        first_mutation: *first_mutation,
                        final_mutation: *final_mutation,
                    },
                });
                union_components(&mut unions, observation.root, *producer)?;
            }
        }
    }

    let mut clusters = BTreeMap::<CapturedProcessId, KbuildNativeLogicalComponent>::new();
    for (root, paths) in components {
        let representative = component_representative(&unions, *root)?;
        let cluster =
            clusters
                .entry(representative)
                .or_insert_with(|| KbuildNativeLogicalComponent {
                    roots: Vec::new(),
                    candidate_paths: BTreeSet::new(),
                });
        cluster.roots.push(*root);
        cluster.candidate_paths.extend(paths.iter().cloned());
    }

    let mut clusters = clusters.into_values().collect::<Vec<_>>();
    let cluster_by_root = clusters
        .iter()
        .enumerate()
        .flat_map(|(cluster, component)| {
            component
                .roots
                .iter()
                .map(move |root| (*root, cluster))
        })
        .collect::<BTreeMap<_, _>>();

    type NativeRootIntervals = BTreeMap<CapturedProcessId, KbuildNativeProcessInterval>;
    let mut candidate_mutations =
        BTreeMap::<(usize, &Path), NativeRootIntervals>::new();
    let mut path_mutations = BTreeMap::<(usize, &Path), NativeRootIntervals>::new();
    let mut path_observations = BTreeMap::<(usize, &Path), NativeRootIntervals>::new();
    let extend_interval =
        |intervals: &mut NativeRootIntervals, root: CapturedProcessId, ordinal: u64| {
            intervals
                .entry(root)
                .and_modify(|interval| {
                    interval.start = interval.start.min(ordinal);
                    interval.finish = interval.finish.max(ordinal);
                })
                .or_insert(KbuildNativeProcessInterval {
                    start: ordinal,
                    finish: ordinal,
                });
        };
    for mutation in &interactions.mutations {
        let cluster = *cluster_by_root
            .get(&mutation.root)
            .context("native Kbuild mutation has no logical component")?;
        extend_interval(
            candidate_mutations
                .entry((cluster, mutation.candidate_path.as_path()))
                .or_default(),
            mutation.root,
            mutation.stream_ordinal,
        );
        extend_interval(
            path_mutations
                .entry((cluster, mutation.interaction_path.as_path()))
                .or_default(),
            mutation.root,
            mutation.stream_ordinal,
        );
    }
    for observation in &interactions.observations {
        let cluster = *cluster_by_root
            .get(&observation.root)
            .context("native Kbuild observation has no logical component")?;
        extend_interval(
            path_observations
                .entry((cluster, observation.path.as_path()))
                .or_default(),
            observation.root,
            observation.stream_ordinal,
        );
    }

    let mut precedence = vec![BTreeSet::<(CapturedProcessId, CapturedProcessId)>::new(); clusters.len()];
    let mut add_precedence = |
        cluster: usize,
        left_root: CapturedProcessId,
        left: KbuildNativeProcessInterval,
        right_root: CapturedProcessId,
        right: KbuildNativeProcessInterval,
    | {
        if left_root == right_root {
            return;
        }
        if left.start < right.finish {
            precedence[cluster].insert((left_root, right_root));
        }
        if right.start < left.finish {
            precedence[cluster].insert((right_root, left_root));
        }
    };
    for ((cluster, _), roots) in &candidate_mutations {
        let roots = roots.iter().collect::<Vec<_>>();
        for left in 0..roots.len() {
            for right in (left + 1)..roots.len() {
                add_precedence(
                    *cluster,
                    *roots[left].0,
                    *roots[left].1,
                    *roots[right].0,
                    *roots[right].1,
                );
            }
        }
    }
    for ((cluster, path), observations) in &path_observations {
        let Some(mutations) = path_mutations.get(&(*cluster, *path)) else {
            continue;
        };
        for (observation_root, observation) in observations {
            for (mutation_root, mutation) in mutations {
                add_precedence(
                    *cluster,
                    *observation_root,
                    *observation,
                    *mutation_root,
                    *mutation,
                );
            }
        }
    }

    for (cluster_index, cluster) in clusters.iter_mut().enumerate() {
        let mut incoming = cluster
            .roots
            .iter()
            .map(|root| (*root, 0_usize))
            .collect::<BTreeMap<_, _>>();
        let mut outgoing = BTreeMap::<CapturedProcessId, BTreeSet<CapturedProcessId>>::new();
        for (before, after) in &precedence[cluster_index] {
            if outgoing.entry(*before).or_default().insert(*after) {
                *incoming
                    .get_mut(after)
                    .context("native Kbuild precedence target is outside its component")? += 1;
            }
        }
        let order_key = |root: CapturedProcessId| {
            (
                interaction_intervals
                    .get(&root)
                    .map(|interval| interval.start)
                    .unwrap_or(u64::MAX),
                timeline
                    .invocation_starts
                    .get(&root)
                    .copied()
                    .unwrap_or(u64::MAX),
                root,
            )
        };
        let mut ready = incoming
            .iter()
            .filter_map(|(root, count)| (*count == 0).then_some(order_key(*root)))
            .collect::<BTreeSet<_>>();
        let mut ordered = Vec::with_capacity(cluster.roots.len());
        while let Some(key) = ready.pop_first() {
            let root = key.2;
            ordered.push(root);
            for after in outgoing.get(&root).into_iter().flatten() {
                let count = incoming
                    .get_mut(after)
                    .context("native Kbuild precedence target disappeared")?;
                *count = count
                    .checked_sub(1)
                    .context("native Kbuild precedence count underflowed")?;
                if *count == 0 {
                    ready.insert(order_key(*after));
                }
            }
        }
        if ordered.len() != cluster.roots.len() {
            let blocked = incoming
                .into_iter()
                .filter_map(|(root, count)| (count != 0).then_some(root))
                .collect::<Vec<_>>();
            anyhow::bail!(
                "native Kbuild compound-action interaction order has a cycle: roots={blocked:?}"
            );
        }
        cluster.roots = ordered;
    }
    clusters.sort_by_key(|cluster| {
        cluster
            .roots
            .first()
            .and_then(|root| timeline.invocation_starts.get(root))
            .copied()
            .unwrap_or(u64::MAX)
    });
    Ok((clusters, union_edges))
}

/// Diagnose why two retained native mutation roots became one logical action.
///
/// The result can contain private paths and must only be persisted privately.
pub fn diagnose_kbuild_native_union_chain(
    process_capture: &KbuildProcessCapture,
    output_root: &Path,
    start: CapturedProcessId,
    finish: CapturedProcessId,
) -> Result<KbuildNativeUnionChainDiagnostic> {
    if start == finish {
        anyhow::bail!("native Kbuild union diagnostic requires two distinct roots");
    }
    let output_root = canonical_root(output_root, "output")?;
    let context = load_kbuild_native_components(process_capture, &output_root)?;
    let (interactions, interaction_stream) = visit_process_capture_stream(
        &process_capture.stream_path,
        process_capture.summary,
        process_capture.limits()?,
        KbuildNativeInteractionTimeline::new(output_root, &context.component_members)?,
    )
    .context("load native Kbuild action interactions")?;
    if interaction_stream != context.stream
        || interactions.stream_ordinal != context.stream.record_count()
    {
        anyhow::bail!("native Kbuild interaction timeline differs from its process stream");
    }
    let (interaction_roots, retained_candidate_paths) =
        retained_native_components_and_paths(&context.components)?;
    let (logical_components, union_edges) =
        coalesce_native_components_with_union_edges_for_roots(
            &context.components,
            &context.component_members,
            &context.timeline,
            &interactions,
            &interaction_roots,
            &retained_candidate_paths,
        )?;
    let component = logical_components
        .into_iter()
        .find(|component| component.roots.contains(&start))
        .context("native Kbuild union diagnostic start root has no logical component")?;
    if !component.roots.contains(&finish) {
        anyhow::bail!(
            "native Kbuild union diagnostic roots are not in one logical component"
        );
    }
    let chain = native_union_chain(&union_edges, start, finish);
    if chain.first().is_some_and(|entry| {
        entry == "no recorded union chain" || entry == "recorded union chain is incomplete"
    }) {
        anyhow::bail!("native Kbuild logical component has no complete recorded union chain");
    }
    Ok(KbuildNativeUnionChainDiagnostic {
        start,
        finish,
        component_roots: component.roots,
        recorded_union_edges: u64::try_from(union_edges.len())
            .context("native Kbuild union edge count does not fit u64")?,
        chain,
    })
}

fn retained_native_component_roots(
    components: &BTreeMap<CapturedProcessId, BTreeSet<PathBuf>>,
) -> Result<BTreeSet<CapturedProcessId>> {
    Ok(retained_native_components_and_paths(components)?.0)
}

fn retained_native_components_and_paths(
    components: &BTreeMap<CapturedProcessId, BTreeSet<PathBuf>>,
) -> Result<(BTreeSet<CapturedProcessId>, BTreeSet<PathBuf>)> {
    let mut retained = BTreeSet::new();
    let mut retained_paths = BTreeSet::new();
    for (root, paths) in components {
        for path in paths {
            match std::fs::symlink_metadata(path) {
                Ok(_) => {
                    retained.insert(*root);
                    retained_paths.insert(path.clone());
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).context("stat native Kbuild mutation candidate");
                }
            }
        }
    }
    Ok((retained, retained_paths))
}

fn validate_logical_component_contexts(
    components: &[KbuildNativeLogicalComponent],
    processes: &BTreeMap<CapturedProcessId, &CapturedProcess>,
) -> Result<()> {
    let mut signal_state = false;
    let mut file_creation_mask = false;
    let mut credential_state = false;
    for component in components {
        let first_root = component
            .roots
            .first()
            .context("native Kbuild logical component has no command roots")?;
        let first_invocation = processes
            .get(first_root)
            .context("native Kbuild logical component root is absent")?
            .invocations
            .first()
            .context("native Kbuild logical component root has no invocation")?;
        let first_command = graph_invocation(first_invocation)?;
        captured_environment(&first_command, "native action")?;
        for root in component.roots.iter().skip(1) {
            let invocation = processes
                .get(root)
                .context("native Kbuild logical component root is absent")?
                .invocations
                .first()
                .context("native Kbuild logical component root has no invocation")?;
            let command = graph_invocation(invocation)?;
            captured_environment(&command, "native action")?;
            signal_state |= invocation.signal_state != first_invocation.signal_state;
            file_creation_mask |=
                invocation.file_creation_mask != first_invocation.file_creation_mask;
            credential_state |= invocation.credentials != first_invocation.credentials;
        }
    }
    let mut mismatches = Vec::new();
    if signal_state {
        mismatches.push("signal state");
    }
    if file_creation_mask {
        mismatches.push("file-creation mask");
    }
    if credential_state {
        mismatches.push("credential state");
    }
    if !mismatches.is_empty() {
        anyhow::bail!(
            "native Kbuild compound actions have mismatched {}",
            mismatches.join(", ")
        );
    }
    Ok(())
}

/// Partition every native mutation component without exposing private paths.
pub fn census_kbuild_native_components(
    process_capture: &KbuildProcessCapture,
    output_root: &Path,
) -> Result<KbuildNativeComponentCensus> {
    let output_root = canonical_root(output_root, "output")?;
    let context = load_kbuild_native_components(process_capture, &output_root)?;
    let mutation_components = u64::try_from(context.components.len())
        .context("native Kbuild component count does not fit u64")?;
    let mut path_owners = BTreeMap::<PathBuf, u64>::new();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rush.kbuild-native-component-census.v1\0");
    let output_root_bytes = os_bytes(output_root.as_os_str());
    hasher.update(
        &u64::try_from(output_root_bytes.len())
            .context("native Kbuild output-root length does not fit u64")?
            .to_le_bytes(),
    );
    hasher.update(&output_root_bytes);
    hasher.update(&context.stream.digest());
    hasher.update(&mutation_components.to_le_bytes());
    let mut candidate_path_references = 0_u64;
    let mut receiptless_components = 0_u64;
    let mut single_receipt_components = 0_u64;
    let mut ambiguous_receipt_components = 0_u64;
    let mut transient_components = 0_u64;
    let mut retained_regular_only_components = 0_u64;
    let mut retained_nonregular_components = 0_u64;
    let mut retained_regular_files = 0_u64;
    let mut retained_directories = 0_u64;
    let mut retained_symlinks = 0_u64;
    let mut retained_special_entries = 0_u64;
    let mut missing_paths = 0_u64;
    for (root, paths) in &context.components {
        hasher.update(&root.to_le_bytes());
        let path_count = u64::try_from(paths.len())
            .context("native Kbuild component path count does not fit u64")?;
        hasher.update(&path_count.to_le_bytes());
        candidate_path_references = candidate_path_references
            .checked_add(path_count)
            .context("native Kbuild candidate-path count overflowed")?;
        let mut receipts = 0_u64;
        let mut retained_regular = 0_u64;
        let mut retained_nonregular = 0_u64;
        for path in paths {
            let path_bytes = os_bytes(path.as_os_str());
            hasher.update(
                &u64::try_from(path_bytes.len())
                    .context("native Kbuild component path length does not fit u64")?
                    .to_le_bytes(),
            );
            hasher.update(&path_bytes);
            let owners = path_owners.entry(path.clone()).or_default();
            *owners = owners
                .checked_add(1)
                .context("native Kbuild path-owner count overflowed")?;
            match std::fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    increment(&mut retained_symlinks, "native Kbuild symlink count")?;
                    increment(
                        &mut retained_nonregular,
                        "native Kbuild retained-entry count",
                    )?;
                    hasher.update(&[2]);
                }
                Ok(metadata) if metadata.is_dir() => {
                    increment(&mut retained_directories, "native Kbuild directory count")?;
                    increment(
                        &mut retained_nonregular,
                        "native Kbuild retained-entry count",
                    )?;
                    hasher.update(&[3]);
                }
                Ok(metadata) if metadata.is_file() => {
                    increment(
                        &mut retained_regular_files,
                        "native Kbuild regular-file count",
                    )?;
                    increment(&mut retained_regular, "native Kbuild retained-entry count")?;
                    let receipt = is_command_receipt(path);
                    if receipt {
                        increment(&mut receipts, "native Kbuild component receipt count")?;
                    }
                    hasher.update(&[1, u8::from(receipt)]);
                }
                Ok(_) => {
                    increment(
                        &mut retained_special_entries,
                        "native Kbuild special-entry count",
                    )?;
                    increment(
                        &mut retained_nonregular,
                        "native Kbuild retained-entry count",
                    )?;
                    hasher.update(&[4]);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    increment(&mut missing_paths, "native Kbuild missing-path count")?;
                    hasher.update(&[0]);
                }
                Err(error) => {
                    return Err(error).context("stat native Kbuild component candidate");
                }
            }
        }
        match receipts {
            0 => increment(
                &mut receiptless_components,
                "native Kbuild receiptless-component count",
            )?,
            1 => increment(
                &mut single_receipt_components,
                "native Kbuild single-receipt-component count",
            )?,
            _ => increment(
                &mut ambiguous_receipt_components,
                "native Kbuild ambiguous-receipt-component count",
            )?,
        }
        if retained_regular == 0 && retained_nonregular == 0 {
            increment(
                &mut transient_components,
                "native Kbuild transient-component count",
            )?;
        } else if retained_nonregular == 0 {
            increment(
                &mut retained_regular_only_components,
                "native Kbuild regular-only-component count",
            )?;
        } else {
            increment(
                &mut retained_nonregular_components,
                "native Kbuild nonregular-component count",
            )?;
        }
        hasher.update(&receipts.to_le_bytes());
        hasher.update(&retained_regular.to_le_bytes());
        hasher.update(&retained_nonregular.to_le_bytes());
    }
    let unique_candidate_paths = u64::try_from(path_owners.len())
        .context("native Kbuild unique-path count does not fit u64")?;
    let multiply_owned_candidate_paths =
        u64::try_from(path_owners.values().filter(|owners| **owners > 1).count())
            .context("native Kbuild multiply-owned path count does not fit u64")?;
    let maximum_path_owners = path_owners.values().copied().max().unwrap_or(0);
    let owner_references = path_owners.values().try_fold(0_u64, |total, owners| {
        total
            .checked_add(*owners)
            .context("native Kbuild path-owner reference count overflowed")
    })?;
    let receipt_partition = checked_sum(
        &[
            receiptless_components,
            single_receipt_components,
            ambiguous_receipt_components,
        ],
        "native Kbuild receipt-component partition",
    )?;
    let retention_partition = checked_sum(
        &[
            transient_components,
            retained_regular_only_components,
            retained_nonregular_components,
        ],
        "native Kbuild retained-component partition",
    )?;
    let candidate_partition = checked_sum(
        &[
            retained_regular_files,
            retained_directories,
            retained_symlinks,
            retained_special_entries,
            missing_paths,
        ],
        "native Kbuild candidate-path partition",
    )?;
    if receipt_partition != mutation_components
        || retention_partition != mutation_components
        || candidate_partition != candidate_path_references
        || owner_references != candidate_path_references
        || unique_candidate_paths > candidate_path_references
        || multiply_owned_candidate_paths > unique_candidate_paths
        || maximum_path_owners > mutation_components
        || (mutation_components == 0) != (candidate_path_references == 0)
        || (unique_candidate_paths == 0) != (maximum_path_owners == 0)
        || (multiply_owned_candidate_paths == 0) != (maximum_path_owners <= 1)
    {
        anyhow::bail!("native Kbuild component census is internally inconsistent");
    }
    for count in [
        candidate_path_references,
        unique_candidate_paths,
        multiply_owned_candidate_paths,
        maximum_path_owners,
        receiptless_components,
        single_receipt_components,
        ambiguous_receipt_components,
        transient_components,
        retained_regular_only_components,
        retained_nonregular_components,
        retained_regular_files,
        retained_directories,
        retained_symlinks,
        retained_special_entries,
        missing_paths,
    ] {
        hasher.update(&count.to_le_bytes());
    }
    Ok(KbuildNativeComponentCensus {
        stream_digest: context.stream.digest(),
        census_digest: *hasher.finalize().as_bytes(),
        mutation_components,
        candidate_path_references,
        unique_candidate_paths,
        multiply_owned_candidate_paths,
        maximum_path_owners,
        receiptless_components,
        single_receipt_components,
        ambiguous_receipt_components,
        transient_components,
        retained_regular_only_components,
        retained_nonregular_components,
        retained_regular_files,
        retained_directories,
        retained_symlinks,
        retained_special_entries,
        missing_paths,
    })
}

/// Derive a candidate Kbuild graph from one complete native process stream.
///
/// Recursive invocations of the root analyzer executable and their ancestor
/// paths remain analyzer-owned. Each remaining mutation-owning process
/// component with a retained path must have one invocation-zero root. Every
/// retained mutation path becomes a declared output. A compound action uses
/// retained Kbuild receipt from its final mutation component must identify
/// exactly one command in the ordered compound action; receipts from earlier
/// interleaved components remain ordinary outputs. Components whose paths are
/// all absent remain analyzer-owned helpers
/// and are admitted later only if the stream proves that every one of their
/// mutation candidates is transient. Unsupported mutation semantics, final
/// receipt ambiguity, multiple path owners, and incomplete observer coverage
/// all reject before any graph artifact is written.
pub fn derive_kbuild_action_graph(
    process_capture: &KbuildProcessCapture,
    output_root: &Path,
    requested_goals: Vec<Vec<u8>>,
    requested_outputs: Vec<PathBuf>,
) -> Result<KbuildNativeGraphDerivation> {
    if requested_goals.is_empty() || requested_outputs.is_empty() {
        anyhow::bail!("native Kbuild graph request has no goal or output");
    }
    let output_root = canonical_root(output_root, "output")?;
    let context = load_kbuild_native_components(process_capture, &output_root)?;
    let (interactions, interaction_stream) = visit_process_capture_stream(
        &process_capture.stream_path,
        process_capture.summary,
        process_capture.limits()?,
        KbuildNativeInteractionTimeline::new(output_root.clone(), &context.component_members)?,
    )
    .context("load native Kbuild action interactions")?;
    if interaction_stream != context.stream
        || interactions.stream_ordinal != context.stream.record_count()
    {
        anyhow::bail!("native Kbuild interaction timeline differs from its process stream");
    }
    let (interaction_roots, retained_candidate_paths) =
        retained_native_components_and_paths(&context.components)?;
    let logical_components = coalesce_native_components_with_union_edges_for_roots(
        &context.components,
        &context.component_members,
        &context.timeline,
        &interactions,
        &interaction_roots,
        &retained_candidate_paths,
    )?
    .0;
    let analyzer_processes = context.analyzer_processes;
    let rustc_processes = context.rustc_processes;
    let receipt = context.receipt;
    let stream = context.stream;
    let analyzer = context.analyzer;
    let mutation_components = context.components;
    let processes = receipt
        .processes()
        .iter()
        .map(|process| (process.id, process))
        .collect::<BTreeMap<_, _>>();
    validate_logical_component_contexts(&logical_components, &processes)?;

    let mut actions = Vec::new();
    let mut boundaries = Vec::new();
    let mut owned_paths = BTreeMap::new();
    let mut receipts = 0_u64;
    let mut command_count = 0_u64;
    for component in logical_components {
        let final_root = *component
            .roots
            .last()
            .context("native Kbuild logical component has no final command root")?;
        let mut persistent_outputs = Vec::new();
        for path in &component.candidate_paths {
            match std::fs::symlink_metadata(path) {
                Ok(metadata)
                    if metadata.is_file()
                        || metadata.is_dir()
                        || metadata.file_type().is_symlink() =>
                {
                    persistent_outputs.push(path.clone());
                }
                Ok(_) => {
                    anyhow::bail!("native Kbuild action retained a special mutation candidate")
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error).context("stat native Kbuild mutation candidate");
                }
            }
            if owned_paths.insert(path.clone(), actions.len()).is_some() {
                anyhow::bail!("native Kbuild mutation candidate has multiple action owners");
            }
        }
        if persistent_outputs.is_empty() {
            continue;
        }
        let mut commands = Vec::with_capacity(component.roots.len());
        let mut command_boundaries = Vec::with_capacity(component.roots.len());
        let mut expected_signal_state = None;
        let mut expected_file_creation_mask = None;
        let mut expected_credentials = None;
        for action_process in component.roots {
            let process = processes
                .get(&action_process)
                .copied()
                .context("native Kbuild action process is absent")?;
            if process.outcome != CapturedProcessOutcome::Exited(0) {
                anyhow::bail!("native Kbuild action boundary did not exit successfully");
            }
            let invocation = process
                .invocations
                .first()
                .context("native Kbuild action boundary has no invocation")?;
            let command = graph_invocation(invocation)?;
            captured_environment(&command, "native action")?;
            if expected_signal_state.get_or_insert(invocation.signal_state)
                != &invocation.signal_state
            {
                anyhow::bail!("native Kbuild compound action has mismatched signal state");
            }
            if expected_file_creation_mask.get_or_insert(invocation.file_creation_mask)
                != &invocation.file_creation_mask
            {
                anyhow::bail!("native Kbuild compound action has mismatched file-creation mask");
            }
            if expected_credentials.get_or_insert_with(|| invocation.credentials.clone())
                != &invocation.credentials
            {
                anyhow::bail!("native Kbuild compound action has mismatched credential state");
            }
            commands.push(command);
            command_boundaries.push(KbuildInvocationBoundary {
                process: action_process,
                invocation_index: 0,
            });
            increment(&mut command_count, "native Kbuild command-boundary count")?;
        }
        let receipt_paths = mutation_components
            .get(&final_root)
            .context("native Kbuild final command has no mutation component")?
            .iter()
            .filter(|path| persistent_outputs.contains(path) && is_command_receipt(path))
            .cloned()
            .collect::<Vec<_>>();
        if receipt_paths.len() > 1 {
            anyhow::bail!(
                "native Kbuild final command component has {} retained command receipts",
                receipt_paths.len()
            );
        }
        let receipt_path = receipt_paths.into_iter().next();
        if let Some(receipt_path) = &receipt_path {
            let command_receipt = parse_kbuild_receipt(
                &std::fs::read(receipt_path).context("read native Kbuild command receipt")?,
            )
            .context("parse native Kbuild command receipt")?;
            let evidence_matches =
                command_receipt_match_indices(&commands, &command_receipt.command);
            if command_receipt_match_index(&commands, &command_receipt.command).is_none() {
                let final_argv = commands
                    .last()
                    .context("native Kbuild logical action has no commands")?
                    .argv
                    .iter()
                    .map(|argument| String::from_utf8_lossy(argument).into_owned())
                    .collect::<Vec<_>>();
                anyhow::bail!(
                    "native Kbuild command receipt matches {} compound commands: receipt {}, \
                     target {}, saved command {:?}, final argv {final_argv:?}",
                    evidence_matches.len(),
                    receipt_path.display(),
                    String::from_utf8_lossy(&command_receipt.target),
                    String::from_utf8_lossy(&command_receipt.command),
                );
            }
            let target = normalized_output_target(&output_root, &command_receipt.target)?;
            if !persistent_outputs.contains(&target) {
                anyhow::bail!("native Kbuild action mutations omit its receipt target");
            }
            increment(&mut receipts, "native Kbuild retained receipt count")?;
        }
        persistent_outputs
            .sort_by(|left, right| os_bytes(left.as_os_str()).cmp(&os_bytes(right.as_os_str())));
        let action_index =
            u64::try_from(actions.len()).context("native Kbuild action count does not fit u64")?;
        actions.push(KbuildActionCapture {
            schema: KBUILD_CAPTURE_SCHEMA.to_string(),
            analyzer: analyzer.clone(),
            commands,
            receipt_path,
            action_inputs: Vec::new(),
            persistent_outputs,
            analysis_inputs: Vec::new(),
            requested_goals: requested_goals.clone(),
        });
        boundaries.push(KbuildActionBoundary {
            action_index,
            commands: command_boundaries,
        });
    }
    if actions.is_empty() {
        anyhow::bail!("native Kbuild graph derivation found no retained actions");
    }
    let requested_outputs = requested_outputs
        .into_iter()
        .map(|path| {
            let path = normalized_absolute_path(&path, "requested output")?;
            let relative = path
                .strip_prefix(&output_root)
                .context("native Kbuild requested output is outside the output root")?;
            if relative.as_os_str().is_empty() {
                anyhow::bail!("native Kbuild requested output names the output root");
            }
            Ok(path)
        })
        .collect::<Result<Vec<_>>>()?;
    let action_count =
        u64::try_from(actions.len()).context("native Kbuild action count does not fit u64")?;
    let receiptless_actions = action_count
        .checked_sub(receipts)
        .context("native Kbuild receipt count exceeds its action count")?;
    let mut certified_runtime_mutation_paths = BTreeSet::new();
    let mut unmodeled_observations = 0_u64;
    let mut first_unmodeled = None;
    for process in receipt.processes() {
        let analyzer_process = analyzer_processes.contains(&process.id);
        let rustc_process = rustc_processes.contains(&process.id);
        let make_process = process_executes_named(process, b"make");
        let xargs_process = process_executes_named(process, b"xargs");
        let m4_process = process_executes_named(process, b"m4");
        let cat_process = process_executes_named(process, b"cat");
        let objtool_process = process_executes_named(process, b"objtool");
        let sed_process = process_executes_named(process, b"sed");
        let exact_kallsyms_ls_process = process.invocations.len() == 1
            && exact_kallsyms_ls_invocation(&process.invocations[0]);
        for attempt in &process.unmodeled_syscall_attempts {
            let make_jobserver_fifo_target = certified_make_jobserver_fifo_runtime_target(
                attempt,
                analyzer_process,
                make_process,
                &process.open_attempts,
                &process.path_removal_attempts,
            );
            if let Some(target) = &make_jobserver_fifo_target {
                certified_runtime_mutation_paths.insert(PathBuf::from(
                    <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(target),
                ));
            }
            if supported_kbuild_runtime_syscall(
                attempt,
                analyzer_process,
                rustc_process,
                make_process,
                xargs_process,
            ) || make_jobserver_fifo_target.is_some()
                || supported_m4_self_procmap_runtime_syscall(
                attempt,
                m4_process,
                &process.open_attempts,
                &process.close_attempts,
                &process.dup2_attempts,
            ) || supported_kallsyms_ls_xattr_runtime_syscall(
                attempt,
                exact_kallsyms_ls_process,
            ) || supported_cat_copy_file_range_runtime_syscall(
                attempt,
                cat_process,
                &process.open_attempts,
                &process.write_attempts,
                &process.close_attempts,
                &process.dup2_attempts,
            ) || supported_objtool_runtime_syscall(
                attempt,
                objtool_process,
                &process.mmap_attempts,
                &process.write_attempts,
                &process.close_attempts,
                &process.dup2_attempts,
            ) || supported_sed_runtime_syscall(
                attempt,
                sed_process,
                process
                    .invocations
                    .last()
                    .map(|invocation| &invocation.credentials),
                &process.write_attempts,
                &process.close_attempts,
                &process.dup2_attempts,
            ) {
                continue;
            }
            unmodeled_observations = unmodeled_observations
                .checked_add(1)
                .context("native Kbuild unmodeled-observation count overflowed")?;
            first_unmodeled.get_or_insert((process, attempt));
        }
    }
    if unmodeled_observations != 0 {
        let (process, attempt) = first_unmodeled
            .context("native Kbuild unmodeled count has no corresponding observation")?;
        let executables = process
            .invocations
            .iter()
            .map(|invocation| String::from_utf8_lossy(&invocation.executable).into_owned())
            .collect::<Vec<_>>();
        anyhow::bail!(
            "native Kbuild graph derivation has {unmodeled_observations} \
             unmodeled observations; first process {:?}, executables {executables:?}, \
             analyzer {}, rustc {}, make {}, xargs {}, syscall {}, arguments {:?}, outcome {:?}",
            process.id,
            analyzer_processes.contains(&process.id),
            rustc_processes.contains(&process.id),
            process_executes_named(process, b"make"),
            process_executes_named(process, b"xargs"),
            attempt.number,
            attempt.arguments,
            attempt.outcome,
        );
    }
    let mutation_paths = native_mutation_path_coverage_count(
        &owned_paths,
        &certified_runtime_mutation_paths,
    )?;
    Ok(KbuildNativeGraphDerivation {
        graph: KbuildActionGraphCapture {
            schema: KBUILD_GRAPH_CAPTURE_SCHEMA.to_string(),
            actions,
            requested_outputs,
            initial_inputs: Vec::new(),
            empty_output_directory_listings: Vec::new(),
        },
        process_boundaries: KbuildProcessBoundaryDeclaration {
            analyzer: KbuildInvocationBoundary {
                process: receipt.root(),
                invocation_index: 0,
            },
            actions: boundaries,
        },
        coverage: KbuildGraphCaptureCoverage {
            process_lifecycles_observed: stream.process_count(),
            stream_records_observed: stream.record_count(),
            stream_records_classified: stream.record_count(),
            command_boundaries_observed: command_count,
            command_boundaries_captured: command_count,
            logical_actions_captured: action_count,
            receipts_discovered: receipts,
            receipts_ingested: receipts,
            receiptless_actions,
            persistent_writes_observed: mutation_paths,
            persistent_writes_attributed: mutation_paths,
            unmodeled_observations,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unmodeled_attempt(
        number: u64,
        arguments: [u64; 6],
        return_value: i64,
        is_error: bool,
    ) -> rush_observe::CapturedUnmodeledSyscallAttempt {
        rush_observe::CapturedUnmodeledSyscallAttempt {
            syscall_index: 1,
            number,
            arguments,
            outcome: rush_observe::CapturedUnmodeledSyscallOutcome::Returned {
                return_value,
                is_error,
            },
        }
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn admits_only_a_witnessed_transient_make_jobserver_fifo() {
        let target = b"/workspace/obj/.rush-tmp/GMfifo12345".to_vec();
        let identity = rush_observe::CapturedDescriptorIdentity {
            target: target.clone(),
            device: 7,
            inode: 11,
            kind: rush_observe::CapturedDescriptorKind::Fifo,
        };
        let opened = |syscall_index, descriptor, flags| rush_observe::CapturedOpenAttempt {
            syscall_index,
            request: rush_observe::CapturedOpenRequest {
                system_call: rush_observe::CapturedOpenSystemCall::OpenAt,
                path: target.clone(),
                directory: None,
                flags,
                mode: 0,
                resolve: 0,
            },
            outcome: rush_observe::CapturedOpenOutcome::Opened {
                descriptor,
                identity: identity.clone(),
                mount_id: 9,
                file_value: None,
            },
        };
        let opens = [
            opened(11, 3, (libc::O_RDONLY | libc::O_NONBLOCK) as u64),
            opened(12, 4, libc::O_WRONLY as u64),
        ];
        let removal = rush_observe::CapturedPathRemovalAttempt {
            syscall_index: 20,
            request: rush_observe::CapturedPathRemovalRequest {
                system_call: rush_observe::CapturedPathRemovalSystemCall::Unlink,
                directory_descriptor: libc::AT_FDCWD,
                path: target.clone(),
                path_is_null: false,
                base: None,
                flags: 0,
            },
            outcome: rush_observe::CapturedPathRemovalOutcome::Removed,
        };
        let mknodat = rush_observe::CapturedUnmodeledSyscallAttempt {
            syscall_index: 10,
            ..unmodeled_attempt(
                libc::SYS_mknodat as u64,
                [
                    libc::AT_FDCWD as u32 as u64,
                    0x4000,
                    libc::S_IFIFO as u64 | 0o600,
                    0,
                    0,
                    0,
                ],
                0,
                false,
            )
        };
        assert_eq!(
            certified_make_jobserver_fifo_runtime_target(
                &mknodat,
                true,
                true,
                &opens,
                std::slice::from_ref(&removal)
            ),
            Some(target.clone())
        );
        assert!(supported_make_jobserver_fifo_runtime_syscall(
            &mknodat,
            true,
            true,
            &opens,
            std::slice::from_ref(&removal)
        ));
        assert!(!supported_make_jobserver_fifo_runtime_syscall(
            &mknodat,
            true,
            true,
            &opens,
            &[]
        ));
        let mut persistent_name = removal.clone();
        persistent_name.request.path = b"/workspace/obj/.rush-tmp/not-a-jobserver".to_vec();
        assert!(!supported_make_jobserver_fifo_runtime_syscall(
            &mknodat,
            true,
            true,
            &opens,
            std::slice::from_ref(&persistent_name)
        ));
        let mut wrong_mode = mknodat;
        wrong_mode.arguments[2] = libc::S_IFIFO as u64 | 0o666;
        assert!(!supported_make_jobserver_fifo_runtime_syscall(
            &wrong_mode,
            true,
            true,
            &opens,
            std::slice::from_ref(&removal)
        ));
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn admits_only_m4_queries_on_its_continuous_self_maps_descriptor() {
        let identity = rush_observe::CapturedDescriptorIdentity {
            target: b"/proc/12345/maps".to_vec(),
            device: 7,
            inode: 13,
            kind: rush_observe::CapturedDescriptorKind::RegularFile,
        };
        let open = rush_observe::CapturedOpenAttempt {
            syscall_index: 10,
            request: rush_observe::CapturedOpenRequest {
                system_call: rush_observe::CapturedOpenSystemCall::OpenAt,
                path: b"/proc/self/maps".to_vec(),
                directory: None,
                flags: (libc::O_RDONLY | libc::O_CLOEXEC) as u64,
                mode: 0,
                resolve: 0,
            },
            outcome: rush_observe::CapturedOpenOutcome::Opened {
                descriptor: 4,
                identity: identity.clone(),
                mount_id: 17,
                file_value: None,
            },
        };
        let close = rush_observe::CapturedCloseAttempt {
            syscall_index: 30,
            request: rush_observe::CapturedCloseRequest {
                descriptor: 4,
                identity: Some(identity),
                mount_id: Some(17),
            },
            outcome: rush_observe::CapturedCloseOutcome::Closed { file_value: None },
        };
        let query = rush_observe::CapturedUnmodeledSyscallAttempt {
            syscall_index: 20,
            ..unmodeled_attempt(
                libc::SYS_ioctl as u64,
                [4, 0xc068_6611, 0x4000, 0, 0, 0],
                0,
                false,
            )
        };
        assert!(supported_m4_self_procmap_runtime_syscall(
            &query,
            true,
            std::slice::from_ref(&open),
            std::slice::from_ref(&close),
            &[]
        ));
        assert!(!supported_m4_self_procmap_runtime_syscall(
            &query,
            false,
            std::slice::from_ref(&open),
            std::slice::from_ref(&close),
            &[]
        ));
        let mut other_maps = open.clone();
        other_maps.request.path = b"/proc/1/maps".to_vec();
        assert!(!supported_m4_self_procmap_runtime_syscall(
            &query,
            true,
            std::slice::from_ref(&other_maps),
            std::slice::from_ref(&close),
            &[]
        ));
        let mut different_command = query;
        different_command.arguments[1] = libc::TIOCGWINSZ as u64;
        assert!(!supported_m4_self_procmap_runtime_syscall(
            &different_command,
            true,
            std::slice::from_ref(&open),
            std::slice::from_ref(&close),
            &[]
        ));
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn admits_only_fully_witnessed_cat_copy_file_range_attempts() {
        for source_descriptor in [3, 5, 17] {
            let value = rush_observe::CapturedMappedFileValue::WholeFile {
                bytes: 4,
                digest: [7; 32],
                mode: 0o100644,
            };
            let source = rush_observe::CapturedDescriptorIdentity {
                target: b"/workspace/obj/input".to_vec(),
                device: 1,
                inode: 2,
                kind: rush_observe::CapturedDescriptorKind::RegularFile,
            };
            let destination = rush_observe::CapturedDescriptorIdentity {
                target: b"/workspace/obj/output".to_vec(),
                device: 1,
                inode: 3,
                kind: rush_observe::CapturedDescriptorKind::RegularFile,
            };
            let open = rush_observe::CapturedOpenAttempt {
                syscall_index: 10,
                request: rush_observe::CapturedOpenRequest {
                    system_call: rush_observe::CapturedOpenSystemCall::OpenAt,
                    path: b"input".to_vec(),
                    directory: Some(b"/workspace/obj".to_vec()),
                    flags: libc::O_RDONLY as u64,
                    mode: 0,
                    resolve: 0,
                },
                outcome: rush_observe::CapturedOpenOutcome::Opened {
                    descriptor: source_descriptor,
                    identity: source.clone(),
                    mount_id: 5,
                    file_value: Some(value),
                },
            };
            let source_close = rush_observe::CapturedCloseAttempt {
                syscall_index: 40,
                request: rush_observe::CapturedCloseRequest {
                    descriptor: source_descriptor,
                    identity: Some(source),
                    mount_id: Some(5),
                },
                outcome: rush_observe::CapturedCloseOutcome::Closed {
                    file_value: Some(value),
                },
            };
            let destination_close = rush_observe::CapturedCloseAttempt {
                syscall_index: 41,
                request: rush_observe::CapturedCloseRequest {
                    descriptor: 1,
                    identity: Some(destination.clone()),
                    mount_id: Some(5),
                },
                outcome: rush_observe::CapturedCloseOutcome::Closed {
                    file_value: Some(value),
                },
            };
            let closes = [source_close.clone(), destination_close.clone()];
            let copy = rush_observe::CapturedUnmodeledSyscallAttempt {
                syscall_index: 20,
                ..unmodeled_attempt(
                    libc::SYS_copy_file_range as u64,
                    [source_descriptor as u64, 0, 1, 0, 0x7ff0_0000, 0],
                    4,
                    false,
                )
            };
            assert!(supported_cat_copy_file_range_runtime_syscall(
                &copy,
                true,
                std::slice::from_ref(&open),
                &[],
                &closes,
                &[]
            ));
            let mut eof = copy.clone();
            eof.outcome = rush_observe::CapturedUnmodeledSyscallOutcome::Returned {
                return_value: 0,
                is_error: false,
            };
            assert!(supported_cat_copy_file_range_runtime_syscall(
                &eof,
                true,
                std::slice::from_ref(&open),
                &[],
                &closes,
                &[]
            ));

            let fallback = rush_observe::CapturedWriteAttempt {
                syscall_index: 30,
                request: rush_observe::CapturedWriteRequest {
                    system_call: rush_observe::CapturedWriteSystemCall::Write,
                    descriptor: 1,
                    identity: Some(destination),
                    mount_id: Some(5),
                    uses_descriptor_offset: true,
                    offset: Some(0),
                    requested_bytes: 4,
                    append: true,
                },
                outcome: rush_observe::CapturedWriteOutcome::Wrote {
                    bytes: 4,
                    digest: [7; 32],
                },
            };
            let mut append_probe = copy.clone();
            append_probe.outcome = rush_observe::CapturedUnmodeledSyscallOutcome::Returned {
                return_value: -libc::EBADF as i64,
                is_error: true,
            };
            assert!(supported_cat_copy_file_range_runtime_syscall(
                &append_probe,
                true,
                std::slice::from_ref(&open),
                std::slice::from_ref(&fallback),
                &closes,
                &[]
            ));
            assert!(!supported_cat_copy_file_range_runtime_syscall(
                &append_probe,
                true,
                std::slice::from_ref(&open),
                &[],
                &closes,
                &[]
            ));
            assert!(!supported_cat_copy_file_range_runtime_syscall(
                &copy,
                false,
                std::slice::from_ref(&open),
                &[],
                &closes,
                &[]
            ));
            let mut concatenated_destination = destination_close;
            concatenated_destination.outcome = rush_observe::CapturedCloseOutcome::Closed {
                file_value: Some(rush_observe::CapturedMappedFileValue::WholeFile {
                    bytes: 9,
                    digest: [8; 32],
                    mode: 0o100644,
                }),
            };
            assert!(supported_cat_copy_file_range_runtime_syscall(
                &copy,
                true,
                std::slice::from_ref(&open),
                &[],
                &[source_close.clone(), concatenated_destination],
                &[]
            ));
            assert!(!supported_cat_copy_file_range_runtime_syscall(
                &copy,
                true,
                std::slice::from_ref(&open),
                &[],
                std::slice::from_ref(&source_close),
                &[]
            ));
            // The source descriptor must match both the witnessed open and close.
            let mut wrong_descriptor = append_probe.clone();
            wrong_descriptor.arguments[0] = source_descriptor as u64 + 1;
            assert!(!supported_cat_copy_file_range_runtime_syscall(
                &wrong_descriptor,
                true,
                std::slice::from_ref(&open),
                std::slice::from_ref(&fallback),
                &closes,
                &[]
            ));
            let mut wrong_close = source_close.clone();
            wrong_close.request.descriptor = source_descriptor + 1;
            assert!(!supported_cat_copy_file_range_runtime_syscall(
                &append_probe,
                true,
                std::slice::from_ref(&open),
                std::slice::from_ref(&fallback),
                &[wrong_close, closes[1].clone()],
                &[]
            ));
            let mut premature_close = source_close.clone();
            premature_close.syscall_index = 15;
            assert!(!supported_cat_copy_file_range_runtime_syscall(
                &append_probe,
                true,
                std::slice::from_ref(&open),
                std::slice::from_ref(&fallback),
                &[premature_close, source_close.clone(), closes[1].clone()],
                &[]
            ));
            let replacement = rush_observe::CapturedDup2Attempt {
                syscall_index: 15,
                request: rush_observe::CapturedDup2Request {
                    system_call: rush_observe::CapturedDescriptorDuplicationSystemCall::Dup2,
                    source_descriptor: 18,
                    source_identity: source_close.request.identity.clone(),
                    source_mount_id: Some(5),
                    target_descriptor: source_descriptor,
                    target_identity: source_close.request.identity.clone(),
                    target_mount_id: Some(5),
                },
                outcome: rush_observe::CapturedDup2Outcome::Duplicated {
                    descriptor: source_descriptor,
                    identity: source_close.request.identity.clone().unwrap(),
                    mount_id: 5,
                    replaced_file_value: Some(value),
                },
            };
            assert!(!supported_cat_copy_file_range_runtime_syscall(
                &append_probe,
                true,
                std::slice::from_ref(&open),
                std::slice::from_ref(&fallback),
                &closes,
                &[replacement]
            ));
            let mut wrong_digest = fallback.clone();
            wrong_digest.outcome = rush_observe::CapturedWriteOutcome::Wrote {
                bytes: 4,
                digest: [9; 32],
            };
            assert!(!supported_cat_copy_file_range_runtime_syscall(
                &append_probe,
                true,
                std::slice::from_ref(&open),
                std::slice::from_ref(&wrong_digest),
                &closes,
                &[]
            ));
            for descriptor in [0, 1, 2, u64::MAX, 0x1_0000_0005] {
                let mut invalid_source = append_probe.clone();
                invalid_source.arguments[0] = descriptor;
                assert!(!cat_copy_file_range_runtime_syscall_candidate(
                    &invalid_source,
                    true
                ));
            }
        }
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn classifies_only_bounded_kbuild_runtime_syscalls() {
        let empty_kallsyms_xattrs = unmodeled_attempt(
            libc::SYS_llistxattr as u64,
            [0x1000, 0x2000, 152, 606, 1, 20800],
            0,
            false,
        );
        assert!(supported_kallsyms_ls_xattr_runtime_syscall(
            &empty_kallsyms_xattrs,
            true
        ));
        assert!(!supported_kallsyms_ls_xattr_runtime_syscall(
            &empty_kallsyms_xattrs,
            false
        ));
        let mut xattrs_present = empty_kallsyms_xattrs.clone();
        xattrs_present.outcome = rush_observe::CapturedUnmodeledSyscallOutcome::Returned {
            return_value: 16,
            is_error: false,
        };
        assert!(!supported_kallsyms_ls_xattr_runtime_syscall(
            &xattrs_present,
            true
        ));
        let mut null_xattr_buffer = empty_kallsyms_xattrs;
        null_xattr_buffer.arguments[1] = 0;
        assert!(!supported_kallsyms_ls_xattr_runtime_syscall(
            &null_xattr_buffer,
            true
        ));
        let scheduler_query = unmodeled_attempt(
            libc::SYS_sched_getscheduler as u64,
            [0, 0x1000, 0, 32, u32::MAX as u64, 0],
            libc::SCHED_OTHER as i64,
            false,
        );
        assert!(supported_kbuild_runtime_syscall(
            &scheduler_query,
            false,
            false,
            false,
            false
        ));
        let mut other_process_scheduler = scheduler_query;
        other_process_scheduler.arguments[0] = 42;
        assert!(!supported_kbuild_runtime_syscall(
            &other_process_scheduler,
            false,
            false,
            false,
            false
        ));
        let pselect = unmodeled_attempt(
            libc::SYS_pselect6 as u64,
            [4, 0x1000, 0, 0, 0, 0x2000],
            1,
            false,
        );
        assert!(supported_kbuild_runtime_syscall(
            &pselect, true, false, false, false
        ));
        assert!(!supported_kbuild_runtime_syscall(
            &pselect, false, true, false, false
        ));
        let make_pselect = unmodeled_attempt(
            libc::SYS_pselect6 as u64,
            [5, 0x1000, 0, 0, 0, 0x2000],
            1,
            false,
        );
        assert!(supported_kbuild_runtime_syscall(
            &make_pselect,
            false,
            false,
            true,
            false
        ));
        assert!(supported_kbuild_runtime_syscall(
            &pselect, false, false, true, false
        ));
        assert!(!supported_kbuild_runtime_syscall(
            &make_pselect,
            false,
            false,
            false,
            false
        ));
        let restarted_make_pselect = unmodeled_attempt(
            libc::SYS_pselect6 as u64,
            [5, 0x1000, 0, 0, 0, 0x2000],
            -514,
            true,
        );
        assert!(supported_kbuild_runtime_syscall(
            &restarted_make_pselect,
            true,
            false,
            true,
            false
        ));
        assert!(!supported_kbuild_runtime_syscall(
            &restarted_make_pselect,
            false,
            false,
            false,
            false
        ));
        let xargs_poll =
            unmodeled_attempt(libc::SYS_poll as u64, [0x1000, 5, 0, 0, 0, 0], 5, false);
        assert!(!supported_kbuild_runtime_syscall(
            &xargs_poll,
            false,
            false,
            false,
            true
        ));
        assert!(!supported_kbuild_runtime_syscall(
            &xargs_poll,
            false,
            false,
            false,
            false
        ));

        let process_local = [
            unmodeled_attempt(
                libc::SYS_madvise as u64,
                [0x2000, 0x4000, libc::MADV_DONTNEED as u64, 0, 0, 0],
                0,
                false,
            ),
            unmodeled_attempt(
                libc::SYS_madvise as u64,
                [0x6000, 0x1000, libc::MADV_FREE as u64, 0, 0, 0],
                0,
                false,
            ),
            unmodeled_attempt(
                libc::SYS_madvise as u64,
                [0x8000, 0x1000, 102, 0, 0, 0],
                0,
                false,
            ),
            unmodeled_attempt(
                libc::SYS_futex as u64,
                [0x5000, 137, 0, 0, 0, u32::MAX as u64],
                0,
                false,
            ),
            unmodeled_attempt(
                libc::SYS_futex as u64,
                [0x5000, 128, 1, 0, u64::MAX, u64::MAX],
                0,
                false,
            ),
            unmodeled_attempt(
                libc::SYS_futex as u64,
                [0x5000, 137, 23, 0, 0, u32::MAX as u64],
                -libc::EINTR as i64,
                true,
            ),
            unmodeled_attempt(libc::SYS_sched_yield as u64, [u64::MAX; 6], 0, false),
            unmodeled_attempt(libc::SYS_fsync as u64, [4, 0, 0, 0, 0, 0], 0, false),
            unmodeled_attempt(
                libc::SYS_mremap as u64,
                [0x10000, 0x2000, 0x4000, libc::MREMAP_MAYMOVE as u64, 0, 0],
                0x20000,
                false,
            ),
        ];
        for attempt in &process_local {
            assert!(supported_kbuild_runtime_syscall(
                attempt, false, false, false, false
            ));
        }
        let timed_value_mismatch = unmodeled_attempt(
            libc::SYS_futex as u64,
            [0x5000, 137, 2, 0x6000, 0, 0x1234_5678_ffff_ffff],
            -libc::EAGAIN as i64,
            true,
        );
        assert!(supported_kbuild_runtime_syscall(
            &timed_value_mismatch,
            false,
            true,
            false,
            false
        ));
        let timed_success = unmodeled_attempt(
            libc::SYS_futex as u64,
            [0x5000, 137, 2, 0x6000, 0, u32::MAX as u64],
            0,
            false,
        );
        assert!(!supported_kbuild_runtime_syscall(
            &timed_success,
            false,
            true,
            false,
            false
        ));
        let timed_wrong_bitset = unmodeled_attempt(
            libc::SYS_futex as u64,
            [0x5000, 137, 2, 0x6000, 0, 1],
            -libc::EAGAIN as i64,
            true,
        );
        assert!(!supported_kbuild_runtime_syscall(
            &timed_wrong_bitset,
            false,
            true,
            false,
            false
        ));
        let mut terminated_wait = unmodeled_attempt(
            libc::SYS_futex as u64,
            [0x5000, 137, 23, 0, 0, u32::MAX as u64],
            0,
            false,
        );
        terminated_wait.outcome = rush_observe::CapturedUnmodeledSyscallOutcome::Terminated;
        assert!(supported_kbuild_runtime_syscall(
            &terminated_wait,
            false,
            false,
            false,
            false
        ));
        let remove_mapping = unmodeled_attempt(
            libc::SYS_madvise as u64,
            [0x2000, 0x4000, libc::MADV_REMOVE as u64, 0, 0, 0],
            0,
            false,
        );
        assert!(!supported_kbuild_runtime_syscall(
            &remove_mapping,
            false,
            false,
            false,
            false
        ));
        let remove_guard = unmodeled_attempt(
            libc::SYS_madvise as u64,
            [0x2000, 0x1000, 103, 0, 0, 0],
            0,
            false,
        );
        assert!(!supported_kbuild_runtime_syscall(
            &remove_guard,
            false,
            false,
            false,
            false
        ));
        let failed_yield = unmodeled_attempt(
            libc::SYS_sched_yield as u64,
            [0; 6],
            -libc::EINTR as i64,
            true,
        );
        assert!(!supported_kbuild_runtime_syscall(
            &failed_yield,
            false,
            false,
            false,
            false
        ));
        let failed_fsync = unmodeled_attempt(
            libc::SYS_fsync as u64,
            [4, 0, 0, 0, 0, 0],
            -libc::EIO as i64,
            true,
        );
        assert!(!supported_kbuild_runtime_syscall(
            &failed_fsync,
            false,
            false,
            false,
            false
        ));
        let fixed_mremap = unmodeled_attempt(
            libc::SYS_mremap as u64,
            [
                0x10000,
                0x2000,
                0x4000,
                (libc::MREMAP_MAYMOVE | libc::MREMAP_FIXED) as u64,
                0x30000,
                0,
            ],
            0x30000,
            false,
        );
        assert!(!supported_kbuild_runtime_syscall(
            &fixed_mremap,
            false,
            false,
            false,
            false
        ));
        let failed_tcgets2 = unmodeled_attempt(
            libc::SYS_ioctl as u64,
            [1, 0x802c_542a, 0x4000, 0, 0, 0],
            -libc::ENOTTY as i64,
            true,
        );
        assert!(supported_kbuild_runtime_syscall(
            &failed_tcgets2,
            true,
            false,
            true,
            false
        ));
        let mut successful_tcgets2 = failed_tcgets2.clone();
        successful_tcgets2.outcome = rush_observe::CapturedUnmodeledSyscallOutcome::Returned {
            return_value: 0,
            is_error: false,
        };
        assert!(!supported_kbuild_runtime_syscall(
            &successful_tcgets2,
            true,
            false,
            true,
            false
        ));
        let mut different_ioctl = failed_tcgets2.clone();
        different_ioctl.arguments[1] = libc::TIOCGWINSZ as u64;
        assert!(!supported_kbuild_runtime_syscall(
            &different_ioctl,
            true,
            false,
            true,
            false
        ));
        let mut null_output = failed_tcgets2;
        null_output.arguments[2] = 0;
        assert!(!supported_kbuild_runtime_syscall(
            &null_output,
            true,
            false,
            true,
            false
        ));
        let rustc_thread_signal = unmodeled_attempt(
            libc::SYS_tgkill as u64,
            [42, 43, libc::SIGUSR1 as u64, u64::MAX, u64::MAX, u64::MAX],
            0,
            false,
        );
        assert!(supported_kbuild_runtime_syscall(
            &rustc_thread_signal,
            false,
            true,
            false,
            false
        ));
        assert!(!supported_kbuild_runtime_syscall(
            &rustc_thread_signal,
            false,
            false,
            false,
            false
        ));
        let mut wrong_signal = rustc_thread_signal.clone();
        wrong_signal.arguments[2] = libc::SIGUSR2 as u64;
        assert!(!supported_kbuild_runtime_syscall(
            &wrong_signal,
            false,
            true,
            false,
            false
        ));
        let mut failed_signal = rustc_thread_signal;
        failed_signal.outcome = rush_observe::CapturedUnmodeledSyscallOutcome::Returned {
            return_value: -libc::ESRCH as i64,
            is_error: true,
        };
        assert!(!supported_kbuild_runtime_syscall(
            &failed_signal,
            false,
            true,
            false,
            false
        ));

        let rustc_runtime = [
            unmodeled_attempt(
                libc::SYS_sigaltstack as u64,
                [0, 0x3000, 0, 0, 0, 0],
                0,
                false,
            ),
            unmodeled_attempt(
                libc::SYS_prctl as u64,
                [libc::PR_SET_NAME as u64, 0x4000, 0, 0, 0, 0],
                0,
                false,
            ),
            unmodeled_attempt(
                libc::SYS_sched_getaffinity as u64,
                [42, 32, 0x6000, 0, 0, 0],
                16,
                false,
            ),
        ];
        for attempt in &rustc_runtime {
            assert!(supported_kbuild_runtime_syscall(
                attempt, false, true, false, false
            ));
            assert!(!supported_kbuild_runtime_syscall(
                attempt, false, false, false, false
            ));
        }

        let objtool_altstack = unmodeled_attempt(
            libc::SYS_sigaltstack as u64,
            [0x7000, 0, 0, 0, 0, 0],
            0,
            false,
        );
        assert!(supported_objtool_runtime_syscall(
            &objtool_altstack,
            true,
            &[],
            &[],
            &[],
            &[]
        ));
        assert!(!supported_objtool_runtime_syscall(
            &objtool_altstack,
            false,
            &[],
            &[],
            &[],
            &[]
        ));
        let mut failed_objtool_altstack = objtool_altstack;
        failed_objtool_altstack.outcome = rush_observe::CapturedUnmodeledSyscallOutcome::Returned {
            return_value: -libc::EINVAL as i64,
            is_error: true,
        };
        assert!(!supported_objtool_runtime_syscall(
            &failed_objtool_altstack,
            true,
            &[],
            &[],
            &[],
            &[]
        ));
        let objtool_truncate = rush_observe::CapturedUnmodeledSyscallAttempt {
            syscall_index: 8,
            ..unmodeled_attempt(
                libc::SYS_ftruncate as u64,
                [4, 0xc9a0, 0, 0, 0, 0],
                0,
                false,
            )
        };
        let objtool_mapping = rush_observe::CapturedMmapAttempt {
            syscall_index: 9,
            request: rush_observe::CapturedMmapRequest {
                address: 0,
                length: 0xc9a0,
                protection: (libc::PROT_READ | libc::PROT_WRITE) as u64,
                flags: libc::MAP_SHARED as u64,
                descriptor: 4,
                identity: Some(rush_observe::CapturedDescriptorIdentity {
                    target: b"/work/output.o".to_vec(),
                    device: 1,
                    inode: 2,
                    kind: rush_observe::CapturedDescriptorKind::RegularFile,
                }),
                mount_id: Some(3),
                offset: 0,
            },
            outcome: rush_observe::CapturedMmapOutcome::Mapped {
                address: 0x10000,
                file_value: Some(rush_observe::CapturedMappedFileValue::WholeFile {
                    bytes: 0xc9a0,
                    digest: [4; 32],
                    mode: 0o100600,
                }),
            },
        };
        assert!(supported_objtool_runtime_syscall(
            &objtool_truncate,
            true,
            std::slice::from_ref(&objtool_mapping),
            &[],
            &[],
            &[]
        ));
        assert!(!supported_objtool_runtime_syscall(
            &objtool_truncate,
            false,
            std::slice::from_ref(&objtool_mapping),
            &[],
            &[],
            &[]
        ));
        let mut delayed_mapping = objtool_mapping.clone();
        delayed_mapping.syscall_index += 1;
        assert!(supported_objtool_runtime_syscall(
            &objtool_truncate,
            true,
            std::slice::from_ref(&delayed_mapping),
            &[],
            &[],
            &[]
        ));
        let intervening_close = rush_observe::CapturedCloseAttempt {
            syscall_index: 9,
            request: rush_observe::CapturedCloseRequest {
                descriptor: 4,
                identity: objtool_mapping.request.identity.clone(),
                mount_id: Some(3),
            },
            outcome: rush_observe::CapturedCloseOutcome::Closed { file_value: None },
        };
        assert!(!supported_objtool_runtime_syscall(
            &objtool_truncate,
            true,
            std::slice::from_ref(&delayed_mapping),
            &[],
            std::slice::from_ref(&intervening_close),
            &[]
        ));
        let objtool_write = rush_observe::CapturedWriteAttempt {
            syscall_index: 10,
            request: rush_observe::CapturedWriteRequest {
                system_call: rush_observe::CapturedWriteSystemCall::Pwrite64,
                descriptor: 4,
                identity: objtool_mapping.request.identity.clone(),
                mount_id: Some(3),
                uses_descriptor_offset: false,
                offset: Some(0),
                requested_bytes: 0xc9a0,
                append: false,
            },
            outcome: rush_observe::CapturedWriteOutcome::Wrote {
                bytes: 0xc9a0,
                digest: [5; 32],
            },
        };
        assert!(supported_objtool_runtime_syscall(
            &objtool_truncate,
            true,
            &[],
            std::slice::from_ref(&objtool_write),
            &[],
            &[]
        ));
        assert!(!supported_objtool_runtime_syscall(
            &objtool_truncate,
            true,
            &[],
            std::slice::from_ref(&objtool_write),
            std::slice::from_ref(&intervening_close),
            &[]
        ));
        let sed_credentials = rush_observe::CapturedCredentialState {
            filesystem_user: 1002,
            filesystem_group: 1002,
            ..Default::default()
        };
        let sed_fchown = rush_observe::CapturedUnmodeledSyscallAttempt {
            syscall_index: 11,
            ..unmodeled_attempt(libc::SYS_fchown as u64, [4, 1002, 1002, 0, 0, 0], 0, false)
        };
        assert!(supported_sed_runtime_syscall(
            &sed_fchown,
            true,
            Some(&sed_credentials),
            std::slice::from_ref(&objtool_write),
            &[],
            &[]
        ));
        assert!(!supported_sed_runtime_syscall(
            &sed_fchown,
            false,
            Some(&sed_credentials),
            std::slice::from_ref(&objtool_write),
            &[],
            &[]
        ));
        let mut wrong_sed_owner = sed_fchown.clone();
        wrong_sed_owner.arguments[1] = 1003;
        assert!(!supported_sed_runtime_syscall(
            &wrong_sed_owner,
            true,
            Some(&sed_credentials),
            std::slice::from_ref(&objtool_write),
            &[],
            &[]
        ));
        let mut write_before_close = objtool_write;
        write_before_close.syscall_index = 8;
        assert!(!supported_sed_runtime_syscall(
            &sed_fchown,
            true,
            Some(&sed_credentials),
            std::slice::from_ref(&write_before_close),
            std::slice::from_ref(&intervening_close),
            &[]
        ));

        let shared_futex = unmodeled_attempt(
            libc::SYS_futex as u64,
            [0x5000, 9, 0, 0, 0, u32::MAX as u64],
            0,
            false,
        );
        assert!(!supported_kbuild_runtime_syscall(
            &shared_futex,
            false,
            true,
            false,
            false
        ));
    }

    fn mutation_interaction(
        root: CapturedProcessId,
        stream_ordinal: u64,
        candidate_path: PathBuf,
        interaction_path: PathBuf,
    ) -> KbuildNativeMutationInteraction {
        KbuildNativeMutationInteraction {
            root,
            stream_ordinal,
            candidate_path,
            interaction_path,
        }
    }

    #[test]
    fn recursive_analyzer_paths_split_action_components() {
        let parents = BTreeMap::from([
            (1, None),
            (2, Some(1)),
            (3, Some(2)),
            (4, Some(3)),
            (5, Some(2)),
        ]);
        let analyzers = BTreeSet::from([1, 2, 3]);

        assert_eq!(
            action_component_root(4, &analyzers, &parents).unwrap(),
            Some(4)
        );
        assert_eq!(
            action_component_root(5, &analyzers, &parents).unwrap(),
            Some(5)
        );
        assert_eq!(
            action_component_root(3, &analyzers, &parents).unwrap(),
            None
        );
    }

    #[test]
    fn mutating_analyzer_ancestor_becomes_an_action_root() {
        let parents = BTreeMap::from([(1, None), (2, Some(1)), (3, Some(2))]);
        let mut analyzers = BTreeSet::from([1, 2, 3]);
        let mutation_owners = BTreeSet::from([1, 2]);

        demote_mutating_analyzer_ancestors(1, &mut analyzers, &mutation_owners);

        assert_eq!(analyzers, BTreeSet::from([1, 3]));
        assert_eq!(
            action_component_root(1, &analyzers, &parents).unwrap(),
            None
        );
        assert_eq!(
            action_component_root(2, &analyzers, &parents).unwrap(),
            Some(2)
        );
        assert_eq!(
            action_component_root(3, &analyzers, &parents).unwrap(),
            None
        );
    }

    #[test]
    fn invocation_environment_must_have_unique_names() {
        let mut invocation = rush_observe::CapturedInvocation {
            executable: b"/bin/tool".to_vec(),
            argv: vec![b"tool".to_vec()],
            environment: vec![b"A=one".to_vec(), b"A=two".to_vec()],
            working_directory: b"/work".to_vec(),
            signal_state: Default::default(),
            file_creation_mask: 0o022,
            credentials: Default::default(),
            file_mappings: Vec::new(),
        };
        assert!(graph_invocation(&invocation)
            .unwrap_err()
            .to_string()
            .contains("repeats"));
        invocation.environment = vec![b"missing-separator".to_vec()];
        assert!(graph_invocation(&invocation)
            .unwrap_err()
            .to_string()
            .contains("separator"));
    }

    #[test]
    fn command_receipt_can_identify_an_earlier_compound_command() {
        let command = |script: &[u8]| CapturedInvocation {
            executable: PathBuf::from("/bin/sh"),
            argv: vec![b"/bin/sh".to_vec(), b"-c".to_vec(), script.to_vec()],
            environment: Vec::new(),
            working_directory: PathBuf::from("/work"),
        };
        let commands = vec![
            command(b"cc -c input.c -o output.o; fixdep > .output.o.cmd"),
            command(b"echo 'output.o: objtool' >> .output.o.cmd"),
        ];

        assert_eq!(
            command_receipt_match_indices(&commands, b"cc -c input.c -o output.o"),
            vec![0]
        );
        assert_eq!(
            command_receipt_match_index(&commands, b"cc -c input.c -o output.o"),
            Some(0)
        );
        assert_eq!(
            command_receipt_match_indices(&commands, b".output.o.cmd"),
            vec![0, 1]
        );
        assert_eq!(
            command_receipt_match_index(&commands, b".output.o.cmd"),
            Some(1)
        );

        let ambiguous = vec![
            command(b"first shared-evidence"),
            command(b"second shared-evidence"),
            command(b"final receipt append"),
        ];
        assert_eq!(
            command_receipt_match_index(&ambiguous, b"shared-evidence"),
            None
        );
    }

    #[test]
    fn overlapping_components_coalesce_in_stream_order() {
        let shared = PathBuf::from("/obj/shared");
        let components = BTreeMap::from([
            (20, BTreeSet::from([shared.clone()])),
            (30, BTreeSet::from([shared.clone()])),
        ]);
        let members = BTreeMap::from([(20, BTreeSet::from([20, 21])), (30, BTreeSet::from([30]))]);
        let timeline = KbuildNativeTimeline {
            stream_ordinal: 8,
            starts: BTreeMap::from([(20, 0), (21, 1), (30, 5)]),
            invocation_starts: BTreeMap::from([(20, 1), (30, 5)]),
            finishes: BTreeMap::from([(20, 3), (21, 4), (30, 7)]),
        };

        let interactions = KbuildNativeInteractionTimeline {
            mutations: vec![
                mutation_interaction(20, 2, shared.clone(), shared.clone()),
                mutation_interaction(30, 6, shared.clone(), shared),
            ],
            ..Default::default()
        };
        let logical =
            coalesce_native_components(&components, &members, &timeline, &interactions).unwrap();
        assert_eq!(logical.len(), 1);
        assert_eq!(logical[0].roots, [20, 30]);
        assert_eq!(logical[0].candidate_paths.len(), 1);
    }

    #[test]
    fn shared_candidate_generations_keep_separate_interaction_epochs() {
        let directory = PathBuf::from("/obj");
        let makefile = directory.join("Makefile");
        let transient = directory.join(".tmp_Makefile");
        let release = directory.join("include/config/kernel.release");
        let components = BTreeMap::from([
            (86, BTreeSet::from([makefile.clone(), transient.clone()])),
            (959, BTreeSet::from([transient.clone()])),
            (965, BTreeSet::from([release.clone()])),
        ]);
        let members = BTreeMap::from([
            (86, BTreeSet::from([86])),
            (959, BTreeSet::from([959, 980])),
            (965, BTreeSet::from([965, 982, 987])),
        ]);
        let timeline = KbuildNativeTimeline {
            stream_ordinal: 12,
            starts: BTreeMap::from([(86, 0), (959, 5), (965, 3)]),
            invocation_starts: BTreeMap::from([(86, 1), (959, 6), (965, 4)]),
            finishes: BTreeMap::from([(86, 3), (959, 7), (965, 11)]),
        };
        let interactions = KbuildNativeInteractionTimeline {
            mutations: vec![
                mutation_interaction(86, 1, transient.clone(), directory.clone()),
                mutation_interaction(86, 2, makefile.clone(), directory.clone()),
                mutation_interaction(959, 6, transient, directory.clone()),
                mutation_interaction(965, 10, release.clone(), directory),
            ],
            observations: vec![KbuildNativePathEvent {
                root: 965,
                stream_ordinal: 4,
                path: PathBuf::from("/obj"),
            }],
            ..Default::default()
        };

        let (logical, edges) = coalesce_native_components_with_union_edges_for_roots(
            &components,
            &members,
            &timeline,
            &interactions,
            &BTreeSet::from([86, 959, 965]),
            &BTreeSet::from([makefile, release]),
        )
        .unwrap();

        assert_eq!(logical.len(), 2);
        assert_eq!(logical[0].roots, [86, 959]);
        assert_eq!(logical[1].roots, [965]);
        assert_eq!(native_union_chain(&edges, 86, 959).len(), 1);
    }

    #[test]
    fn concurrent_overlapping_components_fail_closed() {
        let shared = PathBuf::from("/obj/shared");
        let components = BTreeMap::from([
            (20, BTreeSet::from([shared.clone()])),
            (30, BTreeSet::from([shared.clone()])),
        ]);
        let members = BTreeMap::from([(20, BTreeSet::from([20])), (30, BTreeSet::from([30]))]);
        let timeline = KbuildNativeTimeline {
            stream_ordinal: 7,
            starts: BTreeMap::from([(20, 0), (30, 2)]),
            invocation_starts: BTreeMap::from([(20, 1), (30, 3)]),
            finishes: BTreeMap::from([(20, 5), (30, 6)]),
        };
        let interactions = KbuildNativeInteractionTimeline {
            mutations: vec![
                mutation_interaction(20, 2, shared.clone(), shared.clone()),
                mutation_interaction(30, 3, shared.clone(), shared.clone()),
                mutation_interaction(20, 5, shared.clone(), shared),
            ],
            ..Default::default()
        };

        let error = coalesce_native_components(
            &components,
            &members,
            &timeline,
            &interactions,
        )
        .unwrap_err();
        assert!(error.to_string().contains("interaction order has a cycle"));
    }

    #[test]
    fn early_consumer_coalesces_interleaved_producer_epochs() {
        let object = PathBuf::from("/obj/input.o");
        let linked = PathBuf::from("/obj/output.o");
        let components = BTreeMap::from([
            (20, BTreeSet::from([object.clone()])),
            (30, BTreeSet::from([linked])),
            (40, BTreeSet::from([object.clone()])),
        ]);
        let members = BTreeMap::from([
            (20, BTreeSet::from([20])),
            (30, BTreeSet::from([30])),
            (40, BTreeSet::from([40])),
        ]);
        let timeline = KbuildNativeTimeline {
            stream_ordinal: 10,
            starts: BTreeMap::from([(20, 0), (30, 3), (40, 6)]),
            invocation_starts: BTreeMap::from([(20, 1), (30, 4), (40, 7)]),
            finishes: BTreeMap::from([(20, 2), (30, 5), (40, 9)]),
        };
        let interactions = KbuildNativeInteractionTimeline {
            mutations: vec![
                mutation_interaction(20, 1, object.clone(), object.clone()),
                mutation_interaction(40, 8, object.clone(), object.clone()),
            ],
            observations: vec![KbuildNativePathEvent {
                root: 30,
                stream_ordinal: 4,
                path: object,
            }],
            ..Default::default()
        };

        let logical = coalesce_native_components_for_roots(
            &components,
            &members,
            &timeline,
            &interactions,
            &BTreeSet::from([20, 30]),
        )
        .unwrap();
        assert_eq!(logical.len(), 1);
        assert_eq!(logical[0].roots, [20, 30, 40]);
        assert_eq!(logical[0].candidate_paths.len(), 2);
    }

    #[test]
    fn post_final_consumer_remains_a_separate_dependency() {
        let object = PathBuf::from("/obj/input.o");
        let linked = PathBuf::from("/obj/output.o");
        let components = BTreeMap::from([
            (20, BTreeSet::from([object.clone()])),
            (30, BTreeSet::from([linked])),
        ]);
        let members = BTreeMap::from([(20, BTreeSet::from([20])), (30, BTreeSet::from([30]))]);
        let timeline = KbuildNativeTimeline {
            stream_ordinal: 7,
            starts: BTreeMap::from([(20, 0), (30, 3)]),
            invocation_starts: BTreeMap::from([(20, 1), (30, 4)]),
            finishes: BTreeMap::from([(20, 2), (30, 6)]),
        };
        let interactions = KbuildNativeInteractionTimeline {
            mutations: vec![mutation_interaction(20, 1, object.clone(), object.clone())],
            observations: vec![KbuildNativePathEvent {
                root: 30,
                stream_ordinal: 4,
                path: object,
            }],
            ..Default::default()
        };

        let logical =
            coalesce_native_components(&components, &members, &timeline, &interactions).unwrap();
        assert_eq!(logical.len(), 2);
        assert_eq!(logical[0].roots, [20]);
        assert_eq!(logical[1].roots, [30]);
    }

    #[test]
    fn pre_mutation_consumer_remains_a_separate_dependency() {
        let object = PathBuf::from("/obj/input.o");
        let linked = PathBuf::from("/obj/output.o");
        let components = BTreeMap::from([
            (20, BTreeSet::from([linked])),
            (30, BTreeSet::from([object.clone()])),
        ]);
        let members = BTreeMap::from([(20, BTreeSet::from([20])), (30, BTreeSet::from([30]))]);
        let timeline = KbuildNativeTimeline {
            stream_ordinal: 7,
            starts: BTreeMap::from([(20, 0), (30, 3)]),
            invocation_starts: BTreeMap::from([(20, 1), (30, 4)]),
            finishes: BTreeMap::from([(20, 2), (30, 6)]),
        };
        let interactions = KbuildNativeInteractionTimeline {
            mutations: vec![mutation_interaction(30, 5, object.clone(), object.clone())],
            observations: vec![KbuildNativePathEvent {
                root: 20,
                stream_ordinal: 1,
                path: object,
            }],
            ..Default::default()
        };

        let logical =
            coalesce_native_components(&components, &members, &timeline, &interactions).unwrap();
        assert_eq!(logical.len(), 2);
        assert_eq!(logical[0].roots, [20]);
        assert_eq!(logical[1].roots, [30]);
    }

    #[test]
    fn concurrent_early_consumer_cannot_become_a_compound_action() {
        let object = PathBuf::from("/obj/input.o");
        let linked = PathBuf::from("/obj/output.o");
        let components = BTreeMap::from([
            (20, BTreeSet::from([object.clone()])),
            (30, BTreeSet::from([linked])),
            (40, BTreeSet::from([object.clone()])),
        ]);
        let members = BTreeMap::from([
            (20, BTreeSet::from([20])),
            (30, BTreeSet::from([30])),
            (40, BTreeSet::from([40])),
        ]);
        let timeline = KbuildNativeTimeline {
            stream_ordinal: 10,
            starts: BTreeMap::from([(20, 0), (30, 3), (40, 5)]),
            invocation_starts: BTreeMap::from([(20, 1), (30, 4), (40, 6)]),
            finishes: BTreeMap::from([(20, 2), (30, 7), (40, 9)]),
        };
        let interactions = KbuildNativeInteractionTimeline {
            mutations: vec![
                mutation_interaction(20, 1, object.clone(), object.clone()),
                mutation_interaction(40, 6, object.clone(), object.clone()),
            ],
            observations: vec![
                KbuildNativePathEvent {
                    root: 30,
                    stream_ordinal: 4,
                    path: object.clone(),
                },
                KbuildNativePathEvent {
                    root: 30,
                    stream_ordinal: 7,
                    path: object,
                },
            ],
            ..Default::default()
        };

        let error = coalesce_native_components(&components, &members, &timeline, &interactions)
            .unwrap_err();
        assert!(error.to_string().contains("interaction order has a cycle"));
    }

    #[test]
    fn directory_consumer_coalesces_interleaved_membership_epochs() {
        let directory = PathBuf::from("/obj");
        let member = directory.join("Makefile");
        let consumer_output = directory.join("config");
        let components = BTreeMap::from([
            (20, BTreeSet::from([member.clone()])),
            (30, BTreeSet::from([consumer_output])),
            (40, BTreeSet::from([member.clone()])),
        ]);
        let members = BTreeMap::from([
            (20, BTreeSet::from([20])),
            (30, BTreeSet::from([30])),
            (40, BTreeSet::from([40])),
        ]);
        let timeline = KbuildNativeTimeline {
            stream_ordinal: 10,
            starts: BTreeMap::from([(20, 0), (30, 3), (40, 6)]),
            invocation_starts: BTreeMap::from([(20, 1), (30, 4), (40, 7)]),
            finishes: BTreeMap::from([(20, 2), (30, 5), (40, 9)]),
        };
        let interactions = KbuildNativeInteractionTimeline {
            mutations: vec![
                mutation_interaction(20, 1, member.clone(), directory.clone()),
                mutation_interaction(40, 8, member, directory.clone()),
            ],
            observations: vec![KbuildNativePathEvent {
                root: 30,
                stream_ordinal: 4,
                path: directory,
            }],
            ..Default::default()
        };

        let logical =
            coalesce_native_components(&components, &members, &timeline, &interactions).unwrap();
        assert_eq!(logical.len(), 1);
        assert_eq!(logical[0].roots, [20, 30, 40]);
        assert_eq!(logical[0].candidate_paths.len(), 2);
    }

    #[test]
    fn directory_interaction_does_not_promote_transient_component() {
        let directory = PathBuf::from("/obj");
        let consumer_output = directory.join("config");
        let transient = directory.join("temporary");
        let components = BTreeMap::from([
            (20, BTreeSet::from([consumer_output])),
            (30, BTreeSet::from([transient.clone()])),
        ]);
        let members = BTreeMap::from([(20, BTreeSet::from([20])), (30, BTreeSet::from([30]))]);
        let timeline = KbuildNativeTimeline {
            stream_ordinal: 7,
            starts: BTreeMap::from([(20, 0), (30, 4)]),
            invocation_starts: BTreeMap::from([(20, 1), (30, 5)]),
            finishes: BTreeMap::from([(20, 3), (30, 6)]),
        };
        let interactions = KbuildNativeInteractionTimeline {
            mutations: vec![mutation_interaction(30, 5, transient, directory.clone())],
            observations: vec![KbuildNativePathEvent {
                root: 20,
                stream_ordinal: 2,
                path: directory,
            }],
            ..Default::default()
        };

        let logical = coalesce_native_components_for_roots(
            &components,
            &members,
            &timeline,
            &interactions,
            &BTreeSet::from([20]),
        )
        .unwrap();
        assert_eq!(logical.len(), 2);
        assert!(logical.iter().all(|component| component.roots.len() == 1));
    }

    #[test]
    fn retained_component_roots_require_an_existing_candidate() {
        let temp = tempfile::tempdir().unwrap();
        let retained = temp.path().join("retained");
        std::fs::write(&retained, b"value").unwrap();
        let components = BTreeMap::from([
            (20, BTreeSet::from([retained])),
            (30, BTreeSet::from([temp.path().join("absent")])),
        ]);

        assert_eq!(
            retained_native_component_roots(&components).unwrap(),
            BTreeSet::from([20])
        );
    }

    #[test]
    fn interaction_timeline_tracks_directory_reads_and_membership_effects() {
        let output_root = PathBuf::from("/obj");
        let member = output_root.join("temporary");
        let component_members = BTreeMap::from([(20, BTreeSet::from([20 as CapturedProcessId]))]);
        let mut interactions =
            KbuildNativeInteractionTimeline::new(output_root.clone(), &component_members).unwrap();
        let directory_attempt = rush_observe::CapturedGetdents64Attempt {
            syscall_index: 1,
            request: rush_observe::CapturedGetdents64Request {
                descriptor: 3,
                identity: Some(rush_observe::CapturedDescriptorIdentity {
                    target: os_bytes(output_root.as_os_str()),
                    device: 1,
                    inode: 2,
                    kind: CapturedDescriptorKind::Directory,
                }),
                mount_id: Some(3),
                position_before: Some(0),
                requested_bytes: 4096,
            },
            outcome: CapturedGetdents64Outcome::Enumerated {
                returned_bytes: 0,
                position_after: 0,
                entries: Vec::new(),
                directory_value: CapturedDirectoryValue::Complete {
                    entries: 0,
                    digest: [4; 32],
                    members: Vec::new(),
                },
            },
        };
        interactions
            .visit(ProcessCaptureStreamEvent::Getdents64Attempt {
                process: 20,
                attempt: &directory_attempt,
            })
            .unwrap();

        let removal_attempt = rush_observe::CapturedPathRemovalAttempt {
            syscall_index: 2,
            request: rush_observe::CapturedPathRemovalRequest {
                system_call: rush_observe::CapturedPathRemovalSystemCall::Unlink,
                directory_descriptor: libc::AT_FDCWD,
                path: os_bytes(member.as_os_str()),
                path_is_null: false,
                base: None,
                flags: 0,
            },
            outcome: CapturedPathRemovalOutcome::Removed,
        };
        interactions
            .visit(ProcessCaptureStreamEvent::PathRemovalAttempt {
                process: 20,
                attempt: &removal_attempt,
            })
            .unwrap();

        assert_eq!(interactions.observations.len(), 1);
        assert_eq!(interactions.observations[0].path, output_root);
        assert_eq!(interactions.mutations.len(), 2);
        assert_eq!(interactions.mutations[0].candidate_path, member);
        assert_eq!(
            interactions.mutations[0].interaction_path,
            interactions.mutations[0].candidate_path
        );
        assert_eq!(
            interactions.mutations[1].interaction_path,
            interactions.observations[0].path
        );
    }
}
    #[test]
    fn includes_certified_runtime_mutations_in_coverage_once() {
        let component_path = PathBuf::from("/obj/component");
        let runtime_path = PathBuf::from("/obj/.rush-tmp/GMfifo12345");
        let owned = BTreeMap::from([(component_path.clone(), 0)]);

        assert_eq!(
            native_mutation_path_coverage_count(
                &owned,
                &BTreeSet::from([runtime_path]),
            )
            .unwrap(),
            2
        );
        assert_eq!(
            native_mutation_path_coverage_count(
                &owned,
                &BTreeSet::from([component_path]),
            )
            .unwrap(),
            1
        );
    }
