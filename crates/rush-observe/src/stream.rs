//! Bounded, private persistence for raw process-observation events.
//!
//! A complete stream is still secret-bearing capture evidence. Loading it
//! checks framing integrity against the trusted writer summary and reconstructs
//! a semantically complete raw receipt; it does not certify file values or make
//! the receipt eligible for replay.

use crate::{
    validate_captured_access_attempt, validate_captured_child_wait_attempt,
    validate_captured_close_attempt, validate_captured_credential_attempt,
    validate_captured_cwd_change_attempt, validate_captured_descriptor_timestamp_change_attempt,
    validate_captured_directory_creation_attempt, validate_captured_dup2_attempt,
    validate_captured_exec_attempt, validate_captured_fcntl_attempt,
    validate_captured_file_advice_attempt,
    validate_captured_filesystem_status_attempt, validate_captured_fstat_attempt,
    validate_captured_futex_wake_attempt, validate_captured_getcwd_attempt,
    validate_captured_getdents64_attempt, validate_captured_getrandom_attempt,
    validate_captured_invocation, validate_captured_ioctl_attempt,
    validate_captured_lifecycle_exit_attempt, validate_captured_memory_state_attempt,
    validate_captured_mmap_attempt, validate_captured_newfstatat_attempt,
    validate_captured_open_attempt, validate_captured_path_mode_change_attempt,
    validate_captured_epoll_create_attempt, validate_captured_path_removal_attempt,
    validate_captured_path_rename_attempt, validate_captured_pipe_attempt,
    validate_captured_prlimit_attempt,
    validate_captured_process_creation_attempt, validate_captured_process_identity_attempt,
    validate_captured_read_attempt, validate_captured_readlink_attempt,
    validate_captured_runtime_observation_attempt, validate_captured_seek_attempt,
    captured_signal_delivery_creates_frame, validate_captured_signal_delivery,
    validate_captured_signal_state_attempt,
    validate_captured_socket_connect_attempt,
    validate_captured_socket_create_attempt,
    validate_captured_statx_attempt, validate_captured_symbolic_link_creation_attempt,
    validate_captured_thread_state_attempt, validate_captured_umask_attempt,
    validate_captured_unmodeled_syscall_attempt, validate_captured_write_attempt,
    validate_captured_xattr_attempt, CapturedAccessAttempt, CapturedChildWaitAttempt,
    CapturedChildWaitOutcome, CapturedCloseAttempt, CapturedCredentialAttempt,
    CapturedCwdChangeAttempt, CapturedDescriptorIdentity, CapturedDescriptorKind,
    CapturedDescriptorTimestampChangeAttempt, CapturedDirectoryCreationAttempt,
    CapturedDirectoryValue, CapturedDup2Attempt, CapturedEpollCreateAttempt, CapturedExecAttempt,
    CapturedFcntlAttempt, CapturedFileAdviceAttempt, CapturedFilesystemStatusAttempt,
    CapturedFstatAttempt,
    CapturedFutexWakeAttempt,
    CapturedGetcwdAttempt, CapturedGetdents64Attempt, CapturedGetdents64Outcome,
    CapturedGetrandomAttempt, CapturedInvocation, CapturedIoctlAttempt,
    CapturedLifecycleExitAttempt, CapturedMappedFileValue, CapturedMemoryStateAttempt,
    CapturedMmapAttempt, CapturedMmapOutcome, CapturedNewfstatatAttempt, CapturedOpenAttempt,
    CapturedPathModeChangeAttempt, CapturedPathRemovalAttempt, CapturedPathRenameAttempt,
    CapturedPipeAttempt, CapturedPrlimitAttempt, CapturedProcess, CapturedProcessCreationAttempt,
    CapturedProcessCreationOutcome, CapturedProcessId, CapturedProcessIdentityAttempt,
    CapturedProcessIdentityOutcome, CapturedProcessIdentityRequest, CapturedProcessOutcome,
    CapturedReadAttempt, CapturedReadOutcome, CapturedReadlinkAttempt,
    CapturedRuntimeObservationAttempt, CapturedSeekAttempt, CapturedSignalDelivery,
    CapturedSignalStateAttempt, CapturedSocketConnectAttempt,
    CapturedSocketCreateAttempt, CapturedStatxAttempt,
    CapturedSymbolicLinkCreationAttempt, CapturedThreadStateAttempt,
    CapturedUmaskAttempt, CapturedUnmodeledSyscallAttempt, CapturedWriteAttempt,
    CapturedXattrAttempt, ProcessCaptureReceipt,
    PROCESS_CAPTURE_RECEIPT_VERSION,
};
use anyhow::{Context, Result};
use bincode::Options;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

const HEADER_MAGIC: [u8; 16] = *b"RUSHPCAPSTREAM\0\0";
const TRAILER_MAGIC: [u8; 8] = *b"RUSHCEND";
const DIGEST_DOMAIN: &[u8] = b"rush.process-capture-stream.v1\n";
const HEADER_RESERVED_BYTES: usize = 16;
const HEADER_BYTES: u64 = 88;
const FRAME_PREFIX_BYTES: u64 = 24;
const FRAME_DIGEST_BYTES: u64 = 32;
const FOOTER_BYTES: u64 = 484;
const TRAILER_BYTES: u64 = 48;
const FORMAT_RECORD_VERSION: u32 = 1;
const MAX_SUPPORTED_FRAME_BYTES: u64 = 64 * 1024 * 1024;
const FRAME_TAG_PROCESS_STARTED: u8 = 1;
const FRAME_TAG_INVOCATION: u8 = 2;
const FRAME_TAG_EXEC_ATTEMPT: u8 = 3;
const FRAME_TAG_OPEN_ATTEMPT: u8 = 4;
const FRAME_TAG_READ_ATTEMPT: u8 = 5;
const FRAME_TAG_GETDENTS64_ATTEMPT: u8 = 6;
const FRAME_TAG_MMAP_ATTEMPT: u8 = 7;
const FRAME_TAG_WRITE_ATTEMPT: u8 = 8;
const FRAME_TAG_PROCESS_FINISHED: u8 = 9;
const FRAME_TAG_NEWFSTATAT_ATTEMPT: u8 = 10;
const FRAME_TAG_READLINK_ATTEMPT: u8 = 11;
const FRAME_TAG_ACCESS_ATTEMPT: u8 = 12;
const FRAME_TAG_GETCWD_ATTEMPT: u8 = 13;
const FRAME_TAG_CWD_CHANGE_ATTEMPT: u8 = 14;
const FRAME_TAG_PATH_REMOVAL_ATTEMPT: u8 = 15;
const FRAME_TAG_DIRECTORY_CREATION_ATTEMPT: u8 = 16;
const FRAME_TAG_FILESYSTEM_STATUS_ATTEMPT: u8 = 17;
const FRAME_TAG_PATH_RENAME_ATTEMPT: u8 = 18;
const FRAME_TAG_PATH_MODE_CHANGE_ATTEMPT: u8 = 19;
const FRAME_TAG_DESCRIPTOR_TIMESTAMP_CHANGE_ATTEMPT: u8 = 20;
const FRAME_TAG_SYMBOLIC_LINK_CREATION_ATTEMPT: u8 = 21;
const FRAME_TAG_STATX_ATTEMPT: u8 = 22;
const FRAME_TAG_XATTR_ATTEMPT: u8 = 23;
const FRAME_TAG_FSTAT_ATTEMPT: u8 = 24;
const FRAME_TAG_GETRANDOM_ATTEMPT: u8 = 25;
const FRAME_TAG_PRLIMIT_ATTEMPT: u8 = 26;
const FRAME_TAG_CLOSE_ATTEMPT: u8 = 27;
const FRAME_TAG_MEMORY_STATE_ATTEMPT: u8 = 28;
const FRAME_TAG_THREAD_STATE_ATTEMPT: u8 = 29;
const FRAME_TAG_LIFECYCLE_EXIT_ATTEMPT: u8 = 30;
const FRAME_TAG_UNMODELED_SYSCALL_ATTEMPT: u8 = 31;
const FRAME_TAG_SEEK_ATTEMPT: u8 = 32;
const FRAME_TAG_FCNTL_ATTEMPT: u8 = 33;
const FRAME_TAG_SIGNAL_STATE_ATTEMPT: u8 = 34;
const FRAME_TAG_PIPE_ATTEMPT: u8 = 35;
const FRAME_TAG_DUP2_ATTEMPT: u8 = 36;
const FRAME_TAG_SIGNAL_DELIVERY: u8 = 37;
const FRAME_TAG_UMASK_ATTEMPT: u8 = 38;
const FRAME_TAG_CREDENTIAL_ATTEMPT: u8 = 39;
const FRAME_TAG_FUTEX_WAKE_ATTEMPT: u8 = 40;
const FRAME_TAG_PROCESS_CREATION_ATTEMPT: u8 = 41;
const FRAME_TAG_CHILD_WAIT_ATTEMPT: u8 = 42;
const FRAME_TAG_PROCESS_IDENTITY_ATTEMPT: u8 = 43;
const FRAME_TAG_IOCTL_ATTEMPT: u8 = 44;
const FRAME_TAG_RUNTIME_OBSERVATION_ATTEMPT: u8 = 45;
const FRAME_TAG_FILE_ADVICE_ATTEMPT: u8 = 46;
const FRAME_TAG_EPOLL_CREATE_ATTEMPT: u8 = 47;
const FRAME_TAG_SOCKET_CREATE_ATTEMPT: u8 = 48;
const FRAME_TAG_SOCKET_CONNECT_ATTEMPT: u8 = 49;
const FRAME_TAG_FOOTER: u8 = 255;

/// Current private process-capture stream format.
pub const PROCESS_CAPTURE_STREAM_VERSION: u32 = 71;

/// Controller identity bound into one private process-capture stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessCaptureStreamIdentity {
    capture_id: [u8; 16],
    context_digest: [u8; 32],
}

impl ProcessCaptureStreamIdentity {
    /// Construct an identity from a transaction identifier and its context digest.
    pub fn new(capture_id: [u8; 16], context_digest: [u8; 32]) -> Self {
        Self {
            capture_id,
            context_digest,
        }
    }

    /// Controller-selected capture transaction identifier.
    pub fn capture_id(&self) -> [u8; 16] {
        self.capture_id
    }

    /// Digest of the controller context that admitted this capture.
    pub fn context_digest(&self) -> [u8; 32] {
        self.context_digest
    }
}

/// Hard resource bounds applied identically while writing and loading a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessCaptureStreamLimits {
    max_stream_bytes: u64,
    max_records: u64,
    max_processes: u64,
    max_frame_bytes: u64,
}

impl ProcessCaptureStreamLimits {
    /// Validate and construct one explicit capture budget.
    ///
    /// Frame budgets above the implementation's 64 MiB hard ceiling are
    /// rejected before a stream is created.
    pub fn new(
        max_stream_bytes: u64,
        max_records: u64,
        max_processes: u64,
        max_frame_bytes: u64,
    ) -> Result<Self> {
        let fixed_bytes = HEADER_BYTES
            .checked_add(FOOTER_BYTES)
            .and_then(|bytes| bytes.checked_add(TRAILER_BYTES))
            .context("process capture stream fixed-size budget overflowed")?;
        if max_stream_bytes < fixed_bytes
            || max_records == 0
            || max_processes == 0
            || max_frame_bytes == 0
            || max_frame_bytes > MAX_SUPPORTED_FRAME_BYTES
        {
            anyhow::bail!("process capture stream limits are invalid");
        }
        Ok(Self {
            max_stream_bytes,
            max_records,
            max_processes,
            max_frame_bytes,
        })
    }

    /// Maximum physical stream size, including its footer and trailer.
    pub fn max_stream_bytes(&self) -> u64 {
        self.max_stream_bytes
    }

    /// Maximum number of non-footer event records.
    pub fn max_records(&self) -> u64 {
        self.max_records
    }

    /// Maximum number of distinct process lifecycles.
    pub fn max_processes(&self) -> u64 {
        self.max_processes
    }

    /// Maximum serialized bytes in one event frame.
    pub fn max_frame_bytes(&self) -> u64 {
        self.max_frame_bytes
    }
}

/// Aggregate event and syscall census committed by a complete stream.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessCaptureStreamCensus {
    /// Number of successful exec-boundary invocation records.
    pub invocations: u64,
    /// Number of captured exec attempts.
    pub exec_attempts: u64,
    /// Number of captured pathname-open attempts.
    pub open_attempts: u64,
    /// Number of captured native metadata lookup attempts.
    pub newfstatat_attempts: u64,
    /// Number of captured native descriptor metadata attempts.
    pub fstat_attempts: u64,
    /// Number of captured native randomness attempts.
    pub getrandom_attempts: u64,
    /// Number of captured supported native runtime observations.
    pub runtime_observation_attempts: u64,
    /// Number of captured native process-resource limit attempts.
    pub prlimit_attempts: u64,
    /// Number of captured native descriptor-close attempts.
    pub close_attempts: u64,
    /// Number of captured native descriptor-positioning attempts.
    pub seek_attempts: u64,
    /// Number of captured bounded native file-access advisory attempts.
    pub file_advice_attempts: u64,
    /// Number of captured native descriptor-control attempts.
    pub fcntl_attempts: u64,
    /// Number of captured supported native descriptor ioctl attempts.
    pub ioctl_attempts: u64,
    /// Number of captured native anonymous-pipe creation attempts.
    pub pipe_attempts: u64,
    /// Number of captured native event-poll descriptor creations.
    pub epoll_create_attempts: u64,
    /// Number of captured supported native local stream-socket creations.
    pub socket_create_attempts: u64,
    /// Number of captured supported native local stream-socket connections.
    pub socket_connect_attempts: u64,
    /// Number of captured native descriptor-duplication attempts.
    pub dup2_attempts: u64,
    /// Number of captured native signal-disposition and mask attempts.
    pub signal_state_attempts: u64,
    /// Number of accepted ptrace signal-delivery stops.
    pub signal_deliveries: u64,
    /// Number of captured native file-creation mask transitions.
    pub umask_attempts: u64,
    /// Number of captured native credential queries and transitions.
    pub credential_attempts: u64,
    /// Number of captured process-private native futex wake requests.
    pub futex_wake_attempts: u64,
    /// Number of captured supported native process-creation requests.
    pub process_creation_attempts: u64,
    /// Number of captured supported native child-wait requests.
    pub child_wait_attempts: u64,
    /// Number of captured supported native process-identity queries.
    pub process_identity_attempts: u64,
    /// Number of captured native process-memory state attempts.
    pub memory_state_attempts: u64,
    /// Number of captured native thread-runtime state attempts.
    pub thread_state_attempts: u64,
    /// Number of captured native lifecycle-exit attempts.
    pub lifecycle_exit_attempts: u64,
    /// Number of captured native `statx` attempts.
    pub statx_attempts: u64,
    /// Number of captured pathname extended-attribute reads.
    pub xattr_attempts: u64,
    /// Number of captured native link-target attempts.
    pub readlink_attempts: u64,
    /// Number of captured native access-predicate attempts.
    pub access_attempts: u64,
    /// Number of captured native working-directory queries.
    pub getcwd_attempts: u64,
    /// Number of captured native working-directory mutations.
    pub cwd_change_attempts: u64,
    /// Number of captured native pathname-removal attempts.
    pub path_removal_attempts: u64,
    /// Number of captured native directory-creation attempts.
    pub directory_creation_attempts: u64,
    /// Number of captured native pathname-rename attempts.
    pub path_rename_attempts: u64,
    /// Number of captured native pathname permission-change attempts.
    pub path_mode_change_attempts: u64,
    /// Number of captured descriptor-form timestamp mutations.
    pub descriptor_timestamp_change_attempts: u64,
    /// Number of captured native symbolic-link creation attempts.
    pub symbolic_link_creation_attempts: u64,
    /// Number of captured native filesystem-status attempts.
    pub filesystem_status_attempts: u64,
    /// Number of captured descriptor-read attempts.
    pub read_attempts: u64,
    /// Number of captured directory-enumeration attempts.
    pub getdents64_attempts: u64,
    /// Number of captured native mapping attempts.
    pub mmap_attempts: u64,
    /// Number of captured scalar-write attempts.
    pub write_attempts: u64,
    /// Number of exact syscall attempts without a supported semantic model.
    pub unmodeled_syscall_attempts: u64,
    /// Number of observed syscall entries.
    pub syscall_entries: u64,
    /// Number of paired syscall exits.
    pub syscall_exits: u64,
    /// Number of syscalls completed by task termination.
    pub terminal_syscalls: u64,
}

impl ProcessCaptureStreamCensus {
    /// Exact modeled-or-unmodeled attempt records in this census.
    pub fn recorded_syscall_attempts(self) -> Result<u64> {
        [
            self.exec_attempts,
            self.open_attempts,
            self.newfstatat_attempts,
            self.fstat_attempts,
            self.getrandom_attempts,
            self.runtime_observation_attempts,
            self.prlimit_attempts,
            self.close_attempts,
            self.seek_attempts,
            self.file_advice_attempts,
            self.fcntl_attempts,
            self.ioctl_attempts,
            self.pipe_attempts,
            self.epoll_create_attempts,
            self.socket_create_attempts,
            self.socket_connect_attempts,
            self.dup2_attempts,
            self.signal_state_attempts,
            self.umask_attempts,
            self.credential_attempts,
            self.futex_wake_attempts,
            self.process_creation_attempts,
            self.child_wait_attempts,
            self.process_identity_attempts,
            self.memory_state_attempts,
            self.thread_state_attempts,
            self.lifecycle_exit_attempts,
            self.statx_attempts,
            self.xattr_attempts,
            self.readlink_attempts,
            self.access_attempts,
            self.getcwd_attempts,
            self.cwd_change_attempts,
            self.path_removal_attempts,
            self.directory_creation_attempts,
            self.path_rename_attempts,
            self.path_mode_change_attempts,
            self.descriptor_timestamp_change_attempts,
            self.symbolic_link_creation_attempts,
            self.filesystem_status_attempts,
            self.read_attempts,
            self.getdents64_attempts,
            self.mmap_attempts,
            self.write_attempts,
            self.unmodeled_syscall_attempts,
        ]
        .into_iter()
        .try_fold(0u64, |total, count| {
            total
                .checked_add(count)
                .context("process capture recorded-syscall census overflowed")
        })
    }
}

/// Writer-reported bytes and census for one footer-complete stream.
///
/// This summary is not semantic validation and cannot authorize adoption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessCaptureStreamSummary {
    identity: ProcessCaptureStreamIdentity,
    digest: [u8; 32],
    record_count: u64,
    process_count: u64,
    payload_bytes: u64,
    stream_bytes: u64,
    census: ProcessCaptureStreamCensus,
}

impl ProcessCaptureStreamSummary {
    /// Controller identity copied from the integrity-checked header and footer.
    pub fn identity(&self) -> ProcessCaptureStreamIdentity {
        self.identity
    }

    /// BLAKE3 digest of the complete committed stream prefix.
    pub fn digest(&self) -> [u8; 32] {
        self.digest
    }

    /// Number of non-footer records.
    pub fn record_count(&self) -> u64 {
        self.record_count
    }

    /// Number of complete process lifecycles.
    pub fn process_count(&self) -> u64 {
        self.process_count
    }

    /// Sum of serialized event payload sizes.
    pub fn payload_bytes(&self) -> u64 {
        self.payload_bytes
    }

    /// Exact physical byte size, including the end trailer.
    pub fn stream_bytes(&self) -> u64 {
        self.stream_bytes
    }

    /// Aggregate event and syscall census.
    pub fn census(&self) -> ProcessCaptureStreamCensus {
        self.census
    }
}

/// Integrity-checked summary of a reconstructed, semantically valid raw receipt.
///
/// This seal establishes raw stream completeness, not replay eligibility. The
/// receipt can still contain explicitly unwitnessed file mappings and private
/// host state that a downstream adapter must reject, root, or sanitize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessCaptureStreamSeal {
    summary: ProcessCaptureStreamSummary,
}

impl ProcessCaptureStreamSeal {
    /// Writer and controller identity bound by the stream digest.
    pub fn identity(&self) -> ProcessCaptureStreamIdentity {
        self.summary.identity()
    }

    /// BLAKE3 digest of the complete committed stream prefix.
    pub fn digest(&self) -> [u8; 32] {
        self.summary.digest()
    }

    /// Number of non-footer records.
    pub fn record_count(&self) -> u64 {
        self.summary.record_count()
    }

    /// Number of complete process lifecycles.
    pub fn process_count(&self) -> u64 {
        self.summary.process_count()
    }

    /// Sum of serialized event payload sizes.
    pub fn payload_bytes(&self) -> u64 {
        self.summary.payload_bytes()
    }

    /// Exact physical byte size, including the end trailer.
    pub fn stream_bytes(&self) -> u64 {
        self.summary.stream_bytes()
    }

    /// Aggregate event and syscall census.
    pub fn census(&self) -> ProcessCaptureStreamCensus {
        self.summary.census()
    }
}

/// Narrow census of file and directory values checked in a private stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessCaptureInputWitnessCensus {
    file_mappings: u64,
    witnessed_file_mappings: u64,
    regular_file_reads: u64,
    witnessed_regular_file_reads: u64,
    proc_filesystems_reads: u64,
    proc_process_maps_reads: u64,
    proc_process_statm_reads: u64,
    proc_process_cgroup_reads: u64,
    cgroup_cpu_max_reads: u64,
    proc_vm_overcommit_memory_reads: u64,
    other_unwitnessed_regular_file_reads: u64,
    first_other_unwitnessed_regular_file_read: Option<Vec<u8>>,
    directory_enumerations: u64,
    witnessed_directory_enumerations: u64,
}

impl ProcessCaptureInputWitnessCensus {
    /// File-backed exec and successful mmap values encountered in the stream.
    pub fn file_mappings(&self) -> u64 {
        self.file_mappings
    }

    /// File-backed mapping values carrying complete-file content witnesses.
    pub fn witnessed_file_mappings(&self) -> u64 {
        self.witnessed_file_mappings
    }

    /// Successful reads from descriptors identified as regular files.
    pub fn regular_file_reads(&self) -> u64 {
        self.regular_file_reads
    }

    /// Regular-file reads carrying a matched complete-file value.
    pub fn witnessed_regular_file_reads(&self) -> u64 {
        self.witnessed_regular_file_reads
    }

    /// Unwitnessed reads from `/proc/filesystems`.
    pub fn proc_filesystems_reads(&self) -> u64 {
        self.proc_filesystems_reads
    }

    /// Unwitnessed reads from numeric process `maps` files in procfs.
    pub fn proc_process_maps_reads(&self) -> u64 {
        self.proc_process_maps_reads
    }

    /// Unwitnessed reads from numeric process `statm` files in procfs.
    pub fn proc_process_statm_reads(&self) -> u64 {
        self.proc_process_statm_reads
    }

    /// Unwitnessed reads from numeric process `cgroup` files in procfs.
    pub fn proc_process_cgroup_reads(&self) -> u64 {
        self.proc_process_cgroup_reads
    }

    /// Unwitnessed reads from cgroup-v2 `cpu.max` controller files.
    pub fn cgroup_cpu_max_reads(&self) -> u64 {
        self.cgroup_cpu_max_reads
    }

    /// Unwitnessed reads from `/proc/sys/vm/overcommit_memory`.
    pub fn proc_vm_overcommit_memory_reads(&self) -> u64 {
        self.proc_vm_overcommit_memory_reads
    }

    /// Unwitnessed regular-file reads outside the recognized procfs classes.
    pub fn other_unwitnessed_regular_file_reads(&self) -> u64 {
        self.other_unwitnessed_regular_file_reads
    }

    /// Descriptor target for the first unrecognized unwitnessed regular-file read.
    pub fn first_other_unwitnessed_regular_file_read(&self) -> Option<&[u8]> {
        self.first_other_unwitnessed_regular_file_read.as_deref()
    }

    /// Every regular-file read without a complete-file value witness.
    pub fn unwitnessed_regular_file_reads(&self) -> u64 {
        self.regular_file_reads - self.witnessed_regular_file_reads
    }

    /// Successful directory enumerations encountered in the stream.
    pub fn directory_enumerations(&self) -> u64 {
        self.directory_enumerations
    }

    /// Successful enumerations carrying complete directory values.
    pub fn witnessed_directory_enumerations(&self) -> u64 {
        self.witnessed_directory_enumerations
    }

    fn require_complete(&self) -> Result<()> {
        if self.file_mappings != self.witnessed_file_mappings {
            anyhow::bail!(
                "process capture has {} unwitnessed file-backed mappings",
                self.file_mappings - self.witnessed_file_mappings
            );
        }
        if self.regular_file_reads != self.witnessed_regular_file_reads {
            anyhow::bail!(
                "process capture has {} unwitnessed regular-file reads: proc_filesystems={}, \
                 proc_process_maps={}, proc_process_statm={}, proc_process_cgroup={}, \
                 cgroup_cpu_max={}, \
                 proc_vm_overcommit_memory={}, other={}",
                self.unwitnessed_regular_file_reads(),
                self.proc_filesystems_reads,
                self.proc_process_maps_reads,
                self.proc_process_statm_reads,
                self.proc_process_cgroup_reads,
                self.cgroup_cpu_max_reads,
                self.proc_vm_overcommit_memory_reads,
                self.other_unwitnessed_regular_file_reads,
            );
        }
        if self.directory_enumerations != self.witnessed_directory_enumerations {
            anyhow::bail!(
                "process capture has {} unwitnessed directory enumerations",
                self.directory_enumerations - self.witnessed_directory_enumerations
            );
        }
        Ok(())
    }
}

/// Integrity and semantic seal for mapped-file, regular-read, and directory values.
///
/// This is not whole-capture replay certification. A downstream adapter must
/// still classify every other observable input, effect, and volatility source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessCaptureInputWitnessSeal {
    stream: ProcessCaptureStreamSeal,
    census: ProcessCaptureInputWitnessCensus,
}

impl ProcessCaptureInputWitnessSeal {
    /// Integrity-checked seal of the exact private stream.
    pub fn stream(&self) -> ProcessCaptureStreamSeal {
        self.stream
    }

    /// File and directory witness denominators checked before sealing.
    pub fn census(&self) -> ProcessCaptureInputWitnessCensus {
        self.census.clone()
    }
}

#[derive(Debug)]
pub(crate) enum ProcessCaptureEvent {
    ProcessStarted {
        process: CapturedProcessId,
        parent: Option<CapturedProcessId>,
    },
    Invocation {
        process: CapturedProcessId,
        invocation: CapturedInvocation,
    },
    ExecAttempt {
        process: CapturedProcessId,
        attempt: CapturedExecAttempt,
    },
    OpenAttempt {
        process: CapturedProcessId,
        attempt: CapturedOpenAttempt,
    },
    NewfstatatAttempt {
        process: CapturedProcessId,
        attempt: CapturedNewfstatatAttempt,
    },
    FstatAttempt {
        process: CapturedProcessId,
        attempt: CapturedFstatAttempt,
    },
    GetrandomAttempt {
        process: CapturedProcessId,
        attempt: CapturedGetrandomAttempt,
    },
    RuntimeObservationAttempt {
        process: CapturedProcessId,
        attempt: CapturedRuntimeObservationAttempt,
    },
    PrlimitAttempt {
        process: CapturedProcessId,
        attempt: CapturedPrlimitAttempt,
    },
    CloseAttempt {
        process: CapturedProcessId,
        attempt: CapturedCloseAttempt,
    },
    SeekAttempt {
        process: CapturedProcessId,
        attempt: CapturedSeekAttempt,
    },
    FileAdviceAttempt {
        process: CapturedProcessId,
        attempt: CapturedFileAdviceAttempt,
    },
    FcntlAttempt {
        process: CapturedProcessId,
        attempt: CapturedFcntlAttempt,
    },
    IoctlAttempt {
        process: CapturedProcessId,
        attempt: CapturedIoctlAttempt,
    },
    PipeAttempt {
        process: CapturedProcessId,
        attempt: CapturedPipeAttempt,
    },
    EpollCreateAttempt {
        process: CapturedProcessId,
        attempt: CapturedEpollCreateAttempt,
    },
    SocketCreateAttempt {
        process: CapturedProcessId,
        attempt: CapturedSocketCreateAttempt,
    },
    SocketConnectAttempt {
        process: CapturedProcessId,
        attempt: CapturedSocketConnectAttempt,
    },
    Dup2Attempt {
        process: CapturedProcessId,
        attempt: CapturedDup2Attempt,
    },
    SignalStateAttempt {
        process: CapturedProcessId,
        attempt: CapturedSignalStateAttempt,
    },
    SignalDelivery {
        process: CapturedProcessId,
        delivery: CapturedSignalDelivery,
    },
    UmaskAttempt {
        process: CapturedProcessId,
        attempt: CapturedUmaskAttempt,
    },
    CredentialAttempt {
        process: CapturedProcessId,
        attempt: CapturedCredentialAttempt,
    },
    FutexWakeAttempt {
        process: CapturedProcessId,
        attempt: CapturedFutexWakeAttempt,
    },
    ProcessCreationAttempt {
        process: CapturedProcessId,
        attempt: CapturedProcessCreationAttempt,
    },
    ChildWaitAttempt {
        process: CapturedProcessId,
        attempt: CapturedChildWaitAttempt,
    },
    ProcessIdentityAttempt {
        process: CapturedProcessId,
        attempt: CapturedProcessIdentityAttempt,
    },
    MemoryStateAttempt {
        process: CapturedProcessId,
        attempt: CapturedMemoryStateAttempt,
    },
    ThreadStateAttempt {
        process: CapturedProcessId,
        attempt: CapturedThreadStateAttempt,
    },
    LifecycleExitAttempt {
        process: CapturedProcessId,
        attempt: CapturedLifecycleExitAttempt,
    },
    StatxAttempt {
        process: CapturedProcessId,
        attempt: CapturedStatxAttempt,
    },
    XattrAttempt {
        process: CapturedProcessId,
        attempt: CapturedXattrAttempt,
    },
    ReadlinkAttempt {
        process: CapturedProcessId,
        attempt: CapturedReadlinkAttempt,
    },
    AccessAttempt {
        process: CapturedProcessId,
        attempt: CapturedAccessAttempt,
    },
    GetcwdAttempt {
        process: CapturedProcessId,
        attempt: CapturedGetcwdAttempt,
    },
    CwdChangeAttempt {
        process: CapturedProcessId,
        attempt: CapturedCwdChangeAttempt,
    },
    PathRemovalAttempt {
        process: CapturedProcessId,
        attempt: CapturedPathRemovalAttempt,
    },
    DirectoryCreationAttempt {
        process: CapturedProcessId,
        attempt: CapturedDirectoryCreationAttempt,
    },
    PathRenameAttempt {
        process: CapturedProcessId,
        attempt: CapturedPathRenameAttempt,
    },
    PathModeChangeAttempt {
        process: CapturedProcessId,
        attempt: CapturedPathModeChangeAttempt,
    },
    DescriptorTimestampChangeAttempt {
        process: CapturedProcessId,
        attempt: CapturedDescriptorTimestampChangeAttempt,
    },
    SymbolicLinkCreationAttempt {
        process: CapturedProcessId,
        attempt: CapturedSymbolicLinkCreationAttempt,
    },
    FilesystemStatusAttempt {
        process: CapturedProcessId,
        attempt: CapturedFilesystemStatusAttempt,
    },
    ReadAttempt {
        process: CapturedProcessId,
        attempt: CapturedReadAttempt,
    },
    Getdents64Attempt {
        process: CapturedProcessId,
        attempt: CapturedGetdents64Attempt,
    },
    MmapAttempt {
        process: CapturedProcessId,
        attempt: CapturedMmapAttempt,
    },
    WriteAttempt {
        process: CapturedProcessId,
        attempt: CapturedWriteAttempt,
    },
    UnmodeledSyscallAttempt {
        process: CapturedProcessId,
        attempt: CapturedUnmodeledSyscallAttempt,
    },
    ProcessFinished {
        process: CapturedProcessId,
        summary: ProcessFinish,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProcessFinish {
    pub(crate) invocations: u64,
    pub(crate) exec_syscalls: u64,
    pub(crate) open_syscalls: u64,
    pub(crate) newfstatat_syscalls: u64,
    pub(crate) fstat_syscalls: u64,
    pub(crate) getrandom_syscalls: u64,
    pub(crate) runtime_observation_syscalls: u64,
    pub(crate) prlimit_syscalls: u64,
    pub(crate) close_syscalls: u64,
    pub(crate) seek_syscalls: u64,
    pub(crate) file_advice_syscalls: u64,
    pub(crate) fcntl_syscalls: u64,
    pub(crate) ioctl_syscalls: u64,
    pub(crate) pipe_syscalls: u64,
    pub(crate) epoll_create_syscalls: u64,
    pub(crate) socket_create_syscalls: u64,
    pub(crate) socket_connect_syscalls: u64,
    pub(crate) dup2_syscalls: u64,
    pub(crate) signal_state_syscalls: u64,
    pub(crate) signal_deliveries: u64,
    pub(crate) umask_syscalls: u64,
    pub(crate) credential_syscalls: u64,
    pub(crate) futex_wake_syscalls: u64,
    pub(crate) process_creation_syscalls: u64,
    pub(crate) child_wait_syscalls: u64,
    pub(crate) process_identity_syscalls: u64,
    pub(crate) memory_state_syscalls: u64,
    pub(crate) thread_state_syscalls: u64,
    pub(crate) lifecycle_exit_syscalls: u64,
    pub(crate) statx_syscalls: u64,
    pub(crate) xattr_syscalls: u64,
    pub(crate) readlink_syscalls: u64,
    pub(crate) access_syscalls: u64,
    pub(crate) getcwd_syscalls: u64,
    pub(crate) cwd_change_syscalls: u64,
    pub(crate) path_removal_syscalls: u64,
    pub(crate) directory_creation_syscalls: u64,
    pub(crate) path_rename_syscalls: u64,
    pub(crate) path_mode_change_syscalls: u64,
    pub(crate) descriptor_timestamp_change_syscalls: u64,
    pub(crate) symbolic_link_creation_syscalls: u64,
    pub(crate) filesystem_status_syscalls: u64,
    pub(crate) read_syscalls: u64,
    pub(crate) getdents64_syscalls: u64,
    pub(crate) mmap_syscalls: u64,
    pub(crate) write_syscalls: u64,
    pub(crate) unmodeled_syscalls: u64,
    pub(crate) syscall_entries: u64,
    pub(crate) syscall_exits: u64,
    pub(crate) terminal_syscalls: u64,
    pub(crate) outcome: CapturedProcessOutcome,
}

/// One-event-at-a-time consumer for a semantically checked capture stream.
///
/// Event arguments are valid only provisionally while a visit is in progress.
/// A caller must not authorize replay or perform irreversible work until
/// [`visit_process_capture_stream`] returns the visitor together with its seal.
/// Implementations control any memory they retain; the stream reader itself
/// retains only bounded frame data and compact state per process.
pub trait ProcessCaptureStreamVisitor {
    /// Observe one semantically checked, borrowed event.
    fn visit(&mut self, event: ProcessCaptureStreamEvent<'_>) -> Result<()>;
}

/// One borrowed event delivered by [`visit_process_capture_stream`].
///
/// The enum is non-exhaustive because later receipt versions can add modeled
/// syscall kinds. Event data remains private raw evidence and is valid for
/// adoption only after the visit returns a seal.
#[non_exhaustive]
#[derive(Debug)]
pub enum ProcessCaptureStreamEvent<'a> {
    /// A receipt-local process joined the captured tree.
    ProcessStarted {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Receipt-local parent, absent only for the capture root.
        parent: Option<CapturedProcessId>,
    },
    /// A successful exec installed a new invocation boundary.
    Invocation {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed invocation evidence.
        invocation: &'a CapturedInvocation,
    },
    /// An exec request completed.
    ExecAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed exec evidence.
        attempt: &'a CapturedExecAttempt,
    },
    /// A pathname-open request completed or became terminal.
    OpenAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed open evidence.
        attempt: &'a CapturedOpenAttempt,
    },
    /// A native metadata lookup completed or became terminal.
    NewfstatatAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed metadata lookup evidence.
        attempt: &'a CapturedNewfstatatAttempt,
    },
    /// A native descriptor metadata request completed or became terminal.
    FstatAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed descriptor metadata evidence.
        attempt: &'a CapturedFstatAttempt,
    },
    /// A native randomness request completed or became terminal.
    GetrandomAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed randomness evidence.
        attempt: &'a CapturedGetrandomAttempt,
    },
    /// A supported volatile runtime observation completed or became terminal.
    RuntimeObservationAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed runtime-observation evidence.
        attempt: &'a CapturedRuntimeObservationAttempt,
    },
    /// A native process-resource limit request completed or became terminal.
    PrlimitAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed process-resource limit evidence.
        attempt: &'a CapturedPrlimitAttempt,
    },
    /// A native descriptor-close request completed or became terminal.
    CloseAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed descriptor-close evidence.
        attempt: &'a CapturedCloseAttempt,
    },
    /// A native descriptor-positioning request completed or became terminal.
    SeekAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed descriptor-positioning evidence.
        attempt: &'a CapturedSeekAttempt,
    },
    /// A bounded native file-access advisory completed or became terminal.
    FileAdviceAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed file-access advisory evidence.
        attempt: &'a CapturedFileAdviceAttempt,
    },
    /// A native descriptor-control request completed or became terminal.
    FcntlAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed descriptor-control evidence.
        attempt: &'a CapturedFcntlAttempt,
    },
    /// A supported native descriptor ioctl completed or became terminal.
    IoctlAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed descriptor ioctl evidence.
        attempt: &'a CapturedIoctlAttempt,
    },
    /// A native anonymous-pipe creation completed or became terminal.
    PipeAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed pipe-creation evidence.
        attempt: &'a CapturedPipeAttempt,
    },
    /// A native event-poll descriptor creation completed or became terminal.
    EpollCreateAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed event-poll descriptor creation evidence.
        attempt: &'a CapturedEpollCreateAttempt,
    },
    /// A supported local stream-socket creation completed or became terminal.
    SocketCreateAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed socket-creation evidence.
        attempt: &'a CapturedSocketCreateAttempt,
    },
    /// A supported local stream-socket connection completed or became terminal.
    SocketConnectAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed socket-connection evidence.
        attempt: &'a CapturedSocketConnectAttempt,
    },
    /// A native descriptor duplication completed or became terminal.
    Dup2Attempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed descriptor-duplication evidence.
        attempt: &'a CapturedDup2Attempt,
    },
    /// A native signal-disposition or mask request completed or became terminal.
    SignalStateAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed signal-state evidence.
        attempt: &'a CapturedSignalStateAttempt,
    },
    /// A signal was accepted for delivery to a traced process.
    SignalDelivery {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed signal provenance and interruption position.
        delivery: &'a CapturedSignalDelivery,
    },
    /// A native file-creation mask transition completed or became terminal.
    UmaskAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed file-creation mask evidence.
        attempt: &'a CapturedUmaskAttempt,
    },
    /// A native credential query or transition completed or became terminal.
    CredentialAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed credential evidence.
        attempt: &'a CapturedCredentialAttempt,
    },
    /// A process-private native futex wake completed or became terminal.
    FutexWakeAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed process-private futex wake evidence.
        attempt: &'a CapturedFutexWakeAttempt,
    },
    /// A supported native process-creation request completed or became terminal.
    ProcessCreationAttempt {
        /// Receipt-local parent process identity.
        process: CapturedProcessId,
        /// Borrowed process-creation evidence.
        attempt: &'a CapturedProcessCreationAttempt,
    },
    /// A supported native child-wait request completed or became terminal.
    ChildWaitAttempt {
        /// Receipt-local waiting process identity.
        process: CapturedProcessId,
        /// Borrowed child-wait evidence.
        attempt: &'a CapturedChildWaitAttempt,
    },
    /// A supported native process-identity query completed or became terminal.
    ProcessIdentityAttempt {
        /// Receipt-local querying process identity.
        process: CapturedProcessId,
        /// Borrowed process-identity evidence.
        attempt: &'a CapturedProcessIdentityAttempt,
    },
    /// A native process-memory state request completed or became terminal.
    MemoryStateAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed process-memory state evidence.
        attempt: &'a CapturedMemoryStateAttempt,
    },
    /// A native thread-runtime state request completed or became terminal.
    ThreadStateAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed thread-runtime state evidence.
        attempt: &'a CapturedThreadStateAttempt,
    },
    /// A native lifecycle-exit request reached its process terminal state.
    LifecycleExitAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed lifecycle-exit evidence.
        attempt: &'a CapturedLifecycleExitAttempt,
    },
    /// A native `statx` request completed or became terminal.
    StatxAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed extended metadata evidence.
        attempt: &'a CapturedStatxAttempt,
    },
    /// A pathname extended-attribute read completed or became terminal.
    XattrAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed extended-attribute evidence.
        attempt: &'a CapturedXattrAttempt,
    },
    /// A native link-target request completed or became terminal.
    ReadlinkAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed link-target evidence.
        attempt: &'a CapturedReadlinkAttempt,
    },
    /// A native pathname-access predicate completed or became terminal.
    AccessAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed access-predicate evidence.
        attempt: &'a CapturedAccessAttempt,
    },
    /// A native working-directory query completed or became terminal.
    GetcwdAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed working-directory evidence.
        attempt: &'a CapturedGetcwdAttempt,
    },
    /// A native working-directory mutation completed or became terminal.
    CwdChangeAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed working-directory mutation evidence.
        attempt: &'a CapturedCwdChangeAttempt,
    },
    /// A native pathname-removal request completed or became terminal.
    PathRemovalAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed pathname-removal evidence.
        attempt: &'a CapturedPathRemovalAttempt,
    },
    /// A native directory-creation request completed or became terminal.
    DirectoryCreationAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed directory-creation evidence.
        attempt: &'a CapturedDirectoryCreationAttempt,
    },
    /// A native pathname-rename request completed or became terminal.
    PathRenameAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed pathname-rename evidence.
        attempt: &'a CapturedPathRenameAttempt,
    },
    /// A pathname permission change completed or became terminal.
    PathModeChangeAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed pathname permission-change evidence.
        attempt: &'a CapturedPathModeChangeAttempt,
    },
    /// A descriptor-form timestamp mutation completed or became terminal.
    DescriptorTimestampChangeAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed descriptor timestamp-mutation evidence.
        attempt: &'a CapturedDescriptorTimestampChangeAttempt,
    },
    /// A native symbolic-link creation completed or became terminal.
    SymbolicLinkCreationAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed symbolic-link creation evidence.
        attempt: &'a CapturedSymbolicLinkCreationAttempt,
    },
    /// A native filesystem-status request completed or became terminal.
    FilesystemStatusAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed filesystem-status evidence.
        attempt: &'a CapturedFilesystemStatusAttempt,
    },
    /// A descriptor-read request completed or became terminal.
    ReadAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed read evidence.
        attempt: &'a CapturedReadAttempt,
    },
    /// A directory-enumeration request completed or became terminal.
    Getdents64Attempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed directory-enumeration evidence.
        attempt: &'a CapturedGetdents64Attempt,
    },
    /// A native mapping request completed or became terminal.
    MmapAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed mapping evidence.
        attempt: &'a CapturedMmapAttempt,
    },
    /// A scalar-write request completed or became terminal.
    WriteAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed write evidence.
        attempt: &'a CapturedWriteAttempt,
    },
    /// A syscall without a supported semantic model returned or became terminal.
    UnmodeledSyscallAttempt {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Borrowed exact private syscall evidence.
        attempt: &'a CapturedUnmodeledSyscallAttempt,
    },
    /// A complete process lifecycle ended.
    ProcessFinished {
        /// Receipt-local process identity.
        process: CapturedProcessId,
        /// Captured terminal outcome.
        outcome: CapturedProcessOutcome,
    },
}

pub(crate) trait ProcessCaptureSink {
    fn record(&mut self, event: ProcessCaptureEvent) -> Result<()>;

    fn max_event_bytes(&self) -> Option<u64> {
        None
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessStartedPayload {
    process: CapturedProcessId,
    parent: Option<CapturedProcessId>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessValue<T> {
    process: CapturedProcessId,
    value: T,
}

#[derive(Debug)]
struct ReceiptProcessBuilder {
    parent: Option<CapturedProcessId>,
    invocations: Vec<CapturedInvocation>,
    exec_attempts: Vec<CapturedExecAttempt>,
    open_attempts: Vec<CapturedOpenAttempt>,
    newfstatat_attempts: Vec<CapturedNewfstatatAttempt>,
    fstat_attempts: Vec<CapturedFstatAttempt>,
    getrandom_attempts: Vec<CapturedGetrandomAttempt>,
    runtime_observation_attempts: Vec<CapturedRuntimeObservationAttempt>,
    prlimit_attempts: Vec<CapturedPrlimitAttempt>,
    close_attempts: Vec<CapturedCloseAttempt>,
    seek_attempts: Vec<CapturedSeekAttempt>,
    file_advice_attempts: Vec<CapturedFileAdviceAttempt>,
    fcntl_attempts: Vec<CapturedFcntlAttempt>,
    ioctl_attempts: Vec<CapturedIoctlAttempt>,
    pipe_attempts: Vec<CapturedPipeAttempt>,
    epoll_create_attempts: Vec<CapturedEpollCreateAttempt>,
    socket_create_attempts: Vec<CapturedSocketCreateAttempt>,
    socket_connect_attempts: Vec<CapturedSocketConnectAttempt>,
    dup2_attempts: Vec<CapturedDup2Attempt>,
    signal_state_attempts: Vec<CapturedSignalStateAttempt>,
    signal_deliveries: Vec<CapturedSignalDelivery>,
    umask_attempts: Vec<CapturedUmaskAttempt>,
    credential_attempts: Vec<CapturedCredentialAttempt>,
    futex_wake_attempts: Vec<CapturedFutexWakeAttempt>,
    process_creation_attempts: Vec<CapturedProcessCreationAttempt>,
    child_wait_attempts: Vec<CapturedChildWaitAttempt>,
    process_identity_attempts: Vec<CapturedProcessIdentityAttempt>,
    memory_state_attempts: Vec<CapturedMemoryStateAttempt>,
    thread_state_attempts: Vec<CapturedThreadStateAttempt>,
    lifecycle_exit_attempts: Vec<CapturedLifecycleExitAttempt>,
    statx_attempts: Vec<CapturedStatxAttempt>,
    xattr_attempts: Vec<CapturedXattrAttempt>,
    readlink_attempts: Vec<CapturedReadlinkAttempt>,
    access_attempts: Vec<CapturedAccessAttempt>,
    getcwd_attempts: Vec<CapturedGetcwdAttempt>,
    cwd_change_attempts: Vec<CapturedCwdChangeAttempt>,
    path_removal_attempts: Vec<CapturedPathRemovalAttempt>,
    directory_creation_attempts: Vec<CapturedDirectoryCreationAttempt>,
    path_rename_attempts: Vec<CapturedPathRenameAttempt>,
    path_mode_change_attempts: Vec<CapturedPathModeChangeAttempt>,
    descriptor_timestamp_change_attempts: Vec<CapturedDescriptorTimestampChangeAttempt>,
    symbolic_link_creation_attempts: Vec<CapturedSymbolicLinkCreationAttempt>,
    filesystem_status_attempts: Vec<CapturedFilesystemStatusAttempt>,
    read_attempts: Vec<CapturedReadAttempt>,
    getdents64_attempts: Vec<CapturedGetdents64Attempt>,
    mmap_attempts: Vec<CapturedMmapAttempt>,
    write_attempts: Vec<CapturedWriteAttempt>,
    unmodeled_syscall_attempts: Vec<CapturedUnmodeledSyscallAttempt>,
    finish: Option<ProcessFinish>,
}

#[derive(Debug, Default)]
pub(crate) struct ReceiptBuilder {
    processes: BTreeMap<CapturedProcessId, ReceiptProcessBuilder>,
}

impl ReceiptBuilder {
    pub(crate) fn finish(self, root: CapturedProcessId) -> Result<ProcessCaptureReceipt> {
        let mut processes = Vec::new();
        processes
            .try_reserve_exact(self.processes.len())
            .context("reserve reconstructed process receipt")?;
        for (id, process) in self.processes {
            let finish = process
                .finish
                .context("process capture stream has an unfinished process")?;
            processes.push(CapturedProcess {
                id,
                parent: process.parent,
                invocations: process.invocations,
                exec_attempts: process.exec_attempts,
                exec_syscalls: finish.exec_syscalls,
                open_attempts: process.open_attempts,
                open_syscalls: finish.open_syscalls,
                newfstatat_attempts: process.newfstatat_attempts,
                newfstatat_syscalls: finish.newfstatat_syscalls,
                fstat_attempts: process.fstat_attempts,
                fstat_syscalls: finish.fstat_syscalls,
                getrandom_attempts: process.getrandom_attempts,
                getrandom_syscalls: finish.getrandom_syscalls,
                runtime_observation_attempts: process.runtime_observation_attempts,
                runtime_observation_syscalls: finish.runtime_observation_syscalls,
                prlimit_attempts: process.prlimit_attempts,
                prlimit_syscalls: finish.prlimit_syscalls,
                close_attempts: process.close_attempts,
                close_syscalls: finish.close_syscalls,
                seek_attempts: process.seek_attempts,
                seek_syscalls: finish.seek_syscalls,
                file_advice_attempts: process.file_advice_attempts,
                file_advice_syscalls: finish.file_advice_syscalls,
                fcntl_attempts: process.fcntl_attempts,
                fcntl_syscalls: finish.fcntl_syscalls,
                ioctl_attempts: process.ioctl_attempts,
                ioctl_syscalls: finish.ioctl_syscalls,
                pipe_attempts: process.pipe_attempts,
                pipe_syscalls: finish.pipe_syscalls,
                epoll_create_attempts: process.epoll_create_attempts,
                epoll_create_syscalls: finish.epoll_create_syscalls,
                socket_create_attempts: process.socket_create_attempts,
                socket_create_syscalls: finish.socket_create_syscalls,
                socket_connect_attempts: process.socket_connect_attempts,
                socket_connect_syscalls: finish.socket_connect_syscalls,
                dup2_attempts: process.dup2_attempts,
                dup2_syscalls: finish.dup2_syscalls,
                signal_state_attempts: process.signal_state_attempts,
                signal_state_syscalls: finish.signal_state_syscalls,
                signal_deliveries: process.signal_deliveries,
                umask_attempts: process.umask_attempts,
                umask_syscalls: finish.umask_syscalls,
                credential_attempts: process.credential_attempts,
                credential_syscalls: finish.credential_syscalls,
                futex_wake_attempts: process.futex_wake_attempts,
                futex_wake_syscalls: finish.futex_wake_syscalls,
                process_creation_attempts: process.process_creation_attempts,
                process_creation_syscalls: finish.process_creation_syscalls,
                child_wait_attempts: process.child_wait_attempts,
                child_wait_syscalls: finish.child_wait_syscalls,
                process_identity_attempts: process.process_identity_attempts,
                process_identity_syscalls: finish.process_identity_syscalls,
                memory_state_attempts: process.memory_state_attempts,
                memory_state_syscalls: finish.memory_state_syscalls,
                thread_state_attempts: process.thread_state_attempts,
                thread_state_syscalls: finish.thread_state_syscalls,
                lifecycle_exit_attempts: process.lifecycle_exit_attempts,
                lifecycle_exit_syscalls: finish.lifecycle_exit_syscalls,
                statx_attempts: process.statx_attempts,
                statx_syscalls: finish.statx_syscalls,
                xattr_attempts: process.xattr_attempts,
                xattr_syscalls: finish.xattr_syscalls,
                readlink_attempts: process.readlink_attempts,
                readlink_syscalls: finish.readlink_syscalls,
                access_attempts: process.access_attempts,
                access_syscalls: finish.access_syscalls,
                getcwd_attempts: process.getcwd_attempts,
                getcwd_syscalls: finish.getcwd_syscalls,
                cwd_change_attempts: process.cwd_change_attempts,
                cwd_change_syscalls: finish.cwd_change_syscalls,
                path_removal_attempts: process.path_removal_attempts,
                path_removal_syscalls: finish.path_removal_syscalls,
                directory_creation_attempts: process.directory_creation_attempts,
                directory_creation_syscalls: finish.directory_creation_syscalls,
                path_rename_attempts: process.path_rename_attempts,
                path_rename_syscalls: finish.path_rename_syscalls,
                path_mode_change_attempts: process.path_mode_change_attempts,
                path_mode_change_syscalls: finish.path_mode_change_syscalls,
                descriptor_timestamp_change_attempts: process.descriptor_timestamp_change_attempts,
                descriptor_timestamp_change_syscalls: finish.descriptor_timestamp_change_syscalls,
                symbolic_link_creation_attempts: process.symbolic_link_creation_attempts,
                symbolic_link_creation_syscalls: finish.symbolic_link_creation_syscalls,
                filesystem_status_attempts: process.filesystem_status_attempts,
                filesystem_status_syscalls: finish.filesystem_status_syscalls,
                read_attempts: process.read_attempts,
                read_syscalls: finish.read_syscalls,
                getdents64_attempts: process.getdents64_attempts,
                getdents64_syscalls: finish.getdents64_syscalls,
                mmap_attempts: process.mmap_attempts,
                mmap_syscalls: finish.mmap_syscalls,
                write_attempts: process.write_attempts,
                write_syscalls: finish.write_syscalls,
                unmodeled_syscall_attempts: process.unmodeled_syscall_attempts,
                unmodeled_syscalls: finish.unmodeled_syscalls,
                syscall_entries: finish.syscall_entries,
                syscall_exits: finish.syscall_exits,
                terminal_syscalls: finish.terminal_syscalls,
                outcome: finish.outcome,
            });
        }
        ProcessCaptureReceipt::new(root, processes)
    }

    fn active_process(&mut self, process: CapturedProcessId) -> Result<&mut ReceiptProcessBuilder> {
        let process = self
            .processes
            .get_mut(&process)
            .context("process capture event precedes its process start")?;
        if process.finish.is_some() {
            anyhow::bail!("process capture event follows its process finish");
        }
        Ok(process)
    }
}

impl ProcessCaptureSink for ReceiptBuilder {
    fn record(&mut self, event: ProcessCaptureEvent) -> Result<()> {
        match event {
            ProcessCaptureEvent::ProcessStarted { process, parent } => {
                if process == 0
                    || parent == Some(process)
                    || parent.is_some_and(|parent| {
                        self.processes
                            .get(&parent)
                            .is_none_or(|parent| parent.finish.is_some())
                    })
                    || self
                        .processes
                        .insert(
                            process,
                            ReceiptProcessBuilder {
                                parent,
                                invocations: Vec::new(),
                                exec_attempts: Vec::new(),
                                open_attempts: Vec::new(),
                                newfstatat_attempts: Vec::new(),
                                fstat_attempts: Vec::new(),
                                getrandom_attempts: Vec::new(),
                                runtime_observation_attempts: Vec::new(),
                                prlimit_attempts: Vec::new(),
                                close_attempts: Vec::new(),
                                seek_attempts: Vec::new(),
                                file_advice_attempts: Vec::new(),
                                fcntl_attempts: Vec::new(),
                                ioctl_attempts: Vec::new(),
                                pipe_attempts: Vec::new(),
                                epoll_create_attempts: Vec::new(),
                                socket_create_attempts: Vec::new(),
                                socket_connect_attempts: Vec::new(),
                                dup2_attempts: Vec::new(),
                                signal_state_attempts: Vec::new(),
                                signal_deliveries: Vec::new(),
                                umask_attempts: Vec::new(),
                                credential_attempts: Vec::new(),
                                futex_wake_attempts: Vec::new(),
                                process_creation_attempts: Vec::new(),
                                child_wait_attempts: Vec::new(),
                                process_identity_attempts: Vec::new(),
                                memory_state_attempts: Vec::new(),
                                thread_state_attempts: Vec::new(),
                                lifecycle_exit_attempts: Vec::new(),
                                statx_attempts: Vec::new(),
                                xattr_attempts: Vec::new(),
                                readlink_attempts: Vec::new(),
                                access_attempts: Vec::new(),
                                getcwd_attempts: Vec::new(),
                                cwd_change_attempts: Vec::new(),
                                path_removal_attempts: Vec::new(),
                                directory_creation_attempts: Vec::new(),
                                path_rename_attempts: Vec::new(),
                                path_mode_change_attempts: Vec::new(),
                                descriptor_timestamp_change_attempts: Vec::new(),
                                symbolic_link_creation_attempts: Vec::new(),
                                filesystem_status_attempts: Vec::new(),
                                read_attempts: Vec::new(),
                                getdents64_attempts: Vec::new(),
                                mmap_attempts: Vec::new(),
                                write_attempts: Vec::new(),
                                unmodeled_syscall_attempts: Vec::new(),
                                finish: None,
                            },
                        )
                        .is_some()
                {
                    anyhow::bail!("process capture stream has an invalid process start");
                }
            }
            ProcessCaptureEvent::Invocation {
                process,
                invocation,
            } => push_receipt_event(
                &mut self.active_process(process)?.invocations,
                invocation,
                "invocation",
            )?,
            ProcessCaptureEvent::ExecAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.exec_attempts,
                    attempt,
                    "exec attempt",
                )?;
            }
            ProcessCaptureEvent::OpenAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.open_attempts,
                    attempt,
                    "open attempt",
                )?;
            }
            ProcessCaptureEvent::NewfstatatAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.newfstatat_attempts,
                    attempt,
                    "newfstatat attempt",
                )?;
            }
            ProcessCaptureEvent::FstatAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.fstat_attempts,
                    attempt,
                    "fstat attempt",
                )?;
            }
            ProcessCaptureEvent::GetrandomAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.getrandom_attempts,
                    attempt,
                    "getrandom attempt",
                )?;
            }
            ProcessCaptureEvent::RuntimeObservationAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.runtime_observation_attempts,
                    attempt,
                    "runtime-observation attempt",
                )?;
            }
            ProcessCaptureEvent::PrlimitAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.prlimit_attempts,
                    attempt,
                    "prlimit attempt",
                )?;
            }
            ProcessCaptureEvent::CloseAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.close_attempts,
                    attempt,
                    "close attempt",
                )?;
            }
            ProcessCaptureEvent::SeekAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.seek_attempts,
                    attempt,
                    "seek attempt",
                )?;
            }
            ProcessCaptureEvent::FileAdviceAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.file_advice_attempts,
                    attempt,
                    "file-advice attempt",
                )?;
            }
            ProcessCaptureEvent::FcntlAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.fcntl_attempts,
                    attempt,
                    "fcntl attempt",
                )?;
            }
            ProcessCaptureEvent::IoctlAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.ioctl_attempts,
                    attempt,
                    "ioctl attempt",
                )?;
            }
            ProcessCaptureEvent::PipeAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.pipe_attempts,
                    attempt,
                    "pipe attempt",
                )?;
            }
            ProcessCaptureEvent::EpollCreateAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.epoll_create_attempts,
                    attempt,
                    "epoll-create attempt",
                )?;
            }
            ProcessCaptureEvent::SocketCreateAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.socket_create_attempts,
                    attempt,
                    "socket-create attempt",
                )?;
            }
            ProcessCaptureEvent::SocketConnectAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.socket_connect_attempts,
                    attempt,
                    "socket-connect attempt",
                )?;
            }
            ProcessCaptureEvent::Dup2Attempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.dup2_attempts,
                    attempt,
                    "dup2 attempt",
                )?;
            }
            ProcessCaptureEvent::SignalStateAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.signal_state_attempts,
                    attempt,
                    "signal-state attempt",
                )?;
            }
            ProcessCaptureEvent::SignalDelivery { process, delivery } => {
                push_receipt_event(
                    &mut self.active_process(process)?.signal_deliveries,
                    delivery,
                    "signal delivery",
                )?;
            }
            ProcessCaptureEvent::UmaskAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.umask_attempts,
                    attempt,
                    "umask attempt",
                )?;
            }
            ProcessCaptureEvent::CredentialAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.credential_attempts,
                    attempt,
                    "credential attempt",
                )?;
            }
            ProcessCaptureEvent::FutexWakeAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.futex_wake_attempts,
                    attempt,
                    "futex-wake attempt",
                )?;
            }
            ProcessCaptureEvent::ProcessCreationAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.process_creation_attempts,
                    attempt,
                    "process-creation attempt",
                )?;
            }
            ProcessCaptureEvent::ChildWaitAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.child_wait_attempts,
                    attempt,
                    "child-wait attempt",
                )?;
            }
            ProcessCaptureEvent::ProcessIdentityAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.process_identity_attempts,
                    attempt,
                    "process-identity attempt",
                )?;
            }
            ProcessCaptureEvent::MemoryStateAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.memory_state_attempts,
                    attempt,
                    "memory-state attempt",
                )?;
            }
            ProcessCaptureEvent::ThreadStateAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.thread_state_attempts,
                    attempt,
                    "thread-state attempt",
                )?;
            }
            ProcessCaptureEvent::LifecycleExitAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.lifecycle_exit_attempts,
                    attempt,
                    "lifecycle-exit attempt",
                )?;
            }
            ProcessCaptureEvent::StatxAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.statx_attempts,
                    attempt,
                    "statx attempt",
                )?;
            }
            ProcessCaptureEvent::XattrAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.xattr_attempts,
                    attempt,
                    "xattr attempt",
                )?;
            }
            ProcessCaptureEvent::ReadlinkAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.readlink_attempts,
                    attempt,
                    "readlink attempt",
                )?;
            }
            ProcessCaptureEvent::AccessAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.access_attempts,
                    attempt,
                    "access attempt",
                )?;
            }
            ProcessCaptureEvent::GetcwdAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.getcwd_attempts,
                    attempt,
                    "getcwd attempt",
                )?;
            }
            ProcessCaptureEvent::CwdChangeAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.cwd_change_attempts,
                    attempt,
                    "cwd-change attempt",
                )?;
            }
            ProcessCaptureEvent::PathRemovalAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.path_removal_attempts,
                    attempt,
                    "path-removal attempt",
                )?;
            }
            ProcessCaptureEvent::DirectoryCreationAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.directory_creation_attempts,
                    attempt,
                    "directory-creation attempt",
                )?;
            }
            ProcessCaptureEvent::PathRenameAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.path_rename_attempts,
                    attempt,
                    "path-rename attempt",
                )?;
            }
            ProcessCaptureEvent::PathModeChangeAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.path_mode_change_attempts,
                    attempt,
                    "path-mode-change attempt",
                )?;
            }
            ProcessCaptureEvent::DescriptorTimestampChangeAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self
                        .active_process(process)?
                        .descriptor_timestamp_change_attempts,
                    attempt,
                    "descriptor timestamp-change attempt",
                )?;
            }
            ProcessCaptureEvent::SymbolicLinkCreationAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self
                        .active_process(process)?
                        .symbolic_link_creation_attempts,
                    attempt,
                    "symbolic-link creation attempt",
                )?;
            }
            ProcessCaptureEvent::FilesystemStatusAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.filesystem_status_attempts,
                    attempt,
                    "filesystem-status attempt",
                )?;
            }
            ProcessCaptureEvent::ReadAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.read_attempts,
                    attempt,
                    "read attempt",
                )?;
            }
            ProcessCaptureEvent::Getdents64Attempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.getdents64_attempts,
                    attempt,
                    "getdents64 attempt",
                )?;
            }
            ProcessCaptureEvent::MmapAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.mmap_attempts,
                    attempt,
                    "mmap attempt",
                )?;
            }
            ProcessCaptureEvent::WriteAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.write_attempts,
                    attempt,
                    "write attempt",
                )?;
            }
            ProcessCaptureEvent::UnmodeledSyscallAttempt { process, attempt } => {
                push_receipt_event(
                    &mut self.active_process(process)?.unmodeled_syscall_attempts,
                    attempt,
                    "unmodeled syscall attempt",
                )?;
            }
            ProcessCaptureEvent::ProcessFinished { process, summary } => {
                let active = self.active_process(process)?;
                if u64::try_from(active.invocations.len()).ok() != Some(summary.invocations)
                    || u64::try_from(active.exec_attempts.len()).ok() != Some(summary.exec_syscalls)
                    || u64::try_from(active.open_attempts.len()).ok() != Some(summary.open_syscalls)
                    || u64::try_from(active.newfstatat_attempts.len()).ok()
                        != Some(summary.newfstatat_syscalls)
                    || u64::try_from(active.fstat_attempts.len()).ok()
                        != Some(summary.fstat_syscalls)
                    || u64::try_from(active.getrandom_attempts.len()).ok()
                        != Some(summary.getrandom_syscalls)
                    || u64::try_from(active.runtime_observation_attempts.len()).ok()
                        != Some(summary.runtime_observation_syscalls)
                    || u64::try_from(active.prlimit_attempts.len()).ok()
                        != Some(summary.prlimit_syscalls)
                    || u64::try_from(active.close_attempts.len()).ok()
                        != Some(summary.close_syscalls)
                    || u64::try_from(active.seek_attempts.len()).ok() != Some(summary.seek_syscalls)
                    || u64::try_from(active.file_advice_attempts.len()).ok()
                        != Some(summary.file_advice_syscalls)
                    || u64::try_from(active.fcntl_attempts.len()).ok()
                        != Some(summary.fcntl_syscalls)
                    || u64::try_from(active.ioctl_attempts.len()).ok()
                        != Some(summary.ioctl_syscalls)
                    || u64::try_from(active.pipe_attempts.len()).ok() != Some(summary.pipe_syscalls)
                    || u64::try_from(active.epoll_create_attempts.len()).ok()
                        != Some(summary.epoll_create_syscalls)
                    || u64::try_from(active.socket_create_attempts.len()).ok()
                        != Some(summary.socket_create_syscalls)
                    || u64::try_from(active.socket_connect_attempts.len()).ok()
                        != Some(summary.socket_connect_syscalls)
                    || u64::try_from(active.dup2_attempts.len()).ok() != Some(summary.dup2_syscalls)
                    || u64::try_from(active.signal_state_attempts.len()).ok()
                        != Some(summary.signal_state_syscalls)
                    || u64::try_from(active.signal_deliveries.len()).ok()
                        != Some(summary.signal_deliveries)
                    || u64::try_from(active.umask_attempts.len()).ok()
                        != Some(summary.umask_syscalls)
                    || u64::try_from(active.credential_attempts.len()).ok()
                        != Some(summary.credential_syscalls)
                    || u64::try_from(active.futex_wake_attempts.len()).ok()
                        != Some(summary.futex_wake_syscalls)
                    || u64::try_from(active.process_creation_attempts.len()).ok()
                        != Some(summary.process_creation_syscalls)
                    || u64::try_from(active.child_wait_attempts.len()).ok()
                        != Some(summary.child_wait_syscalls)
                    || u64::try_from(active.process_identity_attempts.len()).ok()
                        != Some(summary.process_identity_syscalls)
                    || u64::try_from(active.memory_state_attempts.len()).ok()
                        != Some(summary.memory_state_syscalls)
                    || u64::try_from(active.thread_state_attempts.len()).ok()
                        != Some(summary.thread_state_syscalls)
                    || u64::try_from(active.lifecycle_exit_attempts.len()).ok()
                        != Some(summary.lifecycle_exit_syscalls)
                    || u64::try_from(active.statx_attempts.len()).ok()
                        != Some(summary.statx_syscalls)
                    || u64::try_from(active.xattr_attempts.len()).ok()
                        != Some(summary.xattr_syscalls)
                    || u64::try_from(active.readlink_attempts.len()).ok()
                        != Some(summary.readlink_syscalls)
                    || u64::try_from(active.access_attempts.len()).ok()
                        != Some(summary.access_syscalls)
                    || u64::try_from(active.getcwd_attempts.len()).ok()
                        != Some(summary.getcwd_syscalls)
                    || u64::try_from(active.cwd_change_attempts.len()).ok()
                        != Some(summary.cwd_change_syscalls)
                    || u64::try_from(active.path_removal_attempts.len()).ok()
                        != Some(summary.path_removal_syscalls)
                    || u64::try_from(active.directory_creation_attempts.len()).ok()
                        != Some(summary.directory_creation_syscalls)
                    || u64::try_from(active.path_rename_attempts.len()).ok()
                        != Some(summary.path_rename_syscalls)
                    || u64::try_from(active.path_mode_change_attempts.len()).ok()
                        != Some(summary.path_mode_change_syscalls)
                    || u64::try_from(active.descriptor_timestamp_change_attempts.len()).ok()
                        != Some(summary.descriptor_timestamp_change_syscalls)
                    || u64::try_from(active.symbolic_link_creation_attempts.len()).ok()
                        != Some(summary.symbolic_link_creation_syscalls)
                    || u64::try_from(active.filesystem_status_attempts.len()).ok()
                        != Some(summary.filesystem_status_syscalls)
                    || u64::try_from(active.read_attempts.len()).ok() != Some(summary.read_syscalls)
                    || u64::try_from(active.getdents64_attempts.len()).ok()
                        != Some(summary.getdents64_syscalls)
                    || u64::try_from(active.mmap_attempts.len()).ok() != Some(summary.mmap_syscalls)
                    || u64::try_from(active.write_attempts.len()).ok()
                        != Some(summary.write_syscalls)
                    || u64::try_from(active.unmodeled_syscall_attempts.len()).ok()
                        != Some(summary.unmodeled_syscalls)
                {
                    anyhow::bail!("process capture finish has an invalid event census");
                }
                active.finish = Some(summary);
            }
        }
        Ok(())
    }
}

fn push_receipt_event<T>(events: &mut Vec<T>, event: T, description: &str) -> Result<()> {
    events
        .try_reserve(1)
        .with_context(|| format!("reserve process capture {description}"))?;
    events.push(event);
    Ok(())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct EventCounts {
    process_started: u64,
    process_finished: u64,
    census: ProcessCaptureStreamCensus,
    denominators: ProcessCaptureStreamCensus,
}

#[derive(Debug, Clone, Copy, Default)]
struct StreamProcessState {
    finished: bool,
    census: ProcessCaptureStreamCensus,
    last_modeled_syscall: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct SemanticStreamProcessState {
    finished: bool,
    is_root: bool,
    parent: Option<CapturedProcessId>,
    thread_group: CapturedProcessId,
    census: ProcessCaptureStreamCensus,
    last_modeled_syscall: Option<u64>,
    maximum_signal_delivery_syscall: Option<u64>,
    next_loaded_invocation: u64,
    modeled_terminal_syscalls: u64,
    lifecycle_exit_outcome: Option<CapturedProcessOutcome>,
}

#[derive(Debug, Default)]
struct SemanticStreamValidator {
    processes: BTreeMap<CapturedProcessId, SemanticStreamProcessState>,
    root: Option<CapturedProcessId>,
    created_processes: std::collections::BTreeSet<CapturedProcessId>,
    reaped_processes: std::collections::BTreeSet<CapturedProcessId>,
    signal_deliveries: BTreeMap<(CapturedProcessId, u64), CapturedSignalDelivery>,
    caught_signal_frames: BTreeMap<CapturedProcessId, Vec<u64>>,
}

impl SemanticStreamValidator {
    fn record(&mut self, event: &ProcessCaptureEvent) -> Result<()> {
        if let ProcessCaptureEvent::ProcessStarted { process, parent } = event {
            if *process == 0
                || parent == &Some(*process)
                || self.processes.contains_key(process)
                || parent.is_some_and(|parent| {
                    self.processes
                        .get(&parent)
                        .is_none_or(|state| state.finished)
                })
                || (parent.is_none() && self.root.is_some())
            {
                anyhow::bail!("process capture stream has an invalid process start");
            }
            let is_root = parent.is_none();
            self.processes.insert(
                *process,
                SemanticStreamProcessState {
                    finished: false,
                    is_root,
                    parent: *parent,
                    thread_group: *process,
                    census: ProcessCaptureStreamCensus::default(),
                    last_modeled_syscall: None,
                    maximum_signal_delivery_syscall: None,
                    next_loaded_invocation: u64::from(is_root),
                    modeled_terminal_syscalls: 0,
                    lifecycle_exit_outcome: None,
                },
            );
            self.caught_signal_frames.insert(*process, Vec::new());
            if is_root {
                self.root = Some(*process);
            }
            return Ok(());
        }

        if let ProcessCaptureEvent::SignalDelivery { delivery, .. } = event {
            if let crate::CapturedSignalSource::TracedProcess { process } = delivery.source {
                if !self.processes.contains_key(&process) {
                    anyhow::bail!("process capture stream has an unknown signal-delivery source");
                }
            }
        }
        if let ProcessCaptureEvent::SignalDelivery { process, delivery } = event {
            validate_captured_signal_delivery(delivery)?;
            let delivery_index = self
                .processes
                .get(process)
                .context("signal delivery was not started")?
                .census
                .signal_deliveries;
            if self
                .signal_deliveries
                .insert((*process, delivery_index), *delivery)
                .is_some()
            {
                anyhow::bail!("process capture stream has a duplicate signal delivery");
            }
            if captured_signal_delivery_creates_frame(delivery) {
                self.caught_signal_frames
                    .get_mut(process)
                    .context("signal delivery has no frame stack")?
                    .push(delivery_index);
            }
        }
        if let ProcessCaptureEvent::ThreadStateAttempt {
            process,
            attempt:
                CapturedThreadStateAttempt {
                    request: crate::CapturedThreadStateRequest::SignalReturn {
                        delivery,
                        ..
                    },
                    ..
                },
        } = event
        {
            if self
                .caught_signal_frames
                .get_mut(process)
                .context("signal return has no frame stack")?
                .pop()
                != Some(*delivery)
            {
                anyhow::bail!("process capture stream has an invalid signal return");
            }
        }
        if let ProcessCaptureEvent::ProcessCreationAttempt {
            process,
            attempt:
                CapturedProcessCreationAttempt {
                    request,
                    outcome: CapturedProcessCreationOutcome::Created(child),
                    ..
                },
        } = event
        {
            if self.processes.get(child).map(|state| state.parent) != Some(Some(*process))
                || !self.created_processes.insert(*child)
            {
                anyhow::bail!("process capture stream has an invalid created-process edge");
            }
            if crate::process_creation_shares_thread_group(*request) {
                let parent_group = self
                    .processes
                    .get(process)
                    .context("created thread has no parent state")?
                    .thread_group;
                self.processes
                    .get_mut(child)
                    .context("created thread has no child state")?
                    .thread_group = parent_group;
            }
        }
        if let ProcessCaptureEvent::ChildWaitAttempt { process, attempt } = event {
            if let crate::CapturedChildWaitSelector::Child(child) = attempt.request.selector {
                if self.processes.get(&child).map(|state| state.parent) != Some(Some(*process)) {
                    anyhow::bail!(
                        "process capture stream has an invalid requested child-wait edge"
                    );
                }
            }
            if let CapturedChildWaitOutcome::Reaped { child, .. } = attempt.outcome {
                if self.processes.get(&child).map(|state| state.parent) != Some(Some(*process))
                    || !self.reaped_processes.insert(child)
                {
                    anyhow::bail!("process capture stream has an invalid reaped-process edge");
                }
            }
        }
        if let ProcessCaptureEvent::ProcessIdentityAttempt { process, attempt } = event {
            let valid = match (attempt.request, attempt.outcome) {
                (
                    CapturedProcessIdentityRequest::Getpid,
                    CapturedProcessIdentityOutcome::Identity(identity),
                ) => self.processes.get(process).map(|state| state.thread_group) == Some(identity),
                (
                    CapturedProcessIdentityRequest::Gettid,
                    CapturedProcessIdentityOutcome::Identity(identity),
                ) => identity == *process,
                (
                    CapturedProcessIdentityRequest::Getppid,
                    CapturedProcessIdentityOutcome::Identity(identity),
                ) => self
                    .processes
                    .get(process)
                    .and_then(|state| self.processes.get(&state.thread_group))
                    .and_then(|leader| leader.parent)
                    .and_then(|parent| self.processes.get(&parent))
                    .map(|parent| parent.thread_group)
                    == Some(identity),
                (
                    CapturedProcessIdentityRequest::Getpgrp,
                    CapturedProcessIdentityOutcome::ProcessGroup(_),
                )
                | (_, CapturedProcessIdentityOutcome::Terminated) => true,
                _ => false,
            };
            if !valid {
                anyhow::bail!("process capture stream has an invalid process-identity edge");
            }
        }
        let process = event.process();
        let state = self
            .processes
            .get_mut(&process)
            .context("process capture event precedes its process start")?;
        if state.finished {
            anyhow::bail!("process capture event follows its process finish");
        }
        if let Some(index) = event.syscall_index() {
            if state
                .last_modeled_syscall
                .is_some_and(|prior| prior >= index)
            {
                anyhow::bail!("process capture stream has a reordered syscall attempt");
            }
        }

        match event {
            ProcessCaptureEvent::ProcessStarted { .. } => unreachable!(),
            ProcessCaptureEvent::Invocation { invocation, .. } => {
                validate_captured_invocation(invocation)?;
                checked_increment(&mut state.census.invocations, "process invocation census")?;
            }
            ProcessCaptureEvent::ExecAttempt { attempt, .. } => {
                if let Some(index) = validate_captured_exec_attempt(attempt)? {
                    if index == state.next_loaded_invocation
                        && index.checked_add(1) == Some(state.census.invocations)
                    {
                        state.next_loaded_invocation = state
                            .next_loaded_invocation
                            .checked_add(1)
                            .context("process loaded-invocation census overflowed")?;
                    } else {
                        anyhow::bail!("process capture stream has an invalid exec result");
                    }
                }
                checked_increment(&mut state.census.exec_attempts, "process exec census")?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::OpenAttempt { attempt, .. } => {
                if validate_captured_open_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(&mut state.census.open_attempts, "process open census")?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::NewfstatatAttempt { attempt, .. } => {
                if validate_captured_newfstatat_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(
                    &mut state.census.newfstatat_attempts,
                    "process newfstatat census",
                )?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::FstatAttempt { attempt, .. } => {
                if validate_captured_fstat_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(&mut state.census.fstat_attempts, "process fstat census")?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::GetrandomAttempt { attempt, .. } => {
                if validate_captured_getrandom_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(
                    &mut state.census.getrandom_attempts,
                    "process getrandom census",
                )?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::RuntimeObservationAttempt { attempt, .. } => {
                if validate_captured_runtime_observation_attempt(attempt).with_context(|| {
                    format!(
                        "validate stream runtime observation for process {} at syscall {}: \
                         request={:?} outcome={:?}",
                        process, attempt.syscall_index, attempt.request, attempt.outcome
                    )
                })? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(
                    &mut state.census.runtime_observation_attempts,
                    "process runtime-observation census",
                )?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::PrlimitAttempt { attempt, .. } => {
                if validate_captured_prlimit_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(&mut state.census.prlimit_attempts, "process prlimit census")?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::CloseAttempt { attempt, .. } => {
                if validate_captured_close_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(&mut state.census.close_attempts, "process close census")?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::SeekAttempt { attempt, .. } => {
                if validate_captured_seek_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(&mut state.census.seek_attempts, "process seek census")?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::FileAdviceAttempt { attempt, .. } => {
                if validate_captured_file_advice_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(
                    &mut state.census.file_advice_attempts,
                    "process file-advice census",
                )?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::FcntlAttempt { attempt, .. } => {
                if validate_captured_fcntl_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(&mut state.census.fcntl_attempts, "process fcntl census")?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::IoctlAttempt { attempt, .. } => {
                if validate_captured_ioctl_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(&mut state.census.ioctl_attempts, "process ioctl census")?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::PipeAttempt { attempt, .. } => {
                if validate_captured_pipe_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(&mut state.census.pipe_attempts, "process pipe census")?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::EpollCreateAttempt { attempt, .. } => {
                if validate_captured_epoll_create_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(
                    &mut state.census.epoll_create_attempts,
                    "process epoll-create census",
                )?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::SocketCreateAttempt { attempt, .. } => {
                if validate_captured_socket_create_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(
                    &mut state.census.socket_create_attempts,
                    "process socket-create census",
                )?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::SocketConnectAttempt { attempt, .. } => {
                if validate_captured_socket_connect_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(
                    &mut state.census.socket_connect_attempts,
                    "process socket-connect census",
                )?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::Dup2Attempt { attempt, .. } => {
                if validate_captured_dup2_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(&mut state.census.dup2_attempts, "process dup2 census")?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::SignalStateAttempt { attempt, .. } => {
                if validate_captured_signal_state_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(
                    &mut state.census.signal_state_attempts,
                    "process signal-state census",
                )?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::SignalDelivery { delivery, .. } => {
                validate_captured_signal_delivery(delivery)?;
                if let Some(index) = delivery.active_syscall {
                    state.maximum_signal_delivery_syscall = Some(
                        state
                            .maximum_signal_delivery_syscall
                            .map_or(index, |prior| prior.max(index)),
                    );
                }
                checked_increment(
                    &mut state.census.signal_deliveries,
                    "process signal-delivery census",
                )?;
            }
            ProcessCaptureEvent::UmaskAttempt { attempt, .. } => {
                if validate_captured_umask_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(&mut state.census.umask_attempts, "process umask census")?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::CredentialAttempt { attempt, .. } => {
                if validate_captured_credential_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(
                    &mut state.census.credential_attempts,
                    "process credential census",
                )?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::FutexWakeAttempt { attempt, .. } => {
                if validate_captured_futex_wake_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(
                    &mut state.census.futex_wake_attempts,
                    "process futex-wake census",
                )?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::ProcessCreationAttempt { attempt, .. } => {
                if validate_captured_process_creation_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(
                    &mut state.census.process_creation_attempts,
                    "process creation census",
                )?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::ChildWaitAttempt { attempt, .. } => {
                if validate_captured_child_wait_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(
                    &mut state.census.child_wait_attempts,
                    "process child-wait census",
                )?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::ProcessIdentityAttempt { attempt, .. } => {
                if validate_captured_process_identity_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(
                    &mut state.census.process_identity_attempts,
                    "process identity census",
                )?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::MemoryStateAttempt { attempt, .. } => {
                if validate_captured_memory_state_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(
                    &mut state.census.memory_state_attempts,
                    "process memory-state census",
                )?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::ThreadStateAttempt { attempt, .. } => {
                if validate_captured_thread_state_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(
                    &mut state.census.thread_state_attempts,
                    "process thread-state census",
                )?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::LifecycleExitAttempt { attempt, .. } => {
                if !validate_captured_lifecycle_exit_attempt(attempt, attempt.outcome)?
                    || state
                        .lifecycle_exit_outcome
                        .replace(attempt.outcome)
                        .is_some()
                {
                    anyhow::bail!("process capture stream has an invalid lifecycle-exit attempt");
                }
                checked_increment(
                    &mut state.modeled_terminal_syscalls,
                    "process modeled terminal census",
                )?;
                checked_increment(
                    &mut state.census.lifecycle_exit_attempts,
                    "process lifecycle-exit census",
                )?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::StatxAttempt { attempt, .. } => {
                if validate_captured_statx_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(&mut state.census.statx_attempts, "process statx census")?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::XattrAttempt { attempt, .. } => {
                if validate_captured_xattr_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(&mut state.census.xattr_attempts, "process xattr census")?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::ReadlinkAttempt { attempt, .. } => {
                if validate_captured_readlink_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(
                    &mut state.census.readlink_attempts,
                    "process readlink census",
                )?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::AccessAttempt { attempt, .. } => {
                if validate_captured_access_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(&mut state.census.access_attempts, "process access census")?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::GetcwdAttempt { attempt, .. } => {
                if validate_captured_getcwd_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(&mut state.census.getcwd_attempts, "process getcwd census")?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::CwdChangeAttempt { attempt, .. } => {
                if validate_captured_cwd_change_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(
                    &mut state.census.cwd_change_attempts,
                    "process cwd-change census",
                )?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::PathRemovalAttempt { attempt, .. } => {
                if validate_captured_path_removal_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(
                    &mut state.census.path_removal_attempts,
                    "process path-removal census",
                )?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::DirectoryCreationAttempt { attempt, .. } => {
                if validate_captured_directory_creation_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(
                    &mut state.census.directory_creation_attempts,
                    "process directory-creation census",
                )?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::PathRenameAttempt { attempt, .. } => {
                if validate_captured_path_rename_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(
                    &mut state.census.path_rename_attempts,
                    "process path-rename census",
                )?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::PathModeChangeAttempt { attempt, .. } => {
                if validate_captured_path_mode_change_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(
                    &mut state.census.path_mode_change_attempts,
                    "process path-mode-change census",
                )?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::DescriptorTimestampChangeAttempt { attempt, .. } => {
                if validate_captured_descriptor_timestamp_change_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(
                    &mut state.census.descriptor_timestamp_change_attempts,
                    "process descriptor timestamp-change census",
                )?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::SymbolicLinkCreationAttempt { attempt, .. } => {
                if validate_captured_symbolic_link_creation_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(
                    &mut state.census.symbolic_link_creation_attempts,
                    "process symbolic-link creation census",
                )?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::FilesystemStatusAttempt { attempt, .. } => {
                if validate_captured_filesystem_status_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(
                    &mut state.census.filesystem_status_attempts,
                    "process filesystem-status census",
                )?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::ReadAttempt { attempt, .. } => {
                if validate_captured_read_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(&mut state.census.read_attempts, "process read census")?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::Getdents64Attempt { attempt, .. } => {
                if validate_captured_getdents64_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(
                    &mut state.census.getdents64_attempts,
                    "process getdents64 census",
                )?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::MmapAttempt { attempt, .. } => {
                if validate_captured_mmap_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(&mut state.census.mmap_attempts, "process mmap census")?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::WriteAttempt { attempt, .. } => {
                if validate_captured_write_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process modeled terminal census",
                    )?;
                }
                checked_increment(&mut state.census.write_attempts, "process write census")?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::UnmodeledSyscallAttempt { attempt, .. } => {
                if validate_captured_unmodeled_syscall_attempt(attempt)? {
                    checked_increment(
                        &mut state.modeled_terminal_syscalls,
                        "process terminal syscall census",
                    )?;
                }
                checked_increment(
                    &mut state.census.unmodeled_syscall_attempts,
                    "process unmodeled syscall census",
                )?;
                state.last_modeled_syscall = Some(attempt.syscall_index);
            }
            ProcessCaptureEvent::ProcessFinished { summary, .. } => {
                let expected = ProcessCaptureStreamCensus {
                    invocations: summary.invocations,
                    exec_attempts: summary.exec_syscalls,
                    open_attempts: summary.open_syscalls,
                    newfstatat_attempts: summary.newfstatat_syscalls,
                    fstat_attempts: summary.fstat_syscalls,
                    getrandom_attempts: summary.getrandom_syscalls,
                    runtime_observation_attempts: summary.runtime_observation_syscalls,
                    prlimit_attempts: summary.prlimit_syscalls,
                    close_attempts: summary.close_syscalls,
                    seek_attempts: summary.seek_syscalls,
                    file_advice_attempts: summary.file_advice_syscalls,
                    fcntl_attempts: summary.fcntl_syscalls,
                    ioctl_attempts: summary.ioctl_syscalls,
                    pipe_attempts: summary.pipe_syscalls,
                    epoll_create_attempts: summary.epoll_create_syscalls,
                    socket_create_attempts: summary.socket_create_syscalls,
                    socket_connect_attempts: summary.socket_connect_syscalls,
                    dup2_attempts: summary.dup2_syscalls,
                    signal_state_attempts: summary.signal_state_syscalls,
                    signal_deliveries: summary.signal_deliveries,
                    umask_attempts: summary.umask_syscalls,
                    credential_attempts: summary.credential_syscalls,
                    futex_wake_attempts: summary.futex_wake_syscalls,
                    process_creation_attempts: summary.process_creation_syscalls,
                    child_wait_attempts: summary.child_wait_syscalls,
                    process_identity_attempts: summary.process_identity_syscalls,
                    memory_state_attempts: summary.memory_state_syscalls,
                    thread_state_attempts: summary.thread_state_syscalls,
                    lifecycle_exit_attempts: summary.lifecycle_exit_syscalls,
                    statx_attempts: summary.statx_syscalls,
                    xattr_attempts: summary.xattr_syscalls,
                    readlink_attempts: summary.readlink_syscalls,
                    access_attempts: summary.access_syscalls,
                    getcwd_attempts: summary.getcwd_syscalls,
                    cwd_change_attempts: summary.cwd_change_syscalls,
                    path_removal_attempts: summary.path_removal_syscalls,
                    directory_creation_attempts: summary.directory_creation_syscalls,
                    path_rename_attempts: summary.path_rename_syscalls,
                    path_mode_change_attempts: summary.path_mode_change_syscalls,
                    descriptor_timestamp_change_attempts: summary
                        .descriptor_timestamp_change_syscalls,
                    symbolic_link_creation_attempts: summary.symbolic_link_creation_syscalls,
                    filesystem_status_attempts: summary.filesystem_status_syscalls,
                    read_attempts: summary.read_syscalls,
                    getdents64_attempts: summary.getdents64_syscalls,
                    mmap_attempts: summary.mmap_syscalls,
                    write_attempts: summary.write_syscalls,
                    unmodeled_syscall_attempts: summary.unmodeled_syscalls,
                    syscall_entries: 0,
                    syscall_exits: 0,
                    terminal_syscalls: 0,
                };
                if state.census != expected
                    || expected.recorded_syscall_attempts()? != summary.syscall_entries
                    || state.next_loaded_invocation != state.census.invocations
                    || (state.is_root && state.census.invocations == 0)
                    || summary.syscall_entries
                        != summary
                            .syscall_exits
                            .checked_add(summary.terminal_syscalls)
                            .context("process syscall census overflowed")?
                    || state
                        .last_modeled_syscall
                        .is_some_and(|index| index >= summary.syscall_entries)
                    || state
                        .maximum_signal_delivery_syscall
                        .is_some_and(|index| index >= summary.syscall_entries)
                    || state.modeled_terminal_syscalls > summary.terminal_syscalls
                    || state
                        .lifecycle_exit_outcome
                        .is_some_and(|outcome| outcome != summary.outcome)
                {
                    anyhow::bail!("process capture finish has invalid semantic coverage");
                }
                state.finished = true;
            }
        }
        Ok(())
    }

    fn finish(&self, root: CapturedProcessId) -> Result<()> {
        if self.processes.is_empty()
            || self.root != Some(root)
            || self.processes.values().any(|state| !state.finished)
        {
            anyhow::bail!("process capture stream has incomplete semantic lifecycles");
        }
        Ok(())
    }
}

fn checked_increment(value: &mut u64, description: &str) -> Result<()> {
    *value = value
        .checked_add(1)
        .with_context(|| format!("{description} overflowed"))?;
    Ok(())
}

impl EventCounts {
    fn record_tag(&mut self, tag: u8) -> Result<()> {
        let counter = match tag {
            FRAME_TAG_PROCESS_STARTED => &mut self.process_started,
            FRAME_TAG_INVOCATION => &mut self.census.invocations,
            FRAME_TAG_EXEC_ATTEMPT => &mut self.census.exec_attempts,
            FRAME_TAG_OPEN_ATTEMPT => &mut self.census.open_attempts,
            FRAME_TAG_NEWFSTATAT_ATTEMPT => &mut self.census.newfstatat_attempts,
            FRAME_TAG_FSTAT_ATTEMPT => &mut self.census.fstat_attempts,
            FRAME_TAG_GETRANDOM_ATTEMPT => &mut self.census.getrandom_attempts,
            FRAME_TAG_RUNTIME_OBSERVATION_ATTEMPT => &mut self.census.runtime_observation_attempts,
            FRAME_TAG_PRLIMIT_ATTEMPT => &mut self.census.prlimit_attempts,
            FRAME_TAG_CLOSE_ATTEMPT => &mut self.census.close_attempts,
            FRAME_TAG_SEEK_ATTEMPT => &mut self.census.seek_attempts,
            FRAME_TAG_FILE_ADVICE_ATTEMPT => &mut self.census.file_advice_attempts,
            FRAME_TAG_FCNTL_ATTEMPT => &mut self.census.fcntl_attempts,
            FRAME_TAG_IOCTL_ATTEMPT => &mut self.census.ioctl_attempts,
            FRAME_TAG_PIPE_ATTEMPT => &mut self.census.pipe_attempts,
            FRAME_TAG_EPOLL_CREATE_ATTEMPT => &mut self.census.epoll_create_attempts,
            FRAME_TAG_SOCKET_CREATE_ATTEMPT => {
                &mut self.census.socket_create_attempts
            }
            FRAME_TAG_SOCKET_CONNECT_ATTEMPT => {
                &mut self.census.socket_connect_attempts
            }
            FRAME_TAG_DUP2_ATTEMPT => &mut self.census.dup2_attempts,
            FRAME_TAG_SIGNAL_STATE_ATTEMPT => &mut self.census.signal_state_attempts,
            FRAME_TAG_SIGNAL_DELIVERY => &mut self.census.signal_deliveries,
            FRAME_TAG_UMASK_ATTEMPT => &mut self.census.umask_attempts,
            FRAME_TAG_CREDENTIAL_ATTEMPT => &mut self.census.credential_attempts,
            FRAME_TAG_FUTEX_WAKE_ATTEMPT => &mut self.census.futex_wake_attempts,
            FRAME_TAG_PROCESS_CREATION_ATTEMPT => &mut self.census.process_creation_attempts,
            FRAME_TAG_CHILD_WAIT_ATTEMPT => &mut self.census.child_wait_attempts,
            FRAME_TAG_PROCESS_IDENTITY_ATTEMPT => &mut self.census.process_identity_attempts,
            FRAME_TAG_MEMORY_STATE_ATTEMPT => &mut self.census.memory_state_attempts,
            FRAME_TAG_THREAD_STATE_ATTEMPT => &mut self.census.thread_state_attempts,
            FRAME_TAG_LIFECYCLE_EXIT_ATTEMPT => &mut self.census.lifecycle_exit_attempts,
            FRAME_TAG_STATX_ATTEMPT => &mut self.census.statx_attempts,
            FRAME_TAG_XATTR_ATTEMPT => &mut self.census.xattr_attempts,
            FRAME_TAG_READLINK_ATTEMPT => &mut self.census.readlink_attempts,
            FRAME_TAG_ACCESS_ATTEMPT => &mut self.census.access_attempts,
            FRAME_TAG_GETCWD_ATTEMPT => &mut self.census.getcwd_attempts,
            FRAME_TAG_CWD_CHANGE_ATTEMPT => &mut self.census.cwd_change_attempts,
            FRAME_TAG_PATH_REMOVAL_ATTEMPT => &mut self.census.path_removal_attempts,
            FRAME_TAG_DIRECTORY_CREATION_ATTEMPT => &mut self.census.directory_creation_attempts,
            FRAME_TAG_PATH_RENAME_ATTEMPT => &mut self.census.path_rename_attempts,
            FRAME_TAG_PATH_MODE_CHANGE_ATTEMPT => &mut self.census.path_mode_change_attempts,
            FRAME_TAG_DESCRIPTOR_TIMESTAMP_CHANGE_ATTEMPT => {
                &mut self.census.descriptor_timestamp_change_attempts
            }
            FRAME_TAG_SYMBOLIC_LINK_CREATION_ATTEMPT => {
                &mut self.census.symbolic_link_creation_attempts
            }
            FRAME_TAG_FILESYSTEM_STATUS_ATTEMPT => &mut self.census.filesystem_status_attempts,
            FRAME_TAG_READ_ATTEMPT => &mut self.census.read_attempts,
            FRAME_TAG_GETDENTS64_ATTEMPT => &mut self.census.getdents64_attempts,
            FRAME_TAG_MMAP_ATTEMPT => &mut self.census.mmap_attempts,
            FRAME_TAG_WRITE_ATTEMPT => &mut self.census.write_attempts,
            FRAME_TAG_UNMODELED_SYSCALL_ATTEMPT => &mut self.census.unmodeled_syscall_attempts,
            FRAME_TAG_PROCESS_FINISHED => &mut self.process_finished,
            _ => anyhow::bail!("process capture stream has an unknown record tag"),
        };
        *counter = counter
            .checked_add(1)
            .context("process capture stream event census overflowed")?;
        Ok(())
    }

    fn add_finish(&mut self, finish: ProcessFinish) -> Result<()> {
        for (total, value) in [
            (&mut self.denominators.invocations, finish.invocations),
            (&mut self.denominators.exec_attempts, finish.exec_syscalls),
            (&mut self.denominators.open_attempts, finish.open_syscalls),
            (
                &mut self.denominators.newfstatat_attempts,
                finish.newfstatat_syscalls,
            ),
            (&mut self.denominators.fstat_attempts, finish.fstat_syscalls),
            (
                &mut self.denominators.getrandom_attempts,
                finish.getrandom_syscalls,
            ),
            (
                &mut self.denominators.runtime_observation_attempts,
                finish.runtime_observation_syscalls,
            ),
            (
                &mut self.denominators.prlimit_attempts,
                finish.prlimit_syscalls,
            ),
            (&mut self.denominators.close_attempts, finish.close_syscalls),
            (&mut self.denominators.seek_attempts, finish.seek_syscalls),
            (
                &mut self.denominators.file_advice_attempts,
                finish.file_advice_syscalls,
            ),
            (&mut self.denominators.fcntl_attempts, finish.fcntl_syscalls),
            (&mut self.denominators.ioctl_attempts, finish.ioctl_syscalls),
            (&mut self.denominators.pipe_attempts, finish.pipe_syscalls),
            (
                &mut self.denominators.epoll_create_attempts,
                finish.epoll_create_syscalls,
            ),
            (
                &mut self.denominators.socket_create_attempts,
                finish.socket_create_syscalls,
            ),
            (
                &mut self.denominators.socket_connect_attempts,
                finish.socket_connect_syscalls,
            ),
            (&mut self.denominators.dup2_attempts, finish.dup2_syscalls),
            (
                &mut self.denominators.signal_state_attempts,
                finish.signal_state_syscalls,
            ),
            (
                &mut self.denominators.signal_deliveries,
                finish.signal_deliveries,
            ),
            (&mut self.denominators.umask_attempts, finish.umask_syscalls),
            (
                &mut self.denominators.credential_attempts,
                finish.credential_syscalls,
            ),
            (
                &mut self.denominators.futex_wake_attempts,
                finish.futex_wake_syscalls,
            ),
            (
                &mut self.denominators.process_creation_attempts,
                finish.process_creation_syscalls,
            ),
            (
                &mut self.denominators.child_wait_attempts,
                finish.child_wait_syscalls,
            ),
            (
                &mut self.denominators.process_identity_attempts,
                finish.process_identity_syscalls,
            ),
            (
                &mut self.denominators.memory_state_attempts,
                finish.memory_state_syscalls,
            ),
            (
                &mut self.denominators.thread_state_attempts,
                finish.thread_state_syscalls,
            ),
            (
                &mut self.denominators.lifecycle_exit_attempts,
                finish.lifecycle_exit_syscalls,
            ),
            (&mut self.denominators.statx_attempts, finish.statx_syscalls),
            (&mut self.denominators.xattr_attempts, finish.xattr_syscalls),
            (
                &mut self.denominators.readlink_attempts,
                finish.readlink_syscalls,
            ),
            (
                &mut self.denominators.access_attempts,
                finish.access_syscalls,
            ),
            (
                &mut self.denominators.getcwd_attempts,
                finish.getcwd_syscalls,
            ),
            (
                &mut self.denominators.cwd_change_attempts,
                finish.cwd_change_syscalls,
            ),
            (
                &mut self.denominators.path_removal_attempts,
                finish.path_removal_syscalls,
            ),
            (
                &mut self.denominators.directory_creation_attempts,
                finish.directory_creation_syscalls,
            ),
            (
                &mut self.denominators.path_rename_attempts,
                finish.path_rename_syscalls,
            ),
            (
                &mut self.denominators.path_mode_change_attempts,
                finish.path_mode_change_syscalls,
            ),
            (
                &mut self.denominators.descriptor_timestamp_change_attempts,
                finish.descriptor_timestamp_change_syscalls,
            ),
            (
                &mut self.denominators.symbolic_link_creation_attempts,
                finish.symbolic_link_creation_syscalls,
            ),
            (
                &mut self.denominators.filesystem_status_attempts,
                finish.filesystem_status_syscalls,
            ),
            (&mut self.denominators.read_attempts, finish.read_syscalls),
            (
                &mut self.denominators.getdents64_attempts,
                finish.getdents64_syscalls,
            ),
            (&mut self.denominators.mmap_attempts, finish.mmap_syscalls),
            (&mut self.denominators.write_attempts, finish.write_syscalls),
            (
                &mut self.denominators.unmodeled_syscall_attempts,
                finish.unmodeled_syscalls,
            ),
            (
                &mut self.denominators.syscall_entries,
                finish.syscall_entries,
            ),
            (&mut self.denominators.syscall_exits, finish.syscall_exits),
            (
                &mut self.denominators.terminal_syscalls,
                finish.terminal_syscalls,
            ),
        ] {
            *total = total
                .checked_add(value)
                .context("process capture stream denominator census overflowed")?;
        }
        self.census.syscall_entries = self
            .census
            .syscall_entries
            .checked_add(finish.syscall_entries)
            .context("process capture stream syscall-entry census overflowed")?;
        self.census.syscall_exits = self
            .census
            .syscall_exits
            .checked_add(finish.syscall_exits)
            .context("process capture stream syscall-exit census overflowed")?;
        self.census.terminal_syscalls = self
            .census
            .terminal_syscalls
            .checked_add(finish.terminal_syscalls)
            .context("process capture stream terminal census overflowed")?;
        Ok(())
    }
}

pub(crate) struct StreamCaptureSink {
    file: BufWriter<File>,
    limits: ProcessCaptureStreamLimits,
    identity: ProcessCaptureStreamIdentity,
    hasher: blake3::Hasher,
    stream_bytes: u64,
    payload_bytes: u64,
    records: u64,
    counts: EventCounts,
    process_states: BTreeMap<CapturedProcessId, StreamProcessState>,
    root: Option<CapturedProcessId>,
}

impl StreamCaptureSink {
    fn create(
        path: &Path,
        identity: ProcessCaptureStreamIdentity,
        limits: ProcessCaptureStreamLimits,
    ) -> Result<Self> {
        validate_private_parent(path)?;
        let mut options = OpenOptions::new();
        options
            .write(true)
            .append(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        let file = options
            .open(path)
            .context("create private process capture stream")?;
        validate_private_file(&file)?;
        let mut sink = Self {
            file: BufWriter::with_capacity(1024 * 1024, file),
            limits,
            identity,
            hasher: blake3::Hasher::new(),
            stream_bytes: 0,
            payload_bytes: 0,
            records: 0,
            counts: EventCounts::default(),
            process_states: BTreeMap::new(),
            root: None,
        };
        sink.hasher.update(DIGEST_DOMAIN);
        let header = encode_header(identity);
        sink.write_hashed(&header)?;
        Ok(sink)
    }

    fn finish(
        mut self,
        root: CapturedProcessId,
        path: &Path,
    ) -> Result<ProcessCaptureStreamSummary> {
        if self.process_states.is_empty()
            || !self
                .process_states
                .get(&root)
                .is_some_and(|state| state.finished)
            || self.process_states.values().any(|state| !state.finished)
            || self.counts.process_started != self.counts.process_finished
            || self.counts.census != self.counts.denominators
            || self.root != Some(root)
        {
            anyhow::bail!("process capture stream has incomplete process lifecycles");
        }
        let footer = encode_footer(
            self.identity,
            root,
            self.records,
            self.counts.process_started,
            self.payload_bytes,
            self.counts.census,
        );
        self.write_hashed(&footer)?;
        let committed_len = self.stream_bytes;
        let mut final_hasher = self.hasher.clone();
        final_hasher.update(&committed_len.to_le_bytes());
        let digest = *final_hasher.finalize().as_bytes();
        let trailer = encode_trailer(committed_len, digest);
        self.write_unhashed(&trailer)?;
        self.file.flush().context("flush process capture stream")?;
        self.file
            .get_ref()
            .sync_all()
            .context("sync process capture stream")?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        File::open(parent)
            .context("open process capture stream parent for sync")?
            .sync_all()
            .context("sync process capture stream parent")?;
        Ok(ProcessCaptureStreamSummary {
            identity: self.identity,
            digest,
            record_count: self.records,
            process_count: self.counts.process_started,
            payload_bytes: self.payload_bytes,
            stream_bytes: self.stream_bytes,
            census: self.counts.census,
        })
    }

    fn write_hashed(&mut self, bytes: &[u8]) -> Result<()> {
        self.write_unhashed(bytes)?;
        self.hasher.update(bytes);
        Ok(())
    }

    fn write_unhashed(&mut self, bytes: &[u8]) -> Result<()> {
        let byte_count = u64::try_from(bytes.len()).context("stream write size exceeds u64")?;
        let next = self
            .stream_bytes
            .checked_add(byte_count)
            .context("process capture stream byte count overflowed")?;
        if next > self.limits.max_stream_bytes {
            anyhow::bail!("process capture stream exceeds its byte budget");
        }
        self.file
            .write_all(bytes)
            .context("append process capture stream")?;
        self.stream_bytes = next;
        Ok(())
    }

    fn validate_lifecycle(&self, event: &ProcessCaptureEvent) -> Result<()> {
        match event {
            ProcessCaptureEvent::ProcessStarted { process, parent } => {
                if *process == 0
                    || parent == &Some(*process)
                    || self.process_states.contains_key(process)
                    || parent.is_some_and(|parent| {
                        self.process_states
                            .get(&parent)
                            .is_none_or(|state| state.finished)
                    })
                    || (parent.is_none() && self.root.is_some())
                {
                    anyhow::bail!("process capture stream has an invalid process start");
                }
                let next = u64::try_from(self.process_states.len())
                    .context("process count exceeds u64")?
                    .checked_add(1)
                    .context("process count overflowed")?;
                if next > self.limits.max_processes {
                    anyhow::bail!("process capture stream exceeds its process budget");
                }
            }
            event => {
                let process = event.process();
                match self.process_states.get(&process) {
                    Some(state) if !state.finished => {
                        if let Some(syscall_index) = event.syscall_index() {
                            if state
                                .last_modeled_syscall
                                .is_some_and(|prior| prior >= syscall_index)
                            {
                                anyhow::bail!(
                                    "process capture stream has a reordered syscall attempt"
                                );
                            }
                        }
                        if let ProcessCaptureEvent::ProcessFinished { summary, .. } = event {
                            let expected = ProcessCaptureStreamCensus {
                                invocations: summary.invocations,
                                exec_attempts: summary.exec_syscalls,
                                open_attempts: summary.open_syscalls,
                                newfstatat_attempts: summary.newfstatat_syscalls,
                                fstat_attempts: summary.fstat_syscalls,
                                getrandom_attempts: summary.getrandom_syscalls,
                                runtime_observation_attempts: summary.runtime_observation_syscalls,
                                prlimit_attempts: summary.prlimit_syscalls,
                                close_attempts: summary.close_syscalls,
                                seek_attempts: summary.seek_syscalls,
                                file_advice_attempts: summary.file_advice_syscalls,
                                fcntl_attempts: summary.fcntl_syscalls,
                                ioctl_attempts: summary.ioctl_syscalls,
                                pipe_attempts: summary.pipe_syscalls,
                                epoll_create_attempts: summary.epoll_create_syscalls,
                                socket_create_attempts:
                                    summary.socket_create_syscalls,
                                socket_connect_attempts:
                                    summary.socket_connect_syscalls,
                                dup2_attempts: summary.dup2_syscalls,
                                signal_state_attempts: summary.signal_state_syscalls,
                                signal_deliveries: summary.signal_deliveries,
                                umask_attempts: summary.umask_syscalls,
                                credential_attempts: summary.credential_syscalls,
                                futex_wake_attempts: summary.futex_wake_syscalls,
                                process_creation_attempts: summary.process_creation_syscalls,
                                child_wait_attempts: summary.child_wait_syscalls,
                                process_identity_attempts: summary.process_identity_syscalls,
                                memory_state_attempts: summary.memory_state_syscalls,
                                thread_state_attempts: summary.thread_state_syscalls,
                                lifecycle_exit_attempts: summary.lifecycle_exit_syscalls,
                                statx_attempts: summary.statx_syscalls,
                                xattr_attempts: summary.xattr_syscalls,
                                readlink_attempts: summary.readlink_syscalls,
                                access_attempts: summary.access_syscalls,
                                getcwd_attempts: summary.getcwd_syscalls,
                                cwd_change_attempts: summary.cwd_change_syscalls,
                                path_removal_attempts: summary.path_removal_syscalls,
                                directory_creation_attempts: summary.directory_creation_syscalls,
                                path_rename_attempts: summary.path_rename_syscalls,
                                path_mode_change_attempts: summary.path_mode_change_syscalls,
                                descriptor_timestamp_change_attempts: summary
                                    .descriptor_timestamp_change_syscalls,
                                symbolic_link_creation_attempts: summary
                                    .symbolic_link_creation_syscalls,
                                filesystem_status_attempts: summary.filesystem_status_syscalls,
                                read_attempts: summary.read_syscalls,
                                getdents64_attempts: summary.getdents64_syscalls,
                                mmap_attempts: summary.mmap_syscalls,
                                write_attempts: summary.write_syscalls,
                                unmodeled_syscall_attempts: summary.unmodeled_syscalls,
                                syscall_entries: 0,
                                syscall_exits: 0,
                                terminal_syscalls: 0,
                            };
                            if state.census != expected
                                || expected.recorded_syscall_attempts()? != summary.syscall_entries
                                || summary.syscall_entries
                                    != summary
                                        .syscall_exits
                                        .checked_add(summary.terminal_syscalls)
                                        .context("process syscall census overflowed")?
                                || state
                                    .last_modeled_syscall
                                    .is_some_and(|index| index >= summary.syscall_entries)
                            {
                                anyhow::bail!(
                                    "process capture finish has an invalid per-process census: observed {:?}, expected {:?}",
                                    state.census,
                                    expected
                                );
                            }
                        }
                    }
                    Some(_) => anyhow::bail!("process capture event follows process finish"),
                    None => anyhow::bail!("process capture event precedes process start"),
                }
            }
        }
        Ok(())
    }

    fn commit_lifecycle(&mut self, event: &ProcessCaptureEvent) -> Result<()> {
        match event {
            ProcessCaptureEvent::ProcessStarted { process, .. } => {
                self.process_states
                    .insert(*process, StreamProcessState::default());
                if let ProcessCaptureEvent::ProcessStarted { parent: None, .. } = event {
                    self.root = Some(*process);
                }
            }
            ProcessCaptureEvent::ProcessFinished { process, summary } => {
                self.process_states
                    .get_mut(process)
                    .context("finished process was not started")?
                    .finished = true;
                self.counts.add_finish(*summary)?;
            }
            event => {
                let state = self
                    .process_states
                    .get_mut(&event.process())
                    .context("process event was not started")?;
                let counter = match event {
                    ProcessCaptureEvent::Invocation { .. } => &mut state.census.invocations,
                    ProcessCaptureEvent::ExecAttempt { .. } => &mut state.census.exec_attempts,
                    ProcessCaptureEvent::OpenAttempt { .. } => &mut state.census.open_attempts,
                    ProcessCaptureEvent::NewfstatatAttempt { .. } => {
                        &mut state.census.newfstatat_attempts
                    }
                    ProcessCaptureEvent::FstatAttempt { .. } => &mut state.census.fstat_attempts,
                    ProcessCaptureEvent::GetrandomAttempt { .. } => {
                        &mut state.census.getrandom_attempts
                    }
                    ProcessCaptureEvent::RuntimeObservationAttempt { .. } => {
                        &mut state.census.runtime_observation_attempts
                    }
                    ProcessCaptureEvent::PrlimitAttempt { .. } => {
                        &mut state.census.prlimit_attempts
                    }
                    ProcessCaptureEvent::CloseAttempt { .. } => &mut state.census.close_attempts,
                    ProcessCaptureEvent::SeekAttempt { .. } => &mut state.census.seek_attempts,
                    ProcessCaptureEvent::FileAdviceAttempt { .. } => {
                        &mut state.census.file_advice_attempts
                    }
                    ProcessCaptureEvent::FcntlAttempt { .. } => &mut state.census.fcntl_attempts,
                    ProcessCaptureEvent::IoctlAttempt { .. } => &mut state.census.ioctl_attempts,
                    ProcessCaptureEvent::PipeAttempt { .. } => &mut state.census.pipe_attempts,
                    ProcessCaptureEvent::EpollCreateAttempt { .. } => {
                        &mut state.census.epoll_create_attempts
                    }
                    ProcessCaptureEvent::SocketCreateAttempt { .. } => {
                        &mut state.census.socket_create_attempts
                    }
                    ProcessCaptureEvent::SocketConnectAttempt { .. } => {
                        &mut state.census.socket_connect_attempts
                    }
                    ProcessCaptureEvent::Dup2Attempt { .. } => &mut state.census.dup2_attempts,
                    ProcessCaptureEvent::SignalStateAttempt { .. } => {
                        &mut state.census.signal_state_attempts
                    }
                    ProcessCaptureEvent::SignalDelivery { .. } => {
                        &mut state.census.signal_deliveries
                    }
                    ProcessCaptureEvent::UmaskAttempt { .. } => &mut state.census.umask_attempts,
                    ProcessCaptureEvent::CredentialAttempt { .. } => {
                        &mut state.census.credential_attempts
                    }
                    ProcessCaptureEvent::FutexWakeAttempt { .. } => {
                        &mut state.census.futex_wake_attempts
                    }
                    ProcessCaptureEvent::ProcessCreationAttempt { .. } => {
                        &mut state.census.process_creation_attempts
                    }
                    ProcessCaptureEvent::ChildWaitAttempt { .. } => {
                        &mut state.census.child_wait_attempts
                    }
                    ProcessCaptureEvent::ProcessIdentityAttempt { .. } => {
                        &mut state.census.process_identity_attempts
                    }
                    ProcessCaptureEvent::MemoryStateAttempt { .. } => {
                        &mut state.census.memory_state_attempts
                    }
                    ProcessCaptureEvent::ThreadStateAttempt { .. } => {
                        &mut state.census.thread_state_attempts
                    }
                    ProcessCaptureEvent::LifecycleExitAttempt { .. } => {
                        &mut state.census.lifecycle_exit_attempts
                    }
                    ProcessCaptureEvent::StatxAttempt { .. } => &mut state.census.statx_attempts,
                    ProcessCaptureEvent::XattrAttempt { .. } => &mut state.census.xattr_attempts,
                    ProcessCaptureEvent::ReadlinkAttempt { .. } => {
                        &mut state.census.readlink_attempts
                    }
                    ProcessCaptureEvent::AccessAttempt { .. } => &mut state.census.access_attempts,
                    ProcessCaptureEvent::GetcwdAttempt { .. } => &mut state.census.getcwd_attempts,
                    ProcessCaptureEvent::CwdChangeAttempt { .. } => {
                        &mut state.census.cwd_change_attempts
                    }
                    ProcessCaptureEvent::PathRemovalAttempt { .. } => {
                        &mut state.census.path_removal_attempts
                    }
                    ProcessCaptureEvent::DirectoryCreationAttempt { .. } => {
                        &mut state.census.directory_creation_attempts
                    }
                    ProcessCaptureEvent::PathRenameAttempt { .. } => {
                        &mut state.census.path_rename_attempts
                    }
                    ProcessCaptureEvent::PathModeChangeAttempt { .. } => {
                        &mut state.census.path_mode_change_attempts
                    }
                    ProcessCaptureEvent::DescriptorTimestampChangeAttempt { .. } => {
                        &mut state.census.descriptor_timestamp_change_attempts
                    }
                    ProcessCaptureEvent::SymbolicLinkCreationAttempt { .. } => {
                        &mut state.census.symbolic_link_creation_attempts
                    }
                    ProcessCaptureEvent::FilesystemStatusAttempt { .. } => {
                        &mut state.census.filesystem_status_attempts
                    }
                    ProcessCaptureEvent::ReadAttempt { .. } => &mut state.census.read_attempts,
                    ProcessCaptureEvent::Getdents64Attempt { .. } => {
                        &mut state.census.getdents64_attempts
                    }
                    ProcessCaptureEvent::MmapAttempt { .. } => &mut state.census.mmap_attempts,
                    ProcessCaptureEvent::WriteAttempt { .. } => &mut state.census.write_attempts,
                    ProcessCaptureEvent::UnmodeledSyscallAttempt { .. } => {
                        &mut state.census.unmodeled_syscall_attempts
                    }
                    ProcessCaptureEvent::ProcessStarted { .. }
                    | ProcessCaptureEvent::ProcessFinished { .. } => unreachable!(),
                };
                *counter = counter
                    .checked_add(1)
                    .context("process capture event census overflowed")?;
                if let Some(syscall_index) = event.syscall_index() {
                    state.last_modeled_syscall = Some(syscall_index);
                }
            }
        }
        Ok(())
    }
}

impl ProcessCaptureSink for StreamCaptureSink {
    fn record(&mut self, event: ProcessCaptureEvent) -> Result<()> {
        self.validate_lifecycle(&event)?;
        if self.records >= self.limits.max_records {
            anyhow::bail!("process capture stream exceeds its record budget");
        }
        let (tag, payload) = encode_event(&event, self.limits.max_frame_bytes)?;
        let payload_bytes = u64::try_from(payload.len()).context("frame size exceeds u64")?;
        let frame_bytes = FRAME_PREFIX_BYTES
            .checked_add(payload_bytes)
            .and_then(|bytes| bytes.checked_add(FRAME_DIGEST_BYTES))
            .context("process capture stream frame size overflowed")?;
        let projected = self
            .stream_bytes
            .checked_add(frame_bytes)
            .and_then(|bytes| bytes.checked_add(FOOTER_BYTES))
            .and_then(|bytes| bytes.checked_add(TRAILER_BYTES))
            .context("process capture stream size overflowed")?;
        if projected > self.limits.max_stream_bytes {
            anyhow::bail!("process capture stream exceeds its byte budget");
        }
        let mut prefix = Vec::with_capacity(FRAME_PREFIX_BYTES as usize);
        prefix.push(tag);
        prefix.extend_from_slice(&[0; 3]);
        prefix.extend_from_slice(&FORMAT_RECORD_VERSION.to_le_bytes());
        prefix.extend_from_slice(&self.records.to_le_bytes());
        prefix.extend_from_slice(&payload_bytes.to_le_bytes());
        self.write_hashed(&prefix)?;
        self.write_hashed(&payload)?;
        self.write_hashed(blake3::hash(&payload).as_bytes())?;
        self.payload_bytes = self
            .payload_bytes
            .checked_add(payload_bytes)
            .context("process capture stream payload count overflowed")?;
        self.records = self
            .records
            .checked_add(1)
            .context("process capture stream record count overflowed")?;
        self.counts.record_tag(tag)?;
        self.commit_lifecycle(&event)?;
        Ok(())
    }

    fn max_event_bytes(&self) -> Option<u64> {
        Some(self.limits.max_frame_bytes)
    }
}

impl ProcessCaptureEvent {
    fn process(&self) -> CapturedProcessId {
        match self {
            Self::ProcessStarted { process, .. }
            | Self::Invocation { process, .. }
            | Self::ExecAttempt { process, .. }
            | Self::OpenAttempt { process, .. }
            | Self::NewfstatatAttempt { process, .. }
            | Self::FstatAttempt { process, .. }
            | Self::GetrandomAttempt { process, .. }
            | Self::RuntimeObservationAttempt { process, .. }
            | Self::PrlimitAttempt { process, .. }
            | Self::CloseAttempt { process, .. }
            | Self::SeekAttempt { process, .. }
            | Self::FileAdviceAttempt { process, .. }
            | Self::FcntlAttempt { process, .. }
            | Self::IoctlAttempt { process, .. }
            | Self::PipeAttempt { process, .. }
            | Self::EpollCreateAttempt { process, .. }
            | Self::SocketCreateAttempt { process, .. }
            | Self::SocketConnectAttempt { process, .. }
            | Self::Dup2Attempt { process, .. }
            | Self::SignalStateAttempt { process, .. }
            | Self::SignalDelivery { process, .. }
            | Self::UmaskAttempt { process, .. }
            | Self::CredentialAttempt { process, .. }
            | Self::FutexWakeAttempt { process, .. }
            | Self::ProcessCreationAttempt { process, .. }
            | Self::ChildWaitAttempt { process, .. }
            | Self::ProcessIdentityAttempt { process, .. }
            | Self::MemoryStateAttempt { process, .. }
            | Self::ThreadStateAttempt { process, .. }
            | Self::LifecycleExitAttempt { process, .. }
            | Self::StatxAttempt { process, .. }
            | Self::XattrAttempt { process, .. }
            | Self::ReadlinkAttempt { process, .. }
            | Self::AccessAttempt { process, .. }
            | Self::GetcwdAttempt { process, .. }
            | Self::CwdChangeAttempt { process, .. }
            | Self::PathRemovalAttempt { process, .. }
            | Self::DirectoryCreationAttempt { process, .. }
            | Self::PathRenameAttempt { process, .. }
            | Self::PathModeChangeAttempt { process, .. }
            | Self::DescriptorTimestampChangeAttempt { process, .. }
            | Self::SymbolicLinkCreationAttempt { process, .. }
            | Self::FilesystemStatusAttempt { process, .. }
            | Self::ReadAttempt { process, .. }
            | Self::Getdents64Attempt { process, .. }
            | Self::MmapAttempt { process, .. }
            | Self::WriteAttempt { process, .. }
            | Self::UnmodeledSyscallAttempt { process, .. }
            | Self::ProcessFinished { process, .. } => *process,
        }
    }

    fn syscall_index(&self) -> Option<u64> {
        match self {
            Self::ExecAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::OpenAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::NewfstatatAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::FstatAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::GetrandomAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::RuntimeObservationAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::PrlimitAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::CloseAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::SeekAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::FileAdviceAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::FcntlAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::IoctlAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::PipeAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::EpollCreateAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::SocketCreateAttempt { attempt, .. } => {
                Some(attempt.syscall_index)
            }
            Self::SocketConnectAttempt { attempt, .. } => {
                Some(attempt.syscall_index)
            }
            Self::Dup2Attempt { attempt, .. } => Some(attempt.syscall_index),
            Self::SignalStateAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::UmaskAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::CredentialAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::FutexWakeAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::ProcessCreationAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::ChildWaitAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::ProcessIdentityAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::MemoryStateAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::ThreadStateAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::LifecycleExitAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::StatxAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::XattrAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::ReadlinkAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::AccessAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::GetcwdAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::CwdChangeAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::PathRemovalAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::DirectoryCreationAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::PathRenameAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::PathModeChangeAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::DescriptorTimestampChangeAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::SymbolicLinkCreationAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::FilesystemStatusAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::ReadAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::Getdents64Attempt { attempt, .. } => Some(attempt.syscall_index),
            Self::MmapAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::WriteAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::UnmodeledSyscallAttempt { attempt, .. } => Some(attempt.syscall_index),
            Self::ProcessStarted { .. }
            | Self::Invocation { .. }
            | Self::SignalDelivery { .. }
            | Self::ProcessFinished { .. } => None,
        }
    }
}

/// Capture one process tree directly into a bounded private event stream.
///
/// The destination must not exist and its parent must be owner-private. An
/// error leaves an incomplete or indeterminate private stream for diagnosis;
/// without the returned trusted summary that path cannot be loaded as complete
/// and cannot be retried without explicit cleanup.
/// Completed event payloads are released after each append. The compatibility
/// [`crate::capture_process_tree`] API remains an intentionally in-memory sink.
#[cfg(target_os = "linux")]
pub fn capture_process_tree_to_path(
    command: &mut std::process::Command,
    path: &Path,
    identity: ProcessCaptureStreamIdentity,
    limits: ProcessCaptureStreamLimits,
) -> Result<ProcessCaptureStreamSummary> {
    let mut sink = StreamCaptureSink::create(path, identity, limits)?;
    let root = crate::linux::capture_process_tree_events(command, &mut sink)?;
    sink.finish(root, path)
}

/// Report unsupported process capture without creating a destination stream.
#[cfg(not(target_os = "linux"))]
pub fn capture_process_tree_to_path(
    _command: &mut std::process::Command,
    _path: &Path,
    _identity: ProcessCaptureStreamIdentity,
    _limits: ProcessCaptureStreamLimits,
) -> Result<ProcessCaptureStreamSummary> {
    anyhow::bail!("process capture is unsupported on this platform")
}

/// Check, reconstruct, and semantically validate a private capture stream.
///
/// The same explicit limits apply during both integrity-checking passes. Success
/// does not imply [`ProcessCaptureReceipt::require_witnessed_file_mappings`]; a
/// downstream private adapter must apply that and every other replay policy.
pub fn load_process_capture_stream(
    path: &Path,
    expected_summary: ProcessCaptureStreamSummary,
    limits: ProcessCaptureStreamLimits,
) -> Result<(ProcessCaptureReceipt, ProcessCaptureStreamSeal)> {
    let expected_identity = expected_summary.identity;
    let mut file = open_capture_stream(path, limits)?;
    let checked = scan_stream(&mut file, expected_identity, limits, false, None)?;
    file.seek(SeekFrom::Start(0))
        .context("rewind integrity-checked process capture stream")?;
    let mut builder = ReceiptBuilder::default();
    let decoded = scan_stream(
        &mut file,
        expected_identity,
        limits,
        true,
        Some(&mut builder),
    )?;
    if checked != decoded {
        anyhow::bail!("process capture stream changed while it was decoded");
    }
    if decoded.summary != expected_summary {
        anyhow::bail!("process capture stream differs from its trusted writer summary");
    }
    let receipt = builder.finish(decoded.root)?;
    validate_footer_census(&receipt, decoded.summary.census)?;
    Ok((
        receipt,
        ProcessCaptureStreamSeal {
            summary: decoded.summary,
        },
    ))
}

/// Visit a private capture stream without materializing a complete receipt.
///
/// The reader first verifies the entire stream without delivering events. A
/// wrong trusted summary therefore produces no callbacks. A second pass
/// decodes one bounded frame at a time, applies receipt-equivalent event
/// validation with compact per-process state, and then invokes `visitor`.
/// Success requires both passes to agree with the trusted writer summary.
///
/// The visitor is consumed and returned only on success. A callback error,
/// panic, final mismatch, or I/O error produces no visitor and no seal. Any
/// callback effects must remain task-owned and reversible; callbacks must not
/// publish, populate a CAS or cache, commit a generation, or authorize replay.
/// This function establishes raw-stream completeness, not witnessed file
/// values or replay eligibility.
pub fn visit_process_capture_stream<V: ProcessCaptureStreamVisitor>(
    path: &Path,
    expected_summary: ProcessCaptureStreamSummary,
    limits: ProcessCaptureStreamLimits,
    mut visitor: V,
) -> Result<(V, ProcessCaptureStreamSeal)> {
    let expected_identity = expected_summary.identity;
    let mut file = open_capture_stream(path, limits)?;
    let checked = scan_stream(&mut file, expected_identity, limits, false, None)?;
    if checked.summary != expected_summary {
        anyhow::bail!("process capture stream differs from its trusted writer summary");
    }
    file.seek(SeekFrom::Start(0))
        .context("rewind integrity-checked process capture stream")?;
    let mut sink = StreamVisitorSink::new(&mut visitor);
    let decoded = scan_stream(&mut file, expected_identity, limits, true, Some(&mut sink))?;
    if checked != decoded {
        anyhow::bail!("process capture stream changed while it was visited");
    }
    if decoded.summary != expected_summary {
        anyhow::bail!("process capture stream differs from its trusted writer summary");
    }
    sink.finish(decoded.root)?;
    drop(sink);
    Ok((
        visitor,
        ProcessCaptureStreamSeal {
            summary: decoded.summary,
        },
    ))
}

#[derive(Default)]
struct InputWitnessVisitor {
    file_mappings: u64,
    witnessed_file_mappings: u64,
    regular_file_reads: u64,
    witnessed_regular_file_reads: u64,
    proc_filesystems_reads: u64,
    proc_process_maps_reads: u64,
    proc_process_statm_reads: u64,
    proc_process_cgroup_reads: u64,
    cgroup_cpu_max_reads: u64,
    proc_vm_overcommit_memory_reads: u64,
    other_unwitnessed_regular_file_reads: u64,
    first_other_unwitnessed_regular_file_read: Option<Vec<u8>>,
    directory_enumerations: u64,
    witnessed_directory_enumerations: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnwitnessedRegularReadClass {
    ProcFilesystems,
    ProcProcessMaps,
    ProcProcessStatm,
    ProcProcessCgroup,
    CgroupCpuMax,
    ProcVmOvercommitMemory,
    Other,
}

fn proc_process_file_target(target: &[u8], name: &[u8]) -> bool {
    let Some(relative) = target.strip_prefix(b"/proc/") else {
        return false;
    };
    let Some(process) = relative.strip_suffix(name) else {
        return false;
    };
    !process.is_empty() && process.iter().all(u8::is_ascii_digit)
}

fn classify_unwitnessed_regular_read(target: &[u8]) -> UnwitnessedRegularReadClass {
    if target == b"/proc/filesystems" {
        UnwitnessedRegularReadClass::ProcFilesystems
    } else if proc_process_file_target(target, b"/maps") {
        UnwitnessedRegularReadClass::ProcProcessMaps
    } else if proc_process_file_target(target, b"/statm") {
        UnwitnessedRegularReadClass::ProcProcessStatm
    } else if proc_process_file_target(target, b"/cgroup") {
        UnwitnessedRegularReadClass::ProcProcessCgroup
    } else if cgroup_cpu_max_target(target) {
        UnwitnessedRegularReadClass::CgroupCpuMax
    } else if target == b"/proc/sys/vm/overcommit_memory" {
        UnwitnessedRegularReadClass::ProcVmOvercommitMemory
    } else {
        UnwitnessedRegularReadClass::Other
    }
}

fn cgroup_cpu_max_target(target: &[u8]) -> bool {
    let Some(relative) = target
        .strip_prefix(b"/sys/fs/cgroup/")
        .and_then(|relative| relative.strip_suffix(b"/cpu.max"))
    else {
        return false;
    };
    !relative.is_empty()
        && relative
            .split(|byte| *byte == b'/')
            .all(|component| !component.is_empty() && component != b"." && component != b"..")
}

impl InputWitnessVisitor {
    fn record_file_mapping(&mut self, value: CapturedMappedFileValue) -> Result<()> {
        self.file_mappings = self
            .file_mappings
            .checked_add(1)
            .context("process capture file-mapping census overflowed")?;
        if matches!(value, CapturedMappedFileValue::WholeFile { .. }) {
            self.witnessed_file_mappings = self
                .witnessed_file_mappings
                .checked_add(1)
                .context("process capture witnessed mapping census overflowed")?;
        }
        Ok(())
    }

    fn record_directory(&mut self, value: &CapturedDirectoryValue) -> Result<()> {
        self.directory_enumerations = self
            .directory_enumerations
            .checked_add(1)
            .context("process capture directory census overflowed")?;
        if matches!(value, CapturedDirectoryValue::Complete { .. }) {
            self.witnessed_directory_enumerations = self
                .witnessed_directory_enumerations
                .checked_add(1)
                .context("process capture witnessed directory census overflowed")?;
        }
        Ok(())
    }

    fn record_regular_file_read(
        &mut self,
        identity: Option<&CapturedDescriptorIdentity>,
        value: Option<CapturedMappedFileValue>,
    ) -> Result<()> {
        let Some(identity) =
            identity.filter(|identity| identity.kind == CapturedDescriptorKind::RegularFile)
        else {
            return Ok(());
        };
        self.regular_file_reads = self
            .regular_file_reads
            .checked_add(1)
            .context("process capture regular-file read census overflowed")?;
        if matches!(value, Some(CapturedMappedFileValue::WholeFile { .. })) {
            self.witnessed_regular_file_reads = self
                .witnessed_regular_file_reads
                .checked_add(1)
                .context("process capture witnessed read census overflowed")?;
        } else {
            let counter = match classify_unwitnessed_regular_read(&identity.target) {
                UnwitnessedRegularReadClass::ProcFilesystems => &mut self.proc_filesystems_reads,
                UnwitnessedRegularReadClass::ProcProcessMaps => &mut self.proc_process_maps_reads,
                UnwitnessedRegularReadClass::ProcProcessStatm => &mut self.proc_process_statm_reads,
                UnwitnessedRegularReadClass::ProcProcessCgroup => {
                    &mut self.proc_process_cgroup_reads
                }
                UnwitnessedRegularReadClass::CgroupCpuMax => &mut self.cgroup_cpu_max_reads,
                UnwitnessedRegularReadClass::ProcVmOvercommitMemory => {
                    &mut self.proc_vm_overcommit_memory_reads
                }
                UnwitnessedRegularReadClass::Other => {
                    self.first_other_unwitnessed_regular_file_read
                        .get_or_insert_with(|| identity.target.clone());
                    &mut self.other_unwitnessed_regular_file_reads
                }
            };
            *counter = counter
                .checked_add(1)
                .context("process capture unwitnessed read-class census overflowed")?;
        }
        Ok(())
    }

    fn finish(self) -> ProcessCaptureInputWitnessCensus {
        ProcessCaptureInputWitnessCensus {
            file_mappings: self.file_mappings,
            witnessed_file_mappings: self.witnessed_file_mappings,
            regular_file_reads: self.regular_file_reads,
            witnessed_regular_file_reads: self.witnessed_regular_file_reads,
            proc_filesystems_reads: self.proc_filesystems_reads,
            proc_process_maps_reads: self.proc_process_maps_reads,
            proc_process_statm_reads: self.proc_process_statm_reads,
            proc_process_cgroup_reads: self.proc_process_cgroup_reads,
            cgroup_cpu_max_reads: self.cgroup_cpu_max_reads,
            proc_vm_overcommit_memory_reads: self.proc_vm_overcommit_memory_reads,
            other_unwitnessed_regular_file_reads: self.other_unwitnessed_regular_file_reads,
            first_other_unwitnessed_regular_file_read: self
                .first_other_unwitnessed_regular_file_read,
            directory_enumerations: self.directory_enumerations,
            witnessed_directory_enumerations: self.witnessed_directory_enumerations,
        }
    }
}

impl ProcessCaptureStreamVisitor for InputWitnessVisitor {
    fn visit(&mut self, event: ProcessCaptureStreamEvent<'_>) -> Result<()> {
        match event {
            ProcessCaptureStreamEvent::Invocation { invocation, .. } => {
                for mapping in &invocation.file_mappings {
                    self.record_file_mapping(mapping.value)?;
                }
            }
            ProcessCaptureStreamEvent::MmapAttempt { attempt, .. } => {
                if let CapturedMmapOutcome::Mapped {
                    file_value: Some(value),
                    ..
                } = attempt.outcome
                {
                    self.record_file_mapping(value)?;
                }
            }
            ProcessCaptureStreamEvent::ReadAttempt { attempt, .. } => {
                if let CapturedReadOutcome::Read { file_value, .. } = attempt.outcome {
                    self.record_regular_file_read(attempt.request.identity.as_ref(), file_value)?;
                }
            }
            ProcessCaptureStreamEvent::Getdents64Attempt { attempt, .. } => {
                if let CapturedGetdents64Outcome::Enumerated {
                    directory_value, ..
                } = &attempt.outcome
                {
                    self.record_directory(directory_value)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Verify a private stream and require mapped-file, regular-read, and directory values.
///
/// The stream is checked without materializing a complete in-memory receipt.
/// Success certifies only these three input-witness families; callers must still
/// reject or model every other observable input, effect, and volatility source.
pub fn certify_process_capture_input_witnesses(
    path: &Path,
    expected_summary: ProcessCaptureStreamSummary,
    limits: ProcessCaptureStreamLimits,
) -> Result<ProcessCaptureInputWitnessSeal> {
    let (census, stream) = census_process_capture_input_witnesses(path, expected_summary, limits)?;
    census.require_complete()?;
    Ok(ProcessCaptureInputWitnessSeal { stream, census })
}

/// Census private-stream input witnesses without authorizing incomplete values.
///
/// Unlike [`certify_process_capture_input_witnesses`], this diagnostic returns
/// recognized gaps so a downstream owner can partition and reject or model
/// them. The returned stream seal proves integrity, not replay eligibility.
pub fn census_process_capture_input_witnesses(
    path: &Path,
    expected_summary: ProcessCaptureStreamSummary,
    limits: ProcessCaptureStreamLimits,
) -> Result<(ProcessCaptureInputWitnessCensus, ProcessCaptureStreamSeal)> {
    let (visitor, stream) = visit_process_capture_stream(
        path,
        expected_summary,
        limits,
        InputWitnessVisitor::default(),
    )?;
    Ok((visitor.finish(), stream))
}

/// Write a synthetic stream without filesystem events for downstream tests.
///
/// This API exists only with the `test-fixtures` feature. Each supplied
/// invocation belongs to one complete process lifecycle. The first is its
/// initial image and every later invocation is paired with one synthetic
/// successful exec. An invocation without file mappings receives one synthetic
/// witnessed executable mapping.
#[cfg(feature = "test-fixtures")]
pub fn write_process_capture_stream_test_fixture(
    path: &Path,
    identity: ProcessCaptureStreamIdentity,
    limits: ProcessCaptureStreamLimits,
    invocations: &[CapturedInvocation],
) -> Result<ProcessCaptureStreamSummary> {
    if invocations.is_empty() {
        anyhow::bail!("process capture test fixture has no invocations");
    }
    write_process_capture_stream_tree_test_fixture(
        path,
        identity,
        limits,
        &[ProcessCaptureStreamTestProcess {
            process: 1,
            parent: None,
            invocations: invocations.to_vec(),
            outcome: CapturedProcessOutcome::Exited(0),
        }],
    )
}

/// One process in a synthetic downstream process-stream fixture.
///
/// This type exists only with the `test-fixtures` feature. Processes must be
/// supplied in parent-before-child order. A root invocation represents the
/// captured initial image; every non-root invocation represents a successful
/// exec. A fork-only helper can therefore have no invocations.
#[cfg(feature = "test-fixtures")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessCaptureStreamTestProcess {
    /// Nonzero receipt-local process identity.
    pub process: CapturedProcessId,
    /// Earlier receipt-local parent, absent only for the single root.
    pub parent: Option<CapturedProcessId>,
    /// Successful invocation boundaries in process-local order.
    pub invocations: Vec<CapturedInvocation>,
    /// Synthetic terminal state for the process.
    pub outcome: CapturedProcessOutcome,
}

/// One successful regular-file write in a synthetic downstream stream fixture.
///
/// This type exists only with the `test-fixtures` feature. The target must be
/// an absolute, lossless procfs-style descriptor target.
#[cfg(feature = "test-fixtures")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessCaptureStreamTestWrite {
    /// Process that owns the synthetic write attempt.
    pub process: CapturedProcessId,
    /// Entry-time regular-file descriptor target.
    pub target: Vec<u8>,
    /// Complete bytes written and observed at the selected final boundary.
    pub bytes: Vec<u8>,
    /// Complete native regular-file type and permission mode.
    pub mode: u32,
    /// Descriptor device identity shared by the write and final value.
    pub device: u64,
    /// Nonzero descriptor inode shared by the write and final value.
    pub inode: u64,
    /// Nonzero descriptor mount identity shared by the write and final value.
    pub mount_id: u64,
}

/// One created-then-removed symbolic link in a synthetic stream fixture.
///
/// This type exists only with the `test-fixtures` feature. The link path must
/// be absolute and absent when a downstream consumer validates final state.
#[cfg(feature = "test-fixtures")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessCaptureStreamTestTransientSymlink {
    /// Process that owns both successful mutation attempts.
    pub process: CapturedProcessId,
    /// Absolute new-link pathname.
    pub link_path: Vec<u8>,
}

/// Boundary carrying synthetic final values for downstream stream fixtures.
///
/// This type exists only with the `test-fixtures` feature.
#[cfg(feature = "test-fixtures")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessCaptureStreamTestFinalValueBoundary {
    /// Emit one successful close with a value after each supplied write.
    Close,
    /// Emit written values on the owning process's lifecycle-exit attempt.
    LifecycleExit,
}

/// One successful unwitnessed regular-file read in a synthetic stream fixture.
///
/// This type exists only with the `test-fixtures` feature. The target must be
/// an absolute, lossless procfs-style descriptor target.
#[cfg(feature = "test-fixtures")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessCaptureStreamTestRead {
    /// Process that owns the synthetic read attempt.
    pub process: CapturedProcessId,
    /// Entry-time regular-file descriptor target.
    pub target: Vec<u8>,
    /// Exact bytes returned to the synthetic tracee.
    pub bytes: Vec<u8>,
    /// Emit an immediately following EOF read on the same descriptor/object.
    pub complete_with_eof: bool,
}

/// Write a synthetic process tree without filesystem events for downstream tests.
///
/// This API exists only with the `test-fixtures` feature. It validates a
/// single parent-before-child tree, then writes ordinary stream records and an
/// integrity footer through the production stream sink. It does not bypass
/// stream budgets or semantic validation, and it carries no replay authority.
#[cfg(feature = "test-fixtures")]
pub fn write_process_capture_stream_tree_test_fixture(
    path: &Path,
    identity: ProcessCaptureStreamIdentity,
    limits: ProcessCaptureStreamLimits,
    processes: &[ProcessCaptureStreamTestProcess],
) -> Result<ProcessCaptureStreamSummary> {
    write_process_capture_stream_tree_fixture(
        path,
        identity,
        limits,
        processes,
        &[],
        &[],
        &[],
        None,
        false,
        None,
    )
}

/// Write a synthetic process tree with successful regular-file writes.
///
/// This API exists only with the `test-fixtures` feature. It has the same
/// integrity and lifecycle checks as [`write_process_capture_stream_tree_test_fixture`]
/// and adds the supplied write attempts in per-process list order.
#[cfg(feature = "test-fixtures")]
pub fn write_process_capture_stream_tree_with_writes_test_fixture(
    path: &Path,
    identity: ProcessCaptureStreamIdentity,
    limits: ProcessCaptureStreamLimits,
    processes: &[ProcessCaptureStreamTestProcess],
    writes: &[ProcessCaptureStreamTestWrite],
) -> Result<ProcessCaptureStreamSummary> {
    write_process_capture_stream_tree_fixture(
        path,
        identity,
        limits,
        processes,
        writes,
        &[],
        &[],
        None,
        false,
        None,
    )
}

/// Write a synthetic process tree with regular-file reads and writes.
///
/// This API exists only with the `test-fixtures` feature. Reads intentionally
/// carry no complete-file value witness so downstream volatility diagnostics
/// can exercise their fail-closed partitions.
#[cfg(feature = "test-fixtures")]
pub fn write_process_capture_stream_tree_with_reads_and_writes_test_fixture(
    path: &Path,
    identity: ProcessCaptureStreamIdentity,
    limits: ProcessCaptureStreamLimits,
    processes: &[ProcessCaptureStreamTestProcess],
    reads: &[ProcessCaptureStreamTestRead],
    writes: &[ProcessCaptureStreamTestWrite],
) -> Result<ProcessCaptureStreamSummary> {
    write_process_capture_stream_tree_fixture(
        path, identity, limits, processes, writes, reads, &[], None, false, None,
    )
}

/// Write a synthetic process tree with writes and successful lifecycle exits.
///
/// This API exists only with the `test-fixtures` feature. Every supplied
/// process must exit normally. Its `exit` or `exit_group` record is counted as
/// a modeled syscall completed by task termination, matching native capture.
#[cfg(feature = "test-fixtures")]
pub fn write_process_capture_stream_tree_with_writes_and_lifecycle_exits_test_fixture(
    path: &Path,
    identity: ProcessCaptureStreamIdentity,
    limits: ProcessCaptureStreamLimits,
    processes: &[ProcessCaptureStreamTestProcess],
    writes: &[ProcessCaptureStreamTestWrite],
) -> Result<ProcessCaptureStreamSummary> {
    write_process_capture_stream_tree_fixture(
        path,
        identity,
        limits,
        processes,
        writes,
        &[],
        &[],
        None,
        true,
        None,
    )
}

/// Write a synthetic process tree with reads, writes, and final file values.
///
/// This API exists only with the `test-fixtures` feature. Each write receives
/// a stable regular-file value at the selected later boundary. Lifecycle-exit
/// boundaries require every supplied process to exit normally.
#[cfg(feature = "test-fixtures")]
pub fn write_process_capture_stream_tree_with_final_values_test_fixture(
    path: &Path,
    identity: ProcessCaptureStreamIdentity,
    limits: ProcessCaptureStreamLimits,
    processes: &[ProcessCaptureStreamTestProcess],
    reads: &[ProcessCaptureStreamTestRead],
    writes: &[ProcessCaptureStreamTestWrite],
    boundary: ProcessCaptureStreamTestFinalValueBoundary,
) -> Result<ProcessCaptureStreamSummary> {
    write_process_capture_stream_tree_fixture(
        path,
        identity,
        limits,
        processes,
        writes,
        reads,
        &[],
        None,
        boundary == ProcessCaptureStreamTestFinalValueBoundary::LifecycleExit,
        Some(boundary),
    )
}

/// Write a synthetic process tree with final values and transient symlinks.
///
/// This API exists only with the `test-fixtures` feature. Each transient path
/// receives a successful symbolic-link creation followed by a successful
/// removal owned by the same process.
#[cfg(feature = "test-fixtures")]
pub fn write_process_capture_stream_tree_with_final_values_and_transient_symlinks_test_fixture(
    path: &Path,
    identity: ProcessCaptureStreamIdentity,
    limits: ProcessCaptureStreamLimits,
    processes: &[ProcessCaptureStreamTestProcess],
    reads: &[ProcessCaptureStreamTestRead],
    writes: &[ProcessCaptureStreamTestWrite],
    transient_symlinks: &[ProcessCaptureStreamTestTransientSymlink],
    boundary: ProcessCaptureStreamTestFinalValueBoundary,
) -> Result<ProcessCaptureStreamSummary> {
    write_process_capture_stream_tree_fixture(
        path,
        identity,
        limits,
        processes,
        writes,
        reads,
        transient_symlinks,
        None,
        boundary == ProcessCaptureStreamTestFinalValueBoundary::LifecycleExit,
        Some(boundary),
    )
}

/// Write a synthetic process tree with one unmodeled terminal syscall.
///
/// This API exists only with the `test-fixtures` feature. It is a negative-test
/// fixture for downstream consumers that must reject an exact unmodeled
/// syscall attempt, including one resolved only by task termination.
#[cfg(feature = "test-fixtures")]
pub fn write_process_capture_stream_unmodeled_terminal_test_fixture(
    path: &Path,
    identity: ProcessCaptureStreamIdentity,
    limits: ProcessCaptureStreamLimits,
    processes: &[ProcessCaptureStreamTestProcess],
    terminal_process: CapturedProcessId,
) -> Result<ProcessCaptureStreamSummary> {
    write_process_capture_stream_tree_fixture(
        path,
        identity,
        limits,
        processes,
        &[],
        &[],
        &[],
        Some((terminal_process, FixtureSyscall::Unknown { terminal: true })),
        false,
        None,
    )
}

/// Write a synthetic process tree with one unmodeled completed syscall.
///
/// This API exists only with the `test-fixtures` feature. It is a negative-test
/// fixture for downstream consumers that must reject an exact returned syscall
/// attempt until it has a supported semantic model.
#[cfg(feature = "test-fixtures")]
pub fn write_process_capture_stream_unmodeled_completed_test_fixture(
    path: &Path,
    identity: ProcessCaptureStreamIdentity,
    limits: ProcessCaptureStreamLimits,
    processes: &[ProcessCaptureStreamTestProcess],
    unmodeled_process: CapturedProcessId,
) -> Result<ProcessCaptureStreamSummary> {
    write_process_capture_stream_tree_fixture(
        path,
        identity,
        limits,
        processes,
        &[],
        &[],
        &[],
        Some((unmodeled_process, FixtureSyscall::Unknown { terminal: false })),
        false,
        None,
    )
}

/// Write a synthetic process tree with one typed open terminated in flight.
///
/// This negative fixture tests semantic rejection independently of unknown
/// syscall-number rejection. It exists only with the `test-fixtures` feature.
#[cfg(feature = "test-fixtures")]
pub fn write_process_capture_stream_terminal_open_test_fixture(
    path: &Path,
    identity: ProcessCaptureStreamIdentity,
    limits: ProcessCaptureStreamLimits,
    processes: &[ProcessCaptureStreamTestProcess],
    terminal_process: CapturedProcessId,
) -> Result<ProcessCaptureStreamSummary> {
    write_process_capture_stream_tree_fixture(
        path,
        identity,
        limits,
        processes,
        &[],
        &[],
        &[],
        Some((terminal_process, FixtureSyscall::TerminalOpen)),
        false,
        None,
    )
}

#[cfg(feature = "test-fixtures")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum FixtureSyscall {
    Unknown { terminal: bool },
    TerminalOpen,
}

#[cfg(feature = "test-fixtures")]
fn write_process_capture_stream_tree_fixture(
    path: &Path,
    identity: ProcessCaptureStreamIdentity,
    limits: ProcessCaptureStreamLimits,
    processes: &[ProcessCaptureStreamTestProcess],
    writes: &[ProcessCaptureStreamTestWrite],
    reads: &[ProcessCaptureStreamTestRead],
    transient_symlinks: &[ProcessCaptureStreamTestTransientSymlink],
    extra_syscall: Option<(CapturedProcessId, FixtureSyscall)>,
    model_lifecycle_exits: bool,
    final_value_boundary: Option<ProcessCaptureStreamTestFinalValueBoundary>,
) -> Result<ProcessCaptureStreamSummary> {
    let mut parents = BTreeMap::new();
    let mut root = None;
    for process in processes {
        if process.process == 0
            || process.parent == Some(process.process)
            || parents.contains_key(&process.process)
            || process
                .parent
                .is_some_and(|parent| !parents.contains_key(&parent))
        {
            anyhow::bail!("process capture test fixture has an invalid process tree");
        }
        if process.parent.is_none() {
            if root.replace(process.process).is_some() {
                anyhow::bail!("process capture test fixture has multiple roots");
            }
            if process.invocations.is_empty() {
                anyhow::bail!("process capture test fixture root has no invocation");
            }
        }
        parents.insert(process.process, process.parent);
    }
    let root = root.context("process capture test fixture has no root")?;
    if extra_syscall.is_some_and(|(process, _)| !parents.contains_key(&process)) {
        anyhow::bail!("process capture test fixture extra-syscall process is absent");
    }
    if writes.iter().any(|write| {
        !parents.contains_key(&write.process)
            || write.target.first() != Some(&b'/')
            || write.target.contains(&0)
            || write.bytes.is_empty()
            || write.mode & libc::S_IFMT != libc::S_IFREG
            || write.inode == 0
            || write.mount_id == 0
    }) {
        anyhow::bail!("process capture test fixture write is invalid");
    }
    if reads.iter().any(|read| {
        !parents.contains_key(&read.process)
            || read.target.first() != Some(&b'/')
            || read.target.contains(&0)
    }) {
        anyhow::bail!("process capture test fixture read is invalid");
    }
    if transient_symlinks.iter().any(|link| {
        !parents.contains_key(&link.process)
            || link.link_path.first() != Some(&b'/')
            || link.link_path.contains(&0)
    }) {
        anyhow::bail!("process capture test fixture transient symlink is invalid");
    }

    let finish = |invocation_count: u64,
                  exec_count: u64,
                  read_count: u64,
                  write_count: u64,
                  close_count: u64,
                  transient_count: u64,
                  extra_syscall: Option<FixtureSyscall>,
                  lifecycle_exit_count: u64,
                  outcome: CapturedProcessOutcome|
     -> Result<ProcessFinish> {
        let unmodeled_count = u64::from(matches!(
            extra_syscall, Some(FixtureSyscall::Unknown { .. })
        ));
        let open_count = u64::from(extra_syscall == Some(FixtureSyscall::TerminalOpen));
        let terminal_count = u64::from(matches!(
            extra_syscall,
            Some(FixtureSyscall::Unknown { terminal: true } | FixtureSyscall::TerminalOpen)
        ))
            .checked_add(lifecycle_exit_count)
            .context("process capture test-fixture terminal count overflowed")?;
        let completed_count = u64::from(
            extra_syscall == Some(FixtureSyscall::Unknown { terminal: false })
        );
        let syscall_entries = exec_count
            .checked_add(read_count)
            .context("process capture test-fixture syscall count overflowed")?
            .checked_add(write_count)
            .context("process capture test-fixture syscall count overflowed")?
            .checked_add(close_count)
            .context("process capture test-fixture syscall count overflowed")?
            .checked_add(
                transient_count
                    .checked_mul(2)
                    .context("process capture test-fixture syscall count overflowed")?,
            )
            .context("process capture test-fixture syscall count overflowed")?
            .checked_add(unmodeled_count + open_count)
            .and_then(|count| count.checked_add(lifecycle_exit_count))
            .context("process capture test-fixture syscall count overflowed")?;
        let syscall_exits = exec_count
            .checked_add(read_count)
            .context("process capture test-fixture syscall count overflowed")?
            .checked_add(write_count)
            .context("process capture test-fixture syscall count overflowed")?
            .checked_add(close_count)
            .context("process capture test-fixture syscall count overflowed")?
            .checked_add(
                transient_count
                    .checked_mul(2)
                    .context("process capture test-fixture syscall count overflowed")?,
            )
            .context("process capture test-fixture syscall count overflowed")?
            .checked_add(completed_count)
            .context("process capture test-fixture syscall count overflowed")?;
        Ok(ProcessFinish {
            invocations: invocation_count,
            exec_syscalls: exec_count,
            open_syscalls: open_count,
            newfstatat_syscalls: 0,
            fstat_syscalls: 0,
            getrandom_syscalls: 0,
            runtime_observation_syscalls: 0,
            prlimit_syscalls: 0,
            close_syscalls: close_count,
            seek_syscalls: 0,
            file_advice_syscalls: 0,
            fcntl_syscalls: 0,
            ioctl_syscalls: 0,
            pipe_syscalls: 0,
            epoll_create_syscalls: 0,
            socket_create_syscalls: 0,
            socket_connect_syscalls: 0,
            dup2_syscalls: 0,
            signal_state_syscalls: 0,
            signal_deliveries: 0,
            umask_syscalls: 0,
            credential_syscalls: 0,
            futex_wake_syscalls: 0,
            process_creation_syscalls: 0,
            child_wait_syscalls: 0,
            process_identity_syscalls: 0,
            memory_state_syscalls: 0,
            thread_state_syscalls: 0,
            lifecycle_exit_syscalls: lifecycle_exit_count,
            statx_syscalls: 0,
            xattr_syscalls: 0,
            readlink_syscalls: 0,
            access_syscalls: 0,
            getcwd_syscalls: 0,
            cwd_change_syscalls: 0,
            path_removal_syscalls: transient_count,
            directory_creation_syscalls: 0,
            path_rename_syscalls: 0,
            path_mode_change_syscalls: 0,
            descriptor_timestamp_change_syscalls: 0,
            symbolic_link_creation_syscalls: transient_count,
            filesystem_status_syscalls: 0,
            read_syscalls: read_count,
            getdents64_syscalls: 0,
            mmap_syscalls: 0,
            write_syscalls: write_count,
            unmodeled_syscalls: unmodeled_count,
            syscall_entries,
            syscall_exits,
            terminal_syscalls: terminal_count,
            outcome,
        })
    };
    let mut sink = StreamCaptureSink::create(path, identity, limits)?;
    for process in processes {
        sink.record(ProcessCaptureEvent::ProcessStarted {
            process: process.process,
            parent: process.parent,
        })?;
    }
    let complete_invocation = |invocation: &CapturedInvocation| {
        let mut invocation = invocation.clone();
        if invocation.file_mappings.is_empty() {
            invocation
                .file_mappings
                .push(crate::CapturedExecFileMapping {
                    start: 0x1000,
                    end: 0x2000,
                    file_offset: 0,
                    readable: true,
                    writable: false,
                    executable: true,
                    shared: false,
                    identity: crate::CapturedMappedFileIdentity {
                        target: invocation.executable.clone(),
                        device: 1,
                        inode: 1,
                        mount_id: 1,
                    },
                    value: CapturedMappedFileValue::WholeFile {
                        bytes: 0,
                        digest: *blake3::hash(&[]).as_bytes(),
                        mode: libc::S_IFREG | 0o755,
                    },
                });
        }
        invocation
    };
    for process in processes {
        let first_loaded = usize::from(process.parent.is_none());
        for (index, invocation) in process.invocations.iter().enumerate() {
            sink.record(ProcessCaptureEvent::Invocation {
                process: process.process,
                invocation: complete_invocation(invocation),
            })?;
            if index >= first_loaded {
                let loaded = u64::try_from(index)
                    .context("process capture test-fixture invocation overflowed")?;
                let syscall_index = loaded
                    .checked_sub(u64::from(process.parent.is_none()))
                    .context("process capture test-fixture exec index underflowed")?;
                sink.record(ProcessCaptureEvent::ExecAttempt {
                    process: process.process,
                    attempt: crate::CapturedExecAttempt {
                        syscall_index,
                        request: crate::CapturedExecRequest {
                            executable: invocation.executable.clone(),
                            directory: None,
                            argv: invocation.argv.clone(),
                            environment: invocation.environment.clone(),
                            flags: 0,
                        },
                        outcome: crate::CapturedExecOutcome::Loaded(loaded),
                    },
                })?;
            }
        }
        let invocation_count = u64::try_from(process.invocations.len())
            .context("process capture test-fixture invocation count overflowed")?;
        let exec_count = invocation_count
            .checked_sub(u64::from(process.parent.is_none()))
            .context("process capture test-fixture exec count underflowed")?;
        let process_reads = reads
            .iter()
            .enumerate()
            .filter(|(_, read)| read.process == process.process)
            .collect::<Vec<_>>();
        let mut read_count = 0_u64;
        for (fixture_index, read) in &process_reads {
            let syscall_index = exec_count
                .checked_add(read_count)
                .context("process capture test-fixture read index overflowed")?;
            let inode = u64::try_from(*fixture_index)
                .context("process capture test-fixture read identity overflowed")?
                .checked_add(1)
                .context("process capture test-fixture read identity overflowed")?;
            let bytes = u64::try_from(read.bytes.len())
                .context("process capture test-fixture read length does not fit u64")?;
            sink.record(ProcessCaptureEvent::ReadAttempt {
                process: process.process,
                attempt: crate::CapturedReadAttempt {
                    syscall_index,
                    request: crate::CapturedReadRequest {
                        system_call: crate::CapturedReadSystemCall::Read,
                        descriptor: 3,
                        identity: Some(crate::CapturedDescriptorIdentity {
                            target: read.target.clone(),
                            device: 1,
                            inode,
                            kind: crate::CapturedDescriptorKind::RegularFile,
                        }),
                        mount_id: Some(1),
                        uses_descriptor_offset: true,
                        offset: Some(0),
                        buffer_lengths: vec![bytes],
                        requested_bytes: bytes,
                        flags: 0,
                    },
                    outcome: crate::CapturedReadOutcome::Read {
                        bytes,
                        digest: *blake3::hash(&read.bytes).as_bytes(),
                        file_value: None,
                    },
                },
            })?;
            read_count = read_count
                .checked_add(1)
                .context("process capture test-fixture read count overflowed")?;
            if read.complete_with_eof {
                let requested_bytes = bytes.max(1);
                sink.record(ProcessCaptureEvent::ReadAttempt {
                    process: process.process,
                    attempt: crate::CapturedReadAttempt {
                        syscall_index: exec_count
                            .checked_add(read_count)
                            .context("process capture test-fixture EOF index overflowed")?,
                        request: crate::CapturedReadRequest {
                            system_call: crate::CapturedReadSystemCall::Read,
                            descriptor: 3,
                            identity: Some(crate::CapturedDescriptorIdentity {
                                target: read.target.clone(),
                                device: 1,
                                inode,
                                kind: crate::CapturedDescriptorKind::RegularFile,
                            }),
                            mount_id: Some(1),
                            uses_descriptor_offset: true,
                            offset: Some(bytes),
                            buffer_lengths: vec![requested_bytes],
                            requested_bytes,
                            flags: 0,
                        },
                        outcome: crate::CapturedReadOutcome::Read {
                            bytes: 0,
                            digest: *blake3::hash(&[]).as_bytes(),
                            file_value: None,
                        },
                    },
                })?;
                read_count = read_count
                    .checked_add(1)
                    .context("process capture test-fixture read count overflowed")?;
            }
        }
        let process_writes = writes
            .iter()
            .enumerate()
            .filter(|(_, write)| write.process == process.process)
            .collect::<Vec<_>>();
        for (process_index, (fixture_index, write)) in process_writes.iter().enumerate() {
            let write_index = u64::try_from(process_index)
                .context("process capture test-fixture write index overflowed")?;
            let syscall_index = exec_count
                .checked_add(read_count)
                .context("process capture test-fixture write index overflowed")?
                .checked_add(write_index)
                .context("process capture test-fixture write index overflowed")?;
            let descriptor = if final_value_boundary.is_some() {
                i32::try_from(*fixture_index)
                    .context("process capture test-fixture descriptor does not fit i32")?
                    .checked_add(3)
                    .context("process capture test-fixture descriptor overflowed")?
            } else {
                3
            };
            sink.record(ProcessCaptureEvent::WriteAttempt {
                process: process.process,
                attempt: crate::CapturedWriteAttempt {
                    syscall_index,
                    request: crate::CapturedWriteRequest {
                        system_call: crate::CapturedWriteSystemCall::Write,
                        descriptor,
                        identity: Some(crate::CapturedDescriptorIdentity {
                            target: write.target.clone(),
                            device: write.device,
                            inode: write.inode,
                            kind: crate::CapturedDescriptorKind::RegularFile,
                        }),
                        mount_id: Some(write.mount_id),
                        uses_descriptor_offset: true,
                        offset: Some(0),
                        requested_bytes: u64::try_from(write.bytes.len())
                            .context("process capture test-fixture write size overflowed")?,
                        append: false,
                    },
                    outcome: crate::CapturedWriteOutcome::Wrote {
                        bytes: u64::try_from(write.bytes.len())
                            .context("process capture test-fixture write size overflowed")?,
                        digest: *blake3::hash(&write.bytes).as_bytes(),
                    },
                },
            })?;
        }
        let write_count = u64::try_from(process_writes.len())
            .context("process capture test-fixture write count overflowed")?;
        let close_count =
            if final_value_boundary == Some(ProcessCaptureStreamTestFinalValueBoundary::Close) {
                for (process_index, (fixture_index, write)) in process_writes.iter().enumerate() {
                    let close_index = u64::try_from(process_index)
                        .context("process capture test-fixture close index overflowed")?;
                    let syscall_index = exec_count
                        .checked_add(read_count)
                        .and_then(|count| count.checked_add(write_count))
                        .and_then(|count| count.checked_add(close_index))
                        .context("process capture test-fixture close index overflowed")?;
                    let descriptor = i32::try_from(*fixture_index)
                        .context("process capture test-fixture descriptor does not fit i32")?
                        .checked_add(3)
                        .context("process capture test-fixture descriptor overflowed")?;
                    sink.record(ProcessCaptureEvent::CloseAttempt {
                        process: process.process,
                        attempt: crate::CapturedCloseAttempt {
                            syscall_index,
                            request: crate::CapturedCloseRequest {
                                descriptor,
                                identity: Some(crate::CapturedDescriptorIdentity {
                                    target: write.target.clone(),
                                    device: write.device,
                                    inode: write.inode,
                                    kind: crate::CapturedDescriptorKind::RegularFile,
                                }),
                                mount_id: Some(write.mount_id),
                            },
                            outcome: crate::CapturedCloseOutcome::Closed {
                                file_value: Some(CapturedMappedFileValue::WholeFile {
                                    bytes: u64::try_from(write.bytes.len()).context(
                                        "process capture test-fixture final value size overflowed",
                                    )?,
                                    digest: *blake3::hash(&write.bytes).as_bytes(),
                                    mode: write.mode,
                                }),
                            },
                        },
                    })?;
                }
                write_count
            } else {
                0
            };
        let process_transient_symlinks = transient_symlinks
            .iter()
            .filter(|link| link.process == process.process)
            .collect::<Vec<_>>();
        let transient_count = u64::try_from(process_transient_symlinks.len())
            .context("process capture test-fixture transient count does not fit u64")?;
        for (index, link) in process_transient_symlinks.iter().enumerate() {
            let transient_index = u64::try_from(index)
                .context("process capture test-fixture transient index does not fit u64")?;
            let syscall_index = exec_count
                .checked_add(read_count)
                .and_then(|count| count.checked_add(write_count))
                .and_then(|count| count.checked_add(close_count))
                .and_then(|count| count.checked_add(transient_index.checked_mul(2)?))
                .context("process capture test-fixture transient index overflowed")?;
            sink.record(ProcessCaptureEvent::SymbolicLinkCreationAttempt {
                process: process.process,
                attempt: crate::CapturedSymbolicLinkCreationAttempt {
                    syscall_index,
                    request: crate::CapturedSymbolicLinkCreationRequest {
                        system_call: crate::CapturedSymbolicLinkCreationSystemCall::SymlinkAt,
                        target: b"fixture-target".to_vec(),
                        target_is_null: false,
                        target_terminated: true,
                        directory_descriptor: libc::AT_FDCWD,
                        link_path: link.link_path.clone(),
                        link_path_is_null: false,
                        link_path_terminated: true,
                        base: None,
                    },
                    outcome: crate::CapturedSymbolicLinkCreationOutcome::Created,
                },
            })?;
            sink.record(ProcessCaptureEvent::PathRemovalAttempt {
                process: process.process,
                attempt: crate::CapturedPathRemovalAttempt {
                    syscall_index: syscall_index
                        .checked_add(1)
                        .context("process capture test-fixture transient index overflowed")?,
                    request: crate::CapturedPathRemovalRequest {
                        system_call: crate::CapturedPathRemovalSystemCall::UnlinkAt,
                        directory_descriptor: libc::AT_FDCWD,
                        path: link.link_path.clone(),
                        path_is_null: false,
                        base: None,
                        flags: 0,
                    },
                    outcome: crate::CapturedPathRemovalOutcome::Removed,
                },
            })?;
        }
        let process_extra = extra_syscall
            .filter(|(extra_process, _)| *extra_process == process.process)
            .map(|(_, syscall)| syscall);
        if let Some(syscall) = process_extra {
            let syscall_index = exec_count
                .checked_add(read_count)
                .and_then(|count| count.checked_add(write_count))
                .and_then(|count| count.checked_add(close_count))
                .and_then(|count| count.checked_add(transient_count.checked_mul(2)?))
                .context("process capture test-fixture extra-syscall index overflowed")?;
            match syscall {
                FixtureSyscall::Unknown { terminal } => {
                    sink.record(ProcessCaptureEvent::UnmodeledSyscallAttempt {
                        process: process.process,
                        attempt: CapturedUnmodeledSyscallAttempt {
                            syscall_index,
                            number: u64::MAX,
                            arguments: [0; 6],
                            outcome: if terminal {
                                crate::CapturedUnmodeledSyscallOutcome::Terminated
                            } else {
                                crate::CapturedUnmodeledSyscallOutcome::Returned {
                                    return_value: 0,
                                    is_error: false,
                                }
                            },
                        },
                    })?;
                }
                FixtureSyscall::TerminalOpen => sink.record(ProcessCaptureEvent::OpenAttempt {
                    process: process.process,
                    attempt: crate::CapturedOpenAttempt {
                        syscall_index,
                        request: crate::CapturedOpenRequest {
                            system_call: crate::CapturedOpenSystemCall::Open,
                            path: b"/fixture-blocked-open".to_vec(),
                            directory: None,
                            flags: libc::O_RDONLY as u64,
                            mode: 0,
                            resolve: 0,
                        },
                        outcome: crate::CapturedOpenOutcome::Terminated,
                    },
                })?,
            }
        }
        let lifecycle_exit_count = u64::from(model_lifecycle_exits);
        if model_lifecycle_exits {
            let status = match process.outcome {
                CapturedProcessOutcome::Exited(status) => status,
                CapturedProcessOutcome::Signaled(_) => {
                    anyhow::bail!(
                        "process capture lifecycle-exit fixture process did not exit normally"
                    )
                }
            };
            let syscall_index = exec_count
                .checked_add(read_count)
                .context("process capture test-fixture lifecycle-exit index overflowed")?
                .checked_add(write_count)
                .and_then(|count| count.checked_add(close_count))
                .and_then(|count| count.checked_add(transient_count.checked_mul(2)?))
                .and_then(|count| count.checked_add(u64::from(process_extra.is_some())))
                .context("process capture test-fixture lifecycle-exit index overflowed")?;
            let open_file_values = if final_value_boundary
                == Some(ProcessCaptureStreamTestFinalValueBoundary::LifecycleExit)
            {
                process_writes
                    .iter()
                    .map(|(fixture_index, write)| {
                        let descriptor = i32::try_from(*fixture_index)
                            .context("process capture test-fixture descriptor does not fit i32")?
                            .checked_add(3)
                            .context("process capture test-fixture descriptor overflowed")?;
                        Ok(crate::CapturedExitFileValue {
                            descriptor,
                            identity: crate::CapturedDescriptorIdentity {
                                target: write.target.clone(),
                                device: write.device,
                                inode: write.inode,
                                kind: crate::CapturedDescriptorKind::RegularFile,
                            },
                            mount_id: write.mount_id,
                            value: CapturedMappedFileValue::WholeFile {
                                bytes: u64::try_from(write.bytes.len()).context(
                                    "process capture test-fixture final value size overflowed",
                                )?,
                                digest: *blake3::hash(&write.bytes).as_bytes(),
                                mode: write.mode,
                            },
                        })
                    })
                    .collect::<Result<Vec<_>>>()?
            } else {
                Vec::new()
            };
            sink.record(ProcessCaptureEvent::LifecycleExitAttempt {
                process: process.process,
                attempt: crate::CapturedLifecycleExitAttempt {
                    syscall_index,
                    request: crate::CapturedLifecycleExitRequest {
                        system_call: crate::CapturedLifecycleExitSystemCall::ExitGroup,
                        status,
                    },
                    open_file_values,
                    outcome: process.outcome,
                },
            })?;
        }
        sink.record(ProcessCaptureEvent::ProcessFinished {
            process: process.process,
            summary: finish(
                invocation_count,
                exec_count,
                read_count,
                write_count,
                close_count,
                transient_count,
                process_extra,
                lifecycle_exit_count,
                process.outcome,
            )?,
        })?;
    }
    sink.finish(root, path)
}

fn open_capture_stream(path: &Path, limits: ProcessCaptureStreamLimits) -> Result<File> {
    validate_private_parent(path)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .context("open private process capture stream")?;
    validate_private_file(&file)?;
    let size = file
        .metadata()
        .context("stat process capture stream")?
        .len();
    if size > limits.max_stream_bytes {
        anyhow::bail!("process capture stream exceeds its byte budget");
    }
    Ok(file)
}

struct StreamVisitorSink<'a, V> {
    validator: SemanticStreamValidator,
    visitor: &'a mut V,
}

impl<'a, V> StreamVisitorSink<'a, V> {
    fn new(visitor: &'a mut V) -> Self {
        Self {
            validator: SemanticStreamValidator::default(),
            visitor,
        }
    }

    fn finish(&self, root: CapturedProcessId) -> Result<()> {
        self.validator.finish(root)
    }
}

impl<V: ProcessCaptureStreamVisitor> ProcessCaptureSink for StreamVisitorSink<'_, V> {
    fn record(&mut self, event: ProcessCaptureEvent) -> Result<()> {
        self.validator.record(&event)?;
        match &event {
            ProcessCaptureEvent::ProcessStarted { process, parent } => {
                self.visitor
                    .visit(ProcessCaptureStreamEvent::ProcessStarted {
                        process: *process,
                        parent: *parent,
                    })
            }
            ProcessCaptureEvent::ProcessCreationAttempt { process, attempt } => {
                self.visitor
                    .visit(ProcessCaptureStreamEvent::ProcessCreationAttempt {
                        process: *process,
                        attempt,
                    })
            }
            ProcessCaptureEvent::ChildWaitAttempt { process, attempt } => {
                self.visitor
                    .visit(ProcessCaptureStreamEvent::ChildWaitAttempt {
                        process: *process,
                        attempt,
                    })
            }
            ProcessCaptureEvent::StatxAttempt { process, attempt } => {
                self.visitor.visit(ProcessCaptureStreamEvent::StatxAttempt {
                    process: *process,
                    attempt,
                })
            }
            ProcessCaptureEvent::ProcessIdentityAttempt { process, attempt } => {
                self.visitor
                    .visit(ProcessCaptureStreamEvent::ProcessIdentityAttempt {
                        process: *process,
                        attempt,
                    })
            }
            ProcessCaptureEvent::XattrAttempt { process, attempt } => {
                self.visitor.visit(ProcessCaptureStreamEvent::XattrAttempt {
                    process: *process,
                    attempt,
                })
            }
            ProcessCaptureEvent::DirectoryCreationAttempt { process, attempt } => self
                .visitor
                .visit(ProcessCaptureStreamEvent::DirectoryCreationAttempt {
                    process: *process,
                    attempt,
                }),
            ProcessCaptureEvent::PathRenameAttempt { process, attempt } => {
                self.visitor
                    .visit(ProcessCaptureStreamEvent::PathRenameAttempt {
                        process: *process,
                        attempt,
                    })
            }
            ProcessCaptureEvent::PathModeChangeAttempt { process, attempt } => {
                self.visitor
                    .visit(ProcessCaptureStreamEvent::PathModeChangeAttempt {
                        process: *process,
                        attempt,
                    })
            }
            ProcessCaptureEvent::DescriptorTimestampChangeAttempt { process, attempt } => {
                self.visitor.visit(
                    ProcessCaptureStreamEvent::DescriptorTimestampChangeAttempt {
                        process: *process,
                        attempt,
                    },
                )
            }
            ProcessCaptureEvent::SymbolicLinkCreationAttempt { process, attempt } => self
                .visitor
                .visit(ProcessCaptureStreamEvent::SymbolicLinkCreationAttempt {
                    process: *process,
                    attempt,
                }),
            ProcessCaptureEvent::FilesystemStatusAttempt { process, attempt } => self
                .visitor
                .visit(ProcessCaptureStreamEvent::FilesystemStatusAttempt {
                    process: *process,
                    attempt,
                }),
            ProcessCaptureEvent::CwdChangeAttempt { process, attempt } => {
                self.visitor
                    .visit(ProcessCaptureStreamEvent::CwdChangeAttempt {
                        process: *process,
                        attempt,
                    })
            }
            ProcessCaptureEvent::PathRemovalAttempt { process, attempt } => {
                self.visitor
                    .visit(ProcessCaptureStreamEvent::PathRemovalAttempt {
                        process: *process,
                        attempt,
                    })
            }
            ProcessCaptureEvent::Invocation {
                process,
                invocation,
            } => self.visitor.visit(ProcessCaptureStreamEvent::Invocation {
                process: *process,
                invocation,
            }),
            ProcessCaptureEvent::ExecAttempt { process, attempt } => {
                self.visitor.visit(ProcessCaptureStreamEvent::ExecAttempt {
                    process: *process,
                    attempt,
                })
            }
            ProcessCaptureEvent::OpenAttempt { process, attempt } => {
                self.visitor.visit(ProcessCaptureStreamEvent::OpenAttempt {
                    process: *process,
                    attempt,
                })
            }
            ProcessCaptureEvent::NewfstatatAttempt { process, attempt } => {
                self.visitor
                    .visit(ProcessCaptureStreamEvent::NewfstatatAttempt {
                        process: *process,
                        attempt,
                    })
            }
            ProcessCaptureEvent::FstatAttempt { process, attempt } => {
                self.visitor.visit(ProcessCaptureStreamEvent::FstatAttempt {
                    process: *process,
                    attempt,
                })
            }
            ProcessCaptureEvent::GetrandomAttempt { process, attempt } => {
                self.visitor
                    .visit(ProcessCaptureStreamEvent::GetrandomAttempt {
                        process: *process,
                        attempt,
                    })
            }
            ProcessCaptureEvent::RuntimeObservationAttempt { process, attempt } => self
                .visitor
                .visit(ProcessCaptureStreamEvent::RuntimeObservationAttempt {
                    process: *process,
                    attempt,
                }),
            ProcessCaptureEvent::PrlimitAttempt { process, attempt } => {
                self.visitor
                    .visit(ProcessCaptureStreamEvent::PrlimitAttempt {
                        process: *process,
                        attempt,
                    })
            }
            ProcessCaptureEvent::CloseAttempt { process, attempt } => {
                self.visitor.visit(ProcessCaptureStreamEvent::CloseAttempt {
                    process: *process,
                    attempt,
                })
            }
            ProcessCaptureEvent::SeekAttempt { process, attempt } => {
                self.visitor.visit(ProcessCaptureStreamEvent::SeekAttempt {
                    process: *process,
                    attempt,
                })
            }
            ProcessCaptureEvent::FileAdviceAttempt { process, attempt } => {
                self.visitor
                    .visit(ProcessCaptureStreamEvent::FileAdviceAttempt {
                        process: *process,
                        attempt,
                    })
            }
            ProcessCaptureEvent::FcntlAttempt { process, attempt } => {
                self.visitor.visit(ProcessCaptureStreamEvent::FcntlAttempt {
                    process: *process,
                    attempt,
                })
            }
            ProcessCaptureEvent::IoctlAttempt { process, attempt } => {
                self.visitor.visit(ProcessCaptureStreamEvent::IoctlAttempt {
                    process: *process,
                    attempt,
                })
            }
            ProcessCaptureEvent::PipeAttempt { process, attempt } => {
                self.visitor.visit(ProcessCaptureStreamEvent::PipeAttempt {
                    process: *process,
                    attempt,
                })
            }
            ProcessCaptureEvent::EpollCreateAttempt { process, attempt } => {
                self.visitor
                    .visit(ProcessCaptureStreamEvent::EpollCreateAttempt {
                        process: *process,
                        attempt,
                    })
            }
            ProcessCaptureEvent::SocketCreateAttempt { process, attempt } => {
                self.visitor
                    .visit(ProcessCaptureStreamEvent::SocketCreateAttempt {
                        process: *process,
                        attempt,
                    })
            }
            ProcessCaptureEvent::SocketConnectAttempt { process, attempt } => {
                self.visitor
                    .visit(ProcessCaptureStreamEvent::SocketConnectAttempt {
                        process: *process,
                        attempt,
                    })
            }
            ProcessCaptureEvent::Dup2Attempt { process, attempt } => {
                self.visitor.visit(ProcessCaptureStreamEvent::Dup2Attempt {
                    process: *process,
                    attempt,
                })
            }
            ProcessCaptureEvent::SignalStateAttempt { process, attempt } => {
                self.visitor
                    .visit(ProcessCaptureStreamEvent::SignalStateAttempt {
                        process: *process,
                        attempt,
                    })
            }
            ProcessCaptureEvent::SignalDelivery { process, delivery } => {
                self.visitor
                    .visit(ProcessCaptureStreamEvent::SignalDelivery {
                        process: *process,
                        delivery,
                    })
            }
            ProcessCaptureEvent::UmaskAttempt { process, attempt } => {
                self.visitor.visit(ProcessCaptureStreamEvent::UmaskAttempt {
                    process: *process,
                    attempt,
                })
            }
            ProcessCaptureEvent::CredentialAttempt { process, attempt } => {
                self.visitor
                    .visit(ProcessCaptureStreamEvent::CredentialAttempt {
                        process: *process,
                        attempt,
                    })
            }
            ProcessCaptureEvent::FutexWakeAttempt { process, attempt } => {
                self.visitor
                    .visit(ProcessCaptureStreamEvent::FutexWakeAttempt {
                        process: *process,
                        attempt,
                    })
            }
            ProcessCaptureEvent::MemoryStateAttempt { process, attempt } => {
                self.visitor
                    .visit(ProcessCaptureStreamEvent::MemoryStateAttempt {
                        process: *process,
                        attempt,
                    })
            }
            ProcessCaptureEvent::ThreadStateAttempt { process, attempt } => {
                self.visitor
                    .visit(ProcessCaptureStreamEvent::ThreadStateAttempt {
                        process: *process,
                        attempt,
                    })
            }
            ProcessCaptureEvent::LifecycleExitAttempt { process, attempt } => {
                self.visitor
                    .visit(ProcessCaptureStreamEvent::LifecycleExitAttempt {
                        process: *process,
                        attempt,
                    })
            }
            ProcessCaptureEvent::ReadlinkAttempt { process, attempt } => {
                self.visitor
                    .visit(ProcessCaptureStreamEvent::ReadlinkAttempt {
                        process: *process,
                        attempt,
                    })
            }
            ProcessCaptureEvent::AccessAttempt { process, attempt } => {
                self.visitor
                    .visit(ProcessCaptureStreamEvent::AccessAttempt {
                        process: *process,
                        attempt,
                    })
            }
            ProcessCaptureEvent::GetcwdAttempt { process, attempt } => {
                self.visitor
                    .visit(ProcessCaptureStreamEvent::GetcwdAttempt {
                        process: *process,
                        attempt,
                    })
            }
            ProcessCaptureEvent::ReadAttempt { process, attempt } => {
                self.visitor.visit(ProcessCaptureStreamEvent::ReadAttempt {
                    process: *process,
                    attempt,
                })
            }
            ProcessCaptureEvent::Getdents64Attempt { process, attempt } => {
                self.visitor
                    .visit(ProcessCaptureStreamEvent::Getdents64Attempt {
                        process: *process,
                        attempt,
                    })
            }
            ProcessCaptureEvent::MmapAttempt { process, attempt } => {
                self.visitor.visit(ProcessCaptureStreamEvent::MmapAttempt {
                    process: *process,
                    attempt,
                })
            }
            ProcessCaptureEvent::WriteAttempt { process, attempt } => {
                self.visitor.visit(ProcessCaptureStreamEvent::WriteAttempt {
                    process: *process,
                    attempt,
                })
            }
            ProcessCaptureEvent::UnmodeledSyscallAttempt { process, attempt } => self
                .visitor
                .visit(ProcessCaptureStreamEvent::UnmodeledSyscallAttempt {
                    process: *process,
                    attempt,
                }),
            ProcessCaptureEvent::ProcessFinished { process, summary } => {
                self.visitor
                    .visit(ProcessCaptureStreamEvent::ProcessFinished {
                        process: *process,
                        outcome: summary.outcome,
                    })
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ScannedStream {
    root: CapturedProcessId,
    summary: ProcessCaptureStreamSummary,
}

fn scan_stream(
    file: &mut File,
    expected_identity: ProcessCaptureStreamIdentity,
    limits: ProcessCaptureStreamLimits,
    decode: bool,
    mut sink: Option<&mut dyn ProcessCaptureSink>,
) -> Result<ScannedStream> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(DIGEST_DOMAIN);
    let mut stream_bytes = 0u64;
    let header = read_array::<{ HEADER_BYTES as usize }>(file, "process capture stream header")?;
    stream_bytes = checked_add(stream_bytes, HEADER_BYTES, "stream byte count")?;
    hasher.update(&header);
    decode_header(&header, expected_identity)?;
    let mut records = 0u64;
    let mut payload_bytes = 0u64;
    let mut counts = EventCounts::default();
    loop {
        let tag = read_array::<1>(file, "process capture stream record tag")?[0];
        stream_bytes = checked_add(stream_bytes, 1, "stream byte count")?;
        hasher.update(&[tag]);
        if tag == FRAME_TAG_FOOTER {
            let remainder =
                read_array::<{ FOOTER_BYTES as usize - 1 }>(file, "process capture stream footer")?;
            stream_bytes = checked_add(stream_bytes, FOOTER_BYTES - 1, "stream byte count")?;
            hasher.update(&remainder);
            let footer = decode_footer(tag, &remainder)?;
            if footer.identity != expected_identity
                || footer.records != records
                || footer.processes != counts.process_started
                || counts.process_started != counts.process_finished
                || footer.payload_bytes != payload_bytes
                || footer.census != counts.census
                || counts.census != counts.denominators
            {
                anyhow::bail!("process capture stream footer census does not match its records");
            }
            let committed_len = stream_bytes;
            let trailer =
                read_array::<{ TRAILER_BYTES as usize }>(file, "process capture stream trailer")?;
            stream_bytes = checked_add(stream_bytes, TRAILER_BYTES, "stream byte count")?;
            if &trailer[..8] != TRAILER_MAGIC.as_slice() {
                anyhow::bail!("process capture stream has an invalid trailer");
            }
            let stored_len = u64::from_le_bytes(trailer[8..16].try_into().unwrap());
            let stored_digest: [u8; 32] = trailer[16..48].try_into().unwrap();
            let mut final_hasher = hasher.clone();
            final_hasher.update(&committed_len.to_le_bytes());
            let digest = *final_hasher.finalize().as_bytes();
            if stored_len != committed_len || stored_digest != digest {
                anyhow::bail!("process capture stream trailer digest does not match its prefix");
            }
            if stream_bytes > limits.max_stream_bytes {
                anyhow::bail!("process capture stream exceeds its byte budget");
            }
            let mut trailing = [0u8; 1];
            if file
                .read(&mut trailing)
                .context("check process capture stream end")?
                != 0
            {
                anyhow::bail!("process capture stream has trailing bytes");
            }
            return Ok(ScannedStream {
                root: footer.root,
                summary: ProcessCaptureStreamSummary {
                    identity: footer.identity,
                    digest,
                    record_count: records,
                    process_count: footer.processes,
                    payload_bytes,
                    stream_bytes,
                    census: footer.census,
                },
            });
        }
        if records >= limits.max_records {
            anyhow::bail!("process capture stream exceeds its record budget");
        }
        let prefix_tail = read_array::<{ FRAME_PREFIX_BYTES as usize - 1 }>(
            file,
            "process capture stream frame prefix",
        )?;
        stream_bytes = checked_add(stream_bytes, FRAME_PREFIX_BYTES - 1, "stream byte count")?;
        hasher.update(&prefix_tail);
        if prefix_tail[..3] != [0; 3]
            || u32::from_le_bytes(prefix_tail[3..7].try_into().unwrap()) != FORMAT_RECORD_VERSION
        {
            anyhow::bail!("process capture stream has an invalid frame prefix");
        }
        let sequence = u64::from_le_bytes(prefix_tail[7..15].try_into().unwrap());
        let payload_len = u64::from_le_bytes(prefix_tail[15..23].try_into().unwrap());
        if sequence != records || payload_len > limits.max_frame_bytes {
            anyhow::bail!("process capture stream has an invalid frame sequence or size");
        }
        let projected = stream_bytes
            .checked_add(payload_len)
            .and_then(|bytes| bytes.checked_add(FRAME_DIGEST_BYTES))
            .and_then(|bytes| bytes.checked_add(FOOTER_BYTES))
            .and_then(|bytes| bytes.checked_add(TRAILER_BYTES))
            .context("process capture stream size overflowed")?;
        if projected > limits.max_stream_bytes {
            anyhow::bail!("process capture stream exceeds its byte budget");
        }
        let retain_payload = decode || tag == FRAME_TAG_PROCESS_FINISHED;
        let mut frame_hasher = blake3::Hasher::new();
        let payload = if retain_payload {
            let payload_len_usize = usize::try_from(payload_len)
                .context("process capture stream frame size exceeds usize")?;
            let mut payload = Vec::new();
            payload
                .try_reserve_exact(payload_len_usize)
                .context("reserve process capture stream frame")?;
            payload.resize(payload_len_usize, 0);
            file.read_exact(&mut payload)
                .context("read process capture stream frame payload")?;
            hasher.update(&payload);
            frame_hasher.update(&payload);
            Some(payload)
        } else {
            let mut remaining = payload_len;
            let mut scratch = [0u8; 64 * 1024];
            while remaining != 0 {
                let chunk = usize::try_from(remaining.min(scratch.len() as u64)).unwrap();
                file.read_exact(&mut scratch[..chunk])
                    .context("read process capture stream frame payload")?;
                hasher.update(&scratch[..chunk]);
                frame_hasher.update(&scratch[..chunk]);
                remaining -= chunk as u64;
            }
            None
        };
        stream_bytes = checked_add(stream_bytes, payload_len, "stream byte count")?;
        let frame_digest = read_array::<{ FRAME_DIGEST_BYTES as usize }>(
            file,
            "process capture stream frame digest",
        )?;
        stream_bytes = checked_add(stream_bytes, FRAME_DIGEST_BYTES, "stream byte count")?;
        hasher.update(&frame_digest);
        if frame_digest != *frame_hasher.finalize().as_bytes() {
            anyhow::bail!("process capture stream frame digest changed");
        }
        counts.record_tag(tag)?;
        if counts.process_started > limits.max_processes {
            anyhow::bail!("process capture stream exceeds its process budget");
        }
        if decode {
            let event = decode_event(
                tag,
                payload
                    .as_deref()
                    .context("decoded process event payload was not retained")?,
                payload_len,
            )?;
            if let ProcessCaptureEvent::ProcessFinished { summary, .. } = event {
                counts.add_finish(summary)?;
                sink.as_deref_mut()
                    .context("decoded stream has no event sink")?
                    .record(ProcessCaptureEvent::ProcessFinished {
                        process: event.process(),
                        summary,
                    })?;
            } else {
                sink.as_deref_mut()
                    .context("decoded stream has no event sink")?
                    .record(event)?;
            }
        } else if tag == FRAME_TAG_PROCESS_FINISHED {
            let event = decode_event(
                tag,
                payload
                    .as_deref()
                    .context("process finish payload was not retained")?,
                payload_len,
            )?;
            let ProcessCaptureEvent::ProcessFinished { summary, .. } = event else {
                unreachable!();
            };
            counts.add_finish(summary)?;
        }
        payload_bytes = checked_add(payload_bytes, payload_len, "payload byte count")?;
        records = checked_add(records, 1, "record count")?;
    }
}

fn encode_event(event: &ProcessCaptureEvent, limit: u64) -> Result<(u8, Vec<u8>)> {
    match event {
        ProcessCaptureEvent::ProcessStarted { process, parent } => Ok((
            FRAME_TAG_PROCESS_STARTED,
            encode_payload(
                &ProcessStartedPayload {
                    process: *process,
                    parent: *parent,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::Invocation {
            process,
            invocation,
        } => Ok((
            FRAME_TAG_INVOCATION,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: invocation,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::ExecAttempt { process, attempt } => Ok((
            FRAME_TAG_EXEC_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::OpenAttempt { process, attempt } => Ok((
            FRAME_TAG_OPEN_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::NewfstatatAttempt { process, attempt } => Ok((
            FRAME_TAG_NEWFSTATAT_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::FstatAttempt { process, attempt } => Ok((
            FRAME_TAG_FSTAT_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::GetrandomAttempt { process, attempt } => Ok((
            FRAME_TAG_GETRANDOM_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::RuntimeObservationAttempt { process, attempt } => Ok((
            FRAME_TAG_RUNTIME_OBSERVATION_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::PrlimitAttempt { process, attempt } => Ok((
            FRAME_TAG_PRLIMIT_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::CloseAttempt { process, attempt } => Ok((
            FRAME_TAG_CLOSE_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::SeekAttempt { process, attempt } => Ok((
            FRAME_TAG_SEEK_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::FileAdviceAttempt { process, attempt } => Ok((
            FRAME_TAG_FILE_ADVICE_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::FcntlAttempt { process, attempt } => Ok((
            FRAME_TAG_FCNTL_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::IoctlAttempt { process, attempt } => Ok((
            FRAME_TAG_IOCTL_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::PipeAttempt { process, attempt } => Ok((
            FRAME_TAG_PIPE_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::EpollCreateAttempt { process, attempt } => Ok((
            FRAME_TAG_EPOLL_CREATE_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::SocketCreateAttempt { process, attempt } => Ok((
            FRAME_TAG_SOCKET_CREATE_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::SocketConnectAttempt { process, attempt } => Ok((
            FRAME_TAG_SOCKET_CONNECT_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::Dup2Attempt { process, attempt } => Ok((
            FRAME_TAG_DUP2_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::SignalStateAttempt { process, attempt } => Ok((
            FRAME_TAG_SIGNAL_STATE_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::SignalDelivery { process, delivery } => Ok((
            FRAME_TAG_SIGNAL_DELIVERY,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: delivery,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::UmaskAttempt { process, attempt } => Ok((
            FRAME_TAG_UMASK_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::CredentialAttempt { process, attempt } => Ok((
            FRAME_TAG_CREDENTIAL_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::FutexWakeAttempt { process, attempt } => Ok((
            FRAME_TAG_FUTEX_WAKE_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::ProcessCreationAttempt { process, attempt } => Ok((
            FRAME_TAG_PROCESS_CREATION_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::ChildWaitAttempt { process, attempt } => Ok((
            FRAME_TAG_CHILD_WAIT_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::ProcessIdentityAttempt { process, attempt } => Ok((
            FRAME_TAG_PROCESS_IDENTITY_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::MemoryStateAttempt { process, attempt } => Ok((
            FRAME_TAG_MEMORY_STATE_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::ThreadStateAttempt { process, attempt } => Ok((
            FRAME_TAG_THREAD_STATE_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::LifecycleExitAttempt { process, attempt } => Ok((
            FRAME_TAG_LIFECYCLE_EXIT_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::StatxAttempt { process, attempt } => Ok((
            FRAME_TAG_STATX_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::XattrAttempt { process, attempt } => Ok((
            FRAME_TAG_XATTR_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::ReadlinkAttempt { process, attempt } => Ok((
            FRAME_TAG_READLINK_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::AccessAttempt { process, attempt } => Ok((
            FRAME_TAG_ACCESS_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::GetcwdAttempt { process, attempt } => Ok((
            FRAME_TAG_GETCWD_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::CwdChangeAttempt { process, attempt } => Ok((
            FRAME_TAG_CWD_CHANGE_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::PathRemovalAttempt { process, attempt } => Ok((
            FRAME_TAG_PATH_REMOVAL_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::DirectoryCreationAttempt { process, attempt } => Ok((
            FRAME_TAG_DIRECTORY_CREATION_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::PathRenameAttempt { process, attempt } => Ok((
            FRAME_TAG_PATH_RENAME_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::PathModeChangeAttempt { process, attempt } => Ok((
            FRAME_TAG_PATH_MODE_CHANGE_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::DescriptorTimestampChangeAttempt { process, attempt } => Ok((
            FRAME_TAG_DESCRIPTOR_TIMESTAMP_CHANGE_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::SymbolicLinkCreationAttempt { process, attempt } => Ok((
            FRAME_TAG_SYMBOLIC_LINK_CREATION_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::FilesystemStatusAttempt { process, attempt } => Ok((
            FRAME_TAG_FILESYSTEM_STATUS_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::ReadAttempt { process, attempt } => Ok((
            FRAME_TAG_READ_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::Getdents64Attempt { process, attempt } => Ok((
            FRAME_TAG_GETDENTS64_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::MmapAttempt { process, attempt } => Ok((
            FRAME_TAG_MMAP_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::WriteAttempt { process, attempt } => Ok((
            FRAME_TAG_WRITE_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::UnmodeledSyscallAttempt { process, attempt } => Ok((
            FRAME_TAG_UNMODELED_SYSCALL_ATTEMPT,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: attempt,
                },
                limit,
            )?,
        )),
        ProcessCaptureEvent::ProcessFinished { process, summary } => Ok((
            FRAME_TAG_PROCESS_FINISHED,
            encode_payload(
                &ProcessValue {
                    process: *process,
                    value: summary,
                },
                limit,
            )?,
        )),
    }
}

fn decode_event(tag: u8, payload: &[u8], limit: u64) -> Result<ProcessCaptureEvent> {
    Ok(match tag {
        FRAME_TAG_PROCESS_STARTED => {
            let value: ProcessStartedPayload = decode_payload(payload, limit)?;
            ProcessCaptureEvent::ProcessStarted {
                process: value.process,
                parent: value.parent,
            }
        }
        FRAME_TAG_INVOCATION => {
            let value: ProcessValue<CapturedInvocation> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::Invocation {
                process: value.process,
                invocation: value.value,
            }
        }
        FRAME_TAG_EXEC_ATTEMPT => {
            let value: ProcessValue<CapturedExecAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::ExecAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_OPEN_ATTEMPT => {
            let value: ProcessValue<CapturedOpenAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::OpenAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_NEWFSTATAT_ATTEMPT => {
            let value: ProcessValue<CapturedNewfstatatAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::NewfstatatAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_FSTAT_ATTEMPT => {
            let value: ProcessValue<CapturedFstatAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::FstatAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_GETRANDOM_ATTEMPT => {
            let value: ProcessValue<CapturedGetrandomAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::GetrandomAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_RUNTIME_OBSERVATION_ATTEMPT => {
            let value: ProcessValue<CapturedRuntimeObservationAttempt> =
                decode_payload(payload, limit)?;
            ProcessCaptureEvent::RuntimeObservationAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_PRLIMIT_ATTEMPT => {
            let value: ProcessValue<CapturedPrlimitAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::PrlimitAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_CLOSE_ATTEMPT => {
            let value: ProcessValue<CapturedCloseAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::CloseAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_SEEK_ATTEMPT => {
            let value: ProcessValue<CapturedSeekAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::SeekAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_FILE_ADVICE_ATTEMPT => {
            let value: ProcessValue<CapturedFileAdviceAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::FileAdviceAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_FCNTL_ATTEMPT => {
            let value: ProcessValue<CapturedFcntlAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::FcntlAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_IOCTL_ATTEMPT => {
            let value: ProcessValue<CapturedIoctlAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::IoctlAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_PIPE_ATTEMPT => {
            let value: ProcessValue<CapturedPipeAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::PipeAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_EPOLL_CREATE_ATTEMPT => {
            let value: ProcessValue<CapturedEpollCreateAttempt> =
                decode_payload(payload, limit)?;
            ProcessCaptureEvent::EpollCreateAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_SOCKET_CREATE_ATTEMPT => {
            let value: ProcessValue<CapturedSocketCreateAttempt> =
                decode_payload(payload, limit)?;
            ProcessCaptureEvent::SocketCreateAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_SOCKET_CONNECT_ATTEMPT => {
            let value: ProcessValue<CapturedSocketConnectAttempt> =
                decode_payload(payload, limit)?;
            ProcessCaptureEvent::SocketConnectAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_DUP2_ATTEMPT => {
            let value: ProcessValue<CapturedDup2Attempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::Dup2Attempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_SIGNAL_STATE_ATTEMPT => {
            let value: ProcessValue<CapturedSignalStateAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::SignalStateAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_SIGNAL_DELIVERY => {
            let value: ProcessValue<CapturedSignalDelivery> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::SignalDelivery {
                process: value.process,
                delivery: value.value,
            }
        }
        FRAME_TAG_UMASK_ATTEMPT => {
            let value: ProcessValue<CapturedUmaskAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::UmaskAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_CREDENTIAL_ATTEMPT => {
            let value: ProcessValue<CapturedCredentialAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::CredentialAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_FUTEX_WAKE_ATTEMPT => {
            let value: ProcessValue<CapturedFutexWakeAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::FutexWakeAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_PROCESS_CREATION_ATTEMPT => {
            let value: ProcessValue<CapturedProcessCreationAttempt> =
                decode_payload(payload, limit)?;
            ProcessCaptureEvent::ProcessCreationAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_CHILD_WAIT_ATTEMPT => {
            let value: ProcessValue<CapturedChildWaitAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::ChildWaitAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_PROCESS_IDENTITY_ATTEMPT => {
            let value: ProcessValue<CapturedProcessIdentityAttempt> =
                decode_payload(payload, limit)?;
            ProcessCaptureEvent::ProcessIdentityAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_MEMORY_STATE_ATTEMPT => {
            let value: ProcessValue<CapturedMemoryStateAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::MemoryStateAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_THREAD_STATE_ATTEMPT => {
            let value: ProcessValue<CapturedThreadStateAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::ThreadStateAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_LIFECYCLE_EXIT_ATTEMPT => {
            let value: ProcessValue<CapturedLifecycleExitAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::LifecycleExitAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_STATX_ATTEMPT => {
            let value: ProcessValue<CapturedStatxAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::StatxAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_XATTR_ATTEMPT => {
            let value: ProcessValue<CapturedXattrAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::XattrAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_READLINK_ATTEMPT => {
            let value: ProcessValue<CapturedReadlinkAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::ReadlinkAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_ACCESS_ATTEMPT => {
            let value: ProcessValue<CapturedAccessAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::AccessAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_GETCWD_ATTEMPT => {
            let value: ProcessValue<CapturedGetcwdAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::GetcwdAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_CWD_CHANGE_ATTEMPT => {
            let value: ProcessValue<CapturedCwdChangeAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::CwdChangeAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_PATH_REMOVAL_ATTEMPT => {
            let value: ProcessValue<CapturedPathRemovalAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::PathRemovalAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_DIRECTORY_CREATION_ATTEMPT => {
            let value: ProcessValue<CapturedDirectoryCreationAttempt> =
                decode_payload(payload, limit)?;
            ProcessCaptureEvent::DirectoryCreationAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_PATH_RENAME_ATTEMPT => {
            let value: ProcessValue<CapturedPathRenameAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::PathRenameAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_PATH_MODE_CHANGE_ATTEMPT => {
            let value: ProcessValue<CapturedPathModeChangeAttempt> =
                decode_payload(payload, limit)?;
            ProcessCaptureEvent::PathModeChangeAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_DESCRIPTOR_TIMESTAMP_CHANGE_ATTEMPT => {
            let value: ProcessValue<CapturedDescriptorTimestampChangeAttempt> =
                decode_payload(payload, limit)?;
            ProcessCaptureEvent::DescriptorTimestampChangeAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_SYMBOLIC_LINK_CREATION_ATTEMPT => {
            let value: ProcessValue<CapturedSymbolicLinkCreationAttempt> =
                decode_payload(payload, limit)?;
            ProcessCaptureEvent::SymbolicLinkCreationAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_FILESYSTEM_STATUS_ATTEMPT => {
            let value: ProcessValue<CapturedFilesystemStatusAttempt> =
                decode_payload(payload, limit)?;
            ProcessCaptureEvent::FilesystemStatusAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_READ_ATTEMPT => {
            let value: ProcessValue<CapturedReadAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::ReadAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_GETDENTS64_ATTEMPT => {
            let value: ProcessValue<CapturedGetdents64Attempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::Getdents64Attempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_MMAP_ATTEMPT => {
            let value: ProcessValue<CapturedMmapAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::MmapAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_WRITE_ATTEMPT => {
            let value: ProcessValue<CapturedWriteAttempt> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::WriteAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_UNMODELED_SYSCALL_ATTEMPT => {
            let value: ProcessValue<CapturedUnmodeledSyscallAttempt> =
                decode_payload(payload, limit)?;
            ProcessCaptureEvent::UnmodeledSyscallAttempt {
                process: value.process,
                attempt: value.value,
            }
        }
        FRAME_TAG_PROCESS_FINISHED => {
            let value: ProcessValue<ProcessFinish> = decode_payload(payload, limit)?;
            ProcessCaptureEvent::ProcessFinished {
                process: value.process,
                summary: value.value,
            }
        }
        _ => anyhow::bail!("process capture stream has an unknown record tag"),
    })
}

fn codec() -> impl Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_little_endian()
        .reject_trailing_bytes()
}

fn encode_payload<T: Serialize>(value: &T, limit: u64) -> Result<Vec<u8>> {
    let size = codec()
        .serialized_size(value)
        .context("size process capture stream event")?;
    if size > limit {
        anyhow::bail!("process capture stream frame exceeds its byte budget");
    }
    let size = usize::try_from(size).context("process capture stream frame exceeds usize")?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .context("reserve process capture stream event")?;
    codec()
        .with_limit(limit)
        .serialize_into(&mut bytes, value)
        .context("encode process capture stream event")?;
    if bytes.len() != size {
        anyhow::bail!("process capture stream event size changed during encoding");
    }
    Ok(bytes)
}

fn decode_payload<T: DeserializeOwned>(payload: &[u8], limit: u64) -> Result<T> {
    codec()
        .with_limit(limit)
        .deserialize(payload)
        .context("decode process capture stream event")
}

fn encode_header(identity: ProcessCaptureStreamIdentity) -> [u8; HEADER_BYTES as usize] {
    let mut header = [0u8; HEADER_BYTES as usize];
    header[..16].copy_from_slice(&HEADER_MAGIC);
    header[16..20].copy_from_slice(&PROCESS_CAPTURE_STREAM_VERSION.to_le_bytes());
    header[20..24].copy_from_slice(&PROCESS_CAPTURE_RECEIPT_VERSION.to_le_bytes());
    header[24..40].copy_from_slice(&identity.capture_id);
    header[40..72].copy_from_slice(&identity.context_digest);
    header
}

fn decode_header(
    header: &[u8; HEADER_BYTES as usize],
    expected_identity: ProcessCaptureStreamIdentity,
) -> Result<()> {
    if header[..16] != HEADER_MAGIC
        || u32::from_le_bytes(header[16..20].try_into().unwrap()) != PROCESS_CAPTURE_STREAM_VERSION
        || u32::from_le_bytes(header[20..24].try_into().unwrap()) != PROCESS_CAPTURE_RECEIPT_VERSION
        || header[24..40] != expected_identity.capture_id
        || header[40..72] != expected_identity.context_digest
        || header[72..72 + HEADER_RESERVED_BYTES] != [0; HEADER_RESERVED_BYTES]
    {
        anyhow::bail!("process capture stream has an invalid header");
    }
    Ok(())
}

fn encode_footer(
    identity: ProcessCaptureStreamIdentity,
    root: CapturedProcessId,
    records: u64,
    processes: u64,
    payload_bytes: u64,
    census: ProcessCaptureStreamCensus,
) -> [u8; FOOTER_BYTES as usize] {
    let mut footer = [0u8; FOOTER_BYTES as usize];
    footer[0] = FRAME_TAG_FOOTER;
    let mut offset = 4;
    for value in [
        root,
        records,
        processes,
        payload_bytes,
        census.invocations,
        census.exec_attempts,
        census.open_attempts,
        census.newfstatat_attempts,
        census.fstat_attempts,
        census.getrandom_attempts,
        census.runtime_observation_attempts,
        census.prlimit_attempts,
        census.close_attempts,
        census.seek_attempts,
        census.file_advice_attempts,
        census.fcntl_attempts,
        census.ioctl_attempts,
        census.pipe_attempts,
        census.epoll_create_attempts,
        census.socket_create_attempts,
        census.socket_connect_attempts,
        census.dup2_attempts,
        census.signal_state_attempts,
        census.signal_deliveries,
        census.umask_attempts,
        census.credential_attempts,
        census.futex_wake_attempts,
        census.process_creation_attempts,
        census.child_wait_attempts,
        census.process_identity_attempts,
        census.memory_state_attempts,
        census.thread_state_attempts,
        census.lifecycle_exit_attempts,
        census.statx_attempts,
        census.xattr_attempts,
        census.readlink_attempts,
        census.access_attempts,
        census.getcwd_attempts,
        census.cwd_change_attempts,
        census.path_removal_attempts,
        census.directory_creation_attempts,
        census.path_rename_attempts,
        census.path_mode_change_attempts,
        census.descriptor_timestamp_change_attempts,
        census.symbolic_link_creation_attempts,
        census.filesystem_status_attempts,
        census.read_attempts,
        census.getdents64_attempts,
        census.mmap_attempts,
        census.write_attempts,
        census.unmodeled_syscall_attempts,
        census.syscall_entries,
        census.syscall_exits,
        census.terminal_syscalls,
    ] {
        footer[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        offset += 8;
    }
    footer[offset..offset + 16].copy_from_slice(&identity.capture_id);
    offset += 16;
    footer[offset..offset + 32].copy_from_slice(&identity.context_digest);
    footer
}

#[derive(Debug, Clone, Copy)]
struct Footer {
    identity: ProcessCaptureStreamIdentity,
    root: CapturedProcessId,
    records: u64,
    processes: u64,
    payload_bytes: u64,
    census: ProcessCaptureStreamCensus,
}

fn decode_footer(tag: u8, remainder: &[u8; FOOTER_BYTES as usize - 1]) -> Result<Footer> {
    if tag != FRAME_TAG_FOOTER || remainder[..3] != [0; 3] {
        anyhow::bail!("process capture stream has an invalid footer");
    }
    let mut offset = 3;
    let mut next_u64 = || {
        let value = u64::from_le_bytes(remainder[offset..offset + 8].try_into().unwrap());
        offset += 8;
        value
    };
    let root = next_u64();
    let records = next_u64();
    let processes = next_u64();
    let payload_bytes = next_u64();
    let census = ProcessCaptureStreamCensus {
        invocations: next_u64(),
        exec_attempts: next_u64(),
        open_attempts: next_u64(),
        newfstatat_attempts: next_u64(),
        fstat_attempts: next_u64(),
        getrandom_attempts: next_u64(),
        runtime_observation_attempts: next_u64(),
        prlimit_attempts: next_u64(),
        close_attempts: next_u64(),
        seek_attempts: next_u64(),
        file_advice_attempts: next_u64(),
        fcntl_attempts: next_u64(),
        ioctl_attempts: next_u64(),
        pipe_attempts: next_u64(),
        epoll_create_attempts: next_u64(),
        socket_create_attempts: next_u64(),
        socket_connect_attempts: next_u64(),
        dup2_attempts: next_u64(),
        signal_state_attempts: next_u64(),
        signal_deliveries: next_u64(),
        umask_attempts: next_u64(),
        credential_attempts: next_u64(),
        futex_wake_attempts: next_u64(),
        process_creation_attempts: next_u64(),
        child_wait_attempts: next_u64(),
        process_identity_attempts: next_u64(),
        memory_state_attempts: next_u64(),
        thread_state_attempts: next_u64(),
        lifecycle_exit_attempts: next_u64(),
        statx_attempts: next_u64(),
        xattr_attempts: next_u64(),
        readlink_attempts: next_u64(),
        access_attempts: next_u64(),
        getcwd_attempts: next_u64(),
        cwd_change_attempts: next_u64(),
        path_removal_attempts: next_u64(),
        directory_creation_attempts: next_u64(),
        path_rename_attempts: next_u64(),
        path_mode_change_attempts: next_u64(),
        descriptor_timestamp_change_attempts: next_u64(),
        symbolic_link_creation_attempts: next_u64(),
        filesystem_status_attempts: next_u64(),
        read_attempts: next_u64(),
        getdents64_attempts: next_u64(),
        mmap_attempts: next_u64(),
        write_attempts: next_u64(),
        unmodeled_syscall_attempts: next_u64(),
        syscall_entries: next_u64(),
        syscall_exits: next_u64(),
        terminal_syscalls: next_u64(),
    };
    let capture_id = remainder[offset..offset + 16].try_into().unwrap();
    offset += 16;
    let context_digest = remainder[offset..offset + 32].try_into().unwrap();
    Ok(Footer {
        identity: ProcessCaptureStreamIdentity {
            capture_id,
            context_digest,
        },
        root,
        records,
        processes,
        payload_bytes,
        census,
    })
}

fn encode_trailer(committed_len: u64, digest: [u8; 32]) -> [u8; TRAILER_BYTES as usize] {
    let mut trailer = [0u8; TRAILER_BYTES as usize];
    trailer[..8].copy_from_slice(&TRAILER_MAGIC);
    trailer[8..16].copy_from_slice(&committed_len.to_le_bytes());
    trailer[16..].copy_from_slice(&digest);
    trailer
}

fn validate_footer_census(
    receipt: &ProcessCaptureReceipt,
    actual: ProcessCaptureStreamCensus,
) -> Result<()> {
    let expected = ProcessCaptureStreamCensus {
        invocations: receipt.processes.iter().try_fold(0u64, |total, process| {
            total
                .checked_add(u64::try_from(process.invocations.len())?)
                .context("process invocation census overflowed")
        })?,
        exec_attempts: receipt.exec_syscalls()?,
        open_attempts: receipt.open_syscalls()?,
        newfstatat_attempts: receipt.newfstatat_syscalls()?,
        fstat_attempts: receipt.fstat_syscalls()?,
        getrandom_attempts: receipt.getrandom_syscalls()?,
        runtime_observation_attempts: receipt.runtime_observation_syscalls()?,
        prlimit_attempts: receipt.prlimit_syscalls()?,
        close_attempts: receipt.close_syscalls()?,
        seek_attempts: receipt.seek_syscalls()?,
        file_advice_attempts: receipt.file_advice_syscalls()?,
        fcntl_attempts: receipt.fcntl_syscalls()?,
        ioctl_attempts: receipt.ioctl_syscalls()?,
        pipe_attempts: receipt.pipe_syscalls()?,
        epoll_create_attempts: receipt.epoll_create_syscalls()?,
        socket_create_attempts: receipt.socket_create_syscalls()?,
        socket_connect_attempts: receipt.socket_connect_syscalls()?,
        dup2_attempts: receipt.dup2_syscalls()?,
        signal_state_attempts: receipt.signal_state_syscalls()?,
        signal_deliveries: receipt.signal_deliveries()?,
        umask_attempts: receipt.umask_syscalls()?,
        credential_attempts: receipt.credential_syscalls()?,
        futex_wake_attempts: receipt.futex_wake_syscalls()?,
        process_creation_attempts: receipt.process_creation_syscalls()?,
        child_wait_attempts: receipt.child_wait_syscalls()?,
        process_identity_attempts: receipt.process_identity_syscalls()?,
        memory_state_attempts: receipt.memory_state_syscalls()?,
        thread_state_attempts: receipt.thread_state_syscalls()?,
        lifecycle_exit_attempts: receipt.lifecycle_exit_syscalls()?,
        statx_attempts: receipt.statx_syscalls()?,
        xattr_attempts: receipt.xattr_syscalls()?,
        readlink_attempts: receipt.readlink_syscalls()?,
        access_attempts: receipt.access_syscalls()?,
        getcwd_attempts: receipt.getcwd_syscalls()?,
        cwd_change_attempts: receipt.cwd_change_syscalls()?,
        path_removal_attempts: receipt.path_removal_syscalls()?,
        directory_creation_attempts: receipt.directory_creation_syscalls()?,
        path_rename_attempts: receipt.path_rename_syscalls()?,
        path_mode_change_attempts: receipt.path_mode_change_syscalls()?,
        descriptor_timestamp_change_attempts: receipt.descriptor_timestamp_change_syscalls()?,
        symbolic_link_creation_attempts: receipt.symbolic_link_creation_syscalls()?,
        filesystem_status_attempts: receipt.filesystem_status_syscalls()?,
        read_attempts: receipt.read_syscalls()?,
        getdents64_attempts: receipt.getdents64_syscalls()?,
        mmap_attempts: receipt.mmap_syscalls()?,
        write_attempts: receipt.write_syscalls()?,
        unmodeled_syscall_attempts: receipt.unmodeled_syscalls()?,
        syscall_entries: receipt.syscall_entries()?,
        syscall_exits: receipt.syscall_exits()?,
        terminal_syscalls: receipt.terminal_syscalls()?,
    };
    if expected != actual {
        anyhow::bail!("process capture stream footer disagrees with its receipt census");
    }
    Ok(())
}

fn validate_private_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let metadata =
        std::fs::symlink_metadata(parent).context("stat process capture stream parent")?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o077 != 0
    {
        anyhow::bail!("process capture stream parent is not owner-private");
    }
    Ok(())
}

fn validate_private_file(file: &File) -> Result<()> {
    let metadata = file.metadata().context("stat process capture stream")?;
    if !metadata.file_type().is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || metadata.permissions().mode() & 0o077 != 0
    {
        anyhow::bail!("process capture stream is not a private regular file");
    }
    Ok(())
}

fn read_array<const N: usize>(file: &mut File, context: &'static str) -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    file.read_exact(&mut bytes).with_context(|| context)?;
    Ok(bytes)
}

fn checked_add(left: u64, right: u64, what: &'static str) -> Result<u64> {
    left.checked_add(right)
        .with_context(|| format!("process capture stream {what} overflowed"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CapturedAccessAttempt, CapturedAccessOutcome, CapturedAccessRequest,
        CapturedAccessSystemCall, CapturedCwdChangeAttempt, CapturedCwdChangeOutcome,
        CapturedCwdChangeRequest, CapturedCwdChangeSystemCall, CapturedDescriptorIdentity,
        CapturedDescriptorKind, CapturedDirectoryValue, CapturedExecAttempt,
        CapturedExecFileMapping, CapturedExecOutcome, CapturedExecRequest, CapturedFstatAttempt,
        CapturedGetcwdAttempt, CapturedGetcwdOutcome, CapturedGetcwdRequest,
        CapturedGetdents64Attempt, CapturedGetdents64Outcome, CapturedGetdents64Request,
        CapturedMappedFileIdentity, CapturedMappedFileValue, CapturedMmapAttempt,
        CapturedMmapOutcome, CapturedMmapRequest, CapturedNewfstatatAttempt,
        CapturedNewfstatatOutcome, CapturedNewfstatatRequest, CapturedOpenAttempt,
        CapturedOpenOutcome, CapturedOpenRequest, CapturedOpenSystemCall, CapturedProcess,
        CapturedReadAttempt, CapturedReadOutcome, CapturedReadRequest, CapturedReadSystemCall,
        CapturedReadlinkAttempt, CapturedReadlinkOutcome, CapturedReadlinkRequest,
        CapturedReadlinkSystemCall, CapturedStatxMetadata, CapturedStatxOutcome,
        CapturedRuntimeObservationOutcome, CapturedRuntimeObservationRequest, CapturedStatxRequest,
        CapturedStatxTimestamp, CapturedWriteAttempt, CapturedWriteOutcome, CapturedWriteRequest,
        CapturedWriteSystemCall, CapturedXattrAttempt, CapturedXattrOutcome, CapturedXattrRequest,
        CapturedXattrSystemCall,
    };
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::process::{Command, Stdio};

    const IDENTITY: ProcessCaptureStreamIdentity = ProcessCaptureStreamIdentity {
        capture_id: [7; 16],
        context_digest: [9; 32],
    };

    fn limits() -> ProcessCaptureStreamLimits {
        ProcessCaptureStreamLimits::new(16 * 1024 * 1024, 100_000, 10_000, 16 * 1024 * 1024)
            .unwrap()
    }

    fn placeholder_summary() -> ProcessCaptureStreamSummary {
        ProcessCaptureStreamSummary {
            identity: IDENTITY,
            digest: [0; 32],
            record_count: 0,
            process_count: 0,
            payload_bytes: 0,
            stream_bytes: 0,
            census: ProcessCaptureStreamCensus::default(),
        }
    }

    fn private_tempdir() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        temp
    }

    fn fixture_invocation() -> CapturedInvocation {
        CapturedInvocation {
            executable: b"/fixture/tool".to_vec(),
            argv: vec![b"tool".to_vec(), vec![0xff]],
            environment: vec![b"RAW=\xfe".to_vec()],
            working_directory: b"/fixture".to_vec(),
            signal_state: crate::CapturedExecSignalState::default(),
            file_creation_mask: 0o022,
            credentials: crate::CapturedCredentialState::default(),
            file_mappings: vec![CapturedExecFileMapping {
                start: 0x1000,
                end: 0x2000,
                file_offset: 0,
                readable: true,
                writable: false,
                executable: true,
                shared: false,
                identity: CapturedMappedFileIdentity {
                    target: b"/fixture/tool".to_vec(),
                    device: 1,
                    inode: 1,
                    mount_id: 1,
                },
                value: CapturedMappedFileValue::WholeFile {
                    bytes: 7,
                    digest: *blake3::hash(b"fixture").as_bytes(),
                    mode: libc::S_IFREG | 0o755,
                },
            }],
        }
    }

    fn fixture_statx_metadata() -> CapturedStatxMetadata {
        let timestamp = CapturedStatxTimestamp {
            seconds: 1,
            nanoseconds: 2,
            reserved: 0,
        };
        CapturedStatxMetadata {
            mask: 0x17ff,
            block_size: 4096,
            attributes: 0,
            hard_links: 1,
            user: 1000,
            group: 1000,
            mode: libc::S_IFREG as u16 | 0o644,
            spare0: 0,
            inode: 2,
            size: 7,
            blocks: 8,
            attributes_mask: 0,
            access_time: timestamp,
            birth_time: timestamp,
            change_time: timestamp,
            modification_time: timestamp,
            special_device_major: 0,
            special_device_minor: 0,
            device_major: 8,
            device_minor: 1,
            mount_id: 3,
            direct_io_memory_alignment: 0,
            direct_io_offset_alignment: 0,
            subvolume: 0,
            atomic_write_unit_minimum: 0,
            atomic_write_unit_maximum: 0,
            atomic_write_segments_maximum: 0,
            direct_io_read_offset_alignment: 0,
            atomic_write_unit_maximum_optimized: 0,
            spare2: 0,
            spare3: [0; 8],
        }
    }

    fn empty_finish(outcome: CapturedProcessOutcome) -> ProcessFinish {
        ProcessFinish {
            invocations: 0,
            exec_syscalls: 0,
            open_syscalls: 0,
            newfstatat_syscalls: 0,
            fstat_syscalls: 0,
            getrandom_syscalls: 0,
            runtime_observation_syscalls: 0,
            prlimit_syscalls: 0,
            close_syscalls: 0,
            seek_syscalls: 0,
            file_advice_syscalls: 0,
            fcntl_syscalls: 0,
            ioctl_syscalls: 0,
            pipe_syscalls: 0,
            epoll_create_syscalls: 0,
            socket_create_syscalls: 0,
            socket_connect_syscalls: 0,
            dup2_syscalls: 0,
            signal_state_syscalls: 0,
            signal_deliveries: 0,
            umask_syscalls: 0,
            credential_syscalls: 0,
            futex_wake_syscalls: 0,
            process_creation_syscalls: 0,
            child_wait_syscalls: 0,
            process_identity_syscalls: 0,
            memory_state_syscalls: 0,
            thread_state_syscalls: 0,
            lifecycle_exit_syscalls: 0,
            statx_syscalls: 0,
            xattr_syscalls: 0,
            readlink_syscalls: 0,
            access_syscalls: 0,
            getcwd_syscalls: 0,
            cwd_change_syscalls: 0,
            path_removal_syscalls: 0,
            directory_creation_syscalls: 0,
            path_rename_syscalls: 0,
            path_mode_change_syscalls: 0,
            descriptor_timestamp_change_syscalls: 0,
            symbolic_link_creation_syscalls: 0,
            filesystem_status_syscalls: 0,
            read_syscalls: 0,
            getdents64_syscalls: 0,
            mmap_syscalls: 0,
            write_syscalls: 0,
            unmodeled_syscalls: 0,
            syscall_entries: 0,
            syscall_exits: 0,
            terminal_syscalls: 0,
            outcome,
        }
    }

    fn all_event_fixture_receipt() -> ProcessCaptureReceipt {
        ProcessCaptureReceipt::new(
            1,
            vec![CapturedProcess {
                id: 1,
                parent: None,
                invocations: vec![fixture_invocation()],
                exec_attempts: vec![CapturedExecAttempt {
                    syscall_index: 0,
                    request: CapturedExecRequest {
                        executable: b"/fixture/missing".to_vec(),
                        directory: None,
                        argv: vec![b"missing".to_vec()],
                        environment: vec![b"A=b".to_vec()],
                        flags: 0,
                    },
                    outcome: CapturedExecOutcome::Failed(libc::ENOENT),
                }],
                exec_syscalls: 1,
                open_attempts: vec![CapturedOpenAttempt {
                    syscall_index: 1,
                    request: CapturedOpenRequest {
                        system_call: CapturedOpenSystemCall::Open,
                        path: b"/fixture/missing".to_vec(),
                        directory: None,
                        flags: 0,
                        mode: 0,
                        resolve: 0,
                    },
                    outcome: CapturedOpenOutcome::Failed(libc::ENOENT),
                }],
                open_syscalls: 1,
                newfstatat_attempts: vec![CapturedNewfstatatAttempt {
                    syscall_index: 6,
                    request: CapturedNewfstatatRequest {
                        directory_descriptor: libc::AT_FDCWD,
                        path: b"/fixture/missing".to_vec(),
                        path_is_null: false,
                        base: None,
                        flags: 0,
                    },
                    outcome: CapturedNewfstatatOutcome::Failed(libc::ENOENT),
                }],
                newfstatat_syscalls: 1,
                fstat_attempts: vec![CapturedFstatAttempt {
                    syscall_index: 20,
                    request: crate::CapturedFstatRequest {
                        descriptor: -1,
                        identity: None,
                        mount_id: None,
                        output_is_null: true,
                    },
                    outcome: crate::CapturedFstatOutcome::Failed(libc::EBADF),
                }],
                fstat_syscalls: 1,
                getrandom_attempts: vec![crate::CapturedGetrandomAttempt {
                    syscall_index: 21,
                    request: crate::CapturedGetrandomRequest {
                        requested_bytes: 0,
                        flags: 0,
                        output_is_null: true,
                    },
                    outcome: crate::CapturedGetrandomOutcome::Bytes {
                        returned_bytes: 0,
                        digest: *blake3::hash(&[]).as_bytes(),
                    },
                }],
                getrandom_syscalls: 1,
                runtime_observation_attempts: vec![crate::CapturedRuntimeObservationAttempt {
                    syscall_index: 39,
                    request: crate::CapturedRuntimeObservationRequest::CallingThreadAffinity {
                        capacity: 128,
                        output_is_null: false,
                    },
                    outcome: crate::CapturedRuntimeObservationOutcome::CpuAffinity(vec![1; 16]),
                }],
                runtime_observation_syscalls: 1,
                prlimit_attempts: vec![crate::CapturedPrlimitAttempt {
                    syscall_index: 22,
                    request: crate::CapturedPrlimitRequest {
                        process: 0,
                        resource: 3,
                        new_limit_is_null: false,
                        old_limit_is_null: true,
                    },
                    outcome: crate::CapturedPrlimitOutcome::Mutated {
                        requested: crate::CapturedResourceLimit {
                            current: 64 * 1024 * 1024,
                            maximum: u64::MAX,
                        },
                        previous: None,
                    },
                }],
                prlimit_syscalls: 1,
                close_attempts: vec![crate::CapturedCloseAttempt {
                    syscall_index: 23,
                    request: crate::CapturedCloseRequest {
                        descriptor: 8,
                        identity: Some(crate::CapturedDescriptorIdentity {
                            target: b"/fixture/close".to_vec(),
                            device: 9,
                            inode: 10,
                            kind: crate::CapturedDescriptorKind::RegularFile,
                        }),
                        mount_id: Some(11),
                    },
                    outcome: crate::CapturedCloseOutcome::Closed {
                        file_value: Some(crate::CapturedMappedFileValue::WholeFile {
                            bytes: 6,
                            digest: *blake3::hash(b"closed").as_bytes(),
                            mode: libc::S_IFREG | 0o644,
                        }),
                    },
                }],
                close_syscalls: 1,
                seek_attempts: vec![crate::CapturedSeekAttempt {
                    syscall_index: 27,
                    request: crate::CapturedSeekRequest {
                        descriptor: -1,
                        identity: None,
                        mount_id: None,
                        offset: -1,
                        whence: libc::SEEK_SET as u64,
                    },
                    outcome: crate::CapturedSeekOutcome::Failed(libc::EBADF),
                }],
                seek_syscalls: 1,
                file_advice_attempts: vec![crate::CapturedFileAdviceAttempt {
                    syscall_index: 40,
                    request: crate::CapturedFileAdviceRequest {
                        descriptor: 8,
                        identity: Some(crate::CapturedDescriptorIdentity {
                            target: b"/fixture/advice".to_vec(),
                            device: 9,
                            inode: 10,
                            kind: crate::CapturedDescriptorKind::RegularFile,
                        }),
                        mount_id: Some(11),
                        offset: 0,
                        length: 0,
                        advice: libc::POSIX_FADV_SEQUENTIAL,
                    },
                    outcome: crate::CapturedFileAdviceOutcome::Completed,
                }],
                file_advice_syscalls: 1,
                fcntl_attempts: vec![crate::CapturedFcntlAttempt {
                    syscall_index: 28,
                    request: crate::CapturedFcntlRequest {
                        descriptor: -1,
                        identity: None,
                        mount_id: None,
                        command: libc::F_GETFD as u32,
                        argument: 0,
                        record_lock: None,
                    },
                    outcome: crate::CapturedFcntlOutcome::Failed(libc::EBADF),
                }],
                fcntl_syscalls: 1,
                ioctl_attempts: vec![crate::CapturedIoctlAttempt {
                    syscall_index: 38,
                    request: crate::CapturedIoctlRequest {
                        descriptor: 9,
                        identity: CapturedDescriptorIdentity {
                            target: b"pipe:[fixture]".to_vec(),
                            device: 7,
                            inode: 8,
                            kind: CapturedDescriptorKind::Fifo,
                        },
                        mount_id: 9,
                        command: libc::TCGETS as u64,
                    },
                    outcome: crate::CapturedIoctlOutcome::Failed(libc::ENOTTY),
                }],
                ioctl_syscalls: 1,
                pipe_attempts: vec![crate::CapturedPipeAttempt {
                    syscall_index: 30,
                    request: crate::CapturedPipeRequest { flags: 0 },
                    outcome: crate::CapturedPipeOutcome::Failed(libc::EFAULT),
                }],
                pipe_syscalls: 1,
                epoll_create_attempts: vec![crate::CapturedEpollCreateAttempt {
                    syscall_index: 41,
                    request: crate::CapturedEpollCreateRequest {
                        flags: libc::EPOLL_CLOEXEC as u32,
                    },
                    outcome: crate::CapturedEpollCreateOutcome::Created {
                        descriptor: 8,
                        identity: CapturedDescriptorIdentity {
                            target: b"anon_inode:[eventpoll]".to_vec(),
                            device: 16,
                            inode: 60,
                            kind: CapturedDescriptorKind::Other,
                        },
                        mount_id: 18,
                    },
                }],
                epoll_create_syscalls: 1,
                socket_create_attempts: vec![
                    crate::CapturedSocketCreateAttempt {
                        syscall_index: 42,
                        request: crate::CapturedSocketCreateRequest {
                            domain: libc::AF_UNIX,
                            socket_type: libc::SOCK_STREAM
                                | libc::SOCK_CLOEXEC,
                            protocol: 0,
                        },
                        outcome: crate::CapturedSocketCreateOutcome::Created {
                            descriptor: 9,
                            identity: CapturedDescriptorIdentity {
                                target: b"socket:[61]".to_vec(),
                                device: 16,
                                inode: 61,
                                kind: CapturedDescriptorKind::Socket,
                            },
                            mount_id: 18,
                        },
                    },
                ],
                socket_create_syscalls: 1,
                socket_connect_attempts: vec![
                    crate::CapturedSocketConnectAttempt {
                        syscall_index: 43,
                        request: crate::CapturedSocketConnectRequest {
                            descriptor: 9,
                            identity: CapturedDescriptorIdentity {
                                target: b"socket:[61]".to_vec(),
                                device: 16,
                                inode: 61,
                                kind: CapturedDescriptorKind::Socket,
                            },
                            mount_id: 18,
                            address_length: 26,
                            address:
                                crate::CapturedUnixSocketAddress::Pathname {
                                    path: b"/fixture/missing.socket".to_vec(),
                                    terminated: true,
                                },
                        },
                        outcome: crate::CapturedSocketConnectOutcome::Failed {
                            errno: libc::ENOENT,
                            identity: CapturedDescriptorIdentity {
                                target: b"socket:[61]".to_vec(),
                                device: 16,
                                inode: 61,
                                kind: CapturedDescriptorKind::Socket,
                            },
                            mount_id: 18,
                        },
                    },
                ],
                socket_connect_syscalls: 1,
                dup2_attempts: vec![crate::CapturedDup2Attempt {
                    syscall_index: 31,
                    request: crate::CapturedDup2Request {
                        system_call: crate::CapturedDescriptorDuplicationSystemCall::Dup3,
                        source_descriptor: 3,
                        source_identity: Some(CapturedDescriptorIdentity {
                            target: b"/fixture/dup-source".to_vec(),
                            device: 5,
                            inode: 6,
                            kind: CapturedDescriptorKind::RegularFile,
                        }),
                        source_mount_id: Some(7),
                        target_descriptor: 4,
                        target_identity: None,
                        target_mount_id: None,
                    },
                    outcome: crate::CapturedDup2Outcome::Duplicated {
                        descriptor: 4,
                        identity: CapturedDescriptorIdentity {
                            target: b"/fixture/dup-source".to_vec(),
                            device: 5,
                            inode: 6,
                            kind: CapturedDescriptorKind::RegularFile,
                        },
                        mount_id: 7,
                        replaced_file_value: None,
                    },
                }],
                dup2_syscalls: 1,
                signal_state_attempts: vec![crate::CapturedSignalStateAttempt {
                    syscall_index: 29,
                    request: crate::CapturedSignalStateRequest::Mask {
                        operation: libc::SIG_SETMASK,
                        mask: None,
                        old_mask_is_null: false,
                        signal_set_bytes: 8,
                    },
                    outcome: crate::CapturedSignalStateOutcome::MaskReturned {
                        previous: Some(vec![0; 8]),
                    },
                }],
                signal_state_syscalls: 1,
                signal_deliveries: vec![crate::CapturedSignalDelivery {
                    signal: libc::SIGCHLD as u32,
                    code: libc::CLD_EXITED,
                    errno: 0,
                    source: crate::CapturedSignalSource::Kernel,
                    child_status: Some(0),
                    syscall_entries: 30,
                    active_syscall: Some(29),
                    disposition: crate::CapturedSignalDisposition::Default,
                }],
                umask_attempts: vec![crate::CapturedUmaskAttempt {
                    syscall_index: 32,
                    requested: 0o077,
                    outcome: crate::CapturedUmaskOutcome::Changed(0o022),
                }],
                umask_syscalls: 1,
                credential_attempts: vec![crate::CapturedCredentialAttempt {
                    syscall_index: 33,
                    request: crate::CapturedCredentialRequest::CapabilityBoundingSetRead {
                        capability: 32,
                    },
                    outcome:
                        crate::CapturedCredentialOutcome::CapabilityBoundingSetMembership(true),
                }],
                credential_syscalls: 1,
                futex_wake_attempts: vec![crate::CapturedFutexWakeAttempt {
                    syscall_index: 34,
                    request: crate::CapturedFutexWakeRequest {
                        address: 0x1000,
                        maximum_waiters: 1,
                    },
                    outcome: crate::CapturedFutexWakeOutcome::Woken(0),
                }],
                futex_wake_syscalls: 1,
                process_creation_attempts: vec![crate::CapturedProcessCreationAttempt {
                    syscall_index: 35,
                    request: crate::CapturedProcessCreationRequest::Vfork,
                    outcome: crate::CapturedProcessCreationOutcome::Failed(libc::EAGAIN),
                }],
                process_creation_syscalls: 1,
                child_wait_attempts: vec![crate::CapturedChildWaitAttempt {
                    syscall_index: 36,
                    request: crate::CapturedChildWaitRequest {
                        selector: crate::CapturedChildWaitSelector::AnyChild,
                        nonblocking: true,
                        status_is_null: false,
                    },
                    outcome: crate::CapturedChildWaitOutcome::Failed(libc::ECHILD),
                }],
                child_wait_syscalls: 1,
                process_identity_attempts: vec![crate::CapturedProcessIdentityAttempt {
                    syscall_index: 37,
                    request: crate::CapturedProcessIdentityRequest::Gettid,
                    outcome: crate::CapturedProcessIdentityOutcome::Identity(1),
                }],
                process_identity_syscalls: 1,
                memory_state_attempts: vec![crate::CapturedMemoryStateAttempt {
                    syscall_index: 24,
                    request: crate::CapturedMemoryStateRequest::Mprotect {
                        address: 1,
                        length: 4096,
                        protection: 0,
                    },
                    outcome: crate::CapturedMemoryStateOutcome::Failed(libc::EINVAL),
                }],
                memory_state_syscalls: 1,
                thread_state_attempts: vec![crate::CapturedThreadStateAttempt {
                    syscall_index: 25,
                    request: crate::CapturedThreadStateRequest::Rseq {
                        address: 0,
                        length: 0,
                        flags: 0,
                        signature: 0,
                    },
                    outcome: crate::CapturedThreadStateOutcome::Failed(libc::EINVAL),
                }],
                thread_state_syscalls: 1,
                lifecycle_exit_attempts: vec![crate::CapturedLifecycleExitAttempt {
                    syscall_index: 44,
                    request: crate::CapturedLifecycleExitRequest {
                        system_call: crate::CapturedLifecycleExitSystemCall::ExitGroup,
                        status: 0,
                    },
                    open_file_values: vec![crate::CapturedExitFileValue {
                        descriptor: 12,
                        identity: crate::CapturedDescriptorIdentity {
                            target: b"/fixture/exit".to_vec(),
                            device: 13,
                            inode: 14,
                            kind: crate::CapturedDescriptorKind::RegularFile,
                        },
                        mount_id: 15,
                        value: crate::CapturedMappedFileValue::WholeFile {
                            bytes: 4,
                            digest: *blake3::hash(b"exit").as_bytes(),
                            mode: libc::S_IFREG | 0o600,
                        },
                    }],
                    outcome: CapturedProcessOutcome::Exited(0),
                }],
                lifecycle_exit_syscalls: 1,
                statx_attempts: vec![CapturedStatxAttempt {
                    syscall_index: 18,
                    request: CapturedStatxRequest {
                        directory_descriptor: libc::AT_FDCWD,
                        path: b"/fixture/value".to_vec(),
                        path_is_null: false,
                        path_terminated: true,
                        base: None,
                        descriptor_identity: None,
                        descriptor_mount_id: None,
                        flags: 0,
                        mask: 0x7ff,
                        output_is_null: false,
                    },
                    outcome: CapturedStatxOutcome::Metadata(Box::new(fixture_statx_metadata())),
                }],
                statx_syscalls: 1,
                xattr_attempts: vec![CapturedXattrAttempt {
                    syscall_index: 19,
                    request: CapturedXattrRequest {
                        system_call: CapturedXattrSystemCall::Getxattr,
                        path: b"/fixture/value".to_vec(),
                        path_is_null: false,
                        path_terminated: true,
                        base: None,
                        descriptor: None,
                        descriptor_identity: None,
                        descriptor_mount_id: None,
                        name: b"user.rush".to_vec(),
                        name_is_null: false,
                        name_terminated: true,
                        requested_bytes: 0,
                        value_is_null: true,
                        set_value: None,
                        flags: 0,
                    },
                    outcome: CapturedXattrOutcome::Failed(libc::ENODATA),
                }],
                xattr_syscalls: 1,
                readlink_attempts: vec![CapturedReadlinkAttempt {
                    syscall_index: 7,
                    request: CapturedReadlinkRequest {
                        system_call: CapturedReadlinkSystemCall::ReadlinkAt,
                        directory_descriptor: libc::AT_FDCWD,
                        path: b"/fixture/missing-link".to_vec(),
                        path_is_null: false,
                        base: None,
                        requested_bytes: 32,
                    },
                    outcome: CapturedReadlinkOutcome::Failed(libc::ENOENT),
                }],
                readlink_syscalls: 1,
                access_attempts: vec![CapturedAccessAttempt {
                    syscall_index: 8,
                    request: CapturedAccessRequest {
                        system_call: CapturedAccessSystemCall::Faccessat2,
                        directory_descriptor: libc::AT_FDCWD,
                        path: b"/fixture/missing-access".to_vec(),
                        path_is_null: false,
                        base: None,
                        mode: libc::F_OK as u32,
                        flags: 0,
                    },
                    outcome: CapturedAccessOutcome::Failed(libc::ENOENT),
                }],
                access_syscalls: 1,
                getcwd_attempts: vec![CapturedGetcwdAttempt {
                    syscall_index: 9,
                    request: CapturedGetcwdRequest {
                        requested_bytes: 4096,
                    },
                    outcome: CapturedGetcwdOutcome::Directory(b"/fixture".to_vec()),
                }],
                getcwd_syscalls: 1,
                cwd_change_attempts: vec![CapturedCwdChangeAttempt {
                    syscall_index: 10,
                    request: CapturedCwdChangeRequest {
                        system_call: CapturedCwdChangeSystemCall::Chdir,
                        path: b"next".to_vec(),
                        path_is_null: false,
                        directory_descriptor: libc::AT_FDCWD,
                        descriptor_target: None,
                        prior_working_directory: b"/fixture".to_vec(),
                    },
                    outcome: CapturedCwdChangeOutcome::Changed(b"/fixture/next".to_vec()),
                }],
                cwd_change_syscalls: 1,
                path_removal_attempts: vec![CapturedPathRemovalAttempt {
                    syscall_index: 11,
                    request: crate::CapturedPathRemovalRequest {
                        system_call: crate::CapturedPathRemovalSystemCall::UnlinkAt,
                        directory_descriptor: libc::AT_FDCWD,
                        path: b"/fixture/obsolete".to_vec(),
                        path_is_null: false,
                        base: None,
                        flags: 0,
                    },
                    outcome: crate::CapturedPathRemovalOutcome::Removed,
                }],
                path_removal_syscalls: 1,
                directory_creation_attempts: vec![CapturedDirectoryCreationAttempt {
                    syscall_index: 12,
                    request: crate::CapturedDirectoryCreationRequest {
                        system_call: crate::CapturedDirectoryCreationSystemCall::MkdirAt,
                        directory_descriptor: libc::AT_FDCWD,
                        path: b"/fixture/new-directory".to_vec(),
                        path_is_null: false,
                        base: None,
                        mode: 0o755,
                    },
                    outcome: crate::CapturedDirectoryCreationOutcome::Created,
                }],
                directory_creation_syscalls: 1,
                path_rename_attempts: vec![CapturedPathRenameAttempt {
                    syscall_index: 14,
                    request: crate::CapturedPathRenameRequest {
                        system_call: crate::CapturedPathRenameSystemCall::RenameAt2,
                        old_directory_descriptor: libc::AT_FDCWD,
                        old_path: b"/fixture/old-name".to_vec(),
                        old_path_is_null: false,
                        old_path_terminated: true,
                        old_base: None,
                        new_directory_descriptor: libc::AT_FDCWD,
                        new_path: b"/fixture/new-name".to_vec(),
                        new_path_is_null: false,
                        new_path_terminated: true,
                        new_base: None,
                        flags: 1,
                    },
                    outcome: crate::CapturedPathRenameOutcome::Completed,
                }],
                path_rename_syscalls: 1,
                path_mode_change_attempts: vec![CapturedPathModeChangeAttempt {
                    syscall_index: 15,
                    request: crate::CapturedPathModeChangeRequest {
                        system_call: crate::CapturedPathModeChangeSystemCall::Fchmod,
                        descriptor: Some(5),
                        descriptor_identity: Some(crate::CapturedDescriptorIdentity {
                            target: b"/fixture/mode".to_vec(),
                            device: 17,
                            inode: 18,
                            kind: crate::CapturedDescriptorKind::RegularFile,
                        }),
                        descriptor_mount_id: Some(19),
                        directory_descriptor: libc::AT_FDCWD,
                        path: Vec::new(),
                        path_is_null: true,
                        path_terminated: false,
                        base: None,
                        mode: 0o755,
                    },
                    outcome: crate::CapturedPathModeChangeOutcome::Changed,
                }],
                path_mode_change_syscalls: 1,
                descriptor_timestamp_change_attempts: vec![
                    CapturedDescriptorTimestampChangeAttempt {
                        syscall_index: 16,
                        request: crate::CapturedDescriptorTimestampChangeRequest {
                            descriptor: 7,
                            identity: Some(crate::CapturedDescriptorIdentity {
                                target: b"/fixture/mode".to_vec(),
                                device: 1,
                                inode: 2,
                                kind: crate::CapturedDescriptorKind::RegularFile,
                            }),
                            mount_id: Some(3),
                            times_is_null: true,
                            times: None,
                            flags: 0,
                        },
                        outcome: crate::CapturedDescriptorTimestampChangeOutcome::Changed,
                    },
                ],
                descriptor_timestamp_change_syscalls: 1,
                symbolic_link_creation_attempts: vec![CapturedSymbolicLinkCreationAttempt {
                    syscall_index: 17,
                    request: crate::CapturedSymbolicLinkCreationRequest {
                        system_call: crate::CapturedSymbolicLinkCreationSystemCall::SymlinkAt,
                        target: b"relative-target".to_vec(),
                        target_is_null: false,
                        target_terminated: true,
                        directory_descriptor: libc::AT_FDCWD,
                        link_path: b"/fixture/link".to_vec(),
                        link_path_is_null: false,
                        link_path_terminated: true,
                        base: None,
                    },
                    outcome: crate::CapturedSymbolicLinkCreationOutcome::Created,
                }],
                symbolic_link_creation_syscalls: 1,
                filesystem_status_attempts: vec![CapturedFilesystemStatusAttempt {
                    syscall_index: 13,
                    request: crate::CapturedFilesystemStatusRequest {
                        system_call: crate::CapturedFilesystemStatusSystemCall::Statfs,
                        descriptor: libc::AT_FDCWD,
                        path: b"/fixture".to_vec(),
                        path_is_null: false,
                        path_terminated: true,
                        output_is_null: false,
                        base: None,
                        descriptor_identity: None,
                        descriptor_mount_id: None,
                    },
                    outcome: crate::CapturedFilesystemStatusOutcome::Observed(
                        crate::CapturedFilesystemStatus {
                            filesystem_type: 0x0102_1994,
                            block_size: 4096,
                            blocks: 1000,
                            blocks_free: 500,
                            blocks_available: 400,
                            files: 200,
                            files_free: 100,
                            filesystem_id: [17, -23],
                            maximum_name_length: 255,
                            fragment_size: 4096,
                            mount_flags: 0x1020,
                            reserved_words: [-1, 2, -3, 4],
                        },
                    ),
                }],
                filesystem_status_syscalls: 1,
                read_attempts: vec![CapturedReadAttempt {
                    syscall_index: 2,
                    request: CapturedReadRequest {
                        system_call: CapturedReadSystemCall::Read,
                        descriptor: -1,
                        identity: None,
                        mount_id: None,
                        uses_descriptor_offset: true,
                        offset: None,
                        buffer_lengths: vec![1],
                        requested_bytes: 1,
                        flags: 0,
                    },
                    outcome: CapturedReadOutcome::Failed(libc::EBADF),
                }],
                read_syscalls: 1,
                getdents64_attempts: vec![CapturedGetdents64Attempt {
                    syscall_index: 3,
                    request: CapturedGetdents64Request {
                        descriptor: 3,
                        identity: Some(CapturedDescriptorIdentity {
                            target: b"/fixture/directory".to_vec(),
                            device: 1,
                            inode: 2,
                            kind: CapturedDescriptorKind::Directory,
                        }),
                        mount_id: Some(1),
                        position_before: Some(0),
                        requested_bytes: 1,
                    },
                    outcome: CapturedGetdents64Outcome::Enumerated {
                        returned_bytes: 0,
                        position_after: 0,
                        entries: Vec::new(),
                        directory_value: CapturedDirectoryValue::Complete {
                            entries: 2,
                            digest: [7; 32],
                            members: vec![b"fixture".to_vec()],
                        },
                    },
                }],
                getdents64_syscalls: 1,
                mmap_attempts: vec![CapturedMmapAttempt {
                    syscall_index: 4,
                    request: CapturedMmapRequest {
                        address: 0,
                        length: 4096,
                        protection: 1,
                        flags: 0x22,
                        descriptor: -1,
                        identity: None,
                        mount_id: None,
                        offset: 0,
                    },
                    outcome: CapturedMmapOutcome::Failed(libc::ENOMEM),
                }],
                mmap_syscalls: 1,
                write_attempts: vec![CapturedWriteAttempt {
                    syscall_index: 5,
                    request: CapturedWriteRequest {
                        system_call: CapturedWriteSystemCall::Write,
                        descriptor: -1,
                        identity: None,
                        mount_id: None,
                        uses_descriptor_offset: true,
                        offset: None,
                        requested_bytes: 1,
                        append: false,
                    },
                    outcome: CapturedWriteOutcome::Failed(libc::EBADF),
                }],
                write_syscalls: 1,
                unmodeled_syscall_attempts: vec![CapturedUnmodeledSyscallAttempt {
                    syscall_index: 26,
                    number: 999,
                    arguments: [1, 2, 3, 4, 5, 6],
                    outcome: crate::CapturedUnmodeledSyscallOutcome::Returned {
                        return_value: -libc::ENOSYS as i64,
                        is_error: true,
                    },
                }],
                unmodeled_syscalls: 1,
                syscall_entries: 45,
                syscall_exits: 44,
                terminal_syscalls: 1,
                outcome: CapturedProcessOutcome::Exited(0),
            }],
        )
        .unwrap()
    }

    fn rich_command() -> Command {
        let script = r#"
import ctypes
import fcntl
import mmap
import os
import select
import socket
import sys
import tempfile
libc = ctypes.CDLL(None, use_errno=True)
libc.syscall.restype = ctypes.c_long
assert isinstance(os.getgroups(), list)
sysinfo_result = ctypes.create_string_buffer(256)
assert libc.syscall(int(sys.argv[4]), ctypes.byref(sysinfo_result)) == 0
uname_result = ctypes.create_string_buffer(512)
assert libc.syscall(int(sys.argv[5]), ctypes.byref(uname_result)) == 0
old_umask = libc.umask(0)
assert 0 <= old_umask <= 0o777
assert libc.umask(old_umask) == 0
d = tempfile.mkdtemp()
p = os.path.join(d, "value")
fd = os.open(p, os.O_CREAT | os.O_RDWR, 0o600)
os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_SEQUENTIAL)
assert os.lseek(fd, 0, os.SEEK_SET) == 0
assert fcntl.fcntl(fd, fcntl.F_GETFL) >= 0
fstat_result = ctypes.create_string_buffer(512)
assert libc.syscall(int(sys.argv[1]), fd, ctypes.byref(fstat_result)) == 0
statx_result = ctypes.create_string_buffer(256)
assert libc.statx(
    -100, os.fsencode(p), 0x800, 0x7ff, ctypes.byref(statx_result)
) == 0
os.setxattr(p, "user.rush", b"x")
xattr_result = ctypes.create_string_buffer(1)
assert libc.getxattr(os.fsencode(p), b"user.rush", xattr_result, 1) == 1
assert libc.lgetxattr(os.fsencode(p), b"user.rush", xattr_result, 1) == 1
link = os.path.join(d, "link")
os.symlink("value", link)
assert os.readlink(link) == "value"
assert os.access(p, os.R_OK)
assert os.getcwd().startswith("/")
cwd_fd = os.open(".", os.O_RDONLY)
os.chdir(d)
os.fchdir(cwd_fd)
os.close(cwd_fd)
os.write(fd, b"abcd")
os.pwrite(fd, b"Z", 1)
os.lseek(fd, 0, os.SEEK_SET)
assert os.read(fd, 4) == b"aZcd"
assert os.pread(fd, 1, 2) == b"c"
with mmap.mmap(fd, 4, access=mmap.ACCESS_READ) as mapping:
    assert mapping[0] == ord("a")
assert "value" in os.listdir(d)
assert os.statvfs(d).f_bsize > 0
renamed = os.path.join(d, "renamed")
os.rename(p, renamed)
p = renamed
os.chmod(p, 0o640)
os.utime(fd, None)
pipe_read, pipe_write = os.pipe2(os.O_CLOEXEC)
event_poll = select.epoll()
event_poll.close()
local_socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
try:
    local_socket.connect(os.path.join(d, "missing.socket"))
except FileNotFoundError:
    pass
else:
    raise AssertionError("unexpected local socket connection")
local_socket.close()
ctypes.set_errno(0)
assert libc.syscall(int(sys.argv[2]), pipe_read, int(sys.argv[3]), 0) == -1
assert ctypes.get_errno() == 25
os.dup2(pipe_read, pipe_read)
duplicate = os.dup(pipe_read)
os.close(duplicate)
os.close(pipe_read)
os.close(pipe_write)
child = os.fork()
if child == 0:
    try:
        os.execve("/fixture/missing", ["missing"], {})
    except OSError:
        os._exit(0)
os.waitpid(child, 0)
spawned = os.posix_spawn("/usr/bin/true", ["true"], {})
os.waitpid(spawned, 0)
os.close(fd)
directory_fd = os.open(d, os.O_RDONLY | os.O_DIRECTORY)
os.unlink("link", dir_fd=directory_fd)
os.close(directory_fd)
os.unlink(p)
os.rmdir(d)
"#;
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "exec python3 -c \"$1\" \"$2\" \"$3\" \"$4\" \"$5\" \"$6\"",
                "sh",
                script,
            ])
            .arg(libc::SYS_fstat.to_string())
            .arg(libc::SYS_ioctl.to_string())
            .arg(libc::TCGETS.to_string())
            .arg(libc::SYS_sysinfo.to_string())
            .arg(libc::SYS_uname.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    fn write_fixture(path: &Path) -> ProcessCaptureStreamSummary {
        let mut sink = StreamCaptureSink::create(path, IDENTITY, limits()).unwrap();
        sink.record(ProcessCaptureEvent::ProcessStarted {
            process: 1,
            parent: None,
        })
        .unwrap();
        sink.record(ProcessCaptureEvent::Invocation {
            process: 1,
            invocation: fixture_invocation(),
        })
        .unwrap();
        sink.record(ProcessCaptureEvent::ProcessStarted {
            process: 2,
            parent: Some(1),
        })
        .unwrap();
        sink.record(ProcessCaptureEvent::ProcessFinished {
            process: 2,
            summary: empty_finish(CapturedProcessOutcome::Exited(0)),
        })
        .unwrap();
        sink.record(ProcessCaptureEvent::ProcessFinished {
            process: 1,
            summary: ProcessFinish {
                invocations: 1,
                ..empty_finish(CapturedProcessOutcome::Exited(0))
            },
        })
        .unwrap();
        sink.finish(1, path).unwrap()
    }

    fn write_private(path: &Path, bytes: &[u8]) {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true).mode(0o600);
        let mut file = options.open(path).unwrap();
        file.write_all(bytes).unwrap();
    }

    fn write_receipt(path: &Path, receipt: &ProcessCaptureReceipt) -> ProcessCaptureStreamSummary {
        let mut sink = StreamCaptureSink::create(path, IDENTITY, limits()).unwrap();
        for process in &receipt.processes {
            sink.record(ProcessCaptureEvent::ProcessStarted {
                process: process.id,
                parent: process.parent,
            })
            .unwrap();
        }
        for process in &receipt.processes {
            for invocation in &process.invocations {
                sink.record(ProcessCaptureEvent::Invocation {
                    process: process.id,
                    invocation: invocation.clone(),
                })
                .unwrap();
            }
            for delivery in &process.signal_deliveries {
                sink.record(ProcessCaptureEvent::SignalDelivery {
                    process: process.id,
                    delivery: *delivery,
                })
                .unwrap();
            }
            let mut attempts = Vec::new();
            for attempt in &process.exec_attempts {
                attempts.push(ProcessCaptureEvent::ExecAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.open_attempts {
                attempts.push(ProcessCaptureEvent::OpenAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.newfstatat_attempts {
                attempts.push(ProcessCaptureEvent::NewfstatatAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.fstat_attempts {
                attempts.push(ProcessCaptureEvent::FstatAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.getrandom_attempts {
                attempts.push(ProcessCaptureEvent::GetrandomAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.runtime_observation_attempts {
                attempts.push(ProcessCaptureEvent::RuntimeObservationAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.prlimit_attempts {
                attempts.push(ProcessCaptureEvent::PrlimitAttempt {
                    process: process.id,
                    attempt: *attempt,
                });
            }
            for attempt in &process.close_attempts {
                attempts.push(ProcessCaptureEvent::CloseAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.seek_attempts {
                attempts.push(ProcessCaptureEvent::SeekAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.file_advice_attempts {
                attempts.push(ProcessCaptureEvent::FileAdviceAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.fcntl_attempts {
                attempts.push(ProcessCaptureEvent::FcntlAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.ioctl_attempts {
                attempts.push(ProcessCaptureEvent::IoctlAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.pipe_attempts {
                attempts.push(ProcessCaptureEvent::PipeAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.epoll_create_attempts {
                attempts.push(ProcessCaptureEvent::EpollCreateAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.socket_create_attempts {
                attempts.push(ProcessCaptureEvent::SocketCreateAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.socket_connect_attempts {
                attempts.push(ProcessCaptureEvent::SocketConnectAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.dup2_attempts {
                attempts.push(ProcessCaptureEvent::Dup2Attempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.signal_state_attempts {
                attempts.push(ProcessCaptureEvent::SignalStateAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.umask_attempts {
                attempts.push(ProcessCaptureEvent::UmaskAttempt {
                    process: process.id,
                    attempt: *attempt,
                });
            }
            for attempt in &process.credential_attempts {
                attempts.push(ProcessCaptureEvent::CredentialAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.futex_wake_attempts {
                attempts.push(ProcessCaptureEvent::FutexWakeAttempt {
                    process: process.id,
                    attempt: *attempt,
                });
            }
            for attempt in &process.process_creation_attempts {
                attempts.push(ProcessCaptureEvent::ProcessCreationAttempt {
                    process: process.id,
                    attempt: *attempt,
                });
            }
            for attempt in &process.child_wait_attempts {
                attempts.push(ProcessCaptureEvent::ChildWaitAttempt {
                    process: process.id,
                    attempt: *attempt,
                });
            }
            for attempt in &process.process_identity_attempts {
                attempts.push(ProcessCaptureEvent::ProcessIdentityAttempt {
                    process: process.id,
                    attempt: *attempt,
                });
            }
            for attempt in &process.memory_state_attempts {
                attempts.push(ProcessCaptureEvent::MemoryStateAttempt {
                    process: process.id,
                    attempt: *attempt,
                });
            }
            for attempt in &process.thread_state_attempts {
                attempts.push(ProcessCaptureEvent::ThreadStateAttempt {
                    process: process.id,
                    attempt: *attempt,
                });
            }
            for attempt in &process.lifecycle_exit_attempts {
                attempts.push(ProcessCaptureEvent::LifecycleExitAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.statx_attempts {
                attempts.push(ProcessCaptureEvent::StatxAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.xattr_attempts {
                attempts.push(ProcessCaptureEvent::XattrAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.readlink_attempts {
                attempts.push(ProcessCaptureEvent::ReadlinkAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.read_attempts {
                attempts.push(ProcessCaptureEvent::ReadAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.access_attempts {
                attempts.push(ProcessCaptureEvent::AccessAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.getcwd_attempts {
                attempts.push(ProcessCaptureEvent::GetcwdAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.cwd_change_attempts {
                attempts.push(ProcessCaptureEvent::CwdChangeAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.path_removal_attempts {
                attempts.push(ProcessCaptureEvent::PathRemovalAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.directory_creation_attempts {
                attempts.push(ProcessCaptureEvent::DirectoryCreationAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.path_rename_attempts {
                attempts.push(ProcessCaptureEvent::PathRenameAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.path_mode_change_attempts {
                attempts.push(ProcessCaptureEvent::PathModeChangeAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.descriptor_timestamp_change_attempts {
                attempts.push(ProcessCaptureEvent::DescriptorTimestampChangeAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.symbolic_link_creation_attempts {
                attempts.push(ProcessCaptureEvent::SymbolicLinkCreationAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.filesystem_status_attempts {
                attempts.push(ProcessCaptureEvent::FilesystemStatusAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.getdents64_attempts {
                attempts.push(ProcessCaptureEvent::Getdents64Attempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.mmap_attempts {
                attempts.push(ProcessCaptureEvent::MmapAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.write_attempts {
                attempts.push(ProcessCaptureEvent::WriteAttempt {
                    process: process.id,
                    attempt: attempt.clone(),
                });
            }
            for attempt in &process.unmodeled_syscall_attempts {
                attempts.push(ProcessCaptureEvent::UnmodeledSyscallAttempt {
                    process: process.id,
                    attempt: *attempt,
                });
            }
            attempts.sort_by_key(|event| event.syscall_index().unwrap());
            for attempt in attempts {
                sink.record(attempt).unwrap();
            }
            sink.record(ProcessCaptureEvent::ProcessFinished {
                process: process.id,
                summary: ProcessFinish {
                    invocations: process.invocations.len() as u64,
                    exec_syscalls: process.exec_syscalls,
                    open_syscalls: process.open_syscalls,
                    newfstatat_syscalls: process.newfstatat_syscalls,
                    fstat_syscalls: process.fstat_syscalls,
                    getrandom_syscalls: process.getrandom_syscalls,
                    runtime_observation_syscalls: process.runtime_observation_syscalls,
                    prlimit_syscalls: process.prlimit_syscalls,
                    close_syscalls: process.close_syscalls,
                    seek_syscalls: process.seek_syscalls,
                    file_advice_syscalls: process.file_advice_syscalls,
                    fcntl_syscalls: process.fcntl_syscalls,
                    ioctl_syscalls: process.ioctl_syscalls,
                    pipe_syscalls: process.pipe_syscalls,
                    epoll_create_syscalls: process.epoll_create_syscalls,
                    socket_create_syscalls: process.socket_create_syscalls,
                    socket_connect_syscalls: process.socket_connect_syscalls,
                    dup2_syscalls: process.dup2_syscalls,
                    signal_state_syscalls: process.signal_state_syscalls,
                    signal_deliveries: process.signal_deliveries.len() as u64,
                    umask_syscalls: process.umask_syscalls,
                    credential_syscalls: process.credential_syscalls,
                    futex_wake_syscalls: process.futex_wake_syscalls,
                    process_creation_syscalls: process.process_creation_syscalls,
                    child_wait_syscalls: process.child_wait_syscalls,
                    process_identity_syscalls: process.process_identity_syscalls,
                    memory_state_syscalls: process.memory_state_syscalls,
                    thread_state_syscalls: process.thread_state_syscalls,
                    lifecycle_exit_syscalls: process.lifecycle_exit_syscalls,
                    statx_syscalls: process.statx_syscalls,
                    xattr_syscalls: process.xattr_syscalls,
                    readlink_syscalls: process.readlink_syscalls,
                    access_syscalls: process.access_syscalls,
                    getcwd_syscalls: process.getcwd_syscalls,
                    cwd_change_syscalls: process.cwd_change_syscalls,
                    path_removal_syscalls: process.path_removal_syscalls,
                    directory_creation_syscalls: process.directory_creation_syscalls,
                    path_rename_syscalls: process.path_rename_syscalls,
                    path_mode_change_syscalls: process.path_mode_change_syscalls,
                    descriptor_timestamp_change_syscalls: process
                        .descriptor_timestamp_change_syscalls,
                    symbolic_link_creation_syscalls: process.symbolic_link_creation_syscalls,
                    filesystem_status_syscalls: process.filesystem_status_syscalls,
                    read_syscalls: process.read_syscalls,
                    getdents64_syscalls: process.getdents64_syscalls,
                    mmap_syscalls: process.mmap_syscalls,
                    write_syscalls: process.write_syscalls,
                    unmodeled_syscalls: process.unmodeled_syscalls,
                    syscall_entries: process.syscall_entries,
                    syscall_exits: process.syscall_exits,
                    terminal_syscalls: process.terminal_syscalls,
                    outcome: process.outcome,
                },
            })
            .unwrap();
        }
        sink.finish(receipt.root, path).unwrap()
    }

    fn recompute_stream_digest(bytes: &mut [u8]) {
        let trailer = bytes.len() - TRAILER_BYTES as usize;
        let committed_len =
            u64::from_le_bytes(bytes[trailer + 8..trailer + 16].try_into().unwrap());
        assert_eq!(committed_len as usize, trailer);
        let mut hasher = blake3::Hasher::new();
        hasher.update(DIGEST_DOMAIN);
        hasher.update(&bytes[..trailer]);
        hasher.update(&committed_len.to_le_bytes());
        bytes[trailer + 16..].copy_from_slice(hasher.finalize().as_bytes());
    }

    #[derive(Default)]
    struct CountingVisitor {
        process_started: u64,
        invocations: u64,
        exec_attempts: u64,
        open_attempts: u64,
        newfstatat_attempts: u64,
        fstat_attempts: u64,
        getrandom_attempts: u64,
        runtime_observation_attempts: u64,
        prlimit_attempts: u64,
        close_attempts: u64,
        seek_attempts: u64,
        file_advice_attempts: u64,
        fcntl_attempts: u64,
        ioctl_attempts: u64,
        pipe_attempts: u64,
        epoll_create_attempts: u64,
        socket_create_attempts: u64,
        socket_connect_attempts: u64,
        dup2_attempts: u64,
        signal_state_attempts: u64,
        signal_deliveries: u64,
        umask_attempts: u64,
        credential_attempts: u64,
        futex_wake_attempts: u64,
        process_creation_attempts: u64,
        child_wait_attempts: u64,
        process_identity_attempts: u64,
        memory_state_attempts: u64,
        thread_state_attempts: u64,
        lifecycle_exit_attempts: u64,
        statx_attempts: u64,
        xattr_attempts: u64,
        readlink_attempts: u64,
        access_attempts: u64,
        getcwd_attempts: u64,
        cwd_change_attempts: u64,
        path_removal_attempts: u64,
        directory_creation_attempts: u64,
        path_rename_attempts: u64,
        path_mode_change_attempts: u64,
        descriptor_timestamp_change_attempts: u64,
        symbolic_link_creation_attempts: u64,
        filesystem_status_attempts: u64,
        read_attempts: u64,
        getdents64_attempts: u64,
        mmap_attempts: u64,
        write_attempts: u64,
        unmodeled_syscall_attempts: u64,
        process_finished: u64,
    }

    impl ProcessCaptureStreamVisitor for CountingVisitor {
        fn visit(&mut self, event: ProcessCaptureStreamEvent<'_>) -> Result<()> {
            let counter = match event {
                ProcessCaptureStreamEvent::ProcessStarted { .. } => &mut self.process_started,
                ProcessCaptureStreamEvent::Invocation { .. } => &mut self.invocations,
                ProcessCaptureStreamEvent::ExecAttempt { .. } => &mut self.exec_attempts,
                ProcessCaptureStreamEvent::OpenAttempt { .. } => &mut self.open_attempts,
                ProcessCaptureStreamEvent::NewfstatatAttempt { .. } => {
                    &mut self.newfstatat_attempts
                }
                ProcessCaptureStreamEvent::FstatAttempt { .. } => &mut self.fstat_attempts,
                ProcessCaptureStreamEvent::GetrandomAttempt { .. } => &mut self.getrandom_attempts,
                ProcessCaptureStreamEvent::RuntimeObservationAttempt { .. } => {
                    &mut self.runtime_observation_attempts
                }
                ProcessCaptureStreamEvent::PrlimitAttempt { .. } => &mut self.prlimit_attempts,
                ProcessCaptureStreamEvent::CloseAttempt { .. } => &mut self.close_attempts,
                ProcessCaptureStreamEvent::SeekAttempt { .. } => &mut self.seek_attempts,
                ProcessCaptureStreamEvent::FileAdviceAttempt { .. } => {
                    &mut self.file_advice_attempts
                }
                ProcessCaptureStreamEvent::FcntlAttempt { .. } => &mut self.fcntl_attempts,
                ProcessCaptureStreamEvent::IoctlAttempt { .. } => &mut self.ioctl_attempts,
                ProcessCaptureStreamEvent::PipeAttempt { .. } => &mut self.pipe_attempts,
                ProcessCaptureStreamEvent::EpollCreateAttempt { .. } => {
                    &mut self.epoll_create_attempts
                }
                ProcessCaptureStreamEvent::SocketCreateAttempt { .. } => {
                    &mut self.socket_create_attempts
                }
                ProcessCaptureStreamEvent::SocketConnectAttempt { .. } => {
                    &mut self.socket_connect_attempts
                }
                ProcessCaptureStreamEvent::Dup2Attempt { .. } => &mut self.dup2_attempts,
                ProcessCaptureStreamEvent::SignalStateAttempt { .. } => {
                    &mut self.signal_state_attempts
                }
                ProcessCaptureStreamEvent::SignalDelivery { .. } => &mut self.signal_deliveries,
                ProcessCaptureStreamEvent::UmaskAttempt { .. } => &mut self.umask_attempts,
                ProcessCaptureStreamEvent::CredentialAttempt { .. } => {
                    &mut self.credential_attempts
                }
                ProcessCaptureStreamEvent::FutexWakeAttempt { .. } => &mut self.futex_wake_attempts,
                ProcessCaptureStreamEvent::ProcessCreationAttempt { .. } => {
                    &mut self.process_creation_attempts
                }
                ProcessCaptureStreamEvent::ChildWaitAttempt { .. } => &mut self.child_wait_attempts,
                ProcessCaptureStreamEvent::ProcessIdentityAttempt { .. } => {
                    &mut self.process_identity_attempts
                }
                ProcessCaptureStreamEvent::MemoryStateAttempt { .. } => {
                    &mut self.memory_state_attempts
                }
                ProcessCaptureStreamEvent::ThreadStateAttempt { .. } => {
                    &mut self.thread_state_attempts
                }
                ProcessCaptureStreamEvent::LifecycleExitAttempt { .. } => {
                    &mut self.lifecycle_exit_attempts
                }
                ProcessCaptureStreamEvent::StatxAttempt { .. } => &mut self.statx_attempts,
                ProcessCaptureStreamEvent::XattrAttempt { .. } => &mut self.xattr_attempts,
                ProcessCaptureStreamEvent::ReadlinkAttempt { .. } => &mut self.readlink_attempts,
                ProcessCaptureStreamEvent::AccessAttempt { .. } => &mut self.access_attempts,
                ProcessCaptureStreamEvent::GetcwdAttempt { .. } => &mut self.getcwd_attempts,
                ProcessCaptureStreamEvent::CwdChangeAttempt { .. } => &mut self.cwd_change_attempts,
                ProcessCaptureStreamEvent::PathRemovalAttempt { .. } => {
                    &mut self.path_removal_attempts
                }
                ProcessCaptureStreamEvent::DirectoryCreationAttempt { .. } => {
                    &mut self.directory_creation_attempts
                }
                ProcessCaptureStreamEvent::PathRenameAttempt { .. } => {
                    &mut self.path_rename_attempts
                }
                ProcessCaptureStreamEvent::PathModeChangeAttempt { .. } => {
                    &mut self.path_mode_change_attempts
                }
                ProcessCaptureStreamEvent::DescriptorTimestampChangeAttempt { .. } => {
                    &mut self.descriptor_timestamp_change_attempts
                }
                ProcessCaptureStreamEvent::SymbolicLinkCreationAttempt { .. } => {
                    &mut self.symbolic_link_creation_attempts
                }
                ProcessCaptureStreamEvent::FilesystemStatusAttempt { .. } => {
                    &mut self.filesystem_status_attempts
                }
                ProcessCaptureStreamEvent::ReadAttempt { .. } => &mut self.read_attempts,
                ProcessCaptureStreamEvent::Getdents64Attempt { .. } => {
                    &mut self.getdents64_attempts
                }
                ProcessCaptureStreamEvent::MmapAttempt { .. } => &mut self.mmap_attempts,
                ProcessCaptureStreamEvent::WriteAttempt { .. } => &mut self.write_attempts,
                ProcessCaptureStreamEvent::UnmodeledSyscallAttempt { .. } => {
                    &mut self.unmodeled_syscall_attempts
                }
                ProcessCaptureStreamEvent::ProcessFinished { .. } => &mut self.process_finished,
            };
            *counter += 1;
            Ok(())
        }
    }

    struct SharedVisitor {
        calls: std::sync::Arc<std::sync::atomic::AtomicU64>,
        fail_at: Option<u64>,
        append_to: Option<std::path::PathBuf>,
    }

    impl ProcessCaptureStreamVisitor for SharedVisitor {
        fn visit(&mut self, _event: ProcessCaptureStreamEvent<'_>) -> Result<()> {
            use std::sync::atomic::Ordering;

            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            if call == 1 {
                if let Some(path) = self.append_to.take() {
                    OpenOptions::new()
                        .append(true)
                        .open(path)
                        .unwrap()
                        .write_all(&[0])
                        .unwrap();
                }
            }
            if self.fail_at == Some(call) {
                anyhow::bail!("test visitor stopped");
            }
            Ok(())
        }
    }

    #[test]
    fn visits_every_runtime_event_kind_without_materializing_a_receipt() {
        let temp = private_tempdir();
        let path = temp.path().join("visited.stream");
        let mut command = rich_command();
        let summary =
            capture_process_tree_to_path(&mut command, &path, IDENTITY, limits()).unwrap();
        let (visitor, seal) =
            visit_process_capture_stream(&path, summary, limits(), CountingVisitor::default())
                .unwrap();
        let census = summary.census();
        assert_eq!(visitor.process_started, summary.process_count());
        assert_eq!(visitor.invocations, census.invocations);
        assert_eq!(visitor.exec_attempts, census.exec_attempts);
        assert_eq!(visitor.open_attempts, census.open_attempts);
        assert_eq!(visitor.newfstatat_attempts, census.newfstatat_attempts);
        assert_eq!(visitor.fstat_attempts, census.fstat_attempts);
        assert_eq!(visitor.getrandom_attempts, census.getrandom_attempts);
        assert_eq!(
            visitor.runtime_observation_attempts,
            census.runtime_observation_attempts
        );
        assert_eq!(visitor.prlimit_attempts, census.prlimit_attempts);
        assert_eq!(visitor.close_attempts, census.close_attempts);
        assert_eq!(visitor.seek_attempts, census.seek_attempts);
        assert_eq!(visitor.file_advice_attempts, census.file_advice_attempts);
        assert_eq!(visitor.fcntl_attempts, census.fcntl_attempts);
        assert_eq!(visitor.ioctl_attempts, census.ioctl_attempts);
        assert_eq!(visitor.pipe_attempts, census.pipe_attempts);
        assert_eq!(visitor.epoll_create_attempts, census.epoll_create_attempts);
        assert_eq!(
            visitor.socket_create_attempts,
            census.socket_create_attempts
        );
        assert_eq!(
            visitor.socket_connect_attempts,
            census.socket_connect_attempts
        );
        assert_eq!(visitor.dup2_attempts, census.dup2_attempts);
        assert_eq!(visitor.signal_state_attempts, census.signal_state_attempts);
        assert_eq!(visitor.signal_deliveries, census.signal_deliveries);
        assert_eq!(visitor.umask_attempts, census.umask_attempts);
        assert_eq!(visitor.credential_attempts, census.credential_attempts);
        assert_eq!(visitor.futex_wake_attempts, census.futex_wake_attempts);
        assert_eq!(
            visitor.process_creation_attempts,
            census.process_creation_attempts
        );
        assert_eq!(visitor.child_wait_attempts, census.child_wait_attempts);
        assert_eq!(
            visitor.process_identity_attempts,
            census.process_identity_attempts
        );
        assert_eq!(visitor.memory_state_attempts, census.memory_state_attempts);
        assert_eq!(visitor.thread_state_attempts, census.thread_state_attempts);
        assert_eq!(
            visitor.lifecycle_exit_attempts,
            census.lifecycle_exit_attempts
        );
        assert_eq!(visitor.statx_attempts, census.statx_attempts);
        assert_eq!(visitor.xattr_attempts, census.xattr_attempts);
        assert_eq!(visitor.readlink_attempts, census.readlink_attempts);
        assert_eq!(visitor.access_attempts, census.access_attempts);
        assert_eq!(visitor.getcwd_attempts, census.getcwd_attempts);
        assert_eq!(visitor.cwd_change_attempts, census.cwd_change_attempts);
        assert_eq!(visitor.path_removal_attempts, census.path_removal_attempts);
        assert_eq!(
            visitor.directory_creation_attempts,
            census.directory_creation_attempts
        );
        assert_eq!(visitor.path_rename_attempts, census.path_rename_attempts);
        assert_eq!(
            visitor.path_mode_change_attempts,
            census.path_mode_change_attempts
        );
        assert_eq!(
            visitor.descriptor_timestamp_change_attempts,
            census.descriptor_timestamp_change_attempts
        );
        assert_eq!(
            visitor.symbolic_link_creation_attempts,
            census.symbolic_link_creation_attempts
        );
        assert_eq!(
            visitor.filesystem_status_attempts,
            census.filesystem_status_attempts
        );
        assert_eq!(visitor.read_attempts, census.read_attempts);
        assert_eq!(visitor.getdents64_attempts, census.getdents64_attempts);
        assert_eq!(visitor.mmap_attempts, census.mmap_attempts);
        assert_eq!(visitor.write_attempts, census.write_attempts);
        assert_eq!(visitor.process_finished, summary.process_count());
        assert_eq!(seal.summary, summary);
    }

    #[test]
    fn visits_no_events_for_a_wrong_trusted_summary() {
        use std::sync::atomic::Ordering;

        let temp = private_tempdir();
        let path = temp.path().join("wrong-summary.stream");
        let summary = write_fixture(&path);
        let mut wrong = summary;
        wrong.digest[0] ^= 1;
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let visitor = SharedVisitor {
            calls: calls.clone(),
            fail_at: None,
            append_to: None,
        };
        assert!(visit_process_capture_stream(&path, wrong, limits(), visitor).is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn visitor_and_loader_reject_reordered_loaded_exec_indices() {
        let temp = private_tempdir();
        let path = temp.path().join("reordered-loaded-execs.stream");
        let mut sink = StreamCaptureSink::create(&path, IDENTITY, limits()).unwrap();
        sink.record(ProcessCaptureEvent::ProcessStarted {
            process: 1,
            parent: None,
        })
        .unwrap();
        for _ in 0..3 {
            sink.record(ProcessCaptureEvent::Invocation {
                process: 1,
                invocation: fixture_invocation(),
            })
            .unwrap();
        }
        for (syscall_index, loaded) in [(0, 2), (1, 1)] {
            sink.record(ProcessCaptureEvent::ExecAttempt {
                process: 1,
                attempt: CapturedExecAttempt {
                    syscall_index,
                    request: CapturedExecRequest {
                        executable: b"/fixture/tool".to_vec(),
                        directory: None,
                        argv: vec![b"tool".to_vec()],
                        environment: Vec::new(),
                        flags: 0,
                    },
                    outcome: CapturedExecOutcome::Loaded(loaded),
                },
            })
            .unwrap();
        }
        sink.record(ProcessCaptureEvent::ProcessFinished {
            process: 1,
            summary: ProcessFinish {
                invocations: 3,
                exec_syscalls: 2,
                syscall_entries: 2,
                syscall_exits: 2,
                ..empty_finish(CapturedProcessOutcome::Exited(0))
            },
        })
        .unwrap();
        let summary = sink.finish(1, &path).unwrap();
        assert!(
            visit_process_capture_stream(&path, summary, limits(), CountingVisitor::default())
                .is_err()
        );
        assert!(load_process_capture_stream(&path, summary, limits()).is_err());
    }

    #[test]
    fn rejects_late_semantic_errors_and_callback_or_stream_failures() {
        use std::sync::atomic::Ordering;

        let invalid_temp = private_tempdir();
        let invalid_path = invalid_temp.path().join("invalid-semantic.stream");
        let mut sink = StreamCaptureSink::create(&invalid_path, IDENTITY, limits()).unwrap();
        sink.record(ProcessCaptureEvent::ProcessStarted {
            process: 1,
            parent: None,
        })
        .unwrap();
        let mut invalid_invocation = fixture_invocation();
        invalid_invocation.argv.clear();
        sink.record(ProcessCaptureEvent::Invocation {
            process: 1,
            invocation: invalid_invocation,
        })
        .unwrap();
        sink.record(ProcessCaptureEvent::ProcessFinished {
            process: 1,
            summary: ProcessFinish {
                invocations: 1,
                ..empty_finish(CapturedProcessOutcome::Exited(0))
            },
        })
        .unwrap();
        let invalid_summary = sink.finish(1, &invalid_path).unwrap();
        let invalid_calls = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let visitor = SharedVisitor {
            calls: invalid_calls.clone(),
            fail_at: None,
            append_to: None,
        };
        assert!(
            visit_process_capture_stream(&invalid_path, invalid_summary, limits(), visitor)
                .is_err()
        );
        assert_eq!(invalid_calls.load(Ordering::SeqCst), 1);
        assert!(load_process_capture_stream(&invalid_path, invalid_summary, limits()).is_err());

        let callback_temp = private_tempdir();
        let callback_path = callback_temp.path().join("callback-error.stream");
        let callback_summary = write_fixture(&callback_path);
        let callback_calls = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let visitor = SharedVisitor {
            calls: callback_calls.clone(),
            fail_at: Some(2),
            append_to: None,
        };
        assert!(
            visit_process_capture_stream(&callback_path, callback_summary, limits(), visitor)
                .is_err()
        );
        assert_eq!(callback_calls.load(Ordering::SeqCst), 2);

        let mutation_temp = private_tempdir();
        let mutation_path = mutation_temp.path().join("mutated.stream");
        let mutation_summary = write_fixture(&mutation_path);
        let mutation_calls = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let visitor = SharedVisitor {
            calls: mutation_calls.clone(),
            fail_at: None,
            append_to: Some(mutation_path.clone()),
        };
        assert!(
            visit_process_capture_stream(&mutation_path, mutation_summary, limits(), visitor)
                .is_err()
        );
        assert!(mutation_calls.load(Ordering::SeqCst) > 0);
    }

    #[test]
    fn visits_many_events_with_one_compact_process_state() {
        const ATTEMPTS: u64 = 20_000;

        let temp = private_tempdir();
        let path = temp.path().join("many-events.stream");
        let mut sink = StreamCaptureSink::create(&path, IDENTITY, limits()).unwrap();
        sink.record(ProcessCaptureEvent::ProcessStarted {
            process: 1,
            parent: None,
        })
        .unwrap();
        sink.record(ProcessCaptureEvent::Invocation {
            process: 1,
            invocation: fixture_invocation(),
        })
        .unwrap();
        for syscall_index in 0..ATTEMPTS {
            sink.record(ProcessCaptureEvent::OpenAttempt {
                process: 1,
                attempt: CapturedOpenAttempt {
                    syscall_index,
                    request: CapturedOpenRequest {
                        system_call: CapturedOpenSystemCall::Open,
                        path: b"/missing".to_vec(),
                        directory: None,
                        flags: 0,
                        mode: 0,
                        resolve: 0,
                    },
                    outcome: CapturedOpenOutcome::Failed(libc::ENOENT),
                },
            })
            .unwrap();
        }
        sink.record(ProcessCaptureEvent::ProcessFinished {
            process: 1,
            summary: ProcessFinish {
                invocations: 1,
                open_syscalls: ATTEMPTS,
                syscall_entries: ATTEMPTS,
                syscall_exits: ATTEMPTS,
                ..empty_finish(CapturedProcessOutcome::Exited(0))
            },
        })
        .unwrap();
        let summary = sink.finish(1, &path).unwrap();
        let (visitor, seal) =
            visit_process_capture_stream(&path, summary, limits(), CountingVisitor::default())
                .unwrap();
        assert_eq!(visitor.process_started, 1);
        assert_eq!(visitor.open_attempts, ATTEMPTS);
        assert_eq!(visitor.process_finished, 1);
        assert_eq!(seal.summary, summary);
    }

    #[test]
    fn streams_and_loads_a_real_capture_privately() {
        let temp = private_tempdir();
        let path = temp.path().join("capture.stream");
        let mut command = Command::new("/bin/true");
        let seal = capture_process_tree_to_path(&mut command, &path, IDENTITY, limits()).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let (receipt, loaded) = load_process_capture_stream(&path, seal, limits()).unwrap();
        assert_eq!(loaded.summary, seal);
        assert_eq!(receipt.processes().len() as u64, seal.process_count());
        receipt.validate_complete().unwrap();
        receipt.require_witnessed_file_mappings().unwrap();
        receipt.require_witnessed_directory_enumerations().unwrap();
        let input_seal = certify_process_capture_input_witnesses(&path, seal, limits()).unwrap();
        assert_eq!(input_seal.stream().summary, seal);
        assert_eq!(
            input_seal.census().regular_file_reads(),
            input_seal.census().witnessed_regular_file_reads()
        );
        let before = std::fs::read(&path).unwrap();
        let mut second = Command::new("/bin/true");
        assert!(capture_process_tree_to_path(&mut second, &path, IDENTITY, limits()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn streams_signal_returns_in_delivery_order() {
        let temp = private_tempdir();
        let path = temp.path().join("signal-return.stream");
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("/bin/true; /bin/true; /bin/true");
        let summary =
            capture_process_tree_to_path(&mut command, &path, IDENTITY, limits()).unwrap();
        let (receipt, _) = load_process_capture_stream(&path, summary, limits()).unwrap();
        assert!(receipt.processes().iter().any(|process| {
            process.thread_state_attempts.iter().any(|attempt| {
                matches!(
                    (attempt.request, attempt.outcome),
                    (
                        crate::CapturedThreadStateRequest::SignalReturn { .. },
                        crate::CapturedThreadStateOutcome::SignalContextRestored
                    )
                )
            })
        }));

        let bad_path = temp.path().join("early-signal-return.stream");
        let mut sink = StreamCaptureSink::create(&bad_path, IDENTITY, limits()).unwrap();
        sink.record(ProcessCaptureEvent::ProcessStarted {
            process: 1,
            parent: None,
        })
        .unwrap();
        sink.record(ProcessCaptureEvent::ProcessStarted {
            process: 2,
            parent: Some(1),
        })
        .unwrap();
        sink.record(ProcessCaptureEvent::Invocation {
            process: 1,
            invocation: fixture_invocation(),
        })
        .unwrap();
        sink.record(ProcessCaptureEvent::SignalDelivery {
            process: 1,
            delivery: crate::CapturedSignalDelivery {
                signal: libc::SIGUSR1 as u32,
                code: 0,
                errno: 0,
                source: crate::CapturedSignalSource::Kernel,
                child_status: None,
                syscall_entries: 0,
                active_syscall: None,
                disposition: crate::CapturedSignalDisposition::Ignored,
            },
        })
        .unwrap();
        sink.record(ProcessCaptureEvent::ThreadStateAttempt {
            process: 1,
            attempt: CapturedThreadStateAttempt {
                syscall_index: 0,
                request: crate::CapturedThreadStateRequest::SignalReturn {
                    delivery: 0,
                    restored_accumulator: 0,
                },
                outcome: crate::CapturedThreadStateOutcome::SignalContextRestored,
            },
        })
        .unwrap();
        sink.record(ProcessCaptureEvent::ProcessFinished {
            process: 2,
            summary: empty_finish(CapturedProcessOutcome::Exited(0)),
        })
        .unwrap();
        sink.record(ProcessCaptureEvent::ProcessFinished {
            process: 1,
            summary: ProcessFinish {
                invocations: 1,
                signal_deliveries: 1,
                thread_state_syscalls: 1,
                syscall_entries: 1,
                syscall_exits: 1,
                ..empty_finish(CapturedProcessOutcome::Exited(0))
            },
        })
        .unwrap();
        let bad_summary = sink.finish(1, &bad_path).unwrap();
        let error = load_process_capture_stream(&bad_path, bad_summary, limits()).unwrap_err();
        assert!(
            error.to_string().contains("invalid signal-return"),
            "{error:#}"
        );
    }

    #[test]
    fn enforces_nested_caught_signal_frame_lifo() {
        fn write_nested(
            path: &Path,
            returns: [u64; 2],
        ) -> ProcessCaptureStreamSummary {
            let mut sink =
                StreamCaptureSink::create(path, IDENTITY, limits()).unwrap();
            sink.record(ProcessCaptureEvent::ProcessStarted {
                process: 1,
                parent: None,
            })
            .unwrap();
            sink.record(ProcessCaptureEvent::Invocation {
                process: 1,
                invocation: fixture_invocation(),
            })
            .unwrap();
            for signal in [libc::SIGUSR1, libc::SIGUSR2] {
                sink.record(ProcessCaptureEvent::SignalDelivery {
                    process: 1,
                    delivery: crate::CapturedSignalDelivery {
                        signal: signal as u32,
                        code: 0,
                        errno: 0,
                        source: crate::CapturedSignalSource::Kernel,
                        child_status: None,
                        syscall_entries: 0,
                        active_syscall: None,
                        disposition: crate::CapturedSignalDisposition::Caught,
                    },
                })
                .unwrap();
            }
            for (syscall_index, delivery) in returns.into_iter().enumerate() {
                sink.record(ProcessCaptureEvent::ThreadStateAttempt {
                    process: 1,
                    attempt: CapturedThreadStateAttempt {
                        syscall_index: u64::try_from(syscall_index).unwrap(),
                        request:
                            crate::CapturedThreadStateRequest::SignalReturn {
                            delivery,
                            restored_accumulator: 0,
                        },
                        outcome: crate::CapturedThreadStateOutcome::
                            SignalContextRestored,
                    },
                })
                .unwrap();
            }
            sink.record(ProcessCaptureEvent::ProcessFinished {
                process: 1,
                summary: ProcessFinish {
                    invocations: 1,
                    signal_deliveries: 2,
                    thread_state_syscalls: 2,
                    syscall_entries: 2,
                    syscall_exits: 2,
                    ..empty_finish(CapturedProcessOutcome::Exited(0))
                },
            })
            .unwrap();
            sink.finish(1, path).unwrap()
        }

        let temp = private_tempdir();
        let valid_path = temp.path().join("nested-signal-return.stream");
        let valid_summary = write_nested(&valid_path, [1, 0]);
        let (receipt, _) = load_process_capture_stream(
            &valid_path,
            valid_summary,
            limits(),
        )
        .unwrap();
        receipt.validate_complete().unwrap();

        let invalid_path = temp.path().join("reordered-signal-return.stream");
        let invalid_summary = write_nested(&invalid_path, [0, 1]);
        let error = load_process_capture_stream(
            &invalid_path,
            invalid_summary,
            limits(),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("invalid signal-return delivery"),
            "{error:#}"
        );
    }

    #[test]
    fn round_trips_every_runtime_event_kind() {
        let mut command = rich_command();
        let receipt = crate::capture_process_tree(&mut command).unwrap();
        assert!(receipt.exec_syscalls().unwrap() > 0);
        assert!(receipt.open_syscalls().unwrap() > 0);
        assert!(receipt.newfstatat_syscalls().unwrap() > 0);
        assert!(receipt.fstat_syscalls().unwrap() > 0);
        assert!(receipt.getrandom_syscalls().unwrap() > 0);
        #[cfg(target_pointer_width = "64")]
        assert!(receipt.runtime_observation_syscalls().unwrap() > 0);
        assert!(receipt.prlimit_syscalls().unwrap() > 0);
        assert!(receipt.close_syscalls().unwrap() > 0);
        assert!(receipt.seek_syscalls().unwrap() > 0);
        #[cfg(target_arch = "x86_64")]
        assert!(receipt.file_advice_syscalls().unwrap() > 0);
        assert!(receipt.fcntl_syscalls().unwrap() > 0);
        assert!(receipt.ioctl_syscalls().unwrap() > 0);
        assert!(receipt.pipe_syscalls().unwrap() > 0);
        assert!(receipt.epoll_create_syscalls().unwrap() > 0);
        assert!(receipt.socket_create_syscalls().unwrap() > 0);
        assert!(receipt.socket_connect_syscalls().unwrap() > 0);
        assert!(receipt.dup2_syscalls().unwrap() > 0);
        #[cfg(target_arch = "x86_64")]
        assert!(receipt.signal_state_syscalls().unwrap() > 0);
        assert!(receipt.umask_syscalls().unwrap() > 0);
        assert!(receipt.memory_state_syscalls().unwrap() > 0);
        assert!(receipt.thread_state_syscalls().unwrap() > 0);
        assert!(receipt.lifecycle_exit_syscalls().unwrap() > 0);
        assert!(receipt.access_syscalls().unwrap() > 0);
        assert!(receipt.getcwd_syscalls().unwrap() > 0);
        assert!(receipt.cwd_change_syscalls().unwrap() > 0);
        assert!(receipt.path_removal_syscalls().unwrap() > 0);
        assert!(receipt.directory_creation_syscalls().unwrap() > 0);
        assert!(receipt.path_rename_syscalls().unwrap() > 0);
        assert!(receipt.path_mode_change_syscalls().unwrap() > 0);
        assert!(receipt.descriptor_timestamp_change_syscalls().unwrap() > 0);
        assert!(receipt.filesystem_status_syscalls().unwrap() > 0);
        assert!(receipt.read_syscalls().unwrap() > 0);
        assert!(receipt.getdents64_syscalls().unwrap() > 0);
        assert!(receipt.mmap_syscalls().unwrap() > 0);
        assert!(receipt.write_syscalls().unwrap() > 0);

        let temp = private_tempdir();
        let path = temp.path().join("all-events.stream");
        let seal = write_receipt(&path, &receipt);
        let (loaded, loaded_seal) = load_process_capture_stream(&path, seal, limits()).unwrap();
        assert_eq!(loaded, receipt);
        assert_eq!(loaded_seal.summary, seal);
    }

    #[test]
    fn streams_every_runtime_event_kind_directly() {
        let temp = private_tempdir();
        let path = temp.path().join("direct-all-events.stream");
        let mut command = rich_command();
        let summary =
            capture_process_tree_to_path(&mut command, &path, IDENTITY, limits()).unwrap();
        let census = summary.census();
        assert!(summary.process_count() > 1);
        assert!(census.invocations > 1);
        assert!(census.exec_attempts > 1);
        assert!(census.open_attempts > 0);
        assert!(census.newfstatat_attempts > 0);
        assert!(census.fstat_attempts > 0);
        assert!(census.getrandom_attempts > 0);
        #[cfg(target_pointer_width = "64")]
        assert!(census.runtime_observation_attempts > 0);
        assert!(census.prlimit_attempts > 0);
        assert!(census.close_attempts > 0);
        assert!(census.seek_attempts > 0);
        #[cfg(target_arch = "x86_64")]
        assert!(census.file_advice_attempts > 0);
        assert!(census.fcntl_attempts > 0);
        assert!(census.ioctl_attempts > 0);
        assert!(census.pipe_attempts > 0);
        assert!(census.epoll_create_attempts > 0);
        assert!(census.socket_create_attempts > 0);
        assert!(census.socket_connect_attempts > 0);
        assert!(census.dup2_attempts > 0);
        #[cfg(target_arch = "x86_64")]
        assert!(census.signal_state_attempts > 0);
        assert!(census.signal_deliveries > 0);
        assert!(census.umask_attempts > 0);
        assert!(census.credential_attempts > 0);
        assert!(census.futex_wake_attempts > 0);
        assert!(census.memory_state_attempts > 0);
        assert!(census.thread_state_attempts > 0);
        assert!(census.lifecycle_exit_attempts > 0);
        assert!(census.statx_attempts > 0);
        assert!(census.xattr_attempts > 0);
        assert!(census.readlink_attempts > 0);
        assert!(census.access_attempts > 0);
        assert!(census.getcwd_attempts > 0);
        assert!(census.cwd_change_attempts > 0);
        assert!(census.path_removal_attempts > 0);
        assert!(census.directory_creation_attempts > 0);
        assert!(census.path_rename_attempts > 0);
        assert!(census.path_mode_change_attempts > 0);
        assert!(census.descriptor_timestamp_change_attempts > 0);
        assert!(census.symbolic_link_creation_attempts > 0);
        assert!(census.filesystem_status_attempts > 0);
        assert!(census.read_attempts > 0);
        assert!(census.getdents64_attempts > 0);
        assert!(census.mmap_attempts > 0);
        assert!(census.write_attempts > 0);
        assert!(census.unmodeled_syscall_attempts > 0);
        let (receipt, seal) = load_process_capture_stream(&path, summary, limits()).unwrap();
        receipt.validate_complete().unwrap();
        assert_eq!(seal.summary, summary);
    }

    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn streams_measured_xargs_descriptor_readiness_probe() {
        let temp = private_tempdir();
        let path = temp.path().join("poll.stream");
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "printf 'x\\n' | /usr/bin/xargs /bin/true"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .env_clear();

        let summary =
            capture_process_tree_to_path(&mut command, &path, IDENTITY, limits()).unwrap();
        let (receipt, seal) = load_process_capture_stream(&path, summary, limits()).unwrap();
        assert_eq!(seal.summary, summary);
        let probe = receipt
            .processes()
            .iter()
            .flat_map(|process| &process.runtime_observation_attempts)
            .find(|attempt| {
                matches!(
                    &attempt.request,
                    CapturedRuntimeObservationRequest::DescriptorReadiness { .. }
                )
            })
            .unwrap();
        let CapturedRuntimeObservationRequest::DescriptorReadiness { descriptors, .. } =
            &probe.request
        else {
            unreachable!();
        };
        assert_eq!(
            descriptors[0].identity.as_ref().unwrap().kind,
            CapturedDescriptorKind::Fifo
        );
        for descriptor in &descriptors[1..=2] {
            let identity = descriptor.identity.as_ref().unwrap();
            assert_eq!(identity.kind, CapturedDescriptorKind::CharacterDevice);
            assert_eq!(identity.target, b"/dev/null");
        }
        assert!(descriptors[3].identity.is_none());
        assert_eq!(
            probe.outcome,
            CapturedRuntimeObservationOutcome::DescriptorReadiness {
                ready: 4,
                returned_events: vec![
                    libc::POLLIN | libc::POLLHUP,
                    libc::POLLIN | libc::POLLOUT,
                    libc::POLLIN | libc::POLLOUT,
                    libc::POLLNVAL,
                ],
            }
        );
    }

    #[test]
    fn pins_the_version_sixty_eight_all_event_encoding() {
        let receipt = all_event_fixture_receipt();
        let temp = private_tempdir();
        let path = temp.path().join("golden.stream");
        let summary = write_receipt(&path, &receipt);
        assert_eq!(summary.stream_bytes(), 7766);
        assert_eq!(
            summary.digest(),
            [
                172, 145, 231, 33, 166, 8, 1, 109, 35, 50, 107, 101, 54, 158, 210, 168, 139, 173,
                31, 115, 101, 124, 234, 226, 219, 82, 241, 130, 18, 107, 169, 81,
            ]
        );
        let (loaded, seal) = load_process_capture_stream(&path, summary, limits()).unwrap();
        assert_eq!(loaded, receipt);
        assert_eq!(seal.summary, summary);
        let encoded = serde_json::to_vec(&summary).unwrap();
        assert_eq!(
            serde_json::from_slice::<ProcessCaptureStreamSummary>(&encoded).unwrap(),
            summary
        );
        let input_seal = certify_process_capture_input_witnesses(&path, summary, limits()).unwrap();
        assert_eq!(input_seal.stream().summary, summary);
        assert_eq!(input_seal.census().file_mappings(), 1);
        assert_eq!(input_seal.census().witnessed_file_mappings(), 1);
        assert_eq!(input_seal.census().regular_file_reads(), 0);
        assert_eq!(input_seal.census().witnessed_regular_file_reads(), 0);
        assert_eq!(input_seal.census().directory_enumerations(), 1);
        assert_eq!(input_seal.census().witnessed_directory_enumerations(), 1);
    }

    #[test]
    fn classifies_only_bounded_procfs_regular_read_targets() {
        assert_eq!(
            classify_unwitnessed_regular_read(b"/proc/filesystems"),
            UnwitnessedRegularReadClass::ProcFilesystems
        );
        assert_eq!(
            classify_unwitnessed_regular_read(b"/proc/1/maps"),
            UnwitnessedRegularReadClass::ProcProcessMaps
        );
        assert_eq!(
            classify_unwitnessed_regular_read(b"/proc/42/statm"),
            UnwitnessedRegularReadClass::ProcProcessStatm
        );
        assert_eq!(
            classify_unwitnessed_regular_read(b"/proc/73/cgroup"),
            UnwitnessedRegularReadClass::ProcProcessCgroup
        );
        assert_eq!(
            classify_unwitnessed_regular_read(
                b"/sys/fs/cgroup/user.slice/user-1000.slice/session.scope/cpu.max"
            ),
            UnwitnessedRegularReadClass::CgroupCpuMax
        );
        assert_eq!(
            classify_unwitnessed_regular_read(b"/proc/sys/vm/overcommit_memory"),
            UnwitnessedRegularReadClass::ProcVmOvercommitMemory
        );
        for target in [
            b"/proc/self/maps".as_slice(),
            b"/proc/self/cgroup",
            b"/sys/fs/cgroup/cpu.max",
            b"/sys/fs/cgroup/../cpu.max",
            b"/proc/1/task/2/maps",
            b"/proc/1/map",
            b"/proc/sys/vm/overcommit_ratio",
            b"/sys/kernel/notes",
        ] {
            assert_eq!(
                classify_unwitnessed_regular_read(target),
                UnwitnessedRegularReadClass::Other
            );
        }
    }

    #[test]
    fn input_witness_seal_rejects_each_unwitnessed_family() {
        let temp = private_tempdir();
        let mut mapping = all_event_fixture_receipt();
        mapping.processes[0].invocations[0].file_mappings[0].value =
            CapturedMappedFileValue::Unwitnessed;
        let mapping_path = temp.path().join("unwitnessed-mapping.stream");
        let mapping_summary = write_receipt(&mapping_path, &mapping);
        let error =
            certify_process_capture_input_witnesses(&mapping_path, mapping_summary, limits())
                .unwrap_err();
        assert!(error
            .to_string()
            .contains("unwitnessed file-backed mappings"));

        let mut read = all_event_fixture_receipt();
        let attempt = &mut read.processes[0].read_attempts[0];
        attempt.request.descriptor = 3;
        attempt.request.identity = Some(CapturedDescriptorIdentity {
            target: b"/fixture/read-input".to_vec(),
            device: 1,
            inode: 3,
            kind: CapturedDescriptorKind::RegularFile,
        });
        attempt.request.mount_id = Some(1);
        attempt.request.offset = Some(0);
        attempt.outcome = CapturedReadOutcome::Read {
            bytes: 1,
            digest: [5; 32],
            file_value: Some(CapturedMappedFileValue::WholeFile {
                bytes: 1,
                digest: [5; 32],
                mode: libc::S_IFREG | 0o644,
            }),
        };
        let read_path = temp.path().join("witnessed-read.stream");
        let read_summary = write_receipt(&read_path, &read);
        let read_seal =
            certify_process_capture_input_witnesses(&read_path, read_summary, limits()).unwrap();
        assert_eq!(read_seal.census().regular_file_reads(), 1);
        assert_eq!(read_seal.census().witnessed_regular_file_reads(), 1);

        let CapturedReadOutcome::Read { file_value, .. } =
            &mut read.processes[0].read_attempts[0].outcome
        else {
            panic!("fixture read attempt is not successful");
        };
        *file_value = None;
        let read_path = temp.path().join("unwitnessed-read.stream");
        let read_summary = write_receipt(&read_path, &read);
        let error = certify_process_capture_input_witnesses(&read_path, read_summary, limits())
            .unwrap_err();
        let error = error.to_string();
        assert!(error.contains("unwitnessed regular-file reads"));
        assert!(error.contains("other=1"));

        let mut pipe_read = all_event_fixture_receipt();
        let attempt = &mut pipe_read.processes[0].read_attempts[0];
        attempt.request.descriptor = 3;
        attempt.request.identity = Some(CapturedDescriptorIdentity {
            target: b"pipe:[3]".to_vec(),
            device: 0,
            inode: 3,
            kind: CapturedDescriptorKind::Fifo,
        });
        attempt.request.mount_id = Some(1);
        attempt.outcome = CapturedReadOutcome::Read {
            bytes: 1,
            digest: [5; 32],
            file_value: None,
        };
        let pipe_path = temp.path().join("pipe-read.stream");
        let pipe_summary = write_receipt(&pipe_path, &pipe_read);
        let pipe_seal =
            certify_process_capture_input_witnesses(&pipe_path, pipe_summary, limits()).unwrap();
        assert_eq!(pipe_seal.census().regular_file_reads(), 0);
        assert_eq!(pipe_seal.census().witnessed_regular_file_reads(), 0);

        let mut directory = all_event_fixture_receipt();
        let CapturedGetdents64Outcome::Enumerated {
            directory_value, ..
        } = &mut directory.processes[0].getdents64_attempts[0].outcome
        else {
            panic!("fixture getdents64 attempt is not successful");
        };
        *directory_value = CapturedDirectoryValue::Unwitnessed;
        let directory_path = temp.path().join("unwitnessed-directory.stream");
        let directory_summary = write_receipt(&directory_path, &directory);
        let error =
            certify_process_capture_input_witnesses(&directory_path, directory_summary, limits())
                .unwrap_err();
        assert!(error
            .to_string()
            .contains("unwitnessed directory enumerations"));
    }

    #[cfg(feature = "test-fixtures")]
    #[test]
    fn downstream_test_fixture_writes_complete_exec_chain() {
        let temp = private_tempdir();
        let path = temp.path().join("downstream-fixture.stream");
        let mut first = fixture_invocation();
        first.file_mappings.clear();
        let mut second = first.clone();
        second.executable = b"/fixture/second".to_vec();
        second.argv = vec![b"second".to_vec()];

        let summary =
            write_process_capture_stream_test_fixture(&path, IDENTITY, limits(), &[first, second])
                .unwrap();
        let seal = certify_process_capture_input_witnesses(&path, summary, limits()).unwrap();
        assert_eq!(seal.stream().process_count(), 1);
        assert_eq!(seal.stream().census().invocations, 2);
        assert_eq!(seal.stream().census().exec_attempts, 1);
        assert_eq!(seal.census().file_mappings(), 2);
        assert_eq!(seal.census().witnessed_file_mappings(), 2);
    }

    #[cfg(feature = "test-fixtures")]
    #[test]
    fn downstream_tree_fixture_writes_explicit_process_lifecycles() {
        let temp = private_tempdir();
        let path = temp.path().join("downstream-tree-fixture.stream");
        let invocation = |executable: &[u8]| {
            let mut invocation = fixture_invocation();
            invocation.executable = executable.to_vec();
            invocation.argv = vec![executable.to_vec()];
            invocation.file_mappings.clear();
            invocation
        };
        let processes = vec![
            ProcessCaptureStreamTestProcess {
                process: 10,
                parent: None,
                invocations: vec![invocation(b"/fixture/analyzer")],
                outcome: CapturedProcessOutcome::Exited(0),
            },
            ProcessCaptureStreamTestProcess {
                process: 20,
                parent: Some(10),
                invocations: vec![
                    invocation(b"/fixture/action-a"),
                    invocation(b"/fixture/action-a-helper"),
                ],
                outcome: CapturedProcessOutcome::Exited(0),
            },
            ProcessCaptureStreamTestProcess {
                process: 30,
                parent: Some(20),
                invocations: Vec::new(),
                outcome: CapturedProcessOutcome::Signaled(libc::SIGTERM),
            },
            ProcessCaptureStreamTestProcess {
                process: 40,
                parent: Some(10),
                invocations: vec![invocation(b"/fixture/action-b")],
                outcome: CapturedProcessOutcome::Exited(7),
            },
        ];

        let summary =
            write_process_capture_stream_tree_test_fixture(&path, IDENTITY, limits(), &processes)
                .unwrap();
        assert_eq!(summary.process_count(), 4);
        assert_eq!(summary.census().invocations, 4);
        assert_eq!(summary.census().exec_attempts, 3);
        assert_eq!(summary.census().syscall_entries, 3);
        assert_eq!(summary.census().syscall_exits, 3);

        let (receipt, seal) = load_process_capture_stream(&path, summary, limits()).unwrap();
        assert_eq!(seal.summary, summary);
        assert_eq!(receipt.root, 10);
        assert_eq!(receipt.processes.len(), 4);
        for (actual, expected) in receipt.processes.iter().zip(&processes) {
            assert_eq!(actual.id, expected.process);
            assert_eq!(actual.parent, expected.parent);
            assert_eq!(actual.invocations.len(), expected.invocations.len());
            assert_eq!(actual.outcome, expected.outcome);
        }
        assert_eq!(receipt.processes[0].exec_attempts.len(), 0);
        assert_eq!(receipt.processes[1].exec_attempts.len(), 2);
        assert_eq!(
            receipt.processes[1].exec_attempts[0].outcome,
            CapturedExecOutcome::Loaded(0)
        );
        assert_eq!(
            receipt.processes[1].exec_attempts[1].outcome,
            CapturedExecOutcome::Loaded(1)
        );
        assert_eq!(receipt.processes[2].exec_attempts.len(), 0);
        assert_eq!(
            receipt.processes[3].exec_attempts[0].outcome,
            CapturedExecOutcome::Loaded(0)
        );

        let (visitor, visited_seal) =
            visit_process_capture_stream(&path, summary, limits(), CountingVisitor::default())
                .unwrap();
        assert_eq!(visited_seal, seal);
        assert_eq!(visitor.process_started, 4);
        assert_eq!(visitor.process_finished, 4);
        assert_eq!(visitor.invocations, 4);
        assert_eq!(visitor.exec_attempts, 3);

        let witness = certify_process_capture_input_witnesses(&path, summary, limits()).unwrap();
        assert_eq!(witness.census().file_mappings(), 4);
        assert_eq!(witness.census().witnessed_file_mappings(), 4);
    }

    #[cfg(feature = "test-fixtures")]
    #[test]
    fn downstream_tree_fixture_writes_regular_file_attempts() {
        let temp = private_tempdir();
        let path = temp.path().join("downstream-write-fixture.stream");
        let mut invocation = fixture_invocation();
        invocation.file_mappings.clear();
        let processes = vec![
            ProcessCaptureStreamTestProcess {
                process: 10,
                parent: None,
                invocations: vec![invocation.clone()],
                outcome: CapturedProcessOutcome::Exited(0),
            },
            ProcessCaptureStreamTestProcess {
                process: 20,
                parent: Some(10),
                invocations: vec![invocation],
                outcome: CapturedProcessOutcome::Exited(0),
            },
        ];
        let writes = vec![
            ProcessCaptureStreamTestWrite {
                process: 20,
                target: b"/output/first".to_vec(),
                bytes: b"x".to_vec(),
                mode: libc::S_IFREG | 0o644,
                device: 11,
                inode: 21,
                mount_id: 31,
            },
            ProcessCaptureStreamTestWrite {
                process: 20,
                target: b"/output/second".to_vec(),
                bytes: b"x".to_vec(),
                mode: libc::S_IFREG | 0o644,
                device: 12,
                inode: 22,
                mount_id: 32,
            },
        ];

        let summary = write_process_capture_stream_tree_with_writes_test_fixture(
            &path,
            IDENTITY,
            limits(),
            &processes,
            &writes,
        )
        .unwrap();
        assert_eq!(summary.census().write_attempts, 2);
        assert_eq!(summary.census().syscall_entries, 3);
        assert_eq!(summary.census().syscall_exits, 3);
        let (receipt, seal) = load_process_capture_stream(&path, summary, limits()).unwrap();
        assert_eq!(seal.summary, summary);
        assert!(receipt.processes[0].write_attempts.is_empty());
        assert_eq!(receipt.processes[1].write_attempts.len(), 2);
        assert_eq!(receipt.processes[1].write_attempts[0].syscall_index, 1);
        assert_eq!(receipt.processes[1].write_attempts[1].syscall_index, 2);
        assert_eq!(
            receipt.processes[1].write_attempts[1]
                .request
                .identity
                .as_ref()
                .unwrap()
                .target,
            b"/output/second"
        );
        let second_request = &receipt.processes[1].write_attempts[1].request;
        assert_eq!(second_request.identity.as_ref().unwrap().device, 12);
        assert_eq!(second_request.identity.as_ref().unwrap().inode, 22);
        assert_eq!(second_request.mount_id, Some(32));
    }

    #[cfg(feature = "test-fixtures")]
    #[test]
    fn downstream_tree_fixture_rejects_invalid_topology() {
        let temp = private_tempdir();
        let invocation = fixture_invocation();
        let process = |process, parent, invocations| ProcessCaptureStreamTestProcess {
            process,
            parent,
            invocations,
            outcome: CapturedProcessOutcome::Exited(0),
        };

        assert!(write_process_capture_stream_tree_test_fixture(
            &temp.path().join("empty.stream"),
            IDENTITY,
            limits(),
            &[],
        )
        .is_err());
        assert!(write_process_capture_stream_tree_test_fixture(
            &temp.path().join("root-without-invocation.stream"),
            IDENTITY,
            limits(),
            &[process(1, None, Vec::new())],
        )
        .is_err());
        assert!(write_process_capture_stream_tree_test_fixture(
            &temp.path().join("multiple-roots.stream"),
            IDENTITY,
            limits(),
            &[
                process(1, None, vec![invocation.clone()]),
                process(2, None, vec![invocation.clone()]),
            ],
        )
        .is_err());
        assert!(write_process_capture_stream_tree_test_fixture(
            &temp.path().join("child-before-parent.stream"),
            IDENTITY,
            limits(),
            &[
                process(2, Some(1), vec![invocation.clone()]),
                process(1, None, vec![invocation.clone()]),
            ],
        )
        .is_err());
        assert!(write_process_capture_stream_tree_test_fixture(
            &temp.path().join("duplicate.stream"),
            IDENTITY,
            limits(),
            &[
                process(1, None, vec![invocation.clone()]),
                process(1, Some(1), vec![invocation]),
            ],
        )
        .is_err());
        assert!(write_process_capture_stream_tree_with_writes_test_fixture(
            &temp.path().join("unknown-write-process.stream"),
            IDENTITY,
            limits(),
            &[process(1, None, vec![fixture_invocation()])],
            &[ProcessCaptureStreamTestWrite {
                process: 2,
                target: b"/output/value".to_vec(),
                bytes: b"x".to_vec(),
                mode: libc::S_IFREG | 0o644,
                device: 1,
                inode: 1,
                mount_id: 1,
            }],
        )
        .is_err());
        assert!(write_process_capture_stream_tree_with_writes_test_fixture(
            &temp.path().join("relative-write-target.stream"),
            IDENTITY,
            limits(),
            &[process(1, None, vec![fixture_invocation()])],
            &[ProcessCaptureStreamTestWrite {
                process: 1,
                target: b"relative/value".to_vec(),
                bytes: b"x".to_vec(),
                mode: libc::S_IFREG | 0o644,
                device: 1,
                inode: 1,
                mount_id: 1,
            }],
        )
        .is_err());
    }

    #[test]
    fn round_trips_terminal_symbolic_link_creation_coverage() {
        let mut receipt = all_event_fixture_receipt();
        let process = &mut receipt.processes[0];
        process.symbolic_link_creation_attempts[0].outcome =
            crate::CapturedSymbolicLinkCreationOutcome::Terminated;
        process.lifecycle_exit_attempts.clear();
        process.lifecycle_exit_syscalls = 0;
        process.syscall_entries -= 1;
        process.syscall_exits -= 1;
        process.terminal_syscalls = 1;
        receipt.validate_complete().unwrap();

        let temp = private_tempdir();
        let path = temp.path().join("terminal-symbolic-link.stream");
        let summary = write_receipt(&path, &receipt);
        let (loaded, seal) = load_process_capture_stream(&path, summary, limits()).unwrap();
        assert_eq!(loaded, receipt);
        assert_eq!(seal.summary, summary);

        let mut missing_terminal = loaded;
        missing_terminal.processes[0].terminal_syscalls = 0;
        assert!(missing_terminal.validate_complete().is_err());
    }

    #[test]
    fn round_trips_terminal_umask_coverage() {
        let mut receipt = all_event_fixture_receipt();
        let process = &mut receipt.processes[0];
        process.umask_attempts[0].outcome = crate::CapturedUmaskOutcome::Terminated;
        process.lifecycle_exit_attempts.clear();
        process.lifecycle_exit_syscalls = 0;
        process.syscall_entries -= 1;
        process.syscall_exits -= 1;
        process.terminal_syscalls = 1;
        receipt.validate_complete().unwrap();

        let temp = private_tempdir();
        let path = temp.path().join("terminal-umask.stream");
        let summary = write_receipt(&path, &receipt);
        let (loaded, seal) = load_process_capture_stream(&path, summary, limits()).unwrap();
        assert_eq!(loaded, receipt);
        assert_eq!(seal.summary, summary);

        let mut missing_terminal = loaded;
        missing_terminal.processes[0].terminal_syscalls = 0;
        assert!(missing_terminal.validate_complete().is_err());
    }

    #[test]
    fn round_trips_terminal_credential_coverage() {
        let mut receipt = all_event_fixture_receipt();
        let process = &mut receipt.processes[0];
        process.credential_attempts[0].outcome = crate::CapturedCredentialOutcome::Terminated;
        process.lifecycle_exit_attempts.clear();
        process.lifecycle_exit_syscalls = 0;
        process.syscall_entries -= 1;
        process.syscall_exits -= 1;
        process.terminal_syscalls = 1;
        receipt.validate_complete().unwrap();

        let temp = private_tempdir();
        let path = temp.path().join("terminal-credential.stream");
        let summary = write_receipt(&path, &receipt);
        let (loaded, seal) = load_process_capture_stream(&path, summary, limits()).unwrap();
        assert_eq!(loaded, receipt);
        assert_eq!(seal.summary, summary);

        let mut missing_terminal = loaded;
        missing_terminal.processes[0].terminal_syscalls = 0;
        assert!(missing_terminal.validate_complete().is_err());
    }

    #[test]
    fn round_trips_terminal_futex_wake_coverage() {
        let mut receipt = all_event_fixture_receipt();
        let process = &mut receipt.processes[0];
        process.futex_wake_attempts[0].outcome = crate::CapturedFutexWakeOutcome::Terminated;
        process.lifecycle_exit_attempts.clear();
        process.lifecycle_exit_syscalls = 0;
        process.syscall_entries -= 1;
        process.syscall_exits -= 1;
        process.terminal_syscalls = 1;
        receipt.validate_complete().unwrap();

        let temp = private_tempdir();
        let path = temp.path().join("terminal-futex-wake.stream");
        let summary = write_receipt(&path, &receipt);
        let (loaded, seal) = load_process_capture_stream(&path, summary, limits()).unwrap();
        assert_eq!(loaded, receipt);
        assert_eq!(seal.summary, summary);

        let mut missing_terminal = loaded;
        missing_terminal.processes[0].terminal_syscalls = 0;
        assert!(missing_terminal.validate_complete().is_err());
    }

    #[test]
    fn round_trips_terminal_process_creation_coverage() {
        let mut receipt = all_event_fixture_receipt();
        let process = &mut receipt.processes[0];
        process.process_creation_attempts[0].outcome =
            crate::CapturedProcessCreationOutcome::Terminated;
        process.lifecycle_exit_attempts.clear();
        process.lifecycle_exit_syscalls = 0;
        process.syscall_entries -= 1;
        process.syscall_exits -= 1;
        process.terminal_syscalls = 1;
        receipt.validate_complete().unwrap();

        let temp = private_tempdir();
        let path = temp.path().join("terminal-process-creation.stream");
        let summary = write_receipt(&path, &receipt);
        let (loaded, seal) = load_process_capture_stream(&path, summary, limits()).unwrap();
        assert_eq!(loaded, receipt);
        assert_eq!(seal.summary, summary);

        let mut missing_terminal = loaded;
        missing_terminal.processes[0].terminal_syscalls = 0;
        assert!(missing_terminal.validate_complete().is_err());
    }

    #[test]
    fn round_trips_terminal_child_wait_coverage() {
        let mut receipt = all_event_fixture_receipt();
        let process = &mut receipt.processes[0];
        process.child_wait_attempts[0].outcome = crate::CapturedChildWaitOutcome::Terminated;
        process.lifecycle_exit_attempts.clear();
        process.lifecycle_exit_syscalls = 0;
        process.syscall_entries -= 1;
        process.syscall_exits -= 1;
        process.terminal_syscalls = 1;
        receipt.validate_complete().unwrap();

        let temp = private_tempdir();
        let path = temp.path().join("terminal-child-wait.stream");
        let summary = write_receipt(&path, &receipt);
        let (loaded, seal) = load_process_capture_stream(&path, summary, limits()).unwrap();
        assert_eq!(loaded, receipt);
        assert_eq!(seal.summary, summary);

        let mut missing_terminal = loaded;
        missing_terminal.processes[0].terminal_syscalls = 0;
        assert!(missing_terminal.validate_complete().is_err());
    }

    #[test]
    fn round_trips_terminal_process_identity_coverage() {
        let mut receipt = all_event_fixture_receipt();
        let process = &mut receipt.processes[0];
        process.process_identity_attempts[0].outcome =
            crate::CapturedProcessIdentityOutcome::Terminated;
        process.lifecycle_exit_attempts.clear();
        process.lifecycle_exit_syscalls = 0;
        process.syscall_entries -= 1;
        process.syscall_exits -= 1;
        process.terminal_syscalls = 1;
        receipt.validate_complete().unwrap();

        let temp = private_tempdir();
        let path = temp.path().join("terminal-process-identity.stream");
        let summary = write_receipt(&path, &receipt);
        let (loaded, seal) = load_process_capture_stream(&path, summary, limits()).unwrap();
        assert_eq!(loaded, receipt);
        assert_eq!(seal.summary, summary);

        let mut missing_terminal = loaded;
        missing_terminal.processes[0].terminal_syscalls = 0;
        assert!(missing_terminal.validate_complete().is_err());
    }

    #[test]
    fn round_trips_terminal_runtime_observation_coverage() {
        let mut receipt = all_event_fixture_receipt();
        let process = &mut receipt.processes[0];
        process.runtime_observation_attempts[0].syscall_index = 39;
        process.runtime_observation_attempts[0].outcome =
            crate::CapturedRuntimeObservationOutcome::Terminated;
        process.lifecycle_exit_attempts.clear();
        process.lifecycle_exit_syscalls = 0;
        process.syscall_entries -= 1;
        process.syscall_exits -= 1;
        process.terminal_syscalls = 1;
        receipt.validate_complete().unwrap();

        let temp = private_tempdir();
        let path = temp.path().join("terminal-runtime-observation.stream");
        let summary = write_receipt(&path, &receipt);
        let (loaded, seal) = load_process_capture_stream(&path, summary, limits()).unwrap();
        assert_eq!(loaded, receipt);
        assert_eq!(seal.summary, summary);

        let mut missing_terminal = loaded;
        missing_terminal.processes[0].terminal_syscalls = 0;
        assert!(missing_terminal.validate_complete().is_err());
    }

    #[test]
    fn round_trips_terminal_ioctl_coverage() {
        let mut receipt = all_event_fixture_receipt();
        let process = &mut receipt.processes[0];
        process.ioctl_attempts[0].outcome = crate::CapturedIoctlOutcome::Terminated;
        process.lifecycle_exit_attempts.clear();
        process.lifecycle_exit_syscalls = 0;
        process.syscall_entries -= 1;
        process.syscall_exits -= 1;
        process.terminal_syscalls = 1;
        receipt.validate_complete().unwrap();

        let temp = private_tempdir();
        let path = temp.path().join("terminal-ioctl.stream");
        let summary = write_receipt(&path, &receipt);
        let (loaded, seal) = load_process_capture_stream(&path, summary, limits()).unwrap();
        assert_eq!(loaded, receipt);
        assert_eq!(seal.summary, summary);

        let mut missing_terminal = loaded;
        missing_terminal.processes[0].terminal_syscalls = 0;
        assert!(missing_terminal.validate_complete().is_err());
    }

    #[test]
    fn round_trips_terminal_statx_coverage() {
        let mut receipt = all_event_fixture_receipt();
        let process = &mut receipt.processes[0];
        process.statx_attempts[0].outcome = CapturedStatxOutcome::Terminated;
        process.lifecycle_exit_attempts.clear();
        process.lifecycle_exit_syscalls = 0;
        process.syscall_entries -= 1;
        process.syscall_exits -= 1;
        process.terminal_syscalls = 1;
        receipt.validate_complete().unwrap();

        let temp = private_tempdir();
        let path = temp.path().join("terminal-statx.stream");
        let summary = write_receipt(&path, &receipt);
        let (loaded, seal) = load_process_capture_stream(&path, summary, limits()).unwrap();
        assert_eq!(loaded, receipt);
        assert_eq!(seal.summary, summary);

        let mut missing_terminal = loaded;
        missing_terminal.processes[0].terminal_syscalls = 0;
        assert!(missing_terminal.validate_complete().is_err());
    }

    #[test]
    fn round_trips_terminal_xattr_coverage() {
        let mut receipt = all_event_fixture_receipt();
        let process = &mut receipt.processes[0];
        process.xattr_attempts[0].outcome = CapturedXattrOutcome::Terminated;
        process.lifecycle_exit_attempts.clear();
        process.lifecycle_exit_syscalls = 0;
        process.syscall_entries -= 1;
        process.syscall_exits -= 1;
        process.terminal_syscalls = 1;
        receipt.validate_complete().unwrap();

        let temp = private_tempdir();
        let path = temp.path().join("terminal-xattr.stream");
        let summary = write_receipt(&path, &receipt);
        let (loaded, seal) = load_process_capture_stream(&path, summary, limits()).unwrap();
        assert_eq!(loaded, receipt);
        assert_eq!(seal.summary, summary);

        let mut missing_terminal = loaded;
        missing_terminal.processes[0].terminal_syscalls = 0;
        assert!(missing_terminal.validate_complete().is_err());
    }

    #[test]
    fn rejects_every_truncated_prefix_and_trailing_bytes() {
        let source = private_tempdir();
        let source_path = source.path().join("complete.stream");
        let summary = write_fixture(&source_path);
        let bytes = std::fs::read(&source_path).unwrap();
        for length in 0..bytes.len() {
            let temp = private_tempdir();
            let path = temp.path().join("truncated.stream");
            write_private(&path, &bytes[..length]);
            assert!(load_process_capture_stream(&path, summary, limits()).is_err());
        }

        let temp = private_tempdir();
        let path = temp.path().join("trailing.stream");
        let mut trailing = bytes;
        trailing.push(0);
        write_private(&path, &trailing);
        assert!(load_process_capture_stream(&path, summary, limits()).is_err());
    }

    #[test]
    fn rejects_corruption_identity_and_broader_permissions() {
        let source = private_tempdir();
        let source_path = source.path().join("complete.stream");
        let seal = write_fixture(&source_path);
        let bytes = std::fs::read(&source_path).unwrap();
        for index in [0, HEADER_BYTES as usize, bytes.len() / 2, bytes.len() - 1] {
            let temp = private_tempdir();
            let path = temp.path().join("corrupt.stream");
            let mut corrupt = bytes.clone();
            corrupt[index] ^= 1;
            write_private(&path, &corrupt);
            assert!(load_process_capture_stream(&path, seal, limits()).is_err());
        }
        let mut wrong = seal;
        wrong.identity = ProcessCaptureStreamIdentity::new([8; 16], [9; 32]);
        assert!(load_process_capture_stream(&source_path, wrong, limits()).is_err());
        let mut wrong_digest = seal;
        wrong_digest.digest[0] ^= 1;
        assert!(load_process_capture_stream(&source_path, wrong_digest, limits()).is_err());
        assert_eq!(seal.identity(), IDENTITY);

        std::fs::set_permissions(&source_path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(load_process_capture_stream(&source_path, seal, limits()).is_err());
    }

    #[test]
    fn rejects_digest_valid_sequence_and_footer_tampering() {
        let source = private_tempdir();
        let source_path = source.path().join("complete.stream");
        let summary = write_fixture(&source_path);
        let bytes = std::fs::read(&source_path).unwrap();

        let sequence_temp = private_tempdir();
        let sequence_path = sequence_temp.path().join("sequence.stream");
        let mut sequence = bytes.clone();
        sequence[HEADER_BYTES as usize + 8..HEADER_BYTES as usize + 16]
            .copy_from_slice(&1u64.to_le_bytes());
        recompute_stream_digest(&mut sequence);
        write_private(&sequence_path, &sequence);
        assert!(load_process_capture_stream(&sequence_path, summary, limits()).is_err());

        let footer_temp = private_tempdir();
        let footer_path = footer_temp.path().join("footer.stream");
        let mut footer = bytes;
        let footer_start = footer.len() - TRAILER_BYTES as usize - FOOTER_BYTES as usize;
        footer[footer_start + 12..footer_start + 20].copy_from_slice(&99u64.to_le_bytes());
        recompute_stream_digest(&mut footer);
        write_private(&footer_path, &footer);
        assert!(load_process_capture_stream(&footer_path, summary, limits()).is_err());

        let tag_temp = private_tempdir();
        let tag_path = tag_temp.path().join("tag.stream");
        let mut tag = std::fs::read(&source_path).unwrap();
        tag[HEADER_BYTES as usize] = 254;
        recompute_stream_digest(&mut tag);
        write_private(&tag_path, &tag);
        assert!(load_process_capture_stream(&tag_path, summary, limits()).is_err());

        let version_temp = private_tempdir();
        let version_path = version_temp.path().join("record-version.stream");
        let mut version = std::fs::read(&source_path).unwrap();
        version[HEADER_BYTES as usize + 4..HEADER_BYTES as usize + 8]
            .copy_from_slice(&2u32.to_le_bytes());
        recompute_stream_digest(&mut version);
        write_private(&version_path, &version);
        assert!(load_process_capture_stream(&version_path, summary, limits()).is_err());
    }

    #[test]
    fn enforces_reader_and_writer_budgets_without_a_footer() {
        let source = private_tempdir();
        let source_path = source.path().join("complete.stream");
        let seal = write_fixture(&source_path);
        let smaller_stream = ProcessCaptureStreamLimits::new(
            seal.stream_bytes() - 1,
            100_000,
            10_000,
            16 * 1024 * 1024,
        )
        .unwrap();
        assert!(load_process_capture_stream(&source_path, seal, smaller_stream).is_err());
        let fewer_records = ProcessCaptureStreamLimits::new(
            16 * 1024 * 1024,
            seal.record_count() - 1,
            10_000,
            16 * 1024 * 1024,
        )
        .unwrap();
        assert!(load_process_capture_stream(&source_path, seal, fewer_records).is_err());
        let fewer_processes =
            ProcessCaptureStreamLimits::new(16 * 1024 * 1024, 100_000, 1, 16 * 1024 * 1024)
                .unwrap();
        assert!(load_process_capture_stream(&source_path, seal, fewer_processes).is_err());

        let temp = private_tempdir();
        let path = temp.path().join("incomplete.stream");
        let tiny =
            ProcessCaptureStreamLimits::new(HEADER_BYTES + FOOTER_BYTES + TRAILER_BYTES, 1, 1, 1)
                .unwrap();
        let mut sink = StreamCaptureSink::create(&path, IDENTITY, tiny).unwrap();
        assert!(sink
            .record(ProcessCaptureEvent::ProcessStarted {
                process: 1,
                parent: None,
            })
            .is_err());
        drop(sink);
        assert!(load_process_capture_stream(&path, seal, limits()).is_err());
    }

    #[test]
    fn rejects_invalid_lifecycle_order_before_or_after_persistence() {
        let mut builder = ReceiptBuilder::default();
        assert!(builder
            .record(ProcessCaptureEvent::Invocation {
                process: 1,
                invocation: fixture_invocation(),
            })
            .is_err());
        builder
            .record(ProcessCaptureEvent::ProcessStarted {
                process: 1,
                parent: None,
            })
            .unwrap();
        assert!(builder
            .record(ProcessCaptureEvent::ProcessStarted {
                process: 2,
                parent: Some(99),
            })
            .is_err());
        builder
            .record(ProcessCaptureEvent::ProcessFinished {
                process: 1,
                summary: empty_finish(CapturedProcessOutcome::Exited(0)),
            })
            .unwrap();
        assert!(builder
            .record(ProcessCaptureEvent::Invocation {
                process: 1,
                invocation: fixture_invocation(),
            })
            .is_err());

        let temp = private_tempdir();
        let path = temp.path().join("unfinished.stream");
        let mut sink = StreamCaptureSink::create(&path, IDENTITY, limits()).unwrap();
        sink.record(ProcessCaptureEvent::ProcessStarted {
            process: 1,
            parent: None,
        })
        .unwrap();
        assert!(sink.finish(1, &path).is_err());
        assert!(load_process_capture_stream(&path, placeholder_summary(), limits()).is_err());
    }

    #[test]
    fn rejects_lifecycle_exit_outcome_mismatch_before_persistence() {
        let mut validator = SemanticStreamValidator::default();
        validator
            .record(&ProcessCaptureEvent::ProcessStarted {
                process: 1,
                parent: None,
            })
            .unwrap();
        validator
            .record(&ProcessCaptureEvent::Invocation {
                process: 1,
                invocation: fixture_invocation(),
            })
            .unwrap();
        validator
            .record(&ProcessCaptureEvent::LifecycleExitAttempt {
                process: 1,
                attempt: crate::CapturedLifecycleExitAttempt {
                    syscall_index: 0,
                    request: crate::CapturedLifecycleExitRequest {
                        system_call: crate::CapturedLifecycleExitSystemCall::ExitGroup,
                        status: 0,
                    },
                    open_file_values: Vec::new(),
                    outcome: CapturedProcessOutcome::Exited(0),
                },
            })
            .unwrap();
        let mut finish = empty_finish(CapturedProcessOutcome::Exited(1));
        finish.invocations = 1;
        finish.lifecycle_exit_syscalls = 1;
        finish.syscall_entries = 1;
        finish.terminal_syscalls = 1;
        assert!(validator
            .record(&ProcessCaptureEvent::ProcessFinished {
                process: 1,
                summary: finish,
            })
            .is_err());
    }

    #[test]
    fn rejects_per_process_census_swaps_before_writing_a_finish() {
        let temp = private_tempdir();
        let path = temp.path().join("census-swap.stream");
        let mut sink = StreamCaptureSink::create(&path, IDENTITY, limits()).unwrap();
        sink.record(ProcessCaptureEvent::ProcessStarted {
            process: 1,
            parent: None,
        })
        .unwrap();
        sink.record(ProcessCaptureEvent::Invocation {
            process: 1,
            invocation: fixture_invocation(),
        })
        .unwrap();
        sink.record(ProcessCaptureEvent::ProcessStarted {
            process: 2,
            parent: Some(1),
        })
        .unwrap();
        assert!(sink
            .record(ProcessCaptureEvent::ProcessFinished {
                process: 1,
                summary: empty_finish(CapturedProcessOutcome::Exited(0)),
            })
            .is_err());
        drop(sink);
        assert!(load_process_capture_stream(&path, placeholder_summary(), limits()).is_err());
    }

    #[test]
    fn applies_the_frame_budget_during_invocation_acquisition() {
        let temp = private_tempdir();
        let path = temp.path().join("bounded-acquisition.stream");
        let tiny = ProcessCaptureStreamLimits::new(1024 * 1024, 100, 10, 1).unwrap();
        let mut command = Command::new("/bin/true");
        assert!(capture_process_tree_to_path(&mut command, &path, IDENTITY, tiny).is_err());
        assert!(path.exists());
        assert!(load_process_capture_stream(&path, placeholder_summary(), limits()).is_err());
    }

    #[test]
    fn rejects_hard_links_and_nonprivate_parents() {
        let private = private_tempdir();
        let path = private.path().join("capture.stream");
        let summary = write_fixture(&path);
        let alias = private.path().join("alias.stream");
        std::fs::hard_link(&path, &alias).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().nlink(), 2);
        assert!(load_process_capture_stream(&path, summary, limits()).is_err());

        let broad = private_tempdir();
        std::fs::set_permissions(broad.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let broad_path = broad.path().join("capture.stream");
        assert!(StreamCaptureSink::create(&broad_path, IDENTITY, limits()).is_err());
        assert!(!broad_path.exists());
    }

    #[test]
    fn rejects_symlinks_directories_and_fifos_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let temp = private_tempdir();
        let complete = temp.path().join("complete.stream");
        let summary = write_fixture(&complete);
        let symlink = temp.path().join("symlink.stream");
        std::os::unix::fs::symlink(&complete, &symlink).unwrap();
        assert!(load_process_capture_stream(&symlink, summary, limits()).is_err());
        assert!(load_process_capture_stream(temp.path(), summary, limits()).is_err());

        let fifo = temp.path().join("fifo.stream");
        let fifo_bytes = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: the path is NUL-terminated and points into an owner-private
        // temporary directory for the duration of the call.
        assert_eq!(unsafe { libc::mkfifo(fifo_bytes.as_ptr(), 0o600) }, 0);
        assert!(load_process_capture_stream(&fifo, summary, limits()).is_err());
    }
}
