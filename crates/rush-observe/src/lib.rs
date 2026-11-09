//! Generic process observation for external build capture.
//!
//! Raw receipts contain exact command environments and absolute host paths.
//! They are private capture inputs, not privacy-safe artifacts. An adapter must
//! root, filter, and certify their contents before publication.

#![warn(missing_docs)]

#[cfg(target_os = "linux")]
mod linux;
#[cfg(unix)]
mod stream;

#[cfg(target_os = "linux")]
pub use linux::{
    capture_prepared_process_tree_with_output, capture_process_tree,
    capture_process_tree_with_output, install_process_tree_capture, ProcessCaptureOutput,
};
#[cfg(unix)]
pub use stream::{
    capture_process_tree_to_path, census_process_capture_input_witnesses,
    certify_process_capture_input_witnesses, load_process_capture_stream,
    visit_process_capture_stream, ProcessCaptureInputWitnessCensus, ProcessCaptureInputWitnessSeal,
    ProcessCaptureStreamCensus, ProcessCaptureStreamEvent, ProcessCaptureStreamIdentity,
    ProcessCaptureStreamLimits, ProcessCaptureStreamSeal, ProcessCaptureStreamSummary,
    ProcessCaptureStreamVisitor, PROCESS_CAPTURE_STREAM_VERSION,
};
#[cfg(all(unix, feature = "test-fixtures"))]
pub use stream::{
    write_process_capture_stream_test_fixture, write_process_capture_stream_tree_test_fixture,
    write_process_capture_stream_tree_with_final_values_and_transient_symlinks_test_fixture,
    write_process_capture_stream_tree_with_final_values_test_fixture,
    write_process_capture_stream_tree_with_reads_and_writes_test_fixture,
    write_process_capture_stream_tree_with_writes_and_lifecycle_exits_test_fixture,
    write_process_capture_stream_tree_with_writes_test_fixture,
    write_process_capture_stream_unmodeled_completed_test_fixture,
    write_process_capture_stream_unmodeled_terminal_test_fixture,
    write_process_capture_stream_terminal_open_test_fixture,
    ProcessCaptureStreamTestFinalValueBoundary, ProcessCaptureStreamTestProcess,
    ProcessCaptureStreamTestRead, ProcessCaptureStreamTestTransientSymlink,
    ProcessCaptureStreamTestWrite,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const LINUX_O_WRONLY: u64 = 1;
const LINUX_O_CREAT: u64 = 0o100;
const LINUX_O_TRUNC: u64 = 0o1000;
const LINUX_O_TMPFILE: u64 = 0o20200000;
const LINUX_UIO_MAXIOV: usize = 1024;
const LINUX_DIRENT64_HEADER_SIZE: usize = 19;
const LINUX_DIRENT64_MINIMUM_RECORD_SIZE: u16 = 24;
const LINUX_DIRENT64_ALIGNMENT: u16 = 8;
const LINUX_PATH_MAX: usize = 4096;
const LINUX_IDENTITY_BYTES_LIMIT: usize = 8 * 1024 * 1024;
const LINUX_GETDENTS64_RESULT_LIMIT: u32 = 8 * 1024 * 1024;
const LINUX_READLINK_RESULT_LIMIT: usize = 8 * 1024 * 1024;
const LINUX_GETCWD_RESULT_LIMIT: usize = 8 * 1024 * 1024;
const LINUX_MAP_ANONYMOUS: u64 = 0x20;
const LINUX_AT_SYMLINK_NOFOLLOW: u32 = 0x100;
const LINUX_AT_EACCESS: u32 = 0x200;
const LINUX_AT_REMOVEDIR: u32 = 0x200;
const LINUX_AT_NO_AUTOMOUNT: u32 = 0x800;
const LINUX_AT_EMPTY_PATH: u32 = 0x1000;
const LINUX_AT_STATX_SYNC_TYPE: u32 = 0x6000;
const LINUX_STATX_TYPE: u32 = 0x0000_0001;
const LINUX_STATX_INO: u32 = 0x0000_0100;
const LINUX_STATX_MNT_ID: u32 = 0x0000_1000;
const LINUX_STATX_MNT_ID_UNIQUE: u32 = 0x0000_4000;
const LINUX_STATX_SUPPORTED_RESULTS: u32 = 0x0003_ffff;
const LINUX_STATX_RESERVED: u32 = 0x8000_0000;
const LINUX_XATTR_NAME_MAX: usize = 255;
const LINUX_XATTR_SIZE_MAX: usize = 65_536;
const LINUX_RENAME_NOREPLACE: u32 = 1;
const LINUX_RENAME_EXCHANGE: u32 = 2;
const LINUX_RENAME_WHITEOUT: u32 = 4;
const LINUX_UTIME_NOW: i64 = (1 << 30) - 1;
const LINUX_UTIME_OMIT: i64 = (1 << 30) - 2;
const LINUX_GRND_NONBLOCK: u32 = 1;
const LINUX_GRND_RANDOM: u32 = 2;
const LINUX_GRND_INSECURE: u32 = 4;
const LINUX_RLIMIT_RTTIME: u32 = 15;
const LINUX_SIGNAL_SET_BYTES_LIMIT: usize = 128;
const LINUX_RT_SIGACTION_PREFIX_BYTES: usize = 24;
const LINUX_POLLIN: i16 = 0x0001;
const LINUX_POLLOUT: i16 = 0x0004;
const LINUX_POLLHUP: i16 = 0x0010;
const LINUX_POLLNVAL: i16 = 0x0020;
const LINUX_CPU_AFFINITY_CAPACITY_LIMIT: u64 = 128;

/// Stable schema for a complete process-lifecycle receipt.
pub const PROCESS_CAPTURE_RECEIPT_SCHEMA: &str = "org.rushbuild.process-capture-receipt";

/// Current process-lifecycle receipt layout.
pub const PROCESS_CAPTURE_RECEIPT_VERSION: u32 = 79;

pub(crate) const COMPLETE_DIRECTORY_DIGEST_DOMAIN: &[u8] = b"org.rushbuild.complete-directory.v1\0";

/// Receipt-local process identity, independent of host PIDs.
pub type CapturedProcessId = u64;

/// Current evidence state for bytes exposed through a file-backed mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedMappedFileValue {
    /// Mapping origin is known, but consumed bytes were not observable.
    Unwitnessed,
    /// Conservative content value of the complete regular backing file.
    WholeFile {
        /// Exact file length hashed at the observation boundary.
        bytes: u64,
        /// BLAKE3 digest of every file byte in order.
        digest: [u8; 32],
        /// Stable native file type and permission mode surrounding that hash.
        mode: u32,
    },
}

/// Filesystem identity of an inode-backed mapping created during exec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedMappedFileIdentity {
    /// Lossless target bytes from the tracee's procfs map-files link.
    pub target: Vec<u8>,
    /// Encoded Linux device number reported by procfs.
    pub device: u64,
    /// Inode number reported by procfs.
    pub inode: u64,
    /// Kernel mount identifier for the mapped backing file.
    pub mount_id: u64,
}

/// One inode-backed virtual-memory area present at an exec boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedExecFileMapping {
    /// Inclusive virtual start address.
    pub start: u64,
    /// Exclusive virtual end address.
    pub end: u64,
    /// File offset mapped at `start`.
    pub file_offset: u64,
    /// Whether the area permits reads.
    pub readable: bool,
    /// Whether the area permits writes.
    pub writable: bool,
    /// Whether the area permits instruction fetches.
    pub executable: bool,
    /// Whether updates are shared with the backing object.
    pub shared: bool,
    /// Backing inode and lossless procfs target.
    pub identity: CapturedMappedFileIdentity,
    /// Explicit evidence state for mapped bytes.
    pub value: CapturedMappedFileValue,
}

/// Native Linux signal state inherited by one successful exec boundary.
///
/// Caught dispositions are reset by `execve`, while ignored dispositions and
/// the calling thread's blocked mask survive. Pending signals remain raw
/// evidence. A downstream adapter must prove that each nonzero bit is
/// reproducible or has no effect before admitting replay.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedExecSignalState {
    /// Blocked-signal bitset from `/proc/<pid>/status` `SigBlk`.
    pub blocked: u64,
    /// Ignored-signal bitset from `/proc/<pid>/status` `SigIgn`.
    pub ignored: u64,
    /// Caught-signal bitset from `/proc/<pid>/status` `SigCgt`.
    pub caught: u64,
    /// Thread-pending signal bitset from `/proc/<pid>/status` `SigPnd`.
    pub pending: u64,
    /// Process-shared pending bitset from `/proc/<pid>/status` `ShdPnd`.
    pub shared_pending: u64,
}

impl CapturedExecSignalState {
    fn validate(self) -> Result<()> {
        let unmaskable = (1_u64 << (libc::SIGKILL - 1)) | (1_u64 << (libc::SIGSTOP - 1));
        if (self.blocked | self.ignored | self.caught) & unmaskable != 0 {
            anyhow::bail!("process capture exec boundary masks an unmaskable signal");
        }
        if self.caught != 0 {
            anyhow::bail!("process capture exec boundary retains a caught signal");
        }
        Ok(())
    }
}

/// Exact invocation state observed at a successful exec boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedInvocation {
    /// Resolved executable path bytes from the stopped process.
    pub executable: Vec<u8>,
    /// Complete argument vector, including argv zero.
    pub argv: Vec<Vec<u8>>,
    /// Complete environment entries in their original `NAME=value` form.
    pub environment: Vec<Vec<u8>>,
    /// Working-directory path bytes immediately after exec.
    pub working_directory: Vec<u8>,
    /// Signal state inherited across this exec boundary.
    pub signal_state: CapturedExecSignalState,
    /// File-creation mask inherited across this exec boundary.
    pub file_creation_mask: u32,
    /// Process credentials inherited across this exec boundary.
    pub credentials: CapturedCredentialState,
    /// Inode-backed mappings installed at this exec boundary.
    pub file_mappings: Vec<CapturedExecFileMapping>,
}

/// Exact Linux process credentials at an exec or credential-change boundary.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedCredentialState {
    /// Real user identity.
    pub real_user: u32,
    /// Effective user identity.
    pub effective_user: u32,
    /// Saved-set user identity.
    pub saved_user: u32,
    /// Filesystem user identity used for permission checks.
    pub filesystem_user: u32,
    /// Real group identity.
    pub real_group: u32,
    /// Effective group identity.
    pub effective_group: u32,
    /// Saved-set group identity.
    pub saved_group: u32,
    /// Filesystem group identity used for permission checks.
    pub filesystem_group: u32,
    /// Canonical nondecreasing supplementary group identities.
    pub supplementary_groups: Vec<u32>,
    /// Inheritable Linux capability set.
    pub inheritable_capabilities: u64,
    /// Permitted Linux capability set.
    pub permitted_capabilities: u64,
    /// Effective Linux capability set.
    pub effective_capabilities: u64,
    /// Bounding Linux capability set.
    pub bounding_capabilities: u64,
    /// Ambient Linux capability set.
    pub ambient_capabilities: u64,
    /// Whether the Linux no-new-privileges bit is set.
    pub no_new_privileges: bool,
}

impl CapturedCredentialState {
    fn validate(&self) -> Result<()> {
        if [
            self.real_user,
            self.effective_user,
            self.saved_user,
            self.filesystem_user,
            self.real_group,
            self.effective_group,
            self.saved_group,
            self.filesystem_group,
        ]
        .contains(&u32::MAX)
            || self.supplementary_groups.len() > 65_536
            || self
                .supplementary_groups
                .iter()
                .any(|identity| *identity == u32::MAX)
            || self
                .supplementary_groups
                .windows(2)
                .any(|pair| pair[0] > pair[1])
        {
            anyhow::bail!("process capture credential state contains an invalid identity");
        }
        Ok(())
    }
}

/// Original arguments copied from a tracee at an exec syscall entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedExecRequest {
    /// Executable pathname exactly as supplied to the syscall.
    pub executable: Vec<u8>,
    /// Lexical cwd or directory-descriptor target for a relative pathname.
    pub directory: Option<Vec<u8>>,
    /// Complete requested argument vector.
    pub argv: Vec<Vec<u8>>,
    /// Complete requested environment in original `NAME=value` form.
    pub environment: Vec<Vec<u8>>,
    /// `execveat` flags, or zero for `execve`.
    pub flags: u32,
}

/// Kernel result of one observed exec request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedExecOutcome {
    /// Request loaded this entry in the process invocation vector.
    Loaded(u64),
    /// Request failed with this positive errno.
    Failed(i32),
}

/// One successful or failed exec request in syscall order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedExecAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Arguments copied while the tracee was stopped at syscall entry.
    pub request: CapturedExecRequest,
    /// Matching kernel result.
    pub outcome: CapturedExecOutcome,
}

/// Pathname-opening syscall whose request was captured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedOpenSystemCall {
    /// Legacy `open`.
    Open,
    /// Directory-relative `openat`.
    OpenAt,
    /// Extensible directory-relative `openat2`.
    OpenAt2,
    /// Legacy `creat`.
    Creat,
}

/// Original pathname and semantic arguments copied at open entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedOpenRequest {
    /// Opening operation selected by the tracee.
    pub system_call: CapturedOpenSystemCall,
    /// Pathname bytes exactly as supplied to the syscall.
    pub path: Vec<u8>,
    /// Lexical cwd or directory-descriptor target for a relative pathname.
    pub directory: Option<Vec<u8>>,
    /// Kernel open flags after decoding the syscall ABI.
    pub flags: u64,
    /// Requested creation mode after decoding the syscall ABI.
    pub mode: u64,
    /// `openat2` resolution flags, or zero for older operations.
    pub resolve: u64,
}

/// Kernel result of one observed pathname-opening request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedOpenOutcome {
    /// Request installed this descriptor for the resolved procfs target.
    Opened {
        /// Descriptor number in the tracee at the syscall exit stop.
        descriptor: i32,
        /// Resolved kernel object installed in the descriptor table.
        identity: CapturedDescriptorIdentity,
        /// Kernel mount identity reported for the installed descriptor.
        mount_id: u64,
        /// Stable whole-file value for a readable regular descriptor.
        ///
        /// Directories, special files, and descriptors opened without read
        /// access have no value here and require their own observation policy.
        file_value: Option<CapturedMappedFileValue>,
    },
    /// Request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the syscall was in progress.
    Terminated,
}

/// One completed or terminal pathname-opening attempt in syscall order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedOpenAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Arguments copied while the tracee was stopped at syscall entry.
    pub request: CapturedOpenRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedOpenOutcome,
}

/// Original pathname and lookup arguments for one native `newfstatat` call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedNewfstatatRequest {
    /// Directory descriptor supplied by the tracee; ignored for absolute paths.
    pub directory_descriptor: i32,
    /// Pathname bytes supplied by the tracee, excluding its terminating NUL.
    pub path: Vec<u8>,
    /// Whether the tracee supplied a null pathname pointer.
    pub path_is_null: bool,
    /// Entry-time cwd or descriptor target for a non-absolute pathname.
    ///
    /// This is absent for an absolute path or an invalid directory descriptor.
    pub base: Option<Vec<u8>>,
    /// Native Linux `AT_*` lookup flags.
    pub flags: u32,
}

/// Canonical fields returned by one successful native metadata lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedFileMetadata {
    /// Filesystem device number.
    pub device: u64,
    /// Filesystem inode number.
    pub inode: u64,
    /// File type and permission mode bits.
    pub mode: u32,
    /// Hard-link count.
    pub hard_links: u64,
    /// Owning user identifier.
    pub user: u32,
    /// Owning group identifier.
    pub group: u32,
    /// Device number represented by a special file.
    pub special_device: u64,
    /// Logical byte size.
    pub size: i64,
    /// Preferred I/O block size.
    pub block_size: i64,
    /// Allocated 512-byte block count.
    pub blocks: i64,
    /// Last-access time in seconds since the Unix epoch.
    pub access_time_seconds: i64,
    /// Nanosecond component of the last-access time.
    pub access_time_nanoseconds: i64,
    /// Last-modification time in seconds since the Unix epoch.
    pub modification_time_seconds: i64,
    /// Nanosecond component of the last-modification time.
    pub modification_time_nanoseconds: i64,
    /// Last-status-change time in seconds since the Unix epoch.
    pub change_time_seconds: i64,
    /// Nanosecond component of the last-status-change time.
    pub change_time_nanoseconds: i64,
}

/// Kernel result of one observed native `newfstatat` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedNewfstatatOutcome {
    /// Kernel copied these canonical metadata fields to tracee memory.
    Metadata(CapturedFileMetadata),
    /// Request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the syscall was in progress.
    Terminated,
}

/// One completed or terminal native `newfstatat` attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedNewfstatatAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Pathname and lookup arguments copied at syscall entry.
    pub request: CapturedNewfstatatRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedNewfstatatOutcome,
}

/// Original descriptor and result-buffer arguments for native `fstat`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedFstatRequest {
    /// Descriptor supplied by the tracee.
    pub descriptor: i32,
    /// Entry-time identity, absent only when the descriptor was invalid.
    pub identity: Option<CapturedDescriptorIdentity>,
    /// Entry-time mount identifier, present exactly with descriptor identity.
    pub mount_id: Option<u64>,
    /// Whether the tracee supplied a null result pointer.
    pub output_is_null: bool,
}

/// Kernel result of one native descriptor metadata request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedFstatOutcome {
    /// Kernel copied these canonical metadata fields to tracee memory.
    Metadata(CapturedFileMetadata),
    /// Request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the request was in progress.
    Terminated,
}

/// One completed or terminal native `fstat` attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedFstatAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Descriptor identity and result-buffer request copied at syscall entry.
    pub request: CapturedFstatRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedFstatOutcome,
}

/// Buffer and policy arguments for one native `getrandom` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedGetrandomRequest {
    /// Maximum random byte count requested from the kernel.
    pub requested_bytes: u64,
    /// Native Linux `GRND_*` policy flags.
    pub flags: u32,
    /// Whether the tracee supplied a null output pointer.
    pub output_is_null: bool,
}

/// Kernel result of one native randomness request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedGetrandomOutcome {
    /// Kernel delivered bytes whose value is bound by this digest.
    Bytes {
        /// Exact byte count returned by the kernel.
        returned_bytes: u64,
        /// BLAKE3 of the delivered bytes in order.
        digest: [u8; 32],
    },
    /// Request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the request was in progress.
    Terminated,
}

/// One completed or terminal native `getrandom` attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedGetrandomAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Buffer size and policy copied at syscall entry.
    pub request: CapturedGetrandomRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedGetrandomOutcome,
}

/// Exact process or host runtime values returned by supported native queries.
///
/// Persisting these values closes syscall accounting only. It does not make a
/// volatile observation replay-safe or authorize snapshot admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedResourceUsage {
    /// User CPU time, whole seconds.
    pub user_seconds: i64,
    /// User CPU time, fractional microseconds.
    pub user_microseconds: i64,
    /// System CPU time, whole seconds.
    pub system_seconds: i64,
    /// System CPU time, fractional microseconds.
    pub system_microseconds: i64,
    /// Maximum resident set size in the kernel ABI's units.
    pub maximum_resident_set_size: i64,
    /// Integral shared-memory size.
    pub integral_shared_memory_size: i64,
    /// Integral unshared-data size.
    pub integral_unshared_data_size: i64,
    /// Integral unshared-stack size.
    pub integral_unshared_stack_size: i64,
    /// Page faults serviced without I/O.
    pub minor_page_faults: i64,
    /// Page faults requiring I/O.
    pub major_page_faults: i64,
    /// Swap operations.
    pub swaps: i64,
    /// Block input operations.
    pub block_inputs: i64,
    /// Block output operations.
    pub block_outputs: i64,
    /// IPC messages sent.
    pub messages_sent: i64,
    /// IPC messages received.
    pub messages_received: i64,
    /// Signals received.
    pub signals_received: i64,
    /// Voluntary context switches.
    pub voluntary_context_switches: i64,
    /// Involuntary context switches.
    pub involuntary_context_switches: i64,
}

/// Exact Linux `sysinfo` fields copied to a tracee.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedSystemInformation {
    /// Seconds since boot.
    pub uptime_seconds: i64,
    /// Kernel fixed-point load averages for one, five, and fifteen minutes.
    pub load_averages: [u64; 3],
    /// Total usable RAM in `memory_unit` units.
    pub total_ram: u64,
    /// Available RAM in `memory_unit` units.
    pub free_ram: u64,
    /// Shared RAM in `memory_unit` units.
    pub shared_ram: u64,
    /// Buffer RAM in `memory_unit` units.
    pub buffer_ram: u64,
    /// Total swap in `memory_unit` units.
    pub total_swap: u64,
    /// Free swap in `memory_unit` units.
    pub free_swap: u64,
    /// Current process count.
    pub process_count: u16,
    /// Total high memory in `memory_unit` units.
    pub total_high_memory: u64,
    /// Free high memory in `memory_unit` units.
    pub free_high_memory: u64,
    /// Byte multiplier for the memory fields.
    pub memory_unit: u32,
}

/// Exact Linux `uname` identity fields copied to a tracee.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedSystemIdentity {
    /// Operating-system name.
    pub system_name: Vec<u8>,
    /// Host name in the observed UTS namespace.
    pub node_name: Vec<u8>,
    /// Kernel release identifier.
    pub release: Vec<u8>,
    /// Kernel build version.
    pub version: Vec<u8>,
    /// Machine architecture name.
    pub machine: Vec<u8>,
    /// NIS or domain name in the observed UTS namespace.
    pub domain_name: Vec<u8>,
}

/// Supported native runtime-observation requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedRuntimeObservationRequest {
    /// `getrusage(RUSAGE_SELF, ...)` for the calling process.
    CallingProcessResourceUsage {
        /// Whether the tracee supplied a null result pointer.
        output_is_null: bool,
    },
    /// Linux `sysinfo(...)` for host load and memory state.
    SystemInformation {
        /// Whether the tracee supplied a null result pointer.
        output_is_null: bool,
    },
    /// Linux `uname(...)` for UTS namespace and kernel identity.
    SystemIdentity {
        /// Whether the tracee supplied a null result pointer.
        output_is_null: bool,
    },
    /// Linux `sched_getaffinity(0, capacity, ...)` for the calling thread.
    CallingThreadAffinity {
        /// Native byte capacity supplied for the CPU mask.
        capacity: u64,
        /// Whether the tracee supplied a null result pointer.
        output_is_null: bool,
    },
    /// Nonblocking readiness query over a measured descriptor set.
    DescriptorReadiness {
        /// Descriptor identities and requested event masks in array order.
        descriptors: Vec<CapturedPollDescriptor>,
        /// Native timeout in milliseconds; zero is the supported probe.
        timeout_milliseconds: i32,
    },
}

/// One descriptor supplied to a native `poll` readiness query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedPollDescriptor {
    /// Descriptor number supplied by the tracee.
    pub descriptor: i32,
    /// Descriptor identity at entry, absent only for an invalid descriptor.
    pub identity: Option<CapturedDescriptorIdentity>,
    /// Mount identifier at entry, present exactly when identity is present.
    pub mount_id: Option<u64>,
    /// Native requested event mask.
    pub requested_events: i16,
}

/// Result of one supported native runtime observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedRuntimeObservationOutcome {
    /// Exact process resource counters copied by `getrusage`.
    ResourceUsage(CapturedResourceUsage),
    /// Exact host state copied by `sysinfo`.
    SystemInformation(CapturedSystemInformation),
    /// Exact UTS namespace and kernel identity copied by `uname`.
    SystemIdentity(CapturedSystemIdentity),
    /// Exact CPU-affinity bytes copied by `sched_getaffinity`.
    CpuAffinity(Vec<u8>),
    /// Exact readiness count and returned event masks copied after `poll`.
    DescriptorReadiness {
        /// Number of descriptors with a nonzero returned event mask.
        ready: u32,
        /// Native returned event masks in request-array order.
        returned_events: Vec<i16>,
    },
    /// Request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the request was in progress.
    Terminated,
}

/// One completed or terminal supported native runtime observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedRuntimeObservationAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Query and pointer form copied at syscall entry.
    pub request: CapturedRuntimeObservationRequest,
    /// Matching copied value, failure, or terminal state.
    pub outcome: CapturedRuntimeObservationOutcome,
}

/// One native soft and hard process-resource limit pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedResourceLimit {
    /// Active soft limit, including `RLIM_INFINITY` as `u64::MAX`.
    pub current: u64,
    /// Hard ceiling, including `RLIM_INFINITY` as `u64::MAX`.
    pub maximum: u64,
}

/// Process, resource, and pointer form of one native `prlimit64` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedPrlimitRequest {
    /// Native process identifier; zero selects the calling process.
    pub process: i32,
    /// Native Linux `RLIMIT_*` resource number.
    pub resource: u32,
    /// Whether the request omitted a new limit and therefore cannot mutate it.
    pub new_limit_is_null: bool,
    /// Whether the request omitted the returned old-limit buffer.
    pub old_limit_is_null: bool,
}

/// Kernel result of one native process-resource limit request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedPrlimitOutcome {
    /// Read-only query returned this exact soft and hard limit pair.
    Limit(CapturedResourceLimit),
    /// Read-only pointer-free request validated the process and resource.
    Checked,
    /// Read-only request failed with this positive errno.
    Failed(i32),
    /// A legacy capture saw a mutation without copying its requested value.
    MutationUnsupported {
        /// Positive errno on failure, or `None` when the syscall returned zero.
        errno: Option<i32>,
    },
    /// The task terminated while the request was in progress.
    Terminated,
    /// A mutating request completed with the exact requested and prior limits.
    Mutated {
        /// Limit bytes copied from the tracee at syscall entry.
        requested: CapturedResourceLimit,
        /// Prior limit returned by the kernel, absent when not requested.
        previous: Option<CapturedResourceLimit>,
    },
    /// A mutating request failed after its exact requested limit was copied.
    MutationFailed {
        /// Limit bytes copied from the tracee at syscall entry.
        requested: CapturedResourceLimit,
        /// Positive Linux errno returned by the kernel.
        errno: i32,
    },
    /// The task terminated during a mutation whose requested limit was copied.
    MutationTerminated {
        /// Limit bytes copied from the tracee at syscall entry.
        requested: CapturedResourceLimit,
    },
}

/// One completed or terminal native `prlimit64` attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedPrlimitAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Process, resource, and pointer form copied at syscall entry.
    pub request: CapturedPrlimitRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedPrlimitOutcome,
}

/// Descriptor supplied to one native `close` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedCloseRequest {
    /// Descriptor number supplied by the tracee.
    pub descriptor: i32,
    /// Descriptor identity at entry, absent only for an invalid descriptor.
    pub identity: Option<CapturedDescriptorIdentity>,
    /// Mount identifier at entry, present exactly when identity is present.
    pub mount_id: Option<u64>,
}

/// Kernel result of one native descriptor-close request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedCloseOutcome {
    /// The descriptor was closed after this optional stable final file value.
    Closed {
        /// Whole-file value at the close boundary for supported regular files.
        file_value: Option<CapturedMappedFileValue>,
    },
    /// The request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the request was in progress.
    Terminated,
}

/// One completed or terminal native descriptor-close attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedCloseAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Descriptor copied at syscall entry.
    pub request: CapturedCloseRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedCloseOutcome,
}

/// Arguments of one native descriptor-positioning request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedSeekRequest {
    /// Descriptor number supplied by the tracee.
    pub descriptor: i32,
    /// Descriptor identity at entry, absent only for an invalid descriptor.
    pub identity: Option<CapturedDescriptorIdentity>,
    /// Mount identifier at entry, present exactly when identity is present.
    pub mount_id: Option<u64>,
    /// Signed native offset supplied by the tracee.
    pub offset: i64,
    /// Native Linux `SEEK_*` operation.
    pub whence: u64,
}

/// Kernel result of one native descriptor-positioning request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedSeekOutcome {
    /// The descriptor now has this nonnegative byte position.
    Positioned(u64),
    /// The request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the request was in progress.
    Terminated,
}

/// One completed or terminal native `lseek` attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedSeekAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Descriptor and positioning operation copied at syscall entry.
    pub request: CapturedSeekRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedSeekOutcome,
}

/// Arguments of one bounded native file-access advisory request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedFileAdviceRequest {
    /// Descriptor number supplied by the tracee.
    pub descriptor: i32,
    /// Descriptor identity at entry, absent only for an invalid descriptor.
    pub identity: Option<CapturedDescriptorIdentity>,
    /// Mount identifier at entry, present exactly when identity is present.
    pub mount_id: Option<u64>,
    /// First advised byte offset.
    pub offset: u64,
    /// Advised byte length; zero extends through end of file.
    pub length: u64,
    /// Native Linux `POSIX_FADV_*` policy.
    pub advice: i32,
}

/// Kernel result of one bounded native file-access advisory request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedFileAdviceOutcome {
    /// The advisory request completed.
    Completed,
    /// The request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the request was in progress.
    Terminated,
}

/// One completed or terminal native `fadvise64` attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedFileAdviceAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Descriptor and advice contract copied at syscall entry.
    pub request: CapturedFileAdviceRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedFileAdviceOutcome,
}

/// Native byte-range lock supplied to a descriptor-control request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedFcntlRecordLock {
    /// Native `F_RDLCK`, `F_WRLCK`, or `F_UNLCK` value.
    pub lock_type: i16,
    /// Native `SEEK_SET`, `SEEK_CUR`, or `SEEK_END` origin.
    pub whence: i16,
    /// Signed byte offset relative to `whence`.
    pub start: i64,
    /// Signed byte length; zero extends through end of file.
    pub length: i64,
}

/// Arguments of one native descriptor-control request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedFcntlRequest {
    /// Descriptor number supplied by the tracee.
    pub descriptor: i32,
    /// Descriptor identity at entry, absent only for an invalid descriptor.
    pub identity: Option<CapturedDescriptorIdentity>,
    /// Mount identifier at entry, present exactly when identity is present.
    pub mount_id: Option<u64>,
    /// Native Linux `F_*` command after kernel-width conversion.
    pub command: u32,
    /// Raw third syscall argument.
    pub argument: u64,
    /// Typed entry-time record lock for a supported pointer command.
    pub record_lock: Option<CapturedFcntlRecordLock>,
}

/// Kernel result of one native descriptor-control request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedFcntlOutcome {
    /// The request returned this exact value and optional duplicated descriptor state.
    Returned {
        /// Native return value.
        value: i64,
        /// Identity of a returned duplicated descriptor, when applicable.
        result_identity: Option<CapturedDescriptorIdentity>,
        /// Mount of a returned duplicated descriptor, when applicable.
        result_mount_id: Option<u64>,
    },
    /// The request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the request was in progress.
    Terminated,
}

/// One completed or terminal native `fcntl` attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedFcntlAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Descriptor-control operation copied at syscall entry.
    pub request: CapturedFcntlRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedFcntlOutcome,
}

/// One supported native descriptor `ioctl` probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedIoctlRequest {
    /// Descriptor number supplied by the tracee.
    pub descriptor: i32,
    /// Exact descriptor identity at syscall entry.
    pub identity: CapturedDescriptorIdentity,
    /// Mount containing the descriptor's kernel object.
    pub mount_id: u64,
    /// Native Linux ioctl request number.
    pub command: u64,
}

impl CapturedIoctlRequest {
    /// Whether this is an exact terminal-state query whose failed result can
    /// be retained as raw evidence. This does not authorize replay for the
    /// captured descriptor kind or identity.
    pub fn is_terminal_probe(&self) -> bool {
        self.command == libc::TCGETS as u64 || self.command == libc::TIOCGWINSZ as u64
    }
}

/// Result of one supported native descriptor `ioctl` probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedIoctlOutcome {
    /// The descriptor rejected the request with this positive errno.
    Failed(i32),
    /// The task terminated while the request was in progress.
    Terminated,
}

/// One completed or terminal supported descriptor `ioctl` probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedIoctlAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Descriptor identity and operation copied at syscall entry.
    pub request: CapturedIoctlRequest,
    /// Exact supported failure or terminal state.
    pub outcome: CapturedIoctlOutcome,
}

/// Arguments of one native `pipe2` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedPipeRequest {
    /// Native Linux `O_*` flags supplied by the tracee.
    pub flags: u32,
}

/// Kernel result of one native `pipe2` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedPipeOutcome {
    /// The kernel created both ends of one anonymous pipe.
    Created {
        /// Returned descriptor for the read end.
        read_descriptor: i32,
        /// Exact kernel-object identity of the returned read end.
        read_identity: CapturedDescriptorIdentity,
        /// Mount identifier of the returned read end.
        read_mount_id: u64,
        /// Returned descriptor for the write end.
        write_descriptor: i32,
        /// Exact kernel-object identity of the returned write end.
        write_identity: CapturedDescriptorIdentity,
        /// Mount identifier of the returned write end.
        write_mount_id: u64,
    },
    /// The request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the request was in progress.
    Terminated,
}

/// One completed or terminal native `pipe2` attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedPipeAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Pipe flags copied at syscall entry.
    pub request: CapturedPipeRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedPipeOutcome,
}

/// Arguments of one native `epoll_create1` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedEpollCreateRequest {
    /// Native Linux flags supplied by the tracee.
    pub flags: u32,
}

/// Kernel result of one native `epoll_create1` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedEpollCreateOutcome {
    /// The kernel created an anonymous event-poll descriptor.
    Created {
        /// Returned descriptor number.
        descriptor: i32,
        /// Exact kernel-object identity of the returned descriptor.
        identity: CapturedDescriptorIdentity,
        /// Mount identifier of the returned descriptor.
        mount_id: u64,
    },
    /// The request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the request was in progress.
    Terminated,
}

/// One completed or terminal native `epoll_create1` attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedEpollCreateAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Creation flags copied at syscall entry.
    pub request: CapturedEpollCreateRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedEpollCreateOutcome,
}

/// Arguments of one supported native `socket` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedSocketCreateRequest {
    /// Native address family supplied by the tracee.
    pub domain: i32,
    /// Native socket type and creation flags supplied by the tracee.
    pub socket_type: i32,
    /// Native protocol supplied by the tracee.
    pub protocol: i32,
}

/// Kernel result of one supported native `socket` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedSocketCreateOutcome {
    /// The kernel created a local stream-socket descriptor.
    Created {
        /// Returned descriptor number.
        descriptor: i32,
        /// Exact kernel-object identity of the returned descriptor.
        identity: CapturedDescriptorIdentity,
        /// Mount identifier of the returned descriptor.
        mount_id: u64,
    },
    /// The request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the request was in progress.
    Terminated,
}

/// One completed or terminal supported native `socket` attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedSocketCreateAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Socket creation arguments copied at syscall entry.
    pub request: CapturedSocketCreateRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedSocketCreateOutcome,
}

/// Canonical semantic form of one bounded native local-socket address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedUnixSocketAddress {
    /// No `sun_path` bytes were supplied.
    Unnamed,
    /// A filesystem pathname, excluding ignored bytes after its first NUL.
    Pathname {
        /// Nonempty pathname bytes before the first NUL.
        path: Vec<u8>,
        /// Whether the supplied bounded address contained a terminating NUL.
        terminated: bool,
    },
    /// An abstract-namespace name, excluding its leading NUL byte.
    Abstract {
        /// Exact name bytes; embedded and trailing NUL bytes are significant.
        name: Vec<u8>,
    },
}

/// Arguments and entry descriptor state of one supported native `connect`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedSocketConnectRequest {
    /// Socket descriptor supplied by the tracee.
    pub descriptor: i32,
    /// Exact socket identity while the tracee was stopped at syscall entry.
    pub identity: CapturedDescriptorIdentity,
    /// Mount identifier of the socket descriptor at syscall entry.
    pub mount_id: u64,
    /// Exact native byte length supplied for the bounded address.
    pub address_length: u32,
    /// Canonical semantic local-socket address.
    pub address: CapturedUnixSocketAddress,
}

/// Kernel result of one supported native local-socket `connect` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedSocketConnectOutcome {
    /// The socket connected and retained this exact descriptor identity.
    Connected {
        /// Exact socket identity while stopped at syscall exit.
        identity: CapturedDescriptorIdentity,
        /// Mount identifier of the socket descriptor at syscall exit.
        mount_id: u64,
    },
    /// The request failed and retained this exact descriptor identity.
    Failed {
        /// Positive native errno.
        errno: i32,
        /// Exact socket identity while stopped at syscall exit.
        identity: CapturedDescriptorIdentity,
        /// Mount identifier of the socket descriptor at syscall exit.
        mount_id: u64,
    },
    /// The task terminated while the request was in progress.
    Terminated,
}

/// One completed or terminal supported native local-socket `connect` attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedSocketConnectAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Address and descriptor state copied at syscall entry.
    pub request: CapturedSocketConnectRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedSocketConnectOutcome,
}

/// Supported native descriptor-duplication system call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedDescriptorDuplicationSystemCall {
    /// Allocate the lowest available descriptor with `dup`.
    Dup,
    /// Replace or retain one selected descriptor with `dup2`.
    Dup2,
    /// Replace one distinct selected descriptor with zero-flag `dup3`.
    Dup3,
}

/// Descriptor state supplied to one native duplication request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedDup2Request {
    /// Native duplication operation.
    pub system_call: CapturedDescriptorDuplicationSystemCall,
    /// Source descriptor number supplied by the tracee.
    pub source_descriptor: i32,
    /// Source identity at entry, absent only for an invalid descriptor.
    pub source_identity: Option<CapturedDescriptorIdentity>,
    /// Source mount at entry, present exactly when its identity is present.
    pub source_mount_id: Option<u64>,
    /// Expected allocated target for `dup`, or selected replacement target.
    pub target_descriptor: i32,
    /// Target identity before selected replacement, absent for `dup`.
    pub target_identity: Option<CapturedDescriptorIdentity>,
    /// Target mount before selected replacement, present with its identity.
    pub target_mount_id: Option<u64>,
}

/// Kernel result of one native descriptor-duplication request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedDup2Outcome {
    /// The target descriptor now names the source kernel object.
    Duplicated {
        /// Descriptor returned by the kernel.
        descriptor: i32,
        /// Exact identity installed at the target descriptor.
        identity: CapturedDescriptorIdentity,
        /// Mount installed at the target descriptor.
        mount_id: u64,
        /// Final value of a supported regular file replaced at the target.
        replaced_file_value: Option<CapturedMappedFileValue>,
    },
    /// The request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the request was in progress.
    Terminated,
}

/// One completed or terminal native descriptor-duplication attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedDup2Attempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Operation and descriptor state copied at syscall entry.
    pub request: CapturedDup2Request,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedDup2Outcome,
}

/// Exact native process signal-state request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedSignalStateRequest {
    /// Query or replace one signal disposition through `rt_sigaction`.
    Action {
        /// Native Linux signal number.
        signal: u32,
        /// Exact native action bytes copied at entry, absent for a pure query.
        action: Option<Vec<u8>>,
        /// Whether the tracee omitted the returned prior action.
        old_action_is_null: bool,
        /// Native signal-set byte count supplied to the kernel.
        signal_set_bytes: u64,
    },
    /// Query or change the calling thread's mask through `rt_sigprocmask`.
    Mask {
        /// Native Linux `SIG_*MASK` operation.
        operation: i32,
        /// Exact native mask bytes copied at entry, absent for a pure query.
        mask: Option<Vec<u8>>,
        /// Whether the tracee omitted the returned prior mask.
        old_mask_is_null: bool,
        /// Native signal-set byte count supplied to the kernel.
        signal_set_bytes: u64,
    },
}

/// Kernel result of one native signal-state request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedSignalStateOutcome {
    /// `rt_sigaction` completed and optionally returned the prior action bytes.
    ActionReturned {
        /// Exact prior action, present exactly when the caller requested it.
        previous: Option<Vec<u8>>,
    },
    /// `rt_sigprocmask` completed and optionally returned the prior mask bytes.
    MaskReturned {
        /// Exact prior mask, present exactly when the caller requested it.
        previous: Option<Vec<u8>>,
    },
    /// Request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the request was in progress.
    Terminated,
}

/// One completed or terminal native signal-state attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedSignalStateAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Exact signal operation and entry values.
    pub request: CapturedSignalStateRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedSignalStateOutcome,
}

/// Provenance class for one signal delivered at a ptrace signal-delivery stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedSignalSource {
    /// The kernel generated the signal without a sending process identity.
    Kernel,
    /// Another process in this receipt generated the signal.
    TracedProcess {
        /// Receipt-local sender identity.
        process: CapturedProcessId,
    },
    /// A process outside this receipt generated the signal.
    ExternalProcess {
        /// Host PID reported by `siginfo_t` in this private receipt.
        process: i32,
        /// Host user ID reported by `siginfo_t`.
        user: u32,
    },
}

/// Disposition installed when one signal was accepted for delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedSignalDisposition {
    /// The kernel's default disposition was installed.
    Default,
    /// The process ignored this signal.
    Ignored,
    /// The process installed a userspace handler for this signal.
    Caught,
}

/// One signal accepted for delivery to a traced process.
///
/// The stream order locates the stop relative to syscall attempts. The active
/// syscall index is retained explicitly because delivery may interrupt a
/// request before its matching exit stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedSignalDelivery {
    /// Native Linux signal number.
    pub signal: u32,
    /// Native `siginfo_t.si_code` provenance code.
    pub code: i32,
    /// Native `siginfo_t.si_errno` value.
    pub errno: i32,
    /// Process or kernel provenance derived from `siginfo_t`.
    pub source: CapturedSignalSource,
    /// `siginfo_t.si_status` for `SIGCHLD`, absent for other signals.
    pub child_status: Option<i32>,
    /// Number of syscall entries observed before this delivery stop.
    pub syscall_entries: u64,
    /// In-flight syscall entry index at the delivery stop, if any.
    pub active_syscall: Option<u64>,
    /// Signal disposition installed at the delivery stop.
    pub disposition: CapturedSignalDisposition,
}

pub(crate) fn validate_captured_signal_delivery(delivery: &CapturedSignalDelivery) -> Result<()> {
    let invalid_source = match delivery.source {
        CapturedSignalSource::Kernel => false,
        CapturedSignalSource::TracedProcess { process } => process == 0,
        CapturedSignalSource::ExternalProcess { process, .. } => process <= 0,
    };
    let unmaskable = matches!(
        delivery.signal,
        signal
            if signal == libc::SIGKILL as u32
                || signal == libc::SIGSTOP as u32
    );
    if delivery.signal == 0
        || delivery.signal > 64
        || delivery.child_status.is_some() != (delivery.signal == libc::SIGCHLD as u32)
        || (unmaskable
            && delivery.disposition != CapturedSignalDisposition::Default)
        || invalid_source
    {
        anyhow::bail!("process capture receipt has an invalid signal delivery");
    }
    Ok(())
}

pub(crate) fn captured_signal_delivery_creates_frame(
    delivery: &CapturedSignalDelivery,
) -> bool {
    delivery.disposition == CapturedSignalDisposition::Caught
}

/// Kernel result of one native `umask` transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedUmaskOutcome {
    /// The process mask changed and the kernel returned this prior mask.
    Changed(u32),
    /// The task terminated while the transition was in flight.
    Terminated,
}

/// One native file-creation mask transition in syscall order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedUmaskAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Raw mask supplied to the kernel after conversion to `mode_t`.
    pub requested: u32,
    /// Matching prior mask or terminal state.
    pub outcome: CapturedUmaskOutcome,
}

pub(crate) fn validate_captured_umask_attempt(attempt: &CapturedUmaskAttempt) -> Result<bool> {
    match attempt.outcome {
        CapturedUmaskOutcome::Changed(previous) if previous & !0o777 == 0 => Ok(false),
        CapturedUmaskOutcome::Terminated => Ok(true),
        CapturedUmaskOutcome::Changed(_) => {
            anyhow::bail!("process capture receipt has an invalid file-creation mask")
        }
    }
}

/// Exact credential query or no-op retention request issued by a process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedCredentialRequest {
    /// Query the real user identity with `getuid`.
    Getuid,
    /// Query the effective user identity with `geteuid`.
    Geteuid,
    /// Query the real group identity with `getgid`.
    Getgid,
    /// Query the effective group identity with `getegid`.
    Getegid,
    /// Query supplementary group identities with `getgroups`.
    Getgroups {
        /// Maximum number of group identities accepted by the output buffer.
        capacity: u32,
        /// Whether the tracee supplied a null output pointer.
        output_is_null: bool,
    },
    /// Query one bit of the calling process's capability bounding set.
    CapabilityBoundingSetRead {
        /// Native Linux capability number supplied to `PR_CAPBSET_READ`.
        capability: u32,
    },
    /// Request real, effective, and saved-set user identities with `setresuid`.
    Setresuid {
        /// Requested real identity, or `None` for the native all-ones sentinel.
        real: Option<u32>,
        /// Requested effective identity, or `None` for the native all-ones sentinel.
        effective: Option<u32>,
        /// Requested saved-set identity, or `None` for the native all-ones sentinel.
        saved: Option<u32>,
    },
    /// Request real, effective, and saved-set group identities with `setresgid`.
    Setresgid {
        /// Requested real identity, or `None` for the native all-ones sentinel.
        real: Option<u32>,
        /// Requested effective identity, or `None` for the native all-ones sentinel.
        effective: Option<u32>,
        /// Requested saved-set identity, or `None` for the native all-ones sentinel.
        saved: Option<u32>,
    },
}

/// Kernel result of one native credential request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedCredentialOutcome {
    /// A query returned this native identity.
    Identity(u32),
    /// A zero-capacity query returned this required group count.
    SupplementaryGroupCount(u32),
    /// A bounded query returned these supplementary groups in kernel order.
    SupplementaryGroups(Vec<u32>),
    /// Whether the queried capability is present in the bounding set.
    CapabilityBoundingSetMembership(bool),
    /// A successful mutation left the process in this exact credential state.
    State(CapturedCredentialState),
    /// The request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the request was in flight.
    Terminated,
}

/// One native credential request in syscall order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedCredentialAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Exact credential operation and meaningful entry values.
    pub request: CapturedCredentialRequest,
    /// Matching identity, state, error, or terminal outcome.
    pub outcome: CapturedCredentialOutcome,
}

pub(crate) fn validate_captured_credential_attempt(
    attempt: &CapturedCredentialAttempt,
) -> Result<bool> {
    match (attempt.request, &attempt.outcome) {
        (
            CapturedCredentialRequest::Getuid
            | CapturedCredentialRequest::Geteuid
            | CapturedCredentialRequest::Getgid
            | CapturedCredentialRequest::Getegid,
            CapturedCredentialOutcome::Identity(identity),
        ) if *identity != u32::MAX => Ok(false),
        (
            CapturedCredentialRequest::Getgroups {
                capacity: 0,
                output_is_null: _,
            },
            CapturedCredentialOutcome::SupplementaryGroupCount(count),
        ) if *count <= 65_536 => Ok(false),
        (
            CapturedCredentialRequest::Getgroups {
                capacity,
                output_is_null,
            },
            CapturedCredentialOutcome::SupplementaryGroups(groups),
        ) if capacity > 0
            && capacity <= 65_536
            && groups.len() <= capacity as usize
            && groups.iter().all(|identity| *identity != u32::MAX)
            && groups.windows(2).all(|pair| pair[0] <= pair[1])
            && (!output_is_null || groups.is_empty()) =>
        {
            Ok(false)
        }
        (
            CapturedCredentialRequest::CapabilityBoundingSetRead { capability },
            CapturedCredentialOutcome::CapabilityBoundingSetMembership(_),
        ) if measured_capability_bounding_set_membership_query(capability) => Ok(false),
        (
            CapturedCredentialRequest::Setresuid {
                real,
                effective,
                saved,
            },
            CapturedCredentialOutcome::State(state),
        ) => {
            state.validate()?;
            if real.is_some_and(|identity| identity != state.real_user)
                || effective.is_some_and(|identity| {
                    identity != state.effective_user || identity != state.filesystem_user
                })
                || saved.is_some_and(|identity| identity != state.saved_user)
            {
                anyhow::bail!("process capture receipt has an inconsistent user transition");
            }
            Ok(false)
        }
        (
            CapturedCredentialRequest::Setresgid {
                real,
                effective,
                saved,
            },
            CapturedCredentialOutcome::State(state),
        ) => {
            state.validate()?;
            if real.is_some_and(|identity| identity != state.real_group)
                || effective.is_some_and(|identity| {
                    identity != state.effective_group || identity != state.filesystem_group
                })
                || saved.is_some_and(|identity| identity != state.saved_group)
            {
                anyhow::bail!("process capture receipt has an inconsistent group transition");
            }
            Ok(false)
        }
        (
            CapturedCredentialRequest::Setresuid { .. }
            | CapturedCredentialRequest::Setresgid { .. },
            CapturedCredentialOutcome::Failed(errno),
        ) if *errno > 0 => Ok(false),
        (
            CapturedCredentialRequest::Getgroups { capacity, .. },
            CapturedCredentialOutcome::Failed(errno),
        ) if capacity <= 65_536 && *errno > 0 => Ok(false),
        (
            CapturedCredentialRequest::CapabilityBoundingSetRead { capability },
            CapturedCredentialOutcome::Failed(errno),
        ) if measured_invalid_capability_bounding_set_query(capability)
            && *errno == libc::EINVAL =>
        {
            Ok(false)
        }
        (
            CapturedCredentialRequest::Getgroups { capacity, .. },
            CapturedCredentialOutcome::Terminated,
        ) if capacity <= 65_536 => Ok(true),
        (
            CapturedCredentialRequest::CapabilityBoundingSetRead { capability },
            CapturedCredentialOutcome::Terminated,
        ) if supported_capability_bounding_set_query(capability) => Ok(true),
        (
            CapturedCredentialRequest::Getuid
            | CapturedCredentialRequest::Geteuid
            | CapturedCredentialRequest::Getgid
            | CapturedCredentialRequest::Getegid
            | CapturedCredentialRequest::Setresuid { .. }
            | CapturedCredentialRequest::Setresgid { .. },
            CapturedCredentialOutcome::Terminated,
        ) => Ok(true),
        _ => anyhow::bail!("process capture receipt has a mismatched credential result"),
    }
}

pub(crate) fn supported_capability_bounding_set_query(capability: u32) -> bool {
    measured_capability_bounding_set_membership_query(capability)
        || measured_invalid_capability_bounding_set_query(capability)
}

fn measured_capability_bounding_set_membership_query(capability: u32) -> bool {
    matches!(capability, 32 | 40)
}

fn measured_invalid_capability_bounding_set_query(capability: u32) -> bool {
    matches!(capability, 41 | 42 | 44 | 48)
}

/// Arguments of one process-private native futex wake request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedFutexWakeRequest {
    /// Tracee virtual address naming the aligned futex word.
    pub address: u64,
    /// Maximum number of same-process waiters requested for wakeup.
    pub maximum_waiters: u32,
}

/// Kernel result of one process-private native futex wake request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedFutexWakeOutcome {
    /// The kernel woke this many same-process waiters.
    Woken(u32),
    /// The request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the request was in flight.
    Terminated,
}

/// One process-private native futex wake attempt in syscall order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedFutexWakeAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Exact meaningful arguments copied at syscall entry.
    pub request: CapturedFutexWakeRequest,
    /// Matching wake count, error, or terminal state.
    pub outcome: CapturedFutexWakeOutcome,
}

pub(crate) fn validate_captured_futex_wake_attempt(
    attempt: &CapturedFutexWakeAttempt,
) -> Result<bool> {
    if attempt.request.address == 0
        || attempt.request.address % std::mem::align_of::<u32>() as u64 != 0
        || attempt.request.maximum_waiters > i32::MAX as u32
    {
        anyhow::bail!("process capture receipt has an invalid private futex wake request");
    }
    match attempt.outcome {
        CapturedFutexWakeOutcome::Woken(waiters) if waiters <= attempt.request.maximum_waiters => {
            Ok(false)
        }
        CapturedFutexWakeOutcome::Failed(errno) if errno > 0 => Ok(false),
        CapturedFutexWakeOutcome::Terminated => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid private futex wake result"),
    }
}

/// Supported native process-creation operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedProcessCreationRequest {
    /// Create one child with the native `vfork` contract.
    Vfork,
    /// Create one child with the bounded legacy `clone` fork contract.
    Clone {
        /// Exact clone flags, including the child termination signal.
        flags: u64,
    },
    /// Create one child with the bounded `clone3` vfork contract.
    Clone3 {
        /// Exact clone flags copied from the request.
        flags: u64,
        /// Child termination signal copied from the request.
        exit_signal: u32,
    },
}

pub(crate) fn process_creation_shares_thread_group(
    request: CapturedProcessCreationRequest,
) -> bool {
    let flags = match request {
        CapturedProcessCreationRequest::Clone { flags }
        | CapturedProcessCreationRequest::Clone3 { flags, .. } => flags,
        CapturedProcessCreationRequest::Vfork => return false,
    };
    flags & 0x0001_0000 != 0
}

/// Kernel result of one supported process-creation request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedProcessCreationOutcome {
    /// The request created this receipt-local child process.
    Created(CapturedProcessId),
    /// The request failed with this positive errno.
    Failed(i32),
    /// The kernel interrupted the request with this internal restart code.
    Restarted(i32),
    /// The parent terminated while the request was in flight.
    Terminated,
}

/// One supported native process-creation attempt in syscall order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedProcessCreationAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Exact supported creation contract copied at syscall entry.
    pub request: CapturedProcessCreationRequest,
    /// Matching child, error, or terminal state.
    pub outcome: CapturedProcessCreationOutcome,
}

pub(crate) fn validate_captured_process_creation_attempt(
    attempt: &CapturedProcessCreationAttempt,
) -> Result<bool> {
    const CLONE_VM: u64 = 0x0000_0100;
    const CLONE_FS: u64 = 0x0000_0200;
    const CLONE_FILES: u64 = 0x0000_0400;
    const CLONE_SIGHAND: u64 = 0x0000_0800;
    const CLONE_VFORK: u64 = 0x0000_4000;
    const CLONE_THREAD: u64 = 0x0001_0000;
    const CLONE_SYSVSEM: u64 = 0x0004_0000;
    const CLONE_SETTLS: u64 = 0x0008_0000;
    const CLONE_PARENT_SETTID: u64 = 0x0010_0000;
    const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;
    const CLONE_CHILD_SETTID: u64 = 0x0100_0000;
    const CLONE_CLEAR_SIGHAND: u64 = 0x0000_0001_0000_0000;
    const SIGCHLD: u32 = 17;
    let thread_required = CLONE_VM
        | CLONE_FS
        | CLONE_FILES
        | CLONE_SIGHAND
        | CLONE_THREAD
        | CLONE_SYSVSEM;
    let thread_allowed = thread_required
        | CLONE_SETTLS
        | CLONE_PARENT_SETTID
        | CLONE_CHILD_CLEARTID
        | CLONE_CHILD_SETTID;
    let is_thread = |flags: u64| {
        flags & thread_required == thread_required && flags & !thread_allowed == 0
    };

    match attempt.request {
        CapturedProcessCreationRequest::Clone { flags }
            if flags != (CLONE_CHILD_CLEARTID | CLONE_CHILD_SETTID | u64::from(SIGCHLD))
                && !is_thread(flags) =>
        {
            anyhow::bail!("process capture receipt has an unsupported clone contract");
        }
        CapturedProcessCreationRequest::Clone3 { flags, exit_signal } => {
            let vfork_flags = CLONE_VM | CLONE_VFORK;
            let is_vfork = (flags == vfork_flags
                || flags == vfork_flags | CLONE_CLEAR_SIGHAND)
                && exit_signal == SIGCHLD;
            if !is_vfork && !(is_thread(flags) && exit_signal == 0)
            {
                anyhow::bail!("process capture receipt has an unsupported clone3 contract");
            }
        }
        _ => {}
    }
    match attempt.outcome {
        CapturedProcessCreationOutcome::Created(process) if process != 0 => Ok(false),
        CapturedProcessCreationOutcome::Failed(errno) if errno > 0 => Ok(false),
        CapturedProcessCreationOutcome::Restarted(513) => Ok(false),
        CapturedProcessCreationOutcome::Terminated => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid process-creation result"),
    }
}

/// Supported child-selection policy for one native `wait4` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedChildWaitSelector {
    /// Wait for any direct child.
    AnyChild,
    /// Wait for this receipt-local direct child.
    Child(CapturedProcessId),
}

/// Supported arguments of one native `wait4` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedChildWaitRequest {
    /// Child selection after translating any real PID to receipt-local identity.
    pub selector: CapturedChildWaitSelector,
    /// Whether the request used the exact `WNOHANG` option.
    pub nonblocking: bool,
    /// Whether the tracee declined the optional child-status result.
    pub status_is_null: bool,
}

/// Kernel result of one supported native `wait4` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedChildWaitOutcome {
    /// This receipt-local child was reaped with the exact native status word.
    Reaped {
        /// Reaped receipt-local child identity.
        child: CapturedProcessId,
        /// Raw status word written by the kernel.
        status: u32,
    },
    /// No matching child had changed state during a nonblocking request.
    NoChildReady,
    /// The request failed with this positive errno.
    Failed(i32),
    /// The parent terminated while the request was in flight.
    Terminated,
}

/// One supported native child-wait attempt in syscall order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedChildWaitAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Exact supported wait policy copied at syscall entry.
    pub request: CapturedChildWaitRequest,
    /// Matching child state, error, or terminal state.
    pub outcome: CapturedChildWaitOutcome,
}

pub(crate) fn validate_captured_child_wait_attempt(
    attempt: &CapturedChildWaitAttempt,
) -> Result<bool> {
    if matches!(
        attempt.request.selector,
        CapturedChildWaitSelector::Child(0)
    ) {
        anyhow::bail!("process capture receipt has an invalid child-wait selector");
    }
    if attempt.request.status_is_null
        && (attempt.request.selector != CapturedChildWaitSelector::AnyChild
            || attempt.request.nonblocking)
    {
        anyhow::bail!("process capture receipt has an invalid null-status child wait");
    }
    match attempt.outcome {
        CapturedChildWaitOutcome::Reaped { child, .. }
            if !attempt.request.status_is_null
                && child != 0
                && !matches!(
                    attempt.request.selector,
                    CapturedChildWaitSelector::Child(requested) if requested != child
                ) =>
        {
            Ok(false)
        }
        CapturedChildWaitOutcome::NoChildReady
            if attempt.request.nonblocking && !attempt.request.status_is_null =>
        {
            Ok(false)
        }
        CapturedChildWaitOutcome::Failed(libc::ECHILD) if attempt.request.status_is_null => {
            Ok(false)
        }
        CapturedChildWaitOutcome::Failed(errno)
            if errno > 0 && !attempt.request.status_is_null =>
        {
            Ok(false)
        }
        CapturedChildWaitOutcome::Terminated => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid child-wait result"),
    }
}

/// Supported native process-identity query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedProcessIdentityRequest {
    /// Query the calling process identity with `getpid`.
    Getpid,
    /// Query the calling task identity with `gettid`.
    Gettid,
    /// Query a captured parent identity with `getppid`.
    Getppid,
    /// Query the calling process's native process group with `getpgrp`.
    Getpgrp,
}

/// Kernel result of one supported process-identity query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedProcessIdentityOutcome {
    /// Receipt-local identity corresponding to the kernel result.
    Identity(CapturedProcessId),
    /// Positive native process-group identity returned by the kernel.
    ProcessGroup(u32),
    /// The process terminated while the request was in flight.
    Terminated,
}

/// One supported native process-identity query in syscall order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedProcessIdentityAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Exact supported identity query copied at syscall entry.
    pub request: CapturedProcessIdentityRequest,
    /// Receipt-local identity or terminal state.
    pub outcome: CapturedProcessIdentityOutcome,
}

pub(crate) fn validate_captured_process_identity_attempt(
    attempt: &CapturedProcessIdentityAttempt,
) -> Result<bool> {
    match (attempt.request, attempt.outcome) {
        (
            CapturedProcessIdentityRequest::Getpid
            | CapturedProcessIdentityRequest::Gettid
            | CapturedProcessIdentityRequest::Getppid,
            CapturedProcessIdentityOutcome::Identity(process),
        ) if process != 0 => Ok(false),
        (
            CapturedProcessIdentityRequest::Getpgrp,
            CapturedProcessIdentityOutcome::ProcessGroup(group),
        ) if group != 0 && i32::try_from(group).is_ok() => Ok(false),
        (_, CapturedProcessIdentityOutcome::Terminated) => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid process-identity result"),
    }
}

/// Arguments of one native process-memory state request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedMemoryStateRequest {
    /// Query or change the process break.
    Brk {
        /// Requested break address; zero queries the current break.
        address: u64,
    },
    /// Change access protections on one virtual-memory range.
    Mprotect {
        /// First virtual address in the range.
        address: u64,
        /// Requested byte length.
        length: u64,
        /// Native Linux `PROT_*` mask.
        protection: u32,
    },
    /// Remove mappings from one virtual-memory range.
    Munmap {
        /// First virtual address in the range.
        address: u64,
        /// Requested byte length.
        length: u64,
    },
}

/// Kernel result of one native process-memory state request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedMemoryStateOutcome {
    /// `brk` returned the resulting process-break address.
    Break(u64),
    /// `mprotect` or `munmap` completed.
    Completed,
    /// The request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the request was in progress.
    Terminated,
}

/// One completed or terminal native process-memory state attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedMemoryStateAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Operation and meaningful arguments copied at syscall entry.
    pub request: CapturedMemoryStateRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedMemoryStateOutcome,
}

/// Arguments of one native thread-runtime registration request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedThreadStateRequest {
    /// Set one x86-64 thread-segment base.
    ArchPrctl {
        /// Native `ARCH_SET_*` operation number.
        operation: u32,
        /// Segment-base address supplied by the tracee.
        address: u64,
    },
    /// Install the address cleared by the kernel when this thread exits.
    SetTidAddress {
        /// Tracee address supplied to the kernel.
        address: u64,
    },
    /// Register the thread's robust-futex list head.
    SetRobustList {
        /// Tracee address of the list head.
        address: u64,
        /// Native list-head byte size.
        length: u64,
    },
    /// Register or unregister Linux restartable-sequence state.
    Rseq {
        /// Tracee address of the rseq area.
        address: u64,
        /// Native rseq area byte size.
        length: u32,
        /// Native registration flags.
        flags: u32,
        /// ABI signature supplied by the tracee.
        signature: u32,
    },
    /// Install one measured x86-64 alternate signal stack.
    SignalStack {
        /// Tracee address of the stack storage.
        address: u64,
        /// Native stack flags copied from the request.
        flags: i32,
        /// Size of the stack storage in bytes.
        bytes: u64,
        /// Whether the request omitted the prior-state output pointer.
        old_state_is_null: bool,
    },
    /// Return from the most recently entered caught-signal frame.
    SignalReturn {
        /// Zero-based position in this process's signal-delivery stream.
        delivery: u64,
        /// Accumulator value saved in and restored from the signal frame.
        restored_accumulator: i64,
    },
}

/// Kernel result of one native thread-runtime registration request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedThreadStateOutcome {
    /// `set_tid_address` returned this positive thread identifier.
    ThreadId(i32),
    /// A non-return-value registration request completed.
    Completed,
    /// `rt_sigreturn` restored the captured interrupted context.
    SignalContextRestored,
    /// The request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the request was in progress.
    Terminated,
}

/// One completed or terminal native thread-runtime state attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedThreadStateAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Operation and meaningful arguments copied at syscall entry.
    pub request: CapturedThreadStateRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedThreadStateOutcome,
}

/// Native syscall used to terminate a process or thread group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedLifecycleExitSystemCall {
    /// Terminate the calling thread.
    Exit,
    /// Terminate every thread in the calling thread group.
    ExitGroup,
}

/// Arguments of one native lifecycle-exit request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedLifecycleExitRequest {
    /// Native exit syscall form.
    pub system_call: CapturedLifecycleExitSystemCall,
    /// Raw signed status argument supplied by the tracee.
    pub status: i32,
}

/// Stable value of one supported regular descriptor still open at exit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedExitFileValue {
    /// Descriptor number in the exiting tracee.
    pub descriptor: i32,
    /// Exact descriptor identity at the exit syscall boundary.
    pub identity: CapturedDescriptorIdentity,
    /// Mount identifier at the exit syscall boundary.
    pub mount_id: u64,
    /// Stable whole-file value read through the retained descriptor.
    pub value: CapturedMappedFileValue,
}

/// One terminal native lifecycle-exit attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedLifecycleExitAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Syscall form and raw status copied at syscall entry.
    pub request: CapturedLifecycleExitRequest,
    /// Supported regular descriptors still open when exit was requested.
    pub open_file_values: Vec<CapturedExitFileValue>,
    /// Matching process terminal state observed by the supervisor.
    pub outcome: CapturedProcessOutcome,
}

/// Kernel disposition of one syscall not yet assigned a supported model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedUnmodeledSyscallOutcome {
    /// The syscall returned through a matching ptrace exit stop.
    Returned {
        /// Raw native return value reported by the kernel.
        return_value: i64,
        /// Whether the kernel identified the return as an error.
        is_error: bool,
    },
    /// The task terminated while the syscall was in progress.
    Terminated,
}

/// One exact syscall attempt retained until a supported model replaces it.
///
/// This is private diagnostic evidence. Recording it does not make the syscall
/// safe for snapshot admission or replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedUnmodeledSyscallAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Native syscall number for the captured architecture.
    pub number: u64,
    /// Six raw native argument words copied at syscall entry.
    pub arguments: [u64; 6],
    /// Matching kernel return or terminal state.
    pub outcome: CapturedUnmodeledSyscallOutcome,
}

/// Original pathname, lookup, and result-mask arguments for native `statx`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedStatxRequest {
    /// Directory descriptor supplied by the tracee.
    pub directory_descriptor: i32,
    /// Bounded pathname prefix, excluding a terminating NUL when present.
    pub path: Vec<u8>,
    /// Whether the tracee supplied a null pathname pointer.
    pub path_is_null: bool,
    /// Whether pathname termination was observed within Linux's limit.
    pub path_terminated: bool,
    /// Entry-time cwd or directory-descriptor target for a relative path.
    pub base: Option<Vec<u8>>,
    /// Entry-time identity for a descriptor-form empty-path request.
    pub descriptor_identity: Option<CapturedDescriptorIdentity>,
    /// Entry-time mount ID, present exactly with descriptor identity.
    pub descriptor_mount_id: Option<u64>,
    /// Native Linux `AT_*` lookup and synchronization flags.
    pub flags: u32,
    /// Native `STATX_*` field-request mask.
    pub mask: u32,
    /// Whether the tracee supplied a null result pointer.
    pub output_is_null: bool,
}

/// One timestamp returned in the fixed native `statx` UAPI structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedStatxTimestamp {
    /// Seconds before or after the Unix epoch.
    pub seconds: i64,
    /// Nanoseconds after the recorded second.
    pub nanoseconds: u32,
    /// Reserved native field, retained exactly.
    pub reserved: i32,
}

/// All fields from one successful fixed-layout native `statx` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedStatxMetadata {
    /// `STATX_*` fields the kernel reports as meaningful.
    pub mask: u32,
    /// Preferred general I/O size.
    pub block_size: u32,
    /// File attribute bits.
    pub attributes: u64,
    /// Hard-link count.
    pub hard_links: u32,
    /// Owning user identifier.
    pub user: u32,
    /// Owning group identifier.
    pub group: u32,
    /// File type and permission bits.
    pub mode: u16,
    /// Reserved word following the mode, retained exactly.
    pub spare0: u16,
    /// Filesystem inode number.
    pub inode: u64,
    /// Logical byte size.
    pub size: u64,
    /// Allocated 512-byte block count.
    pub blocks: u64,
    /// Attribute bits supported by the filesystem.
    pub attributes_mask: u64,
    /// Last-access timestamp.
    pub access_time: CapturedStatxTimestamp,
    /// Creation timestamp.
    pub birth_time: CapturedStatxTimestamp,
    /// Last-status-change timestamp.
    pub change_time: CapturedStatxTimestamp,
    /// Last-data-modification timestamp.
    pub modification_time: CapturedStatxTimestamp,
    /// Major number of a represented device.
    pub special_device_major: u32,
    /// Minor number of a represented device.
    pub special_device_minor: u32,
    /// Major number of the containing device.
    pub device_major: u32,
    /// Minor number of the containing device.
    pub device_minor: u32,
    /// Kernel mount identifier.
    pub mount_id: u64,
    /// Required memory alignment for direct I/O.
    pub direct_io_memory_alignment: u32,
    /// Required file-offset alignment for direct I/O writes.
    pub direct_io_offset_alignment: u32,
    /// Filesystem subvolume identifier.
    pub subvolume: u64,
    /// Minimum atomic-write unit.
    pub atomic_write_unit_minimum: u32,
    /// Maximum atomic-write unit.
    pub atomic_write_unit_maximum: u32,
    /// Maximum atomic-write segment count.
    pub atomic_write_segments_maximum: u32,
    /// Required file-offset alignment for direct I/O reads.
    pub direct_io_read_offset_alignment: u32,
    /// Optimized maximum atomic-write unit.
    pub atomic_write_unit_maximum_optimized: u32,
    /// Reserved word following atomic-write fields, retained exactly.
    pub spare2: u32,
    /// Reserved tail words, retained exactly.
    pub spare3: [u64; 8],
}

/// Kernel result of one observed native `statx` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedStatxOutcome {
    /// Kernel copied this complete fixed-layout metadata record.
    Metadata(Box<CapturedStatxMetadata>),
    /// Request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the lookup was in progress.
    Terminated,
}

/// One completed or terminal native `statx` attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedStatxAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Arguments and entry-time path identity.
    pub request: CapturedStatxRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedStatxOutcome,
}

/// Native Linux pathname extended-attribute read operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedXattrSystemCall {
    /// `getxattr(path, name, value, size)`, following a final symlink.
    Getxattr,
    /// `lgetxattr(path, name, value, size)`, not following a final symlink.
    Lgetxattr,
    /// `fgetxattr(fd, name, value, size)` on an open descriptor.
    Fgetxattr,
    /// `fsetxattr(fd, name, value, size, flags)` on an open descriptor.
    Fsetxattr,
}

/// Original pathname, attribute name, and value buffer for an xattr read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedXattrRequest {
    /// Native operation used by the tracee.
    pub system_call: CapturedXattrSystemCall,
    /// Bounded pathname prefix, excluding a terminating NUL when present.
    pub path: Vec<u8>,
    /// Whether the tracee supplied a null pathname pointer.
    pub path_is_null: bool,
    /// Whether pathname termination was observed within Linux's limit.
    pub path_terminated: bool,
    /// Entry-time cwd for a meaningful relative pathname.
    pub base: Option<Vec<u8>>,
    /// Descriptor supplied to `fgetxattr`, absent for pathname operations.
    pub descriptor: Option<i32>,
    /// Entry-time descriptor identity, present for a valid descriptor.
    pub descriptor_identity: Option<CapturedDescriptorIdentity>,
    /// Entry-time descriptor mount, present exactly with its identity.
    pub descriptor_mount_id: Option<u64>,
    /// Bounded attribute-name prefix, excluding a terminating NUL when present.
    pub name: Vec<u8>,
    /// Whether the tracee supplied a null attribute-name pointer.
    pub name_is_null: bool,
    /// Whether name termination was observed within Linux's limit.
    pub name_terminated: bool,
    /// Maximum value bytes requested by the tracee.
    pub requested_bytes: u64,
    /// Whether the tracee supplied a null value pointer.
    pub value_is_null: bool,
    /// Exact value supplied to `fsetxattr`, absent for read operations.
    pub set_value: Option<Vec<u8>>,
    /// Native set-operation flags, zero for read operations.
    pub flags: u32,
}

/// Kernel result of one observed native extended-attribute read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedXattrOutcome {
    /// Attribute exists and returned this byte count and optional exact value.
    Value {
        /// Kernel return value, including for a size-only query.
        returned_bytes: u64,
        /// Exact bytes copied, or `None` when the request was size-only.
        bytes: Option<Vec<u8>>,
    },
    /// A descriptor extended attribute was changed successfully.
    Changed,
    /// Request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the syscall was in progress.
    Terminated,
}

/// One completed or terminal native extended-attribute read attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedXattrAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Pathname, name, and value-buffer request copied at syscall entry.
    pub request: CapturedXattrRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedXattrOutcome,
}

/// Native Linux pathname-to-link-target operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedReadlinkSystemCall {
    /// `readlink(path, buffer, size)`.
    Readlink,
    /// `readlinkat(dirfd, path, buffer, size)`.
    ReadlinkAt,
}

/// Original pathname and buffer size for one native link-target request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedReadlinkRequest {
    /// Native operation used by the tracee.
    pub system_call: CapturedReadlinkSystemCall,
    /// Directory descriptor supplied by the tracee.
    pub directory_descriptor: i32,
    /// Pathname bytes supplied by the tracee, excluding its terminating NUL.
    pub path: Vec<u8>,
    /// Whether the tracee supplied a null pathname pointer.
    pub path_is_null: bool,
    /// Entry-time cwd or descriptor target for a non-absolute pathname.
    ///
    /// This is absent for an absolute path or an invalid directory descriptor.
    pub base: Option<Vec<u8>>,
    /// Maximum target bytes requested by the tracee.
    pub requested_bytes: u64,
}

/// Kernel result of one observed native link-target request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedReadlinkOutcome {
    /// Exact target prefix copied by the kernel.
    Target {
        /// Lossless bytes delivered to the tracee, without a terminating NUL.
        bytes: Vec<u8>,
        /// Whether the result filled the buffer and may therefore be truncated.
        possibly_truncated: bool,
    },
    /// Request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the syscall was in progress.
    Terminated,
}

/// One completed or terminal native link-target attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedReadlinkAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Pathname and buffer size copied at syscall entry.
    pub request: CapturedReadlinkRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedReadlinkOutcome,
}

/// Native Linux pathname-access predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedAccessSystemCall {
    /// `access(path, mode)`.
    Access,
    /// `faccessat(dirfd, path, mode)`.
    Faccessat,
    /// `faccessat2(dirfd, path, mode, flags)`.
    Faccessat2,
}

/// Original pathname and predicate arguments for one native access check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedAccessRequest {
    /// Native operation used by the tracee.
    pub system_call: CapturedAccessSystemCall,
    /// Directory descriptor supplied by the tracee.
    pub directory_descriptor: i32,
    /// Pathname bytes supplied by the tracee, excluding its terminating NUL.
    pub path: Vec<u8>,
    /// Whether the tracee supplied a null pathname pointer.
    pub path_is_null: bool,
    /// Entry-time cwd or descriptor target for a non-absolute pathname.
    ///
    /// This is absent for an absolute path or an invalid directory descriptor.
    pub base: Option<Vec<u8>>,
    /// Native `F_OK`, `R_OK`, `W_OK`, and `X_OK` predicate bits.
    pub mode: u32,
    /// Native Linux `AT_*` access-check flags.
    pub flags: u32,
}

/// Kernel result of one observed native access predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedAccessOutcome {
    /// The requested access predicate held at the syscall exit boundary.
    Accessible,
    /// Request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the syscall was in progress.
    Terminated,
}

/// One completed or terminal native access attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedAccessAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Pathname and predicate arguments copied at syscall entry.
    pub request: CapturedAccessRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedAccessOutcome,
}

/// Original buffer capacity for one native Linux `getcwd` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedGetcwdRequest {
    /// Maximum byte count, including the terminating NUL, requested by the tracee.
    pub requested_bytes: u64,
}

/// Kernel result of one observed native Linux `getcwd` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedGetcwdOutcome {
    /// Exact absolute bytes, or Linux's `(unreachable)/` form, before the NUL.
    Directory(Vec<u8>),
    /// Request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the syscall was in progress.
    Terminated,
}

/// One completed or terminal native Linux `getcwd` attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedGetcwdAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Requested result-buffer capacity.
    pub request: CapturedGetcwdRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedGetcwdOutcome,
}

/// Native Linux operation that can replace a process's working directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedCwdChangeSystemCall {
    /// Pathname-based `chdir`.
    Chdir,
    /// Descriptor-based `fchdir`.
    Fchdir,
}

/// Entry-time evidence for one native Linux working-directory mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedCwdChangeRequest {
    /// Native syscall operation.
    pub system_call: CapturedCwdChangeSystemCall,
    /// Exact pathname bytes for `chdir`, empty for `fchdir` or a null pointer.
    pub path: Vec<u8>,
    /// Distinguishes a null `chdir` pathname from an empty string.
    pub path_is_null: bool,
    /// `fchdir` descriptor, or `AT_FDCWD` for `chdir`.
    pub directory_descriptor: i32,
    /// Entry-time procfs target for the `fchdir` descriptor, when resolvable.
    pub descriptor_target: Option<Vec<u8>>,
    /// Entry-time procfs working-directory target.
    pub prior_working_directory: Vec<u8>,
}

/// Kernel result of one native Linux working-directory mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedCwdChangeOutcome {
    /// Mutation succeeded and procfs reported these new working-directory bytes.
    Changed(Vec<u8>),
    /// Request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the mutation was in progress.
    Terminated,
}

/// One completed or terminal native Linux working-directory mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedCwdChangeAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Entry-time mutation evidence.
    pub request: CapturedCwdChangeRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedCwdChangeOutcome,
}

/// Native Linux operation that removes a filesystem name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedPathRemovalSystemCall {
    /// Legacy `unlink`.
    Unlink,
    /// Directory-relative `unlinkat`.
    UnlinkAt,
}

/// Original pathname and lookup arguments for one removal request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedPathRemovalRequest {
    /// Native syscall operation.
    pub system_call: CapturedPathRemovalSystemCall,
    /// Directory descriptor supplied by the tracee; ignored for absolute paths.
    pub directory_descriptor: i32,
    /// Pathname bytes supplied by the tracee, excluding its terminating NUL.
    pub path: Vec<u8>,
    /// Whether the tracee supplied a null pathname pointer.
    pub path_is_null: bool,
    /// Entry-time cwd or descriptor target for a non-absolute pathname.
    ///
    /// This is absent for an absolute path or an invalid directory descriptor.
    pub base: Option<Vec<u8>>,
    /// Native `unlinkat` flags, or zero for legacy `unlink`.
    pub flags: u32,
}

/// Kernel result of one native Linux pathname-removal request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedPathRemovalOutcome {
    /// The named non-directory or directory entry was removed.
    Removed,
    /// Request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the removal was in progress.
    Terminated,
}

/// One completed or terminal native Linux pathname-removal attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedPathRemovalAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Pathname and lookup arguments copied at syscall entry.
    pub request: CapturedPathRemovalRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedPathRemovalOutcome,
}

/// Native Linux operation that creates a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedDirectoryCreationSystemCall {
    /// Legacy `mkdir`.
    Mkdir,
    /// Directory-relative `mkdirat`.
    MkdirAt,
}

/// Original pathname, mode, and lookup arguments for one directory creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedDirectoryCreationRequest {
    /// Native syscall operation.
    pub system_call: CapturedDirectoryCreationSystemCall,
    /// Directory descriptor supplied by the tracee; ignored for absolute paths.
    pub directory_descriptor: i32,
    /// Pathname bytes supplied by the tracee, excluding its terminating NUL.
    pub path: Vec<u8>,
    /// Whether the tracee supplied a null pathname pointer.
    pub path_is_null: bool,
    /// Entry-time cwd or descriptor target for a non-absolute pathname.
    ///
    /// This is absent for an absolute path or an invalid directory descriptor.
    pub base: Option<Vec<u8>>,
    /// Raw mode argument supplied to the kernel.
    pub mode: u32,
}

/// Kernel result of one native Linux directory-creation request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedDirectoryCreationOutcome {
    /// The requested directory was created.
    Created,
    /// Request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the creation was in progress.
    Terminated,
}

/// One completed or terminal native Linux directory-creation attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedDirectoryCreationAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Pathname, mode, and lookup arguments copied at syscall entry.
    pub request: CapturedDirectoryCreationRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedDirectoryCreationOutcome,
}

/// Native Linux operation that renames one or two pathname entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedPathRenameSystemCall {
    /// Legacy `rename`.
    Rename,
    /// Directory-relative `renameat`.
    RenameAt,
    /// Flag-bearing directory-relative `renameat2`.
    RenameAt2,
}

/// Original source, destination, lookup, and flag arguments for one rename.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedPathRenameRequest {
    /// Native syscall operation.
    pub system_call: CapturedPathRenameSystemCall,
    /// Directory descriptor for the source pathname.
    pub old_directory_descriptor: i32,
    /// Source pathname prefix, excluding a terminating NUL when present.
    pub old_path: Vec<u8>,
    /// Whether the tracee supplied a null source-path pointer.
    pub old_path_is_null: bool,
    /// Whether source-path termination was observed within Linux's limit.
    pub old_path_terminated: bool,
    /// Entry-time cwd or descriptor target for a meaningful relative source.
    pub old_base: Option<Vec<u8>>,
    /// Directory descriptor for the destination pathname.
    pub new_directory_descriptor: i32,
    /// Destination pathname prefix, excluding a terminating NUL when present.
    pub new_path: Vec<u8>,
    /// Whether the tracee supplied a null destination-path pointer.
    pub new_path_is_null: bool,
    /// Whether destination-path termination was observed within Linux's limit.
    pub new_path_terminated: bool,
    /// Entry-time cwd or descriptor target for a meaningful relative destination.
    pub new_base: Option<Vec<u8>>,
    /// Raw `renameat2` flags, or zero for the older operations.
    pub flags: u32,
}

/// Kernel result of one native Linux pathname-rename request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedPathRenameOutcome {
    /// The requested rename, exchange, or whiteout operation completed.
    Completed,
    /// Request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the rename was in progress.
    Terminated,
}

/// One completed or terminal native Linux pathname-rename attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedPathRenameAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Source and destination lookup arguments copied at syscall entry.
    pub request: CapturedPathRenameRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedPathRenameOutcome,
}

/// Native Linux pathname operation that changes file permission bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedPathModeChangeSystemCall {
    /// Legacy `chmod`.
    Chmod,
    /// Directory-relative `fchmodat`.
    FchmodAt,
    /// Descriptor-based `fchmod`.
    Fchmod,
}

/// Original target and mode arguments for one permission change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedPathModeChangeRequest {
    /// Native syscall operation.
    pub system_call: CapturedPathModeChangeSystemCall,
    /// Operated descriptor for `fchmod`, absent for pathname operations.
    pub descriptor: Option<i32>,
    /// Entry-time operated descriptor identity for `fchmod`.
    pub descriptor_identity: Option<CapturedDescriptorIdentity>,
    /// Entry-time operated descriptor mount identity for `fchmod`.
    pub descriptor_mount_id: Option<u64>,
    /// Directory descriptor for pathname operations; `AT_FDCWD` for `fchmod`.
    pub directory_descriptor: i32,
    /// Pathname prefix, excluding a terminating NUL when present.
    pub path: Vec<u8>,
    /// Whether the tracee supplied a null pathname pointer.
    pub path_is_null: bool,
    /// Whether pathname termination was observed within Linux's limit.
    pub path_terminated: bool,
    /// Entry-time cwd or descriptor target for a meaningful relative pathname.
    pub base: Option<Vec<u8>>,
    /// Low 32 bits of the raw native mode argument supplied to the kernel.
    pub mode: u32,
}

/// Kernel result of one native Linux permission change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedPathModeChangeOutcome {
    /// The requested permission change completed.
    Changed,
    /// Request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the mutation was in progress.
    Terminated,
}

/// One completed or terminal native Linux permission change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedPathModeChangeAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Target, lookup, and mode arguments copied at syscall entry.
    pub request: CapturedPathModeChangeRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedPathModeChangeOutcome,
}

/// One native Linux seconds-and-nanoseconds timestamp argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedTimestamp {
    /// Seconds since the Unix epoch, or ignored data for a special nanosecond value.
    pub seconds: i64,
    /// Nanoseconds, `UTIME_NOW`, or `UTIME_OMIT`.
    pub nanoseconds: i64,
}

/// Original arguments for descriptor-form `utimensat` with a null pathname.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedDescriptorTimestampChangeRequest {
    /// Descriptor supplied by the tracee.
    pub descriptor: i32,
    /// Entry-time descriptor identity, absent only for an invalid descriptor.
    pub identity: Option<CapturedDescriptorIdentity>,
    /// Entry-time descriptor mount ID, present exactly when identity is present.
    pub mount_id: Option<u64>,
    /// Whether the tracee supplied a null timestamp-array pointer.
    pub times_is_null: bool,
    /// Access and modification timestamps copied from a nonnull array.
    pub times: Option<[CapturedTimestamp; 2]>,
    /// Raw native flags.
    pub flags: u32,
}

/// Kernel result of one descriptor-form timestamp mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedDescriptorTimestampChangeOutcome {
    /// The requested timestamp operation completed.
    Changed,
    /// Request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the mutation was in progress.
    Terminated,
}

/// One completed or terminal descriptor-form timestamp mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedDescriptorTimestampChangeAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Descriptor and timestamp arguments copied at syscall entry.
    pub request: CapturedDescriptorTimestampChangeRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedDescriptorTimestampChangeOutcome,
}

/// Native Linux operation that creates a symbolic link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedSymbolicLinkCreationSystemCall {
    /// Legacy `symlink`.
    Symlink,
    /// Directory-relative `symlinkat`.
    SymlinkAt,
}

/// Original target and link-name arguments for one symbolic-link creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedSymbolicLinkCreationRequest {
    /// Native syscall operation.
    pub system_call: CapturedSymbolicLinkCreationSystemCall,
    /// Opaque target prefix, excluding a terminating NUL when present.
    pub target: Vec<u8>,
    /// Whether the tracee supplied a null target pointer.
    pub target_is_null: bool,
    /// Whether target termination was observed within Linux's limit.
    pub target_terminated: bool,
    /// Directory descriptor supplied for the new link name.
    pub directory_descriptor: i32,
    /// New link-name prefix, excluding a terminating NUL when present.
    pub link_path: Vec<u8>,
    /// Whether the tracee supplied a null link-name pointer.
    pub link_path_is_null: bool,
    /// Whether link-name termination was observed within Linux's limit.
    pub link_path_terminated: bool,
    /// Entry-time cwd or descriptor target for a meaningful relative link name.
    pub base: Option<Vec<u8>>,
}

/// Kernel result of one native Linux symbolic-link creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedSymbolicLinkCreationOutcome {
    /// The symbolic link was created.
    Created,
    /// Request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the mutation was in progress.
    Terminated,
}

/// One completed or terminal native Linux symbolic-link creation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedSymbolicLinkCreationAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Target and new link-name evidence copied at syscall entry.
    pub request: CapturedSymbolicLinkCreationRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedSymbolicLinkCreationOutcome,
}

/// Native Linux operation that reports filesystem-wide status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedFilesystemStatusSystemCall {
    /// Pathname-based `statfs`.
    Statfs,
    /// Descriptor-based `fstatfs`.
    Fstatfs,
}

/// Original lookup arguments for one filesystem-status observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedFilesystemStatusRequest {
    /// Native syscall operation.
    pub system_call: CapturedFilesystemStatusSystemCall,
    /// Descriptor supplied to `fstatfs`, or `AT_FDCWD` for `statfs`.
    pub descriptor: i32,
    /// Kernel-visible pathname prefix, excluding a terminating NUL when present.
    pub path: Vec<u8>,
    /// Whether `statfs` received a null pathname pointer.
    pub path_is_null: bool,
    /// Whether a terminating NUL was observed within Linux's pathname limit.
    pub path_terminated: bool,
    /// Whether the tracee supplied a null result-buffer pointer.
    pub output_is_null: bool,
    /// Entry-time cwd for a relative `statfs` pathname.
    pub base: Option<Vec<u8>>,
    /// Entry-time kernel-object identity for the `fstatfs` descriptor.
    pub descriptor_identity: Option<CapturedDescriptorIdentity>,
    /// Entry-time mount identifier reported for the `fstatfs` descriptor.
    pub descriptor_mount_id: Option<u64>,
}

/// Canonical fields returned by a native Linux filesystem-status syscall.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedFilesystemStatus {
    /// Filesystem implementation magic.
    pub filesystem_type: i64,
    /// Optimal transfer block size.
    pub block_size: i64,
    /// Total data blocks.
    pub blocks: u64,
    /// Free data blocks.
    pub blocks_free: u64,
    /// Free data blocks available to an unprivileged caller.
    pub blocks_available: u64,
    /// Total file nodes.
    pub files: u64,
    /// Free file nodes.
    pub files_free: u64,
    /// Filesystem identifier as the two native 32-bit words returned by Linux.
    pub filesystem_id: [i32; 2],
    /// Maximum component-name length.
    pub maximum_name_length: i64,
    /// Fragment size.
    pub fragment_size: i64,
    /// Mount flags reported by the kernel.
    pub mount_flags: i64,
    /// Reserved native words copied to userspace after the mount flags.
    pub reserved_words: [i64; 4],
}

/// Kernel result of one native Linux filesystem-status request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedFilesystemStatusOutcome {
    /// The kernel returned these canonical filesystem fields.
    Observed(CapturedFilesystemStatus),
    /// Request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the observation was in progress.
    Terminated,
}

/// One completed or terminal native Linux filesystem-status attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedFilesystemStatusAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Path or descriptor identity copied at syscall entry.
    pub request: CapturedFilesystemStatusRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedFilesystemStatusOutcome,
}

/// Kernel object type behind a descriptor at an observation boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedDescriptorKind {
    /// Regular filesystem file.
    RegularFile,
    /// Filesystem directory.
    Directory,
    /// Character device.
    CharacterDevice,
    /// Block device.
    BlockDevice,
    /// FIFO or pipe.
    Fifo,
    /// Socket.
    Socket,
    /// Another descriptor type not yet given semantic replay treatment.
    Other,
}

/// Descriptor identity resolved while its tracee was stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedDescriptorIdentity {
    /// Lossless target bytes from the tracee's procfs descriptor link.
    pub target: Vec<u8>,
    /// Device number reported for the open kernel object.
    pub device: u64,
    /// Inode number reported for the open kernel object.
    pub inode: u64,
    /// Kernel object type.
    pub kind: CapturedDescriptorKind,
}

/// One semantic directory entry returned by Linux `getdents64`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedDirectoryEntry {
    /// Filesystem inode number supplied by the kernel.
    pub inode: u64,
    /// Opaque signed cookie for the next directory position.
    pub next_position: i64,
    /// Native record length, including the name terminator and padding.
    pub record_length: u16,
    /// Native `DT_*` file-type byte.
    pub file_type: u8,
    /// Lossless entry name without its terminating NUL or padding.
    pub name: Vec<u8>,
}

/// Descriptor and buffer requested at one `getdents64` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedGetdents64Request {
    /// Descriptor number supplied by the tracee.
    pub descriptor: i32,
    /// Descriptor identity, absent only when the descriptor was invalid.
    pub identity: Option<CapturedDescriptorIdentity>,
    /// Mount ID paired with `identity`, absent only when identity is absent.
    pub mount_id: Option<u64>,
    /// Opaque directory position before the syscall, when available.
    pub position_before: Option<i64>,
    /// Maximum byte count requested from the kernel.
    pub requested_bytes: u32,
}

/// Conservative value of the complete directory behind one enumeration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedDirectoryValue {
    /// Enumeration was observed, but the complete directory was not witnessed.
    Unwitnessed,
    /// Ordered semantic value from a separate complete enumeration.
    Complete {
        /// Number of entries included in the complete value.
        entries: u64,
        /// Domain-separated BLAKE3 digest of the ordered semantic entries.
        digest: [u8; 32],
        /// Sorted member names, excluding the synthetic `.` and `..` entries.
        members: Vec<Vec<u8>>,
    },
}

/// Kernel result of one observed `getdents64` request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedGetdents64Outcome {
    /// Kernel-defined entries, excluding undefined record padding bytes.
    Enumerated {
        /// Exact byte count returned by the kernel.
        returned_bytes: u32,
        /// Opaque directory position after the successful syscall.
        position_after: i64,
        /// Parsed semantic entries in delivery order.
        entries: Vec<CapturedDirectoryEntry>,
        /// Independently witnessed value of the complete directory.
        directory_value: CapturedDirectoryValue,
    },
    /// Request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the syscall was in progress.
    Terminated,
}

/// One completed or terminal `getdents64` attempt in syscall order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedGetdents64Attempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Descriptor identity and buffer size captured at syscall entry.
    pub request: CapturedGetdents64Request,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedGetdents64Outcome,
}

/// Native Linux `mmap` request captured at syscall entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedMmapRequest {
    /// Requested address hint or fixed address.
    pub address: u64,
    /// Requested byte length before kernel page rounding.
    pub length: u64,
    /// Native protection bits.
    pub protection: u64,
    /// Native mapping flags.
    pub flags: u64,
    /// Descriptor number supplied by the tracee.
    pub descriptor: i32,
    /// Entry-time identity, absent for anonymous or invalid descriptors.
    pub identity: Option<CapturedDescriptorIdentity>,
    /// Entry-time mount ID, absent exactly when identity is absent.
    pub mount_id: Option<u64>,
    /// Requested backing-object offset.
    pub offset: u64,
}

/// Kernel result of one observed native Linux `mmap` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedMmapOutcome {
    /// Mapping was installed at this returned address.
    Mapped {
        /// Inclusive virtual start address returned by the kernel.
        address: u64,
        /// File-value evidence, absent exactly for anonymous mappings.
        file_value: Option<CapturedMappedFileValue>,
    },
    /// Request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the syscall was in progress.
    Terminated,
}

/// One completed or terminal native Linux `mmap` attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedMmapAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Mapping parameters and entry-time descriptor identity.
    pub request: CapturedMmapRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedMmapOutcome,
}

/// Scalar descriptor-write syscall whose consumed bytes were captured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedWriteSystemCall {
    /// Position-advancing `write`.
    Write,
    /// Offset-explicit `pwrite64`.
    Pwrite64,
}

/// Descriptor and range requested by one scalar write syscall.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedWriteRequest {
    /// Native write operation.
    pub system_call: CapturedWriteSystemCall,
    /// Descriptor number supplied by the tracee.
    pub descriptor: i32,
    /// Descriptor identity, absent only when the descriptor was invalid.
    pub identity: Option<CapturedDescriptorIdentity>,
    /// Mount ID paired with `identity`, absent only when identity is absent.
    pub mount_id: Option<u64>,
    /// Whether the operation used the open file description's position.
    pub uses_descriptor_offset: bool,
    /// Entry-time descriptor position or explicit write offset.
    pub offset: Option<u64>,
    /// Maximum byte count requested from the kernel.
    pub requested_bytes: u64,
    /// Whether the open file description had Linux `O_APPEND` set.
    pub append: bool,
}

/// Kernel result of one observed scalar descriptor write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedWriteOutcome {
    /// Exact logical prefix consumed by the kernel.
    Wrote {
        /// Byte count accepted by the kernel.
        bytes: u64,
        /// BLAKE3 digest of exactly those bytes.
        digest: [u8; 32],
    },
    /// Request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the syscall was in progress.
    Terminated,
}

/// One completed or terminal scalar descriptor-write attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedWriteAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Descriptor identity and requested range captured at syscall entry.
    pub request: CapturedWriteRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedWriteOutcome,
}

/// Descriptor-read syscall whose delivered bytes were captured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedReadSystemCall {
    /// Position-advancing `read`.
    Read,
    /// Offset-explicit `pread64`.
    Pread64,
    /// Position-advancing vectored `readv`.
    Readv,
    /// Offset-explicit vectored `preadv`.
    Preadv,
    /// Flag-bearing vectored `preadv2`.
    Preadv2,
}

/// Descriptor, buffers, and range requested at one read entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedReadRequest {
    /// Read operation selected by the tracee.
    pub system_call: CapturedReadSystemCall,
    /// Descriptor number supplied by the tracee.
    pub descriptor: i32,
    /// Descriptor identity, absent only when the descriptor was invalid.
    pub identity: Option<CapturedDescriptorIdentity>,
    /// Mount ID paired with `identity`, absent only when identity is absent.
    pub mount_id: Option<u64>,
    /// Whether the operation selected descriptor-position semantics.
    pub uses_descriptor_offset: bool,
    /// Starting file offset when the operation has a meaningful position.
    pub offset: Option<u64>,
    /// Requested buffer lengths in kernel traversal order.
    pub buffer_lengths: Vec<u64>,
    /// Maximum byte count requested from the kernel.
    pub requested_bytes: u64,
    /// Per-call read flags; zero except for `preadv2`.
    pub flags: u32,
}

/// Kernel result of one observed descriptor read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedReadOutcome {
    /// Bytes delivered to tracee memory, including an ordinary zero-byte EOF.
    Read {
        /// Exact delivered byte count.
        bytes: u64,
        /// BLAKE3 of the delivered bytes in order.
        digest: [u8; 32],
        /// Stable whole-file value whose returned range matched `digest`.
        file_value: Option<CapturedMappedFileValue>,
    },
    /// Request failed with this positive errno.
    Failed(i32),
    /// The task terminated while the syscall was in progress.
    Terminated,
}

/// One completed or terminal descriptor read in syscall order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedReadAttempt {
    /// Zero-based entry position in this process's syscall stream.
    pub syscall_index: u64,
    /// Arguments and descriptor identity captured at syscall entry.
    pub request: CapturedReadRequest,
    /// Matching kernel return or terminal state.
    pub outcome: CapturedReadOutcome,
}

/// Terminal state of one captured process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapturedProcessOutcome {
    /// Process returned normally with this exit status.
    Exited(i32),
    /// Process terminated from this signal number.
    Signaled(i32),
}

/// Complete lifecycle of one process in the supervised tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedProcess {
    /// Receipt-local identity.
    pub id: CapturedProcessId,
    /// Receipt-local parent identity; absent only for the capture root.
    pub parent: Option<CapturedProcessId>,
    /// Every successful exec in order. Fork-only helpers may have none.
    pub invocations: Vec<CapturedInvocation>,
    /// Every descendant exec request, including failed probes.
    pub exec_attempts: Vec<CapturedExecAttempt>,
    /// Exec syscall entries observed after this task joined capture.
    pub exec_syscalls: u64,
    /// Every open, openat, openat2, or creat request after capture began.
    pub open_attempts: Vec<CapturedOpenAttempt>,
    /// Captured pathname-opening entries for exact coverage checks.
    pub open_syscalls: u64,
    /// Every native `newfstatat` request after capture began.
    pub newfstatat_attempts: Vec<CapturedNewfstatatAttempt>,
    /// Captured native `newfstatat` entries for exact coverage checks.
    pub newfstatat_syscalls: u64,
    /// Every native descriptor `fstat` request after capture began.
    pub fstat_attempts: Vec<CapturedFstatAttempt>,
    /// Captured native `fstat` entries for exact coverage checks.
    pub fstat_syscalls: u64,
    /// Every native `getrandom` request after capture began.
    pub getrandom_attempts: Vec<CapturedGetrandomAttempt>,
    /// Captured native `getrandom` entries for exact coverage checks.
    pub getrandom_syscalls: u64,
    /// Every supported volatile runtime observation after capture began.
    pub runtime_observation_attempts: Vec<CapturedRuntimeObservationAttempt>,
    /// Captured native runtime-observation entries for exact coverage checks.
    pub runtime_observation_syscalls: u64,
    /// Every native `prlimit64` request after capture began.
    pub prlimit_attempts: Vec<CapturedPrlimitAttempt>,
    /// Captured native `prlimit64` entries for exact coverage checks.
    pub prlimit_syscalls: u64,
    /// Every native descriptor `close` request after capture began.
    pub close_attempts: Vec<CapturedCloseAttempt>,
    /// Captured native descriptor `close` entries for exact coverage checks.
    pub close_syscalls: u64,
    /// Every native descriptor `lseek` request after capture began.
    pub seek_attempts: Vec<CapturedSeekAttempt>,
    /// Captured native descriptor `lseek` entries for exact coverage checks.
    pub seek_syscalls: u64,
    /// Every bounded native `fadvise64` request after capture began.
    pub file_advice_attempts: Vec<CapturedFileAdviceAttempt>,
    /// Captured bounded native `fadvise64` entries for exact coverage checks.
    pub file_advice_syscalls: u64,
    /// Every native descriptor `fcntl` request after capture began.
    pub fcntl_attempts: Vec<CapturedFcntlAttempt>,
    /// Captured native descriptor `fcntl` entries for exact coverage checks.
    pub fcntl_syscalls: u64,
    /// Every supported native descriptor `ioctl` probe after capture began.
    pub ioctl_attempts: Vec<CapturedIoctlAttempt>,
    /// Captured supported native `ioctl` entries for exact coverage checks.
    pub ioctl_syscalls: u64,
    /// Every native anonymous-pipe creation after capture began.
    pub pipe_attempts: Vec<CapturedPipeAttempt>,
    /// Captured native `pipe2` entries for exact coverage checks.
    pub pipe_syscalls: u64,
    /// Every native event-poll descriptor creation after capture began.
    pub epoll_create_attempts: Vec<CapturedEpollCreateAttempt>,
    /// Captured native `epoll_create1` entries for exact coverage checks.
    pub epoll_create_syscalls: u64,
    /// Every supported native local stream-socket creation after capture began.
    pub socket_create_attempts: Vec<CapturedSocketCreateAttempt>,
    /// Captured supported native `socket` entries for exact coverage checks.
    pub socket_create_syscalls: u64,
    /// Every supported native local stream-socket connection after capture
    /// began.
    pub socket_connect_attempts: Vec<CapturedSocketConnectAttempt>,
    /// Captured supported native `connect` entries for exact coverage checks.
    pub socket_connect_syscalls: u64,
    /// Every native descriptor duplication after capture began.
    pub dup2_attempts: Vec<CapturedDup2Attempt>,
    /// Captured native descriptor-duplication entries for coverage checks.
    pub dup2_syscalls: u64,
    /// Every native signal-disposition or mask request after capture began.
    pub signal_state_attempts: Vec<CapturedSignalStateAttempt>,
    /// Captured native signal-state entries for exact coverage checks.
    pub signal_state_syscalls: u64,
    /// Every signal accepted at a ptrace signal-delivery stop.
    pub signal_deliveries: Vec<CapturedSignalDelivery>,
    /// Every native file-creation mask transition after capture began.
    pub umask_attempts: Vec<CapturedUmaskAttempt>,
    /// Captured native file-creation mask entries for exact coverage checks.
    pub umask_syscalls: u64,
    /// Every native credential query or transition after capture began.
    pub credential_attempts: Vec<CapturedCredentialAttempt>,
    /// Captured native credential entries for exact coverage checks.
    pub credential_syscalls: u64,
    /// Every process-private native futex wake after capture began.
    pub futex_wake_attempts: Vec<CapturedFutexWakeAttempt>,
    /// Captured process-private futex wake entries for exact coverage checks.
    pub futex_wake_syscalls: u64,
    /// Every supported native process-creation request after capture began.
    pub process_creation_attempts: Vec<CapturedProcessCreationAttempt>,
    /// Captured supported process-creation entries for exact coverage checks.
    pub process_creation_syscalls: u64,
    /// Every supported native child wait after capture began.
    pub child_wait_attempts: Vec<CapturedChildWaitAttempt>,
    /// Captured supported native `wait4` entries for exact coverage checks.
    pub child_wait_syscalls: u64,
    /// Every supported native process-identity query after capture began.
    pub process_identity_attempts: Vec<CapturedProcessIdentityAttempt>,
    /// Captured process-identity entries for exact coverage checks.
    pub process_identity_syscalls: u64,
    /// Every native process-memory state request after capture began.
    pub memory_state_attempts: Vec<CapturedMemoryStateAttempt>,
    /// Captured process-memory state entries for exact coverage checks.
    pub memory_state_syscalls: u64,
    /// Every native thread-runtime state request after capture began.
    pub thread_state_attempts: Vec<CapturedThreadStateAttempt>,
    /// Captured thread-runtime state entries for exact coverage checks.
    pub thread_state_syscalls: u64,
    /// Every native lifecycle-exit request after capture began.
    pub lifecycle_exit_attempts: Vec<CapturedLifecycleExitAttempt>,
    /// Captured native lifecycle-exit entries for exact coverage checks.
    pub lifecycle_exit_syscalls: u64,
    /// Every native Linux `statx` request after capture began.
    pub statx_attempts: Vec<CapturedStatxAttempt>,
    /// Captured native Linux `statx` entries for coverage checks.
    pub statx_syscalls: u64,
    /// Every native pathname extended-attribute read after capture began.
    pub xattr_attempts: Vec<CapturedXattrAttempt>,
    /// Captured native pathname extended-attribute reads for coverage checks.
    pub xattr_syscalls: u64,
    /// Every native `readlink` or `readlinkat` request after capture began.
    pub readlink_attempts: Vec<CapturedReadlinkAttempt>,
    /// Captured native link-target entries for exact coverage checks.
    pub readlink_syscalls: u64,
    /// Every native access predicate after capture began.
    pub access_attempts: Vec<CapturedAccessAttempt>,
    /// Captured native access entries for exact coverage checks.
    pub access_syscalls: u64,
    /// Every native Linux `getcwd` request after capture began.
    pub getcwd_attempts: Vec<CapturedGetcwdAttempt>,
    /// Captured native Linux `getcwd` entries for exact coverage checks.
    pub getcwd_syscalls: u64,
    /// Every native Linux working-directory mutation after capture began.
    pub cwd_change_attempts: Vec<CapturedCwdChangeAttempt>,
    /// Captured native Linux `chdir` and `fchdir` entries for coverage checks.
    pub cwd_change_syscalls: u64,
    /// Every native Linux `unlink` or `unlinkat` request after capture began.
    pub path_removal_attempts: Vec<CapturedPathRemovalAttempt>,
    /// Captured native Linux pathname-removal entries for coverage checks.
    pub path_removal_syscalls: u64,
    /// Every native Linux `mkdir` or `mkdirat` request after capture began.
    pub directory_creation_attempts: Vec<CapturedDirectoryCreationAttempt>,
    /// Captured native Linux directory-creation entries for coverage checks.
    pub directory_creation_syscalls: u64,
    /// Every native Linux pathname-rename request after capture began.
    pub path_rename_attempts: Vec<CapturedPathRenameAttempt>,
    /// Captured native Linux pathname-rename entries for coverage checks.
    pub path_rename_syscalls: u64,
    /// Every native Linux pathname permission change after capture began.
    pub path_mode_change_attempts: Vec<CapturedPathModeChangeAttempt>,
    /// Captured native Linux pathname permission changes for coverage checks.
    pub path_mode_change_syscalls: u64,
    /// Every descriptor-form native Linux timestamp mutation after capture began.
    pub descriptor_timestamp_change_attempts: Vec<CapturedDescriptorTimestampChangeAttempt>,
    /// Captured descriptor-form timestamp mutations for coverage checks.
    pub descriptor_timestamp_change_syscalls: u64,
    /// Every native Linux symbolic-link creation after capture began.
    pub symbolic_link_creation_attempts: Vec<CapturedSymbolicLinkCreationAttempt>,
    /// Captured native Linux symbolic-link creations for coverage checks.
    pub symbolic_link_creation_syscalls: u64,
    /// Every native Linux `statfs` or `fstatfs` request after capture began.
    pub filesystem_status_attempts: Vec<CapturedFilesystemStatusAttempt>,
    /// Captured native Linux filesystem-status entries for coverage checks.
    pub filesystem_status_syscalls: u64,
    /// Every scalar or vectored descriptor read after capture began.
    pub read_attempts: Vec<CapturedReadAttempt>,
    /// Captured descriptor-read entries for exact coverage checks.
    pub read_syscalls: u64,
    /// Every `getdents64` request after capture began.
    pub getdents64_attempts: Vec<CapturedGetdents64Attempt>,
    /// Captured `getdents64` entries for exact coverage checks.
    pub getdents64_syscalls: u64,
    /// Every native Linux `mmap` request after capture began.
    pub mmap_attempts: Vec<CapturedMmapAttempt>,
    /// Captured native Linux `mmap` entries for exact coverage checks.
    pub mmap_syscalls: u64,
    /// Every scalar descriptor write after capture began.
    pub write_attempts: Vec<CapturedWriteAttempt>,
    /// Captured scalar-write entries for exact coverage checks.
    pub write_syscalls: u64,
    /// Every syscall not yet assigned a supported semantic model.
    pub unmodeled_syscall_attempts: Vec<CapturedUnmodeledSyscallAttempt>,
    /// Unmodeled syscall entries retained as exact private evidence.
    pub unmodeled_syscalls: u64,
    /// Syscall entries observed after this task joined capture.
    pub syscall_entries: u64,
    /// Syscalls that returned through a matching exit stop.
    pub syscall_exits: u64,
    /// Entered syscalls resolved by process termination instead of return.
    pub terminal_syscalls: u64,
    /// Terminal state. It is mandatory in a complete receipt.
    pub outcome: CapturedProcessOutcome,
}

/// One fail-closed process-tree lifecycle receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessCaptureReceipt {
    schema: String,
    version: u32,
    root: CapturedProcessId,
    processes: Vec<CapturedProcess>,
}

impl ProcessCaptureReceipt {
    pub(crate) fn new(root: CapturedProcessId, processes: Vec<CapturedProcess>) -> Result<Self> {
        let receipt = Self {
            schema: PROCESS_CAPTURE_RECEIPT_SCHEMA.to_string(),
            version: PROCESS_CAPTURE_RECEIPT_VERSION,
            root,
            processes,
        };
        receipt.validate_complete()?;
        Ok(receipt)
    }

    /// Revalidate schema and exact process-tree lifecycle coverage.
    pub fn validate_complete(&self) -> Result<()> {
        if self.schema != PROCESS_CAPTURE_RECEIPT_SCHEMA
            || self.version != PROCESS_CAPTURE_RECEIPT_VERSION
        {
            anyhow::bail!("process capture receipt has an unsupported schema");
        }
        let mut parents = BTreeMap::new();
        for process in &self.processes {
            if parents.insert(process.id, process.parent).is_some() {
                anyhow::bail!("process capture receipt repeats a process identity");
            }
            for invocation in &process.invocations {
                validate_captured_invocation(invocation)?;
            }
            let mut modeled_syscalls = BTreeSet::new();
            let mut prior_exec = None;
            let mut next_loaded_invocation = u64::from(process.id == self.root);
            for attempt in &process.exec_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_exec.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid exec position");
                }
                prior_exec = Some(attempt.syscall_index);
                if let Some(index) = validate_captured_exec_attempt(attempt)? {
                    if index != next_loaded_invocation
                        || usize::try_from(index)
                            .ok()
                            .is_none_or(|index| index >= process.invocations.len())
                    {
                        anyhow::bail!("process capture receipt has an invalid exec result");
                    }
                    next_loaded_invocation = next_loaded_invocation
                        .checked_add(1)
                        .context("process loaded-invocation census overflowed")?;
                }
            }
            if usize::try_from(next_loaded_invocation).ok() != Some(process.invocations.len()) {
                anyhow::bail!("process capture receipt has an unpaired successful exec");
            }
            if usize::try_from(process.exec_syscalls).ok() != Some(process.exec_attempts.len()) {
                anyhow::bail!("process capture receipt has incomplete exec attempts");
            }
            let mut terminal_opens = 0u64;
            let mut prior_open = None;
            for attempt in &process.open_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_open.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid open position");
                }
                prior_open = Some(attempt.syscall_index);
                if validate_captured_open_attempt(attempt)? {
                    terminal_opens = terminal_opens.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!("process terminal open census overflowed")
                    })?;
                }
            }
            if usize::try_from(process.open_syscalls).ok() != Some(process.open_attempts.len())
                || terminal_opens > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete open attempts");
            }
            let mut terminal_newfstatat = 0u64;
            let mut prior_newfstatat = None;
            for attempt in &process.newfstatat_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_newfstatat.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid newfstatat position");
                }
                prior_newfstatat = Some(attempt.syscall_index);
                if validate_captured_newfstatat_attempt(attempt)? {
                    terminal_newfstatat = terminal_newfstatat.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!("process terminal newfstatat census overflowed")
                    })?;
                }
            }
            if usize::try_from(process.newfstatat_syscalls).ok()
                != Some(process.newfstatat_attempts.len())
                || terminal_newfstatat > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete newfstatat attempts");
            }
            let mut terminal_fstat = 0u64;
            let mut prior_fstat = None;
            for attempt in &process.fstat_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_fstat.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid fstat position");
                }
                prior_fstat = Some(attempt.syscall_index);
                if validate_captured_fstat_attempt(attempt)? {
                    terminal_fstat = terminal_fstat.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!("process terminal fstat census overflowed")
                    })?;
                }
            }
            if usize::try_from(process.fstat_syscalls).ok() != Some(process.fstat_attempts.len())
                || terminal_fstat > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete fstat attempts");
            }
            let mut terminal_getrandom = 0u64;
            let mut prior_getrandom = None;
            for attempt in &process.getrandom_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_getrandom.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid getrandom position");
                }
                prior_getrandom = Some(attempt.syscall_index);
                if validate_captured_getrandom_attempt(attempt)? {
                    terminal_getrandom = terminal_getrandom.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!("process terminal getrandom census overflowed")
                    })?;
                }
            }
            if usize::try_from(process.getrandom_syscalls).ok()
                != Some(process.getrandom_attempts.len())
                || terminal_getrandom > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete getrandom attempts");
            }
            let mut terminal_runtime_observation = 0u64;
            let mut prior_runtime_observation = None;
            for attempt in &process.runtime_observation_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_runtime_observation.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!(
                        "process capture receipt has an invalid runtime-observation position"
                    );
                }
                prior_runtime_observation = Some(attempt.syscall_index);
                if validate_captured_runtime_observation_attempt(attempt).with_context(|| {
                    format!(
                        "validate runtime observation for process {} at syscall {}: \
                         request={:?} outcome={:?}",
                        process.id, attempt.syscall_index, attempt.request, attempt.outcome
                    )
                })? {
                    terminal_runtime_observation =
                        terminal_runtime_observation.checked_add(1).ok_or_else(|| {
                            anyhow::anyhow!(
                                "process terminal runtime-observation census overflowed"
                            )
                        })?;
                }
            }
            if usize::try_from(process.runtime_observation_syscalls).ok()
                != Some(process.runtime_observation_attempts.len())
                || terminal_runtime_observation > process.terminal_syscalls
            {
                anyhow::bail!(
                    "process capture receipt has incomplete runtime-observation attempts"
                );
            }
            let mut terminal_prlimit = 0u64;
            let mut prior_prlimit = None;
            for attempt in &process.prlimit_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_prlimit.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid prlimit position");
                }
                prior_prlimit = Some(attempt.syscall_index);
                if validate_captured_prlimit_attempt(attempt)? {
                    terminal_prlimit = terminal_prlimit.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!("process terminal prlimit census overflowed")
                    })?;
                }
            }
            if usize::try_from(process.prlimit_syscalls).ok()
                != Some(process.prlimit_attempts.len())
                || terminal_prlimit > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete prlimit attempts");
            }
            let mut terminal_close = 0u64;
            let mut prior_close = None;
            for attempt in &process.close_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_close.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid close position");
                }
                prior_close = Some(attempt.syscall_index);
                if validate_captured_close_attempt(attempt)? {
                    terminal_close = terminal_close.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!("process terminal close census overflowed")
                    })?;
                }
            }
            if usize::try_from(process.close_syscalls).ok() != Some(process.close_attempts.len())
                || terminal_close > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete close attempts");
            }
            let mut terminal_seek = 0u64;
            let mut prior_seek = None;
            for attempt in &process.seek_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_seek.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid seek position");
                }
                prior_seek = Some(attempt.syscall_index);
                if validate_captured_seek_attempt(attempt)? {
                    terminal_seek = terminal_seek.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!("process terminal seek census overflowed")
                    })?;
                }
            }
            if usize::try_from(process.seek_syscalls).ok() != Some(process.seek_attempts.len())
                || terminal_seek > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete seek attempts");
            }
            let mut terminal_file_advice = 0u64;
            let mut prior_file_advice = None;
            for attempt in &process.file_advice_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_file_advice.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid file-advice position");
                }
                prior_file_advice = Some(attempt.syscall_index);
                if validate_captured_file_advice_attempt(attempt)? {
                    terminal_file_advice = terminal_file_advice.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!("process terminal file-advice census overflowed")
                    })?;
                }
            }
            if usize::try_from(process.file_advice_syscalls).ok()
                != Some(process.file_advice_attempts.len())
                || terminal_file_advice > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete file-advice attempts");
            }
            let mut terminal_fcntl = 0u64;
            let mut prior_fcntl = None;
            for attempt in &process.fcntl_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_fcntl.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid fcntl position");
                }
                prior_fcntl = Some(attempt.syscall_index);
                if validate_captured_fcntl_attempt(attempt)? {
                    terminal_fcntl = terminal_fcntl.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!("process terminal fcntl census overflowed")
                    })?;
                }
            }
            if usize::try_from(process.fcntl_syscalls).ok() != Some(process.fcntl_attempts.len())
                || terminal_fcntl > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete fcntl attempts");
            }
            let mut terminal_ioctl = 0u64;
            let mut prior_ioctl = None;
            for attempt in &process.ioctl_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_ioctl.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid ioctl position");
                }
                prior_ioctl = Some(attempt.syscall_index);
                if validate_captured_ioctl_attempt(attempt)? {
                    terminal_ioctl = terminal_ioctl.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!("process terminal ioctl census overflowed")
                    })?;
                }
            }
            if usize::try_from(process.ioctl_syscalls).ok() != Some(process.ioctl_attempts.len())
                || terminal_ioctl > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete ioctl attempts");
            }
            let mut terminal_pipe = 0u64;
            let mut prior_pipe = None;
            for attempt in &process.pipe_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_pipe.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid pipe position");
                }
                prior_pipe = Some(attempt.syscall_index);
                if validate_captured_pipe_attempt(attempt)? {
                    terminal_pipe = terminal_pipe.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!("process terminal pipe census overflowed")
                    })?;
                }
            }
            if usize::try_from(process.pipe_syscalls).ok() != Some(process.pipe_attempts.len())
                || terminal_pipe > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete pipe attempts");
            }
            let mut terminal_epoll_create = 0u64;
            let mut prior_epoll_create = None;
            for attempt in &process.epoll_create_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_epoll_create.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!(
                        "process capture receipt has an invalid epoll-create position"
                    );
                }
                prior_epoll_create = Some(attempt.syscall_index);
                if validate_captured_epoll_create_attempt(attempt)? {
                    terminal_epoll_create = terminal_epoll_create.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!("process terminal epoll-create census overflowed")
                    })?;
                }
            }
            if usize::try_from(process.epoll_create_syscalls).ok()
                != Some(process.epoll_create_attempts.len())
                || terminal_epoll_create > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete epoll-create attempts");
            }
            let mut terminal_socket_create = 0u64;
            let mut prior_socket_create = None;
            for attempt in &process.socket_create_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_socket_create
                        .is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!(concat!(
                        "process capture receipt has an invalid ",
                        "socket-create position"
                    ));
                }
                prior_socket_create = Some(attempt.syscall_index);
                if validate_captured_socket_create_attempt(attempt)? {
                    terminal_socket_create =
                        terminal_socket_create.checked_add(1).ok_or_else(|| {
                            anyhow::anyhow!(concat!(
                                "process terminal socket-create census ",
                                "overflowed"
                            ))
                        })?;
                }
            }
            if usize::try_from(process.socket_create_syscalls).ok()
                != Some(process.socket_create_attempts.len())
                || terminal_socket_create > process.terminal_syscalls
            {
                anyhow::bail!(concat!(
                    "process capture receipt has incomplete ",
                    "socket-create attempts"
                ));
            }
            let mut terminal_socket_connect = 0u64;
            let mut prior_socket_connect = None;
            for attempt in &process.socket_connect_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_socket_connect
                        .is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!(concat!(
                        "process capture receipt has an invalid ",
                        "socket-connect position"
                    ));
                }
                prior_socket_connect = Some(attempt.syscall_index);
                if validate_captured_socket_connect_attempt(attempt)? {
                    terminal_socket_connect =
                        terminal_socket_connect.checked_add(1).ok_or_else(|| {
                            anyhow::anyhow!(concat!(
                                "process terminal socket-connect census ",
                                "overflowed"
                            ))
                        })?;
                }
            }
            if usize::try_from(process.socket_connect_syscalls).ok()
                != Some(process.socket_connect_attempts.len())
                || terminal_socket_connect > process.terminal_syscalls
            {
                anyhow::bail!(concat!(
                    "process capture receipt has incomplete ",
                    "socket-connect attempts"
                ));
            }
            let mut terminal_dup2 = 0u64;
            let mut prior_dup2 = None;
            for attempt in &process.dup2_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_dup2.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!(
                        "process capture receipt has an invalid descriptor-duplication position"
                    );
                }
                prior_dup2 = Some(attempt.syscall_index);
                if validate_captured_dup2_attempt(attempt)? {
                    terminal_dup2 = terminal_dup2.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!("process terminal duplication census overflowed")
                    })?;
                }
            }
            if usize::try_from(process.dup2_syscalls).ok() != Some(process.dup2_attempts.len())
                || terminal_dup2 > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete duplication attempts");
            }
            let mut terminal_signal_state = 0u64;
            let mut prior_signal_state = None;
            for attempt in &process.signal_state_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_signal_state.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid signal-state position");
                }
                prior_signal_state = Some(attempt.syscall_index);
                if validate_captured_signal_state_attempt(attempt)? {
                    terminal_signal_state =
                        terminal_signal_state.checked_add(1).ok_or_else(|| {
                            anyhow::anyhow!("process terminal signal-state census overflowed")
                        })?;
                }
            }
            if usize::try_from(process.signal_state_syscalls).ok()
                != Some(process.signal_state_attempts.len())
                || terminal_signal_state > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete signal-state attempts");
            }
            let mut prior_signal_delivery_entries = None;
            for delivery in &process.signal_deliveries {
                validate_captured_signal_delivery(delivery)?;
                if delivery.syscall_entries > process.syscall_entries
                    || prior_signal_delivery_entries
                        .is_some_and(|prior| prior > delivery.syscall_entries)
                    || delivery.active_syscall.is_some_and(|index| {
                        index.checked_add(1) != Some(delivery.syscall_entries)
                    })
                {
                    anyhow::bail!("process capture receipt has an invalid signal delivery");
                }
                prior_signal_delivery_entries = Some(delivery.syscall_entries);
            }
            let mut terminal_umask = 0u64;
            let mut prior_umask = None;
            for attempt in &process.umask_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_umask.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid umask position");
                }
                prior_umask = Some(attempt.syscall_index);
                if validate_captured_umask_attempt(attempt)? {
                    terminal_umask = terminal_umask.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!("process terminal umask census overflowed")
                    })?;
                }
            }
            if usize::try_from(process.umask_syscalls).ok() != Some(process.umask_attempts.len())
                || terminal_umask > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete umask attempts");
            }
            let mut terminal_credentials = 0u64;
            let mut prior_credential = None;
            for attempt in &process.credential_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_credential.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid credential position");
                }
                prior_credential = Some(attempt.syscall_index);
                if validate_captured_credential_attempt(attempt)? {
                    terminal_credentials =
                        terminal_credentials.checked_add(1).ok_or_else(|| {
                            anyhow::anyhow!("process terminal credential census overflowed")
                        })?;
                }
            }
            if usize::try_from(process.credential_syscalls).ok()
                != Some(process.credential_attempts.len())
                || terminal_credentials > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete credential attempts");
            }
            let mut terminal_futex_wakes = 0u64;
            let mut prior_futex_wake = None;
            for attempt in &process.futex_wake_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_futex_wake.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid futex-wake position");
                }
                prior_futex_wake = Some(attempt.syscall_index);
                if validate_captured_futex_wake_attempt(attempt)? {
                    terminal_futex_wakes =
                        terminal_futex_wakes.checked_add(1).ok_or_else(|| {
                            anyhow::anyhow!("process terminal futex-wake census overflowed")
                        })?;
                }
            }
            if usize::try_from(process.futex_wake_syscalls).ok()
                != Some(process.futex_wake_attempts.len())
                || terminal_futex_wakes > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete futex-wake attempts");
            }
            let mut terminal_process_creations = 0u64;
            let mut prior_process_creation = None;
            for attempt in &process.process_creation_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_process_creation.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!(
                        "process capture receipt has an invalid process-creation position"
                    );
                }
                prior_process_creation = Some(attempt.syscall_index);
                if validate_captured_process_creation_attempt(attempt)? {
                    terminal_process_creations =
                        terminal_process_creations.checked_add(1).ok_or_else(|| {
                            anyhow::anyhow!("process terminal creation census overflowed")
                        })?;
                }
            }
            if usize::try_from(process.process_creation_syscalls).ok()
                != Some(process.process_creation_attempts.len())
                || terminal_process_creations > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete process-creation attempts");
            }
            let mut terminal_child_waits = 0u64;
            let mut prior_child_wait = None;
            for attempt in &process.child_wait_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_child_wait.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid child-wait position");
                }
                prior_child_wait = Some(attempt.syscall_index);
                if validate_captured_child_wait_attempt(attempt)? {
                    terminal_child_waits =
                        terminal_child_waits.checked_add(1).ok_or_else(|| {
                            anyhow::anyhow!("process terminal child-wait census overflowed")
                        })?;
                }
            }
            if usize::try_from(process.child_wait_syscalls).ok()
                != Some(process.child_wait_attempts.len())
                || terminal_child_waits > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete child-wait attempts");
            }
            let mut terminal_process_identities = 0u64;
            let mut prior_process_identity = None;
            for attempt in &process.process_identity_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_process_identity.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!(
                        "process capture receipt has an invalid process-identity position"
                    );
                }
                prior_process_identity = Some(attempt.syscall_index);
                if validate_captured_process_identity_attempt(attempt)? {
                    terminal_process_identities =
                        terminal_process_identities.checked_add(1).ok_or_else(|| {
                            anyhow::anyhow!("process terminal identity census overflowed")
                        })?;
                }
            }
            if usize::try_from(process.process_identity_syscalls).ok()
                != Some(process.process_identity_attempts.len())
                || terminal_process_identities > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete process-identity attempts");
            }
            let mut terminal_memory_state = 0u64;
            let mut prior_memory_state = None;
            for attempt in &process.memory_state_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_memory_state.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid memory-state position");
                }
                prior_memory_state = Some(attempt.syscall_index);
                if validate_captured_memory_state_attempt(attempt)? {
                    terminal_memory_state =
                        terminal_memory_state.checked_add(1).ok_or_else(|| {
                            anyhow::anyhow!("process terminal memory-state census overflowed")
                        })?;
                }
            }
            if usize::try_from(process.memory_state_syscalls).ok()
                != Some(process.memory_state_attempts.len())
                || terminal_memory_state > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete memory-state attempts");
            }
            let mut terminal_thread_state = 0u64;
            let mut prior_thread_state = None;
            let mut next_signal_delivery = 0usize;
            let mut caught_signal_frames = Vec::new();
            for attempt in &process.thread_state_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_thread_state.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid thread-state position");
                }
                prior_thread_state = Some(attempt.syscall_index);
                if let CapturedThreadStateRequest::SignalReturn {
                    delivery,
                    ..
                } = attempt.request
                {
                    while process
                        .signal_deliveries
                        .get(next_signal_delivery)
                        .is_some_and(|candidate| {
                            candidate.syscall_entries <= attempt.syscall_index
                        })
                    {
                        if captured_signal_delivery_creates_frame(
                            &process.signal_deliveries[next_signal_delivery],
                        ) {
                            caught_signal_frames.push(
                                u64::try_from(next_signal_delivery).context(
                                    "process signal-delivery index exceeds u64",
                                )?,
                            );
                        }
                        next_signal_delivery = next_signal_delivery
                            .checked_add(1)
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "process signal-delivery index overflowed"
                                )
                            })?;
                    }
                    if caught_signal_frames.pop() != Some(delivery) {
                        anyhow::bail!(
                            "process capture receipt has an invalid signal-return delivery"
                        );
                    }
                }
                if validate_captured_thread_state_attempt(attempt)? {
                    terminal_thread_state =
                        terminal_thread_state.checked_add(1).ok_or_else(|| {
                            anyhow::anyhow!("process terminal thread-state census overflowed")
                        })?;
                }
            }
            if usize::try_from(process.thread_state_syscalls).ok()
                != Some(process.thread_state_attempts.len())
                || terminal_thread_state > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete thread-state attempts");
            }
            let mut terminal_lifecycle_exits = 0u64;
            let mut prior_lifecycle_exit = None;
            for attempt in &process.lifecycle_exit_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_lifecycle_exit.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid lifecycle-exit position");
                }
                prior_lifecycle_exit = Some(attempt.syscall_index);
                if validate_captured_lifecycle_exit_attempt(attempt, process.outcome)? {
                    terminal_lifecycle_exits =
                        terminal_lifecycle_exits.checked_add(1).ok_or_else(|| {
                            anyhow::anyhow!("process terminal lifecycle-exit census overflowed")
                        })?;
                }
            }
            if usize::try_from(process.lifecycle_exit_syscalls).ok()
                != Some(process.lifecycle_exit_attempts.len())
                || process.lifecycle_exit_syscalls > 1
                || terminal_lifecycle_exits > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete lifecycle-exit attempts");
            }
            let mut terminal_statx = 0u64;
            let mut prior_statx = None;
            for attempt in &process.statx_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_statx.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid statx position");
                }
                prior_statx = Some(attempt.syscall_index);
                if validate_captured_statx_attempt(attempt)? {
                    terminal_statx = terminal_statx.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!("process terminal statx census overflowed")
                    })?;
                }
            }
            if usize::try_from(process.statx_syscalls).ok() != Some(process.statx_attempts.len())
                || terminal_statx > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete statx attempts");
            }
            let mut terminal_xattrs = 0u64;
            let mut prior_xattr = None;
            for attempt in &process.xattr_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_xattr.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid xattr position");
                }
                prior_xattr = Some(attempt.syscall_index);
                if validate_captured_xattr_attempt(attempt)? {
                    terminal_xattrs = terminal_xattrs.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!("process terminal xattr census overflowed")
                    })?;
                }
            }
            if usize::try_from(process.xattr_syscalls).ok() != Some(process.xattr_attempts.len())
                || terminal_xattrs > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete xattr attempts");
            }
            let mut terminal_readlinks = 0u64;
            let mut prior_readlink = None;
            for attempt in &process.readlink_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_readlink.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid readlink position");
                }
                prior_readlink = Some(attempt.syscall_index);
                if validate_captured_readlink_attempt(attempt)? {
                    terminal_readlinks = terminal_readlinks.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!("process terminal readlink census overflowed")
                    })?;
                }
            }
            if usize::try_from(process.readlink_syscalls).ok()
                != Some(process.readlink_attempts.len())
                || terminal_readlinks > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete readlink attempts");
            }
            let mut terminal_accesses = 0u64;
            let mut prior_access = None;
            for attempt in &process.access_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_access.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid access position");
                }
                prior_access = Some(attempt.syscall_index);
                if validate_captured_access_attempt(attempt)? {
                    terminal_accesses = terminal_accesses.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!("process terminal access census overflowed")
                    })?;
                }
            }
            if usize::try_from(process.access_syscalls).ok() != Some(process.access_attempts.len())
                || terminal_accesses > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete access attempts");
            }
            let mut terminal_getcwd = 0u64;
            let mut prior_getcwd = None;
            for attempt in &process.getcwd_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_getcwd.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid getcwd position");
                }
                prior_getcwd = Some(attempt.syscall_index);
                if validate_captured_getcwd_attempt(attempt)? {
                    terminal_getcwd = terminal_getcwd.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!("process terminal getcwd census overflowed")
                    })?;
                }
            }
            if usize::try_from(process.getcwd_syscalls).ok() != Some(process.getcwd_attempts.len())
                || terminal_getcwd > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete getcwd attempts");
            }
            let mut terminal_cwd_changes = 0u64;
            let mut prior_cwd_change = None;
            for attempt in &process.cwd_change_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_cwd_change.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid cwd-change position");
                }
                prior_cwd_change = Some(attempt.syscall_index);
                if validate_captured_cwd_change_attempt(attempt)? {
                    terminal_cwd_changes =
                        terminal_cwd_changes.checked_add(1).ok_or_else(|| {
                            anyhow::anyhow!("process terminal cwd-change census overflowed")
                        })?;
                }
            }
            if usize::try_from(process.cwd_change_syscalls).ok()
                != Some(process.cwd_change_attempts.len())
                || terminal_cwd_changes > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete cwd-change attempts");
            }
            let mut terminal_path_removals = 0u64;
            let mut prior_path_removal = None;
            for attempt in &process.path_removal_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_path_removal.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid path-removal position");
                }
                prior_path_removal = Some(attempt.syscall_index);
                if validate_captured_path_removal_attempt(attempt)? {
                    terminal_path_removals =
                        terminal_path_removals.checked_add(1).ok_or_else(|| {
                            anyhow::anyhow!("process terminal path-removal census overflowed")
                        })?;
                }
            }
            if usize::try_from(process.path_removal_syscalls).ok()
                != Some(process.path_removal_attempts.len())
                || terminal_path_removals > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete path-removal attempts");
            }
            let mut terminal_directory_creations = 0u64;
            let mut prior_directory_creation = None;
            for attempt in &process.directory_creation_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_directory_creation.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!(
                        "process capture receipt has an invalid directory-creation position"
                    );
                }
                prior_directory_creation = Some(attempt.syscall_index);
                if validate_captured_directory_creation_attempt(attempt)? {
                    terminal_directory_creations =
                        terminal_directory_creations.checked_add(1).ok_or_else(|| {
                            anyhow::anyhow!("process terminal directory-creation census overflowed")
                        })?;
                }
            }
            if usize::try_from(process.directory_creation_syscalls).ok()
                != Some(process.directory_creation_attempts.len())
                || terminal_directory_creations > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete directory-creation attempts");
            }
            let mut terminal_path_renames = 0u64;
            let mut prior_path_rename = None;
            for attempt in &process.path_rename_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_path_rename.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid path-rename position");
                }
                prior_path_rename = Some(attempt.syscall_index);
                if validate_captured_path_rename_attempt(attempt)? {
                    terminal_path_renames =
                        terminal_path_renames.checked_add(1).ok_or_else(|| {
                            anyhow::anyhow!("process terminal path-rename census overflowed")
                        })?;
                }
            }
            if usize::try_from(process.path_rename_syscalls).ok()
                != Some(process.path_rename_attempts.len())
                || terminal_path_renames > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete path-rename attempts");
            }
            let mut terminal_path_mode_changes = 0u64;
            let mut prior_path_mode_change = None;
            for attempt in &process.path_mode_change_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_path_mode_change.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!(
                        "process capture receipt has an invalid path-mode-change position"
                    );
                }
                prior_path_mode_change = Some(attempt.syscall_index);
                if validate_captured_path_mode_change_attempt(attempt)? {
                    terminal_path_mode_changes =
                        terminal_path_mode_changes.checked_add(1).ok_or_else(|| {
                            anyhow::anyhow!("process terminal path-mode-change census overflowed")
                        })?;
                }
            }
            if usize::try_from(process.path_mode_change_syscalls).ok()
                != Some(process.path_mode_change_attempts.len())
                || terminal_path_mode_changes > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete path-mode-change attempts");
            }
            let mut terminal_descriptor_timestamp_changes = 0u64;
            let mut prior_descriptor_timestamp_change = None;
            for attempt in &process.descriptor_timestamp_change_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_descriptor_timestamp_change
                        .is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!(
                        "process capture receipt has an invalid timestamp-change position"
                    );
                }
                prior_descriptor_timestamp_change = Some(attempt.syscall_index);
                if validate_captured_descriptor_timestamp_change_attempt(attempt)? {
                    terminal_descriptor_timestamp_changes = terminal_descriptor_timestamp_changes
                        .checked_add(1)
                        .ok_or_else(|| {
                            anyhow::anyhow!("process terminal timestamp-change census overflowed")
                        })?;
                }
            }
            if usize::try_from(process.descriptor_timestamp_change_syscalls).ok()
                != Some(process.descriptor_timestamp_change_attempts.len())
                || terminal_descriptor_timestamp_changes > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete timestamp-change attempts");
            }
            let mut terminal_symbolic_link_creations = 0u64;
            let mut prior_symbolic_link_creation = None;
            for attempt in &process.symbolic_link_creation_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_symbolic_link_creation
                        .is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid symbolic-link position");
                }
                prior_symbolic_link_creation = Some(attempt.syscall_index);
                if validate_captured_symbolic_link_creation_attempt(attempt)? {
                    terminal_symbolic_link_creations = terminal_symbolic_link_creations
                        .checked_add(1)
                        .ok_or_else(|| {
                            anyhow::anyhow!("process terminal symbolic-link census overflowed")
                        })?;
                }
            }
            if usize::try_from(process.symbolic_link_creation_syscalls).ok()
                != Some(process.symbolic_link_creation_attempts.len())
                || terminal_symbolic_link_creations > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete symbolic-link attempts");
            }
            let mut terminal_filesystem_statuses = 0u64;
            let mut prior_filesystem_status = None;
            for attempt in &process.filesystem_status_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_filesystem_status.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!(
                        "process capture receipt has an invalid filesystem-status position"
                    );
                }
                prior_filesystem_status = Some(attempt.syscall_index);
                if validate_captured_filesystem_status_attempt(attempt)? {
                    terminal_filesystem_statuses =
                        terminal_filesystem_statuses.checked_add(1).ok_or_else(|| {
                            anyhow::anyhow!("process terminal filesystem-status census overflowed")
                        })?;
                }
            }
            if usize::try_from(process.filesystem_status_syscalls).ok()
                != Some(process.filesystem_status_attempts.len())
                || terminal_filesystem_statuses > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete filesystem-status attempts");
            }
            let mut terminal_reads = 0u64;
            let mut prior_read = None;
            for attempt in &process.read_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_read.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid read position");
                }
                prior_read = Some(attempt.syscall_index);
                if validate_captured_read_attempt(attempt)? {
                    terminal_reads = terminal_reads.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!("process terminal read census overflowed")
                    })?;
                }
            }
            if usize::try_from(process.read_syscalls).ok() != Some(process.read_attempts.len())
                || terminal_reads > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete read attempts");
            }
            let mut terminal_getdents64 = 0u64;
            let mut prior_getdents64 = None;
            for attempt in &process.getdents64_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_getdents64.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid getdents64 position");
                }
                prior_getdents64 = Some(attempt.syscall_index);
                if validate_captured_getdents64_attempt(attempt)? {
                    terminal_getdents64 = terminal_getdents64.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!("process terminal getdents64 census overflowed")
                    })?;
                }
            }
            if usize::try_from(process.getdents64_syscalls).ok()
                != Some(process.getdents64_attempts.len())
                || terminal_getdents64 > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete getdents64 attempts");
            }
            let mut terminal_mmaps = 0u64;
            let mut prior_mmap = None;
            for attempt in &process.mmap_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_mmap.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid mmap position");
                }
                prior_mmap = Some(attempt.syscall_index);
                if validate_captured_mmap_attempt(attempt)? {
                    terminal_mmaps = terminal_mmaps.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!("process terminal mmap census overflowed")
                    })?;
                }
            }
            if usize::try_from(process.mmap_syscalls).ok() != Some(process.mmap_attempts.len())
                || terminal_mmaps > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete mmap attempts");
            }
            let mut terminal_writes = 0u64;
            let mut prior_write = None;
            for attempt in &process.write_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_write.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid write position");
                }
                prior_write = Some(attempt.syscall_index);
                if validate_captured_write_attempt(attempt)? {
                    terminal_writes = terminal_writes.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!("process terminal write census overflowed")
                    })?;
                }
            }
            if usize::try_from(process.write_syscalls).ok() != Some(process.write_attempts.len())
                || terminal_writes > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete write attempts");
            }
            let mut terminal_unmodeled = 0u64;
            let mut prior_unmodeled = None;
            for attempt in &process.unmodeled_syscall_attempts {
                if attempt.syscall_index >= process.syscall_entries
                    || prior_unmodeled.is_some_and(|prior| prior >= attempt.syscall_index)
                    || !modeled_syscalls.insert(attempt.syscall_index)
                {
                    anyhow::bail!("process capture receipt has an invalid unmodeled position");
                }
                prior_unmodeled = Some(attempt.syscall_index);
                if validate_captured_unmodeled_syscall_attempt(attempt)? {
                    terminal_unmodeled = terminal_unmodeled.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!("process terminal unmodeled syscall census overflowed")
                    })?;
                }
            }
            if usize::try_from(process.unmodeled_syscalls).ok()
                != Some(process.unmodeled_syscall_attempts.len())
                || terminal_unmodeled > process.terminal_syscalls
            {
                anyhow::bail!("process capture receipt has incomplete unmodeled attempts");
            }
            if u64::try_from(modeled_syscalls.len()).ok() != Some(process.syscall_entries) {
                anyhow::bail!("process capture receipt has incomplete syscall attempt coverage");
            }
            let modeled_terminal_syscalls = terminal_opens
                .checked_add(terminal_newfstatat)
                .and_then(|total| total.checked_add(terminal_fstat))
                .and_then(|total| total.checked_add(terminal_getrandom))
                .and_then(|total| total.checked_add(terminal_runtime_observation))
                .and_then(|total| total.checked_add(terminal_prlimit))
                .and_then(|total| total.checked_add(terminal_close))
                .and_then(|total| total.checked_add(terminal_seek))
                .and_then(|total| total.checked_add(terminal_fcntl))
                .and_then(|total| total.checked_add(terminal_ioctl))
                .and_then(|total| total.checked_add(terminal_signal_state))
                .and_then(|total| total.checked_add(terminal_umask))
                .and_then(|total| total.checked_add(terminal_credentials))
                .and_then(|total| total.checked_add(terminal_futex_wakes))
                .and_then(|total| total.checked_add(terminal_process_creations))
                .and_then(|total| total.checked_add(terminal_child_waits))
                .and_then(|total| total.checked_add(terminal_process_identities))
                .and_then(|total| total.checked_add(terminal_memory_state))
                .and_then(|total| total.checked_add(terminal_thread_state))
                .and_then(|total| total.checked_add(terminal_lifecycle_exits))
                .and_then(|total| total.checked_add(terminal_statx))
                .and_then(|total| total.checked_add(terminal_xattrs))
                .and_then(|total| total.checked_add(terminal_readlinks))
                .and_then(|total| total.checked_add(terminal_accesses))
                .and_then(|total| total.checked_add(terminal_getcwd))
                .and_then(|total| total.checked_add(terminal_cwd_changes))
                .and_then(|total| total.checked_add(terminal_path_removals))
                .and_then(|total| total.checked_add(terminal_directory_creations))
                .and_then(|total| total.checked_add(terminal_path_renames))
                .and_then(|total| total.checked_add(terminal_path_mode_changes))
                .and_then(|total| total.checked_add(terminal_descriptor_timestamp_changes))
                .and_then(|total| total.checked_add(terminal_symbolic_link_creations))
                .and_then(|total| total.checked_add(terminal_filesystem_statuses))
                .and_then(|total| total.checked_add(terminal_reads))
                .and_then(|total| total.checked_add(terminal_getdents64))
                .and_then(|total| total.checked_add(terminal_mmaps))
                .and_then(|total| total.checked_add(terminal_writes))
                .and_then(|total| total.checked_add(terminal_unmodeled))
                .ok_or_else(|| anyhow::anyhow!("modeled terminal syscall census overflowed"))?;
            if modeled_terminal_syscalls > process.terminal_syscalls {
                anyhow::bail!("process capture receipt overcounts terminal syscalls");
            }
            if process.syscall_entries
                != process
                    .syscall_exits
                    .checked_add(process.terminal_syscalls)
                    .ok_or_else(|| anyhow::anyhow!("process syscall census overflowed"))?
            {
                anyhow::bail!("process capture receipt has incomplete syscall pairs");
            }
        }
        if parents.get(&self.root) != Some(&None) {
            anyhow::bail!("process capture receipt has no unique root");
        }
        if self
            .processes
            .iter()
            .find(|process| process.id == self.root)
            .is_none_or(|process| process.invocations.is_empty())
        {
            anyhow::bail!("process capture root has no successful exec boundary");
        }
        if parents.values().filter(|parent| parent.is_none()).count() != 1 {
            anyhow::bail!("process capture receipt has more than one root");
        }
        for process in &self.processes {
            for delivery in &process.signal_deliveries {
                if let CapturedSignalSource::TracedProcess { process } = delivery.source {
                    if !parents.contains_key(&process) {
                        anyhow::bail!(
                            "process capture receipt has an unknown signal-delivery source"
                        );
                    }
                }
            }
        }
        let mut created_processes = BTreeSet::new();
        let mut thread_groups = self
            .processes
            .iter()
            .map(|process| (process.id, process.id))
            .collect::<BTreeMap<_, _>>();
        for process in &self.processes {
            for attempt in &process.process_creation_attempts {
                if let CapturedProcessCreationOutcome::Created(child) = attempt.outcome {
                    if parents.get(&child) != Some(&Some(process.id))
                        || !created_processes.insert(child)
                    {
                        anyhow::bail!(
                            "process capture receipt has an invalid created-process edge"
                        );
                    }
                    if process_creation_shares_thread_group(attempt.request) {
                        let parent_group = *thread_groups
                            .get(&process.id)
                            .context("created thread has no parent thread group")?;
                        thread_groups.insert(child, parent_group);
                    }
                }
            }
        }
        let mut reaped_processes = BTreeSet::new();
        for process in &self.processes {
            for attempt in &process.child_wait_attempts {
                if let CapturedChildWaitSelector::Child(child) = attempt.request.selector {
                    if parents.get(&child) != Some(&Some(process.id)) {
                        anyhow::bail!(
                            "process capture receipt has an invalid requested child-wait edge"
                        );
                    }
                }
                if let CapturedChildWaitOutcome::Reaped { child, .. } = attempt.outcome {
                    if parents.get(&child) != Some(&Some(process.id))
                        || !reaped_processes.insert(child)
                    {
                        anyhow::bail!("process capture receipt has an invalid reaped-process edge");
                    }
                }
            }
        }
        for process in &self.processes {
            for attempt in &process.process_identity_attempts {
                match (attempt.request, attempt.outcome) {
                    (
                        CapturedProcessIdentityRequest::Getpid,
                        CapturedProcessIdentityOutcome::Identity(identity),
                    ) if thread_groups.get(&process.id) == Some(&identity) => {}
                    (
                        CapturedProcessIdentityRequest::Gettid,
                        CapturedProcessIdentityOutcome::Identity(identity),
                    ) if identity == process.id => {}
                    (
                        CapturedProcessIdentityRequest::Getppid,
                        CapturedProcessIdentityOutcome::Identity(identity),
                    ) if thread_groups
                        .get(&process.id)
                        .and_then(|group| parents.get(group))
                        .copied()
                        .flatten()
                        .and_then(|parent| thread_groups.get(&parent))
                        .copied()
                        == Some(identity) => {}
                    (
                        CapturedProcessIdentityRequest::Getpgrp,
                        CapturedProcessIdentityOutcome::ProcessGroup(_),
                    )
                    | (_, CapturedProcessIdentityOutcome::Terminated) => {}
                    _ => anyhow::bail!(
                        "process capture receipt has an invalid process-identity edge"
                    ),
                }
            }
        }
        let mut reaches_root = BTreeSet::from([self.root]);
        for (id, parent) in &parents {
            if let Some(parent) = parent {
                if parent == id || !parents.contains_key(parent) {
                    anyhow::bail!("process capture receipt has an invalid parent edge");
                }
                let mut candidate = *id;
                let mut path = Vec::new();
                let mut local = BTreeSet::new();
                while !reaches_root.contains(&candidate) {
                    if !local.insert(candidate) {
                        anyhow::bail!("process capture receipt contains a process cycle");
                    }
                    path.push(candidate);
                    candidate = parents
                        .get(&candidate)
                        .copied()
                        .flatten()
                        .ok_or_else(|| anyhow::anyhow!("process does not descend from root"))?;
                }
                reaches_root.extend(path);
            }
        }
        self.syscall_entries()?;
        self.syscall_exits()?;
        self.terminal_syscalls()?;
        self.exec_syscalls()?;
        self.open_syscalls()?;
        self.newfstatat_syscalls()?;
        self.fstat_syscalls()?;
        self.getrandom_syscalls()?;
        self.runtime_observation_syscalls()?;
        self.prlimit_syscalls()?;
        self.close_syscalls()?;
        self.seek_syscalls()?;
        self.fcntl_syscalls()?;
        self.ioctl_syscalls()?;
        self.signal_state_syscalls()?;
        self.umask_syscalls()?;
        self.credential_syscalls()?;
        self.futex_wake_syscalls()?;
        self.process_creation_syscalls()?;
        self.child_wait_syscalls()?;
        self.process_identity_syscalls()?;
        self.memory_state_syscalls()?;
        self.thread_state_syscalls()?;
        self.lifecycle_exit_syscalls()?;
        self.statx_syscalls()?;
        self.xattr_syscalls()?;
        self.readlink_syscalls()?;
        self.access_syscalls()?;
        self.getcwd_syscalls()?;
        self.cwd_change_syscalls()?;
        self.path_removal_syscalls()?;
        self.directory_creation_syscalls()?;
        self.path_rename_syscalls()?;
        self.path_mode_change_syscalls()?;
        self.descriptor_timestamp_change_syscalls()?;
        self.symbolic_link_creation_syscalls()?;
        self.filesystem_status_syscalls()?;
        self.read_syscalls()?;
        self.getdents64_syscalls()?;
        self.mmap_syscalls()?;
        self.write_syscalls()?;
        self.unmodeled_syscalls()?;
        Ok(())
    }

    /// Receipt-local root identity.
    pub fn root(&self) -> CapturedProcessId {
        self.root
    }

    /// Complete process records in capture-discovery order.
    pub fn processes(&self) -> &[CapturedProcess] {
        &self.processes
    }

    /// Total syscall entries covered by matching return or terminal stops.
    pub fn syscall_entries(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.syscall_entries)
    }

    /// Total syscalls with ordinary return stops.
    pub fn syscall_exits(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.syscall_exits)
    }

    /// Total syscalls resolved by their task's terminal state.
    pub fn terminal_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.terminal_syscalls)
    }

    /// Total exec requests represented by exact attempt records.
    pub fn exec_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.exec_syscalls)
    }

    /// Total pathname-opening requests represented by exact attempt records.
    pub fn open_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.open_syscalls)
    }

    /// Total native `newfstatat` requests represented by exact attempt records.
    pub fn newfstatat_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.newfstatat_syscalls)
    }

    /// Total native `fstat` requests represented by exact attempt records.
    pub fn fstat_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.fstat_syscalls)
    }

    /// Total native `getrandom` requests represented by exact attempt records.
    pub fn getrandom_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.getrandom_syscalls)
    }

    /// Total supported runtime observations represented by exact attempts.
    pub fn runtime_observation_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| {
            process.runtime_observation_syscalls
        })
    }

    /// Total native `prlimit64` requests represented by exact attempt records.
    pub fn prlimit_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.prlimit_syscalls)
    }

    /// Total native descriptor `close` requests represented by exact attempts.
    pub fn close_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.close_syscalls)
    }

    /// Total native descriptor `lseek` requests represented by exact attempts.
    pub fn seek_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.seek_syscalls)
    }

    /// Total bounded native `fadvise64` requests represented by exact attempts.
    pub fn file_advice_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.file_advice_syscalls)
    }

    /// Total native descriptor `fcntl` requests represented by exact attempts.
    pub fn fcntl_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.fcntl_syscalls)
    }

    /// Total supported native descriptor `ioctl` probes represented by exact attempts.
    pub fn ioctl_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.ioctl_syscalls)
    }

    /// Total native `pipe2` requests represented by exact attempts.
    pub fn pipe_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.pipe_syscalls)
    }

    /// Total native `epoll_create1` requests represented by exact attempts.
    pub fn epoll_create_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.epoll_create_syscalls)
    }

    /// Total supported native `socket` requests represented by exact attempts.
    pub fn socket_create_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| {
            process.socket_create_syscalls
        })
    }

    /// Total supported native `connect` requests represented by exact attempts.
    pub fn socket_connect_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| {
            process.socket_connect_syscalls
        })
    }

    /// Total native descriptor duplications represented by exact attempts.
    pub fn dup2_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.dup2_syscalls)
    }

    /// Total signal-disposition and mask requests represented by exact attempts.
    pub fn signal_state_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.signal_state_syscalls)
    }

    /// Total ptrace signal-delivery stops represented by exact records.
    pub fn signal_deliveries(&self) -> Result<u64> {
        self.processes.iter().try_fold(0u64, |total, process| {
            total
                .checked_add(u64::try_from(process.signal_deliveries.len())?)
                .context("process signal-delivery census overflowed")
        })
    }

    /// Total native file-creation mask transitions represented by exact attempts.
    pub fn umask_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.umask_syscalls)
    }

    /// Total native credential requests represented by exact attempts.
    pub fn credential_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.credential_syscalls)
    }

    /// Total process-private futex wakes represented by exact attempts.
    pub fn futex_wake_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.futex_wake_syscalls)
    }

    /// Total supported native process creations represented by exact attempts.
    pub fn process_creation_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.process_creation_syscalls)
    }

    /// Total supported native child waits represented by exact attempts.
    pub fn child_wait_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.child_wait_syscalls)
    }

    /// Total supported native process-identity queries represented by exact attempts.
    pub fn process_identity_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.process_identity_syscalls)
    }

    /// Total process-memory state requests represented by exact attempts.
    pub fn memory_state_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.memory_state_syscalls)
    }

    /// Total thread-runtime state requests represented by exact attempts.
    pub fn thread_state_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.thread_state_syscalls)
    }

    /// Total native lifecycle-exit requests represented by exact attempts.
    pub fn lifecycle_exit_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.lifecycle_exit_syscalls)
    }

    /// Total native `statx` requests represented by exact attempt records.
    pub fn statx_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.statx_syscalls)
    }

    /// Total pathname xattr reads represented by exact attempt records.
    pub fn xattr_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.xattr_syscalls)
    }

    /// Total native link-target requests represented by exact attempt records.
    pub fn readlink_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.readlink_syscalls)
    }

    /// Total native access predicates represented by exact attempt records.
    pub fn access_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.access_syscalls)
    }

    /// Total native Linux `getcwd` requests represented by exact attempts.
    pub fn getcwd_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.getcwd_syscalls)
    }

    /// Total native Linux working-directory mutations represented by attempts.
    pub fn cwd_change_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.cwd_change_syscalls)
    }

    /// Total native Linux pathname removals represented by exact attempts.
    pub fn path_removal_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.path_removal_syscalls)
    }

    /// Total native Linux directory creations represented by exact attempts.
    pub fn directory_creation_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| {
            process.directory_creation_syscalls
        })
    }

    /// Total native Linux pathname renames represented by exact attempts.
    pub fn path_rename_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.path_rename_syscalls)
    }

    /// Total pathname permission changes represented by exact attempts.
    pub fn path_mode_change_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.path_mode_change_syscalls)
    }

    /// Total descriptor-form timestamp mutations represented by exact attempts.
    pub fn descriptor_timestamp_change_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| {
            process.descriptor_timestamp_change_syscalls
        })
    }

    /// Total native symbolic-link creations represented by exact attempts.
    pub fn symbolic_link_creation_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| {
            process.symbolic_link_creation_syscalls
        })
    }

    /// Total native Linux filesystem-status requests represented by attempts.
    pub fn filesystem_status_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| {
            process.filesystem_status_syscalls
        })
    }

    /// Total descriptor reads represented by exact attempt records.
    pub fn read_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.read_syscalls)
    }

    /// Total `getdents64` requests represented by exact attempt records.
    pub fn getdents64_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.getdents64_syscalls)
    }

    /// Number of successful enumerations without a complete directory value.
    pub fn unwitnessed_directory_enumerations(&self) -> Result<u64> {
        self.processes.iter().try_fold(0u64, |total, process| {
            let unwitnessed = process
                .getdents64_attempts
                .iter()
                .filter(|attempt| {
                    matches!(
                        attempt.outcome,
                        CapturedGetdents64Outcome::Enumerated {
                            directory_value: CapturedDirectoryValue::Unwitnessed,
                            ..
                        }
                    )
                })
                .count();
            total
                .checked_add(
                    u64::try_from(unwitnessed)
                        .context("directory enumeration count exceeds u64")?,
                )
                .ok_or_else(|| anyhow::anyhow!("directory witness census overflowed"))
        })
    }

    /// Fail closed until every successful enumeration has a complete value.
    pub fn require_witnessed_directory_enumerations(&self) -> Result<()> {
        self.validate_complete()?;
        let unwitnessed = self.unwitnessed_directory_enumerations()?;
        if unwitnessed != 0 {
            anyhow::bail!("process capture has {unwitnessed} unwitnessed directory enumerations");
        }
        Ok(())
    }

    /// Total native Linux `mmap` requests represented by exact attempt records.
    pub fn mmap_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.mmap_syscalls)
    }

    /// Total scalar descriptor writes represented by exact attempt records.
    pub fn write_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.write_syscalls)
    }

    /// Total exact syscall attempts not yet assigned a supported model.
    pub fn unmodeled_syscalls(&self) -> Result<u64> {
        checked_census_sum(&self.processes, |process| process.unmodeled_syscalls)
    }

    /// Number of file-backed mappings whose consumed bytes remain unwitnessed.
    pub fn unwitnessed_file_mappings(&self) -> Result<u64> {
        self.processes.iter().try_fold(0u64, |total, process| {
            let exec_mappings =
                process
                    .invocations
                    .iter()
                    .try_fold(0u64, |invocation_total, invocation| {
                        let unwitnessed = invocation
                            .file_mappings
                            .iter()
                            .filter(|mapping| mapping.value == CapturedMappedFileValue::Unwitnessed)
                            .count();
                        invocation_total
                            .checked_add(
                                u64::try_from(unwitnessed)
                                    .context("exec mapping count exceeds u64")?,
                            )
                            .ok_or_else(|| anyhow::anyhow!("exec mapping census overflowed"))
                    })?;
            let mmap_mappings =
                process
                    .mmap_attempts
                    .iter()
                    .try_fold(0u64, |mmap_total, attempt| {
                        let unwitnessed = u64::from(matches!(
                            attempt.outcome,
                            CapturedMmapOutcome::Mapped {
                                file_value: Some(CapturedMappedFileValue::Unwitnessed),
                                ..
                            }
                        ));
                        mmap_total
                            .checked_add(unwitnessed)
                            .ok_or_else(|| anyhow::anyhow!("mmap value census overflowed"))
                    })?;
            total
                .checked_add(exec_mappings)
                .and_then(|sum| sum.checked_add(mmap_mappings))
                .ok_or_else(|| anyhow::anyhow!("mapped file value census overflowed"))
        })
    }

    /// Fail closed until every file-backed mapping has a content witness.
    pub fn require_witnessed_file_mappings(&self) -> Result<()> {
        self.validate_complete()?;
        let unwitnessed = self.unwitnessed_file_mappings()?;
        if unwitnessed != 0 {
            anyhow::bail!("process capture has {unwitnessed} unwitnessed file-backed mappings");
        }
        Ok(())
    }
}

pub(crate) fn validate_captured_invocation(invocation: &CapturedInvocation) -> Result<()> {
    if invocation.executable.is_empty()
        || invocation.argv.is_empty()
        || invocation.argv[0].is_empty()
        || invocation.working_directory.is_empty()
        || invocation.file_creation_mask & !0o777 != 0
    {
        anyhow::bail!("process capture receipt has an incomplete exec boundary");
    }
    validate_environment(&invocation.environment)?;
    invocation.signal_state.validate()?;
    invocation.credentials.validate()?;
    validate_exec_file_mappings(&invocation.file_mappings)
}

pub(crate) fn validate_captured_exec_attempt(attempt: &CapturedExecAttempt) -> Result<Option<u64>> {
    validate_exec_request(&attempt.request)?;
    match attempt.outcome {
        CapturedExecOutcome::Loaded(index) => Ok(Some(index)),
        CapturedExecOutcome::Failed(errno) if errno > 0 => Ok(None),
        CapturedExecOutcome::Failed(_) => {
            anyhow::bail!("process capture receipt has an invalid exec errno")
        }
    }
}

pub(crate) fn validate_captured_open_attempt(attempt: &CapturedOpenAttempt) -> Result<bool> {
    validate_open_request(&attempt.request)?;
    match &attempt.outcome {
        CapturedOpenOutcome::Opened {
            descriptor,
            identity,
            mount_id: _,
            file_value,
        } if *descriptor >= 0
            && !identity.target.is_empty()
            && identity.target.len() <= LINUX_IDENTITY_BYTES_LIMIT
            && !identity.target.contains(&0)
            && !matches!(file_value, Some(CapturedMappedFileValue::Unwitnessed))
            && file_value
                .is_none_or(|_| identity.kind == CapturedDescriptorKind::RegularFile)
            && file_value.is_none_or(valid_mapped_file_value) =>
        {
            Ok(false)
        }
        CapturedOpenOutcome::Failed(errno) if *errno > 0 => Ok(false),
        CapturedOpenOutcome::Terminated => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid open result"),
    }
}

pub(crate) fn validate_captured_newfstatat_attempt(
    attempt: &CapturedNewfstatatAttempt,
) -> Result<bool> {
    let request = &attempt.request;
    let absolute = request.path.first() == Some(&b'/');
    if (request.path_is_null && !request.path.is_empty())
        || request.path.contains(&0)
        || request.path.len() > LINUX_PATH_MAX
        || (absolute && request.base.is_some())
        || request
            .base
            .as_ref()
            .is_some_and(|base| base.is_empty() || base.contains(&0))
    {
        anyhow::bail!("process capture receipt has an invalid newfstatat request");
    }
    match attempt.outcome {
        CapturedNewfstatatOutcome::Metadata(metadata) => {
            let supported_flags =
                LINUX_AT_SYMLINK_NOFOLLOW | LINUX_AT_NO_AUTOMOUNT | LINUX_AT_EMPTY_PATH;
            let file_type = metadata.mode & libc::S_IFMT;
            let known_file_type = matches!(
                file_type,
                libc::S_IFREG
                    | libc::S_IFDIR
                    | libc::S_IFLNK
                    | libc::S_IFCHR
                    | libc::S_IFBLK
                    | libc::S_IFIFO
                    | libc::S_IFSOCK
            );
            if request.flags & !supported_flags != 0
                || (request.path.is_empty() && request.flags & LINUX_AT_EMPTY_PATH == 0)
                || (!absolute && request.base.is_none())
                || (!absolute
                    && (request.directory_descriptor == libc::AT_FDCWD || !request.path.is_empty())
                    && request
                        .base
                        .as_ref()
                        .is_some_and(|base| base.first() != Some(&b'/')))
                || (!absolute
                    && request.directory_descriptor != libc::AT_FDCWD
                    && request.directory_descriptor < 0)
                || !known_file_type
                || metadata.mode & !(libc::S_IFMT | 0o7777) != 0
                || !(0..1_000_000_000).contains(&metadata.access_time_nanoseconds)
                || !(0..1_000_000_000).contains(&metadata.modification_time_nanoseconds)
                || !(0..1_000_000_000).contains(&metadata.change_time_nanoseconds)
            {
                anyhow::bail!("process capture receipt has an invalid newfstatat result");
            }
            Ok(false)
        }
        CapturedNewfstatatOutcome::Failed(errno) if errno > 0 => Ok(false),
        CapturedNewfstatatOutcome::Terminated => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid newfstatat result"),
    }
}

pub(crate) fn validate_captured_fstat_attempt(attempt: &CapturedFstatAttempt) -> Result<bool> {
    let request = &attempt.request;
    if request.identity.is_some() != request.mount_id.is_some()
        || (request.descriptor < 0 && request.identity.is_some())
        || request.identity.as_ref().is_some_and(|identity| {
            identity.target.is_empty()
                || identity.target.len() > LINUX_IDENTITY_BYTES_LIMIT
                || identity.target.contains(&0)
        })
    {
        anyhow::bail!("process capture receipt has an invalid fstat request");
    }
    match attempt.outcome {
        CapturedFstatOutcome::Metadata(metadata) => {
            let file_type = metadata.mode & libc::S_IFMT;
            let known_file_type = matches!(
                file_type,
                libc::S_IFREG
                    | libc::S_IFDIR
                    | libc::S_IFLNK
                    | libc::S_IFCHR
                    | libc::S_IFBLK
                    | libc::S_IFIFO
                    | libc::S_IFSOCK
            );
            let identity_matches = request.identity.as_ref().is_some_and(|identity| {
                identity.device == metadata.device
                    && identity.inode == metadata.inode
                    && descriptor_kind_matches(identity, metadata.mode)
            });
            if request.output_is_null
                || request.descriptor < 0
                || !identity_matches
                || !known_file_type
                || metadata.mode & !(libc::S_IFMT | 0o7777) != 0
                || !(0..1_000_000_000).contains(&metadata.access_time_nanoseconds)
                || !(0..1_000_000_000).contains(&metadata.modification_time_nanoseconds)
                || !(0..1_000_000_000).contains(&metadata.change_time_nanoseconds)
            {
                anyhow::bail!("process capture receipt has an invalid fstat result");
            }
            Ok(false)
        }
        CapturedFstatOutcome::Failed(errno) if errno > 0 => Ok(false),
        CapturedFstatOutcome::Terminated => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid fstat result"),
    }
}

pub(crate) fn validate_captured_getrandom_attempt(
    attempt: &CapturedGetrandomAttempt,
) -> Result<bool> {
    match attempt.outcome {
        CapturedGetrandomOutcome::Bytes {
            returned_bytes,
            digest,
        } => {
            let supported_flags = LINUX_GRND_NONBLOCK | LINUX_GRND_RANDOM | LINUX_GRND_INSECURE;
            if returned_bytes > attempt.request.requested_bytes
                || (returned_bytes != 0 && attempt.request.output_is_null)
                || attempt.request.flags & !supported_flags != 0
                || (attempt.request.flags & LINUX_GRND_RANDOM != 0
                    && attempt.request.flags & LINUX_GRND_INSECURE != 0)
                || (returned_bytes == 0 && digest != *blake3::hash(&[]).as_bytes())
            {
                anyhow::bail!("process capture receipt has an invalid getrandom result");
            }
            Ok(false)
        }
        CapturedGetrandomOutcome::Failed(errno) if errno > 0 => Ok(false),
        CapturedGetrandomOutcome::Terminated => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid getrandom result"),
    }
}

pub(crate) fn validate_captured_runtime_observation_attempt(
    attempt: &CapturedRuntimeObservationAttempt,
) -> Result<bool> {
    let successful_nonnull = match (&attempt.request, &attempt.outcome) {
        (
            CapturedRuntimeObservationRequest::CallingProcessResourceUsage { output_is_null },
            CapturedRuntimeObservationOutcome::ResourceUsage(usage),
        ) => {
            let counters = [
                usage.user_seconds,
                usage.system_seconds,
                usage.maximum_resident_set_size,
                usage.integral_shared_memory_size,
                usage.integral_unshared_data_size,
                usage.integral_unshared_stack_size,
                usage.minor_page_faults,
                usage.major_page_faults,
                usage.swaps,
                usage.block_inputs,
                usage.block_outputs,
                usage.messages_sent,
                usage.messages_received,
                usage.signals_received,
                usage.voluntary_context_switches,
                usage.involuntary_context_switches,
            ];
            !*output_is_null
                && (0..1_000_000).contains(&usage.user_microseconds)
                && (0..1_000_000).contains(&usage.system_microseconds)
                && counters.into_iter().all(|value| value >= 0)
        }
        (
            CapturedRuntimeObservationRequest::SystemInformation { output_is_null },
            CapturedRuntimeObservationOutcome::SystemInformation(information),
        ) => !*output_is_null && information.uptime_seconds >= 0 && information.memory_unit != 0,
        (
            CapturedRuntimeObservationRequest::SystemIdentity { output_is_null },
            CapturedRuntimeObservationOutcome::SystemIdentity(identity),
        ) => {
            !*output_is_null
                && valid_uts_name_field(&identity.system_name, false)
                && valid_uts_name_field(&identity.node_name, true)
                && valid_uts_name_field(&identity.release, false)
                && valid_uts_name_field(&identity.version, false)
                && valid_uts_name_field(&identity.machine, false)
                && valid_uts_name_field(&identity.domain_name, true)
        }
        (
            CapturedRuntimeObservationRequest::CallingThreadAffinity {
                capacity,
                output_is_null,
            },
            CapturedRuntimeObservationOutcome::CpuAffinity(mask),
        ) => {
            valid_cpu_affinity_capacity(*capacity)
                && !*output_is_null
                && !mask.is_empty()
                && mask.len() <= *capacity as usize
                && mask.len() % std::mem::size_of::<libc::c_ulong>() == 0
                && mask.iter().any(|byte| *byte != 0)
        }
        (
            request @ CapturedRuntimeObservationRequest::DescriptorReadiness {
                descriptors,
                ..
            },
            CapturedRuntimeObservationOutcome::DescriptorReadiness {
                ready,
                returned_events,
            },
        ) => {
            let result_shape_is_valid = match descriptors.len() {
                3 => {
                    (*ready == 0 && returned_events.as_slice() == [0, 0, 0])
                        || (*ready == 1
                            && returned_events.as_slice() == [LINUX_POLLHUP, 0, 0]
                            && descriptors[0]
                                .identity
                                .as_ref()
                                .is_some_and(|identity| {
                                    identity.kind == CapturedDescriptorKind::Fifo
                                }))
                }
                4 => {
                    *ready == 4
                        && returned_events.as_slice()
                            == [
                                LINUX_POLLIN | LINUX_POLLHUP,
                                LINUX_POLLIN | LINUX_POLLOUT,
                                LINUX_POLLIN | LINUX_POLLOUT,
                                LINUX_POLLNVAL,
                            ]
                }
                6 => {
                    *ready == 6
                        && returned_events.as_slice()
                            == [
                                LINUX_POLLIN | LINUX_POLLHUP,
                                LINUX_POLLIN | LINUX_POLLOUT,
                                LINUX_POLLIN | LINUX_POLLOUT,
                                LINUX_POLLIN,
                                LINUX_POLLOUT,
                                LINUX_POLLNVAL,
                            ]
                }
                _ => false,
            };
            valid_captured_poll_request(request)
                && result_shape_is_valid
                && returned_events.iter().filter(|events| **events != 0).count()
                    == *ready as usize
        }
        (
            request @ CapturedRuntimeObservationRequest::DescriptorReadiness { .. },
            CapturedRuntimeObservationOutcome::Failed(errno),
        ) if valid_captured_poll_request(request) && *errno > 0 => return Ok(false),
        (
            request @ CapturedRuntimeObservationRequest::DescriptorReadiness { .. },
            CapturedRuntimeObservationOutcome::Terminated,
        ) if valid_captured_poll_request(request) => return Ok(true),
        (
            CapturedRuntimeObservationRequest::CallingThreadAffinity { capacity, .. },
            CapturedRuntimeObservationOutcome::Failed(errno),
        ) if valid_cpu_affinity_capacity(*capacity) && *errno > 0 => return Ok(false),
        (
            CapturedRuntimeObservationRequest::CallingThreadAffinity { capacity, .. },
            CapturedRuntimeObservationOutcome::Terminated,
        ) if valid_cpu_affinity_capacity(*capacity) => return Ok(true),
        (request, CapturedRuntimeObservationOutcome::Failed(errno))
            if !matches!(
                request,
                CapturedRuntimeObservationRequest::DescriptorReadiness { .. }
                    | CapturedRuntimeObservationRequest::CallingThreadAffinity { .. }
            ) && *errno > 0 =>
        {
            return Ok(false);
        }
        (request, CapturedRuntimeObservationOutcome::Terminated)
            if !matches!(
                request,
                CapturedRuntimeObservationRequest::DescriptorReadiness { .. }
                    | CapturedRuntimeObservationRequest::CallingThreadAffinity { .. }
            ) =>
        {
            return Ok(true);
        }
        _ => false,
    };
    if !successful_nonnull {
        anyhow::bail!(
            "process capture receipt has an invalid runtime-observation result: \
             request={:?} outcome={:?}",
            attempt.request,
            attempt.outcome
        );
    }
    Ok(false)
}

fn valid_cpu_affinity_capacity(capacity: u64) -> bool {
    capacity != 0
        && capacity <= LINUX_CPU_AFFINITY_CAPACITY_LIMIT
        && capacity % std::mem::size_of::<libc::c_ulong>() as u64 == 0
}

fn valid_captured_poll_request(request: &CapturedRuntimeObservationRequest) -> bool {
    let CapturedRuntimeObservationRequest::DescriptorReadiness {
        descriptors,
        timeout_milliseconds,
    } = request
    else {
        return false;
    };
    let valid_identity = |descriptor: &CapturedPollDescriptor| {
        descriptor.identity.is_some() == descriptor.mount_id.is_some()
            && descriptor.identity.as_ref().is_none_or(|identity| {
                !identity.target.is_empty()
                    && identity.target.len() <= LINUX_IDENTITY_BYTES_LIMIT
                    && !identity.target.contains(&0)
            })
    };
    let rustc_probe = descriptors.len() == 3
        && descriptors.iter().enumerate().all(|(index, descriptor)| {
            descriptor.descriptor == index as i32
                && descriptor.requested_events == 0
                && descriptor.identity.is_some()
                && valid_identity(descriptor)
        });
    // The six-descriptor profile is a measured pipe-jobserver probe: both
    // endpoints must identify the same anonymous pipe, separate from stdin.
    let pipe_jobserver_pair = descriptors.len() == 6
        && descriptors[3].identity.as_ref().is_some_and(|pipe| {
            pipe.kind == CapturedDescriptorKind::Fifo
                && pipe.inode != 0
                && pipe.target == format!("pipe:[{}]", pipe.inode).as_bytes()
                && descriptors[4].identity.as_ref() == Some(pipe)
                && descriptors[3].mount_id == descriptors[4].mount_id
                && descriptors[0].identity.as_ref().is_some_and(|input| {
                    (input.device, input.inode) != (pipe.device, pipe.inode)
                })
        });
    let xargs_probe = matches!(descriptors.len(), 4 | 6)
        && descriptors.iter().enumerate().all(|(index, descriptor)| {
            descriptor.descriptor == index as i32
                && descriptor.requested_events == LINUX_POLLIN | LINUX_POLLOUT
                && valid_identity(descriptor)
                && match (index, descriptor.identity.as_ref()) {
                    (0, Some(identity)) => identity.kind == CapturedDescriptorKind::Fifo,
                    (1 | 2, Some(identity)) => {
                        identity.kind == CapturedDescriptorKind::RegularFile
                            || (identity.kind == CapturedDescriptorKind::CharacterDevice
                                && identity.target == b"/dev/null")
                    }
                    (3 | 4, Some(_)) if pipe_jobserver_pair => true,
                    (index, None) if index == descriptors.len() - 1 => true,
                    _ => false,
                }
        });
    *timeout_milliseconds == 0 && (rustc_probe || xargs_probe)
}

fn valid_uts_name_field(value: &[u8], allow_empty: bool) -> bool {
    (allow_empty || !value.is_empty()) && value.len() <= 64 && !value.contains(&0)
}

pub(crate) fn validate_captured_prlimit_attempt(attempt: &CapturedPrlimitAttempt) -> Result<bool> {
    let request = attempt.request;
    match attempt.outcome {
        CapturedPrlimitOutcome::Limit(limit)
            if request.new_limit_is_null
                && !request.old_limit_is_null
                && request.resource <= LINUX_RLIMIT_RTTIME
                && limit.current <= limit.maximum =>
        {
            Ok(false)
        }
        CapturedPrlimitOutcome::Checked
            if request.new_limit_is_null
                && request.old_limit_is_null
                && request.resource <= LINUX_RLIMIT_RTTIME =>
        {
            Ok(false)
        }
        CapturedPrlimitOutcome::Failed(errno) if request.new_limit_is_null && errno > 0 => {
            Ok(false)
        }
        CapturedPrlimitOutcome::MutationUnsupported { errno }
            if !request.new_limit_is_null && errno.is_none_or(|errno| errno > 0) =>
        {
            Ok(false)
        }
        CapturedPrlimitOutcome::Terminated if request.new_limit_is_null => Ok(true),
        CapturedPrlimitOutcome::Mutated {
            requested,
            previous,
        } if !request.new_limit_is_null
            && request.resource <= LINUX_RLIMIT_RTTIME
            && requested.current <= requested.maximum
            && previous.is_some() == !request.old_limit_is_null
            && previous.is_none_or(|limit| limit.current <= limit.maximum) =>
        {
            Ok(false)
        }
        CapturedPrlimitOutcome::MutationFailed { errno, .. }
            if !request.new_limit_is_null && errno > 0 =>
        {
            Ok(false)
        }
        CapturedPrlimitOutcome::MutationTerminated { .. } if !request.new_limit_is_null => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid prlimit result"),
    }
}

pub(crate) fn validate_captured_close_attempt(attempt: &CapturedCloseAttempt) -> Result<bool> {
    let request = &attempt.request;
    if request.identity.is_some() != request.mount_id.is_some()
        || (request.descriptor < 0 && request.identity.is_some())
        || request.identity.as_ref().is_some_and(|identity| {
            identity.target.is_empty()
                || identity.target.len() > LINUX_IDENTITY_BYTES_LIMIT
                || identity.target.contains(&0)
        })
    {
        anyhow::bail!("process capture receipt has an invalid close request");
    }
    match attempt.outcome {
        CapturedCloseOutcome::Closed { file_value }
            if request.identity.is_some()
                && request.descriptor >= 0
                && !matches!(file_value, Some(CapturedMappedFileValue::Unwitnessed))
                && file_value.is_none_or(valid_mapped_file_value)
                && match (&request.identity, file_value) {
                    (
                        Some(CapturedDescriptorIdentity {
                            kind: CapturedDescriptorKind::RegularFile,
                            ..
                        }),
                        Some(CapturedMappedFileValue::WholeFile { .. }),
                    ) => true,
                    (_, None) => true,
                    _ => false,
                } => Ok(false),
        CapturedCloseOutcome::Failed(errno) if errno > 0 => Ok(false),
        CapturedCloseOutcome::Terminated => Ok(true),
        _ => {
            anyhow::bail!("process capture receipt has an invalid close result")
        }
    }
}

pub(crate) fn validate_captured_seek_attempt(attempt: &CapturedSeekAttempt) -> Result<bool> {
    let request = &attempt.request;
    if request.identity.is_some() != request.mount_id.is_some()
        || (request.descriptor < 0 && request.identity.is_some())
        || request.identity.as_ref().is_some_and(|identity| {
            identity.target.is_empty()
                || identity.target.len() > LINUX_IDENTITY_BYTES_LIMIT
                || identity.target.contains(&0)
        })
    {
        anyhow::bail!("process capture receipt has an invalid seek request");
    }
    match attempt.outcome {
        CapturedSeekOutcome::Positioned(position)
            if request.identity.is_some()
                && request.descriptor >= 0
                && request.whence <= libc::SEEK_HOLE as u64
                && position <= i64::MAX as u64 =>
        {
            Ok(false)
        }
        CapturedSeekOutcome::Failed(errno) if errno > 0 => Ok(false),
        CapturedSeekOutcome::Terminated => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid seek result"),
    }
}

pub(crate) fn validate_captured_file_advice_attempt(
    attempt: &CapturedFileAdviceAttempt,
) -> Result<bool> {
    let request = &attempt.request;
    if request.identity.is_some() != request.mount_id.is_some()
        || (request.descriptor < 0 && request.identity.is_some())
        || request.identity.as_ref().is_some_and(|identity| {
            identity.target.is_empty()
                || identity.target.len() > LINUX_IDENTITY_BYTES_LIMIT
                || identity.target.contains(&0)
        })
        || request.offset != 0
        || request.length != 0
        || request.advice != libc::POSIX_FADV_SEQUENTIAL
    {
        anyhow::bail!("process capture receipt has an unsupported file-advice contract");
    }
    match attempt.outcome {
        CapturedFileAdviceOutcome::Completed
            if request.descriptor >= 0
                && request.identity.as_ref().is_some_and(|identity| {
                    identity.kind == CapturedDescriptorKind::RegularFile
                }) =>
        {
            Ok(false)
        }
        CapturedFileAdviceOutcome::Failed(errno) if errno > 0 => Ok(false),
        CapturedFileAdviceOutcome::Terminated => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid file-advice result"),
    }
}

pub(crate) fn validate_captured_fcntl_attempt(attempt: &CapturedFcntlAttempt) -> Result<bool> {
    let request = &attempt.request;
    let set_lock_wait = request.command == libc::F_SETLKW as u32;
    if request.identity.is_some() != request.mount_id.is_some()
        || (request.descriptor < 0 && request.identity.is_some())
        || request.identity.as_ref().is_some_and(|identity| {
            identity.target.is_empty()
                || identity.target.len() > LINUX_IDENTITY_BYTES_LIMIT
                || identity.target.contains(&0)
        })
        || request.record_lock.is_some_and(|lock| {
            !set_lock_wait
                || request.argument == 0
                || !matches!(
                    lock.lock_type,
                    value if value == libc::F_RDLCK as i16
                        || value == libc::F_WRLCK as i16
                        || value == libc::F_UNLCK as i16
                )
                || !matches!(
                    lock.whence,
                    value if value == libc::SEEK_SET as i16
                        || value == libc::SEEK_CUR as i16
                        || value == libc::SEEK_END as i16
                )
        })
    {
        anyhow::bail!("process capture receipt has an invalid fcntl request");
    }
    match &attempt.outcome {
        CapturedFcntlOutcome::Returned {
            value,
            result_identity,
            result_mount_id,
        } => {
            if request.descriptor < 0
                || request.identity.is_none()
                || (set_lock_wait && request.record_lock.is_none())
                || result_identity.is_some() != result_mount_id.is_some()
                || result_identity.as_ref().is_some_and(|identity| {
                    identity.target.is_empty()
                        || identity.target.len() > LINUX_IDENTITY_BYTES_LIMIT
                        || identity.target.contains(&0)
                })
            {
                anyhow::bail!("process capture receipt has an invalid fcntl result");
            }
            let duplicates = matches!(
                request.command,
                command if command == libc::F_DUPFD as u32
                    || command == libc::F_DUPFD_CLOEXEC as u32
            );
            if duplicates {
                let descriptor = i32::try_from(*value)
                    .context("process capture fcntl descriptor exceeds i32")?;
                if descriptor < 0
                    || descriptor == request.descriptor
                    || u64::try_from(descriptor)
                        .context("process capture fcntl descriptor is negative")?
                        < request.argument
                    || result_identity.as_ref() != request.identity.as_ref()
                    || *result_mount_id != request.mount_id
                {
                    anyhow::bail!("process capture receipt has an invalid fcntl duplicate");
                }
            } else if result_identity.is_some() {
                anyhow::bail!("process capture receipt has an unexpected fcntl descriptor");
            }
            Ok(false)
        }
        CapturedFcntlOutcome::Failed(errno) if *errno > 0 => Ok(false),
        CapturedFcntlOutcome::Terminated => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid fcntl result"),
    }
}

pub(crate) fn validate_captured_ioctl_attempt(attempt: &CapturedIoctlAttempt) -> Result<bool> {
    let request = &attempt.request;
    if request.descriptor < 0
        || request.identity.target.is_empty()
        || request.identity.target.len() > LINUX_IDENTITY_BYTES_LIMIT
        || request.identity.target.contains(&0)
        || !request.is_terminal_probe()
    {
        anyhow::bail!("process capture receipt has an invalid ioctl request");
    }
    match attempt.outcome {
        CapturedIoctlOutcome::Failed(errno) if errno == libc::ENOTTY => Ok(false),
        CapturedIoctlOutcome::Terminated => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid ioctl result"),
    }
}

pub(crate) fn validate_captured_pipe_attempt(attempt: &CapturedPipeAttempt) -> Result<bool> {
    match &attempt.outcome {
        CapturedPipeOutcome::Created {
            read_descriptor,
            read_identity,
            read_mount_id,
            write_descriptor,
            write_identity,
            write_mount_id,
        } => {
            let allowed_flags = (libc::O_CLOEXEC | libc::O_NONBLOCK) as u32;
            let identity_valid = |identity: &CapturedDescriptorIdentity| {
                !identity.target.is_empty()
                    && identity.target.len() <= LINUX_IDENTITY_BYTES_LIMIT
                    && !identity.target.contains(&0)
                    && identity.kind == CapturedDescriptorKind::Fifo
            };
            if attempt.request.flags & !allowed_flags != 0
                || *read_descriptor < 0
                || *write_descriptor < 0
                || read_descriptor == write_descriptor
                || !identity_valid(read_identity)
                || !identity_valid(write_identity)
                || read_identity != write_identity
                || read_mount_id != write_mount_id
            {
                anyhow::bail!("process capture receipt has an invalid pipe result");
            }
            Ok(false)
        }
        CapturedPipeOutcome::Failed(errno) if *errno > 0 => Ok(false),
        CapturedPipeOutcome::Terminated => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid pipe result"),
    }
}

pub(crate) fn validate_captured_epoll_create_attempt(
    attempt: &CapturedEpollCreateAttempt,
) -> Result<bool> {
    if attempt.request.flags & !(libc::EPOLL_CLOEXEC as u32) != 0 {
        anyhow::bail!("process capture receipt has unsupported epoll-create flags");
    }
    match &attempt.outcome {
        CapturedEpollCreateOutcome::Created {
            descriptor,
            identity,
            ..
        } => {
            if *descriptor < 0
                || identity.target != b"anon_inode:[eventpoll]"
                || identity.kind != CapturedDescriptorKind::Other
            {
                anyhow::bail!("process capture receipt has an invalid epoll-create result");
            }
            Ok(false)
        }
        CapturedEpollCreateOutcome::Failed(errno) if *errno > 0 => Ok(false),
        CapturedEpollCreateOutcome::Terminated => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid epoll-create result"),
    }
}

fn captured_socket_identity_is_valid(
    identity: &CapturedDescriptorIdentity,
) -> bool {
    if identity.kind != CapturedDescriptorKind::Socket || identity.inode == 0 {
        return false;
    }
    let Some(inode) = identity
        .target
        .strip_prefix(b"socket:[")
        .and_then(|target| target.strip_suffix(b"]"))
    else {
        return false;
    };
    if inode.is_empty()
        || inode[0] == b'0'
        || !inode.iter().all(u8::is_ascii_digit)
    {
        return false;
    }
    let Some(target_inode) = inode.iter().try_fold(0u64, |value, digit| {
        value.checked_mul(10)?.checked_add(u64::from(*digit - b'0'))
    }) else {
        return false;
    };
    target_inode == identity.inode
}

fn captured_socket_create_request_is_supported(
    request: CapturedSocketCreateRequest,
) -> bool {
    let allowed_flags = libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK;
    request.domain == libc::AF_UNIX
        && request.socket_type & !allowed_flags == libc::SOCK_STREAM
        && request.protocol == 0
}

pub(crate) fn validate_captured_socket_create_attempt(
    attempt: &CapturedSocketCreateAttempt,
) -> Result<bool> {
    if !captured_socket_create_request_is_supported(attempt.request) {
        anyhow::bail!(
            "process capture receipt has an unsupported socket-create request"
        );
    }
    match &attempt.outcome {
        CapturedSocketCreateOutcome::Created {
            descriptor,
            identity,
            mount_id,
        } if *descriptor >= 0
            && captured_socket_identity_is_valid(identity)
            && *mount_id > 0 =>
        {
            Ok(false)
        }
        CapturedSocketCreateOutcome::Created { .. } => {
            anyhow::bail!(
                "process capture receipt has an invalid socket-create result"
            )
        }
        CapturedSocketCreateOutcome::Failed(errno) if *errno > 0 => Ok(false),
        CapturedSocketCreateOutcome::Terminated => Ok(true),
        CapturedSocketCreateOutcome::Failed(_) => {
            anyhow::bail!(
                "process capture receipt has an invalid socket-create failure"
            )
        }
    }
}

fn captured_unix_socket_address_is_valid(
    address_length: u32,
    address: &CapturedUnixSocketAddress,
) -> bool {
    const FAMILY_BYTES: u32 = std::mem::size_of::<libc::sa_family_t>() as u32;
    const UNIX_ADDRESS_BYTES: u32 =
        std::mem::size_of::<libc::sockaddr_un>() as u32;
    if !(FAMILY_BYTES..=UNIX_ADDRESS_BYTES).contains(&address_length) {
        return false;
    }
    let path_bytes = usize::try_from(address_length - FAMILY_BYTES).unwrap();
    match address {
        CapturedUnixSocketAddress::Unnamed => path_bytes == 0,
        CapturedUnixSocketAddress::Pathname { path, terminated } => {
            !path.is_empty()
                && !path.contains(&0)
                && if *terminated {
                    path.len() < path_bytes
                } else {
                    path.len() == path_bytes
                }
        }
        CapturedUnixSocketAddress::Abstract { name } => {
            path_bytes > 0 && name.len() == path_bytes - 1
        }
    }
}

pub(crate) fn validate_captured_socket_connect_attempt(
    attempt: &CapturedSocketConnectAttempt,
) -> Result<bool> {
    let request = &attempt.request;
    if request.descriptor < 0
        || !captured_socket_identity_is_valid(&request.identity)
        || request.mount_id == 0
        || !captured_unix_socket_address_is_valid(
            request.address_length,
            &request.address,
        )
    {
        anyhow::bail!(
            "process capture receipt has an invalid socket-connect request"
        );
    }
    match &attempt.outcome {
        CapturedSocketConnectOutcome::Connected { identity, mount_id }
            if identity == &request.identity && *mount_id == request.mount_id =>
        {
            Ok(false)
        }
        CapturedSocketConnectOutcome::Connected { .. } => {
            anyhow::bail!(
                "process capture receipt has an invalid socket-connect result"
            )
        }
        CapturedSocketConnectOutcome::Failed {
            errno,
            identity,
            mount_id,
        } if *errno > 0
            && identity == &request.identity
            && *mount_id == request.mount_id =>
        {
            Ok(false)
        }
        CapturedSocketConnectOutcome::Terminated => Ok(true),
        CapturedSocketConnectOutcome::Failed { .. } => {
            anyhow::bail!(
                "process capture receipt has an invalid socket-connect failure"
            )
        }
    }
}

/// Whether an admitted regular-file value has a kernel-synthesized metadata length.
///
/// These exact sysfs files report a page-sized `st_size` even though reading
/// them returns a shorter complete value. Their exact Rush local-isolation
/// mirrors have the same property. Callers must validate their content length
/// instead of equating it with `st_size`.
pub fn regular_file_value_has_synthetic_size(target: &[u8]) -> bool {
    const CPU_ONLINE: &[u8] = b"/sys/devices/system/cpu/online";
    const THP_ENABLED: &[u8] = b"/sys/kernel/mm/transparent_hugepage/enabled";
    if matches!(target, CPU_ONLINE | THP_ENABLED) {
        return true;
    }
    let Some(relative) = target.strip_prefix(b"/tmp/rush-local-isolation-") else {
        return false;
    };
    let Some(separator) = relative.iter().position(|byte| *byte == b'/') else {
        return false;
    };
    let identity = &relative[..separator];
    !identity.is_empty()
        && identity
            .iter()
            .all(|byte| byte.is_ascii_digit() || *byte == b'-')
        && matches!(&relative[separator..], CPU_ONLINE | THP_ENABLED)
}

/// Whether an exact regular-file target may carry a captured value witness.
///
/// Ordinary absolute files and tmpfs files under `/dev/shm` are admitted.
/// Device nodes, procfs, and generic sysfs targets remain excluded; only the
/// exact synthetic-size sysfs inputs above are admitted.
pub fn regular_file_value_target(target: &[u8]) -> bool {
    if regular_file_value_has_synthetic_size(target) {
        return true;
    }
    target.starts_with(b"/")
        && (!target.starts_with(b"/dev/") || target.starts_with(b"/dev/shm/"))
        && ![b"/proc/".as_slice(), b"/sys/"]
            .iter()
            .any(|prefix| target.starts_with(prefix))
}

pub(crate) fn validate_captured_dup2_attempt(attempt: &CapturedDup2Attempt) -> Result<bool> {
    let request = &attempt.request;
    let identity_valid = |identity: &CapturedDescriptorIdentity| {
        !identity.target.is_empty()
            && identity.target.len() <= LINUX_IDENTITY_BYTES_LIMIT
            && !identity.target.contains(&0)
    };
    if request.source_identity.is_some() != request.source_mount_id.is_some()
        || request.target_identity.is_some() != request.target_mount_id.is_some()
        || (request.source_descriptor < 0 && request.source_identity.is_some())
        || (request.target_descriptor < 0 && request.target_identity.is_some())
        || request
            .source_identity
            .as_ref()
            .is_some_and(|identity| !identity_valid(identity))
        || request
            .target_identity
            .as_ref()
            .is_some_and(|identity| !identity_valid(identity))
        || match request.system_call {
            CapturedDescriptorDuplicationSystemCall::Dup => {
                request.target_descriptor < 0
                    || request.target_descriptor == request.source_descriptor
                    || request.target_identity.is_some()
                    || request.target_mount_id.is_some()
            }
            CapturedDescriptorDuplicationSystemCall::Dup2 => {
                request.target_descriptor == request.source_descriptor
                    && (request.source_identity != request.target_identity
                        || request.source_mount_id != request.target_mount_id)
            }
            CapturedDescriptorDuplicationSystemCall::Dup3 => {
                request.target_descriptor == request.source_descriptor
            }
        }
    {
        anyhow::bail!("process capture receipt has an invalid duplication request");
    }
    let expects_replaced_file_value = request.source_descriptor != request.target_descriptor
        && request.system_call != CapturedDescriptorDuplicationSystemCall::Dup
        && request.target_identity.as_ref().is_some_and(|identity| {
            identity.kind == CapturedDescriptorKind::RegularFile
                && regular_file_value_target(&identity.target)
        });
    match &attempt.outcome {
        CapturedDup2Outcome::Duplicated {
            descriptor,
            identity,
            mount_id,
            replaced_file_value,
        } if request.source_descriptor >= 0
            && request.source_identity.is_some()
            && request.target_descriptor >= 0
            && *descriptor == request.target_descriptor
            && identity_valid(identity)
            && Some(identity) == request.source_identity.as_ref()
            && Some(*mount_id) == request.source_mount_id
            && !matches!(
                replaced_file_value,
                Some(CapturedMappedFileValue::Unwitnessed)
            )
            && replaced_file_value.is_some() == expects_replaced_file_value
            && replaced_file_value.is_none_or(|value| valid_mapped_file_value(value)) =>
        {
            Ok(false)
        }
        CapturedDup2Outcome::Failed(errno) if *errno > 0 => Ok(false),
        CapturedDup2Outcome::Terminated => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid duplication result"),
    }
}

pub(crate) fn validate_captured_signal_state_attempt(
    attempt: &CapturedSignalStateAttempt,
) -> Result<bool> {
    let expected_previous_bytes = match &attempt.request {
        CapturedSignalStateRequest::Action {
            action,
            old_action_is_null,
            signal_set_bytes,
            ..
        } => {
            let signal_set_bytes = usize::try_from(*signal_set_bytes)
                .context("process capture signal-set size exceeds usize")?;
            if signal_set_bytes == 0 || signal_set_bytes > LINUX_SIGNAL_SET_BYTES_LIMIT {
                anyhow::bail!("process capture receipt has an invalid signal-set size");
            }
            let action_bytes = signal_set_bytes
                .checked_add(LINUX_RT_SIGACTION_PREFIX_BYTES)
                .context("process capture signal-action size overflowed")?;
            if action
                .as_ref()
                .is_some_and(|action| action.len() != action_bytes)
            {
                anyhow::bail!("process capture receipt has an invalid signal action");
            }
            (!*old_action_is_null).then_some(action_bytes)
        }
        CapturedSignalStateRequest::Mask {
            mask,
            old_mask_is_null,
            signal_set_bytes,
            ..
        } => {
            let signal_set_bytes = usize::try_from(*signal_set_bytes)
                .context("process capture signal-set size exceeds usize")?;
            if signal_set_bytes == 0 || signal_set_bytes > LINUX_SIGNAL_SET_BYTES_LIMIT {
                anyhow::bail!("process capture receipt has an invalid signal-set size");
            }
            if mask
                .as_ref()
                .is_some_and(|mask| mask.len() != signal_set_bytes)
            {
                anyhow::bail!("process capture receipt has an invalid signal mask");
            }
            (!*old_mask_is_null).then_some(signal_set_bytes)
        }
    };
    match (&attempt.request, &attempt.outcome) {
        (
            CapturedSignalStateRequest::Action { .. },
            CapturedSignalStateOutcome::ActionReturned { previous },
        )
        | (
            CapturedSignalStateRequest::Mask { .. },
            CapturedSignalStateOutcome::MaskReturned { previous },
        ) => {
            if previous.as_ref().map(Vec::len) != expected_previous_bytes {
                anyhow::bail!("process capture receipt has an invalid prior signal state");
            }
            Ok(false)
        }
        (_, CapturedSignalStateOutcome::Failed(errno)) if *errno > 0 => Ok(false),
        (_, CapturedSignalStateOutcome::Terminated) => Ok(true),
        _ => anyhow::bail!("process capture receipt has a mismatched signal-state result"),
    }
}

pub(crate) fn validate_captured_memory_state_attempt(
    attempt: &CapturedMemoryStateAttempt,
) -> Result<bool> {
    match (attempt.request, attempt.outcome) {
        (CapturedMemoryStateRequest::Brk { .. }, CapturedMemoryStateOutcome::Break(address))
            if address <= i64::MAX as u64 =>
        {
            Ok(false)
        }
        (
            CapturedMemoryStateRequest::Mprotect { protection, .. },
            CapturedMemoryStateOutcome::Completed,
        ) => {
            let supported = (libc::PROT_READ
                | libc::PROT_WRITE
                | libc::PROT_EXEC
                | libc::PROT_GROWSDOWN
                | libc::PROT_GROWSUP) as u32;
            if protection & !supported != 0 {
                anyhow::bail!("process capture receipt has an invalid mprotect result");
            }
            Ok(false)
        }
        (
            CapturedMemoryStateRequest::Munmap { length, .. },
            CapturedMemoryStateOutcome::Completed,
        ) if length != 0 => Ok(false),
        (_, CapturedMemoryStateOutcome::Failed(errno)) if errno > 0 => Ok(false),
        (_, CapturedMemoryStateOutcome::Terminated) => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid memory-state result"),
    }
}

pub(crate) fn validate_captured_thread_state_attempt(
    attempt: &CapturedThreadStateAttempt,
) -> Result<bool> {
    const ARCH_SET_GS: u32 = 0x1001;
    const ARCH_SET_FS: u32 = 0x1002;
    if let CapturedThreadStateRequest::SignalStack {
        address,
        flags,
        bytes,
        old_state_is_null,
    } = attempt.request
    {
        if address == 0
            || flags != 0
            || !matches!(bytes, 16_384 | 65_536)
            || !old_state_is_null
        {
            anyhow::bail!("process capture receipt has an unsupported signal-stack contract");
        }
    }
    match (attempt.request, attempt.outcome) {
        (
            CapturedThreadStateRequest::ArchPrctl {
                operation: ARCH_SET_GS | ARCH_SET_FS,
                ..
            },
            CapturedThreadStateOutcome::Completed,
        ) => Ok(false),
        (
            CapturedThreadStateRequest::SetTidAddress { .. },
            CapturedThreadStateOutcome::ThreadId(thread),
        ) if thread > 0 => Ok(false),
        (
            CapturedThreadStateRequest::SetRobustList { length, .. },
            CapturedThreadStateOutcome::Completed,
        ) if length != 0 => Ok(false),
        (
            CapturedThreadStateRequest::Rseq { length, flags, .. },
            CapturedThreadStateOutcome::Completed,
        ) if length != 0 && flags <= 1 => Ok(false),
        (
            CapturedThreadStateRequest::SignalStack {
                flags: 0,
                bytes: 16_384 | 65_536,
                old_state_is_null: true,
                ..
            },
            CapturedThreadStateOutcome::Completed,
        ) => Ok(false),
        (
            CapturedThreadStateRequest::SignalReturn { .. },
            CapturedThreadStateOutcome::SignalContextRestored,
        ) => Ok(false),
        (CapturedThreadStateRequest::SignalReturn { .. }, _) => {
            anyhow::bail!(
                "process capture receipt has an invalid signal-return result"
            )
        }
        (_, CapturedThreadStateOutcome::Failed(errno)) if errno > 0 => Ok(false),
        (_, CapturedThreadStateOutcome::Terminated) => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid thread-state result"),
    }
}

pub(crate) fn validate_captured_lifecycle_exit_attempt(
    attempt: &CapturedLifecycleExitAttempt,
    process_outcome: CapturedProcessOutcome,
) -> Result<bool> {
    let mut prior_descriptor = None;
    for file in &attempt.open_file_values {
        if file.descriptor < 0
            || prior_descriptor.is_some_and(|prior| prior >= file.descriptor)
            || file.identity.kind != CapturedDescriptorKind::RegularFile
            || file.identity.target.is_empty()
            || file.identity.target.len() > LINUX_IDENTITY_BYTES_LIMIT
            || file.identity.target.contains(&0)
            || file.identity.inode == 0
            || !matches!(file.value, CapturedMappedFileValue::WholeFile { .. })
            || !valid_mapped_file_value(file.value)
        {
            anyhow::bail!("process capture receipt has an invalid exit file value");
        }
        prior_descriptor = Some(file.descriptor);
    }
    if attempt.outcome != process_outcome {
        anyhow::bail!("process capture receipt has a mismatched lifecycle-exit outcome");
    }
    match attempt.outcome {
        CapturedProcessOutcome::Exited(status)
            if (0..=u8::MAX.into()).contains(&status)
                && attempt.request.status & i32::from(u8::MAX) == status =>
        {
            Ok(true)
        }
        CapturedProcessOutcome::Signaled(signal) if signal > 0 => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid lifecycle-exit result"),
    }
}

pub(crate) fn validate_captured_unmodeled_syscall_attempt(
    attempt: &CapturedUnmodeledSyscallAttempt,
) -> Result<bool> {
    match attempt.outcome {
        CapturedUnmodeledSyscallOutcome::Returned {
            return_value,
            is_error: true,
        } if (-4095..=-1).contains(&return_value) => Ok(false),
        CapturedUnmodeledSyscallOutcome::Returned {
            is_error: false, ..
        } => Ok(false),
        CapturedUnmodeledSyscallOutcome::Terminated => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid unmodeled syscall result"),
    }
}

fn validate_captured_statx_timestamp(timestamp: CapturedStatxTimestamp) -> bool {
    timestamp.nanoseconds < 1_000_000_000 && timestamp.reserved == 0
}

fn linux_device_number(major: u32, minor: u32) -> u64 {
    let major = u64::from(major);
    let minor = u64::from(minor);
    (minor & 0xff) | ((major & 0xfff) << 8) | ((minor & !0xff) << 12) | ((major & !0xfff) << 32)
}

fn descriptor_kind_matches(identity: &CapturedDescriptorIdentity, mode: u32) -> bool {
    let file_type = mode & libc::S_IFMT;
    match identity.kind {
        CapturedDescriptorKind::RegularFile => file_type == libc::S_IFREG,
        CapturedDescriptorKind::Directory => file_type == libc::S_IFDIR,
        CapturedDescriptorKind::CharacterDevice => file_type == libc::S_IFCHR,
        CapturedDescriptorKind::BlockDevice => file_type == libc::S_IFBLK,
        CapturedDescriptorKind::Fifo => file_type == libc::S_IFIFO,
        CapturedDescriptorKind::Socket => file_type == libc::S_IFSOCK,
        CapturedDescriptorKind::Other => true,
    }
}

pub(crate) fn validate_captured_statx_attempt(attempt: &CapturedStatxAttempt) -> Result<bool> {
    let request = &attempt.request;
    let absolute = request.path.first() == Some(&b'/');
    let meaningful_relative =
        !request.path_is_null && request.path_terminated && !request.path.is_empty() && !absolute;
    let empty_path = !request.path_is_null && request.path_terminated && request.path.is_empty();
    let descriptor_form = request.flags & LINUX_AT_EMPTY_PATH != 0
        && request.directory_descriptor >= 0
        && (request.path_is_null || empty_path);
    let cwd_empty_path = request.flags & LINUX_AT_EMPTY_PATH != 0
        && request.directory_descriptor == libc::AT_FDCWD
        && empty_path;
    if request.path.contains(&0)
        || request.path.len() > LINUX_PATH_MAX
        || (request.path_is_null && (!request.path.is_empty() || request.path_terminated))
        || (!request.path_is_null
            && !request.path_terminated
            && request.path.len() != LINUX_PATH_MAX)
        || (request.path_terminated && request.path.len() >= LINUX_PATH_MAX)
        || request.descriptor_identity.is_some() != request.descriptor_mount_id.is_some()
        || request
            .descriptor_identity
            .as_ref()
            .is_some_and(|identity| {
                identity.target.is_empty()
                    || identity.target.len() > LINUX_IDENTITY_BYTES_LIMIT
                    || identity.target.contains(&0)
            })
        || (!descriptor_form
            && (request.descriptor_identity.is_some() || request.descriptor_mount_id.is_some()))
        || (descriptor_form
            && request
                .descriptor_identity
                .as_ref()
                .is_some_and(|_| request.directory_descriptor < 0))
        || (!meaningful_relative && !cwd_empty_path && request.base.is_some())
        || request.base.as_ref().is_some_and(|base| {
            base.is_empty() || base.len() > LINUX_IDENTITY_BYTES_LIMIT || base.contains(&0)
        })
        || (absolute
            && (request.base.is_some()
                || request.descriptor_identity.is_some()
                || request.descriptor_mount_id.is_some()))
    {
        anyhow::bail!("process capture receipt has an invalid statx request");
    }
    match &attempt.outcome {
        CapturedStatxOutcome::Metadata(metadata) => {
            let supported_flags = LINUX_AT_SYMLINK_NOFOLLOW
                | LINUX_AT_NO_AUTOMOUNT
                | LINUX_AT_EMPTY_PATH
                | LINUX_AT_STATX_SYNC_TYPE;
            let path_is_valid = absolute
                || (meaningful_relative
                    && request
                        .base
                        .as_ref()
                        .is_some_and(|base| base.first() == Some(&b'/'))
                    && (request.directory_descriptor == libc::AT_FDCWD
                        || request.directory_descriptor >= 0))
                || (descriptor_form
                    && request.descriptor_identity.is_some()
                    && request.descriptor_mount_id.is_some())
                || (cwd_empty_path
                    && request
                        .base
                        .as_ref()
                        .is_some_and(|base| base.first() == Some(&b'/')));
            let known_file_type = matches!(
                u32::from(metadata.mode) & libc::S_IFMT,
                libc::S_IFREG
                    | libc::S_IFDIR
                    | libc::S_IFLNK
                    | libc::S_IFCHR
                    | libc::S_IFBLK
                    | libc::S_IFIFO
                    | libc::S_IFSOCK
            );
            let ordinary_mount_matches = metadata.mask & LINUX_STATX_MNT_ID == 0
                || metadata.mask & LINUX_STATX_MNT_ID_UNIQUE != 0
                || request
                    .descriptor_mount_id
                    .is_none_or(|mount_id| mount_id == metadata.mount_id);
            let descriptor_identity_matches =
                request.descriptor_identity.as_ref().is_none_or(|identity| {
                    identity.device
                        == linux_device_number(metadata.device_major, metadata.device_minor)
                        && (metadata.mask & LINUX_STATX_INO == 0
                            || identity.inode == metadata.inode)
                        && (metadata.mask & LINUX_STATX_TYPE == 0
                            || descriptor_kind_matches(identity, u32::from(metadata.mode)))
                });
            if request.output_is_null
                || request.flags & !supported_flags != 0
                || request.flags & LINUX_AT_STATX_SYNC_TYPE == LINUX_AT_STATX_SYNC_TYPE
                || request.mask & LINUX_STATX_RESERVED != 0
                || !path_is_valid
                || metadata.mask & !LINUX_STATX_SUPPORTED_RESULTS != 0
                || metadata.attributes & !metadata.attributes_mask != 0
                || metadata.spare0 != 0
                || metadata.spare2 != 0
                || metadata.spare3 != [0; 8]
                || !validate_captured_statx_timestamp(metadata.access_time)
                || !validate_captured_statx_timestamp(metadata.birth_time)
                || !validate_captured_statx_timestamp(metadata.change_time)
                || !validate_captured_statx_timestamp(metadata.modification_time)
                || (metadata.mask & LINUX_STATX_TYPE != 0 && !known_file_type)
                || !ordinary_mount_matches
                || !descriptor_identity_matches
            {
                anyhow::bail!("process capture receipt has an invalid statx result");
            }
            Ok(false)
        }
        CapturedStatxOutcome::Failed(errno) if *errno > 0 => Ok(false),
        CapturedStatxOutcome::Terminated => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid statx result"),
    }
}

pub(crate) fn validate_captured_xattr_attempt(attempt: &CapturedXattrAttempt) -> Result<bool> {
    let request = &attempt.request;
    let absolute = request.path.first() == Some(&b'/');
    let meaningful_relative =
        !request.path_is_null && request.path_terminated && !request.path.is_empty() && !absolute;
    let pathname_form = matches!(
        request.system_call,
        CapturedXattrSystemCall::Getxattr | CapturedXattrSystemCall::Lgetxattr
    );
    let descriptor_form = matches!(
        request.system_call,
        CapturedXattrSystemCall::Fgetxattr | CapturedXattrSystemCall::Fsetxattr
    );
    let descriptor_shape_is_valid = if pathname_form {
        request.descriptor.is_none()
            && request.descriptor_identity.is_none()
            && request.descriptor_mount_id.is_none()
    } else {
        request.descriptor.is_some()
            && request.path.is_empty()
            && request.path_is_null
            && !request.path_terminated
            && request.base.is_none()
            && (request.descriptor_identity.is_some() == request.descriptor_mount_id.is_some())
            && request.descriptor_identity.as_ref().is_none_or(|identity| {
                !identity.target.is_empty()
                    && identity.target.len() <= LINUX_IDENTITY_BYTES_LIMIT
                    && !identity.target.contains(&0)
                    && identity.inode != 0
            })
            && request.descriptor_mount_id.is_none_or(|mount_id| mount_id != 0)
    };
    let set_form = request.system_call == CapturedXattrSystemCall::Fsetxattr;
    let value_shape_is_valid = if set_form {
        request.requested_bytes <= LINUX_XATTR_SIZE_MAX as u64
            && request.set_value.as_ref().is_some_and(|value| {
                u64::try_from(value.len()).ok() == Some(request.requested_bytes)
            })
            && (!request.value_is_null || request.requested_bytes == 0)
            && request.flags & !((libc::XATTR_CREATE | libc::XATTR_REPLACE) as u32) == 0
    } else {
        request.set_value.is_none() && request.flags == 0
    };
    if !descriptor_form && !pathname_form
        || !descriptor_shape_is_valid
        || !value_shape_is_valid
        || request.path.contains(&0)
        || request.path.len() > LINUX_PATH_MAX
        || (request.path_is_null && (!request.path.is_empty() || request.path_terminated))
        || (!request.path_is_null
            && !request.path_terminated
            && request.path.len() != LINUX_PATH_MAX)
        || (request.path_terminated && request.path.len() >= LINUX_PATH_MAX)
        || request.name.contains(&0)
        || request.name.len() > LINUX_XATTR_NAME_MAX + 1
        || (request.name_is_null && (!request.name.is_empty() || request.name_terminated))
        || (!request.name_is_null
            && !request.name_terminated
            && request.name.len() != LINUX_XATTR_NAME_MAX + 1)
        || (request.name_terminated && request.name.len() > LINUX_XATTR_NAME_MAX)
        || (!meaningful_relative && request.base.is_some())
        || request.base.as_ref().is_some_and(|base| {
            base.is_empty() || base.len() > LINUX_IDENTITY_BYTES_LIMIT || base.contains(&0)
        })
    {
        anyhow::bail!("process capture receipt has an invalid xattr request");
    }
    match &attempt.outcome {
        CapturedXattrOutcome::Value {
            returned_bytes,
            bytes,
        } => {
            let target_is_valid = if pathname_form {
                !request.path_is_null
                    && request.path_terminated
                    && !request.path.is_empty()
                    && (absolute
                        || request
                            .base
                            .as_ref()
                            .is_some_and(|base| base.first() == Some(&b'/')))
            } else {
                request.descriptor.is_some_and(|descriptor| descriptor >= 0)
                    && request.descriptor_identity.is_some()
                    && request.descriptor_mount_id.is_some()
            };
            let name_is_valid =
                !request.name_is_null && request.name_terminated && !request.name.is_empty();
            let result_is_valid = if request.requested_bytes == 0 {
                bytes.is_none()
            } else if request.value_is_null {
                *returned_bytes == 0 && bytes.is_none()
            } else {
                bytes.as_ref().is_some_and(|value| {
                    u64::try_from(value.len()).ok() == Some(*returned_bytes)
                        && *returned_bytes <= request.requested_bytes
                })
            };
            if set_form
                || !target_is_valid
                || !name_is_valid
                || *returned_bytes > LINUX_XATTR_SIZE_MAX as u64
                || !result_is_valid
            {
                anyhow::bail!("process capture receipt has an invalid xattr result");
            }
            Ok(false)
        }
        CapturedXattrOutcome::Changed
            if set_form
                && request.descriptor.is_some_and(|descriptor| descriptor >= 0)
                && request.descriptor_identity.is_some()
                && request.descriptor_mount_id.is_some()
                && !request.name_is_null
                && request.name_terminated
                && !request.name.is_empty() =>
        {
            Ok(false)
        }
        CapturedXattrOutcome::Failed(errno) if *errno > 0 => Ok(false),
        CapturedXattrOutcome::Terminated => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid xattr result"),
    }
}

pub(crate) fn validate_captured_readlink_attempt(
    attempt: &CapturedReadlinkAttempt,
) -> Result<bool> {
    let request = &attempt.request;
    let absolute = request.path.first() == Some(&b'/');
    if (request.path_is_null && !request.path.is_empty())
        || request.path.contains(&0)
        || request.path.len() >= LINUX_PATH_MAX
        || (absolute && request.base.is_some())
        || request
            .base
            .as_ref()
            .is_some_and(|base| base.is_empty() || base.contains(&0))
        || (request.system_call == CapturedReadlinkSystemCall::Readlink
            && request.directory_descriptor != libc::AT_FDCWD)
    {
        anyhow::bail!("process capture receipt has an invalid readlink request");
    }
    match &attempt.outcome {
        CapturedReadlinkOutcome::Target {
            bytes,
            possibly_truncated,
        } => {
            let returned = u64::try_from(bytes.len()).context("readlink result exceeds u64")?;
            if request.path_is_null
                || (request.system_call == CapturedReadlinkSystemCall::Readlink
                    && request.path.is_empty())
                || (!absolute && request.base.is_none())
                || (!absolute
                    && request
                        .base
                        .as_ref()
                        .is_some_and(|base| base.first() != Some(&b'/')))
                || (!absolute
                    && request.directory_descriptor != libc::AT_FDCWD
                    && request.directory_descriptor < 0)
                || bytes.is_empty()
                || bytes.contains(&0)
                || bytes.len() > LINUX_READLINK_RESULT_LIMIT
                || returned > request.requested_bytes
                || *possibly_truncated != (returned == request.requested_bytes)
            {
                anyhow::bail!("process capture receipt has an invalid readlink result");
            }
            Ok(false)
        }
        CapturedReadlinkOutcome::Failed(errno) if *errno > 0 => Ok(false),
        CapturedReadlinkOutcome::Terminated => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid readlink result"),
    }
}

pub(crate) fn validate_captured_access_attempt(attempt: &CapturedAccessAttempt) -> Result<bool> {
    let request = &attempt.request;
    let absolute = request.path.first() == Some(&b'/');
    let access_mode = libc::R_OK | libc::W_OK | libc::X_OK;
    let supported_flags = LINUX_AT_EACCESS | LINUX_AT_SYMLINK_NOFOLLOW | LINUX_AT_EMPTY_PATH;
    if (request.path_is_null && !request.path.is_empty())
        || request.path.contains(&0)
        || request.path.len() >= LINUX_PATH_MAX
        || (absolute && request.base.is_some())
        || request
            .base
            .as_ref()
            .is_some_and(|base| base.is_empty() || base.contains(&0))
        || (request.system_call == CapturedAccessSystemCall::Access
            && (request.directory_descriptor != libc::AT_FDCWD || request.flags != 0))
        || (request.system_call == CapturedAccessSystemCall::Faccessat && request.flags != 0)
    {
        anyhow::bail!("process capture receipt has an invalid access request");
    }
    match attempt.outcome {
        CapturedAccessOutcome::Accessible => {
            if request.mode & !(access_mode as u32) != 0
                || request.flags & !supported_flags != 0
                || request.path_is_null
                || (request.path.is_empty()
                    && (request.system_call != CapturedAccessSystemCall::Faccessat2
                        || request.flags & LINUX_AT_EMPTY_PATH == 0))
                || (!absolute && request.base.is_none())
                || (!absolute
                    && (request.directory_descriptor == libc::AT_FDCWD || !request.path.is_empty())
                    && request
                        .base
                        .as_ref()
                        .is_some_and(|base| base.first() != Some(&b'/')))
                || (!absolute
                    && request.directory_descriptor != libc::AT_FDCWD
                    && request.directory_descriptor < 0)
            {
                anyhow::bail!("process capture receipt has an invalid access result");
            }
            Ok(false)
        }
        CapturedAccessOutcome::Failed(errno) if errno > 0 => Ok(false),
        CapturedAccessOutcome::Terminated => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid access result"),
    }
}

pub(crate) fn validate_captured_getcwd_attempt(attempt: &CapturedGetcwdAttempt) -> Result<bool> {
    match &attempt.outcome {
        CapturedGetcwdOutcome::Directory(directory) => {
            let returned = u64::try_from(directory.len())
                .context("getcwd result exceeds u64")?
                .checked_add(1)
                .context("getcwd result byte count overflowed")?;
            if directory.is_empty()
                || directory.contains(&0)
                || returned > LINUX_GETCWD_RESULT_LIMIT as u64
                || (directory.first() != Some(&b'/') && !directory.starts_with(b"(unreachable)/"))
                || returned > attempt.request.requested_bytes
            {
                anyhow::bail!("process capture receipt has an invalid getcwd result");
            }
            Ok(false)
        }
        CapturedGetcwdOutcome::Failed(errno) if *errno > 0 => Ok(false),
        CapturedGetcwdOutcome::Terminated => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid getcwd result"),
    }
}

pub(crate) fn validate_captured_cwd_change_attempt(
    attempt: &CapturedCwdChangeAttempt,
) -> Result<bool> {
    let request = &attempt.request;
    if request.path.len() > LINUX_PATH_MAX
        || request.path.contains(&0)
        || request.prior_working_directory.is_empty()
        || request.prior_working_directory.first() != Some(&b'/')
        || request.prior_working_directory.len() >= LINUX_PATH_MAX
        || request.prior_working_directory.contains(&0)
        || request.descriptor_target.as_ref().is_some_and(|target| {
            target.is_empty() || target.len() >= LINUX_PATH_MAX || target.contains(&0)
        })
    {
        anyhow::bail!("process capture receipt has an invalid cwd-change request");
    }
    match request.system_call {
        CapturedCwdChangeSystemCall::Chdir
            if request.directory_descriptor == libc::AT_FDCWD
                && request.descriptor_target.is_none()
                && (!request.path_is_null || request.path.is_empty()) => {}
        CapturedCwdChangeSystemCall::Fchdir
            if request.path.is_empty()
                && !request.path_is_null
                && (request.directory_descriptor >= 0 || request.descriptor_target.is_none()) => {}
        _ => anyhow::bail!("process capture receipt has an invalid cwd-change request"),
    }
    match &attempt.outcome {
        CapturedCwdChangeOutcome::Changed(directory)
            if !directory.is_empty()
                && directory.first() == Some(&b'/')
                && directory.len() < LINUX_PATH_MAX
                && !directory.contains(&0)
                && match request.system_call {
                    CapturedCwdChangeSystemCall::Chdir => {
                        !request.path_is_null
                            && !request.path.is_empty()
                            && request.path.len() < LINUX_PATH_MAX
                    }
                    CapturedCwdChangeSystemCall::Fchdir => {
                        request.directory_descriptor >= 0
                            && request
                                .descriptor_target
                                .as_ref()
                                .is_some_and(|target| target.first() == Some(&b'/'))
                    }
                } =>
        {
            Ok(false)
        }
        CapturedCwdChangeOutcome::Failed(errno) if *errno > 0 => Ok(false),
        CapturedCwdChangeOutcome::Terminated => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid cwd-change result"),
    }
}

pub(crate) fn validate_captured_path_removal_attempt(
    attempt: &CapturedPathRemovalAttempt,
) -> Result<bool> {
    let request = &attempt.request;
    let absolute = request.path.first() == Some(&b'/');
    if (request.path_is_null && !request.path.is_empty())
        || request.path.contains(&0)
        || request.path.len() > LINUX_PATH_MAX
        || (absolute && request.base.is_some())
        || request.base.as_ref().is_some_and(|base| {
            base.is_empty() || base.len() >= LINUX_PATH_MAX || base.contains(&0)
        })
        || (request.system_call == CapturedPathRemovalSystemCall::Unlink
            && (request.directory_descriptor != libc::AT_FDCWD || request.flags != 0))
    {
        anyhow::bail!("process capture receipt has an invalid path-removal request");
    }
    match attempt.outcome {
        CapturedPathRemovalOutcome::Removed => {
            if request.path_is_null
                || request.path.is_empty()
                || request.path.len() >= LINUX_PATH_MAX
                || request.flags & !LINUX_AT_REMOVEDIR != 0
                || (!absolute && request.base.is_none())
                || (!absolute
                    && request
                        .base
                        .as_ref()
                        .is_some_and(|base| base.first() != Some(&b'/')))
                || (!absolute
                    && request.directory_descriptor != libc::AT_FDCWD
                    && request.directory_descriptor < 0)
            {
                anyhow::bail!("process capture receipt has an invalid path-removal result");
            }
            Ok(false)
        }
        CapturedPathRemovalOutcome::Failed(errno) if errno > 0 => Ok(false),
        CapturedPathRemovalOutcome::Terminated => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid path-removal result"),
    }
}

pub(crate) fn validate_captured_directory_creation_attempt(
    attempt: &CapturedDirectoryCreationAttempt,
) -> Result<bool> {
    let request = &attempt.request;
    let absolute = request.path.first() == Some(&b'/');
    if (request.path_is_null && !request.path.is_empty())
        || request.path.contains(&0)
        || request.path.len() > LINUX_PATH_MAX
        || (absolute && request.base.is_some())
        || request.base.as_ref().is_some_and(|base| {
            base.is_empty() || base.len() >= LINUX_PATH_MAX || base.contains(&0)
        })
        || (request.system_call == CapturedDirectoryCreationSystemCall::Mkdir
            && request.directory_descriptor != libc::AT_FDCWD)
    {
        anyhow::bail!("process capture receipt has an invalid directory-creation request");
    }
    match attempt.outcome {
        CapturedDirectoryCreationOutcome::Created => {
            if request.path_is_null
                || request.path.is_empty()
                || request.path.len() >= LINUX_PATH_MAX
                || (!absolute && request.base.is_none())
                || (!absolute
                    && request
                        .base
                        .as_ref()
                        .is_some_and(|base| base.first() != Some(&b'/')))
                || (!absolute
                    && request.directory_descriptor != libc::AT_FDCWD
                    && request.directory_descriptor < 0)
            {
                anyhow::bail!("process capture receipt has an invalid directory-creation result");
            }
            Ok(false)
        }
        CapturedDirectoryCreationOutcome::Failed(errno) if errno > 0 => Ok(false),
        CapturedDirectoryCreationOutcome::Terminated => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid directory-creation result"),
    }
}

fn validate_captured_bounded_path_operand(
    path: &[u8],
    path_is_null: bool,
    path_terminated: bool,
    base: Option<&[u8]>,
) -> Result<bool> {
    let absolute = path.first() == Some(&b'/');
    let meaningful_relative = !path_is_null && path_terminated && !path.is_empty() && !absolute;
    if path.contains(&0)
        || path.len() > LINUX_PATH_MAX
        || (path_is_null && (!path.is_empty() || path_terminated))
        || (!path_is_null && !path_terminated && path.len() != LINUX_PATH_MAX)
        || (path_terminated && path.len() >= LINUX_PATH_MAX)
        || (!meaningful_relative && base.is_some())
        || base.is_some_and(|base| {
            base.is_empty() || base.len() > LINUX_IDENTITY_BYTES_LIMIT || base.contains(&0)
        })
    {
        anyhow::bail!("process capture receipt has an invalid bounded-path operand");
    }
    Ok(absolute)
}

pub(crate) fn validate_captured_path_rename_attempt(
    attempt: &CapturedPathRenameAttempt,
) -> Result<bool> {
    let request = &attempt.request;
    let old_absolute = validate_captured_bounded_path_operand(
        &request.old_path,
        request.old_path_is_null,
        request.old_path_terminated,
        request.old_base.as_deref(),
    )?;
    let new_absolute = validate_captured_bounded_path_operand(
        &request.new_path,
        request.new_path_is_null,
        request.new_path_terminated,
        request.new_base.as_deref(),
    )?;
    if (request.old_base.is_some()
        && request.old_directory_descriptor != libc::AT_FDCWD
        && request.old_directory_descriptor < 0)
        || (request.new_base.is_some()
            && request.new_directory_descriptor != libc::AT_FDCWD
            && request.new_directory_descriptor < 0)
    {
        anyhow::bail!("process capture receipt has an invalid path-rename base");
    }
    match request.system_call {
        CapturedPathRenameSystemCall::Rename => {
            if request.old_directory_descriptor != libc::AT_FDCWD
                || request.new_directory_descriptor != libc::AT_FDCWD
                || request.flags != 0
            {
                anyhow::bail!("process capture receipt has an invalid rename request");
            }
        }
        CapturedPathRenameSystemCall::RenameAt if request.flags != 0 => {
            anyhow::bail!("process capture receipt has an invalid renameat request");
        }
        CapturedPathRenameSystemCall::RenameAt => {}
        CapturedPathRenameSystemCall::RenameAt2 => {}
    }
    match attempt.outcome {
        CapturedPathRenameOutcome::Completed => {
            let allowed_flags =
                LINUX_RENAME_NOREPLACE | LINUX_RENAME_EXCHANGE | LINUX_RENAME_WHITEOUT;
            if request.old_path_is_null
                || !request.old_path_terminated
                || request.old_path.is_empty()
                || request.new_path_is_null
                || !request.new_path_terminated
                || request.new_path.is_empty()
                || request.flags & !allowed_flags != 0
                || (request.flags & LINUX_RENAME_EXCHANGE != 0
                    && request.flags & (LINUX_RENAME_NOREPLACE | LINUX_RENAME_WHITEOUT) != 0)
                || (!old_absolute
                    && request
                        .old_base
                        .as_ref()
                        .is_none_or(|base| base.first() != Some(&b'/')))
                || (!new_absolute
                    && request
                        .new_base
                        .as_ref()
                        .is_none_or(|base| base.first() != Some(&b'/')))
                || (!old_absolute
                    && request.old_directory_descriptor != libc::AT_FDCWD
                    && request.old_directory_descriptor < 0)
                || (!new_absolute
                    && request.new_directory_descriptor != libc::AT_FDCWD
                    && request.new_directory_descriptor < 0)
            {
                anyhow::bail!("process capture receipt has an invalid path-rename result");
            }
            Ok(false)
        }
        CapturedPathRenameOutcome::Failed(errno) if errno > 0 => Ok(false),
        CapturedPathRenameOutcome::Terminated => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid path-rename result"),
    }
}

pub(crate) fn validate_captured_path_mode_change_attempt(
    attempt: &CapturedPathModeChangeAttempt,
) -> Result<bool> {
    let request = &attempt.request;
    if request.system_call == CapturedPathModeChangeSystemCall::Fchmod {
        let descriptor = request
            .descriptor
            .context("process capture receipt has no fchmod descriptor")?;
        if request.directory_descriptor != libc::AT_FDCWD
            || !request.path.is_empty()
            || !request.path_is_null
            || request.path_terminated
            || request.base.is_some()
            || request.descriptor_identity.is_some() != request.descriptor_mount_id.is_some()
            || request.descriptor_identity.as_ref().is_some_and(|identity| {
                descriptor < 0
                    || identity.target.is_empty()
                    || identity.target.len() > LINUX_IDENTITY_BYTES_LIMIT
                    || identity.target.contains(&0)
            })
        {
            anyhow::bail!("process capture receipt has an invalid fchmod request");
        }
        return match attempt.outcome {
            CapturedPathModeChangeOutcome::Changed
                if descriptor >= 0 && request.descriptor_identity.is_some() =>
            {
                Ok(false)
            }
            CapturedPathModeChangeOutcome::Failed(errno) if errno > 0 => Ok(false),
            CapturedPathModeChangeOutcome::Terminated => Ok(true),
            _ => anyhow::bail!("process capture receipt has an invalid fchmod result"),
        };
    }
    if request.descriptor.is_some()
        || request.descriptor_identity.is_some()
        || request.descriptor_mount_id.is_some()
    {
        anyhow::bail!("process capture receipt has descriptor data for pathname chmod");
    }
    let absolute = validate_captured_bounded_path_operand(
        &request.path,
        request.path_is_null,
        request.path_terminated,
        request.base.as_deref(),
    )?;
    if request.base.is_some()
        && request.directory_descriptor != libc::AT_FDCWD
        && request.directory_descriptor < 0
    {
        anyhow::bail!("process capture receipt has an invalid path-mode-change base");
    }
    if request.system_call == CapturedPathModeChangeSystemCall::Chmod
        && request.directory_descriptor != libc::AT_FDCWD
    {
        anyhow::bail!("process capture receipt has an invalid chmod request");
    }
    match attempt.outcome {
        CapturedPathModeChangeOutcome::Changed => {
            if request.path_is_null
                || !request.path_terminated
                || request.path.is_empty()
                || (!absolute
                    && request
                        .base
                        .as_ref()
                        .is_none_or(|base| base.first() != Some(&b'/')))
                || (!absolute
                    && request.directory_descriptor != libc::AT_FDCWD
                    && request.directory_descriptor < 0)
            {
                anyhow::bail!("process capture receipt has an invalid path-mode-change result");
            }
            Ok(false)
        }
        CapturedPathModeChangeOutcome::Failed(errno) if errno > 0 => Ok(false),
        CapturedPathModeChangeOutcome::Terminated => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid path-mode-change result"),
    }
}

pub(crate) fn validate_captured_descriptor_timestamp_change_attempt(
    attempt: &CapturedDescriptorTimestampChangeAttempt,
) -> Result<bool> {
    let request = &attempt.request;
    if request.times_is_null != request.times.is_none()
        || request.identity.is_some() != request.mount_id.is_some()
        || (request.descriptor < 0 && request.identity.is_some())
        || request.identity.as_ref().is_some_and(|identity| {
            identity.target.is_empty()
                || identity.target.len() > LINUX_IDENTITY_BYTES_LIMIT
                || identity.target.contains(&0)
        })
    {
        anyhow::bail!("process capture receipt has an invalid timestamp-change request");
    }
    let both_omit = request.times.is_some_and(|times| {
        times[0].nanoseconds == LINUX_UTIME_OMIT && times[1].nanoseconds == LINUX_UTIME_OMIT
    });
    match attempt.outcome {
        CapturedDescriptorTimestampChangeOutcome::Changed => {
            let valid_nanoseconds = request.times.is_none_or(|times| {
                times.iter().all(|time| {
                    (0..=999_999_999).contains(&time.nanoseconds)
                        || time.nanoseconds == LINUX_UTIME_NOW
                        || time.nanoseconds == LINUX_UTIME_OMIT
                })
            });
            if !valid_nanoseconds
                || (!both_omit
                    && (request.descriptor < 0
                        || request.identity.is_none()
                        || request.mount_id.is_none()
                        || request.flags != 0))
            {
                anyhow::bail!("process capture receipt has an invalid timestamp-change result");
            }
            Ok(false)
        }
        CapturedDescriptorTimestampChangeOutcome::Failed(errno) if errno > 0 => Ok(false),
        CapturedDescriptorTimestampChangeOutcome::Terminated => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid timestamp-change result"),
    }
}

pub(crate) fn validate_captured_symbolic_link_creation_attempt(
    attempt: &CapturedSymbolicLinkCreationAttempt,
) -> Result<bool> {
    let request = &attempt.request;
    validate_captured_bounded_path_operand(
        &request.target,
        request.target_is_null,
        request.target_terminated,
        None,
    )?;
    let link_absolute = validate_captured_bounded_path_operand(
        &request.link_path,
        request.link_path_is_null,
        request.link_path_terminated,
        request.base.as_deref(),
    )?;
    if request.base.is_some()
        && request.directory_descriptor != libc::AT_FDCWD
        && request.directory_descriptor < 0
    {
        anyhow::bail!("process capture receipt has an invalid symbolic-link base");
    }
    if request.system_call == CapturedSymbolicLinkCreationSystemCall::Symlink
        && request.directory_descriptor != libc::AT_FDCWD
    {
        anyhow::bail!("process capture receipt has an invalid symlink request");
    }
    match attempt.outcome {
        CapturedSymbolicLinkCreationOutcome::Created => {
            if request.target_is_null
                || !request.target_terminated
                || request.target.is_empty()
                || request.link_path_is_null
                || !request.link_path_terminated
                || request.link_path.is_empty()
                || (!link_absolute
                    && request
                        .base
                        .as_ref()
                        .is_none_or(|base| base.first() != Some(&b'/')))
                || (!link_absolute
                    && request.directory_descriptor != libc::AT_FDCWD
                    && request.directory_descriptor < 0)
            {
                anyhow::bail!("process capture receipt has an invalid symbolic-link result");
            }
            Ok(false)
        }
        CapturedSymbolicLinkCreationOutcome::Failed(errno) if errno > 0 => Ok(false),
        CapturedSymbolicLinkCreationOutcome::Terminated => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid symbolic-link result"),
    }
}

pub(crate) fn validate_captured_filesystem_status_attempt(
    attempt: &CapturedFilesystemStatusAttempt,
) -> Result<bool> {
    let request = &attempt.request;
    let absolute = request.path.first() == Some(&b'/');
    let meaningful_relative_path =
        !request.path_is_null && request.path_terminated && !request.path.is_empty() && !absolute;
    if request.path.contains(&0)
        || request.path.len() > LINUX_PATH_MAX
        || request.base.as_ref().is_some_and(|base| {
            base.is_empty() || base.len() > LINUX_IDENTITY_BYTES_LIMIT || base.contains(&0)
        })
        || request
            .descriptor_identity
            .as_ref()
            .is_some_and(|identity| {
                identity.target.is_empty()
                    || identity.target.len() > LINUX_IDENTITY_BYTES_LIMIT
                    || identity.target.contains(&0)
            })
        || request.descriptor_identity.is_some() != request.descriptor_mount_id.is_some()
    {
        anyhow::bail!("process capture receipt has an invalid filesystem-status request");
    }
    match request.system_call {
        CapturedFilesystemStatusSystemCall::Statfs => {
            if request.descriptor != libc::AT_FDCWD
                || (request.path_is_null && !request.path.is_empty())
                || (request.path_is_null && request.path_terminated)
                || (!request.path_is_null
                    && !request.path_terminated
                    && request.path.len() != LINUX_PATH_MAX)
                || (request.path_terminated && request.path.len() >= LINUX_PATH_MAX)
                || (!meaningful_relative_path && request.base.is_some())
                || request.descriptor_identity.is_some()
                || request.descriptor_mount_id.is_some()
            {
                anyhow::bail!("process capture receipt has an invalid statfs request");
            }
        }
        CapturedFilesystemStatusSystemCall::Fstatfs => {
            if !request.path.is_empty()
                || request.path_is_null
                || request.path_terminated
                || request.base.is_some()
                || (request.descriptor < 0 && request.descriptor_identity.is_some())
            {
                anyhow::bail!("process capture receipt has an invalid fstatfs request");
            }
        }
    }
    match &attempt.outcome {
        CapturedFilesystemStatusOutcome::Observed(_) => {
            if request.output_is_null {
                anyhow::bail!(
                    "process capture receipt has a null successful filesystem-status output"
                );
            }
            match request.system_call {
                CapturedFilesystemStatusSystemCall::Statfs => {
                    if request.path_is_null
                        || !request.path_terminated
                        || request.path.is_empty()
                        || request.path.len() >= LINUX_PATH_MAX
                        || (!absolute && request.base.is_none())
                        || (!absolute
                            && request
                                .base
                                .as_ref()
                                .is_some_and(|base| base.first() != Some(&b'/')))
                    {
                        anyhow::bail!("process capture receipt has an invalid statfs result");
                    }
                }
                CapturedFilesystemStatusSystemCall::Fstatfs => {
                    if request.descriptor < 0
                        || request.descriptor_identity.is_none()
                        || request.descriptor_mount_id.is_none()
                    {
                        anyhow::bail!("process capture receipt has an invalid fstatfs result");
                    }
                }
            }
            Ok(false)
        }
        CapturedFilesystemStatusOutcome::Failed(errno) if *errno > 0 => Ok(false),
        CapturedFilesystemStatusOutcome::Terminated => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid filesystem-status result"),
    }
}

pub(crate) fn validate_captured_read_attempt(attempt: &CapturedReadAttempt) -> Result<bool> {
    validate_read_request(&attempt.request)?;
    match attempt.outcome {
        CapturedReadOutcome::Read {
            bytes,
            digest,
            file_value,
        } if attempt.request.identity.is_some()
            && bytes <= attempt.request.requested_bytes
            && (bytes != 0 || digest == *blake3::hash(&[]).as_bytes())
            && !matches!(file_value, Some(CapturedMappedFileValue::Unwitnessed))
            && file_value.is_none_or(valid_mapped_file_value)
            && match (&attempt.request.identity, file_value) {
                (
                    Some(CapturedDescriptorIdentity {
                        kind: CapturedDescriptorKind::RegularFile,
                        ..
                    }),
                    Some(CapturedMappedFileValue::WholeFile {
                        bytes: file_bytes, ..
                    }),
                ) => attempt.request.offset.is_some_and(|offset| {
                    if bytes == 0 {
                        attempt.request.requested_bytes == 0 || offset >= file_bytes
                    } else {
                        offset
                            .checked_add(bytes)
                            .is_some_and(|end| end <= file_bytes)
                    }
                }),
                (
                    Some(CapturedDescriptorIdentity {
                        kind: CapturedDescriptorKind::RegularFile,
                        ..
                    }),
                    None,
                ) => true,
                (_, None) => true,
                _ => false,
            } =>
        {
            Ok(false)
        }
        CapturedReadOutcome::Failed(errno)
            if errno > 0 && (attempt.request.identity.is_some() || errno == libc::EBADF) =>
        {
            Ok(false)
        }
        CapturedReadOutcome::Terminated if attempt.request.identity.is_some() => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid read result"),
    }
}

pub(crate) fn validate_captured_getdents64_attempt(
    attempt: &CapturedGetdents64Attempt,
) -> Result<bool> {
    validate_getdents64_request(&attempt.request)?;
    match &attempt.outcome {
        CapturedGetdents64Outcome::Enumerated {
            returned_bytes,
            position_after,
            entries,
            directory_value,
        } => {
            let expected_position = entries
                .last()
                .map(|entry| entry.next_position)
                .or(attempt.request.position_before);
            let complete_covers_chunk = match directory_value {
                CapturedDirectoryValue::Unwitnessed => true,
                CapturedDirectoryValue::Complete {
                    entries: complete, ..
                } => u64::try_from(entries.len()).is_ok_and(|observed| observed <= *complete),
            };
            if !attempt
                .request
                .identity
                .as_ref()
                .is_some_and(|identity| identity.kind == CapturedDescriptorKind::Directory)
                || returned_bytes > &attempt.request.requested_bytes
                || *returned_bytes > LINUX_GETDENTS64_RESULT_LIMIT
                || (*returned_bytes == 0) != entries.is_empty()
                || expected_position != Some(*position_after)
                || !complete_covers_chunk
                || !valid_directory_value(directory_value)
            {
                anyhow::bail!("process capture receipt has an invalid getdents64 result");
            }
            validate_directory_entries(*returned_bytes, entries)?;
            Ok(false)
        }
        CapturedGetdents64Outcome::Failed(errno)
            if *errno > 0 && (attempt.request.identity.is_some() || *errno == libc::EBADF) =>
        {
            Ok(false)
        }
        CapturedGetdents64Outcome::Terminated => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid getdents64 result"),
    }
}

fn valid_directory_value(value: &CapturedDirectoryValue) -> bool {
    match value {
        CapturedDirectoryValue::Unwitnessed => true,
        CapturedDirectoryValue::Complete {
            entries,
            digest,
            members,
        } => (entries != &0
            || (digest == blake3::hash(COMPLETE_DIRECTORY_DIGEST_DOMAIN).as_bytes()
                && members.is_empty()))
            && u64::try_from(members.len()).is_ok_and(|member_count| member_count <= *entries)
            && members.windows(2).all(|pair| pair[0] < pair[1])
            && members.iter().all(|member| {
                !member.is_empty()
                    && !member.contains(&0)
                    && !member.contains(&b'/')
                    && member.as_slice() != b"."
                    && member.as_slice() != b".."
            }),
    }
}

pub(crate) fn validate_captured_mmap_attempt(attempt: &CapturedMmapAttempt) -> Result<bool> {
    validate_mmap_request(&attempt.request)?;
    match attempt.outcome {
        CapturedMmapOutcome::Mapped {
            address,
            file_value,
        } => {
            let anonymous = attempt.request.flags & LINUX_MAP_ANONYMOUS != 0;
            let valid_value = if anonymous {
                file_value.is_none()
            } else {
                attempt.request.identity.is_some()
                    && file_value.is_some_and(valid_mapped_file_value)
            };
            if attempt.request.length == 0
                || address.checked_add(attempt.request.length).is_none()
                || !valid_value
            {
                anyhow::bail!("process capture receipt has an invalid mmap result");
            }
            Ok(false)
        }
        CapturedMmapOutcome::Failed(errno) if errno > 0 => Ok(false),
        CapturedMmapOutcome::Failed(_) => {
            anyhow::bail!("process capture receipt has an invalid mmap errno")
        }
        CapturedMmapOutcome::Terminated => Ok(true),
    }
}

pub(crate) fn validate_captured_write_attempt(attempt: &CapturedWriteAttempt) -> Result<bool> {
    validate_write_request(&attempt.request)?;
    match attempt.outcome {
        CapturedWriteOutcome::Wrote { bytes, digest }
            if attempt.request.identity.is_some()
                && bytes <= attempt.request.requested_bytes
                && (bytes != 0 || digest == *blake3::hash(&[]).as_bytes()) =>
        {
            Ok(false)
        }
        CapturedWriteOutcome::Failed(errno)
            if errno > 0 && (attempt.request.identity.is_some() || errno == libc::EBADF) =>
        {
            Ok(false)
        }
        CapturedWriteOutcome::Terminated if attempt.request.identity.is_some() => Ok(true),
        _ => anyhow::bail!("process capture receipt has an invalid write result"),
    }
}

fn validate_exec_file_mappings(mappings: &[CapturedExecFileMapping]) -> Result<()> {
    if mappings.is_empty() || !mappings.iter().any(|mapping| mapping.executable) {
        anyhow::bail!("process capture receipt has no executable file mapping");
    }
    let mut prior_end = None;
    for mapping in mappings {
        if mapping.start >= mapping.end
            || prior_end.is_some_and(|end| end > mapping.start)
            || mapping.identity.target.is_empty()
            || mapping.identity.inode == 0
            || mapping.identity.mount_id == 0
            || !valid_mapped_file_value(mapping.value)
        {
            anyhow::bail!("process capture receipt has an invalid exec file mapping");
        }
        prior_end = Some(mapping.end);
    }
    Ok(())
}

fn valid_mapped_file_value(value: CapturedMappedFileValue) -> bool {
    match value {
        CapturedMappedFileValue::Unwitnessed => true,
        CapturedMappedFileValue::WholeFile {
            bytes,
            digest,
            mode,
        } => {
            mode & libc::S_IFMT == libc::S_IFREG
                && (bytes != 0 || digest == *blake3::hash(&[]).as_bytes())
        }
    }
}

fn validate_exec_request(request: &CapturedExecRequest) -> Result<()> {
    const AT_EMPTY_PATH: u32 = 0x1000;

    let empty_path = request.executable.is_empty() && request.flags & AT_EMPTY_PATH != 0;
    if (!empty_path && request.executable.is_empty())
        || (request.executable.first() == Some(&b'/') && request.directory.is_some())
        || (request.executable.first() != Some(&b'/')
            && request
                .directory
                .as_ref()
                .is_none_or(|directory| directory.first() != Some(&b'/')))
    {
        anyhow::bail!("process capture receipt has an unresolved exec path");
    }
    validate_environment(&request.environment)
}

fn validate_open_request(request: &CapturedOpenRequest) -> Result<()> {
    if request.path.is_empty()
        || (request.path.first() == Some(&b'/') && request.directory.is_some())
        || (request.path.first() != Some(&b'/')
            && request
                .directory
                .as_ref()
                .is_none_or(|directory| directory.first() != Some(&b'/')))
        || (request.system_call != CapturedOpenSystemCall::OpenAt2 && request.resolve != 0)
        || (request.system_call != CapturedOpenSystemCall::OpenAt2
            && request.flags > u64::from(u32::MAX))
        || (request.system_call == CapturedOpenSystemCall::Creat
            && request.flags != (LINUX_O_CREAT | LINUX_O_WRONLY | LINUX_O_TRUNC))
        || (request.system_call != CapturedOpenSystemCall::Creat
            && !open_mode_is_used(request.flags)
            && request.mode != 0)
    {
        anyhow::bail!("process capture receipt has an invalid open request");
    }
    Ok(())
}

fn open_mode_is_used(flags: u64) -> bool {
    flags & (LINUX_O_CREAT | LINUX_O_TMPFILE) != 0
}

fn validate_read_request(request: &CapturedReadRequest) -> Result<()> {
    if request.identity.is_some() != request.mount_id.is_some() {
        anyhow::bail!("process capture receipt has an unpaired read mount identity");
    }
    if let Some(identity) = &request.identity {
        if request.descriptor < 0 || identity.target.is_empty() {
            anyhow::bail!("process capture receipt has an invalid read identity");
        }
    }
    let vectored = matches!(
        request.system_call,
        CapturedReadSystemCall::Readv
            | CapturedReadSystemCall::Preadv
            | CapturedReadSystemCall::Preadv2
    );
    if (!vectored && request.buffer_lengths.len() != 1)
        || (vectored && request.buffer_lengths.len() > LINUX_UIO_MAXIOV)
        || request
            .buffer_lengths
            .iter()
            .try_fold(0u64, |total, length| total.checked_add(*length))
            != Some(request.requested_bytes)
        || (request.system_call != CapturedReadSystemCall::Preadv2 && request.flags != 0)
    {
        anyhow::bail!("process capture receipt has invalid read buffers");
    }
    let descriptor_offset = match &request.identity {
        Some(identity) if identity.kind == CapturedDescriptorKind::RegularFile => {
            request.offset.is_some()
        }
        _ => request.offset.is_none(),
    };
    let valid_offset = match request.system_call {
        CapturedReadSystemCall::Read | CapturedReadSystemCall::Readv => {
            request.uses_descriptor_offset && descriptor_offset
        }
        CapturedReadSystemCall::Pread64 | CapturedReadSystemCall::Preadv => {
            !request.uses_descriptor_offset && request.offset.is_some()
        }
        CapturedReadSystemCall::Preadv2 if request.uses_descriptor_offset => descriptor_offset,
        CapturedReadSystemCall::Preadv2 => request.offset.is_some(),
    };
    if !valid_offset {
        anyhow::bail!("process capture receipt has an invalid read offset");
    }
    Ok(())
}

fn validate_getdents64_request(request: &CapturedGetdents64Request) -> Result<()> {
    if request.identity.is_some() != request.mount_id.is_some() {
        anyhow::bail!("process capture receipt has an unpaired getdents64 mount identity");
    }
    if let Some(identity) = &request.identity {
        if request.descriptor < 0 || identity.target.is_empty() {
            anyhow::bail!("process capture receipt has an invalid getdents64 identity");
        }
    }
    let has_directory_position = request
        .identity
        .as_ref()
        .is_some_and(|identity| identity.kind == CapturedDescriptorKind::Directory);
    if request.position_before.is_some() != has_directory_position {
        anyhow::bail!("process capture receipt has an invalid getdents64 position");
    }
    Ok(())
}

fn validate_mmap_request(request: &CapturedMmapRequest) -> Result<()> {
    let anonymous = request.flags & LINUX_MAP_ANONYMOUS != 0;
    if request.identity.is_some() != request.mount_id.is_some() {
        anyhow::bail!("process capture receipt has an unpaired mmap mount identity");
    }
    if anonymous && request.identity.is_some() {
        anyhow::bail!("process capture receipt resolves an anonymous mmap descriptor");
    }
    if let Some(identity) = &request.identity {
        if request.descriptor < 0 || identity.target.is_empty() {
            anyhow::bail!("process capture receipt has an invalid mmap identity");
        }
    }
    Ok(())
}

fn validate_write_request(request: &CapturedWriteRequest) -> Result<()> {
    if request.identity.is_some() != request.mount_id.is_some() {
        anyhow::bail!("process capture receipt has an unpaired write mount identity");
    }
    if let Some(identity) = &request.identity {
        if request.descriptor < 0 || identity.target.is_empty() {
            anyhow::bail!("process capture receipt has an invalid write identity");
        }
    } else if request.append {
        anyhow::bail!("process capture receipt gives an invalid descriptor append state");
    }
    let descriptor_offset = match &request.identity {
        Some(identity) if identity.kind == CapturedDescriptorKind::RegularFile => {
            request.offset.is_some()
        }
        _ => request.offset.is_none(),
    };
    let valid_offset = match request.system_call {
        CapturedWriteSystemCall::Write => request.uses_descriptor_offset && descriptor_offset,
        CapturedWriteSystemCall::Pwrite64 => {
            !request.uses_descriptor_offset && request.offset.is_some()
        }
    };
    if !valid_offset {
        anyhow::bail!("process capture receipt has an invalid write offset");
    }
    Ok(())
}

fn validate_directory_entries(
    returned_bytes: u32,
    entries: &[CapturedDirectoryEntry],
) -> Result<()> {
    let mut framed_bytes = 0u32;
    for entry in entries {
        let minimum_length = LINUX_DIRENT64_HEADER_SIZE
            .checked_add(entry.name.len())
            .and_then(|length| length.checked_add(1))
            .ok_or_else(|| anyhow::anyhow!("directory entry length overflowed"))?;
        if entry.record_length < LINUX_DIRENT64_MINIMUM_RECORD_SIZE
            || entry.record_length % LINUX_DIRENT64_ALIGNMENT != 0
            || usize::from(entry.record_length) < minimum_length
            || entry.name.is_empty()
            || entry.name.len() >= LINUX_PATH_MAX
            || entry.name.contains(&0)
            || entry.name.contains(&b'/')
        {
            anyhow::bail!("process capture receipt has an invalid directory entry");
        }
        framed_bytes = framed_bytes
            .checked_add(u32::from(entry.record_length))
            .ok_or_else(|| anyhow::anyhow!("directory entry byte count overflowed"))?;
    }
    if framed_bytes != returned_bytes {
        anyhow::bail!("process capture receipt has an invalid directory byte count");
    }
    Ok(())
}

fn validate_environment(environment: &[Vec<u8>]) -> Result<()> {
    let mut names = BTreeSet::new();
    for entry in environment {
        let Some(separator) = entry.iter().position(|byte| *byte == b'=') else {
            anyhow::bail!("process capture environment entry has no name");
        };
        let name = &entry[..separator];
        if name.is_empty() || !names.insert(name) {
            anyhow::bail!("process capture environment repeats or omits a name");
        }
    }
    Ok(())
}

fn checked_census_sum(
    processes: &[CapturedProcess],
    value: impl Fn(&CapturedProcess) -> u64,
) -> Result<u64> {
    processes.iter().try_fold(0u64, |total, process| {
        total
            .checked_add(value(process))
            .ok_or_else(|| anyhow::anyhow!("process syscall census overflowed"))
    })
}

/// Capture a command and every descendant process on supported platforms.
#[cfg(not(target_os = "linux"))]
pub fn capture_process_tree(_command: &mut std::process::Command) -> Result<ProcessCaptureReceipt> {
    anyhow::bail!("process capture is unsupported on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_size_targets_admit_exact_isolation_mirrors() {
        assert!(regular_file_value_has_synthetic_size(
            b"/sys/devices/system/cpu/online"
        ));
        assert!(regular_file_value_has_synthetic_size(
            b"/tmp/rush-local-isolation-964982-1789551301260383473-1/sys/devices/system/cpu/online"
        ));
        assert!(regular_file_value_has_synthetic_size(
            b"/tmp/rush-local-isolation-1/sys/kernel/mm/transparent_hugepage/enabled"
        ));
        assert!(!regular_file_value_has_synthetic_size(
            b"/tmp/not-rush-local-isolation-1/sys/devices/system/cpu/online"
        ));
        assert!(!regular_file_value_has_synthetic_size(
            b"/tmp/rush-local-isolation-name/sys/devices/system/cpu/online"
        ));
        assert!(!regular_file_value_has_synthetic_size(
            b"/tmp/rush-local-isolation-/sys/devices/system/cpu/online"
        ));
        assert!(!regular_file_value_has_synthetic_size(
            b"/tmp/rush-local-isolation-1/sys/devices/system/cpu/possible"
        ));
    }

    fn fixture_exec_mapping() -> CapturedExecFileMapping {
        CapturedExecFileMapping {
            start: 0x1000,
            end: 0x2000,
            file_offset: 0,
            readable: true,
            writable: false,
            executable: true,
            shared: false,
            identity: CapturedMappedFileIdentity {
                target: b"/fixture/executable".to_vec(),
                device: 1,
                inode: 1,
                mount_id: 1,
            },
            value: CapturedMappedFileValue::Unwitnessed,
        }
    }

    fn fixture_invocation(executable: &[u8]) -> CapturedInvocation {
        CapturedInvocation {
            executable: executable.to_vec(),
            argv: vec![executable.to_vec()],
            environment: Vec::new(),
            working_directory: b"/".to_vec(),
            signal_state: CapturedExecSignalState::default(),
            file_creation_mask: 0o022,
            credentials: CapturedCredentialState::default(),
            file_mappings: vec![fixture_exec_mapping()],
        }
    }

    #[test]
    fn validates_whole_file_witness_modes() {
        let empty = *blake3::hash(&[]).as_bytes();
        assert!(valid_mapped_file_value(CapturedMappedFileValue::WholeFile {
            bytes: 0,
            digest: empty,
            mode: libc::S_IFREG | 0o755,
        }));
        assert!(!valid_mapped_file_value(
            CapturedMappedFileValue::WholeFile {
                bytes: 0,
                digest: empty,
                mode: libc::S_IFDIR | 0o755,
            }
        ));
        assert!(!valid_mapped_file_value(
            CapturedMappedFileValue::WholeFile {
                bytes: 0,
                digest: [7; 32],
                mode: libc::S_IFREG | 0o755,
            }
        ));
    }

    #[test]
    fn validates_exec_boundary_signal_state() {
        let mut invocation = fixture_invocation(b"/bin/true");
        invocation.signal_state.blocked = 1_u64 << (libc::SIGUSR1 - 1);
        invocation.signal_state.ignored = 1_u64 << (libc::SIGUSR2 - 1);
        validate_captured_invocation(&invocation).unwrap();

        invocation.signal_state.caught = 1_u64 << (libc::SIGTERM - 1);
        assert!(validate_captured_invocation(&invocation)
            .unwrap_err()
            .to_string()
            .contains("retains a caught signal"));
        invocation.signal_state.caught = 0;
        invocation.signal_state.pending = 1_u64 << (libc::SIGUSR1 - 1);
        invocation.signal_state.shared_pending = 1_u64 << (libc::SIGCHLD - 1);
        validate_captured_invocation(&invocation).unwrap();
        invocation.signal_state.pending = 0;
        invocation.signal_state.shared_pending = 0;
        invocation.signal_state.blocked = 1_u64 << (libc::SIGKILL - 1);
        assert!(validate_captured_invocation(&invocation)
            .unwrap_err()
            .to_string()
            .contains("masks an unmaskable signal"));

        invocation.signal_state.blocked = 0;
        invocation.file_creation_mask = 0o1000;
        assert!(validate_captured_invocation(&invocation)
            .unwrap_err()
            .to_string()
            .contains("incomplete exec boundary"));
    }

    #[test]
    fn validates_signal_delivery_shapes() {
        let mut delivery = CapturedSignalDelivery {
            signal: libc::SIGCHLD as u32,
            code: libc::CLD_EXITED,
            errno: 0,
            source: CapturedSignalSource::Kernel,
            child_status: Some(0),
            syscall_entries: 0,
            active_syscall: None,
            disposition: CapturedSignalDisposition::Caught,
        };
        validate_captured_signal_delivery(&delivery).unwrap();

        delivery.signal = 0;
        assert!(validate_captured_signal_delivery(&delivery).is_err());
        delivery.signal = libc::SIGTERM as u32;
        assert!(validate_captured_signal_delivery(&delivery).is_err());
        delivery.child_status = None;
        validate_captured_signal_delivery(&delivery).unwrap();

        delivery.signal = libc::SIGKILL as u32;
        assert!(validate_captured_signal_delivery(&delivery).is_err());
        delivery.disposition = CapturedSignalDisposition::Default;
        validate_captured_signal_delivery(&delivery).unwrap();
        delivery.signal = libc::SIGTERM as u32;

        delivery.source = CapturedSignalSource::TracedProcess { process: 0 };
        assert!(validate_captured_signal_delivery(&delivery).is_err());
        delivery.source = CapturedSignalSource::ExternalProcess {
            process: -1,
            user: 0,
        };
        assert!(validate_captured_signal_delivery(&delivery).is_err());
    }

    #[test]
    fn validates_umask_attempt_shapes() {
        let mut attempt = CapturedUmaskAttempt {
            syscall_index: 0,
            requested: 0o077,
            outcome: CapturedUmaskOutcome::Changed(0o022),
        };
        assert!(!validate_captured_umask_attempt(&attempt).unwrap());

        attempt.outcome = CapturedUmaskOutcome::Changed(0o1000);
        assert!(validate_captured_umask_attempt(&attempt).is_err());
        attempt.outcome = CapturedUmaskOutcome::Terminated;
        assert!(validate_captured_umask_attempt(&attempt).unwrap());
    }

    #[test]
    fn validates_credential_attempt_shapes() {
        let mut attempt = CapturedCredentialAttempt {
            syscall_index: 0,
            request: CapturedCredentialRequest::Getuid,
            outcome: CapturedCredentialOutcome::Identity(1000),
        };
        assert!(!validate_captured_credential_attempt(&attempt).unwrap());

        attempt.outcome = CapturedCredentialOutcome::State(CapturedCredentialState::default());
        assert!(validate_captured_credential_attempt(&attempt).is_err());
        attempt.request = CapturedCredentialRequest::Getgroups {
            capacity: 0,
            output_is_null: true,
        };
        attempt.outcome = CapturedCredentialOutcome::SupplementaryGroupCount(2);
        assert!(!validate_captured_credential_attempt(&attempt).unwrap());
        attempt.request = CapturedCredentialRequest::Getgroups {
            capacity: 2,
            output_is_null: false,
        };
        attempt.outcome = CapturedCredentialOutcome::SupplementaryGroups(vec![7, 9]);
        assert!(!validate_captured_credential_attempt(&attempt).unwrap());
        attempt.outcome = CapturedCredentialOutcome::SupplementaryGroups(vec![7, 7]);
        assert!(!validate_captured_credential_attempt(&attempt).unwrap());
        attempt.outcome = CapturedCredentialOutcome::SupplementaryGroups(vec![9, 7]);
        assert!(validate_captured_credential_attempt(&attempt).is_err());
        attempt.outcome = CapturedCredentialOutcome::SupplementaryGroups(vec![u32::MAX]);
        assert!(validate_captured_credential_attempt(&attempt).is_err());
        attempt.request = CapturedCredentialRequest::Getgroups {
            capacity: 2,
            output_is_null: true,
        };
        attempt.outcome = CapturedCredentialOutcome::SupplementaryGroups(vec![7]);
        assert!(validate_captured_credential_attempt(&attempt).is_err());
        attempt.request = CapturedCredentialRequest::Getgroups {
            capacity: 1,
            output_is_null: false,
        };
        attempt.outcome = CapturedCredentialOutcome::SupplementaryGroups(vec![7, 9]);
        assert!(validate_captured_credential_attempt(&attempt).is_err());
        attempt.request = CapturedCredentialRequest::Getgroups {
            capacity: 65_537,
            output_is_null: false,
        };
        attempt.outcome = CapturedCredentialOutcome::Failed(libc::EINVAL);
        assert!(validate_captured_credential_attempt(&attempt).is_err());
        attempt.outcome = CapturedCredentialOutcome::Terminated;
        assert!(validate_captured_credential_attempt(&attempt).is_err());
        attempt.request = CapturedCredentialRequest::CapabilityBoundingSetRead {
            capability: 40,
        };
        attempt.outcome = CapturedCredentialOutcome::CapabilityBoundingSetMembership(true);
        assert!(!validate_captured_credential_attempt(&attempt).unwrap());
        attempt.outcome = CapturedCredentialOutcome::Failed(libc::EINVAL);
        assert!(validate_captured_credential_attempt(&attempt).is_err());
        attempt.request = CapturedCredentialRequest::CapabilityBoundingSetRead {
            capability: 48,
        };
        assert!(!validate_captured_credential_attempt(&attempt).unwrap());
        attempt.outcome = CapturedCredentialOutcome::Failed(libc::EPERM);
        assert!(validate_captured_credential_attempt(&attempt).is_err());
        attempt.request = CapturedCredentialRequest::CapabilityBoundingSetRead {
            capability: 39,
        };
        assert!(validate_captured_credential_attempt(&attempt).is_err());
        attempt.outcome = CapturedCredentialOutcome::Terminated;
        assert!(validate_captured_credential_attempt(&attempt).is_err());
        attempt.request = CapturedCredentialRequest::Setresuid {
            real: None,
            effective: Some(1000),
            saved: None,
        };
        attempt.outcome = CapturedCredentialOutcome::State(CapturedCredentialState {
            effective_user: 1000,
            filesystem_user: 1000,
            supplementary_groups: vec![7, 7, 9],
            ..CapturedCredentialState::default()
        });
        assert!(!validate_captured_credential_attempt(&attempt).unwrap());
        attempt.outcome = CapturedCredentialOutcome::State(CapturedCredentialState {
            effective_user: 1000,
            filesystem_user: 1000,
            supplementary_groups: vec![9, 7],
            ..CapturedCredentialState::default()
        });
        assert!(validate_captured_credential_attempt(&attempt).is_err());
        attempt.outcome = CapturedCredentialOutcome::Failed(0);
        assert!(validate_captured_credential_attempt(&attempt).is_err());
        attempt.outcome = CapturedCredentialOutcome::Terminated;
        assert!(validate_captured_credential_attempt(&attempt).unwrap());
    }

    #[test]
    fn validates_runtime_observation_shapes_without_authorizing_replay() {
        let usage = CapturedResourceUsage {
            user_seconds: 1,
            user_microseconds: 2,
            system_seconds: 3,
            system_microseconds: 4,
            maximum_resident_set_size: 5,
            integral_shared_memory_size: 0,
            integral_unshared_data_size: 0,
            integral_unshared_stack_size: 0,
            minor_page_faults: 6,
            major_page_faults: 0,
            swaps: 0,
            block_inputs: 0,
            block_outputs: 0,
            messages_sent: 0,
            messages_received: 0,
            signals_received: 0,
            voluntary_context_switches: 7,
            involuntary_context_switches: 8,
        };
        let mut attempt = CapturedRuntimeObservationAttempt {
            syscall_index: 0,
            request: CapturedRuntimeObservationRequest::CallingProcessResourceUsage {
                output_is_null: false,
            },
            outcome: CapturedRuntimeObservationOutcome::ResourceUsage(usage),
        };
        assert!(!validate_captured_runtime_observation_attempt(&attempt).unwrap());

        attempt.request = CapturedRuntimeObservationRequest::SystemInformation {
            output_is_null: false,
        };
        assert!(validate_captured_runtime_observation_attempt(&attempt).is_err());

        let identity = CapturedSystemIdentity {
            system_name: b"Linux".to_vec(),
            node_name: b"fixture".to_vec(),
            release: b"1.2.3".to_vec(),
            version: b"fixture version".to_vec(),
            machine: b"x86_64".to_vec(),
            domain_name: Vec::new(),
        };
        attempt.request = CapturedRuntimeObservationRequest::SystemIdentity {
            output_is_null: false,
        };
        attempt.outcome = CapturedRuntimeObservationOutcome::SystemIdentity(identity.clone());
        assert!(!validate_captured_runtime_observation_attempt(&attempt).unwrap());
        let mut invalid_identity = identity.clone();
        invalid_identity.release.push(0);
        attempt.outcome =
            CapturedRuntimeObservationOutcome::SystemIdentity(invalid_identity);
        assert!(validate_captured_runtime_observation_attempt(&attempt).is_err());
        let mut oversized_identity = identity.clone();
        oversized_identity.node_name = vec![b'x'; 65];
        attempt.outcome =
            CapturedRuntimeObservationOutcome::SystemIdentity(oversized_identity);
        assert!(validate_captured_runtime_observation_attempt(&attempt).is_err());
        attempt.request = CapturedRuntimeObservationRequest::SystemIdentity {
            output_is_null: true,
        };
        attempt.outcome = CapturedRuntimeObservationOutcome::SystemIdentity(identity);
        assert!(validate_captured_runtime_observation_attempt(&attempt).is_err());

        attempt.request = CapturedRuntimeObservationRequest::CallingThreadAffinity {
            capacity: 128,
            output_is_null: false,
        };
        attempt.outcome = CapturedRuntimeObservationOutcome::CpuAffinity(vec![1; 16]);
        assert!(!validate_captured_runtime_observation_attempt(&attempt).unwrap());
        attempt.outcome = CapturedRuntimeObservationOutcome::CpuAffinity(vec![0; 16]);
        assert!(validate_captured_runtime_observation_attempt(&attempt).is_err());
        attempt.outcome = CapturedRuntimeObservationOutcome::CpuAffinity(vec![1; 15]);
        assert!(validate_captured_runtime_observation_attempt(&attempt).is_err());
        attempt.outcome = CapturedRuntimeObservationOutcome::CpuAffinity(vec![1; 129]);
        assert!(validate_captured_runtime_observation_attempt(&attempt).is_err());
        attempt.request = CapturedRuntimeObservationRequest::CallingThreadAffinity {
            capacity: 128,
            output_is_null: true,
        };
        attempt.outcome = CapturedRuntimeObservationOutcome::CpuAffinity(vec![1; 16]);
        assert!(validate_captured_runtime_observation_attempt(&attempt).is_err());
        attempt.outcome = CapturedRuntimeObservationOutcome::Failed(0);
        assert!(validate_captured_runtime_observation_attempt(&attempt).is_err());
        attempt.request = CapturedRuntimeObservationRequest::CallingThreadAffinity {
            capacity: 32,
            output_is_null: false,
        };
        attempt.outcome = CapturedRuntimeObservationOutcome::CpuAffinity(vec![1; 8]);
        assert!(!validate_captured_runtime_observation_attempt(&attempt).unwrap());
        attempt.request = CapturedRuntimeObservationRequest::CallingThreadAffinity {
            capacity: 7,
            output_is_null: false,
        };
        attempt.outcome = CapturedRuntimeObservationOutcome::CpuAffinity(vec![1; 8]);
        assert!(validate_captured_runtime_observation_attempt(&attempt).is_err());
        attempt.outcome = CapturedRuntimeObservationOutcome::Failed(libc::EFAULT);
        assert!(validate_captured_runtime_observation_attempt(&attempt).is_err());
        attempt.outcome = CapturedRuntimeObservationOutcome::Terminated;
        assert!(validate_captured_runtime_observation_attempt(&attempt).is_err());
        attempt.request = CapturedRuntimeObservationRequest::CallingThreadAffinity {
            capacity: 128,
            output_is_null: true,
        };
        attempt.outcome = CapturedRuntimeObservationOutcome::Failed(libc::EFAULT);
        assert!(!validate_captured_runtime_observation_attempt(&attempt).unwrap());

        let regular_identity = CapturedDescriptorIdentity {
            target: b"output".to_vec(),
            device: 1,
            inode: 2,
            kind: CapturedDescriptorKind::RegularFile,
        };
        let poll_request = CapturedRuntimeObservationRequest::DescriptorReadiness {
            descriptors: vec![
                CapturedPollDescriptor {
                    descriptor: 0,
                    identity: Some(CapturedDescriptorIdentity {
                        target: b"pipe:[1]".to_vec(),
                        device: 3,
                        inode: 4,
                        kind: CapturedDescriptorKind::Fifo,
                    }),
                    mount_id: Some(5),
                    requested_events: LINUX_POLLIN | LINUX_POLLOUT,
                },
                CapturedPollDescriptor {
                    descriptor: 1,
                    identity: Some(regular_identity.clone()),
                    mount_id: Some(6),
                    requested_events: LINUX_POLLIN | LINUX_POLLOUT,
                },
                CapturedPollDescriptor {
                    descriptor: 2,
                    identity: Some(regular_identity),
                    mount_id: Some(6),
                    requested_events: LINUX_POLLIN | LINUX_POLLOUT,
                },
                CapturedPollDescriptor {
                    descriptor: 3,
                    identity: None,
                    mount_id: None,
                    requested_events: LINUX_POLLIN | LINUX_POLLOUT,
                },
            ],
            timeout_milliseconds: 0,
        };
        attempt.request = poll_request.clone();
        attempt.outcome = CapturedRuntimeObservationOutcome::DescriptorReadiness {
            ready: 4,
            returned_events: vec![
                LINUX_POLLIN | LINUX_POLLHUP,
                LINUX_POLLIN | LINUX_POLLOUT,
                LINUX_POLLIN | LINUX_POLLOUT,
                LINUX_POLLNVAL,
            ],
        };
        assert!(!validate_captured_runtime_observation_attempt(&attempt).unwrap());
        let CapturedRuntimeObservationRequest::DescriptorReadiness { descriptors, .. } =
            &mut attempt.request
        else {
            unreachable!();
        };
        for descriptor in &mut descriptors[1..=2] {
            descriptor.identity = Some(CapturedDescriptorIdentity {
                target: b"/dev/null".to_vec(),
                device: 5,
                inode: 5,
                kind: CapturedDescriptorKind::CharacterDevice,
            });
            descriptor.mount_id = Some(7);
        }
        assert!(!validate_captured_runtime_observation_attempt(&attempt).unwrap());
        {
            let CapturedRuntimeObservationRequest::DescriptorReadiness { descriptors, .. } =
                &mut attempt.request
            else {
                unreachable!();
            };
            descriptors[1].identity.as_mut().unwrap().target = b"/dev/zero".to_vec();
        }
        assert!(validate_captured_runtime_observation_attempt(&attempt).is_err());
        let CapturedRuntimeObservationRequest::DescriptorReadiness { descriptors, .. } =
            &mut attempt.request
        else {
            unreachable!();
        };
        descriptors[1].identity.as_mut().unwrap().target = b"/dev/null".to_vec();
        descriptors[3].identity = Some(CapturedDescriptorIdentity {
            target: b"unexpected".to_vec(),
            device: 1,
            inode: 9,
            kind: CapturedDescriptorKind::RegularFile,
        });
        descriptors[3].mount_id = Some(8);
        assert!(validate_captured_runtime_observation_attempt(&attempt).is_err());
        let CapturedRuntimeObservationRequest::DescriptorReadiness { descriptors, .. } =
            &mut attempt.request
        else {
            unreachable!();
        };
        descriptors[3].identity = None;
        descriptors[3].mount_id = None;
        attempt.outcome = CapturedRuntimeObservationOutcome::DescriptorReadiness {
            ready: 3,
            returned_events: vec![
                LINUX_POLLIN | LINUX_POLLHUP,
                LINUX_POLLIN | LINUX_POLLOUT,
                LINUX_POLLIN | LINUX_POLLOUT,
                LINUX_POLLNVAL,
            ],
        };
        assert!(validate_captured_runtime_observation_attempt(&attempt).is_err());
        attempt.outcome = CapturedRuntimeObservationOutcome::Failed(libc::EFAULT);
        assert!(!validate_captured_runtime_observation_attempt(&attempt).unwrap());
        let CapturedRuntimeObservationRequest::DescriptorReadiness { descriptors, .. } =
            &mut attempt.request
        else {
            unreachable!();
        };
        descriptors.pop();
        attempt.outcome = CapturedRuntimeObservationOutcome::Terminated;
        assert!(validate_captured_runtime_observation_attempt(&attempt).is_err());

        attempt.request = CapturedRuntimeObservationRequest::DescriptorReadiness {
            descriptors: vec![
                CapturedPollDescriptor {
                    descriptor: 0,
                    identity: Some(CapturedDescriptorIdentity {
                        target: b"pipe:[9]".to_vec(),
                        device: 3,
                        inode: 9,
                        kind: CapturedDescriptorKind::Fifo,
                    }),
                    mount_id: Some(5),
                    requested_events: 0,
                },
                CapturedPollDescriptor {
                    descriptor: 1,
                    identity: Some(CapturedDescriptorIdentity {
                        target: b"/dev/null".to_vec(),
                        device: 5,
                        inode: 5,
                        kind: CapturedDescriptorKind::CharacterDevice,
                    }),
                    mount_id: Some(7),
                    requested_events: 0,
                },
                CapturedPollDescriptor {
                    descriptor: 2,
                    identity: Some(CapturedDescriptorIdentity {
                        target: b"/dev/null".to_vec(),
                        device: 5,
                        inode: 5,
                        kind: CapturedDescriptorKind::CharacterDevice,
                    }),
                    mount_id: Some(7),
                    requested_events: 0,
                },
            ],
            timeout_milliseconds: 0,
        };
        attempt.outcome = CapturedRuntimeObservationOutcome::DescriptorReadiness {
            ready: 0,
            returned_events: vec![0, 0, 0],
        };
        assert!(!validate_captured_runtime_observation_attempt(&attempt).unwrap());
        attempt.outcome = CapturedRuntimeObservationOutcome::DescriptorReadiness {
            ready: 1,
            returned_events: vec![LINUX_POLLHUP, 0, 0],
        };
        assert!(!validate_captured_runtime_observation_attempt(&attempt).unwrap());
        let CapturedRuntimeObservationRequest::DescriptorReadiness { descriptors, .. } =
            &mut attempt.request
        else {
            unreachable!();
        };
        descriptors[0].identity.as_mut().unwrap().kind = CapturedDescriptorKind::RegularFile;
        assert!(validate_captured_runtime_observation_attempt(&attempt).is_err());

        attempt.request = CapturedRuntimeObservationRequest::SystemIdentity {
            output_is_null: true,
        };
        attempt.outcome = CapturedRuntimeObservationOutcome::Failed(libc::EFAULT);
        assert!(!validate_captured_runtime_observation_attempt(&attempt).unwrap());
        attempt.outcome = CapturedRuntimeObservationOutcome::Failed(0);
        assert!(validate_captured_runtime_observation_attempt(&attempt).is_err());
        attempt.outcome = CapturedRuntimeObservationOutcome::Terminated;
        assert!(validate_captured_runtime_observation_attempt(&attempt).unwrap());

        attempt.request = CapturedRuntimeObservationRequest::CallingProcessResourceUsage {
            output_is_null: true,
        };
        attempt.outcome = CapturedRuntimeObservationOutcome::ResourceUsage(usage);
        assert!(validate_captured_runtime_observation_attempt(&attempt).is_err());
        let mut invalid_usage = usage;
        invalid_usage.user_microseconds = 1_000_000;
        attempt.request = CapturedRuntimeObservationRequest::CallingProcessResourceUsage {
            output_is_null: false,
        };
        attempt.outcome = CapturedRuntimeObservationOutcome::ResourceUsage(invalid_usage);
        assert!(validate_captured_runtime_observation_attempt(&attempt).is_err());
    }

    #[test]
    fn validates_measured_xargs_pipe_jobserver_poll_shape() {
        use CapturedRuntimeObservationOutcome::DescriptorReadiness as Readiness;
        let accepts = |descriptors: &[CapturedPollDescriptor], timeout, ready, events: &[i16]| {
            validate_captured_runtime_observation_attempt(&CapturedRuntimeObservationAttempt {
                syscall_index: 1,
                request: CapturedRuntimeObservationRequest::DescriptorReadiness {
                    descriptors: descriptors.to_vec(),
                    timeout_milliseconds: timeout,
                },
                outcome: Readiness {
                    ready,
                    returned_events: events.to_vec(),
                },
            })
            .is_ok()
        };

        let identity = |target: &[u8], inode, kind| CapturedDescriptorIdentity {
            target: target.to_vec(),
            device: 1,
            inode,
            kind,
        };
        let descriptors = vec![
            CapturedPollDescriptor {
                descriptor: 0,
                identity: Some(identity(b"pipe:[11]", 11, CapturedDescriptorKind::Fifo)),
                mount_id: Some(2),
                requested_events: LINUX_POLLIN | LINUX_POLLOUT,
            },
            CapturedPollDescriptor {
                descriptor: 1,
                identity: Some(identity(b"output", 12, CapturedDescriptorKind::RegularFile)),
                mount_id: Some(3),
                requested_events: LINUX_POLLIN | LINUX_POLLOUT,
            },
            CapturedPollDescriptor {
                descriptor: 2,
                identity: Some(identity(b"errors", 13, CapturedDescriptorKind::RegularFile)),
                mount_id: Some(3),
                requested_events: LINUX_POLLIN | LINUX_POLLOUT,
            },
            CapturedPollDescriptor {
                descriptor: 3,
                identity: Some(identity(b"pipe:[14]", 14, CapturedDescriptorKind::Fifo)),
                mount_id: Some(2),
                requested_events: LINUX_POLLIN | LINUX_POLLOUT,
            },
            CapturedPollDescriptor {
                descriptor: 4,
                identity: Some(identity(b"pipe:[14]", 14, CapturedDescriptorKind::Fifo)),
                mount_id: Some(2),
                requested_events: LINUX_POLLIN | LINUX_POLLOUT,
            },
            CapturedPollDescriptor {
                descriptor: 5,
                identity: None,
                mount_id: None,
                requested_events: LINUX_POLLIN | LINUX_POLLOUT,
            },
        ];
        let events = [
            LINUX_POLLIN | LINUX_POLLHUP,
            LINUX_POLLIN | LINUX_POLLOUT,
            LINUX_POLLIN | LINUX_POLLOUT,
            LINUX_POLLIN,
            LINUX_POLLOUT,
            LINUX_POLLNVAL,
        ];
        assert!(accepts(&descriptors, 0, 6, &events));
        assert!(!accepts(&descriptors, 1, 6, &events));
        assert!(!accepts(&descriptors, 0, 5, &events));
        assert!(!accepts(&descriptors[..5], 0, 5, &events[..5]));
        assert!(!accepts(&descriptors, 0, 6, &events[..5]));
        for index in 0..6 {
            let mut changed = descriptors.clone();
            changed[index].descriptor += 1;
            assert!(!accepts(&changed, 0, 6, &events));
            let mut changed = descriptors.clone();
            changed[index].requested_events = LINUX_POLLIN;
            assert!(!accepts(&changed, 0, 6, &events));
            let mut changed_events = events;
            changed_events[index] = 0;
            assert!(!accepts(&descriptors, 0, 6, &changed_events));
        }
        let mut changed = descriptors.clone();
        changed[4].identity.as_mut().unwrap().inode += 1;
        assert!(!accepts(&changed, 0, 6, &events));
        let mut changed = descriptors.clone();
        changed[4].mount_id = Some(9);
        assert!(!accepts(&changed, 0, 6, &events));
        let mut changed = descriptors.clone();
        for descriptor in &mut changed[3..5] {
            descriptor.identity.as_mut().unwrap().target = b"named-fifo".to_vec();
        }
        assert!(!accepts(&changed, 0, 6, &events));
        let mut changed = descriptors.clone();
        for descriptor in &mut changed[3..5] {
            descriptor.identity.as_mut().unwrap().kind = CapturedDescriptorKind::RegularFile;
        }
        assert!(!accepts(&changed, 0, 6, &events));
        let mut changed = descriptors.clone();
        changed[0].identity = changed[3].identity.clone();
        assert!(!accepts(&changed, 0, 6, &events));
        let mut changed = descriptors.clone();
        changed[5].identity = changed[3].identity.clone();
        changed[5].mount_id = Some(2);
        assert!(!accepts(&changed, 0, 6, &events));
        let mut reversed = events;
        reversed.swap(3, 4);
        assert!(!accepts(&descriptors, 0, 6, &reversed));
    }

    #[test]
    fn validates_event_poll_creation_shapes() {
        let mut attempt = CapturedEpollCreateAttempt {
            syscall_index: 7,
            request: CapturedEpollCreateRequest {
                flags: libc::EPOLL_CLOEXEC as u32,
            },
            outcome: CapturedEpollCreateOutcome::Created {
                descriptor: 8,
                identity: CapturedDescriptorIdentity {
                    target: b"anon_inode:[eventpoll]".to_vec(),
                    device: 16,
                    inode: 60,
                    kind: CapturedDescriptorKind::Other,
                },
                mount_id: 18,
            },
        };
        assert!(!validate_captured_epoll_create_attempt(&attempt).unwrap());

        attempt.outcome = CapturedEpollCreateOutcome::Failed(libc::EMFILE);
        assert!(!validate_captured_epoll_create_attempt(&attempt).unwrap());
        attempt.outcome = CapturedEpollCreateOutcome::Terminated;
        assert!(validate_captured_epoll_create_attempt(&attempt).unwrap());
        attempt.outcome = CapturedEpollCreateOutcome::Failed(0);
        assert!(validate_captured_epoll_create_attempt(&attempt).is_err());
        attempt.request.flags = 1;
        assert!(validate_captured_epoll_create_attempt(&attempt).is_err());
        attempt.request.flags = libc::EPOLL_CLOEXEC as u32;
        attempt.outcome = CapturedEpollCreateOutcome::Created {
            descriptor: -1,
            identity: CapturedDescriptorIdentity {
                target: b"anon_inode:[eventpoll]".to_vec(),
                device: 16,
                inode: 60,
                kind: CapturedDescriptorKind::Other,
            },
            mount_id: 18,
        };
        assert!(validate_captured_epoll_create_attempt(&attempt).is_err());
        attempt.outcome = CapturedEpollCreateOutcome::Created {
            descriptor: 8,
            identity: CapturedDescriptorIdentity {
                target: b"anon_inode:[eventpoll]".to_vec(),
                device: 16,
                inode: 60,
                kind: CapturedDescriptorKind::RegularFile,
            },
            mount_id: 18,
        };
        assert!(validate_captured_epoll_create_attempt(&attempt).is_err());
        attempt.outcome = CapturedEpollCreateOutcome::Created {
            descriptor: 8,
            identity: CapturedDescriptorIdentity {
                target: b"anon_inode:[other]".to_vec(),
                device: 16,
                inode: 60,
                kind: CapturedDescriptorKind::Other,
            },
            mount_id: 18,
        };
        assert!(validate_captured_epoll_create_attempt(&attempt).is_err());
    }

    #[test]
    fn validates_local_socket_creation_and_connection_shapes() {
        let identity = CapturedDescriptorIdentity {
            target: b"socket:[60]".to_vec(),
            device: 16,
            inode: 60,
            kind: CapturedDescriptorKind::Socket,
        };
        let mut create = CapturedSocketCreateAttempt {
            syscall_index: 7,
            request: CapturedSocketCreateRequest {
                domain: libc::AF_UNIX,
                socket_type: libc::SOCK_STREAM
                    | libc::SOCK_CLOEXEC
                    | libc::SOCK_NONBLOCK,
                protocol: 0,
            },
            outcome: CapturedSocketCreateOutcome::Created {
                descriptor: 8,
                identity: identity.clone(),
                mount_id: 18,
            },
        };
        assert!(!validate_captured_socket_create_attempt(&create).unwrap());
        create.outcome = CapturedSocketCreateOutcome::Failed(libc::EMFILE);
        assert!(!validate_captured_socket_create_attempt(&create).unwrap());
        create.outcome = CapturedSocketCreateOutcome::Terminated;
        assert!(validate_captured_socket_create_attempt(&create).unwrap());
        create.request.domain = libc::AF_INET;
        assert!(validate_captured_socket_create_attempt(&create).is_err());
        create.request.domain = libc::AF_UNIX;
        create.request.socket_type = libc::SOCK_DGRAM;
        assert!(validate_captured_socket_create_attempt(&create).is_err());
        create.request.socket_type = libc::SOCK_STREAM;
        create.request.protocol = 1;
        assert!(validate_captured_socket_create_attempt(&create).is_err());
        create.request.protocol = 0;
        create.outcome = CapturedSocketCreateOutcome::Failed(0);
        assert!(validate_captured_socket_create_attempt(&create).is_err());
        create.outcome = CapturedSocketCreateOutcome::Created {
            descriptor: -1,
            identity: identity.clone(),
            mount_id: 18,
        };
        assert!(validate_captured_socket_create_attempt(&create).is_err());
        create.outcome = CapturedSocketCreateOutcome::Created {
            descriptor: 8,
            identity: CapturedDescriptorIdentity {
                target: b"socket:[060]".to_vec(),
                ..identity.clone()
            },
            mount_id: 18,
        };
        assert!(validate_captured_socket_create_attempt(&create).is_err());

        let mut connect = CapturedSocketConnectAttempt {
            syscall_index: 8,
            request: CapturedSocketConnectRequest {
                descriptor: 8,
                identity: identity.clone(),
                mount_id: 18,
                address_length: std::mem::size_of::<libc::sockaddr_un>() as u32,
                address: CapturedUnixSocketAddress::Pathname {
                    path: b"/nonexistent/socket".to_vec(),
                    terminated: true,
                },
            },
            outcome: CapturedSocketConnectOutcome::Failed {
                errno: libc::ENOENT,
                identity: identity.clone(),
                mount_id: 18,
            },
        };
        assert!(!validate_captured_socket_connect_attempt(&connect).unwrap());
        connect.outcome = CapturedSocketConnectOutcome::Connected {
            identity: identity.clone(),
            mount_id: 18,
        };
        assert!(!validate_captured_socket_connect_attempt(&connect).unwrap());
        connect.outcome = CapturedSocketConnectOutcome::Terminated;
        assert!(validate_captured_socket_connect_attempt(&connect).unwrap());
        connect.request.address_length = 2;
        assert!(validate_captured_socket_connect_attempt(&connect).is_err());
        connect.request.address_length = 3;
        connect.request.address = CapturedUnixSocketAddress::Abstract {
            name: Vec::new(),
        };
        assert!(validate_captured_socket_connect_attempt(&connect).unwrap());
        connect.request.address_length = 4;
        assert!(validate_captured_socket_connect_attempt(&connect).is_err());
        connect.request.address_length = 2;
        connect.request.address = CapturedUnixSocketAddress::Unnamed;
        connect.outcome = CapturedSocketConnectOutcome::Failed {
            errno: 0,
            identity: identity.clone(),
            mount_id: 18,
        };
        assert!(validate_captured_socket_connect_attempt(&connect).is_err());
        connect.outcome = CapturedSocketConnectOutcome::Failed {
            errno: libc::ENOENT,
            identity: CapturedDescriptorIdentity {
                inode: 61,
                ..identity
            },
            mount_id: 18,
        };
        assert!(validate_captured_socket_connect_attempt(&connect).is_err());
    }

    #[test]
    fn validates_private_futex_wake_shapes() {
        let mut attempt = CapturedFutexWakeAttempt {
            syscall_index: 7,
            request: CapturedFutexWakeRequest {
                address: 0x1000,
                maximum_waiters: i32::MAX as u32,
            },
            outcome: CapturedFutexWakeOutcome::Woken(0),
        };
        assert!(!validate_captured_futex_wake_attempt(&attempt).unwrap());

        attempt.outcome = CapturedFutexWakeOutcome::Failed(libc::EAGAIN);
        assert!(!validate_captured_futex_wake_attempt(&attempt).unwrap());
        attempt.outcome = CapturedFutexWakeOutcome::Terminated;
        assert!(validate_captured_futex_wake_attempt(&attempt).unwrap());
        attempt.outcome = CapturedFutexWakeOutcome::Woken(i32::MAX as u32);
        assert!(!validate_captured_futex_wake_attempt(&attempt).unwrap());

        attempt.outcome = CapturedFutexWakeOutcome::Woken(i32::MAX as u32 + 1);
        assert!(validate_captured_futex_wake_attempt(&attempt).is_err());
        attempt.outcome = CapturedFutexWakeOutcome::Woken(0);
        attempt.request.address = 1;
        assert!(validate_captured_futex_wake_attempt(&attempt).is_err());
        attempt.request.address = 0x1000;
        attempt.request.maximum_waiters = i32::MAX as u32 + 1;
        assert!(validate_captured_futex_wake_attempt(&attempt).is_err());
    }

    #[test]
    fn validates_supported_process_creation_shapes() {
        let mut attempt = CapturedProcessCreationAttempt {
            syscall_index: 8,
            request: CapturedProcessCreationRequest::Clone3 {
                flags: 0x0000_0001_0000_4100,
                exit_signal: 17,
            },
            outcome: CapturedProcessCreationOutcome::Created(2),
        };
        assert!(!validate_captured_process_creation_attempt(&attempt).unwrap());

        attempt.request = CapturedProcessCreationRequest::Clone3 {
            flags: 0x4100,
            exit_signal: 17,
        };
        assert!(!validate_captured_process_creation_attempt(&attempt).unwrap());

        attempt.outcome = CapturedProcessCreationOutcome::Failed(libc::EAGAIN);
        assert!(!validate_captured_process_creation_attempt(&attempt).unwrap());
        attempt.outcome = CapturedProcessCreationOutcome::Restarted(513);
        assert!(!validate_captured_process_creation_attempt(&attempt).unwrap());
        attempt.outcome = CapturedProcessCreationOutcome::Restarted(512);
        assert!(validate_captured_process_creation_attempt(&attempt).is_err());
        attempt.outcome = CapturedProcessCreationOutcome::Terminated;
        assert!(validate_captured_process_creation_attempt(&attempt).unwrap());
        attempt.outcome = CapturedProcessCreationOutcome::Created(0);
        assert!(validate_captured_process_creation_attempt(&attempt).is_err());
        attempt.outcome = CapturedProcessCreationOutcome::Created(2);
        attempt.request = CapturedProcessCreationRequest::Clone3 {
            flags: 0x4000,
            exit_signal: 17,
        };
        assert!(validate_captured_process_creation_attempt(&attempt).is_err());
        attempt.request = CapturedProcessCreationRequest::Vfork;
        assert!(!validate_captured_process_creation_attempt(&attempt).unwrap());
        attempt.request = CapturedProcessCreationRequest::Clone { flags: 0x0120_0011 };
        assert!(!validate_captured_process_creation_attempt(&attempt).unwrap());
        attempt.request = CapturedProcessCreationRequest::Clone { flags: 0x11 };
        assert!(validate_captured_process_creation_attempt(&attempt).is_err());
    }

    #[test]
    fn validates_supported_child_wait_shapes() {
        let mut attempt = CapturedChildWaitAttempt {
            syscall_index: 9,
            request: CapturedChildWaitRequest {
                selector: CapturedChildWaitSelector::Child(2),
                nonblocking: false,
                status_is_null: false,
            },
            outcome: CapturedChildWaitOutcome::Reaped {
                child: 2,
                status: 0,
            },
        };
        assert!(!validate_captured_child_wait_attempt(&attempt).unwrap());

        attempt.outcome = CapturedChildWaitOutcome::Failed(libc::ECHILD);
        assert!(!validate_captured_child_wait_attempt(&attempt).unwrap());
        attempt.outcome = CapturedChildWaitOutcome::Terminated;
        assert!(validate_captured_child_wait_attempt(&attempt).unwrap());
        attempt.outcome = CapturedChildWaitOutcome::NoChildReady;
        assert!(validate_captured_child_wait_attempt(&attempt).is_err());
        attempt.request.nonblocking = true;
        assert!(!validate_captured_child_wait_attempt(&attempt).unwrap());
        attempt.outcome = CapturedChildWaitOutcome::Reaped {
            child: 3,
            status: 0,
        };
        assert!(validate_captured_child_wait_attempt(&attempt).is_err());
        attempt.request.selector = CapturedChildWaitSelector::AnyChild;
        assert!(!validate_captured_child_wait_attempt(&attempt).unwrap());
        attempt.request.selector = CapturedChildWaitSelector::Child(0);
        assert!(validate_captured_child_wait_attempt(&attempt).is_err());

        attempt.request = CapturedChildWaitRequest {
            selector: CapturedChildWaitSelector::AnyChild,
            nonblocking: false,
            status_is_null: true,
        };
        attempt.outcome = CapturedChildWaitOutcome::Failed(libc::ECHILD);
        assert!(!validate_captured_child_wait_attempt(&attempt).unwrap());
        attempt.outcome = CapturedChildWaitOutcome::Failed(libc::EBADF);
        assert!(validate_captured_child_wait_attempt(&attempt).is_err());
        attempt.outcome = CapturedChildWaitOutcome::Reaped {
            child: 2,
            status: 0,
        };
        assert!(validate_captured_child_wait_attempt(&attempt).is_err());
        attempt.outcome = CapturedChildWaitOutcome::Terminated;
        assert!(validate_captured_child_wait_attempt(&attempt).unwrap());
        attempt.request.nonblocking = true;
        assert!(validate_captured_child_wait_attempt(&attempt).is_err());
    }

    #[test]
    fn validates_supported_process_identity_shapes() {
        let mut attempt = CapturedProcessIdentityAttempt {
            syscall_index: 10,
            request: CapturedProcessIdentityRequest::Getpid,
            outcome: CapturedProcessIdentityOutcome::Identity(2),
        };
        assert!(!validate_captured_process_identity_attempt(&attempt).unwrap());
        attempt.outcome = CapturedProcessIdentityOutcome::Terminated;
        assert!(validate_captured_process_identity_attempt(&attempt).unwrap());
        attempt.outcome = CapturedProcessIdentityOutcome::Identity(0);
        assert!(validate_captured_process_identity_attempt(&attempt).is_err());
        attempt.request = CapturedProcessIdentityRequest::Gettid;
        attempt.outcome = CapturedProcessIdentityOutcome::Identity(2);
        assert!(!validate_captured_process_identity_attempt(&attempt).unwrap());
        attempt.outcome = CapturedProcessIdentityOutcome::ProcessGroup(2);
        assert!(validate_captured_process_identity_attempt(&attempt).is_err());
        attempt.request = CapturedProcessIdentityRequest::Getppid;
        attempt.outcome = CapturedProcessIdentityOutcome::Identity(1);
        assert!(!validate_captured_process_identity_attempt(&attempt).unwrap());
        attempt.request = CapturedProcessIdentityRequest::Getpgrp;
        assert!(validate_captured_process_identity_attempt(&attempt).is_err());
        attempt.outcome = CapturedProcessIdentityOutcome::ProcessGroup(1);
        assert!(!validate_captured_process_identity_attempt(&attempt).unwrap());
        attempt.outcome = CapturedProcessIdentityOutcome::ProcessGroup(0);
        assert!(validate_captured_process_identity_attempt(&attempt).is_err());
        attempt.outcome = CapturedProcessIdentityOutcome::ProcessGroup(u32::MAX);
        assert!(validate_captured_process_identity_attempt(&attempt).is_err());
    }

    #[test]
    fn validates_terminal_probe_enotty_attempts_as_raw_evidence() {
        let mut attempt = CapturedIoctlAttempt {
            syscall_index: 10,
            request: CapturedIoctlRequest {
                descriptor: 2,
                identity: CapturedDescriptorIdentity {
                    target: b"pipe:[fixture]".to_vec(),
                    device: 1,
                    inode: 2,
                    kind: CapturedDescriptorKind::Fifo,
                },
                mount_id: 3,
                command: libc::TCGETS as u64,
            },
            outcome: CapturedIoctlOutcome::Failed(libc::ENOTTY),
        };
        assert!(!validate_captured_ioctl_attempt(&attempt).unwrap());
        attempt.outcome = CapturedIoctlOutcome::Terminated;
        assert!(validate_captured_ioctl_attempt(&attempt).unwrap());
        attempt.outcome = CapturedIoctlOutcome::Failed(libc::EINVAL);
        assert!(validate_captured_ioctl_attempt(&attempt).is_err());
        attempt.outcome = CapturedIoctlOutcome::Failed(libc::ENOTTY);
        attempt.request.command = libc::TIOCGWINSZ as u64;
        validate_captured_ioctl_attempt(&attempt).unwrap();
        attempt.request.identity.kind = CapturedDescriptorKind::RegularFile;
        validate_captured_ioctl_attempt(&attempt).unwrap();
        attempt.request.identity.kind = CapturedDescriptorKind::Directory;
        validate_captured_ioctl_attempt(&attempt).unwrap();
        attempt.request.identity.kind = CapturedDescriptorKind::CharacterDevice;
        attempt.request.identity.target = b"/dev/null".to_vec();
        validate_captured_ioctl_attempt(&attempt).unwrap();
        attempt.request.identity.target = b"/dev/tty".to_vec();
        validate_captured_ioctl_attempt(&attempt).unwrap();
        attempt.request.identity.kind = CapturedDescriptorKind::Socket;
        validate_captured_ioctl_attempt(&attempt).unwrap();
        attempt.request.command = 0;
        assert!(validate_captured_ioctl_attempt(&attempt).is_err());
    }

    #[test]
    fn validates_exact_prlimit_mutation_shapes() {
        let requested = CapturedResourceLimit {
            current: 64 * 1024 * 1024,
            maximum: u64::MAX,
        };
        let mut attempt = CapturedPrlimitAttempt {
            syscall_index: 1,
            request: CapturedPrlimitRequest {
                process: 0,
                resource: libc::RLIMIT_STACK,
                new_limit_is_null: false,
                old_limit_is_null: true,
            },
            outcome: CapturedPrlimitOutcome::Mutated {
                requested,
                previous: None,
            },
        };
        assert!(!validate_captured_prlimit_attempt(&attempt).unwrap());

        attempt.outcome = CapturedPrlimitOutcome::MutationFailed {
            requested,
            errno: libc::EPERM,
        };
        assert!(!validate_captured_prlimit_attempt(&attempt).unwrap());
        attempt.outcome = CapturedPrlimitOutcome::MutationTerminated { requested };
        assert!(validate_captured_prlimit_attempt(&attempt).unwrap());

        attempt.outcome = CapturedPrlimitOutcome::Mutated {
            requested,
            previous: Some(requested),
        };
        assert!(validate_captured_prlimit_attempt(&attempt).is_err());
        attempt.request.old_limit_is_null = false;
        assert!(!validate_captured_prlimit_attempt(&attempt).unwrap());

        attempt.request.new_limit_is_null = true;
        assert!(validate_captured_prlimit_attempt(&attempt).is_err());
    }

    #[test]
    fn validates_exact_unmodeled_syscall_dispositions() {
        let mut attempt = CapturedUnmodeledSyscallAttempt {
            syscall_index: 4,
            number: 99,
            arguments: [1, 2, 3, 4, 5, 6],
            outcome: CapturedUnmodeledSyscallOutcome::Returned {
                return_value: 7,
                is_error: false,
            },
        };
        assert!(!validate_captured_unmodeled_syscall_attempt(&attempt).unwrap());

        attempt.outcome = CapturedUnmodeledSyscallOutcome::Returned {
            return_value: -libc::EINVAL as i64,
            is_error: true,
        };
        assert!(!validate_captured_unmodeled_syscall_attempt(&attempt).unwrap());
        attempt.outcome = CapturedUnmodeledSyscallOutcome::Terminated;
        assert!(validate_captured_unmodeled_syscall_attempt(&attempt).unwrap());

        attempt.outcome = CapturedUnmodeledSyscallOutcome::Returned {
            return_value: 0,
            is_error: true,
        };
        assert!(validate_captured_unmodeled_syscall_attempt(&attempt).is_err());
        attempt.outcome = CapturedUnmodeledSyscallOutcome::Returned {
            return_value: -4096,
            is_error: true,
        };
        assert!(validate_captured_unmodeled_syscall_attempt(&attempt).is_err());
    }

    #[test]
    fn validation_rejects_unknown_fields_and_broken_parentage() {
        let receipt = ProcessCaptureReceipt::new(
            1,
            vec![CapturedProcess {
                id: 1,
                parent: None,
                invocations: vec![CapturedInvocation {
                    executable: b"/bin/true".to_vec(),
                    argv: vec![b"true".to_vec()],
                    environment: vec![b"A=b".to_vec()],
                    working_directory: b"/".to_vec(),
                    signal_state: CapturedExecSignalState::default(),
                    file_creation_mask: 0o022,
                    credentials: CapturedCredentialState::default(),
                    file_mappings: vec![fixture_exec_mapping()],
                }],
                exec_attempts: Vec::new(),
                exec_syscalls: 0,
                open_attempts: Vec::new(),
                open_syscalls: 0,
                newfstatat_attempts: Vec::new(),
                newfstatat_syscalls: 0,
                fstat_attempts: Vec::new(),
                fstat_syscalls: 0,
                getrandom_attempts: Vec::new(),
                getrandom_syscalls: 0,
                runtime_observation_attempts: Vec::new(),
                runtime_observation_syscalls: 0,
                prlimit_attempts: Vec::new(),
                prlimit_syscalls: 0,
                close_attempts: Vec::new(),
                close_syscalls: 0,
                seek_attempts: Vec::new(),
                seek_syscalls: 0,
                file_advice_attempts: Vec::new(),
                file_advice_syscalls: 0,
                fcntl_attempts: Vec::new(),
                fcntl_syscalls: 0,
                ioctl_attempts: Vec::new(),
                ioctl_syscalls: 0,
                pipe_attempts: Vec::new(),
                pipe_syscalls: 0,
                epoll_create_attempts: Vec::new(),
                epoll_create_syscalls: 0,
                socket_create_attempts: Vec::new(),
                socket_create_syscalls: 0,
                socket_connect_attempts: Vec::new(),
                socket_connect_syscalls: 0,
                dup2_attempts: Vec::new(),
                dup2_syscalls: 0,
                signal_state_attempts: Vec::new(),
                signal_state_syscalls: 0,
                signal_deliveries: Vec::new(),
                umask_attempts: Vec::new(),
                umask_syscalls: 0,
                credential_attempts: Vec::new(),
                credential_syscalls: 0,
                futex_wake_attempts: Vec::new(),
                process_creation_attempts: Vec::new(),
                futex_wake_syscalls: 0,
                process_creation_syscalls: 0,
                child_wait_attempts: Vec::new(),
                child_wait_syscalls: 0,
                process_identity_attempts: Vec::new(),
                process_identity_syscalls: 0,
                memory_state_attempts: Vec::new(),
                memory_state_syscalls: 0,
                thread_state_attempts: Vec::new(),
                thread_state_syscalls: 0,
                lifecycle_exit_attempts: Vec::new(),
                lifecycle_exit_syscalls: 0,
                statx_attempts: Vec::new(),
                statx_syscalls: 0,
                xattr_attempts: Vec::new(),
                xattr_syscalls: 0,
                readlink_attempts: Vec::new(),
                readlink_syscalls: 0,
                access_attempts: Vec::new(),
                access_syscalls: 0,
                getcwd_attempts: Vec::new(),
                getcwd_syscalls: 0,
                cwd_change_attempts: Vec::new(),
                cwd_change_syscalls: 0,
                path_removal_attempts: Vec::new(),
                path_removal_syscalls: 0,
                directory_creation_attempts: Vec::new(),
                directory_creation_syscalls: 0,
                path_rename_attempts: Vec::new(),
                path_rename_syscalls: 0,
                path_mode_change_attempts: Vec::new(),
                path_mode_change_syscalls: 0,
                descriptor_timestamp_change_attempts: Vec::new(),
                descriptor_timestamp_change_syscalls: 0,
                symbolic_link_creation_attempts: Vec::new(),
                symbolic_link_creation_syscalls: 0,
                filesystem_status_attempts: Vec::new(),
                filesystem_status_syscalls: 0,
                read_attempts: Vec::new(),
                read_syscalls: 0,
                getdents64_attempts: Vec::new(),
                getdents64_syscalls: 0,
                mmap_attempts: Vec::new(),
                mmap_syscalls: 0,
                write_attempts: Vec::new(),
                write_syscalls: 0,
                unmodeled_syscall_attempts: Vec::new(),
                unmodeled_syscalls: 0,
                syscall_entries: 0,
                syscall_exits: 0,
                terminal_syscalls: 0,
                outcome: CapturedProcessOutcome::Exited(0),
            }],
        )
        .unwrap();
        let mut encoded = serde_json::to_value(&receipt).unwrap();
        encoded["unexpected"] = serde_json::json!(true);
        assert!(serde_json::from_value::<ProcessCaptureReceipt>(encoded).is_err());

        let mut incomplete = receipt.clone();
        incomplete.processes[0].syscall_entries += 1;
        assert!(incomplete.validate_complete().is_err());

        let mut overflowed = receipt.clone();
        overflowed.processes[0].syscall_entries = u64::MAX;
        overflowed.processes[0].syscall_exits = u64::MAX;
        overflowed.processes.push(CapturedProcess {
            id: 2,
            parent: Some(1),
            invocations: Vec::new(),
            exec_attempts: Vec::new(),
            exec_syscalls: 0,
            open_attempts: Vec::new(),
            open_syscalls: 0,
            newfstatat_attempts: Vec::new(),
            newfstatat_syscalls: 0,
            fstat_attempts: Vec::new(),
            fstat_syscalls: 0,
            getrandom_attempts: Vec::new(),
            getrandom_syscalls: 0,
            runtime_observation_attempts: Vec::new(),
            runtime_observation_syscalls: 0,
            prlimit_attempts: Vec::new(),
            prlimit_syscalls: 0,
            close_attempts: Vec::new(),
            close_syscalls: 0,
            seek_attempts: Vec::new(),
            seek_syscalls: 0,
            file_advice_attempts: Vec::new(),
            file_advice_syscalls: 0,
            fcntl_attempts: Vec::new(),
            fcntl_syscalls: 0,
            ioctl_attempts: Vec::new(),
            ioctl_syscalls: 0,
            pipe_attempts: Vec::new(),
            pipe_syscalls: 0,
            epoll_create_attempts: Vec::new(),
            epoll_create_syscalls: 0,
            socket_create_attempts: Vec::new(),
            socket_create_syscalls: 0,
            socket_connect_attempts: Vec::new(),
            socket_connect_syscalls: 0,
            dup2_attempts: Vec::new(),
            dup2_syscalls: 0,
            signal_state_attempts: Vec::new(),
            signal_state_syscalls: 0,
            signal_deliveries: Vec::new(),
            umask_attempts: Vec::new(),
            umask_syscalls: 0,
            credential_attempts: Vec::new(),
            credential_syscalls: 0,
            futex_wake_attempts: Vec::new(),
            process_creation_attempts: Vec::new(),
            futex_wake_syscalls: 0,
            process_creation_syscalls: 0,
            child_wait_attempts: Vec::new(),
            child_wait_syscalls: 0,
            process_identity_attempts: Vec::new(),
            process_identity_syscalls: 0,
            memory_state_attempts: Vec::new(),
            memory_state_syscalls: 0,
            thread_state_attempts: Vec::new(),
            thread_state_syscalls: 0,
            lifecycle_exit_attempts: Vec::new(),
            lifecycle_exit_syscalls: 0,
            statx_attempts: Vec::new(),
            statx_syscalls: 0,
            xattr_attempts: Vec::new(),
            xattr_syscalls: 0,
            readlink_attempts: Vec::new(),
            readlink_syscalls: 0,
            access_attempts: Vec::new(),
            access_syscalls: 0,
            getcwd_attempts: Vec::new(),
            getcwd_syscalls: 0,
            cwd_change_attempts: Vec::new(),
            cwd_change_syscalls: 0,
            path_removal_attempts: Vec::new(),
            path_removal_syscalls: 0,
            directory_creation_attempts: Vec::new(),
            directory_creation_syscalls: 0,
            path_rename_attempts: Vec::new(),
            path_rename_syscalls: 0,
            path_mode_change_attempts: Vec::new(),
            path_mode_change_syscalls: 0,
            descriptor_timestamp_change_attempts: Vec::new(),
            descriptor_timestamp_change_syscalls: 0,
            symbolic_link_creation_attempts: Vec::new(),
            symbolic_link_creation_syscalls: 0,
            filesystem_status_attempts: Vec::new(),
            filesystem_status_syscalls: 0,
            read_attempts: Vec::new(),
            read_syscalls: 0,
            getdents64_attempts: Vec::new(),
            getdents64_syscalls: 0,
            mmap_attempts: Vec::new(),
            mmap_syscalls: 0,
            write_attempts: Vec::new(),
            write_syscalls: 0,
            unmodeled_syscall_attempts: Vec::new(),
            unmodeled_syscalls: 0,
            syscall_entries: 1,
            syscall_exits: 1,
            terminal_syscalls: 0,
            outcome: CapturedProcessOutcome::Exited(0),
        });
        assert!(overflowed.validate_complete().is_err());

        let mut broken = receipt;
        broken.processes[0].parent = Some(99);
        assert!(broken.validate_complete().is_err());
    }

    #[test]
    fn validation_requires_loaded_execs_in_invocation_order() {
        let request = |executable: &[u8]| CapturedExecRequest {
            executable: executable.to_vec(),
            directory: None,
            argv: vec![executable.to_vec()],
            environment: Vec::new(),
            flags: 0,
        };
        let receipt = ProcessCaptureReceipt::new(
            1,
            vec![CapturedProcess {
                id: 1,
                parent: None,
                invocations: vec![
                    fixture_invocation(b"/initial"),
                    fixture_invocation(b"/second"),
                    fixture_invocation(b"/third"),
                ],
                exec_attempts: vec![
                    CapturedExecAttempt {
                        syscall_index: 0,
                        request: request(b"/second"),
                        outcome: CapturedExecOutcome::Loaded(1),
                    },
                    CapturedExecAttempt {
                        syscall_index: 1,
                        request: request(b"/third"),
                        outcome: CapturedExecOutcome::Loaded(2),
                    },
                ],
                exec_syscalls: 2,
                open_attempts: Vec::new(),
                open_syscalls: 0,
                newfstatat_attempts: Vec::new(),
                newfstatat_syscalls: 0,
                fstat_attempts: Vec::new(),
                fstat_syscalls: 0,
                getrandom_attempts: Vec::new(),
                getrandom_syscalls: 0,
                runtime_observation_attempts: Vec::new(),
                runtime_observation_syscalls: 0,
                prlimit_attempts: Vec::new(),
                prlimit_syscalls: 0,
                close_attempts: Vec::new(),
                close_syscalls: 0,
                seek_attempts: Vec::new(),
                seek_syscalls: 0,
                file_advice_attempts: Vec::new(),
                file_advice_syscalls: 0,
                fcntl_attempts: Vec::new(),
                fcntl_syscalls: 0,
                ioctl_attempts: Vec::new(),
                ioctl_syscalls: 0,
                pipe_attempts: Vec::new(),
                pipe_syscalls: 0,
                epoll_create_attempts: Vec::new(),
                epoll_create_syscalls: 0,
                socket_create_attempts: Vec::new(),
                socket_create_syscalls: 0,
                socket_connect_attempts: Vec::new(),
                socket_connect_syscalls: 0,
                dup2_attempts: Vec::new(),
                dup2_syscalls: 0,
                signal_state_attempts: Vec::new(),
                signal_state_syscalls: 0,
                signal_deliveries: Vec::new(),
                umask_attempts: Vec::new(),
                umask_syscalls: 0,
                credential_attempts: Vec::new(),
                credential_syscalls: 0,
                futex_wake_attempts: Vec::new(),
                process_creation_attempts: Vec::new(),
                futex_wake_syscalls: 0,
                process_creation_syscalls: 0,
                child_wait_attempts: Vec::new(),
                child_wait_syscalls: 0,
                process_identity_attempts: Vec::new(),
                process_identity_syscalls: 0,
                memory_state_attempts: Vec::new(),
                memory_state_syscalls: 0,
                thread_state_attempts: Vec::new(),
                thread_state_syscalls: 0,
                lifecycle_exit_attempts: Vec::new(),
                lifecycle_exit_syscalls: 0,
                statx_attempts: Vec::new(),
                statx_syscalls: 0,
                xattr_attempts: Vec::new(),
                xattr_syscalls: 0,
                readlink_attempts: Vec::new(),
                readlink_syscalls: 0,
                access_attempts: Vec::new(),
                access_syscalls: 0,
                getcwd_attempts: Vec::new(),
                getcwd_syscalls: 0,
                cwd_change_attempts: Vec::new(),
                cwd_change_syscalls: 0,
                path_removal_attempts: Vec::new(),
                path_removal_syscalls: 0,
                directory_creation_attempts: Vec::new(),
                directory_creation_syscalls: 0,
                path_rename_attempts: Vec::new(),
                path_rename_syscalls: 0,
                path_mode_change_attempts: Vec::new(),
                path_mode_change_syscalls: 0,
                descriptor_timestamp_change_attempts: Vec::new(),
                descriptor_timestamp_change_syscalls: 0,
                symbolic_link_creation_attempts: Vec::new(),
                symbolic_link_creation_syscalls: 0,
                filesystem_status_attempts: Vec::new(),
                filesystem_status_syscalls: 0,
                read_attempts: Vec::new(),
                read_syscalls: 0,
                getdents64_attempts: Vec::new(),
                getdents64_syscalls: 0,
                mmap_attempts: Vec::new(),
                mmap_syscalls: 0,
                write_attempts: Vec::new(),
                write_syscalls: 0,
                unmodeled_syscall_attempts: Vec::new(),
                unmodeled_syscalls: 0,
                syscall_entries: 2,
                syscall_exits: 2,
                terminal_syscalls: 0,
                outcome: CapturedProcessOutcome::Exited(0),
            }],
        )
        .unwrap();

        let mut reordered = receipt;
        reordered.processes[0].exec_attempts[0].outcome = CapturedExecOutcome::Loaded(2);
        reordered.processes[0].exec_attempts[1].outcome = CapturedExecOutcome::Loaded(1);
        assert!(reordered.validate_complete().is_err());
    }

    #[test]
    fn validates_terminal_newfstatat_coverage() {
        let receipt = ProcessCaptureReceipt::new(
            1,
            vec![CapturedProcess {
                id: 1,
                parent: None,
                invocations: vec![fixture_invocation(b"/initial")],
                exec_attempts: Vec::new(),
                exec_syscalls: 0,
                open_attempts: Vec::new(),
                open_syscalls: 0,
                newfstatat_attempts: vec![CapturedNewfstatatAttempt {
                    syscall_index: 0,
                    request: CapturedNewfstatatRequest {
                        directory_descriptor: libc::AT_FDCWD,
                        path: b"/fixture/value".to_vec(),
                        path_is_null: false,
                        base: None,
                        flags: 0,
                    },
                    outcome: CapturedNewfstatatOutcome::Terminated,
                }],
                newfstatat_syscalls: 1,
                fstat_attempts: Vec::new(),
                fstat_syscalls: 0,
                getrandom_attempts: Vec::new(),
                getrandom_syscalls: 0,
                runtime_observation_attempts: Vec::new(),
                runtime_observation_syscalls: 0,
                prlimit_attempts: Vec::new(),
                prlimit_syscalls: 0,
                close_attempts: Vec::new(),
                close_syscalls: 0,
                seek_attempts: Vec::new(),
                seek_syscalls: 0,
                file_advice_attempts: Vec::new(),
                file_advice_syscalls: 0,
                fcntl_attempts: Vec::new(),
                fcntl_syscalls: 0,
                ioctl_attempts: Vec::new(),
                ioctl_syscalls: 0,
                pipe_attempts: Vec::new(),
                pipe_syscalls: 0,
                epoll_create_attempts: Vec::new(),
                epoll_create_syscalls: 0,
                socket_create_attempts: Vec::new(),
                socket_create_syscalls: 0,
                socket_connect_attempts: Vec::new(),
                socket_connect_syscalls: 0,
                dup2_attempts: Vec::new(),
                dup2_syscalls: 0,
                signal_state_attempts: Vec::new(),
                signal_state_syscalls: 0,
                signal_deliveries: Vec::new(),
                umask_attempts: Vec::new(),
                umask_syscalls: 0,
                credential_attempts: Vec::new(),
                credential_syscalls: 0,
                futex_wake_attempts: Vec::new(),
                process_creation_attempts: Vec::new(),
                futex_wake_syscalls: 0,
                process_creation_syscalls: 0,
                child_wait_attempts: Vec::new(),
                child_wait_syscalls: 0,
                process_identity_attempts: Vec::new(),
                process_identity_syscalls: 0,
                memory_state_attempts: Vec::new(),
                memory_state_syscalls: 0,
                thread_state_attempts: Vec::new(),
                thread_state_syscalls: 0,
                lifecycle_exit_attempts: Vec::new(),
                lifecycle_exit_syscalls: 0,
                statx_attempts: Vec::new(),
                statx_syscalls: 0,
                xattr_attempts: Vec::new(),
                xattr_syscalls: 0,
                readlink_attempts: Vec::new(),
                readlink_syscalls: 0,
                access_attempts: Vec::new(),
                access_syscalls: 0,
                getcwd_attempts: Vec::new(),
                getcwd_syscalls: 0,
                cwd_change_attempts: Vec::new(),
                cwd_change_syscalls: 0,
                path_removal_attempts: Vec::new(),
                path_removal_syscalls: 0,
                directory_creation_attempts: Vec::new(),
                directory_creation_syscalls: 0,
                path_rename_attempts: Vec::new(),
                path_rename_syscalls: 0,
                path_mode_change_attempts: Vec::new(),
                path_mode_change_syscalls: 0,
                descriptor_timestamp_change_attempts: Vec::new(),
                descriptor_timestamp_change_syscalls: 0,
                symbolic_link_creation_attempts: Vec::new(),
                symbolic_link_creation_syscalls: 0,
                filesystem_status_attempts: Vec::new(),
                filesystem_status_syscalls: 0,
                read_attempts: Vec::new(),
                read_syscalls: 0,
                getdents64_attempts: Vec::new(),
                getdents64_syscalls: 0,
                mmap_attempts: Vec::new(),
                mmap_syscalls: 0,
                write_attempts: Vec::new(),
                write_syscalls: 0,
                unmodeled_syscall_attempts: Vec::new(),
                unmodeled_syscalls: 0,
                syscall_entries: 1,
                syscall_exits: 0,
                terminal_syscalls: 1,
                outcome: CapturedProcessOutcome::Signaled(libc::SIGKILL),
            }],
        )
        .unwrap();
        assert_eq!(receipt.newfstatat_syscalls().unwrap(), 1);

        let mut fstat_receipt = receipt.clone();
        let process = &mut fstat_receipt.processes[0];
        process.newfstatat_attempts.clear();
        process.newfstatat_syscalls = 0;
        process.fstat_attempts = vec![CapturedFstatAttempt {
            syscall_index: 0,
            request: CapturedFstatRequest {
                descriptor: 3,
                identity: Some(CapturedDescriptorIdentity {
                    target: b"/fixture/value".to_vec(),
                    device: 1,
                    inode: 2,
                    kind: CapturedDescriptorKind::RegularFile,
                }),
                mount_id: Some(3),
                output_is_null: false,
            },
            outcome: CapturedFstatOutcome::Terminated,
        }];
        process.fstat_syscalls = 1;
        fstat_receipt.validate_complete().unwrap();
        assert_eq!(fstat_receipt.fstat_syscalls().unwrap(), 1);

        let mut incomplete_fstat = fstat_receipt.clone();
        incomplete_fstat.processes[0].fstat_attempts.clear();
        assert!(incomplete_fstat.validate_complete().is_err());

        let mut getrandom_receipt = fstat_receipt.clone();
        let process = &mut getrandom_receipt.processes[0];
        process.fstat_attempts.clear();
        process.fstat_syscalls = 0;
        process.getrandom_attempts = vec![CapturedGetrandomAttempt {
            syscall_index: 0,
            request: CapturedGetrandomRequest {
                requested_bytes: 1,
                flags: 0,
                output_is_null: false,
            },
            outcome: CapturedGetrandomOutcome::Terminated,
        }];
        process.getrandom_syscalls = 1;
        getrandom_receipt.validate_complete().unwrap();
        assert_eq!(getrandom_receipt.getrandom_syscalls().unwrap(), 1);

        let mut incomplete_getrandom = getrandom_receipt;
        incomplete_getrandom.processes[0].getrandom_attempts.clear();
        assert!(incomplete_getrandom.validate_complete().is_err());

        let mut invalid_fstat_identity = fstat_receipt;
        invalid_fstat_identity.processes[0].fstat_attempts[0]
            .request
            .mount_id = None;
        assert!(invalid_fstat_identity.validate_complete().is_err());

        let mut missing_terminal = receipt;
        missing_terminal.processes[0].terminal_syscalls = 0;
        assert!(missing_terminal.validate_complete().is_err());
    }

    #[test]
    fn validates_terminal_readlink_coverage() {
        let receipt = ProcessCaptureReceipt::new(
            1,
            vec![CapturedProcess {
                id: 1,
                parent: None,
                invocations: vec![fixture_invocation(b"/initial")],
                exec_attempts: Vec::new(),
                exec_syscalls: 0,
                open_attempts: Vec::new(),
                open_syscalls: 0,
                newfstatat_attempts: Vec::new(),
                newfstatat_syscalls: 0,
                fstat_attempts: Vec::new(),
                fstat_syscalls: 0,
                getrandom_attempts: Vec::new(),
                getrandom_syscalls: 0,
                runtime_observation_attempts: Vec::new(),
                runtime_observation_syscalls: 0,
                prlimit_attempts: Vec::new(),
                prlimit_syscalls: 0,
                close_attempts: Vec::new(),
                close_syscalls: 0,
                seek_attempts: Vec::new(),
                seek_syscalls: 0,
                file_advice_attempts: Vec::new(),
                file_advice_syscalls: 0,
                fcntl_attempts: Vec::new(),
                fcntl_syscalls: 0,
                ioctl_attempts: Vec::new(),
                ioctl_syscalls: 0,
                pipe_attempts: Vec::new(),
                pipe_syscalls: 0,
                epoll_create_attempts: Vec::new(),
                epoll_create_syscalls: 0,
                socket_create_attempts: Vec::new(),
                socket_create_syscalls: 0,
                socket_connect_attempts: Vec::new(),
                socket_connect_syscalls: 0,
                dup2_attempts: Vec::new(),
                dup2_syscalls: 0,
                signal_state_attempts: Vec::new(),
                signal_state_syscalls: 0,
                signal_deliveries: Vec::new(),
                umask_attempts: Vec::new(),
                umask_syscalls: 0,
                credential_attempts: Vec::new(),
                credential_syscalls: 0,
                futex_wake_attempts: Vec::new(),
                process_creation_attempts: Vec::new(),
                futex_wake_syscalls: 0,
                process_creation_syscalls: 0,
                child_wait_attempts: Vec::new(),
                child_wait_syscalls: 0,
                process_identity_attempts: Vec::new(),
                process_identity_syscalls: 0,
                memory_state_attempts: Vec::new(),
                memory_state_syscalls: 0,
                thread_state_attempts: Vec::new(),
                thread_state_syscalls: 0,
                lifecycle_exit_attempts: Vec::new(),
                lifecycle_exit_syscalls: 0,
                statx_attempts: Vec::new(),
                statx_syscalls: 0,
                xattr_attempts: Vec::new(),
                xattr_syscalls: 0,
                readlink_attempts: vec![CapturedReadlinkAttempt {
                    syscall_index: 0,
                    request: CapturedReadlinkRequest {
                        system_call: CapturedReadlinkSystemCall::ReadlinkAt,
                        directory_descriptor: libc::AT_FDCWD,
                        path: b"/fixture/link".to_vec(),
                        path_is_null: false,
                        base: None,
                        requested_bytes: 128,
                    },
                    outcome: CapturedReadlinkOutcome::Terminated,
                }],
                readlink_syscalls: 1,
                access_attempts: Vec::new(),
                access_syscalls: 0,
                getcwd_attempts: Vec::new(),
                getcwd_syscalls: 0,
                cwd_change_attempts: Vec::new(),
                cwd_change_syscalls: 0,
                path_removal_attempts: Vec::new(),
                path_removal_syscalls: 0,
                directory_creation_attempts: Vec::new(),
                directory_creation_syscalls: 0,
                path_rename_attempts: Vec::new(),
                path_rename_syscalls: 0,
                path_mode_change_attempts: Vec::new(),
                path_mode_change_syscalls: 0,
                descriptor_timestamp_change_attempts: Vec::new(),
                descriptor_timestamp_change_syscalls: 0,
                symbolic_link_creation_attempts: Vec::new(),
                symbolic_link_creation_syscalls: 0,
                filesystem_status_attempts: Vec::new(),
                filesystem_status_syscalls: 0,
                read_attempts: Vec::new(),
                read_syscalls: 0,
                getdents64_attempts: Vec::new(),
                getdents64_syscalls: 0,
                mmap_attempts: Vec::new(),
                mmap_syscalls: 0,
                write_attempts: Vec::new(),
                write_syscalls: 0,
                unmodeled_syscall_attempts: Vec::new(),
                unmodeled_syscalls: 0,
                syscall_entries: 1,
                syscall_exits: 0,
                terminal_syscalls: 1,
                outcome: CapturedProcessOutcome::Signaled(libc::SIGKILL),
            }],
        )
        .unwrap();
        assert_eq!(receipt.readlink_syscalls().unwrap(), 1);

        let mut missing_terminal = receipt;
        missing_terminal.processes[0].terminal_syscalls = 0;
        assert!(missing_terminal.validate_complete().is_err());
    }

    #[test]
    fn validates_terminal_access_coverage() {
        let receipt = ProcessCaptureReceipt::new(
            1,
            vec![CapturedProcess {
                id: 1,
                parent: None,
                invocations: vec![fixture_invocation(b"/initial")],
                exec_attempts: Vec::new(),
                exec_syscalls: 0,
                open_attempts: Vec::new(),
                open_syscalls: 0,
                newfstatat_attempts: Vec::new(),
                newfstatat_syscalls: 0,
                fstat_attempts: Vec::new(),
                fstat_syscalls: 0,
                getrandom_attempts: Vec::new(),
                getrandom_syscalls: 0,
                runtime_observation_attempts: Vec::new(),
                runtime_observation_syscalls: 0,
                prlimit_attempts: Vec::new(),
                prlimit_syscalls: 0,
                close_attempts: Vec::new(),
                close_syscalls: 0,
                seek_attempts: Vec::new(),
                seek_syscalls: 0,
                file_advice_attempts: Vec::new(),
                file_advice_syscalls: 0,
                fcntl_attempts: Vec::new(),
                fcntl_syscalls: 0,
                ioctl_attempts: Vec::new(),
                ioctl_syscalls: 0,
                pipe_attempts: Vec::new(),
                pipe_syscalls: 0,
                epoll_create_attempts: Vec::new(),
                epoll_create_syscalls: 0,
                socket_create_attempts: Vec::new(),
                socket_create_syscalls: 0,
                socket_connect_attempts: Vec::new(),
                socket_connect_syscalls: 0,
                dup2_attempts: Vec::new(),
                dup2_syscalls: 0,
                signal_state_attempts: Vec::new(),
                signal_state_syscalls: 0,
                signal_deliveries: Vec::new(),
                umask_attempts: Vec::new(),
                umask_syscalls: 0,
                credential_attempts: Vec::new(),
                credential_syscalls: 0,
                futex_wake_attempts: Vec::new(),
                process_creation_attempts: Vec::new(),
                futex_wake_syscalls: 0,
                process_creation_syscalls: 0,
                child_wait_attempts: Vec::new(),
                child_wait_syscalls: 0,
                process_identity_attempts: Vec::new(),
                process_identity_syscalls: 0,
                memory_state_attempts: Vec::new(),
                memory_state_syscalls: 0,
                thread_state_attempts: Vec::new(),
                thread_state_syscalls: 0,
                lifecycle_exit_attempts: Vec::new(),
                lifecycle_exit_syscalls: 0,
                statx_attempts: Vec::new(),
                statx_syscalls: 0,
                xattr_attempts: Vec::new(),
                xattr_syscalls: 0,
                readlink_attempts: Vec::new(),
                readlink_syscalls: 0,
                access_attempts: vec![CapturedAccessAttempt {
                    syscall_index: 0,
                    request: CapturedAccessRequest {
                        system_call: CapturedAccessSystemCall::Faccessat2,
                        directory_descriptor: libc::AT_FDCWD,
                        path: b"/fixture/value".to_vec(),
                        path_is_null: false,
                        base: None,
                        mode: libc::F_OK as u32,
                        flags: 0,
                    },
                    outcome: CapturedAccessOutcome::Terminated,
                }],
                access_syscalls: 1,
                getcwd_attempts: Vec::new(),
                getcwd_syscalls: 0,
                cwd_change_attempts: Vec::new(),
                cwd_change_syscalls: 0,
                path_removal_attempts: Vec::new(),
                path_removal_syscalls: 0,
                directory_creation_attempts: Vec::new(),
                directory_creation_syscalls: 0,
                path_rename_attempts: Vec::new(),
                path_rename_syscalls: 0,
                path_mode_change_attempts: Vec::new(),
                path_mode_change_syscalls: 0,
                descriptor_timestamp_change_attempts: Vec::new(),
                descriptor_timestamp_change_syscalls: 0,
                symbolic_link_creation_attempts: Vec::new(),
                symbolic_link_creation_syscalls: 0,
                filesystem_status_attempts: Vec::new(),
                filesystem_status_syscalls: 0,
                read_attempts: Vec::new(),
                read_syscalls: 0,
                getdents64_attempts: Vec::new(),
                getdents64_syscalls: 0,
                mmap_attempts: Vec::new(),
                mmap_syscalls: 0,
                write_attempts: Vec::new(),
                write_syscalls: 0,
                unmodeled_syscall_attempts: Vec::new(),
                unmodeled_syscalls: 0,
                syscall_entries: 1,
                syscall_exits: 0,
                terminal_syscalls: 1,
                outcome: CapturedProcessOutcome::Signaled(libc::SIGKILL),
            }],
        )
        .unwrap();
        assert_eq!(receipt.access_syscalls().unwrap(), 1);

        let mut missing_terminal = receipt;
        missing_terminal.processes[0].terminal_syscalls = 0;
        assert!(missing_terminal.validate_complete().is_err());
    }

    #[test]
    fn validates_terminal_getcwd_coverage() {
        let receipt = ProcessCaptureReceipt::new(
            1,
            vec![CapturedProcess {
                id: 1,
                parent: None,
                invocations: vec![fixture_invocation(b"/initial")],
                exec_attempts: Vec::new(),
                exec_syscalls: 0,
                open_attempts: Vec::new(),
                open_syscalls: 0,
                newfstatat_attempts: Vec::new(),
                newfstatat_syscalls: 0,
                fstat_attempts: Vec::new(),
                fstat_syscalls: 0,
                getrandom_attempts: Vec::new(),
                getrandom_syscalls: 0,
                runtime_observation_attempts: Vec::new(),
                runtime_observation_syscalls: 0,
                prlimit_attempts: Vec::new(),
                prlimit_syscalls: 0,
                close_attempts: Vec::new(),
                close_syscalls: 0,
                seek_attempts: Vec::new(),
                seek_syscalls: 0,
                file_advice_attempts: Vec::new(),
                file_advice_syscalls: 0,
                fcntl_attempts: Vec::new(),
                fcntl_syscalls: 0,
                ioctl_attempts: Vec::new(),
                ioctl_syscalls: 0,
                pipe_attempts: Vec::new(),
                pipe_syscalls: 0,
                epoll_create_attempts: Vec::new(),
                epoll_create_syscalls: 0,
                socket_create_attempts: Vec::new(),
                socket_create_syscalls: 0,
                socket_connect_attempts: Vec::new(),
                socket_connect_syscalls: 0,
                dup2_attempts: Vec::new(),
                dup2_syscalls: 0,
                signal_state_attempts: Vec::new(),
                signal_state_syscalls: 0,
                signal_deliveries: Vec::new(),
                umask_attempts: Vec::new(),
                umask_syscalls: 0,
                credential_attempts: Vec::new(),
                credential_syscalls: 0,
                futex_wake_attempts: Vec::new(),
                process_creation_attempts: Vec::new(),
                futex_wake_syscalls: 0,
                process_creation_syscalls: 0,
                child_wait_attempts: Vec::new(),
                child_wait_syscalls: 0,
                process_identity_attempts: Vec::new(),
                process_identity_syscalls: 0,
                memory_state_attempts: Vec::new(),
                memory_state_syscalls: 0,
                thread_state_attempts: Vec::new(),
                thread_state_syscalls: 0,
                lifecycle_exit_attempts: Vec::new(),
                lifecycle_exit_syscalls: 0,
                statx_attempts: Vec::new(),
                statx_syscalls: 0,
                xattr_attempts: Vec::new(),
                xattr_syscalls: 0,
                readlink_attempts: Vec::new(),
                readlink_syscalls: 0,
                access_attempts: Vec::new(),
                access_syscalls: 0,
                getcwd_attempts: vec![CapturedGetcwdAttempt {
                    syscall_index: 0,
                    request: CapturedGetcwdRequest {
                        requested_bytes: 4096,
                    },
                    outcome: CapturedGetcwdOutcome::Terminated,
                }],
                getcwd_syscalls: 1,
                cwd_change_attempts: Vec::new(),
                cwd_change_syscalls: 0,
                path_removal_attempts: Vec::new(),
                path_removal_syscalls: 0,
                directory_creation_attempts: Vec::new(),
                directory_creation_syscalls: 0,
                path_rename_attempts: Vec::new(),
                path_rename_syscalls: 0,
                path_mode_change_attempts: Vec::new(),
                path_mode_change_syscalls: 0,
                descriptor_timestamp_change_attempts: Vec::new(),
                descriptor_timestamp_change_syscalls: 0,
                symbolic_link_creation_attempts: Vec::new(),
                symbolic_link_creation_syscalls: 0,
                filesystem_status_attempts: Vec::new(),
                filesystem_status_syscalls: 0,
                read_attempts: Vec::new(),
                read_syscalls: 0,
                getdents64_attempts: Vec::new(),
                getdents64_syscalls: 0,
                mmap_attempts: Vec::new(),
                mmap_syscalls: 0,
                write_attempts: Vec::new(),
                write_syscalls: 0,
                unmodeled_syscall_attempts: Vec::new(),
                unmodeled_syscalls: 0,
                syscall_entries: 1,
                syscall_exits: 0,
                terminal_syscalls: 1,
                outcome: CapturedProcessOutcome::Signaled(libc::SIGKILL),
            }],
        )
        .unwrap();
        assert_eq!(receipt.getcwd_syscalls().unwrap(), 1);

        let mut missing_terminal = receipt;
        missing_terminal.processes[0].terminal_syscalls = 0;
        assert!(missing_terminal.validate_complete().is_err());
    }

    #[test]
    fn validates_getcwd_result_shape_and_capture_bound() {
        let unreachable = CapturedGetcwdAttempt {
            syscall_index: 0,
            request: CapturedGetcwdRequest {
                requested_bytes: 4096,
            },
            outcome: CapturedGetcwdOutcome::Directory(b"(unreachable)/fixture".to_vec()),
        };
        assert!(!validate_captured_getcwd_attempt(&unreachable).unwrap());

        let mut maximum = vec![b'a'; LINUX_GETCWD_RESULT_LIMIT - 1];
        maximum[0] = b'/';
        let maximum = CapturedGetcwdAttempt {
            syscall_index: 0,
            request: CapturedGetcwdRequest {
                requested_bytes: LINUX_GETCWD_RESULT_LIMIT as u64,
            },
            outcome: CapturedGetcwdOutcome::Directory(maximum),
        };
        assert!(!validate_captured_getcwd_attempt(&maximum).unwrap());

        let mut short_request = maximum.clone();
        short_request.request.requested_bytes -= 1;
        assert!(validate_captured_getcwd_attempt(&short_request).is_err());

        let mut oversized = maximum;
        let CapturedGetcwdOutcome::Directory(directory) = &mut oversized.outcome else {
            unreachable!();
        };
        directory.push(b'a');
        oversized.request.requested_bytes += 1;
        assert!(validate_captured_getcwd_attempt(&oversized).is_err());
    }

    #[test]
    fn validates_terminal_cwd_change_coverage() {
        let receipt = ProcessCaptureReceipt::new(
            1,
            vec![CapturedProcess {
                id: 1,
                parent: None,
                invocations: vec![fixture_invocation(b"/initial")],
                exec_attempts: Vec::new(),
                exec_syscalls: 0,
                open_attempts: Vec::new(),
                open_syscalls: 0,
                newfstatat_attempts: Vec::new(),
                newfstatat_syscalls: 0,
                fstat_attempts: Vec::new(),
                fstat_syscalls: 0,
                getrandom_attempts: Vec::new(),
                getrandom_syscalls: 0,
                runtime_observation_attempts: Vec::new(),
                runtime_observation_syscalls: 0,
                prlimit_attempts: Vec::new(),
                prlimit_syscalls: 0,
                close_attempts: Vec::new(),
                close_syscalls: 0,
                seek_attempts: Vec::new(),
                seek_syscalls: 0,
                file_advice_attempts: Vec::new(),
                file_advice_syscalls: 0,
                fcntl_attempts: Vec::new(),
                fcntl_syscalls: 0,
                ioctl_attempts: Vec::new(),
                ioctl_syscalls: 0,
                pipe_attempts: Vec::new(),
                pipe_syscalls: 0,
                epoll_create_attempts: Vec::new(),
                epoll_create_syscalls: 0,
                socket_create_attempts: Vec::new(),
                socket_create_syscalls: 0,
                socket_connect_attempts: Vec::new(),
                socket_connect_syscalls: 0,
                dup2_attempts: Vec::new(),
                dup2_syscalls: 0,
                signal_state_attempts: Vec::new(),
                signal_state_syscalls: 0,
                signal_deliveries: Vec::new(),
                umask_attempts: Vec::new(),
                umask_syscalls: 0,
                credential_attempts: Vec::new(),
                credential_syscalls: 0,
                futex_wake_attempts: Vec::new(),
                process_creation_attempts: Vec::new(),
                futex_wake_syscalls: 0,
                process_creation_syscalls: 0,
                child_wait_attempts: Vec::new(),
                child_wait_syscalls: 0,
                process_identity_attempts: Vec::new(),
                process_identity_syscalls: 0,
                memory_state_attempts: Vec::new(),
                memory_state_syscalls: 0,
                thread_state_attempts: Vec::new(),
                thread_state_syscalls: 0,
                lifecycle_exit_attempts: Vec::new(),
                lifecycle_exit_syscalls: 0,
                statx_attempts: Vec::new(),
                statx_syscalls: 0,
                xattr_attempts: Vec::new(),
                xattr_syscalls: 0,
                readlink_attempts: Vec::new(),
                readlink_syscalls: 0,
                access_attempts: Vec::new(),
                access_syscalls: 0,
                getcwd_attempts: Vec::new(),
                getcwd_syscalls: 0,
                cwd_change_attempts: vec![CapturedCwdChangeAttempt {
                    syscall_index: 0,
                    request: CapturedCwdChangeRequest {
                        system_call: CapturedCwdChangeSystemCall::Chdir,
                        path: b"next".to_vec(),
                        path_is_null: false,
                        directory_descriptor: libc::AT_FDCWD,
                        descriptor_target: None,
                        prior_working_directory: b"/initial".to_vec(),
                    },
                    outcome: CapturedCwdChangeOutcome::Terminated,
                }],
                cwd_change_syscalls: 1,
                path_removal_attempts: Vec::new(),
                path_removal_syscalls: 0,
                directory_creation_attempts: Vec::new(),
                directory_creation_syscalls: 0,
                path_rename_attempts: Vec::new(),
                path_rename_syscalls: 0,
                path_mode_change_attempts: Vec::new(),
                path_mode_change_syscalls: 0,
                descriptor_timestamp_change_attempts: Vec::new(),
                descriptor_timestamp_change_syscalls: 0,
                symbolic_link_creation_attempts: Vec::new(),
                symbolic_link_creation_syscalls: 0,
                filesystem_status_attempts: Vec::new(),
                filesystem_status_syscalls: 0,
                read_attempts: Vec::new(),
                read_syscalls: 0,
                getdents64_attempts: Vec::new(),
                getdents64_syscalls: 0,
                mmap_attempts: Vec::new(),
                mmap_syscalls: 0,
                write_attempts: Vec::new(),
                write_syscalls: 0,
                unmodeled_syscall_attempts: Vec::new(),
                unmodeled_syscalls: 0,
                syscall_entries: 1,
                syscall_exits: 0,
                terminal_syscalls: 1,
                outcome: CapturedProcessOutcome::Signaled(libc::SIGKILL),
            }],
        )
        .unwrap();
        assert_eq!(receipt.cwd_change_syscalls().unwrap(), 1);

        let mut missing_terminal = receipt;
        missing_terminal.processes[0].terminal_syscalls = 0;
        assert!(missing_terminal.validate_complete().is_err());
    }

    #[test]
    fn validates_cwd_change_path_limits_and_fchdir_targets() {
        let mut maximum = vec![b'a'; LINUX_PATH_MAX - 1];
        maximum[0] = b'/';
        let chdir = CapturedCwdChangeAttempt {
            syscall_index: 0,
            request: CapturedCwdChangeRequest {
                system_call: CapturedCwdChangeSystemCall::Chdir,
                path: maximum.clone(),
                path_is_null: false,
                directory_descriptor: libc::AT_FDCWD,
                descriptor_target: None,
                prior_working_directory: b"/prior".to_vec(),
            },
            outcome: CapturedCwdChangeOutcome::Changed(maximum.clone()),
        };
        assert!(!validate_captured_cwd_change_attempt(&chdir).unwrap());

        let mut oversized_path = chdir.clone();
        oversized_path.request.path.push(b'a');
        assert!(validate_captured_cwd_change_attempt(&oversized_path).is_err());
        let mut oversized_result = chdir.clone();
        let CapturedCwdChangeOutcome::Changed(directory) = &mut oversized_result.outcome else {
            unreachable!();
        };
        directory.push(b'a');
        assert!(validate_captured_cwd_change_attempt(&oversized_result).is_err());

        let fchdir = CapturedCwdChangeAttempt {
            syscall_index: 0,
            request: CapturedCwdChangeRequest {
                system_call: CapturedCwdChangeSystemCall::Fchdir,
                path: Vec::new(),
                path_is_null: false,
                directory_descriptor: 7,
                descriptor_target: Some(maximum.clone()),
                prior_working_directory: b"/prior".to_vec(),
            },
            outcome: CapturedCwdChangeOutcome::Changed(maximum),
        };
        assert!(!validate_captured_cwd_change_attempt(&fchdir).unwrap());
        let mut missing_target = fchdir.clone();
        missing_target.request.descriptor_target = None;
        assert!(validate_captured_cwd_change_attempt(&missing_target).is_err());
        let mut relative_target = fchdir.clone();
        relative_target.request.descriptor_target = Some(b"relative".to_vec());
        assert!(validate_captured_cwd_change_attempt(&relative_target).is_err());
        let mut negative_target = fchdir;
        negative_target.request.directory_descriptor = -1;
        assert!(validate_captured_cwd_change_attempt(&negative_target).is_err());
    }

    #[test]
    fn validates_terminal_path_removal_coverage() {
        let receipt = ProcessCaptureReceipt::new(
            1,
            vec![CapturedProcess {
                id: 1,
                parent: None,
                invocations: vec![fixture_invocation(b"/initial")],
                exec_attempts: Vec::new(),
                exec_syscalls: 0,
                open_attempts: Vec::new(),
                open_syscalls: 0,
                newfstatat_attempts: Vec::new(),
                newfstatat_syscalls: 0,
                fstat_attempts: Vec::new(),
                fstat_syscalls: 0,
                getrandom_attempts: Vec::new(),
                getrandom_syscalls: 0,
                runtime_observation_attempts: Vec::new(),
                runtime_observation_syscalls: 0,
                prlimit_attempts: Vec::new(),
                prlimit_syscalls: 0,
                close_attempts: Vec::new(),
                close_syscalls: 0,
                seek_attempts: Vec::new(),
                seek_syscalls: 0,
                file_advice_attempts: Vec::new(),
                file_advice_syscalls: 0,
                fcntl_attempts: Vec::new(),
                fcntl_syscalls: 0,
                ioctl_attempts: Vec::new(),
                ioctl_syscalls: 0,
                pipe_attempts: Vec::new(),
                pipe_syscalls: 0,
                epoll_create_attempts: Vec::new(),
                epoll_create_syscalls: 0,
                socket_create_attempts: Vec::new(),
                socket_create_syscalls: 0,
                socket_connect_attempts: Vec::new(),
                socket_connect_syscalls: 0,
                dup2_attempts: Vec::new(),
                dup2_syscalls: 0,
                signal_state_attempts: Vec::new(),
                signal_state_syscalls: 0,
                signal_deliveries: Vec::new(),
                umask_attempts: Vec::new(),
                umask_syscalls: 0,
                credential_attempts: Vec::new(),
                credential_syscalls: 0,
                futex_wake_attempts: Vec::new(),
                process_creation_attempts: Vec::new(),
                futex_wake_syscalls: 0,
                process_creation_syscalls: 0,
                child_wait_attempts: Vec::new(),
                child_wait_syscalls: 0,
                process_identity_attempts: Vec::new(),
                process_identity_syscalls: 0,
                memory_state_attempts: Vec::new(),
                memory_state_syscalls: 0,
                thread_state_attempts: Vec::new(),
                thread_state_syscalls: 0,
                lifecycle_exit_attempts: Vec::new(),
                lifecycle_exit_syscalls: 0,
                statx_attempts: Vec::new(),
                statx_syscalls: 0,
                xattr_attempts: Vec::new(),
                xattr_syscalls: 0,
                readlink_attempts: Vec::new(),
                readlink_syscalls: 0,
                access_attempts: Vec::new(),
                access_syscalls: 0,
                getcwd_attempts: Vec::new(),
                getcwd_syscalls: 0,
                cwd_change_attempts: Vec::new(),
                cwd_change_syscalls: 0,
                path_removal_attempts: vec![CapturedPathRemovalAttempt {
                    syscall_index: 0,
                    request: CapturedPathRemovalRequest {
                        system_call: CapturedPathRemovalSystemCall::UnlinkAt,
                        directory_descriptor: libc::AT_FDCWD,
                        path: b"obsolete".to_vec(),
                        path_is_null: false,
                        base: Some(b"/initial".to_vec()),
                        flags: 0,
                    },
                    outcome: CapturedPathRemovalOutcome::Terminated,
                }],
                path_removal_syscalls: 1,
                directory_creation_attempts: Vec::new(),
                directory_creation_syscalls: 0,
                path_rename_attempts: Vec::new(),
                path_rename_syscalls: 0,
                path_mode_change_attempts: Vec::new(),
                path_mode_change_syscalls: 0,
                descriptor_timestamp_change_attempts: Vec::new(),
                descriptor_timestamp_change_syscalls: 0,
                symbolic_link_creation_attempts: Vec::new(),
                symbolic_link_creation_syscalls: 0,
                filesystem_status_attempts: Vec::new(),
                filesystem_status_syscalls: 0,
                read_attempts: Vec::new(),
                read_syscalls: 0,
                getdents64_attempts: Vec::new(),
                getdents64_syscalls: 0,
                mmap_attempts: Vec::new(),
                mmap_syscalls: 0,
                write_attempts: Vec::new(),
                write_syscalls: 0,
                unmodeled_syscall_attempts: Vec::new(),
                unmodeled_syscalls: 0,
                syscall_entries: 1,
                syscall_exits: 0,
                terminal_syscalls: 1,
                outcome: CapturedProcessOutcome::Signaled(libc::SIGKILL),
            }],
        )
        .unwrap();
        assert_eq!(receipt.path_removal_syscalls().unwrap(), 1);

        let mut missing_terminal = receipt;
        missing_terminal.processes[0].terminal_syscalls = 0;
        assert!(missing_terminal.validate_complete().is_err());
    }

    #[test]
    fn validates_path_removal_shapes_and_path_limits() {
        let removal = CapturedPathRemovalAttempt {
            syscall_index: 0,
            request: CapturedPathRemovalRequest {
                system_call: CapturedPathRemovalSystemCall::UnlinkAt,
                directory_descriptor: libc::AT_FDCWD,
                path: vec![b'a'; LINUX_PATH_MAX - 1],
                path_is_null: false,
                base: Some(b"/base".to_vec()),
                flags: LINUX_AT_REMOVEDIR,
            },
            outcome: CapturedPathRemovalOutcome::Removed,
        };
        assert!(!validate_captured_path_removal_attempt(&removal).unwrap());

        let mut oversized_path = removal.clone();
        oversized_path.request.path.push(b'a');
        oversized_path.outcome = CapturedPathRemovalOutcome::Failed(libc::ENAMETOOLONG);
        assert!(!validate_captured_path_removal_attempt(&oversized_path).unwrap());
        oversized_path.request.path.push(b'a');
        assert!(validate_captured_path_removal_attempt(&oversized_path).is_err());
        let mut oversized_base = removal.clone();
        oversized_base.request.base = Some(vec![b'/'; LINUX_PATH_MAX]);
        assert!(validate_captured_path_removal_attempt(&oversized_base).is_err());
        let mut missing_base = removal.clone();
        missing_base.request.base = None;
        assert!(validate_captured_path_removal_attempt(&missing_base).is_err());
        let mut null_success = removal.clone();
        null_success.request.path.clear();
        null_success.request.path_is_null = true;
        assert!(validate_captured_path_removal_attempt(&null_success).is_err());
        let mut bad_flags = removal.clone();
        bad_flags.request.flags = 0x4000_0000;
        assert!(validate_captured_path_removal_attempt(&bad_flags).is_err());
        bad_flags.outcome = CapturedPathRemovalOutcome::Failed(libc::EINVAL);
        assert!(!validate_captured_path_removal_attempt(&bad_flags).unwrap());
        let mut invalid_legacy = removal;
        invalid_legacy.request.system_call = CapturedPathRemovalSystemCall::Unlink;
        invalid_legacy.request.directory_descriptor = 7;
        invalid_legacy.request.flags = 0;
        invalid_legacy.outcome = CapturedPathRemovalOutcome::Failed(libc::ENOENT);
        assert!(validate_captured_path_removal_attempt(&invalid_legacy).is_err());
    }

    #[test]
    fn validates_terminal_directory_creation_coverage() {
        let receipt = ProcessCaptureReceipt::new(
            1,
            vec![CapturedProcess {
                id: 1,
                parent: None,
                invocations: vec![fixture_invocation(b"/initial")],
                exec_attempts: Vec::new(),
                exec_syscalls: 0,
                open_attempts: Vec::new(),
                open_syscalls: 0,
                newfstatat_attempts: Vec::new(),
                newfstatat_syscalls: 0,
                fstat_attempts: Vec::new(),
                fstat_syscalls: 0,
                getrandom_attempts: Vec::new(),
                getrandom_syscalls: 0,
                runtime_observation_attempts: Vec::new(),
                runtime_observation_syscalls: 0,
                prlimit_attempts: Vec::new(),
                prlimit_syscalls: 0,
                close_attempts: Vec::new(),
                close_syscalls: 0,
                seek_attempts: Vec::new(),
                seek_syscalls: 0,
                file_advice_attempts: Vec::new(),
                file_advice_syscalls: 0,
                fcntl_attempts: Vec::new(),
                fcntl_syscalls: 0,
                ioctl_attempts: Vec::new(),
                ioctl_syscalls: 0,
                pipe_attempts: Vec::new(),
                pipe_syscalls: 0,
                epoll_create_attempts: Vec::new(),
                epoll_create_syscalls: 0,
                socket_create_attempts: Vec::new(),
                socket_create_syscalls: 0,
                socket_connect_attempts: Vec::new(),
                socket_connect_syscalls: 0,
                dup2_attempts: Vec::new(),
                dup2_syscalls: 0,
                signal_state_attempts: Vec::new(),
                signal_state_syscalls: 0,
                signal_deliveries: Vec::new(),
                umask_attempts: Vec::new(),
                umask_syscalls: 0,
                credential_attempts: Vec::new(),
                credential_syscalls: 0,
                futex_wake_attempts: Vec::new(),
                process_creation_attempts: Vec::new(),
                futex_wake_syscalls: 0,
                process_creation_syscalls: 0,
                child_wait_attempts: Vec::new(),
                child_wait_syscalls: 0,
                process_identity_attempts: Vec::new(),
                process_identity_syscalls: 0,
                memory_state_attempts: Vec::new(),
                memory_state_syscalls: 0,
                thread_state_attempts: Vec::new(),
                thread_state_syscalls: 0,
                lifecycle_exit_attempts: Vec::new(),
                lifecycle_exit_syscalls: 0,
                statx_attempts: Vec::new(),
                statx_syscalls: 0,
                xattr_attempts: Vec::new(),
                xattr_syscalls: 0,
                readlink_attempts: Vec::new(),
                readlink_syscalls: 0,
                access_attempts: Vec::new(),
                access_syscalls: 0,
                getcwd_attempts: Vec::new(),
                getcwd_syscalls: 0,
                cwd_change_attempts: Vec::new(),
                cwd_change_syscalls: 0,
                path_removal_attempts: Vec::new(),
                path_removal_syscalls: 0,
                directory_creation_attempts: vec![CapturedDirectoryCreationAttempt {
                    syscall_index: 0,
                    request: CapturedDirectoryCreationRequest {
                        system_call: CapturedDirectoryCreationSystemCall::MkdirAt,
                        directory_descriptor: libc::AT_FDCWD,
                        path: b"created".to_vec(),
                        path_is_null: false,
                        base: Some(b"/initial".to_vec()),
                        mode: 0o755,
                    },
                    outcome: CapturedDirectoryCreationOutcome::Terminated,
                }],
                directory_creation_syscalls: 1,
                path_rename_attempts: Vec::new(),
                path_rename_syscalls: 0,
                path_mode_change_attempts: Vec::new(),
                path_mode_change_syscalls: 0,
                descriptor_timestamp_change_attempts: Vec::new(),
                descriptor_timestamp_change_syscalls: 0,
                symbolic_link_creation_attempts: Vec::new(),
                symbolic_link_creation_syscalls: 0,
                filesystem_status_attempts: Vec::new(),
                filesystem_status_syscalls: 0,
                read_attempts: Vec::new(),
                read_syscalls: 0,
                getdents64_attempts: Vec::new(),
                getdents64_syscalls: 0,
                mmap_attempts: Vec::new(),
                mmap_syscalls: 0,
                write_attempts: Vec::new(),
                write_syscalls: 0,
                unmodeled_syscall_attempts: Vec::new(),
                unmodeled_syscalls: 0,
                syscall_entries: 1,
                syscall_exits: 0,
                terminal_syscalls: 1,
                outcome: CapturedProcessOutcome::Signaled(libc::SIGKILL),
            }],
        )
        .unwrap();
        assert_eq!(receipt.directory_creation_syscalls().unwrap(), 1);

        let mut missing_terminal = receipt;
        missing_terminal.processes[0].terminal_syscalls = 0;
        assert!(missing_terminal.validate_complete().is_err());
    }

    #[test]
    fn validates_directory_creation_shapes_and_path_limits() {
        let creation = CapturedDirectoryCreationAttempt {
            syscall_index: 0,
            request: CapturedDirectoryCreationRequest {
                system_call: CapturedDirectoryCreationSystemCall::MkdirAt,
                directory_descriptor: libc::AT_FDCWD,
                path: vec![b'a'; LINUX_PATH_MAX - 1],
                path_is_null: false,
                base: Some(b"/base".to_vec()),
                mode: u32::MAX,
            },
            outcome: CapturedDirectoryCreationOutcome::Created,
        };
        assert!(!validate_captured_directory_creation_attempt(&creation).unwrap());

        let mut maximum_failure = creation.clone();
        maximum_failure.request.path.push(b'a');
        maximum_failure.outcome = CapturedDirectoryCreationOutcome::Failed(libc::ENAMETOOLONG);
        assert!(!validate_captured_directory_creation_attempt(&maximum_failure).unwrap());
        maximum_failure.request.path.push(b'a');
        assert!(validate_captured_directory_creation_attempt(&maximum_failure).is_err());
        let mut oversized_base = creation.clone();
        oversized_base.request.base = Some(vec![b'/'; LINUX_PATH_MAX]);
        assert!(validate_captured_directory_creation_attempt(&oversized_base).is_err());
        let mut missing_base = creation.clone();
        missing_base.request.base = None;
        assert!(validate_captured_directory_creation_attempt(&missing_base).is_err());
        let mut null_success = creation.clone();
        null_success.request.path.clear();
        null_success.request.path_is_null = true;
        assert!(validate_captured_directory_creation_attempt(&null_success).is_err());
        let mut invalid_legacy = creation;
        invalid_legacy.request.system_call = CapturedDirectoryCreationSystemCall::Mkdir;
        invalid_legacy.request.directory_descriptor = 7;
        invalid_legacy.outcome = CapturedDirectoryCreationOutcome::Failed(libc::ENOENT);
        assert!(validate_captured_directory_creation_attempt(&invalid_legacy).is_err());
    }

    #[test]
    fn validates_terminal_path_rename_coverage() {
        let receipt = ProcessCaptureReceipt::new(
            1,
            vec![CapturedProcess {
                id: 1,
                parent: None,
                invocations: vec![fixture_invocation(b"/initial")],
                exec_attempts: Vec::new(),
                exec_syscalls: 0,
                open_attempts: Vec::new(),
                open_syscalls: 0,
                newfstatat_attempts: Vec::new(),
                newfstatat_syscalls: 0,
                fstat_attempts: Vec::new(),
                fstat_syscalls: 0,
                getrandom_attempts: Vec::new(),
                getrandom_syscalls: 0,
                runtime_observation_attempts: Vec::new(),
                runtime_observation_syscalls: 0,
                prlimit_attempts: Vec::new(),
                prlimit_syscalls: 0,
                close_attempts: Vec::new(),
                close_syscalls: 0,
                seek_attempts: Vec::new(),
                seek_syscalls: 0,
                file_advice_attempts: Vec::new(),
                file_advice_syscalls: 0,
                fcntl_attempts: Vec::new(),
                fcntl_syscalls: 0,
                ioctl_attempts: Vec::new(),
                ioctl_syscalls: 0,
                pipe_attempts: Vec::new(),
                pipe_syscalls: 0,
                epoll_create_attempts: Vec::new(),
                epoll_create_syscalls: 0,
                socket_create_attempts: Vec::new(),
                socket_create_syscalls: 0,
                socket_connect_attempts: Vec::new(),
                socket_connect_syscalls: 0,
                dup2_attempts: Vec::new(),
                dup2_syscalls: 0,
                signal_state_attempts: Vec::new(),
                signal_state_syscalls: 0,
                signal_deliveries: Vec::new(),
                umask_attempts: Vec::new(),
                umask_syscalls: 0,
                credential_attempts: Vec::new(),
                credential_syscalls: 0,
                futex_wake_attempts: Vec::new(),
                process_creation_attempts: Vec::new(),
                futex_wake_syscalls: 0,
                process_creation_syscalls: 0,
                child_wait_attempts: Vec::new(),
                child_wait_syscalls: 0,
                process_identity_attempts: Vec::new(),
                process_identity_syscalls: 0,
                memory_state_attempts: Vec::new(),
                memory_state_syscalls: 0,
                thread_state_attempts: Vec::new(),
                thread_state_syscalls: 0,
                lifecycle_exit_attempts: Vec::new(),
                lifecycle_exit_syscalls: 0,
                statx_attempts: Vec::new(),
                statx_syscalls: 0,
                xattr_attempts: Vec::new(),
                xattr_syscalls: 0,
                readlink_attempts: Vec::new(),
                readlink_syscalls: 0,
                access_attempts: Vec::new(),
                access_syscalls: 0,
                getcwd_attempts: Vec::new(),
                getcwd_syscalls: 0,
                cwd_change_attempts: Vec::new(),
                cwd_change_syscalls: 0,
                path_removal_attempts: Vec::new(),
                path_removal_syscalls: 0,
                directory_creation_attempts: Vec::new(),
                directory_creation_syscalls: 0,
                path_rename_attempts: vec![CapturedPathRenameAttempt {
                    syscall_index: 0,
                    request: CapturedPathRenameRequest {
                        system_call: CapturedPathRenameSystemCall::RenameAt2,
                        old_directory_descriptor: libc::AT_FDCWD,
                        old_path: b"old".to_vec(),
                        old_path_is_null: false,
                        old_path_terminated: true,
                        old_base: Some(b"/initial".to_vec()),
                        new_directory_descriptor: libc::AT_FDCWD,
                        new_path: b"new".to_vec(),
                        new_path_is_null: false,
                        new_path_terminated: true,
                        new_base: Some(b"/initial".to_vec()),
                        flags: 0,
                    },
                    outcome: CapturedPathRenameOutcome::Terminated,
                }],
                path_rename_syscalls: 1,
                path_mode_change_attempts: Vec::new(),
                path_mode_change_syscalls: 0,
                descriptor_timestamp_change_attempts: Vec::new(),
                descriptor_timestamp_change_syscalls: 0,
                symbolic_link_creation_attempts: Vec::new(),
                symbolic_link_creation_syscalls: 0,
                filesystem_status_attempts: Vec::new(),
                filesystem_status_syscalls: 0,
                read_attempts: Vec::new(),
                read_syscalls: 0,
                getdents64_attempts: Vec::new(),
                getdents64_syscalls: 0,
                mmap_attempts: Vec::new(),
                mmap_syscalls: 0,
                write_attempts: Vec::new(),
                write_syscalls: 0,
                unmodeled_syscall_attempts: Vec::new(),
                unmodeled_syscalls: 0,
                syscall_entries: 1,
                syscall_exits: 0,
                terminal_syscalls: 1,
                outcome: CapturedProcessOutcome::Signaled(libc::SIGKILL),
            }],
        )
        .unwrap();
        assert_eq!(receipt.path_rename_syscalls().unwrap(), 1);

        let mut missing_terminal = receipt;
        missing_terminal.processes[0].terminal_syscalls = 0;
        assert!(missing_terminal.validate_complete().is_err());
    }

    #[test]
    fn validates_path_rename_operands_flags_and_success() {
        let completed = CapturedPathRenameAttempt {
            syscall_index: 0,
            request: CapturedPathRenameRequest {
                system_call: CapturedPathRenameSystemCall::RenameAt2,
                old_directory_descriptor: 3,
                old_path: b"old".to_vec(),
                old_path_is_null: false,
                old_path_terminated: true,
                old_base: Some(b"/old-base".to_vec()),
                new_directory_descriptor: 4,
                new_path: b"new".to_vec(),
                new_path_is_null: false,
                new_path_terminated: true,
                new_base: Some(b"/new-base".to_vec()),
                flags: LINUX_RENAME_NOREPLACE,
            },
            outcome: CapturedPathRenameOutcome::Completed,
        };
        assert!(!validate_captured_path_rename_attempt(&completed).unwrap());

        let mut long_base = completed.clone();
        let mut base = vec![b'a'; LINUX_PATH_MAX];
        base[0] = b'/';
        long_base.request.old_base = Some(base);
        assert!(!validate_captured_path_rename_attempt(&long_base).unwrap());

        let mut absolute = completed.clone();
        absolute.request.old_directory_descriptor = -1;
        absolute.request.old_path = b"/old".to_vec();
        absolute.request.old_base = None;
        absolute.request.new_directory_descriptor = -2;
        absolute.request.new_path = b"/new".to_vec();
        absolute.request.new_base = None;
        assert!(!validate_captured_path_rename_attempt(&absolute).unwrap());

        let mut unterminated = completed.clone();
        unterminated.request.old_path = vec![b'x'; LINUX_PATH_MAX];
        unterminated.request.old_path_terminated = false;
        unterminated.request.old_base = None;
        unterminated.outcome = CapturedPathRenameOutcome::Failed(libc::ENAMETOOLONG);
        assert!(!validate_captured_path_rename_attempt(&unterminated).unwrap());
        unterminated.request.old_path.pop();
        assert!(validate_captured_path_rename_attempt(&unterminated).is_err());

        let mut missing_base = completed.clone();
        missing_base.request.old_base = None;
        assert!(validate_captured_path_rename_attempt(&missing_base).is_err());
        let mut unknown_flags = completed.clone();
        unknown_flags.request.flags = 0x8000_0000;
        assert!(validate_captured_path_rename_attempt(&unknown_flags).is_err());
        let mut conflict = completed.clone();
        conflict.request.flags = LINUX_RENAME_NOREPLACE | LINUX_RENAME_EXCHANGE;
        assert!(validate_captured_path_rename_attempt(&conflict).is_err());
        let mut whiteout = completed.clone();
        whiteout.request.flags = LINUX_RENAME_NOREPLACE | LINUX_RENAME_WHITEOUT;
        assert!(!validate_captured_path_rename_attempt(&whiteout).unwrap());
        let mut exchange_whiteout = completed.clone();
        exchange_whiteout.request.flags = LINUX_RENAME_EXCHANGE | LINUX_RENAME_WHITEOUT;
        assert!(validate_captured_path_rename_attempt(&exchange_whiteout).is_err());
        let mut invalid_legacy = completed.clone();
        invalid_legacy.request.system_call = CapturedPathRenameSystemCall::Rename;
        invalid_legacy.outcome = CapturedPathRenameOutcome::Failed(libc::EINVAL);
        assert!(validate_captured_path_rename_attempt(&invalid_legacy).is_err());
        let mut null_failure = completed.clone();
        null_failure.request.old_path.clear();
        null_failure.request.old_path_is_null = true;
        null_failure.request.old_path_terminated = false;
        null_failure.request.old_base = None;
        null_failure.outcome = CapturedPathRenameOutcome::Failed(libc::EFAULT);
        assert!(!validate_captured_path_rename_attempt(&null_failure).unwrap());
        let mut zero_errno = completed;
        zero_errno.outcome = CapturedPathRenameOutcome::Failed(0);
        assert!(validate_captured_path_rename_attempt(&zero_errno).is_err());
    }

    #[test]
    fn validates_path_mode_change_shapes_and_bounds() {
        let changed = CapturedPathModeChangeAttempt {
            syscall_index: 0,
            request: CapturedPathModeChangeRequest {
                system_call: CapturedPathModeChangeSystemCall::FchmodAt,
                descriptor: None,
                descriptor_identity: None,
                descriptor_mount_id: None,
                directory_descriptor: 3,
                path: b"value".to_vec(),
                path_is_null: false,
                path_terminated: true,
                base: Some(b"/base".to_vec()),
                mode: u32::MAX,
            },
            outcome: CapturedPathModeChangeOutcome::Changed,
        };
        assert!(!validate_captured_path_mode_change_attempt(&changed).unwrap());

        let mut long_base = changed.clone();
        let mut base = vec![b'a'; LINUX_PATH_MAX];
        base[0] = b'/';
        long_base.request.base = Some(base);
        assert!(!validate_captured_path_mode_change_attempt(&long_base).unwrap());

        let mut absolute = changed.clone();
        absolute.request.directory_descriptor = -1;
        absolute.request.path = b"/value".to_vec();
        absolute.request.base = None;
        assert!(!validate_captured_path_mode_change_attempt(&absolute).unwrap());

        let mut unterminated = changed.clone();
        unterminated.request.path = vec![b'x'; LINUX_PATH_MAX];
        unterminated.request.path_terminated = false;
        unterminated.request.base = None;
        unterminated.outcome = CapturedPathModeChangeOutcome::Failed(libc::ENAMETOOLONG);
        assert!(!validate_captured_path_mode_change_attempt(&unterminated).unwrap());
        unterminated.request.path.pop();
        assert!(validate_captured_path_mode_change_attempt(&unterminated).is_err());

        let mut missing_base = changed.clone();
        missing_base.request.base = None;
        assert!(validate_captured_path_mode_change_attempt(&missing_base).is_err());
        let mut invalid_legacy = changed.clone();
        invalid_legacy.request.system_call = CapturedPathModeChangeSystemCall::Chmod;
        invalid_legacy.outcome = CapturedPathModeChangeOutcome::Failed(libc::ENOENT);
        assert!(validate_captured_path_mode_change_attempt(&invalid_legacy).is_err());
        let mut null_failure = changed.clone();
        null_failure.request.path.clear();
        null_failure.request.path_is_null = true;
        null_failure.request.path_terminated = false;
        null_failure.request.base = None;
        null_failure.outcome = CapturedPathModeChangeOutcome::Failed(libc::EFAULT);
        assert!(!validate_captured_path_mode_change_attempt(&null_failure).unwrap());
        let mut zero_errno = changed;
        zero_errno.outcome = CapturedPathModeChangeOutcome::Failed(0);
        assert!(validate_captured_path_mode_change_attempt(&zero_errno).is_err());

        let descriptor_identity = CapturedDescriptorIdentity {
            target: b"/output/value".to_vec(),
            device: 1,
            inode: 2,
            kind: CapturedDescriptorKind::RegularFile,
        };
        let descriptor_changed = CapturedPathModeChangeAttempt {
            syscall_index: 1,
            request: CapturedPathModeChangeRequest {
                system_call: CapturedPathModeChangeSystemCall::Fchmod,
                descriptor: Some(5),
                descriptor_identity: Some(descriptor_identity),
                descriptor_mount_id: Some(3),
                directory_descriptor: libc::AT_FDCWD,
                path: Vec::new(),
                path_is_null: true,
                path_terminated: false,
                base: None,
                mode: 0o100600,
            },
            outcome: CapturedPathModeChangeOutcome::Changed,
        };
        assert!(!validate_captured_path_mode_change_attempt(&descriptor_changed).unwrap());
        let mut missing_identity = descriptor_changed.clone();
        missing_identity.request.descriptor_identity = None;
        missing_identity.request.descriptor_mount_id = None;
        assert!(validate_captured_path_mode_change_attempt(&missing_identity).is_err());
        let mut mismatched_mount = descriptor_changed.clone();
        mismatched_mount.request.descriptor_mount_id = None;
        assert!(validate_captured_path_mode_change_attempt(&mismatched_mount).is_err());
        let mut forged_path = descriptor_changed.clone();
        forged_path.request.path = b"value".to_vec();
        assert!(validate_captured_path_mode_change_attempt(&forged_path).is_err());
        let mut invalid_descriptor = descriptor_changed;
        invalid_descriptor.request.descriptor = Some(-1);
        invalid_descriptor.request.descriptor_identity = None;
        invalid_descriptor.request.descriptor_mount_id = None;
        invalid_descriptor.outcome = CapturedPathModeChangeOutcome::Failed(libc::EBADF);
        assert!(!validate_captured_path_mode_change_attempt(&invalid_descriptor).unwrap());
    }

    #[test]
    fn validates_terminal_path_mode_change_coverage() {
        let receipt = ProcessCaptureReceipt::new(
            1,
            vec![CapturedProcess {
                id: 1,
                parent: None,
                invocations: vec![fixture_invocation(b"/initial")],
                exec_attempts: Vec::new(),
                exec_syscalls: 0,
                open_attempts: Vec::new(),
                open_syscalls: 0,
                newfstatat_attempts: Vec::new(),
                newfstatat_syscalls: 0,
                fstat_attempts: Vec::new(),
                fstat_syscalls: 0,
                getrandom_attempts: Vec::new(),
                getrandom_syscalls: 0,
                runtime_observation_attempts: Vec::new(),
                runtime_observation_syscalls: 0,
                prlimit_attempts: Vec::new(),
                prlimit_syscalls: 0,
                close_attempts: Vec::new(),
                close_syscalls: 0,
                seek_attempts: Vec::new(),
                seek_syscalls: 0,
                file_advice_attempts: Vec::new(),
                file_advice_syscalls: 0,
                fcntl_attempts: Vec::new(),
                fcntl_syscalls: 0,
                ioctl_attempts: Vec::new(),
                ioctl_syscalls: 0,
                pipe_attempts: Vec::new(),
                pipe_syscalls: 0,
                epoll_create_attempts: Vec::new(),
                epoll_create_syscalls: 0,
                socket_create_attempts: Vec::new(),
                socket_create_syscalls: 0,
                socket_connect_attempts: Vec::new(),
                socket_connect_syscalls: 0,
                dup2_attempts: Vec::new(),
                dup2_syscalls: 0,
                signal_state_attempts: Vec::new(),
                signal_state_syscalls: 0,
                signal_deliveries: Vec::new(),
                umask_attempts: Vec::new(),
                umask_syscalls: 0,
                credential_attempts: Vec::new(),
                credential_syscalls: 0,
                futex_wake_attempts: Vec::new(),
                process_creation_attempts: Vec::new(),
                futex_wake_syscalls: 0,
                process_creation_syscalls: 0,
                child_wait_attempts: Vec::new(),
                child_wait_syscalls: 0,
                process_identity_attempts: Vec::new(),
                process_identity_syscalls: 0,
                memory_state_attempts: Vec::new(),
                memory_state_syscalls: 0,
                thread_state_attempts: Vec::new(),
                thread_state_syscalls: 0,
                lifecycle_exit_attempts: Vec::new(),
                lifecycle_exit_syscalls: 0,
                statx_attempts: Vec::new(),
                statx_syscalls: 0,
                xattr_attempts: Vec::new(),
                xattr_syscalls: 0,
                readlink_attempts: Vec::new(),
                readlink_syscalls: 0,
                access_attempts: Vec::new(),
                access_syscalls: 0,
                getcwd_attempts: Vec::new(),
                getcwd_syscalls: 0,
                cwd_change_attempts: Vec::new(),
                cwd_change_syscalls: 0,
                path_removal_attempts: Vec::new(),
                path_removal_syscalls: 0,
                directory_creation_attempts: Vec::new(),
                directory_creation_syscalls: 0,
                path_rename_attempts: Vec::new(),
                path_rename_syscalls: 0,
                path_mode_change_attempts: vec![CapturedPathModeChangeAttempt {
                    syscall_index: 0,
                    request: CapturedPathModeChangeRequest {
                        system_call: CapturedPathModeChangeSystemCall::FchmodAt,
                        descriptor: None,
                        descriptor_identity: None,
                        descriptor_mount_id: None,
                        directory_descriptor: libc::AT_FDCWD,
                        path: b"value".to_vec(),
                        path_is_null: false,
                        path_terminated: true,
                        base: Some(b"/initial".to_vec()),
                        mode: 0o600,
                    },
                    outcome: CapturedPathModeChangeOutcome::Terminated,
                }],
                path_mode_change_syscalls: 1,
                descriptor_timestamp_change_attempts: Vec::new(),
                descriptor_timestamp_change_syscalls: 0,
                symbolic_link_creation_attempts: Vec::new(),
                symbolic_link_creation_syscalls: 0,
                filesystem_status_attempts: Vec::new(),
                filesystem_status_syscalls: 0,
                read_attempts: Vec::new(),
                read_syscalls: 0,
                getdents64_attempts: Vec::new(),
                getdents64_syscalls: 0,
                mmap_attempts: Vec::new(),
                mmap_syscalls: 0,
                write_attempts: Vec::new(),
                write_syscalls: 0,
                unmodeled_syscall_attempts: Vec::new(),
                unmodeled_syscalls: 0,
                syscall_entries: 1,
                syscall_exits: 0,
                terminal_syscalls: 1,
                outcome: CapturedProcessOutcome::Signaled(libc::SIGKILL),
            }],
        )
        .unwrap();
        assert_eq!(receipt.path_mode_change_syscalls().unwrap(), 1);

        let mut missing_terminal = receipt;
        missing_terminal.processes[0].terminal_syscalls = 0;
        assert!(missing_terminal.validate_complete().is_err());
    }

    #[test]
    fn validates_descriptor_timestamp_change_shapes() {
        let changed = CapturedDescriptorTimestampChangeAttempt {
            syscall_index: 0,
            request: CapturedDescriptorTimestampChangeRequest {
                descriptor: 7,
                identity: Some(CapturedDescriptorIdentity {
                    target: b"/value".to_vec(),
                    device: 1,
                    inode: 2,
                    kind: CapturedDescriptorKind::RegularFile,
                }),
                mount_id: Some(3),
                times_is_null: true,
                times: None,
                flags: 0,
            },
            outcome: CapturedDescriptorTimestampChangeOutcome::Changed,
        };
        assert!(!validate_captured_descriptor_timestamp_change_attempt(&changed).unwrap());

        let mut both_omit = changed.clone();
        both_omit.request.descriptor = -1;
        both_omit.request.identity = None;
        both_omit.request.mount_id = None;
        both_omit.request.times_is_null = false;
        both_omit.request.times = Some([
            CapturedTimestamp {
                seconds: i64::MIN,
                nanoseconds: LINUX_UTIME_OMIT,
            },
            CapturedTimestamp {
                seconds: i64::MAX,
                nanoseconds: LINUX_UTIME_OMIT,
            },
        ]);
        both_omit.request.flags = u32::MAX;
        assert!(!validate_captured_descriptor_timestamp_change_attempt(&both_omit).unwrap());

        let mut invalid_nanoseconds = changed.clone();
        invalid_nanoseconds.request.times_is_null = false;
        invalid_nanoseconds.request.times = Some([
            CapturedTimestamp {
                seconds: 1,
                nanoseconds: -1,
            },
            CapturedTimestamp {
                seconds: 2,
                nanoseconds: 0,
            },
        ]);
        assert!(
            validate_captured_descriptor_timestamp_change_attempt(&invalid_nanoseconds).is_err()
        );
        let mut mismatched_nullness = changed.clone();
        mismatched_nullness.request.times_is_null = false;
        assert!(
            validate_captured_descriptor_timestamp_change_attempt(&mismatched_nullness).is_err()
        );
        let mut missing_mount = changed.clone();
        missing_mount.request.mount_id = None;
        assert!(validate_captured_descriptor_timestamp_change_attempt(&missing_mount).is_err());
        let mut terminal = changed;
        terminal.outcome = CapturedDescriptorTimestampChangeOutcome::Terminated;
        assert!(validate_captured_descriptor_timestamp_change_attempt(&terminal).unwrap());
    }

    #[test]
    fn validates_symbolic_link_creation_shapes() {
        let created = CapturedSymbolicLinkCreationAttempt {
            syscall_index: 0,
            request: CapturedSymbolicLinkCreationRequest {
                system_call: CapturedSymbolicLinkCreationSystemCall::SymlinkAt,
                target: b"../opaque-target".to_vec(),
                target_is_null: false,
                target_terminated: true,
                directory_descriptor: 7,
                link_path: b"new-link".to_vec(),
                link_path_is_null: false,
                link_path_terminated: true,
                base: Some(b"/base".to_vec()),
            },
            outcome: CapturedSymbolicLinkCreationOutcome::Created,
        };
        assert!(!validate_captured_symbolic_link_creation_attempt(&created).unwrap());

        let mut absolute = created.clone();
        absolute.request.directory_descriptor = -1;
        absolute.request.link_path = b"/absolute-link".to_vec();
        absolute.request.base = None;
        assert!(!validate_captured_symbolic_link_creation_attempt(&absolute).unwrap());

        let mut legacy = created.clone();
        legacy.request.system_call = CapturedSymbolicLinkCreationSystemCall::Symlink;
        legacy.request.directory_descriptor = libc::AT_FDCWD;
        legacy.request.base = Some(b"/cwd".to_vec());
        assert!(!validate_captured_symbolic_link_creation_attempt(&legacy).unwrap());

        let mut unterminated = created.clone();
        unterminated.request.target = vec![b'x'; LINUX_PATH_MAX];
        unterminated.request.target_terminated = false;
        unterminated.outcome = CapturedSymbolicLinkCreationOutcome::Failed(libc::ENAMETOOLONG);
        assert!(!validate_captured_symbolic_link_creation_attempt(&unterminated).unwrap());
        unterminated.request.target.pop();
        assert!(validate_captured_symbolic_link_creation_attempt(&unterminated).is_err());

        let mut missing_base = created.clone();
        missing_base.request.base = None;
        assert!(validate_captured_symbolic_link_creation_attempt(&missing_base).is_err());
        let mut invalid_legacy = legacy;
        invalid_legacy.request.directory_descriptor = 7;
        assert!(validate_captured_symbolic_link_creation_attempt(&invalid_legacy).is_err());
        let mut zero_errno = created.clone();
        zero_errno.outcome = CapturedSymbolicLinkCreationOutcome::Failed(0);
        assert!(validate_captured_symbolic_link_creation_attempt(&zero_errno).is_err());
        let mut terminal = created;
        terminal.outcome = CapturedSymbolicLinkCreationOutcome::Terminated;
        assert!(validate_captured_symbolic_link_creation_attempt(&terminal).unwrap());
    }

    #[test]
    fn validates_terminal_filesystem_status_coverage() {
        let receipt = ProcessCaptureReceipt::new(
            1,
            vec![CapturedProcess {
                id: 1,
                parent: None,
                invocations: vec![fixture_invocation(b"/initial")],
                exec_attempts: Vec::new(),
                exec_syscalls: 0,
                open_attempts: Vec::new(),
                open_syscalls: 0,
                newfstatat_attempts: Vec::new(),
                newfstatat_syscalls: 0,
                fstat_attempts: Vec::new(),
                fstat_syscalls: 0,
                getrandom_attempts: Vec::new(),
                getrandom_syscalls: 0,
                runtime_observation_attempts: Vec::new(),
                runtime_observation_syscalls: 0,
                prlimit_attempts: Vec::new(),
                prlimit_syscalls: 0,
                close_attempts: Vec::new(),
                close_syscalls: 0,
                seek_attempts: Vec::new(),
                seek_syscalls: 0,
                file_advice_attempts: Vec::new(),
                file_advice_syscalls: 0,
                fcntl_attempts: Vec::new(),
                fcntl_syscalls: 0,
                ioctl_attempts: Vec::new(),
                ioctl_syscalls: 0,
                pipe_attempts: Vec::new(),
                pipe_syscalls: 0,
                epoll_create_attempts: Vec::new(),
                epoll_create_syscalls: 0,
                socket_create_attempts: Vec::new(),
                socket_create_syscalls: 0,
                socket_connect_attempts: Vec::new(),
                socket_connect_syscalls: 0,
                dup2_attempts: Vec::new(),
                dup2_syscalls: 0,
                signal_state_attempts: Vec::new(),
                signal_state_syscalls: 0,
                signal_deliveries: Vec::new(),
                umask_attempts: Vec::new(),
                umask_syscalls: 0,
                credential_attempts: Vec::new(),
                credential_syscalls: 0,
                futex_wake_attempts: Vec::new(),
                process_creation_attempts: Vec::new(),
                futex_wake_syscalls: 0,
                process_creation_syscalls: 0,
                child_wait_attempts: Vec::new(),
                child_wait_syscalls: 0,
                process_identity_attempts: Vec::new(),
                process_identity_syscalls: 0,
                memory_state_attempts: Vec::new(),
                memory_state_syscalls: 0,
                thread_state_attempts: Vec::new(),
                thread_state_syscalls: 0,
                lifecycle_exit_attempts: Vec::new(),
                lifecycle_exit_syscalls: 0,
                statx_attempts: Vec::new(),
                statx_syscalls: 0,
                xattr_attempts: Vec::new(),
                xattr_syscalls: 0,
                readlink_attempts: Vec::new(),
                readlink_syscalls: 0,
                access_attempts: Vec::new(),
                access_syscalls: 0,
                getcwd_attempts: Vec::new(),
                getcwd_syscalls: 0,
                cwd_change_attempts: Vec::new(),
                cwd_change_syscalls: 0,
                path_removal_attempts: Vec::new(),
                path_removal_syscalls: 0,
                directory_creation_attempts: Vec::new(),
                directory_creation_syscalls: 0,
                path_rename_attempts: Vec::new(),
                path_rename_syscalls: 0,
                path_mode_change_attempts: Vec::new(),
                path_mode_change_syscalls: 0,
                descriptor_timestamp_change_attempts: Vec::new(),
                descriptor_timestamp_change_syscalls: 0,
                symbolic_link_creation_attempts: Vec::new(),
                symbolic_link_creation_syscalls: 0,
                filesystem_status_attempts: vec![CapturedFilesystemStatusAttempt {
                    syscall_index: 0,
                    request: CapturedFilesystemStatusRequest {
                        system_call: CapturedFilesystemStatusSystemCall::Statfs,
                        descriptor: libc::AT_FDCWD,
                        path: b".".to_vec(),
                        path_is_null: false,
                        path_terminated: true,
                        output_is_null: false,
                        base: Some(b"/initial".to_vec()),
                        descriptor_identity: None,
                        descriptor_mount_id: None,
                    },
                    outcome: CapturedFilesystemStatusOutcome::Terminated,
                }],
                filesystem_status_syscalls: 1,
                read_attempts: Vec::new(),
                read_syscalls: 0,
                getdents64_attempts: Vec::new(),
                getdents64_syscalls: 0,
                mmap_attempts: Vec::new(),
                mmap_syscalls: 0,
                write_attempts: Vec::new(),
                write_syscalls: 0,
                unmodeled_syscall_attempts: Vec::new(),
                unmodeled_syscalls: 0,
                syscall_entries: 1,
                syscall_exits: 0,
                terminal_syscalls: 1,
                outcome: CapturedProcessOutcome::Signaled(libc::SIGKILL),
            }],
        )
        .unwrap();
        assert_eq!(receipt.filesystem_status_syscalls().unwrap(), 1);

        let mut missing_terminal = receipt;
        missing_terminal.processes[0].terminal_syscalls = 0;
        assert!(missing_terminal.validate_complete().is_err());
    }

    #[test]
    fn validates_filesystem_status_request_shapes_and_path_termination() {
        let status = CapturedFilesystemStatus {
            filesystem_type: 0x0102_1994,
            block_size: 4096,
            blocks: 100,
            blocks_free: 50,
            blocks_available: 40,
            files: 20,
            files_free: 10,
            filesystem_id: [1, -2],
            maximum_name_length: 255,
            fragment_size: 4096,
            mount_flags: 0,
            reserved_words: [0; 4],
        };
        let observed = CapturedFilesystemStatusAttempt {
            syscall_index: 0,
            request: CapturedFilesystemStatusRequest {
                system_call: CapturedFilesystemStatusSystemCall::Statfs,
                descriptor: libc::AT_FDCWD,
                path: vec![b'a'; LINUX_PATH_MAX - 1],
                path_is_null: false,
                path_terminated: true,
                output_is_null: false,
                base: Some(b"/base".to_vec()),
                descriptor_identity: None,
                descriptor_mount_id: None,
            },
            outcome: CapturedFilesystemStatusOutcome::Observed(status.clone()),
        };
        assert!(!validate_captured_filesystem_status_attempt(&observed).unwrap());

        let mut long_base = observed.clone();
        let mut base = vec![b'a'; LINUX_PATH_MAX];
        base[0] = b'/';
        long_base.request.base = Some(base);
        assert!(!validate_captured_filesystem_status_attempt(&long_base).unwrap());

        let mut unterminated = observed.clone();
        unterminated.request.path.push(b'a');
        unterminated.request.path_terminated = false;
        unterminated.request.base = None;
        unterminated.outcome = CapturedFilesystemStatusOutcome::Failed(libc::ENAMETOOLONG);
        assert!(!validate_captured_filesystem_status_attempt(&unterminated).unwrap());
        unterminated.request.path.pop();
        assert!(validate_captured_filesystem_status_attempt(&unterminated).is_err());

        let mut null_output = observed.clone();
        null_output.request.output_is_null = true;
        assert!(validate_captured_filesystem_status_attempt(&null_output).is_err());

        let identity = CapturedDescriptorIdentity {
            target: b"/base/file".to_vec(),
            device: 1,
            inode: 2,
            kind: CapturedDescriptorKind::RegularFile,
        };
        let descriptor = CapturedFilesystemStatusAttempt {
            syscall_index: 1,
            request: CapturedFilesystemStatusRequest {
                system_call: CapturedFilesystemStatusSystemCall::Fstatfs,
                descriptor: 7,
                path: Vec::new(),
                path_is_null: false,
                path_terminated: false,
                output_is_null: false,
                base: None,
                descriptor_identity: Some(identity),
                descriptor_mount_id: Some(19),
            },
            outcome: CapturedFilesystemStatusOutcome::Observed(status),
        };
        assert!(!validate_captured_filesystem_status_attempt(&descriptor).unwrap());

        let mut missing_mount = descriptor.clone();
        missing_mount.request.descriptor_mount_id = None;
        assert!(validate_captured_filesystem_status_attempt(&missing_mount).is_err());
        let mut invalid_failed = descriptor.clone();
        invalid_failed.request.descriptor = -1;
        invalid_failed.request.descriptor_identity = None;
        invalid_failed.request.descriptor_mount_id = None;
        invalid_failed.outcome = CapturedFilesystemStatusOutcome::Failed(libc::EBADF);
        assert!(!validate_captured_filesystem_status_attempt(&invalid_failed).unwrap());
        let mut cross_kind = invalid_failed;
        cross_kind.request.path_terminated = true;
        assert!(validate_captured_filesystem_status_attempt(&cross_kind).is_err());
        let mut zero_errno = descriptor;
        zero_errno.outcome = CapturedFilesystemStatusOutcome::Failed(0);
        assert!(validate_captured_filesystem_status_attempt(&zero_errno).is_err());
    }

    #[test]
    fn validates_terminal_getdents64_and_cross_kind_census() {
        let directory = CapturedDescriptorIdentity {
            target: b"/fixture/directory".to_vec(),
            device: 1,
            inode: 2,
            kind: CapturedDescriptorKind::Directory,
        };
        let receipt = ProcessCaptureReceipt::new(
            1,
            vec![CapturedProcess {
                id: 1,
                parent: None,
                invocations: vec![CapturedInvocation {
                    executable: b"/bin/true".to_vec(),
                    argv: vec![b"true".to_vec()],
                    environment: Vec::new(),
                    working_directory: b"/".to_vec(),
                    signal_state: CapturedExecSignalState::default(),
                    file_creation_mask: 0o022,
                    credentials: CapturedCredentialState::default(),
                    file_mappings: vec![fixture_exec_mapping()],
                }],
                exec_attempts: Vec::new(),
                exec_syscalls: 0,
                open_attempts: Vec::new(),
                open_syscalls: 0,
                newfstatat_attempts: Vec::new(),
                newfstatat_syscalls: 0,
                fstat_attempts: Vec::new(),
                fstat_syscalls: 0,
                getrandom_attempts: Vec::new(),
                getrandom_syscalls: 0,
                runtime_observation_attempts: Vec::new(),
                runtime_observation_syscalls: 0,
                prlimit_attempts: Vec::new(),
                prlimit_syscalls: 0,
                close_attempts: Vec::new(),
                close_syscalls: 0,
                seek_attempts: Vec::new(),
                seek_syscalls: 0,
                file_advice_attempts: Vec::new(),
                file_advice_syscalls: 0,
                fcntl_attempts: Vec::new(),
                fcntl_syscalls: 0,
                ioctl_attempts: Vec::new(),
                ioctl_syscalls: 0,
                pipe_attempts: Vec::new(),
                pipe_syscalls: 0,
                epoll_create_attempts: Vec::new(),
                epoll_create_syscalls: 0,
                socket_create_attempts: Vec::new(),
                socket_create_syscalls: 0,
                socket_connect_attempts: Vec::new(),
                socket_connect_syscalls: 0,
                dup2_attempts: Vec::new(),
                dup2_syscalls: 0,
                signal_state_attempts: Vec::new(),
                signal_state_syscalls: 0,
                signal_deliveries: Vec::new(),
                umask_attempts: Vec::new(),
                umask_syscalls: 0,
                credential_attempts: Vec::new(),
                credential_syscalls: 0,
                futex_wake_attempts: Vec::new(),
                process_creation_attempts: Vec::new(),
                futex_wake_syscalls: 0,
                process_creation_syscalls: 0,
                child_wait_attempts: Vec::new(),
                child_wait_syscalls: 0,
                process_identity_attempts: Vec::new(),
                process_identity_syscalls: 0,
                memory_state_attempts: Vec::new(),
                memory_state_syscalls: 0,
                thread_state_attempts: Vec::new(),
                thread_state_syscalls: 0,
                lifecycle_exit_attempts: Vec::new(),
                lifecycle_exit_syscalls: 0,
                statx_attempts: Vec::new(),
                statx_syscalls: 0,
                xattr_attempts: Vec::new(),
                xattr_syscalls: 0,
                readlink_attempts: Vec::new(),
                readlink_syscalls: 0,
                access_attempts: Vec::new(),
                access_syscalls: 0,
                getcwd_attempts: Vec::new(),
                getcwd_syscalls: 0,
                cwd_change_attempts: Vec::new(),
                cwd_change_syscalls: 0,
                path_removal_attempts: Vec::new(),
                path_removal_syscalls: 0,
                directory_creation_attempts: Vec::new(),
                directory_creation_syscalls: 0,
                path_rename_attempts: Vec::new(),
                path_rename_syscalls: 0,
                path_mode_change_attempts: Vec::new(),
                path_mode_change_syscalls: 0,
                descriptor_timestamp_change_attempts: Vec::new(),
                descriptor_timestamp_change_syscalls: 0,
                symbolic_link_creation_attempts: Vec::new(),
                symbolic_link_creation_syscalls: 0,
                filesystem_status_attempts: Vec::new(),
                filesystem_status_syscalls: 0,
                read_attempts: Vec::new(),
                read_syscalls: 0,
                getdents64_attempts: vec![CapturedGetdents64Attempt {
                    syscall_index: 0,
                    request: CapturedGetdents64Request {
                        descriptor: 3,
                        identity: Some(directory),
                        mount_id: Some(1),
                        position_before: Some(0),
                        requested_bytes: 4096,
                    },
                    outcome: CapturedGetdents64Outcome::Terminated,
                }],
                getdents64_syscalls: 1,
                mmap_attempts: Vec::new(),
                mmap_syscalls: 0,
                write_attempts: Vec::new(),
                write_syscalls: 0,
                unmodeled_syscall_attempts: Vec::new(),
                unmodeled_syscalls: 0,
                syscall_entries: 1,
                syscall_exits: 0,
                terminal_syscalls: 1,
                outcome: CapturedProcessOutcome::Signaled(libc::SIGKILL),
            }],
        )
        .unwrap();

        let mut overcounted = receipt.clone();
        let process = &mut overcounted.processes[0];
        process.read_attempts.push(CapturedReadAttempt {
            syscall_index: 1,
            request: CapturedReadRequest {
                system_call: CapturedReadSystemCall::Read,
                descriptor: 4,
                identity: Some(CapturedDescriptorIdentity {
                    target: b"/fixture/input".to_vec(),
                    device: 1,
                    inode: 3,
                    kind: CapturedDescriptorKind::RegularFile,
                }),
                mount_id: Some(1),
                uses_descriptor_offset: true,
                offset: Some(0),
                buffer_lengths: vec![1],
                requested_bytes: 1,
                flags: 0,
            },
            outcome: CapturedReadOutcome::Terminated,
        });
        process.read_syscalls = 1;
        process.syscall_entries = 2;
        process.syscall_exits = 1;
        assert!(overcounted.validate_complete().is_err());

        let mut mmap_overcounted = receipt;
        let process = &mut mmap_overcounted.processes[0];
        process.mmap_attempts.push(CapturedMmapAttempt {
            syscall_index: 1,
            request: CapturedMmapRequest {
                address: 0,
                length: 4096,
                protection: 3,
                flags: LINUX_MAP_ANONYMOUS | 2,
                descriptor: -1,
                identity: None,
                mount_id: None,
                offset: 0,
            },
            outcome: CapturedMmapOutcome::Terminated,
        });
        process.mmap_syscalls = 1;
        process.syscall_entries = 2;
        process.syscall_exits = 1;
        assert!(mmap_overcounted.validate_complete().is_err());
    }

    #[test]
    fn validates_complete_directory_member_names() {
        let empty = CapturedDirectoryValue::Complete {
            entries: 0,
            digest: *blake3::hash(COMPLETE_DIRECTORY_DIGEST_DOMAIN).as_bytes(),
            members: Vec::new(),
        };
        assert!(valid_directory_value(&empty));
        let complete = CapturedDirectoryValue::Complete {
            entries: 4,
            digest: [7; 32],
            members: vec![b"a".to_vec(), b"b".to_vec()],
        };
        assert!(valid_directory_value(&complete));
        for members in [
            vec![b"b".to_vec(), b"a".to_vec()],
            vec![b"a".to_vec(), b"a".to_vec()],
            vec![b".".to_vec()],
            vec![b"..".to_vec()],
            vec![b"a/b".to_vec()],
            vec![vec![0]],
        ] {
            let invalid = CapturedDirectoryValue::Complete {
                entries: 4,
                digest: [7; 32],
                members,
            };
            assert!(!valid_directory_value(&invalid));
        }
        let incomplete = CapturedDirectoryValue::Complete {
            entries: 1,
            digest: [7; 32],
            members: vec![b"a".to_vec(), b"b".to_vec()],
        };
        assert!(!valid_directory_value(&incomplete));
    }
}
