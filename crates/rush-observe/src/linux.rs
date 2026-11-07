use crate::stream::{ProcessCaptureEvent, ProcessCaptureSink, ProcessFinish, ReceiptBuilder};
use crate::{
    CapturedAccessAttempt, CapturedAccessOutcome, CapturedAccessRequest, CapturedAccessSystemCall,
    CapturedChildWaitAttempt, CapturedChildWaitOutcome, CapturedChildWaitRequest,
    CapturedChildWaitSelector, CapturedCloseAttempt, CapturedCloseOutcome, CapturedCloseRequest,
    CapturedCredentialAttempt, CapturedCredentialOutcome, CapturedCredentialRequest,
    CapturedCredentialState, CapturedCwdChangeAttempt, CapturedCwdChangeOutcome,
    CapturedCwdChangeRequest, CapturedCwdChangeSystemCall,
    CapturedDescriptorDuplicationSystemCall, CapturedDescriptorIdentity, CapturedDescriptorKind,
    CapturedDescriptorTimestampChangeAttempt,
    CapturedDescriptorTimestampChangeOutcome, CapturedDescriptorTimestampChangeRequest,
    CapturedDirectoryCreationAttempt, CapturedDirectoryCreationOutcome,
    CapturedDirectoryCreationRequest, CapturedDirectoryCreationSystemCall, CapturedDirectoryEntry,
    CapturedDirectoryValue, CapturedDup2Attempt, CapturedDup2Outcome, CapturedDup2Request,
    CapturedEpollCreateAttempt, CapturedEpollCreateOutcome, CapturedEpollCreateRequest,
    CapturedExecAttempt, CapturedExecFileMapping, CapturedExecOutcome, CapturedExecRequest,
    CapturedExecSignalState, CapturedExitFileValue, CapturedFcntlAttempt, CapturedFcntlOutcome,
    CapturedFcntlRecordLock, CapturedFcntlRequest, CapturedFileAdviceAttempt,
    CapturedFileAdviceOutcome,
    CapturedFileAdviceRequest,
    CapturedFilesystemStatus, CapturedFilesystemStatusAttempt, CapturedFilesystemStatusOutcome,
    CapturedFilesystemStatusRequest, CapturedFilesystemStatusSystemCall, CapturedFstatAttempt,
    CapturedFstatOutcome, CapturedFstatRequest, CapturedFutexWakeAttempt, CapturedFutexWakeOutcome,
    CapturedFutexWakeRequest, CapturedGetcwdAttempt, CapturedGetcwdOutcome, CapturedGetcwdRequest,
    CapturedGetdents64Attempt, CapturedGetdents64Outcome, CapturedGetdents64Request,
    CapturedGetrandomAttempt, CapturedGetrandomOutcome, CapturedGetrandomRequest,
    CapturedInvocation, CapturedIoctlAttempt, CapturedIoctlOutcome, CapturedIoctlRequest,
    CapturedLifecycleExitAttempt, CapturedLifecycleExitRequest, CapturedLifecycleExitSystemCall,
    CapturedMappedFileIdentity, CapturedMappedFileValue, CapturedMemoryStateAttempt,
    CapturedMemoryStateOutcome, CapturedMemoryStateRequest, CapturedMmapAttempt,
    CapturedMmapOutcome, CapturedMmapRequest, CapturedNewfstatatAttempt, CapturedNewfstatatOutcome,
    CapturedNewfstatatRequest, CapturedOpenAttempt, CapturedOpenOutcome, CapturedOpenRequest,
    CapturedOpenSystemCall, CapturedPathModeChangeAttempt, CapturedPathModeChangeOutcome,
    CapturedPathModeChangeRequest, CapturedPathModeChangeSystemCall, CapturedPathRemovalAttempt,
    CapturedPathRemovalOutcome, CapturedPathRemovalRequest, CapturedPathRemovalSystemCall,
    CapturedPathRenameAttempt, CapturedPathRenameOutcome, CapturedPathRenameRequest,
    CapturedPathRenameSystemCall, CapturedPipeAttempt, CapturedPipeOutcome, CapturedPipeRequest,
    CapturedPollDescriptor, CapturedPrlimitAttempt, CapturedPrlimitOutcome, CapturedPrlimitRequest,
    CapturedProcessCreationAttempt, CapturedProcessCreationOutcome, CapturedProcessCreationRequest,
    CapturedProcessId, CapturedProcessIdentityAttempt, CapturedProcessIdentityOutcome,
    CapturedProcessIdentityRequest, CapturedProcessOutcome, CapturedReadAttempt,
    CapturedReadOutcome, CapturedReadRequest, CapturedReadSystemCall, CapturedReadlinkAttempt,
    CapturedReadlinkOutcome, CapturedReadlinkRequest, CapturedReadlinkSystemCall,
    CapturedResourceLimit, CapturedResourceUsage, CapturedRuntimeObservationAttempt,
    CapturedRuntimeObservationOutcome, CapturedRuntimeObservationRequest, CapturedSeekAttempt,
    CapturedSeekOutcome, CapturedSeekRequest, CapturedSignalDelivery,
    CapturedSignalDisposition, CapturedSignalSource, CapturedSignalStateAttempt,
    CapturedSignalStateOutcome, CapturedSignalStateRequest,
    CapturedSocketConnectAttempt, CapturedSocketConnectOutcome,
    CapturedSocketConnectRequest, CapturedSocketCreateAttempt,
    CapturedSocketCreateOutcome, CapturedSocketCreateRequest,
    CapturedStatxAttempt, CapturedStatxMetadata, CapturedStatxOutcome, CapturedStatxRequest,
    CapturedStatxTimestamp, CapturedSymbolicLinkCreationAttempt,
    CapturedSymbolicLinkCreationOutcome, CapturedSymbolicLinkCreationRequest,
    CapturedSymbolicLinkCreationSystemCall, CapturedSystemIdentity, CapturedSystemInformation,
    CapturedThreadStateAttempt, CapturedThreadStateOutcome, CapturedThreadStateRequest,
    CapturedTimestamp,
    CapturedUmaskAttempt, CapturedUmaskOutcome, CapturedUnixSocketAddress,
    CapturedUnmodeledSyscallAttempt, CapturedUnmodeledSyscallOutcome,
    CapturedWriteAttempt, CapturedWriteOutcome, CapturedWriteRequest,
    CapturedWriteSystemCall, CapturedXattrAttempt, CapturedXattrOutcome,
    CapturedXattrRequest, CapturedXattrSystemCall, ProcessCaptureReceipt,
    COMPLETE_DIRECTORY_DIGEST_DOMAIN,
};
use anyhow::{Context, Result};
use nix::sys::ptrace;
use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

const MAX_EXEC_VECTOR_ENTRIES: usize = 131_072;
const MAX_EXEC_REQUEST_BYTES: usize = 8 * 1024 * 1024;
const MAX_INVOCATION_EVENT_BYTES: usize = 32 * 1024 * 1024;
const MAX_EXEC_FILE_MAPPINGS: usize = 131_072;
const MAX_READ_VECTOR_ENTRIES: usize = 1024;
const MAX_GETDENTS64_RESULT_BYTES: u32 = 8 * 1024 * 1024;
const MAX_COMPLETE_DIRECTORY_ENTRIES: u64 = 1_048_576;
const MAX_COMPLETE_DIRECTORY_NAME_BYTES: u64 = 256 * 1024 * 1024;
const MAX_READLINK_RESULT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_XATTR_RESULT_BYTES: u64 = 65_536;
const MAX_GETCWD_RESULT_BYTES: u64 = 8 * 1024 * 1024;
const MAX_IDENTITY_BYTES: usize = 8 * 1024 * 1024;
const MAX_FDINFO_BYTES: usize = 64 * 1024;
const LINUX_DIRENT64_HEADER_SIZE: usize = 19;
const LINUX_DIRENT64_MINIMUM_RECORD_SIZE: usize = 24;
const LINUX_DIRENT64_ALIGNMENT: usize = 8;
const LINUX_PATH_MAX: usize = 4096;
const LINUX_MAP_ANONYMOUS: u64 = 0x20;
const OPEN_HOW_SIZE: u64 = 24;

#[cfg(target_arch = "x86_64")]
const NATIVE_AUDIT_ARCHITECTURE: Option<u32> = Some(0xc000_003e);
#[cfg(target_arch = "aarch64")]
const NATIVE_AUDIT_ARCHITECTURE: Option<u32> = Some(0xc000_00b7);
#[cfg(target_arch = "riscv64")]
const NATIVE_AUDIT_ARCHITECTURE: Option<u32> = Some(0xc000_00f3);
#[cfg(not(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64"
)))]
const NATIVE_AUDIT_ARCHITECTURE: Option<u32> = None;

thread_local! {
    static TRACEE_ROOT_CACHE: std::cell::RefCell<BTreeMap<Pid, PathBuf>> =
        std::cell::RefCell::new(BTreeMap::new());
}

const TRACE_OPTIONS: ptrace::Options = ptrace::Options::PTRACE_O_TRACECLONE
    .union(ptrace::Options::PTRACE_O_TRACEFORK)
    .union(ptrace::Options::PTRACE_O_TRACEVFORK)
    .union(ptrace::Options::PTRACE_O_TRACEEXEC)
    .union(ptrace::Options::PTRACE_O_TRACESYSGOOD)
    .union(ptrace::Options::PTRACE_O_EXITKILL);
const LINUX_SIGNAL_SET_BYTES_LIMIT: u64 = 128;
const LINUX_RT_SIGACTION_PREFIX_BYTES: u64 = 24;

#[derive(Debug)]
struct PendingProcess {
    id: CapturedProcessId,
    address_space: CapturedProcessId,
    thread_group: CapturedProcessId,
    invocations: u64,
    exec_syscalls: u64,
    open_syscalls: u64,
    newfstatat_syscalls: u64,
    fstat_syscalls: u64,
    getrandom_syscalls: u64,
    runtime_observation_syscalls: u64,
    prlimit_syscalls: u64,
    close_syscalls: u64,
    seek_syscalls: u64,
    file_advice_syscalls: u64,
    fcntl_syscalls: u64,
    ioctl_syscalls: u64,
    pipe_syscalls: u64,
    epoll_create_syscalls: u64,
    socket_create_syscalls: u64,
    socket_connect_syscalls: u64,
    dup2_syscalls: u64,
    signal_state_syscalls: u64,
    signal_deliveries: u64,
    umask_syscalls: u64,
    credential_syscalls: u64,
    futex_wake_syscalls: u64,
    process_creation_syscalls: u64,
    child_wait_syscalls: u64,
    process_identity_syscalls: u64,
    memory_state_syscalls: u64,
    thread_state_syscalls: u64,
    lifecycle_exit_syscalls: u64,
    statx_syscalls: u64,
    xattr_syscalls: u64,
    readlink_syscalls: u64,
    access_syscalls: u64,
    getcwd_syscalls: u64,
    cwd_change_syscalls: u64,
    path_removal_syscalls: u64,
    directory_creation_syscalls: u64,
    path_rename_syscalls: u64,
    path_mode_change_syscalls: u64,
    descriptor_timestamp_change_syscalls: u64,
    symbolic_link_creation_syscalls: u64,
    filesystem_status_syscalls: u64,
    read_syscalls: u64,
    getdents64_syscalls: u64,
    mmap_syscalls: u64,
    write_syscalls: u64,
    unmodeled_syscalls: u64,
    pending_exec: Option<PendingExec>,
    pending_open: Option<PendingOpen>,
    pending_newfstatat: Option<PendingNewfstatat>,
    pending_fstat: Option<PendingFstat>,
    pending_getrandom: Option<PendingGetrandom>,
    pending_runtime_observation: Option<PendingRuntimeObservation>,
    pending_prlimit: Option<PendingPrlimit>,
    pending_close: Option<PendingClose>,
    pending_seek: Option<PendingSeek>,
    pending_file_advice: Option<PendingFileAdvice>,
    pending_fcntl: Option<PendingFcntl>,
    pending_ioctl: Option<PendingIoctl>,
    pending_pipe: Option<PendingPipe>,
    pending_epoll_create: Option<PendingEpollCreate>,
    pending_socket_create: Option<PendingSocketCreate>,
    pending_socket_connect: Option<PendingSocketConnect>,
    pending_dup2: Option<PendingDup2>,
    pending_signal_state: Option<PendingSignalState>,
    pending_umask: Option<PendingUmask>,
    pending_credential: Option<PendingCredential>,
    pending_futex_wake: Option<PendingFutexWake>,
    pending_process_creation: Option<PendingProcessCreation>,
    pending_child_wait: Option<PendingChildWait>,
    pending_process_identity: Option<PendingProcessIdentity>,
    pending_memory_state: Option<PendingMemoryState>,
    pending_thread_state: Option<PendingThreadState>,
    caught_signal_frames: Vec<u64>,
    pending_lifecycle_exit: Option<PendingLifecycleExit>,
    pending_statx: Option<PendingStatx>,
    pending_xattr: Option<PendingXattr>,
    pending_readlink: Option<PendingReadlink>,
    pending_access: Option<PendingAccess>,
    pending_getcwd: Option<PendingGetcwd>,
    pending_cwd_change: Option<PendingCwdChange>,
    pending_path_removal: Option<PendingPathRemoval>,
    pending_directory_creation: Option<PendingDirectoryCreation>,
    pending_path_rename: Option<PendingPathRename>,
    pending_path_mode_change: Option<PendingPathModeChange>,
    pending_descriptor_timestamp_change: Option<PendingDescriptorTimestampChange>,
    pending_symbolic_link_creation: Option<PendingSymbolicLinkCreation>,
    pending_filesystem_status: Option<PendingFilesystemStatus>,
    pending_read: Option<PendingRead>,
    pending_getdents64: Option<PendingGetdents64>,
    pending_mmap: Option<PendingMmap>,
    pending_write: Option<PendingWrite>,
    pending_unmodeled_syscall: Option<PendingUnmodeledSyscall>,
    active_syscall: Option<SyscallEntry>,
    syscall_entries: u64,
    syscall_exits: u64,
    terminal_syscalls: u64,
    allow_inherited_exit: bool,
    restart_after_interrupt: bool,
}

#[derive(Debug)]
struct PendingExec {
    syscall_index: u64,
    request: CapturedExecRequest,
    loaded_invocation: Option<u64>,
}

#[derive(Debug)]
struct PendingUnmodeledSyscall {
    syscall_index: u64,
    number: u64,
    arguments: [u64; 6],
}

#[derive(Debug)]
struct PendingOpen {
    syscall_index: u64,
    request: CapturedOpenRequest,
}

#[derive(Debug)]
struct PendingNewfstatat {
    syscall_index: u64,
    request: CapturedNewfstatatRequest,
    metadata_address: u64,
}

#[derive(Debug)]
struct PendingFstat {
    syscall_index: u64,
    request: CapturedFstatRequest,
    metadata_address: u64,
}

#[derive(Debug)]
struct PendingGetrandom {
    syscall_index: u64,
    request: CapturedGetrandomRequest,
    buffer: ReadBuffer,
}

#[derive(Debug)]
struct PendingRuntimeObservation {
    syscall_index: u64,
    request: CapturedRuntimeObservationRequest,
    output_address: u64,
}

#[derive(Debug)]
struct PendingPrlimit {
    syscall_index: u64,
    request: CapturedPrlimitRequest,
    new_limit: Option<CapturedResourceLimit>,
    old_limit_address: u64,
}

#[derive(Debug)]
struct PendingClose {
    syscall_index: u64,
    request: CapturedCloseRequest,
    backing_file: Option<File>,
}

#[derive(Debug)]
struct PendingSeek {
    syscall_index: u64,
    request: CapturedSeekRequest,
}

#[derive(Debug)]
struct PendingFileAdvice {
    syscall_index: u64,
    request: CapturedFileAdviceRequest,
}

#[derive(Debug)]
struct PendingFcntl {
    syscall_index: u64,
    request: CapturedFcntlRequest,
}

#[derive(Debug)]
struct PendingIoctl {
    syscall_index: u64,
    request: CapturedIoctlRequest,
}

#[derive(Debug)]
struct PendingPipe {
    syscall_index: u64,
    request: CapturedPipeRequest,
    descriptor_address: u64,
}

#[derive(Debug)]
struct PendingEpollCreate {
    syscall_index: u64,
    request: CapturedEpollCreateRequest,
}

#[derive(Debug)]
struct PendingSocketCreate {
    syscall_index: u64,
    request: CapturedSocketCreateRequest,
}

#[derive(Debug)]
struct PendingSocketConnect {
    syscall_index: u64,
    request: CapturedSocketConnectRequest,
}

#[derive(Debug)]
struct PendingDup2 {
    syscall_index: u64,
    request: CapturedDup2Request,
    replaced_backing_file: Option<File>,
}

#[derive(Debug)]
struct PendingSignalState {
    syscall_index: u64,
    request: CapturedSignalStateRequest,
    old_state_address: u64,
}

#[derive(Debug)]
struct PendingUmask {
    syscall_index: u64,
    requested: u32,
}

#[derive(Debug)]
struct PendingCredential {
    syscall_index: u64,
    request: CapturedCredentialRequest,
    output_address: u64,
}

#[derive(Debug)]
struct PendingFutexWake {
    syscall_index: u64,
    request: CapturedFutexWakeRequest,
}

#[derive(Debug)]
struct PendingProcessCreation {
    syscall_index: u64,
    request: CapturedProcessCreationRequest,
    child: Option<(Pid, CapturedProcessId)>,
}

#[derive(Debug)]
struct PendingChildWait {
    syscall_index: u64,
    request: CapturedChildWaitRequest,
    status_address: u64,
}

#[derive(Debug)]
struct PendingProcessIdentity {
    syscall_index: u64,
    request: CapturedProcessIdentityRequest,
    expected_process: Option<(Pid, CapturedProcessId)>,
}

#[derive(Debug)]
struct PendingMemoryState {
    syscall_index: u64,
    request: CapturedMemoryStateRequest,
}

#[derive(Debug)]
struct PendingThreadState {
    syscall_index: u64,
    request: CapturedThreadStateRequest,
    restored_return_value: Option<i64>,
}

#[derive(Debug)]
struct PendingLifecycleExit {
    syscall_index: u64,
    request: CapturedLifecycleExitRequest,
    open_file_values: Vec<CapturedExitFileValue>,
}

#[derive(Debug)]
struct PendingStatx {
    syscall_index: u64,
    request: CapturedStatxRequest,
    metadata_address: u64,
}

#[derive(Debug)]
struct PendingXattr {
    syscall_index: u64,
    request: CapturedXattrRequest,
    value_address: u64,
}

#[derive(Debug)]
struct PendingReadlink {
    syscall_index: u64,
    request: CapturedReadlinkRequest,
    buffer_address: u64,
}

#[derive(Debug)]
struct PendingAccess {
    syscall_index: u64,
    request: CapturedAccessRequest,
}

#[derive(Debug)]
struct PendingGetcwd {
    syscall_index: u64,
    request: CapturedGetcwdRequest,
    buffer_address: u64,
}

#[derive(Debug)]
struct PendingCwdChange {
    syscall_index: u64,
    request: CapturedCwdChangeRequest,
}

#[derive(Debug)]
struct PendingPathRemoval {
    syscall_index: u64,
    request: CapturedPathRemovalRequest,
}

#[derive(Debug)]
struct PendingDirectoryCreation {
    syscall_index: u64,
    request: CapturedDirectoryCreationRequest,
}

#[derive(Debug)]
struct PendingPathRename {
    syscall_index: u64,
    request: CapturedPathRenameRequest,
}

#[derive(Debug)]
struct PendingPathModeChange {
    syscall_index: u64,
    request: CapturedPathModeChangeRequest,
}

#[derive(Debug)]
struct PendingDescriptorTimestampChange {
    syscall_index: u64,
    request: CapturedDescriptorTimestampChangeRequest,
}

#[derive(Debug)]
struct PendingSymbolicLinkCreation {
    syscall_index: u64,
    request: CapturedSymbolicLinkCreationRequest,
}

#[derive(Debug, Clone, Copy)]
#[repr(C)]
struct NativeKernelTimestamp {
    seconds: i64,
    nanoseconds: i64,
}

#[derive(Debug, Clone, Copy)]
#[repr(C)]
struct NativeStatxTimestamp {
    seconds: i64,
    nanoseconds: u32,
    reserved: i32,
}

#[derive(Debug, Clone, Copy)]
#[repr(C)]
struct NativeStatx {
    mask: u32,
    block_size: u32,
    attributes: u64,
    hard_links: u32,
    user: u32,
    group: u32,
    mode: u16,
    spare0: [u16; 1],
    inode: u64,
    size: u64,
    blocks: u64,
    attributes_mask: u64,
    access_time: NativeStatxTimestamp,
    birth_time: NativeStatxTimestamp,
    change_time: NativeStatxTimestamp,
    modification_time: NativeStatxTimestamp,
    special_device_major: u32,
    special_device_minor: u32,
    device_major: u32,
    device_minor: u32,
    mount_id: u64,
    direct_io_memory_alignment: u32,
    direct_io_offset_alignment: u32,
    subvolume: u64,
    atomic_write_unit_minimum: u32,
    atomic_write_unit_maximum: u32,
    atomic_write_segments_maximum: u32,
    direct_io_read_offset_alignment: u32,
    atomic_write_unit_maximum_optimized: u32,
    spare2: [u32; 1],
    spare3: [u64; 8],
}

const NATIVE_STATX_BYTES: usize = 256;

const NATIVE_KERNEL_TIMESTAMP_PAIR_BYTES: usize = 2 * std::mem::size_of::<NativeKernelTimestamp>();

struct PathRenameOperand {
    path: Vec<u8>,
    path_is_null: bool,
    path_terminated: bool,
    base: Option<Vec<u8>>,
}

#[derive(Debug)]
struct PendingFilesystemStatus {
    syscall_index: u64,
    request: CapturedFilesystemStatusRequest,
    buffer_address: u64,
}

#[derive(Debug, Clone, Copy)]
#[repr(C)]
struct NativeFilesystemStatus {
    filesystem_type: i64,
    block_size: i64,
    blocks: u64,
    blocks_free: u64,
    blocks_available: u64,
    files: u64,
    files_free: u64,
    filesystem_id: [i32; 2],
    maximum_name_length: i64,
    fragment_size: i64,
    mount_flags: i64,
    spare: [i64; 4],
}

const NATIVE_FILESYSTEM_STATUS_BYTES: usize = 120;

#[derive(Debug)]
struct PendingRead {
    syscall_index: u64,
    request: CapturedReadRequest,
    buffers: Vec<ReadBuffer>,
    backing_file: Option<File>,
}

type ReadEntryState = (CapturedReadRequest, Vec<ReadBuffer>, Option<File>);

#[derive(Debug)]
struct PendingGetdents64 {
    syscall_index: u64,
    request: CapturedGetdents64Request,
    buffer_address: u64,
    backing_directory: Option<Result<PendingDirectoryWitness>>,
}

type Getdents64EntryState = (
    CapturedGetdents64Request,
    u64,
    Option<Result<PendingDirectoryWitness>>,
);

#[derive(Debug)]
struct PendingMmap {
    syscall_index: u64,
    request: CapturedMmapRequest,
    backing_file: Option<File>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct MappedFileWitnessKey {
    device: u64,
    inode: u64,
    bytes: u64,
    mode: u32,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

type MappedFileWitnessCache = BTreeMap<MappedFileWitnessKey, CapturedMappedFileValue>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct DirectoryWitnessKey {
    device: u64,
    inode: u64,
    bytes: u64,
    mode: u32,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

#[derive(Debug)]
struct PendingDirectoryWitness {
    directory: File,
    key: DirectoryWitnessKey,
    entry_value: CapturedDirectoryValue,
}

#[derive(Debug)]
struct PendingWrite {
    syscall_index: u64,
    request: CapturedWriteRequest,
    buffer: ReadBuffer,
}

fn current_syscall_has_model(process: &PendingProcess, syscall_index: u64) -> bool {
    macro_rules! pending_at {
        ($field:ident) => {
            process
                .$field
                .as_ref()
                .is_some_and(|pending| pending.syscall_index == syscall_index)
        };
    }
    pending_at!(pending_exec)
        || pending_at!(pending_open)
        || pending_at!(pending_newfstatat)
        || pending_at!(pending_fstat)
        || pending_at!(pending_getrandom)
        || pending_at!(pending_runtime_observation)
        || pending_at!(pending_prlimit)
        || pending_at!(pending_close)
        || pending_at!(pending_seek)
        || pending_at!(pending_file_advice)
        || pending_at!(pending_fcntl)
        || pending_at!(pending_ioctl)
        || pending_at!(pending_pipe)
        || pending_at!(pending_epoll_create)
        || pending_at!(pending_socket_create)
        || pending_at!(pending_socket_connect)
        || pending_at!(pending_dup2)
        || pending_at!(pending_signal_state)
        || pending_at!(pending_umask)
        || pending_at!(pending_credential)
        || pending_at!(pending_futex_wake)
        || pending_at!(pending_process_creation)
        || pending_at!(pending_child_wait)
        || pending_at!(pending_process_identity)
        || pending_at!(pending_memory_state)
        || pending_at!(pending_thread_state)
        || pending_at!(pending_lifecycle_exit)
        || pending_at!(pending_statx)
        || pending_at!(pending_xattr)
        || pending_at!(pending_readlink)
        || pending_at!(pending_access)
        || pending_at!(pending_getcwd)
        || pending_at!(pending_cwd_change)
        || pending_at!(pending_path_removal)
        || pending_at!(pending_directory_creation)
        || pending_at!(pending_path_rename)
        || pending_at!(pending_path_mode_change)
        || pending_at!(pending_descriptor_timestamp_change)
        || pending_at!(pending_symbolic_link_creation)
        || pending_at!(pending_filesystem_status)
        || pending_at!(pending_read)
        || pending_at!(pending_getdents64)
        || pending_at!(pending_mmap)
        || pending_at!(pending_write)
}

#[derive(Debug, Clone, Copy)]
struct ReadBuffer {
    address: u64,
    length: u64,
}

#[derive(Debug, Clone, Copy)]
struct ProcFileMapping {
    start: u64,
    end: u64,
    file_offset: u64,
    readable: bool,
    writable: bool,
    executable: bool,
    shared: bool,
    device: u64,
    inode: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyscallStop {
    Entry {
        entry: SyscallEntry,
        stack_pointer: u64,
    },
    Exit(SyscallExit),
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SyscallEntry {
    number: u64,
    arguments: [u64; 6],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SyscallExit {
    return_value: i64,
    is_error: u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SyscallSeccomp {
    number: u64,
    arguments: [u64; 6],
    return_data: u32,
}

#[repr(C)]
union SyscallData {
    entry: SyscallEntry,
    exit: SyscallExit,
    seccomp: SyscallSeccomp,
}

#[repr(C)]
struct SyscallInfo {
    operation: u8,
    padding: [u8; 3],
    architecture: u32,
    instruction_pointer: u64,
    stack_pointer: u64,
    data: SyscallData,
}

struct TraceGuard {
    active: BTreeSet<Pid>,
}

impl TraceGuard {
    fn new(root: Pid) -> Self {
        Self {
            active: BTreeSet::from([root]),
        }
    }
}

impl Drop for TraceGuard {
    fn drop(&mut self) {
        for pid in self.active.iter().copied() {
            let _ = kill(pid, Signal::SIGKILL);
        }
        while !self.active.is_empty() {
            let mut observed = false;
            for pid in self.active.clone() {
                match waitpid(
                    pid,
                    Some(WaitPidFlag::__WALL | WaitPidFlag::WNOHANG),
                ) {
                    Ok(WaitStatus::Exited(pid, _)) | Ok(WaitStatus::Signaled(pid, _, _)) => {
                        self.active.remove(&pid);
                        observed = true;
                    }
                    Ok(WaitStatus::StillAlive) => {}
                    Ok(status) => {
                        observed = true;
                        if let Some(pid) = wait_status_pid(status) {
                            let _ = ptrace::cont(pid, None);
                        }
                    }
                    Err(_) => {
                        self.active.remove(&pid);
                        observed = true;
                    }
                }
            }
            if !observed {
                std::thread::yield_now();
            }
        }
    }
}

fn wait_for_active_tracee(active: &BTreeSet<Pid>) -> Result<WaitStatus> {
    let flags = WaitPidFlag::__WALL | WaitPidFlag::WNOHANG;
    loop {
        for pid in active.iter().copied() {
            let status = waitpid(pid, Some(flags))
                .with_context(|| format!("wait for traced PID {pid}"))?;
            if status != WaitStatus::StillAlive {
                return Ok(status);
            }
        }
        std::thread::yield_now();
    }
}

fn continue_tracee(
    pid: Pid,
    signal: Option<Signal>,
    running: &mut BTreeSet<Pid>,
    description: &str,
) -> Result<()> {
    ptrace::syscall(pid, signal).with_context(|| description.to_string())?;
    if !running.insert(pid) {
        anyhow::bail!("traced PID {pid} was already running");
    }
    Ok(())
}

fn quiesce_shared_address_space(
    current: Pid,
    processes: &BTreeMap<Pid, PendingProcess>,
    running: &mut BTreeSet<Pid>,
    pending_statuses: &mut VecDeque<WaitStatus>,
    interrupted: &mut BTreeSet<Pid>,
) -> Result<()> {
    let Some(address_space) = processes.get(&current).map(|process| process.address_space) else {
        return Ok(());
    };
    let siblings = running
        .iter()
        .copied()
        .filter(|pid| {
            processes
                .get(pid)
                .is_some_and(|process| {
                    process.address_space == address_space && process.active_syscall.is_none()
                })
        })
        .collect::<Vec<_>>();
    for pid in &siblings {
        if interrupted.contains(pid) {
            continue;
        }
        ptrace::interrupt(*pid)
            .with_context(|| format!("interrupt shared-address-space PID {pid}"))?;
        interrupted.insert(*pid);
    }
    for pid in siblings {
        let status = waitpid(pid, Some(WaitPidFlag::__WALL))
            .with_context(|| format!("wait for interrupted PID {pid}"))?;
        if !running.remove(&pid) {
            anyhow::bail!("interrupted PID {pid} was not running");
        }
        pending_statuses.push_back(status);
    }
    Ok(())
}

fn is_internal_restart(exit: SyscallExit) -> bool {
    exit.is_error != 0 && matches!(exit.return_value, -512 | -513 | -514 | -516)
}

fn syscall_allows_shared_address_space_progress(number: u64) -> bool {
    number == libc::SYS_futex as u64
        || number == libc::SYS_sched_yield as u64
        || number == libc::SYS_nanosleep as u64
        || number == libc::SYS_clock_nanosleep as u64
        || number == libc::SYS_restart_syscall as u64
        || number == libc::SYS_exit as u64
        || number == libc::SYS_exit_group as u64
}

fn process_has_address_space_siblings(
    pid: Pid,
    processes: &BTreeMap<Pid, PendingProcess>,
) -> bool {
    let Some(address_space) = processes.get(&pid).map(|process| process.address_space) else {
        return false;
    };
    processes
        .iter()
        .any(|(candidate, process)| *candidate != pid && process.address_space == address_space)
}

fn tracee_can_advance(
    pid: Pid,
    processes: &BTreeMap<Pid, PendingProcess>,
    exclusive: &BTreeMap<CapturedProcessId, Pid>,
) -> bool {
    processes.get(&pid).is_none_or(|process| {
        exclusive
            .get(&process.address_space)
            .is_none_or(|owner| *owner == pid)
    })
}

fn prepare_root_for_interrupts(pid: Pid) -> Result<()> {
    ptrace::detach(pid, Some(Signal::SIGSTOP))
        .context("stop and detach initial observed process")?;
    let stopped = waitpid(pid, Some(WaitPidFlag::WUNTRACED))
        .context("wait for detached observed process to stop")?;
    if stopped != WaitStatus::Stopped(pid, Signal::SIGSTOP) {
        anyhow::bail!("detached observed process did not enter its stop boundary");
    }
    ptrace::seize(pid, TRACE_OPTIONS).context("seize initial observed process")?;
    kill(pid, Signal::SIGCONT).context("continue seized observed process group")?;
    loop {
        match waitpid(pid, Some(WaitPidFlag::__WALL))
            .context("wait for seized observed process transition")?
        {
            WaitStatus::PtraceEvent(
                stopped_pid,
                Signal::SIGSTOP | Signal::SIGTRAP,
                libc::PTRACE_EVENT_STOP,
            ) if stopped_pid == pid => {
                ptrace::syscall(pid, None)
                    .context("continue seized observed process transition")?;
            }
            WaitStatus::Stopped(stopped_pid, Signal::SIGCONT) if stopped_pid == pid => {
                return Ok(())
            }
            status => anyhow::bail!(
                "observed process entered an unexpected seize transition: {status:?}"
            ),
        }
    }
}

fn read_signal_delivery(
    pid: Pid,
    signal: Signal,
    process: &PendingProcess,
    receipt_ids_by_pid: &BTreeMap<Pid, CapturedProcessId>,
) -> Result<CapturedSignalDelivery> {
    let info = ptrace::getsiginfo(pid).context("read traced signal-delivery information")?;
    let signal_number = signal as i32;
    if info.si_signo != signal_number {
        anyhow::bail!("signal-delivery stop disagrees with siginfo signal number");
    }
    let sender_pid = unsafe { info.si_pid() };
    let process_source = signal == Signal::SIGCHLD || info.si_code <= 0;
    let source = if !process_source || sender_pid <= 0 {
        CapturedSignalSource::Kernel
    } else if let Some(sender) = receipt_ids_by_pid.get(&Pid::from_raw(sender_pid)) {
        CapturedSignalSource::TracedProcess { process: *sender }
    } else {
        CapturedSignalSource::ExternalProcess {
            process: sender_pid,
            user: unsafe { info.si_uid() },
        }
    };
    let active_syscall = if process.active_syscall.is_some() {
        Some(
            process
                .syscall_entries
                .checked_sub(1)
                .context("active signal-delivery syscall has no entry index")?,
        )
    } else {
        None
    };
    let disposition = read_signal_disposition(pid, signal_number)?;
    Ok(CapturedSignalDelivery {
        signal: u32::try_from(signal_number).context("signal number is negative")?,
        code: info.si_code,
        errno: info.si_errno,
        source,
        child_status: if signal == Signal::SIGCHLD {
            Some(unsafe { info.si_status() })
        } else {
            None
        },
        syscall_entries: process.syscall_entries,
        active_syscall,
        disposition,
    })
}

fn read_signal_disposition(
    pid: Pid,
    signal: i32,
) -> Result<CapturedSignalDisposition> {
    if !(1..=64).contains(&signal) {
        anyhow::bail!("signal disposition has an invalid signal number");
    }
    let path = PathBuf::from(format!("/proc/{pid}/status"));
    let contents = read_bounded_file(
        &path,
        MAX_FDINFO_BYTES,
        "signal-delivery disposition",
    )?;
    let contents = std::str::from_utf8(&contents)
        .context("decode signal-delivery disposition")?;
    let field = |name: &str| -> Result<u64> {
        let mut values = contents.lines().filter_map(|line| {
            line.strip_prefix(name)
                .and_then(|value| value.strip_prefix(':'))
                .map(str::trim)
        });
        let value = values
            .next()
            .with_context(|| format!("signal disposition omits {name}"))?;
        if values.next().is_some() {
            anyhow::bail!("signal disposition repeats {name}");
        }
        u64::from_str_radix(value, 16)
            .with_context(|| format!("decode signal disposition {name}"))
    };
    let ignored = field("SigIgn")?;
    let caught = field("SigCgt")?;
    let shift = u32::try_from(signal - 1)
        .context("signal disposition bit exceeds u32")?;
    let bit = 1_u64 << shift;
    match (ignored & bit != 0, caught & bit != 0) {
        (false, false) => Ok(CapturedSignalDisposition::Default),
        (true, false) => Ok(CapturedSignalDisposition::Ignored),
        (false, true) => Ok(CapturedSignalDisposition::Caught),
        (true, true) => {
            anyhow::bail!("signal disposition is both ignored and caught")
        }
    }
}

/// Run `command` under ptrace and return every descendant lifecycle.
pub fn capture_process_tree(command: &mut Command) -> Result<ProcessCaptureReceipt> {
    let mut sink = ReceiptBuilder::default();
    let root = capture_process_tree_events(command, &mut sink)?;
    sink.finish(root)
}

/// A process-tree receipt paired with bounded standard-stream tails.
pub struct ProcessCaptureOutput {
    /// Validated process-tree observation receipt.
    pub receipt: ProcessCaptureReceipt,
    /// Bounded tail of the root command's standard output.
    pub stdout_tail: Vec<u8>,
    /// Bounded tail of the root command's standard error.
    pub stderr_tail: Vec<u8>,
}

/// Run `command` under ptrace while draining both output streams concurrently.
pub fn capture_process_tree_with_output(
    command: &mut Command,
    tail_limit: usize,
) -> Result<ProcessCaptureOutput> {
    capture_process_tree_output(command, tail_limit, false)
}

/// Install the trace hook before a later pre-exec hook restricts `ptrace`.
///
/// The command must subsequently be passed exactly once to
/// [`capture_prepared_process_tree_with_output`].
pub fn install_process_tree_capture(command: &mut Command) {
    // SAFETY: `pre_exec` runs after fork. The closure performs only the ptrace
    // syscall and constructs an `io::Error` if that syscall fails.
    unsafe {
        command.pre_exec(|| {
            ptrace::traceme().map_err(std::io::Error::other)?;
            Ok(())
        });
    }
}

/// Capture a command whose trace hook was installed before restrictive hooks.
pub fn capture_prepared_process_tree_with_output(
    command: &mut Command,
    tail_limit: usize,
) -> Result<ProcessCaptureOutput> {
    capture_process_tree_output(command, tail_limit, true)
}

fn capture_process_tree_output(
    command: &mut Command,
    tail_limit: usize,
    trace_prepared: bool,
) -> Result<ProcessCaptureOutput> {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut drains = None;
    let mut sink = ReceiptBuilder::default();
    let root = capture_process_tree_events_with_spawn(
        command,
        &mut sink,
        |child| {
            let stdout = child
                .stdout
                .take()
                .context("observed process stdout was not piped")?;
            let stderr = child
                .stderr
                .take()
                .context("observed process stderr was not piped")?;
            drains = Some((
                std::thread::spawn(move || read_bounded_tail(stdout, tail_limit)),
                std::thread::spawn(move || read_bounded_tail(stderr, tail_limit)),
            ));
            Ok(())
        },
        trace_prepared,
    )?;
    let (stdout_drain, stderr_drain) =
        drains.context("observed process output drains were not installed")?;
    let stdout_tail = stdout_drain
        .join()
        .map_err(|_| anyhow::anyhow!("observed process stdout drain panicked"))??;
    let stderr_tail = stderr_drain
        .join()
        .map_err(|_| anyhow::anyhow!("observed process stderr drain panicked"))??;
    Ok(ProcessCaptureOutput {
        receipt: sink.finish(root)?,
        stdout_tail,
        stderr_tail,
    })
}

fn read_bounded_tail(mut input: impl Read, limit: usize) -> Result<Vec<u8>> {
    let mut tail = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let bytes = input.read(&mut buffer)?;
        if bytes == 0 {
            return Ok(tail);
        }
        if limit == 0 {
            continue;
        }
        if bytes >= limit {
            tail.clear();
            tail.extend_from_slice(&buffer[bytes - limit..bytes]);
            continue;
        }
        let overflow = tail.len().saturating_add(bytes).saturating_sub(limit);
        if overflow != 0 {
            tail.drain(..overflow);
        }
        tail.extend_from_slice(&buffer[..bytes]);
    }
}

pub(crate) fn capture_process_tree_events(
    command: &mut Command,
    sink: &mut impl ProcessCaptureSink,
) -> Result<CapturedProcessId> {
    capture_process_tree_events_with_spawn(command, sink, |_| Ok(()), false)
}

fn capture_process_tree_events_with_spawn(
    command: &mut Command,
    sink: &mut impl ProcessCaptureSink,
    on_spawn: impl FnOnce(&mut Child) -> Result<()>,
    trace_prepared: bool,
) -> Result<CapturedProcessId> {
    TRACEE_ROOT_CACHE.with(|cache| cache.borrow_mut().clear());
    if !trace_prepared {
        install_process_tree_capture(command);
    }
    let mut child = command.spawn().context("launch observed process")?;
    on_spawn(&mut child)?;
    let root_pid = Pid::from_raw(i32::try_from(child.id()).context("child PID exceeds i32")?);
    drop(child);
    let mut guard = TraceGuard::new(root_pid);

    let initial = waitpid(root_pid, None).context("wait for initial exec stop")?;
    if initial != WaitStatus::Stopped(root_pid, Signal::SIGTRAP) {
        anyhow::bail!("observed process did not stop at its initial exec boundary");
    }
    prepare_root_for_interrupts(root_pid)?;

    let root_id = 1;
    let mut next_id = root_id + 1;
    let mut mapped_file_witnesses = MappedFileWitnessCache::new();
    let root_invocation =
        read_invocation(root_pid, sink.max_event_bytes(), &mut mapped_file_witnesses)?;
    sink.record(ProcessCaptureEvent::ProcessStarted {
        process: root_id,
        parent: None,
    })?;
    sink.record(ProcessCaptureEvent::Invocation {
        process: root_id,
        invocation: root_invocation,
    })?;
    let mut by_pid = BTreeMap::from([(
        root_pid,
        PendingProcess {
            id: root_id,
            address_space: root_id,
            thread_group: root_id,
            invocations: 1,
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
            pending_exec: None,
            pending_open: None,
            pending_newfstatat: None,
            pending_fstat: None,
            pending_getrandom: None,
            pending_runtime_observation: None,
            pending_prlimit: None,
            pending_close: None,
            pending_seek: None,
            pending_file_advice: None,
            pending_fcntl: None,
            pending_ioctl: None,
            pending_pipe: None,
            pending_epoll_create: None,
            pending_socket_create: None,
            pending_socket_connect: None,
            pending_dup2: None,
            pending_signal_state: None,
            pending_umask: None,
            pending_credential: None,
            pending_futex_wake: None,
            pending_process_creation: None,
            pending_child_wait: None,
            pending_process_identity: None,
            pending_memory_state: None,
            pending_thread_state: None,
            caught_signal_frames: Vec::new(),
            pending_lifecycle_exit: None,
            pending_statx: None,
            pending_xattr: None,
            pending_readlink: None,
            pending_access: None,
            pending_getcwd: None,
            pending_cwd_change: None,
            pending_path_removal: None,
            pending_directory_creation: None,
            pending_path_rename: None,
            pending_path_mode_change: None,
            pending_descriptor_timestamp_change: None,
            pending_symbolic_link_creation: None,
            pending_filesystem_status: None,
            pending_read: None,
            pending_getdents64: None,
            pending_mmap: None,
            pending_write: None,
            pending_unmodeled_syscall: None,
            active_syscall: None,
            syscall_entries: 0,
            syscall_exits: 0,
            terminal_syscalls: 0,
            allow_inherited_exit: false,
            restart_after_interrupt: false,
        },
    )]);
    let mut receipt_ids_by_pid = BTreeMap::from([(root_pid, root_id)]);
    let mut parents_by_receipt_id = BTreeMap::from([(root_id, None)]);
    let mut thread_groups_by_receipt_id = BTreeMap::from([(root_id, root_id)]);
    let mut newborn = BTreeSet::new();
    let mut early_newborn_stops = BTreeMap::new();
    let mut running = BTreeSet::new();
    let mut pending_statuses = VecDeque::new();
    let mut interrupted = BTreeSet::new();
    let mut exclusive = BTreeMap::new();
    continue_tracee(
        root_pid,
        Some(Signal::SIGCONT),
        &mut running,
        "continue observed root",
    )?;

    while !guard.active.is_empty() {
        let pending_index = pending_statuses.iter().position(|status| {
            wait_status_pid(*status)
                .is_some_and(|pid| tracee_can_advance(pid, &by_pid, &exclusive))
        });
        let status = if let Some(index) = pending_index {
            pending_statuses
                .remove(index)
                .context("selected pending tracee status is missing")?
        } else {
            let mut awaiting = running
                .iter()
                .copied()
                .filter(|pid| tracee_can_advance(*pid, &by_pid, &exclusive))
                .collect::<BTreeSet<_>>();
            awaiting.extend(
                newborn
                    .iter()
                    .copied()
                    .filter(|pid| tracee_can_advance(*pid, &by_pid, &exclusive)),
            );
            if awaiting.is_empty() {
                anyhow::bail!("process capture has active tracees but no pending stop");
            }
            wait_for_active_tracee(&awaiting)?
        };
        let status_pid = wait_status_pid(status).context("traced status has no PID")?;
        running.remove(&status_pid);
        let was_interrupted = interrupted.remove(&status_pid);

        if matches!(
            status,
            WaitStatus::PtraceEvent(pid, Signal::SIGTRAP, libc::PTRACE_EVENT_STOP)
                if pid == status_pid
                    && by_pid.contains_key(&pid)
                    && !newborn.contains(&pid)
        ) {
            continue_tracee(
                status_pid,
                None,
                &mut running,
                "continue interrupted tracee",
            )?;
            continue;
        }
        if let WaitStatus::PtraceSyscall(pid) = status {
            let stop = syscall_stop(pid)?;
            let process = by_pid
                .get_mut(&pid)
                .with_context(|| format!("syscall stop from unknown traced PID {pid}"))?;
            if was_interrupted
                && matches!(stop, SyscallStop::Exit(exit) if is_internal_restart(exit))
            {
                process.restart_after_interrupt = true;
                continue_tracee(
                    pid,
                    None,
                    &mut running,
                    "restart interrupted traced syscall",
                )?;
                continue;
            }
            if process.restart_after_interrupt {
                let SyscallStop::Entry { entry, .. } = stop else {
                    anyhow::bail!("interrupted syscall restart did not reenter the kernel");
                };
                let original = process
                    .active_syscall
                    .context("interrupted syscall restart has no original entry")?;
                if entry.number != libc::SYS_restart_syscall as u64
                    && entry.number != original.number
                {
                    anyhow::bail!("interrupted syscall restarted as an unrelated syscall");
                }
                process.restart_after_interrupt = false;
                continue_tracee(
                    pid,
                    None,
                    &mut running,
                    "continue reentered traced syscall",
                )?;
                continue;
            }
        }
        let requires_quiescence = match status {
            WaitStatus::PtraceSyscall(pid) => match syscall_stop(pid)? {
                SyscallStop::Entry { entry, .. } => {
                    !syscall_allows_shared_address_space_progress(entry.number)
                }
                SyscallStop::Exit(_) => by_pid
                    .get(&pid)
                    .and_then(|process| process.active_syscall)
                    .is_some_and(|entry| {
                        !syscall_allows_shared_address_space_progress(entry.number)
                    }),
            },
            WaitStatus::PtraceEvent(_, _, libc::PTRACE_EVENT_CLONE)
            | WaitStatus::PtraceEvent(_, _, libc::PTRACE_EVENT_FORK)
            | WaitStatus::PtraceEvent(_, _, libc::PTRACE_EVENT_VFORK) => false,
            _ => true,
        };
        if let Some(owner) = by_pid
            .get(&status_pid)
            .and_then(|process| exclusive.get(&process.address_space))
        {
            if *owner != status_pid {
                anyhow::bail!("non-owner tracee ran during exclusive buffer capture");
            }
        }
        if requires_quiescence {
            quiesce_shared_address_space(
                status_pid,
                &by_pid,
                &mut running,
                &mut pending_statuses,
                &mut interrupted,
            )?;
        }
        match status {
            WaitStatus::PtraceEvent(pid, Signal::SIGTRAP, libc::PTRACE_EVENT_STOP) => {
                if !by_pid.contains_key(&pid) {
                    if !guard.active.insert(pid) {
                        anyhow::bail!("early seized child PID was already active");
                    }
                    if early_newborn_stops.insert(pid, Signal::SIGTRAP).is_some() {
                        anyhow::bail!("duplicate early seized child stop");
                    }
                    continue;
                }
                if !newborn.remove(&pid) {
                    anyhow::bail!("unexpected seized tracee event stop");
                }
                continue_tracee(
                    pid,
                    None,
                    &mut running,
                    "continue newborn seized tracee",
                )?;
            }
            WaitStatus::PtraceEvent(
                pid,
                Signal::SIGTRAP,
                event @ (libc::PTRACE_EVENT_CLONE
                | libc::PTRACE_EVENT_FORK
                | libc::PTRACE_EVENT_VFORK),
            ) => {
                let child_raw = ptrace::getevent(pid).context("read traced child identity")?;
                let child_pid = Pid::from_raw(
                    i32::try_from(child_raw).context("traced child PID exceeds i32")?,
                );
                let parent_process = by_pid.get(&pid).with_context(|| {
                    format!("unknown traced parent PID {pid}")
                })?;
                let parent = parent_process.id;
                let child_address_space = parent_process
                    .pending_process_creation
                    .as_ref()
                    .filter(|pending| process_creation_shares_address_space(pending.request))
                    .map_or(next_id, |_| parent_process.address_space);
                let child_thread_group = parent_process
                    .pending_process_creation
                    .as_ref()
                    .filter(|pending| process_creation_shares_address_space(pending.request))
                    .map_or(next_id, |_| parent_process.thread_group);
                if !parent_process.caught_signal_frames.is_empty() {
                    anyhow::bail!(
                        "process creation from a caught signal frame is \
                         unsupported"
                    );
                }
                if let Some(pending) = &mut by_pid
                    .get_mut(&pid)
                    .with_context(|| format!("unknown traced parent PID {pid}"))?
                    .pending_process_creation
                {
                    let expected_event = match pending.request {
                        CapturedProcessCreationRequest::Clone { flags }
                        | CapturedProcessCreationRequest::Clone3 { flags, .. }
                            if supported_thread_clone_flags(flags) =>
                        {
                            libc::PTRACE_EVENT_CLONE
                        }
                        CapturedProcessCreationRequest::Clone { .. } => libc::PTRACE_EVENT_FORK,
                        CapturedProcessCreationRequest::Vfork
                        | CapturedProcessCreationRequest::Clone3 { .. } => libc::PTRACE_EVENT_VFORK,
                    };
                    if event != expected_event || pending.child.is_some() {
                        anyhow::bail!(
                            "supported process creation has an inconsistent ptrace event"
                        );
                    }
                    pending.child = Some((child_pid, next_id));
                }
                let stopped_early = early_newborn_stops.remove(&child_pid).is_some();
                if stopped_early {
                    if !guard.active.contains(&child_pid) {
                        anyhow::bail!("early traced child was absent from the termination guard");
                    }
                } else if !guard.active.insert(child_pid) {
                    anyhow::bail!("traced child PID was already active");
                }
                if by_pid
                    .insert(
                        child_pid,
                        PendingProcess {
                            id: next_id,
                            address_space: child_address_space,
                            thread_group: child_thread_group,
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
                            pending_exec: None,
                            pending_open: None,
                            pending_newfstatat: None,
                            pending_fstat: None,
                            pending_getrandom: None,
                            pending_runtime_observation: None,
                            pending_prlimit: None,
                            pending_close: None,
                            pending_seek: None,
                            pending_file_advice: None,
                            pending_fcntl: None,
                            pending_ioctl: None,
                            pending_pipe: None,
                            pending_epoll_create: None,
                            pending_socket_create: None,
                            pending_socket_connect: None,
                            pending_dup2: None,
                            pending_signal_state: None,
                            pending_umask: None,
                            pending_credential: None,
                            pending_futex_wake: None,
                            pending_process_creation: None,
                            pending_child_wait: None,
                            pending_process_identity: None,
                            pending_memory_state: None,
                            pending_thread_state: None,
                            caught_signal_frames: Vec::new(),
                            pending_lifecycle_exit: None,
                            pending_statx: None,
                            pending_xattr: None,
                            pending_readlink: None,
                            pending_access: None,
                            pending_getcwd: None,
                            pending_cwd_change: None,
                            pending_path_removal: None,
                            pending_directory_creation: None,
                            pending_path_rename: None,
                            pending_path_mode_change: None,
                            pending_descriptor_timestamp_change: None,
                            pending_symbolic_link_creation: None,
                            pending_filesystem_status: None,
                            pending_read: None,
                            pending_getdents64: None,
                            pending_mmap: None,
                            pending_write: None,
                            pending_unmodeled_syscall: None,
                            active_syscall: None,
                            syscall_entries: 0,
                            syscall_exits: 0,
                            terminal_syscalls: 0,
                            allow_inherited_exit: true,
                            restart_after_interrupt: false,
                        },
                    )
                    .is_some()
                {
                    anyhow::bail!("traced child PID was already active");
                }
                if receipt_ids_by_pid.insert(child_pid, next_id).is_some() {
                    anyhow::bail!("traced child PID was already assigned a receipt identity");
                }
                if parents_by_receipt_id
                    .insert(next_id, Some(parent))
                    .is_some()
                {
                    anyhow::bail!("traced child identity already had a parent");
                }
                if thread_groups_by_receipt_id
                    .insert(next_id, child_thread_group)
                    .is_some()
                {
                    anyhow::bail!("traced child identity already had a thread group");
                }
                sink.record(ProcessCaptureEvent::ProcessStarted {
                    process: next_id,
                    parent: Some(parent),
                })?;
                if let Some(pending) = by_pid
                    .get(&pid)
                    .and_then(|process| process.pending_process_creation.as_ref())
                {
                    sink.record(ProcessCaptureEvent::ProcessCreationAttempt {
                        process: parent,
                        attempt: CapturedProcessCreationAttempt {
                            syscall_index: pending.syscall_index,
                            request: pending.request,
                            outcome: CapturedProcessCreationOutcome::Created(next_id),
                        },
                    })?;
                }
                next_id = next_id
                    .checked_add(1)
                    .context("process capture identity overflowed")?;
                if stopped_early {
                    ptrace::setoptions(child_pid, TRACE_OPTIONS)
                        .context("configure early descendant process tracing")?;
                    continue_tracee(
                        child_pid,
                        None,
                        &mut running,
                        "continue early stopped traced process",
                    )?;
                } else {
                    newborn.insert(child_pid);
                }
                continue_tracee(
                    pid,
                    None,
                    &mut running,
                    "continue traced parent",
                )?;
            }
            WaitStatus::PtraceEvent(pid, Signal::SIGTRAP, libc::PTRACE_EVENT_EXEC) => {
                let former_tid = ptrace::getevent(pid).context("read pre-exec task identity")?;
                if former_tid != 0 && former_tid != pid.as_raw() as i64 {
                    anyhow::bail!(
                        "thread-group identity changed during exec; capture is unsupported"
                    );
                }
                let process = by_pid
                    .get_mut(&pid)
                    .with_context(|| format!("unknown execing traced PID {pid}"))?;
                if !matches!(
                    process.active_syscall,
                    Some(entry)
                        if entry.number == libc::SYS_execve as u64
                            || entry.number == libc::SYS_execveat as u64
                ) {
                    anyhow::bail!("exec event has no matching exec syscall entry");
                }
                let invocation_state =
                    read_invocation(pid, sink.max_event_bytes(), &mut mapped_file_witnesses)?;
                let invocation = process.invocations;
                process.invocations = process
                    .invocations
                    .checked_add(1)
                    .context("exec invocation index overflowed")?;
                sink.record(ProcessCaptureEvent::Invocation {
                    process: process.id,
                    invocation: invocation_state,
                })?;
                let pending = process
                    .pending_exec
                    .as_mut()
                    .context("exec event has no captured exec request")?;
                if pending.loaded_invocation.replace(invocation).is_some() {
                    anyhow::bail!("exec request loaded more than one process image");
                }
                process.caught_signal_frames.clear();
                continue_tracee(
                    pid,
                    None,
                    &mut running,
                    "continue execing traced process",
                )?;
            }
            WaitStatus::PtraceSyscall(pid) => {
                let has_address_space_siblings =
                    process_has_address_space_siblings(pid, &by_pid);
                let process = by_pid
                    .get_mut(&pid)
                    .with_context(|| format!("syscall stop from unknown traced PID {pid}"))?;
                match syscall_stop(pid)? {
                    SyscallStop::Entry {
                        entry,
                        stack_pointer,
                    } => {
                        process.allow_inherited_exit = false;
                        if process.active_syscall.is_some() {
                            anyhow::bail!("traced process entered a syscall before its prior exit");
                        }
                        reject_unsupported_clone_sharing(pid, entry)?;
                        if entry.number == libc::SYS_execve as u64
                            || entry.number == libc::SYS_execveat as u64
                        {
                            if process.pending_exec.is_some() {
                                anyhow::bail!("traced process has overlapping exec requests");
                            }
                            process.pending_exec = Some(PendingExec {
                                syscall_index: process.syscall_entries,
                                request: read_exec_request(pid, entry)?,
                                loaded_invocation: None,
                            });
                            process.exec_syscalls = process
                                .exec_syscalls
                                .checked_add(1)
                                .ok_or_else(|| anyhow::anyhow!("exec syscall census overflowed"))?;
                        }
                        if let Some(request) = read_open_request(pid, entry)? {
                            if process
                                .pending_open
                                .replace(PendingOpen {
                                    syscall_index: process.syscall_entries,
                                    request,
                                })
                                .is_some()
                            {
                                anyhow::bail!("traced process has overlapping open requests");
                            }
                            process.open_syscalls = process
                                .open_syscalls
                                .checked_add(1)
                                .ok_or_else(|| anyhow::anyhow!("open syscall census overflowed"))?;
                        }
                        if let Some((request, metadata_address)) =
                            read_newfstatat_request(pid, entry)?
                        {
                            if process
                                .pending_newfstatat
                                .replace(PendingNewfstatat {
                                    syscall_index: process.syscall_entries,
                                    request,
                                    metadata_address,
                                })
                                .is_some()
                            {
                                anyhow::bail!("traced process has overlapping newfstatat requests");
                            }
                            process.newfstatat_syscalls =
                                process.newfstatat_syscalls.checked_add(1).ok_or_else(|| {
                                    anyhow::anyhow!("newfstatat syscall census overflowed")
                                })?;
                        }
                        if let Some((request, metadata_address)) = read_fstat_request(pid, entry)? {
                            if process
                                .pending_fstat
                                .replace(PendingFstat {
                                    syscall_index: process.syscall_entries,
                                    request,
                                    metadata_address,
                                })
                                .is_some()
                            {
                                anyhow::bail!("traced process has overlapping fstat requests");
                            }
                            process.fstat_syscalls =
                                process.fstat_syscalls.checked_add(1).ok_or_else(|| {
                                    anyhow::anyhow!("fstat syscall census overflowed")
                                })?;
                        }
                        if let Some((request, buffer)) = read_getrandom_request(entry) {
                            if process
                                .pending_getrandom
                                .replace(PendingGetrandom {
                                    syscall_index: process.syscall_entries,
                                    request,
                                    buffer,
                                })
                                .is_some()
                            {
                                anyhow::bail!("traced process has overlapping getrandom requests");
                            }
                            process.getrandom_syscalls =
                                process.getrandom_syscalls.checked_add(1).ok_or_else(|| {
                                    anyhow::anyhow!("getrandom syscall census overflowed")
                                })?;
                        }
                        let runtime_observation =
                            read_poll_runtime_observation_request(pid, entry)?
                                .or_else(|| read_runtime_observation_request(pid, entry));
                        if let Some((request, output_address)) = runtime_observation
                        {
                            if process
                                .pending_runtime_observation
                                .replace(PendingRuntimeObservation {
                                    syscall_index: process.syscall_entries,
                                    request,
                                    output_address,
                                })
                                .is_some()
                            {
                                anyhow::bail!(
                                    "traced process has overlapping runtime observations"
                                );
                            }
                            process.runtime_observation_syscalls = process
                                .runtime_observation_syscalls
                                .checked_add(1)
                                .ok_or_else(|| {
                                    anyhow::anyhow!("runtime-observation syscall census overflowed")
                                })?;
                        }
                        if let Some((request, new_limit, old_limit_address)) =
                            read_prlimit_request(pid, entry)?
                        {
                            if process
                                .pending_prlimit
                                .replace(PendingPrlimit {
                                    syscall_index: process.syscall_entries,
                                    request,
                                    new_limit,
                                    old_limit_address,
                                })
                                .is_some()
                            {
                                anyhow::bail!("traced process has overlapping prlimit requests");
                            }
                            process.prlimit_syscalls =
                                process.prlimit_syscalls.checked_add(1).ok_or_else(|| {
                                    anyhow::anyhow!("prlimit syscall census overflowed")
                                })?;
                        }
                        if let Some((request, backing_file)) = read_close_request(pid, entry)? {
                            if process
                                .pending_close
                                .replace(PendingClose {
                                    syscall_index: process.syscall_entries,
                                    request,
                                    backing_file,
                                })
                                .is_some()
                            {
                                anyhow::bail!("traced process has overlapping close requests");
                            }
                            process.close_syscalls =
                                process.close_syscalls.checked_add(1).ok_or_else(|| {
                                    anyhow::anyhow!("close syscall census overflowed")
                                })?;
                        }
                        if let Some(request) = read_seek_request(pid, entry)? {
                            if process
                                .pending_seek
                                .replace(PendingSeek {
                                    syscall_index: process.syscall_entries,
                                    request,
                                })
                                .is_some()
                            {
                                anyhow::bail!("traced process has overlapping seek requests");
                            }
                            process.seek_syscalls = process
                                .seek_syscalls
                                .checked_add(1)
                                .ok_or_else(|| anyhow::anyhow!("seek syscall census overflowed"))?;
                        }
                        if let Some(request) = read_file_advice_request(pid, entry)? {
                            if process
                                .pending_file_advice
                                .replace(PendingFileAdvice {
                                    syscall_index: process.syscall_entries,
                                    request,
                                })
                                .is_some()
                            {
                                anyhow::bail!(
                                    "traced process has overlapping file-advice requests"
                                );
                            }
                            process.file_advice_syscalls = process
                                .file_advice_syscalls
                                .checked_add(1)
                                .ok_or_else(|| {
                                    anyhow::anyhow!("file-advice syscall census overflowed")
                                })?;
                        }
                        if let Some(request) = read_fcntl_request(pid, entry)? {
                            if process
                                .pending_fcntl
                                .replace(PendingFcntl {
                                    syscall_index: process.syscall_entries,
                                    request,
                                })
                                .is_some()
                            {
                                anyhow::bail!("traced process has overlapping fcntl requests");
                            }
                            process.fcntl_syscalls =
                                process.fcntl_syscalls.checked_add(1).ok_or_else(|| {
                                    anyhow::anyhow!("fcntl syscall census overflowed")
                                })?;
                        }
                        if let Some(request) = read_ioctl_request(pid, entry)? {
                            if process
                                .pending_ioctl
                                .replace(PendingIoctl {
                                    syscall_index: process.syscall_entries,
                                    request,
                                })
                                .is_some()
                            {
                                anyhow::bail!("traced process has overlapping ioctl requests");
                            }
                            process.ioctl_syscalls =
                                process.ioctl_syscalls.checked_add(1).ok_or_else(|| {
                                    anyhow::anyhow!("ioctl syscall census overflowed")
                                })?;
                        }
                        if let Some((request, descriptor_address)) = read_pipe_request(entry) {
                            if process
                                .pending_pipe
                                .replace(PendingPipe {
                                    syscall_index: process.syscall_entries,
                                    request,
                                    descriptor_address,
                                })
                                .is_some()
                            {
                                anyhow::bail!("traced process has overlapping pipe requests");
                            }
                            process.pipe_syscalls = process
                                .pipe_syscalls
                                .checked_add(1)
                                .ok_or_else(|| anyhow::anyhow!("pipe syscall census overflowed"))?;
                        }
                        if let Some(request) = read_epoll_create_request(entry) {
                            if process
                                .pending_epoll_create
                                .replace(PendingEpollCreate {
                                    syscall_index: process.syscall_entries,
                                    request,
                                })
                                .is_some()
                            {
                                anyhow::bail!(
                                    "traced process has overlapping epoll-create requests"
                                );
                            }
                            process.epoll_create_syscalls = process
                                .epoll_create_syscalls
                                .checked_add(1)
                                .ok_or_else(|| {
                                    anyhow::anyhow!("epoll-create syscall census overflowed")
                                })?;
                        }
                        if let Some(request) =
                            read_socket_create_request(entry)
                        {
                            if process
                                .pending_socket_create
                                .replace(PendingSocketCreate {
                                    syscall_index: process.syscall_entries,
                                    request,
                                })
                                .is_some()
                            {
                                anyhow::bail!(concat!(
                                    "traced process has overlapping ",
                                    "socket-create requests"
                                ));
                            }
                            process.socket_create_syscalls = process
                                .socket_create_syscalls
                                .checked_add(1)
                                .ok_or_else(|| {
                                    anyhow::anyhow!(concat!(
                                        "socket-create syscall census ",
                                        "overflowed"
                                    ))
                                })?;
                        }
                        if let Some(request) =
                            read_socket_connect_request(pid, entry)?
                        {
                            if process
                                .pending_socket_connect
                                .replace(PendingSocketConnect {
                                    syscall_index: process.syscall_entries,
                                    request,
                                })
                                .is_some()
                            {
                                anyhow::bail!(concat!(
                                    "traced process has overlapping ",
                                    "socket-connect requests"
                                ));
                            }
                            process.socket_connect_syscalls = process
                                .socket_connect_syscalls
                                .checked_add(1)
                                .ok_or_else(|| {
                                    anyhow::anyhow!(concat!(
                                        "socket-connect syscall census ",
                                        "overflowed"
                                    ))
                                })?;
                        }
                        if let Some((request, replaced_backing_file)) =
                            read_duplication_request(pid, entry)?
                        {
                            if process
                                .pending_dup2
                                .replace(PendingDup2 {
                                    syscall_index: process.syscall_entries,
                                    request,
                                    replaced_backing_file,
                                })
                                .is_some()
                            {
                                anyhow::bail!(
                                    "traced process has overlapping descriptor duplications"
                                );
                            }
                            process.dup2_syscalls = process
                                .dup2_syscalls
                                .checked_add(1)
                                .ok_or_else(|| {
                                    anyhow::anyhow!("descriptor duplication census overflowed")
                                })?;
                        }
                        if let Some((request, old_state_address)) =
                            read_signal_state_request(pid, entry)?
                        {
                            if process
                                .pending_signal_state
                                .replace(PendingSignalState {
                                    syscall_index: process.syscall_entries,
                                    request,
                                    old_state_address,
                                })
                                .is_some()
                            {
                                anyhow::bail!(
                                    "traced process has overlapping signal-state requests"
                                );
                            }
                            process.signal_state_syscalls = process
                                .signal_state_syscalls
                                .checked_add(1)
                                .ok_or_else(|| {
                                    anyhow::anyhow!("signal-state syscall census overflowed")
                                })?;
                        }
                        if let Some(requested) = read_umask_request(entry) {
                            if process
                                .pending_umask
                                .replace(PendingUmask {
                                    syscall_index: process.syscall_entries,
                                    requested,
                                })
                                .is_some()
                            {
                                anyhow::bail!("traced process has overlapping umask requests");
                            }
                            process.umask_syscalls =
                                process.umask_syscalls.checked_add(1).ok_or_else(|| {
                                    anyhow::anyhow!("umask syscall census overflowed")
                                })?;
                        }
                        if let Some((request, output_address)) = read_credential_request(entry) {
                            if process
                                .pending_credential
                                .replace(PendingCredential {
                                    syscall_index: process.syscall_entries,
                                    request,
                                    output_address,
                                })
                                .is_some()
                            {
                                anyhow::bail!("traced process has overlapping credential requests");
                            }
                            process.credential_syscalls =
                                process.credential_syscalls.checked_add(1).ok_or_else(|| {
                                    anyhow::anyhow!("credential syscall census overflowed")
                                })?;
                        }
                        if let Some(request) = read_futex_wake_request(entry) {
                            if process
                                .pending_futex_wake
                                .replace(PendingFutexWake {
                                    syscall_index: process.syscall_entries,
                                    request,
                                })
                                .is_some()
                            {
                                anyhow::bail!("traced process has overlapping private futex wakes");
                            }
                            process.futex_wake_syscalls =
                                process.futex_wake_syscalls.checked_add(1).ok_or_else(|| {
                                    anyhow::anyhow!("futex-wake syscall census overflowed")
                                })?;
                        }
                        if let Some(request) = read_process_creation_request(pid, entry)? {
                            if process
                                .pending_process_creation
                                .replace(PendingProcessCreation {
                                    syscall_index: process.syscall_entries,
                                    request,
                                    child: None,
                                })
                                .is_some()
                            {
                                anyhow::bail!("traced process has overlapping process creations");
                            }
                            process.process_creation_syscalls = process
                                .process_creation_syscalls
                                .checked_add(1)
                                .ok_or_else(|| {
                                    anyhow::anyhow!("process-creation syscall census overflowed")
                                })?;
                        }
                        if let Some((request, status_address)) = read_child_wait_request(
                            entry,
                            process.id,
                            &receipt_ids_by_pid,
                            &parents_by_receipt_id,
                        ) {
                            if process
                                .pending_child_wait
                                .replace(PendingChildWait {
                                    syscall_index: process.syscall_entries,
                                    request,
                                    status_address,
                                })
                                .is_some()
                            {
                                anyhow::bail!("traced process has overlapping child waits");
                            }
                            process.child_wait_syscalls =
                                process.child_wait_syscalls.checked_add(1).ok_or_else(|| {
                                    anyhow::anyhow!("child-wait syscall census overflowed")
                                })?;
                        }
                        if let Some((request, expected_process)) =
                            read_process_identity_request(
                                pid,
                                entry,
                                process.id,
                                process.thread_group,
                                &receipt_ids_by_pid,
                                &parents_by_receipt_id,
                                &thread_groups_by_receipt_id,
                            )
                        {
                            if process
                                .pending_process_identity
                                .replace(PendingProcessIdentity {
                                    syscall_index: process.syscall_entries,
                                    request,
                                    expected_process,
                                })
                                .is_some()
                            {
                                anyhow::bail!(
                                    "traced process has overlapping process-identity queries"
                                );
                            }
                            process.process_identity_syscalls = process
                                .process_identity_syscalls
                                .checked_add(1)
                                .ok_or_else(|| {
                                    anyhow::anyhow!("process-identity syscall census overflowed")
                                })?;
                        }
                        if let Some(request) = read_memory_state_request(entry) {
                            if process
                                .pending_memory_state
                                .replace(PendingMemoryState {
                                    syscall_index: process.syscall_entries,
                                    request,
                                })
                                .is_some()
                            {
                                anyhow::bail!(
                                    "traced process has overlapping memory-state requests"
                                );
                            }
                            process.memory_state_syscalls = process
                                .memory_state_syscalls
                                .checked_add(1)
                                .ok_or_else(|| {
                                    anyhow::anyhow!("memory-state syscall census overflowed")
                                })?;
                        }
                        let signal_return = read_signal_return_request(
                            pid,
                            entry,
                            stack_pointer,
                            &mut process.caught_signal_frames,
                        )?;
                        let thread_state = if let Some((request, value)) = signal_return {
                            Some((request, Some(value)))
                        } else if let Some(request) = read_signal_stack_request(pid, entry)? {
                            Some((request, None))
                        } else {
                            read_thread_state_request(entry).map(|request| (request, None))
                        };
                        if let Some((request, restored_return_value)) = thread_state {
                            if process
                                .pending_thread_state
                                .replace(PendingThreadState {
                                    syscall_index: process.syscall_entries,
                                    request,
                                    restored_return_value,
                                })
                                .is_some()
                            {
                                anyhow::bail!(
                                    "traced process has overlapping thread-state requests"
                                );
                            }
                            process.thread_state_syscalls = process
                                .thread_state_syscalls
                                .checked_add(1)
                                .ok_or_else(|| {
                                    anyhow::anyhow!("thread-state syscall census overflowed")
                                })?;
                        }
                        if let Some(request) = read_lifecycle_exit_request(entry) {
                            let open_file_values =
                                capture_exit_file_values(pid, &mut mapped_file_witnesses)?;
                            if process
                                .pending_lifecycle_exit
                                .replace(PendingLifecycleExit {
                                    syscall_index: process.syscall_entries,
                                    request,
                                    open_file_values,
                                })
                                .is_some()
                            {
                                anyhow::bail!(
                                    "traced process has overlapping lifecycle-exit requests"
                                );
                            }
                            process.lifecycle_exit_syscalls =
                                process.lifecycle_exit_syscalls.checked_add(1).ok_or_else(
                                    || anyhow::anyhow!("lifecycle-exit syscall census overflowed"),
                                )?;
                        }
                        if let Some((request, metadata_address)) = read_statx_request(pid, entry)? {
                            if process
                                .pending_statx
                                .replace(PendingStatx {
                                    syscall_index: process.syscall_entries,
                                    request,
                                    metadata_address,
                                })
                                .is_some()
                            {
                                anyhow::bail!("traced process has overlapping statx requests");
                            }
                            process.statx_syscalls =
                                process.statx_syscalls.checked_add(1).ok_or_else(|| {
                                    anyhow::anyhow!("statx syscall census overflowed")
                                })?;
                        }
                        if let Some((request, value_address)) = read_xattr_request(pid, entry)? {
                            if process
                                .pending_xattr
                                .replace(PendingXattr {
                                    syscall_index: process.syscall_entries,
                                    request,
                                    value_address,
                                })
                                .is_some()
                            {
                                anyhow::bail!("traced process has overlapping xattr requests");
                            }
                            process.xattr_syscalls =
                                process.xattr_syscalls.checked_add(1).ok_or_else(|| {
                                    anyhow::anyhow!("xattr syscall census overflowed")
                                })?;
                        }
                        if let Some((request, buffer_address)) = read_readlink_request(pid, entry)?
                        {
                            if process
                                .pending_readlink
                                .replace(PendingReadlink {
                                    syscall_index: process.syscall_entries,
                                    request,
                                    buffer_address,
                                })
                                .is_some()
                            {
                                anyhow::bail!("traced process has overlapping readlink requests");
                            }
                            process.readlink_syscalls =
                                process.readlink_syscalls.checked_add(1).ok_or_else(|| {
                                    anyhow::anyhow!("readlink syscall census overflowed")
                                })?;
                        }
                        if let Some(request) = read_access_request(pid, entry)? {
                            if process
                                .pending_access
                                .replace(PendingAccess {
                                    syscall_index: process.syscall_entries,
                                    request,
                                })
                                .is_some()
                            {
                                anyhow::bail!("traced process has overlapping access requests");
                            }
                            process.access_syscalls =
                                process.access_syscalls.checked_add(1).ok_or_else(|| {
                                    anyhow::anyhow!("access syscall census overflowed")
                                })?;
                        }
                        if entry.number == libc::SYS_getcwd as u64 {
                            if process
                                .pending_getcwd
                                .replace(PendingGetcwd {
                                    syscall_index: process.syscall_entries,
                                    request: CapturedGetcwdRequest {
                                        requested_bytes: entry.arguments[1],
                                    },
                                    buffer_address: entry.arguments[0],
                                })
                                .is_some()
                            {
                                anyhow::bail!("traced process has overlapping getcwd requests");
                            }
                            process.getcwd_syscalls =
                                process.getcwd_syscalls.checked_add(1).ok_or_else(|| {
                                    anyhow::anyhow!("getcwd syscall census overflowed")
                                })?;
                        }
                        if let Some(request) = read_cwd_change_request(pid, entry)? {
                            if process
                                .pending_cwd_change
                                .replace(PendingCwdChange {
                                    syscall_index: process.syscall_entries,
                                    request,
                                })
                                .is_some()
                            {
                                anyhow::bail!("traced process has overlapping cwd-change requests");
                            }
                            process.cwd_change_syscalls =
                                process.cwd_change_syscalls.checked_add(1).ok_or_else(|| {
                                    anyhow::anyhow!("cwd-change syscall census overflowed")
                                })?;
                        }
                        if let Some(request) = read_path_removal_request(pid, entry)? {
                            if process
                                .pending_path_removal
                                .replace(PendingPathRemoval {
                                    syscall_index: process.syscall_entries,
                                    request,
                                })
                                .is_some()
                            {
                                anyhow::bail!(
                                    "traced process has overlapping path-removal requests"
                                );
                            }
                            process.path_removal_syscalls = process
                                .path_removal_syscalls
                                .checked_add(1)
                                .ok_or_else(|| {
                                    anyhow::anyhow!("path-removal syscall census overflowed")
                                })?;
                        }
                        if let Some(request) = read_directory_creation_request(pid, entry)? {
                            if process
                                .pending_directory_creation
                                .replace(PendingDirectoryCreation {
                                    syscall_index: process.syscall_entries,
                                    request,
                                })
                                .is_some()
                            {
                                anyhow::bail!(
                                    "traced process has overlapping directory-creation requests"
                                );
                            }
                            process.directory_creation_syscalls = process
                                .directory_creation_syscalls
                                .checked_add(1)
                                .ok_or_else(|| {
                                    anyhow::anyhow!("directory-creation syscall census overflowed")
                                })?;
                        }
                        if let Some(request) = read_path_rename_request(pid, entry)? {
                            if process
                                .pending_path_rename
                                .replace(PendingPathRename {
                                    syscall_index: process.syscall_entries,
                                    request,
                                })
                                .is_some()
                            {
                                anyhow::bail!(
                                    "traced process has overlapping path-rename requests"
                                );
                            }
                            process.path_rename_syscalls =
                                process.path_rename_syscalls.checked_add(1).ok_or_else(|| {
                                    anyhow::anyhow!("path-rename syscall census overflowed")
                                })?;
                        }
                        if let Some(request) = read_path_mode_change_request(pid, entry)? {
                            if process
                                .pending_path_mode_change
                                .replace(PendingPathModeChange {
                                    syscall_index: process.syscall_entries,
                                    request,
                                })
                                .is_some()
                            {
                                anyhow::bail!(
                                    "traced process has overlapping path-mode-change requests"
                                );
                            }
                            process.path_mode_change_syscalls = process
                                .path_mode_change_syscalls
                                .checked_add(1)
                                .ok_or_else(|| {
                                    anyhow::anyhow!("path-mode-change syscall census overflowed")
                                })?;
                        }
                        if let Some(request) = read_descriptor_timestamp_change_request(pid, entry)?
                        {
                            if process
                                .pending_descriptor_timestamp_change
                                .replace(PendingDescriptorTimestampChange {
                                    syscall_index: process.syscall_entries,
                                    request,
                                })
                                .is_some()
                            {
                                anyhow::bail!(
                                    "traced process has overlapping timestamp-change requests"
                                );
                            }
                            process.descriptor_timestamp_change_syscalls = process
                                .descriptor_timestamp_change_syscalls
                                .checked_add(1)
                                .ok_or_else(|| {
                                    anyhow::anyhow!("descriptor timestamp-change census overflowed")
                                })?;
                        }
                        if let Some(request) = read_symbolic_link_creation_request(pid, entry)? {
                            if process
                                .pending_symbolic_link_creation
                                .replace(PendingSymbolicLinkCreation {
                                    syscall_index: process.syscall_entries,
                                    request,
                                })
                                .is_some()
                            {
                                anyhow::bail!(
                                    "traced process has overlapping symbolic-link requests"
                                );
                            }
                            process.symbolic_link_creation_syscalls = process
                                .symbolic_link_creation_syscalls
                                .checked_add(1)
                                .ok_or_else(|| {
                                    anyhow::anyhow!(
                                        "symbolic-link creation syscall census overflowed"
                                    )
                                })?;
                        }
                        if let Some((request, buffer_address)) =
                            read_filesystem_status_request(pid, entry)?
                        {
                            if process
                                .pending_filesystem_status
                                .replace(PendingFilesystemStatus {
                                    syscall_index: process.syscall_entries,
                                    request,
                                    buffer_address,
                                })
                                .is_some()
                            {
                                anyhow::bail!(
                                    "traced process has overlapping filesystem-status requests"
                                );
                            }
                            process.filesystem_status_syscalls = process
                                .filesystem_status_syscalls
                                .checked_add(1)
                                .ok_or_else(|| {
                                    anyhow::anyhow!("filesystem-status syscall census overflowed")
                                })?;
                        }
                        if let Some((request, buffers, backing_file)) =
                            read_read_request(pid, entry)?
                        {
                            let requires_shared_progress = has_address_space_siblings
                                && request.requested_bytes != 0
                                && request.identity.as_ref().is_some_and(|identity| {
                                    matches!(
                                        identity.kind,
                                        CapturedDescriptorKind::Fifo
                                            | CapturedDescriptorKind::Socket
                                    )
                                })
                                && shared_descriptor_read_requires_progress(
                                    pid,
                                    request.descriptor,
                                    request.identity.as_ref().unwrap(),
                                )?;
                            if requires_shared_progress {
                                anyhow::bail!(
                                    "shared-address-space read with a writable sibling endpoint is not safely observable"
                                );
                            }
                            if process
                                .pending_read
                                .replace(PendingRead {
                                    syscall_index: process.syscall_entries,
                                    request,
                                    buffers,
                                    backing_file,
                                })
                                .is_some()
                            {
                                anyhow::bail!("traced process has overlapping read requests");
                            }
                            process.read_syscalls = process
                                .read_syscalls
                                .checked_add(1)
                                .ok_or_else(|| anyhow::anyhow!("read syscall census overflowed"))?;
                        }
                        if let Some((request, buffer_address, backing_directory)) =
                            read_getdents64_request(pid, entry)?
                        {
                            if process
                                .pending_getdents64
                                .replace(PendingGetdents64 {
                                    syscall_index: process.syscall_entries,
                                    request,
                                    buffer_address,
                                    backing_directory,
                                })
                                .is_some()
                            {
                                anyhow::bail!("traced process has overlapping getdents64 requests");
                            }
                            process.getdents64_syscalls =
                                process.getdents64_syscalls.checked_add(1).ok_or_else(|| {
                                    anyhow::anyhow!("getdents64 syscall census overflowed")
                                })?;
                        }
                        if let Some((request, backing_file)) = read_mmap_request(pid, entry)? {
                            if process
                                .pending_mmap
                                .replace(PendingMmap {
                                    syscall_index: process.syscall_entries,
                                    request,
                                    backing_file,
                                })
                                .is_some()
                            {
                                anyhow::bail!("traced process has overlapping mmap requests");
                            }
                            process.mmap_syscalls = process
                                .mmap_syscalls
                                .checked_add(1)
                                .ok_or_else(|| anyhow::anyhow!("mmap syscall census overflowed"))?;
                        }
                        if let Some((request, buffer)) = read_write_request(pid, entry)? {
                            if process
                                .pending_write
                                .replace(PendingWrite {
                                    syscall_index: process.syscall_entries,
                                    request,
                                    buffer,
                                })
                                .is_some()
                            {
                                anyhow::bail!("traced process has overlapping write requests");
                            }
                            process.write_syscalls =
                                process.write_syscalls.checked_add(1).ok_or_else(|| {
                                    anyhow::anyhow!("write syscall census overflowed")
                                })?;
                        }
                        if !current_syscall_has_model(process, process.syscall_entries) {
                            if process
                                .pending_unmodeled_syscall
                                .replace(PendingUnmodeledSyscall {
                                    syscall_index: process.syscall_entries,
                                    number: entry.number,
                                    arguments: entry.arguments,
                                })
                                .is_some()
                            {
                                anyhow::bail!("traced process has overlapping unmodeled syscalls");
                            }
                            process.unmodeled_syscalls =
                                process.unmodeled_syscalls.checked_add(1).ok_or_else(|| {
                                    anyhow::anyhow!("unmodeled syscall census overflowed")
                                })?;
                        }
                        process.active_syscall = Some(entry);
                        process.syscall_entries = process
                            .syscall_entries
                            .checked_add(1)
                            .ok_or_else(|| anyhow::anyhow!("syscall entry census overflowed"))?;
                        if has_address_space_siblings
                            && !syscall_allows_shared_address_space_progress(entry.number)
                        {
                            if exclusive.insert(process.address_space, pid).is_some() {
                                anyhow::bail!(
                                    "address space already has an exclusive buffer capture"
                                );
                            }
                        }
                    }
                    SyscallStop::Exit(exit) => {
                        if let Some(entry) = process.active_syscall.take() {
                            process.syscall_exits = process
                                .syscall_exits
                                .checked_add(1)
                                .ok_or_else(|| anyhow::anyhow!("syscall exit census overflowed"))?;
                            if entry.number == libc::SYS_execve as u64
                                || entry.number == libc::SYS_execveat as u64
                            {
                                finish_exec_attempt(process, exit, sink)?;
                            }
                            if open_system_call(entry.number).is_some() {
                                finish_open_attempt(
                                    process,
                                    pid,
                                    exit,
                                    sink,
                                    &mut mapped_file_witnesses,
                                )?;
                            }
                            if entry.number == libc::SYS_newfstatat as u64 {
                                finish_newfstatat_attempt(process, pid, exit, sink)?;
                            }
                            if entry.number == libc::SYS_fstat as u64 {
                                finish_fstat_attempt(process, pid, exit, sink)?;
                            }
                            if entry.number == libc::SYS_getrandom as u64 {
                                finish_getrandom_attempt(process, pid, exit, sink)?;
                            }
                            if process.pending_runtime_observation.is_some() {
                                finish_runtime_observation_attempt(process, pid, exit, sink)?;
                            }
                            if entry.number == libc::SYS_prlimit64 as u64 {
                                finish_prlimit_attempt(process, pid, exit, sink)?;
                            }
                            if entry.number == libc::SYS_close as u64 {
                                finish_close_attempt(
                                    process,
                                    exit,
                                    sink,
                                    &mut mapped_file_witnesses,
                                )?;
                            }
                            if entry.number == libc::SYS_lseek as u64 {
                                finish_seek_attempt(process, exit, sink)?;
                            }
                            if process.pending_file_advice.is_some() {
                                finish_file_advice_attempt(process, exit, sink)?;
                            }
                            if entry.number == libc::SYS_fcntl as u64 {
                                finish_fcntl_attempt(pid, process, exit, sink)?;
                            }
                            if process.pending_ioctl.is_some() {
                                finish_ioctl_attempt(process, exit, sink)?;
                            }
                            if entry.number == libc::SYS_pipe2 as u64 {
                                finish_pipe_attempt(pid, process, exit, sink)?;
                            }
                            if process.pending_epoll_create.is_some() {
                                finish_epoll_create_attempt(pid, process, exit, sink)?;
                            }
                            if process.pending_socket_create.is_some() {
                                finish_socket_create_attempt(
                                    pid, process, exit, sink,
                                )?;
                            }
                            if process.pending_socket_connect.is_some() {
                                finish_socket_connect_attempt(
                                    pid, process, exit, sink,
                                )?;
                            }
                            if process.pending_dup2.is_some() {
                                finish_dup2_attempt(
                                    pid,
                                    process,
                                    exit,
                                    sink,
                                    &mut mapped_file_witnesses,
                                )?;
                            }
                            if process.pending_signal_state.is_some() {
                                finish_signal_state_attempt(pid, process, exit, sink)?;
                            }
                            if entry.number == libc::SYS_umask as u64 {
                                finish_umask_attempt(process, exit, sink)?;
                            }
                            if process.pending_credential.is_some() {
                                finish_credential_attempt(pid, process, exit, sink)?;
                            }
                            if process.pending_futex_wake.is_some() {
                                finish_futex_wake_attempt(process, exit, sink)?;
                            }
                            if process.pending_process_creation.is_some() {
                                finish_process_creation_attempt(process, exit, sink)?;
                            }
                            if process.pending_child_wait.is_some() {
                                finish_child_wait_attempt(
                                    pid,
                                    process,
                                    exit,
                                    &receipt_ids_by_pid,
                                    sink,
                                )?;
                            }
                            if process.pending_process_identity.is_some() {
                                finish_process_identity_attempt(pid, process, exit, sink)?;
                            }
                            if read_memory_state_request(entry).is_some() {
                                finish_memory_state_attempt(process, exit, sink)?;
                            }
                            if process.pending_thread_state.is_some() {
                                finish_thread_state_attempt(process, exit, sink)?;
                            }
                            if entry.number == libc::SYS_statx as u64 {
                                finish_statx_attempt(process, pid, exit, sink)?;
                            }
                            if xattr_system_call(entry.number).is_some() {
                                finish_xattr_attempt(process, pid, exit, sink)?;
                            }
                            if readlink_system_call(entry.number).is_some() {
                                finish_readlink_attempt(process, pid, exit, sink)?;
                            }
                            if access_system_call(entry.number).is_some() {
                                finish_access_attempt(process, exit, sink)?;
                            }
                            if entry.number == libc::SYS_getcwd as u64 {
                                finish_getcwd_attempt(process, pid, exit, sink)?;
                            }
                            if cwd_change_system_call(entry.number).is_some() {
                                finish_cwd_change_attempt(process, pid, exit, sink)?;
                            }
                            if path_removal_system_call(entry.number).is_some() {
                                finish_path_removal_attempt(process, exit, sink)?;
                            }
                            if directory_creation_system_call(entry.number).is_some() {
                                finish_directory_creation_attempt(process, exit, sink)?;
                            }
                            if path_rename_system_call(entry.number).is_some() {
                                finish_path_rename_attempt(process, exit, sink)?;
                            }
                            if process.pending_path_mode_change.is_some() {
                                finish_path_mode_change_attempt(process, exit, sink)?;
                            }
                            if entry.number == libc::SYS_utimensat as u64 {
                                finish_descriptor_timestamp_change_attempt(process, exit, sink)?;
                            }
                            if symbolic_link_creation_system_call(entry.number).is_some() {
                                finish_symbolic_link_creation_attempt(process, exit, sink)?;
                            }
                            if filesystem_status_system_call(entry.number).is_some() {
                                finish_filesystem_status_attempt(process, pid, exit, sink)?;
                            }
                            if read_system_call(entry.number).is_some() {
                                finish_read_attempt(process, pid, exit, sink)?;
                            }
                            if entry.number == libc::SYS_getdents64 as u64 {
                                finish_getdents64_attempt(process, pid, exit, sink)?;
                            }
                            if entry.number == libc::SYS_mmap as u64 {
                                finish_mmap_attempt(
                                    process,
                                    exit,
                                    sink,
                                    &mut mapped_file_witnesses,
                                )?;
                            }
                            if write_system_call(entry.number).is_some() {
                                finish_write_attempt(process, pid, exit, sink)?;
                            }
                            if process.pending_unmodeled_syscall.is_some() {
                                finish_unmodeled_syscall_attempt(process, exit, sink)?;
                            }
                            if exclusive.get(&process.address_space) == Some(&pid) {
                                exclusive.remove(&process.address_space);
                            }
                        } else if process.allow_inherited_exit {
                            process.allow_inherited_exit = false;
                        } else {
                            anyhow::bail!("traced process exited a syscall without its entry");
                        }
                    }
                }
                continue_tracee(
                    pid,
                    None,
                    &mut running,
                    "continue traced syscall",
                )?;
            }
            WaitStatus::Stopped(pid, signal) => {
                if !by_pid.contains_key(&pid) {
                    if signal != Signal::SIGSTOP {
                        anyhow::bail!("non-SIGSTOP stop from unknown traced PID {pid}");
                    }
                    if !guard.active.insert(pid) {
                        anyhow::bail!("early traced child PID was already active");
                    }
                    if early_newborn_stops.insert(pid, signal).is_some() {
                        anyhow::bail!("duplicate early traced child stop");
                    }
                    continue;
                }
                ptrace::setoptions(pid, TRACE_OPTIONS)
                    .context("configure descendant process tracing")?;
                let deliver = if newborn.remove(&pid) && signal == Signal::SIGSTOP {
                    None
                } else {
                    let process = by_pid
                        .get_mut(&pid)
                        .with_context(|| format!("signal stop from unknown traced PID {pid}"))?;
                    let delivery = read_signal_delivery(pid, signal, process, &receipt_ids_by_pid)?;
                    if delivery.disposition
                        == CapturedSignalDisposition::Caught
                    {
                        process
                            .caught_signal_frames
                            .push(process.signal_deliveries);
                    }
                    process.signal_deliveries = process
                        .signal_deliveries
                        .checked_add(1)
                        .context("signal-delivery census overflowed")?;
                    sink.record(ProcessCaptureEvent::SignalDelivery {
                        process: process.id,
                        delivery,
                    })?;
                    Some(signal)
                };
                continue_tracee(
                    pid,
                    deliver,
                    &mut running,
                    "continue stopped traced process",
                )?;
            }
            WaitStatus::Exited(pid, code) => {
                interrupted.remove(&pid);
                if let Some(address_space) = by_pid.get(&pid).map(|process| process.address_space) {
                    if exclusive.get(&address_space) == Some(&pid) {
                        exclusive.remove(&address_space);
                    }
                }
                finish_process(
                    pid,
                    CapturedProcessOutcome::Exited(code),
                    &mut by_pid,
                    &mut guard,
                    sink,
                )?;
            }
            WaitStatus::Signaled(pid, signal, _) => {
                interrupted.remove(&pid);
                if let Some(address_space) = by_pid.get(&pid).map(|process| process.address_space) {
                    if exclusive.get(&address_space) == Some(&pid) {
                        exclusive.remove(&address_space);
                    }
                }
                finish_process(
                    pid,
                    CapturedProcessOutcome::Signaled(signal as i32),
                    &mut by_pid,
                    &mut guard,
                    sink,
                )?;
            }
            WaitStatus::Continued(_) | WaitStatus::StillAlive => {}
            other => anyhow::bail!("unsupported traced process state: {other:?}"),
        }
    }
    if !by_pid.is_empty() {
        anyhow::bail!("process capture ended with incomplete lifecycles");
    }
    if !early_newborn_stops.is_empty() {
        anyhow::bail!("process capture ended with unannounced traced children");
    }
    Ok(root_id)
}

fn finish_process(
    pid: Pid,
    outcome: CapturedProcessOutcome,
    by_pid: &mut BTreeMap<Pid, PendingProcess>,
    guard: &mut TraceGuard,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let mut process = by_pid
        .remove(&pid)
        .with_context(|| format!("terminal state from unknown traced PID {pid}"))?;
    guard.active.remove(&pid);
    if let Some(pending) = process.pending_exec.take() {
        let Some(invocation) = pending.loaded_invocation else {
            anyhow::bail!("process terminated before its exec result was observed");
        };
        sink.record(ProcessCaptureEvent::ExecAttempt {
            process: process.id,
            attempt: CapturedExecAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedExecOutcome::Loaded(invocation),
            },
        })?;
    }
    if process.pending_open.is_some()
        && !matches!(process.active_syscall, Some(entry) if open_system_call(entry.number).is_some())
    {
        anyhow::bail!("terminal open request does not match the active syscall");
    }
    if let Some(pending) = process.pending_open.take() {
        sink.record(ProcessCaptureEvent::OpenAttempt {
            process: process.id,
            attempt: CapturedOpenAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedOpenOutcome::Terminated,
            },
        })?;
    }
    if process.pending_newfstatat.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if entry.number == libc::SYS_newfstatat as u64
        )
    {
        anyhow::bail!("terminal newfstatat request does not match the active syscall");
    }
    if let Some(pending) = process.pending_newfstatat.take() {
        sink.record(ProcessCaptureEvent::NewfstatatAttempt {
            process: process.id,
            attempt: CapturedNewfstatatAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedNewfstatatOutcome::Terminated,
            },
        })?;
    }
    if process.pending_fstat.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if entry.number == libc::SYS_fstat as u64
        )
    {
        anyhow::bail!("terminal fstat request does not match the active syscall");
    }
    if let Some(pending) = process.pending_fstat.take() {
        sink.record(ProcessCaptureEvent::FstatAttempt {
            process: process.id,
            attempt: CapturedFstatAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedFstatOutcome::Terminated,
            },
        })?;
    }
    if process.pending_getrandom.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if entry.number == libc::SYS_getrandom as u64
        )
    {
        anyhow::bail!("terminal getrandom request does not match the active syscall");
    }
    if let Some(pending) = process.pending_getrandom.take() {
        sink.record(ProcessCaptureEvent::GetrandomAttempt {
            process: process.id,
            attempt: CapturedGetrandomAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedGetrandomOutcome::Terminated,
            },
        })?;
    }
    if process.pending_runtime_observation.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if runtime_observation_system_call(entry.number)
        )
    {
        anyhow::bail!("terminal runtime observation does not match the active syscall");
    }
    if let Some(pending) = process.pending_runtime_observation.take() {
        sink.record(ProcessCaptureEvent::RuntimeObservationAttempt {
            process: process.id,
            attempt: CapturedRuntimeObservationAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedRuntimeObservationOutcome::Terminated,
            },
        })?;
    }
    if process.pending_prlimit.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if entry.number == libc::SYS_prlimit64 as u64
        )
    {
        anyhow::bail!("terminal prlimit request does not match the active syscall");
    }
    if let Some(pending) = process.pending_prlimit.take() {
        let outcome = match pending.new_limit {
            Some(requested) => CapturedPrlimitOutcome::MutationTerminated { requested },
            None => CapturedPrlimitOutcome::Terminated,
        };
        sink.record(ProcessCaptureEvent::PrlimitAttempt {
            process: process.id,
            attempt: CapturedPrlimitAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome,
            },
        })?;
    }
    if process.pending_close.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if entry.number == libc::SYS_close as u64
        )
    {
        anyhow::bail!("terminal close request does not match the active syscall");
    }
    if let Some(pending) = process.pending_close.take() {
        sink.record(ProcessCaptureEvent::CloseAttempt {
            process: process.id,
            attempt: CapturedCloseAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedCloseOutcome::Terminated,
            },
        })?;
    }
    if process.pending_seek.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if entry.number == libc::SYS_lseek as u64
        )
    {
        anyhow::bail!("terminal seek request does not match the active syscall");
    }
    if let Some(pending) = process.pending_seek.take() {
        sink.record(ProcessCaptureEvent::SeekAttempt {
            process: process.id,
            attempt: CapturedSeekAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedSeekOutcome::Terminated,
            },
        })?;
    }
    if process.pending_file_advice.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if entry.number == libc::SYS_fadvise64 as u64
        )
    {
        anyhow::bail!("terminal file-advice request does not match the active syscall");
    }
    if let Some(pending) = process.pending_file_advice.take() {
        sink.record(ProcessCaptureEvent::FileAdviceAttempt {
            process: process.id,
            attempt: CapturedFileAdviceAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedFileAdviceOutcome::Terminated,
            },
        })?;
    }
    if process.pending_fcntl.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if entry.number == libc::SYS_fcntl as u64
        )
    {
        anyhow::bail!("terminal fcntl request does not match the active syscall");
    }
    if let Some(pending) = process.pending_fcntl.take() {
        sink.record(ProcessCaptureEvent::FcntlAttempt {
            process: process.id,
            attempt: CapturedFcntlAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedFcntlOutcome::Terminated,
            },
        })?;
    }
    if process.pending_ioctl.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if entry.number == libc::SYS_ioctl as u64
        )
    {
        anyhow::bail!("terminal ioctl request does not match the active syscall");
    }
    if let Some(pending) = process.pending_ioctl.take() {
        sink.record(ProcessCaptureEvent::IoctlAttempt {
            process: process.id,
            attempt: CapturedIoctlAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedIoctlOutcome::Terminated,
            },
        })?;
    }
    if process.pending_pipe.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if entry.number == libc::SYS_pipe2 as u64
        )
    {
        anyhow::bail!("terminal pipe request does not match the active syscall");
    }
    if let Some(pending) = process.pending_pipe.take() {
        sink.record(ProcessCaptureEvent::PipeAttempt {
            process: process.id,
            attempt: CapturedPipeAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedPipeOutcome::Terminated,
            },
        })?;
    }
    if process.pending_epoll_create.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if entry.number == libc::SYS_epoll_create1 as u64
        )
    {
        anyhow::bail!("terminal epoll-create request does not match the active syscall");
    }
    if let Some(pending) = process.pending_epoll_create.take() {
        sink.record(ProcessCaptureEvent::EpollCreateAttempt {
            process: process.id,
            attempt: CapturedEpollCreateAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedEpollCreateOutcome::Terminated,
            },
        })?;
    }
    if process.pending_socket_create.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if entry.number == libc::SYS_socket as u64
        )
    {
        anyhow::bail!(
            "terminal socket-create request does not match the active syscall"
        );
    }
    if let Some(pending) = process.pending_socket_create.take() {
        sink.record(ProcessCaptureEvent::SocketCreateAttempt {
            process: process.id,
            attempt: CapturedSocketCreateAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedSocketCreateOutcome::Terminated,
            },
        })?;
    }
    if process.pending_socket_connect.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if entry.number == libc::SYS_connect as u64
        )
    {
        anyhow::bail!(
            "terminal socket-connect request does not match the active syscall"
        );
    }
    if let Some(pending) = process.pending_socket_connect.take() {
        sink.record(ProcessCaptureEvent::SocketConnectAttempt {
            process: process.id,
            attempt: CapturedSocketConnectAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedSocketConnectOutcome::Terminated,
            },
        })?;
    }
    if process.pending_dup2.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if descriptor_duplication_system_call(entry.number).is_some()
        )
    {
        anyhow::bail!("terminal descriptor duplication does not match the active syscall");
    }
    if let Some(pending) = process.pending_dup2.take() {
        sink.record(ProcessCaptureEvent::Dup2Attempt {
            process: process.id,
            attempt: CapturedDup2Attempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedDup2Outcome::Terminated,
            },
        })?;
    }
    if process.pending_signal_state.is_some()
        && process.active_syscall.is_none_or(|entry| {
            process
                .pending_signal_state
                .as_ref()
                .is_none_or(|_| !signal_state_system_call(entry.number))
        })
    {
        anyhow::bail!("terminal signal-state request does not match the active syscall");
    }
    if let Some(pending) = process.pending_signal_state.take() {
        sink.record(ProcessCaptureEvent::SignalStateAttempt {
            process: process.id,
            attempt: CapturedSignalStateAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedSignalStateOutcome::Terminated,
            },
        })?;
    }
    if process.pending_umask.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if entry.number == libc::SYS_umask as u64
        )
    {
        anyhow::bail!("terminal umask request does not match the active syscall");
    }
    if let Some(pending) = process.pending_umask.take() {
        sink.record(ProcessCaptureEvent::UmaskAttempt {
            process: process.id,
            attempt: CapturedUmaskAttempt {
                syscall_index: pending.syscall_index,
                requested: pending.requested,
                outcome: CapturedUmaskOutcome::Terminated,
            },
        })?;
    }
    if process.pending_credential.is_some()
        && process
            .active_syscall
            .is_none_or(|entry| read_credential_request(entry).is_none())
    {
        anyhow::bail!("terminal credential request does not match the active syscall");
    }
    if let Some(pending) = process.pending_credential.take() {
        sink.record(ProcessCaptureEvent::CredentialAttempt {
            process: process.id,
            attempt: CapturedCredentialAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedCredentialOutcome::Terminated,
            },
        })?;
    }
    if process.pending_futex_wake.is_some()
        && process
            .active_syscall
            .is_none_or(|entry| read_futex_wake_request(entry).is_none())
    {
        anyhow::bail!("terminal futex-wake request does not match the active syscall");
    }
    if let Some(pending) = process.pending_futex_wake.take() {
        sink.record(ProcessCaptureEvent::FutexWakeAttempt {
            process: process.id,
            attempt: CapturedFutexWakeAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedFutexWakeOutcome::Terminated,
            },
        })?;
    }
    if process.pending_process_creation.is_some()
        && process.active_syscall.is_none_or(|entry| {
            !process_creation_entry_matches_request(
                entry,
                process.pending_process_creation.as_ref().unwrap().request,
            )
        })
    {
        anyhow::bail!("terminal process-creation request does not match the active syscall");
    }
    if let Some(pending) = process.pending_process_creation.take() {
        if pending.child.is_none() {
            sink.record(ProcessCaptureEvent::ProcessCreationAttempt {
                process: process.id,
                attempt: CapturedProcessCreationAttempt {
                    syscall_index: pending.syscall_index,
                    request: pending.request,
                    outcome: CapturedProcessCreationOutcome::Terminated,
                },
            })?;
        }
    }
    if process.pending_child_wait.is_some()
        && process
            .active_syscall
            .is_none_or(|entry| entry.number != libc::SYS_wait4 as u64)
    {
        anyhow::bail!("terminal child-wait request does not match the active syscall");
    }
    if let Some(pending) = process.pending_child_wait.take() {
        sink.record(ProcessCaptureEvent::ChildWaitAttempt {
            process: process.id,
            attempt: CapturedChildWaitAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedChildWaitOutcome::Terminated,
            },
        })?;
    }
    if process.pending_process_identity.is_some()
        && process.active_syscall.is_none_or(|entry| {
            !process_identity_entry_matches_request(
                entry,
                process.pending_process_identity.as_ref().unwrap().request,
            )
        })
    {
        anyhow::bail!("terminal process-identity request does not match the active syscall");
    }
    if let Some(pending) = process.pending_process_identity.take() {
        sink.record(ProcessCaptureEvent::ProcessIdentityAttempt {
            process: process.id,
            attempt: CapturedProcessIdentityAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedProcessIdentityOutcome::Terminated,
            },
        })?;
    }
    if process.pending_memory_state.is_some()
        && process
            .active_syscall
            .is_none_or(|entry| read_memory_state_request(entry).is_none())
    {
        anyhow::bail!("terminal memory-state request does not match the active syscall");
    }
    if let Some(pending) = process.pending_memory_state.take() {
        sink.record(ProcessCaptureEvent::MemoryStateAttempt {
            process: process.id,
            attempt: CapturedMemoryStateAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedMemoryStateOutcome::Terminated,
            },
        })?;
    }
    if process.pending_thread_state.is_some()
        && process.active_syscall.is_none_or(|entry| {
            process
                .pending_thread_state
                .as_ref()
                .is_none_or(|pending| !thread_state_request_matches(entry, pending.request))
        })
    {
        anyhow::bail!("terminal thread-state request does not match the active syscall");
    }
    if let Some(pending) = process.pending_thread_state.take() {
        sink.record(ProcessCaptureEvent::ThreadStateAttempt {
            process: process.id,
            attempt: CapturedThreadStateAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedThreadStateOutcome::Terminated,
            },
        })?;
    }
    if process.pending_lifecycle_exit.is_some()
        && process
            .active_syscall
            .is_none_or(|entry| read_lifecycle_exit_request(entry).is_none())
    {
        anyhow::bail!("terminal lifecycle-exit request does not match the active syscall");
    }
    if let Some(pending) = process.pending_lifecycle_exit.take() {
        sink.record(ProcessCaptureEvent::LifecycleExitAttempt {
            process: process.id,
            attempt: CapturedLifecycleExitAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                open_file_values: pending.open_file_values,
                outcome,
            },
        })?;
    }
    if process.pending_statx.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if entry.number == libc::SYS_statx as u64
        )
    {
        anyhow::bail!("terminal statx request does not match the active syscall");
    }
    if let Some(pending) = process.pending_statx.take() {
        sink.record(ProcessCaptureEvent::StatxAttempt {
            process: process.id,
            attempt: CapturedStatxAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedStatxOutcome::Terminated,
            },
        })?;
    }
    if process.pending_xattr.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if xattr_system_call(entry.number).is_some()
        )
    {
        anyhow::bail!("terminal xattr request does not match the active syscall");
    }
    if let Some(pending) = process.pending_xattr.take() {
        sink.record(ProcessCaptureEvent::XattrAttempt {
            process: process.id,
            attempt: CapturedXattrAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedXattrOutcome::Terminated,
            },
        })?;
    }
    if process.pending_readlink.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if readlink_system_call(entry.number).is_some()
        )
    {
        anyhow::bail!("terminal readlink request does not match the active syscall");
    }
    if let Some(pending) = process.pending_readlink.take() {
        sink.record(ProcessCaptureEvent::ReadlinkAttempt {
            process: process.id,
            attempt: CapturedReadlinkAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedReadlinkOutcome::Terminated,
            },
        })?;
    }
    if process.pending_access.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if access_system_call(entry.number).is_some()
        )
    {
        anyhow::bail!("terminal access request does not match the active syscall");
    }
    if let Some(pending) = process.pending_access.take() {
        sink.record(ProcessCaptureEvent::AccessAttempt {
            process: process.id,
            attempt: CapturedAccessAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedAccessOutcome::Terminated,
            },
        })?;
    }
    if process.pending_getcwd.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if entry.number == libc::SYS_getcwd as u64
        )
    {
        anyhow::bail!("terminal getcwd request does not match the active syscall");
    }
    if let Some(pending) = process.pending_getcwd.take() {
        sink.record(ProcessCaptureEvent::GetcwdAttempt {
            process: process.id,
            attempt: CapturedGetcwdAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedGetcwdOutcome::Terminated,
            },
        })?;
    }
    if process.pending_cwd_change.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if cwd_change_system_call(entry.number).is_some()
        )
    {
        anyhow::bail!("terminal cwd-change request does not match the active syscall");
    }
    if let Some(pending) = process.pending_cwd_change.take() {
        sink.record(ProcessCaptureEvent::CwdChangeAttempt {
            process: process.id,
            attempt: CapturedCwdChangeAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedCwdChangeOutcome::Terminated,
            },
        })?;
    }
    if process.pending_path_removal.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if path_removal_system_call(entry.number).is_some()
        )
    {
        anyhow::bail!("terminal path-removal request does not match the active syscall");
    }
    if let Some(pending) = process.pending_path_removal.take() {
        sink.record(ProcessCaptureEvent::PathRemovalAttempt {
            process: process.id,
            attempt: CapturedPathRemovalAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedPathRemovalOutcome::Terminated,
            },
        })?;
    }
    if process.pending_directory_creation.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if directory_creation_system_call(entry.number).is_some()
        )
    {
        anyhow::bail!("terminal directory-creation request does not match the active syscall");
    }
    if let Some(pending) = process.pending_directory_creation.take() {
        sink.record(ProcessCaptureEvent::DirectoryCreationAttempt {
            process: process.id,
            attempt: CapturedDirectoryCreationAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedDirectoryCreationOutcome::Terminated,
            },
        })?;
    }
    if process.pending_path_rename.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if path_rename_system_call(entry.number).is_some()
        )
    {
        anyhow::bail!("terminal path-rename request does not match the active syscall");
    }
    if let Some(pending) = process.pending_path_rename.take() {
        sink.record(ProcessCaptureEvent::PathRenameAttempt {
            process: process.id,
            attempt: CapturedPathRenameAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedPathRenameOutcome::Terminated,
            },
        })?;
    }
    if process.pending_path_mode_change.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if path_mode_change_system_call(entry.number).is_some()
        )
    {
        anyhow::bail!("terminal path-mode-change request does not match the active syscall");
    }
    if let Some(pending) = process.pending_path_mode_change.take() {
        sink.record(ProcessCaptureEvent::PathModeChangeAttempt {
            process: process.id,
            attempt: CapturedPathModeChangeAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedPathModeChangeOutcome::Terminated,
            },
        })?;
    }
    if process.pending_descriptor_timestamp_change.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if entry.number == libc::SYS_utimensat as u64
        )
    {
        anyhow::bail!("terminal timestamp-change request does not match the active syscall");
    }
    if let Some(pending) = process.pending_descriptor_timestamp_change.take() {
        sink.record(ProcessCaptureEvent::DescriptorTimestampChangeAttempt {
            process: process.id,
            attempt: CapturedDescriptorTimestampChangeAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedDescriptorTimestampChangeOutcome::Terminated,
            },
        })?;
    }
    if process.pending_symbolic_link_creation.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if symbolic_link_creation_system_call(entry.number).is_some()
        )
    {
        anyhow::bail!("terminal symbolic-link request does not match the active syscall");
    }
    if let Some(pending) = process.pending_symbolic_link_creation.take() {
        sink.record(ProcessCaptureEvent::SymbolicLinkCreationAttempt {
            process: process.id,
            attempt: CapturedSymbolicLinkCreationAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedSymbolicLinkCreationOutcome::Terminated,
            },
        })?;
    }
    if process.pending_filesystem_status.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if filesystem_status_system_call(entry.number).is_some()
        )
    {
        anyhow::bail!("terminal filesystem-status request does not match the active syscall");
    }
    if let Some(pending) = process.pending_filesystem_status.take() {
        sink.record(ProcessCaptureEvent::FilesystemStatusAttempt {
            process: process.id,
            attempt: CapturedFilesystemStatusAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedFilesystemStatusOutcome::Terminated,
            },
        })?;
    }
    if process.pending_read.is_some()
        && !matches!(process.active_syscall, Some(entry) if read_system_call(entry.number).is_some())
    {
        anyhow::bail!("terminal read request does not match the active syscall");
    }
    if let Some(pending) = process.pending_read.take() {
        sink.record(ProcessCaptureEvent::ReadAttempt {
            process: process.id,
            attempt: CapturedReadAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedReadOutcome::Terminated,
            },
        })?;
    }
    if process.pending_getdents64.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if entry.number == libc::SYS_getdents64 as u64
        )
    {
        anyhow::bail!("terminal getdents64 request does not match the active syscall");
    }
    if let Some(pending) = process.pending_getdents64.take() {
        sink.record(ProcessCaptureEvent::Getdents64Attempt {
            process: process.id,
            attempt: CapturedGetdents64Attempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedGetdents64Outcome::Terminated,
            },
        })?;
    }
    if process.pending_mmap.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if entry.number == libc::SYS_mmap as u64
        )
    {
        anyhow::bail!("terminal mmap request does not match the active syscall");
    }
    if let Some(pending) = process.pending_mmap.take() {
        sink.record(ProcessCaptureEvent::MmapAttempt {
            process: process.id,
            attempt: CapturedMmapAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedMmapOutcome::Terminated,
            },
        })?;
    }
    if process.pending_write.is_some()
        && !matches!(
            process.active_syscall,
            Some(entry) if write_system_call(entry.number).is_some()
        )
    {
        anyhow::bail!("terminal write request does not match the active syscall");
    }
    if let Some(pending) = process.pending_write.take() {
        sink.record(ProcessCaptureEvent::WriteAttempt {
            process: process.id,
            attempt: CapturedWriteAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome: CapturedWriteOutcome::Terminated,
            },
        })?;
    }
    if process.pending_unmodeled_syscall.is_some()
        && process.active_syscall.is_none_or(|entry| {
            process
                .pending_unmodeled_syscall
                .as_ref()
                .is_none_or(|pending| pending.number != entry.number)
        })
    {
        anyhow::bail!("terminal unmodeled request does not match the active syscall");
    }
    if let Some(pending) = process.pending_unmodeled_syscall.take() {
        sink.record(ProcessCaptureEvent::UnmodeledSyscallAttempt {
            process: process.id,
            attempt: CapturedUnmodeledSyscallAttempt {
                syscall_index: pending.syscall_index,
                number: pending.number,
                arguments: pending.arguments,
                outcome: CapturedUnmodeledSyscallOutcome::Terminated,
            },
        })?;
    }
    if process.active_syscall.take().is_some() {
        process.terminal_syscalls = process
            .terminal_syscalls
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("terminal syscall census overflowed"))?;
    }
    sink.record(ProcessCaptureEvent::ProcessFinished {
        process: process.id,
        summary: ProcessFinish {
            invocations: process.invocations,
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
            signal_deliveries: process.signal_deliveries,
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
            descriptor_timestamp_change_syscalls: process.descriptor_timestamp_change_syscalls,
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
            outcome,
        },
    })?;
    Ok(())
}

fn finish_exec_attempt(
    process: &mut PendingProcess,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_exec
        .take()
        .context("exec syscall exit has no captured request")?;
    let outcome = match pending.loaded_invocation {
        Some(invocation) if exit.is_error == 0 => CapturedExecOutcome::Loaded(invocation),
        Some(_) => anyhow::bail!("loaded exec request returned a kernel error"),
        None if exit.is_error != 0 && exit.return_value < 0 => {
            let errno = i32::try_from(-exit.return_value).context("exec errno exceeds i32")?;
            CapturedExecOutcome::Failed(errno)
        }
        None => anyhow::bail!("successful exec return has no loaded process image"),
    };
    sink.record(ProcessCaptureEvent::ExecAttempt {
        process: process.id,
        attempt: CapturedExecAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_open_attempt(
    process: &mut PendingProcess,
    pid: Pid,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
    witnesses: &mut MappedFileWitnessCache,
) -> Result<()> {
    let pending = process
        .pending_open
        .take()
        .context("open syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno = i32::try_from(-exit.return_value).context("open errno exceeds i32")?;
        CapturedOpenOutcome::Failed(errno)
    } else if exit.is_error == 0 && exit.return_value >= 0 {
        let descriptor = i32::try_from(exit.return_value).context("open descriptor exceeds i32")?;
        let identity = read_descriptor_identity(pid, descriptor)?
            .context("opened descriptor has no resolved identity")?;
        let mount_id = read_descriptor_mount_id(pid, descriptor)?;
        let file_value = witness_opened_regular_file(
            pid,
            descriptor,
            pending.request.flags,
            &identity.target,
            witnesses,
        )?;
        CapturedOpenOutcome::Opened {
            descriptor,
            identity,
            mount_id,
            file_value,
        }
    } else {
        anyhow::bail!("open syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::OpenAttempt {
        process: process.id,
        attempt: CapturedOpenAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn witness_opened_regular_file(
    pid: Pid,
    descriptor: i32,
    flags: u64,
    target: &[u8],
    witnesses: &mut MappedFileWitnessCache,
) -> Result<Option<CapturedMappedFileValue>> {
    let access_mode = flags & libc::O_ACCMODE as u64;
    if access_mode == libc::O_WRONLY as u64
        || flags & libc::O_PATH as u64 != 0
        || !regular_file_value_target(target)
    {
        return Ok(None);
    }
    let descriptor_path = PathBuf::from(format!("/proc/{pid}/fd/{descriptor}"));
    let metadata =
        std::fs::metadata(&descriptor_path).context("stat opened descriptor before witnessing")?;
    if !metadata.file_type().is_file() {
        return Ok(None);
    }
    use std::os::unix::fs::MetadataExt as _;

    let Some(file) = open_tracee_descriptor_for_witness(pid, descriptor)? else {
        return Ok(None);
    };
    witness_regular_mapped_file(file, metadata.dev(), metadata.ino(), witnesses).map(Some)
}

fn regular_file_value_target(target: &[u8]) -> bool {
    crate::regular_file_value_target(target)
}

fn open_tracee_descriptor_for_witness(pid: Pid, descriptor: i32) -> Result<Option<File>> {
    let descriptor_path = PathBuf::from(format!("/proc/{pid}/fd/{descriptor}"));
    use std::os::unix::fs::OpenOptionsExt as _;
    let open = |flags| {
        let mut options = std::fs::OpenOptions::new();
        options.read(true).custom_flags(flags);
        options.open(&descriptor_path)
    };
    match open(libc::O_CLOEXEC | libc::O_NOATIME) {
        Ok(file) => Ok(Some(file)),
        // Linux reserves O_NOATIME for the file owner or CAP_FOWNER. Exec
        // mapping witnesses already read accessible system files without it;
        // apply the same fallback here and retain the identical inode and
        // pre/post content-metadata checks in the witness pass.
        Err(error) if error.raw_os_error() == Some(libc::EPERM) => open(libc::O_CLOEXEC)
            .context("reopen regular descriptor without O_NOATIME")
            .map(Some),
        Err(error) => Err(error).context("reopen regular descriptor for witnessing"),
    }
}

fn finish_newfstatat_attempt(
    process: &mut PendingProcess,
    pid: Pid,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_newfstatat
        .take()
        .context("newfstatat syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno = i32::try_from(-exit.return_value).context("newfstatat errno exceeds i32")?;
        CapturedNewfstatatOutcome::Failed(errno)
    } else if exit.is_error == 0 && exit.return_value == 0 {
        let bytes = copy_tracee_bytes(
            pid,
            pending.metadata_address,
            std::mem::size_of::<libc::stat>(),
            "copy returned newfstatat metadata",
        )?;
        // SAFETY: `bytes` has exactly `size_of::<libc::stat>()` initialized
        // bytes. `read_unaligned` copies them into a native value before any
        // field is read, so the vector's alignment is irrelevant.
        let metadata = unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<libc::stat>()) };
        CapturedNewfstatatOutcome::Metadata(captured_file_metadata(metadata))
    } else {
        anyhow::bail!("newfstatat syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::NewfstatatAttempt {
        process: process.id,
        attempt: CapturedNewfstatatAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_fstat_attempt(
    process: &mut PendingProcess,
    pid: Pid,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_fstat
        .take()
        .context("fstat syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno = i32::try_from(-exit.return_value).context("fstat errno exceeds i32")?;
        CapturedFstatOutcome::Failed(errno)
    } else if exit.is_error == 0 && exit.return_value == 0 {
        let bytes = copy_tracee_bytes(
            pid,
            pending.metadata_address,
            std::mem::size_of::<libc::stat>(),
            "copy returned fstat metadata",
        )?;
        // SAFETY: `bytes` has exactly `size_of::<libc::stat>()` initialized
        // bytes. `read_unaligned` copies them into a native value before any
        // field is read, so the vector's alignment is irrelevant.
        let metadata = unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<libc::stat>()) };
        CapturedFstatOutcome::Metadata(captured_file_metadata(metadata))
    } else {
        anyhow::bail!("fstat syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::FstatAttempt {
        process: process.id,
        attempt: CapturedFstatAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_getrandom_attempt(
    process: &mut PendingProcess,
    pid: Pid,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_getrandom
        .take()
        .context("getrandom syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno = i32::try_from(-exit.return_value).context("getrandom errno exceeds i32")?;
        CapturedGetrandomOutcome::Failed(errno)
    } else if exit.is_error == 0 && exit.return_value >= 0 {
        let returned_bytes =
            u64::try_from(exit.return_value).context("getrandom byte count is negative")?;
        if returned_bytes > pending.request.requested_bytes {
            anyhow::bail!("getrandom returned more bytes than requested");
        }
        let digest = hash_tracee_buffer_prefix(
            pid,
            &[pending.buffer],
            returned_bytes,
            "copy delivered getrandom bytes",
        )?;
        CapturedGetrandomOutcome::Bytes {
            returned_bytes,
            digest,
        }
    } else {
        anyhow::bail!("getrandom syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::GetrandomAttempt {
        process: process.id,
        attempt: CapturedGetrandomAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_runtime_observation_attempt(
    process: &mut PendingProcess,
    pid: Pid,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_runtime_observation
        .take()
        .context("runtime-observation syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        CapturedRuntimeObservationOutcome::Failed(
            i32::try_from(-exit.return_value).context("runtime-observation errno exceeds i32")?,
        )
    } else if exit.is_error == 0 {
        match &pending.request {
            CapturedRuntimeObservationRequest::CallingProcessResourceUsage { .. }
                if exit.return_value == 0 =>
            {
                let bytes = copy_tracee_bytes(
                    pid,
                    pending.output_address,
                    std::mem::size_of::<libc::rusage>(),
                    "copy returned process resource usage",
                )?;
                // SAFETY: `bytes` contains exactly one initialized native
                // `rusage`; `read_unaligned` copies before fields are read.
                let usage =
                    unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<libc::rusage>()) };
                CapturedRuntimeObservationOutcome::ResourceUsage(captured_resource_usage(usage))
            }
            CapturedRuntimeObservationRequest::SystemInformation { .. }
                if exit.return_value == 0 =>
            {
                let bytes = copy_tracee_bytes(
                    pid,
                    pending.output_address,
                    std::mem::size_of::<libc::sysinfo>(),
                    "copy returned system information",
                )?;
                // SAFETY: `bytes` contains exactly one initialized native
                // `sysinfo`; `read_unaligned` copies before fields are read.
                let information =
                    unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<libc::sysinfo>()) };
                CapturedRuntimeObservationOutcome::SystemInformation(captured_system_information(
                    information,
                ))
            }
            CapturedRuntimeObservationRequest::SystemIdentity { .. }
                if exit.return_value == 0 =>
            {
                let bytes = copy_tracee_bytes(
                    pid,
                    pending.output_address,
                    std::mem::size_of::<libc::utsname>(),
                    "copy returned system identity",
                )?;
                // SAFETY: `bytes` contains exactly one initialized native
                // `utsname`; `read_unaligned` copies before fields are read.
                let identity =
                    unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<libc::utsname>()) };
                CapturedRuntimeObservationOutcome::SystemIdentity(captured_system_identity(
                    identity,
                )?)
            }
            CapturedRuntimeObservationRequest::CallingThreadAffinity {
                capacity,
                output_is_null: false,
            } if exit.return_value > 0 && exit.return_value as u64 <= *capacity => {
                let returned_bytes = usize::try_from(exit.return_value)
                    .context("CPU-affinity result size exceeds usize")?;
                let mask = copy_tracee_bytes(
                    pid,
                    pending.output_address,
                    returned_bytes,
                    "copy returned CPU-affinity mask",
                )?;
                CapturedRuntimeObservationOutcome::CpuAffinity(mask)
            }
            CapturedRuntimeObservationRequest::DescriptorReadiness { descriptors, .. }
                if exit.return_value >= 0
                    && usize::try_from(exit.return_value).ok()
                        .is_some_and(|ready| ready <= descriptors.len()) =>
            {
                let byte_count = descriptors
                    .len()
                    .checked_mul(std::mem::size_of::<libc::pollfd>())
                    .context("poll result bytes overflowed")?;
                let bytes = copy_tracee_bytes(
                    pid,
                    pending.output_address,
                    byte_count,
                    "copy returned descriptor readiness",
                )?;
                let mut returned_events = Vec::with_capacity(descriptors.len());
                for (bytes, descriptor) in bytes
                    .chunks_exact(std::mem::size_of::<libc::pollfd>())
                    .zip(descriptors)
                {
                    // SAFETY: each chunk contains one copied native pollfd;
                    // read_unaligned copies it before any field is inspected.
                    let returned = unsafe {
                        std::ptr::read_unaligned(bytes.as_ptr().cast::<libc::pollfd>())
                    };
                    if returned.fd != descriptor.descriptor
                        || returned.events != descriptor.requested_events
                    {
                        anyhow::bail!("poll changed its descriptor request array");
                    }
                    returned_events.push(returned.revents);
                }
                let ready = u32::try_from(exit.return_value)
                    .context("poll readiness count exceeds u32")?;
                if returned_events.iter().filter(|events| **events != 0).count()
                    != ready as usize
                {
                    anyhow::bail!("poll readiness count differs from returned event masks");
                }
                CapturedRuntimeObservationOutcome::DescriptorReadiness {
                    ready,
                    returned_events,
                }
            }
            _ => anyhow::bail!("runtime-observation syscall returned an inconsistent result"),
        }
    } else {
        anyhow::bail!("runtime-observation syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::RuntimeObservationAttempt {
        process: process.id,
        attempt: CapturedRuntimeObservationAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn captured_resource_usage(usage: libc::rusage) -> CapturedResourceUsage {
    CapturedResourceUsage {
        user_seconds: usage.ru_utime.tv_sec as i64,
        user_microseconds: usage.ru_utime.tv_usec as i64,
        system_seconds: usage.ru_stime.tv_sec as i64,
        system_microseconds: usage.ru_stime.tv_usec as i64,
        maximum_resident_set_size: usage.ru_maxrss as i64,
        integral_shared_memory_size: usage.ru_ixrss as i64,
        integral_unshared_data_size: usage.ru_idrss as i64,
        integral_unshared_stack_size: usage.ru_isrss as i64,
        minor_page_faults: usage.ru_minflt as i64,
        major_page_faults: usage.ru_majflt as i64,
        swaps: usage.ru_nswap as i64,
        block_inputs: usage.ru_inblock as i64,
        block_outputs: usage.ru_oublock as i64,
        messages_sent: usage.ru_msgsnd as i64,
        messages_received: usage.ru_msgrcv as i64,
        signals_received: usage.ru_nsignals as i64,
        voluntary_context_switches: usage.ru_nvcsw as i64,
        involuntary_context_switches: usage.ru_nivcsw as i64,
    }
}

fn captured_system_information(information: libc::sysinfo) -> CapturedSystemInformation {
    CapturedSystemInformation {
        uptime_seconds: information.uptime as i64,
        load_averages: information.loads.map(|value| value as u64),
        total_ram: information.totalram as u64,
        free_ram: information.freeram as u64,
        shared_ram: information.sharedram as u64,
        buffer_ram: information.bufferram as u64,
        total_swap: information.totalswap as u64,
        free_swap: information.freeswap as u64,
        process_count: information.procs,
        total_high_memory: information.totalhigh as u64,
        free_high_memory: information.freehigh as u64,
        memory_unit: information.mem_unit,
    }
}

fn captured_system_identity(identity: libc::utsname) -> Result<CapturedSystemIdentity> {
    Ok(CapturedSystemIdentity {
        system_name: captured_uts_name_field(&identity.sysname, "system name")?,
        node_name: captured_uts_name_field(&identity.nodename, "node name")?,
        release: captured_uts_name_field(&identity.release, "release")?,
        version: captured_uts_name_field(&identity.version, "version")?,
        machine: captured_uts_name_field(&identity.machine, "machine")?,
        domain_name: captured_uts_name_field(&identity.domainname, "domain name")?,
    })
}

fn captured_uts_name_field(field: &[libc::c_char], name: &str) -> Result<Vec<u8>> {
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .with_context(|| format!("returned uname {name} is not terminated"))?;
    Ok(field[..end].iter().map(|byte| *byte as u8).collect())
}

fn finish_prlimit_attempt(
    process: &mut PendingProcess,
    pid: Pid,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_prlimit
        .take()
        .context("prlimit syscall exit has no captured request")?;
    let outcome = if let Some(requested) = pending.new_limit {
        if exit.is_error != 0 && exit.return_value < 0 {
            let errno = i32::try_from(-exit.return_value).context("prlimit errno exceeds i32")?;
            CapturedPrlimitOutcome::MutationFailed { requested, errno }
        } else if exit.is_error == 0 && exit.return_value == 0 {
            let previous = if pending.request.old_limit_is_null {
                None
            } else {
                Some(copy_resource_limit(
                    pid,
                    pending.old_limit_address,
                    "copy returned prlimit value",
                )?)
            };
            CapturedPrlimitOutcome::Mutated {
                requested,
                previous,
            }
        } else {
            anyhow::bail!("prlimit syscall returned an inconsistent mutation result");
        }
    } else if exit.is_error != 0 && exit.return_value < 0 {
        let errno = i32::try_from(-exit.return_value).context("prlimit errno exceeds i32")?;
        CapturedPrlimitOutcome::Failed(errno)
    } else if exit.is_error == 0 && exit.return_value == 0 {
        if pending.request.old_limit_is_null {
            CapturedPrlimitOutcome::Checked
        } else {
            let limit = copy_resource_limit(
                pid,
                pending.old_limit_address,
                "copy returned prlimit value",
            )?;
            CapturedPrlimitOutcome::Limit(limit)
        }
    } else {
        anyhow::bail!("prlimit syscall returned an inconsistent query result");
    };
    sink.record(ProcessCaptureEvent::PrlimitAttempt {
        process: process.id,
        attempt: CapturedPrlimitAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn copy_resource_limit(pid: Pid, address: u64, description: &str) -> Result<CapturedResourceLimit> {
    let bytes = copy_tracee_bytes(pid, address, 16, description)?;
    let current = u64::from_ne_bytes(bytes[0..8].try_into().unwrap());
    let maximum = u64::from_ne_bytes(bytes[8..16].try_into().unwrap());
    Ok(CapturedResourceLimit { current, maximum })
}

fn finish_close_attempt(
    process: &mut PendingProcess,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
    witnesses: &mut MappedFileWitnessCache,
) -> Result<()> {
    let pending = process
        .pending_close
        .take()
        .context("close syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno = i32::try_from(-exit.return_value).context("close errno exceeds i32")?;
        CapturedCloseOutcome::Failed(errno)
    } else if exit.is_error == 0 && exit.return_value == 0 {
        let file_value = match (pending.request.identity.as_ref(), pending.backing_file) {
            (Some(identity), Some(file)) => Some(witness_regular_mapped_file(
                file,
                identity.device,
                identity.inode,
                witnesses,
            )?),
            (_, None) => None,
            (None, Some(_)) => anyhow::bail!("close witness has no descriptor identity"),
        };
        CapturedCloseOutcome::Closed { file_value }
    } else {
        anyhow::bail!("close syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::CloseAttempt {
        process: process.id,
        attempt: CapturedCloseAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_seek_attempt(
    process: &mut PendingProcess,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_seek
        .take()
        .context("seek syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno = i32::try_from(-exit.return_value).context("seek errno exceeds i32")?;
        CapturedSeekOutcome::Failed(errno)
    } else if exit.is_error == 0 && exit.return_value >= 0 {
        CapturedSeekOutcome::Positioned(exit.return_value as u64)
    } else {
        anyhow::bail!("seek syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::SeekAttempt {
        process: process.id,
        attempt: CapturedSeekAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_file_advice_attempt(
    process: &mut PendingProcess,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_file_advice
        .take()
        .context("file-advice syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno =
            i32::try_from(-exit.return_value).context("file-advice errno exceeds i32")?;
        CapturedFileAdviceOutcome::Failed(errno)
    } else if exit.is_error == 0 && exit.return_value == 0 {
        CapturedFileAdviceOutcome::Completed
    } else {
        anyhow::bail!("file-advice syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::FileAdviceAttempt {
        process: process.id,
        attempt: CapturedFileAdviceAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_fcntl_attempt(
    pid: Pid,
    process: &mut PendingProcess,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_fcntl
        .take()
        .context("fcntl syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno = i32::try_from(-exit.return_value).context("fcntl errno exceeds i32")?;
        CapturedFcntlOutcome::Failed(errno)
    } else if exit.is_error == 0 {
        let duplicates = matches!(
            pending.request.command,
            command if command == libc::F_DUPFD as u32
                || command == libc::F_DUPFD_CLOEXEC as u32
        );
        let (result_identity, result_mount_id) = if duplicates {
            let descriptor = i32::try_from(exit.return_value)
                .context("fcntl returned descriptor exceeds i32")?;
            if descriptor < 0 {
                anyhow::bail!("fcntl returned a negative duplicated descriptor");
            }
            let identity = read_descriptor_identity(pid, descriptor)?
                .context("fcntl returned descriptor has no identity")?;
            let mount_id = read_descriptor_mount_id(pid, descriptor)?;
            (Some(identity), Some(mount_id))
        } else {
            (None, None)
        };
        CapturedFcntlOutcome::Returned {
            value: exit.return_value,
            result_identity,
            result_mount_id,
        }
    } else {
        anyhow::bail!("fcntl syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::FcntlAttempt {
        process: process.id,
        attempt: CapturedFcntlAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_ioctl_attempt(
    process: &mut PendingProcess,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_ioctl
        .take()
        .context("ioctl syscall exit has no captured request")?;
    if exit.is_error == 0 || exit.return_value != -i64::from(libc::ENOTTY) {
        anyhow::bail!("supported terminal probe did not fail with ENOTTY");
    }
    sink.record(ProcessCaptureEvent::IoctlAttempt {
        process: process.id,
        attempt: CapturedIoctlAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome: CapturedIoctlOutcome::Failed(libc::ENOTTY),
        },
    })?;
    Ok(())
}

fn finish_pipe_attempt(
    pid: Pid,
    process: &mut PendingProcess,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_pipe
        .take()
        .context("pipe2 syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno = i32::try_from(-exit.return_value).context("pipe2 errno exceeds i32")?;
        CapturedPipeOutcome::Failed(errno)
    } else if exit.is_error == 0 && exit.return_value == 0 {
        let bytes = copy_tracee_bytes(
            pid,
            pending.descriptor_address,
            2 * std::mem::size_of::<i32>(),
            "copy returned pipe descriptors",
        )?;
        let read_descriptor = i32::from_ne_bytes(bytes[0..4].try_into().unwrap());
        let write_descriptor = i32::from_ne_bytes(bytes[4..8].try_into().unwrap());
        let read_identity = read_descriptor_identity(pid, read_descriptor)?
            .context("pipe2 returned read descriptor has no identity")?;
        let write_identity = read_descriptor_identity(pid, write_descriptor)?
            .context("pipe2 returned write descriptor has no identity")?;
        let read_mount_id = read_descriptor_mount_id(pid, read_descriptor)?;
        let write_mount_id = read_descriptor_mount_id(pid, write_descriptor)?;
        CapturedPipeOutcome::Created {
            read_descriptor,
            read_identity,
            read_mount_id,
            write_descriptor,
            write_identity,
            write_mount_id,
        }
    } else {
        anyhow::bail!("pipe2 syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::PipeAttempt {
        process: process.id,
        attempt: CapturedPipeAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_epoll_create_attempt(
    pid: Pid,
    process: &mut PendingProcess,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_epoll_create
        .take()
        .context("epoll_create1 syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno =
            i32::try_from(-exit.return_value).context("epoll_create1 errno exceeds i32")?;
        CapturedEpollCreateOutcome::Failed(errno)
    } else if exit.is_error == 0 {
        let descriptor = i32::try_from(exit.return_value)
            .context("epoll_create1 returned descriptor exceeds i32")?;
        if descriptor < 0 {
            anyhow::bail!("epoll_create1 returned a negative descriptor");
        }
        let identity = read_descriptor_identity(pid, descriptor)?
            .context("epoll_create1 returned descriptor has no identity")?;
        let mount_id = read_descriptor_mount_id(pid, descriptor)?;
        CapturedEpollCreateOutcome::Created {
            descriptor,
            identity,
            mount_id,
        }
    } else {
        anyhow::bail!("epoll_create1 syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::EpollCreateAttempt {
        process: process.id,
        attempt: CapturedEpollCreateAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_socket_create_attempt(
    pid: Pid,
    process: &mut PendingProcess,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_socket_create
        .take()
        .context("socket syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno = i32::try_from(-exit.return_value)
            .context("socket errno exceeds i32")?;
        CapturedSocketCreateOutcome::Failed(errno)
    } else if exit.is_error == 0 {
        let descriptor = i32::try_from(exit.return_value)
            .context("socket returned descriptor exceeds i32")?;
        if descriptor < 0 {
            anyhow::bail!("socket returned a negative descriptor");
        }
        let identity = read_descriptor_identity(pid, descriptor)?
            .context("socket returned descriptor has no identity")?;
        let mount_id = read_descriptor_mount_id(pid, descriptor)?;
        CapturedSocketCreateOutcome::Created {
            descriptor,
            identity,
            mount_id,
        }
    } else {
        anyhow::bail!("socket syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::SocketCreateAttempt {
        process: process.id,
        attempt: CapturedSocketCreateAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_socket_connect_attempt(
    pid: Pid,
    process: &mut PendingProcess,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_socket_connect
        .take()
        .context("connect syscall exit has no captured request")?;
    let identity = read_descriptor_identity(pid, pending.request.descriptor)?
        .context("connect result descriptor has no identity")?;
    let mount_id = read_descriptor_mount_id(pid, pending.request.descriptor)?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno = i32::try_from(-exit.return_value)
            .context("connect errno exceeds i32")?;
        CapturedSocketConnectOutcome::Failed {
            errno,
            identity,
            mount_id,
        }
    } else if exit.is_error == 0 && exit.return_value == 0 {
        CapturedSocketConnectOutcome::Connected { identity, mount_id }
    } else {
        anyhow::bail!("connect syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::SocketConnectAttempt {
        process: process.id,
        attempt: CapturedSocketConnectAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_dup2_attempt(
    pid: Pid,
    process: &mut PendingProcess,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
    witnesses: &mut MappedFileWitnessCache,
) -> Result<()> {
    let pending = process
        .pending_dup2
        .take()
        .context("descriptor-duplication exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno = i32::try_from(-exit.return_value)
            .context("descriptor-duplication errno exceeds i32")?;
        CapturedDup2Outcome::Failed(errno)
    } else if exit.is_error == 0
        && exit.return_value == i64::from(pending.request.target_descriptor)
    {
        let descriptor = i32::try_from(exit.return_value)
            .context("descriptor duplication returned descriptor exceeds i32")?;
        let identity = read_descriptor_identity(pid, descriptor)?
            .context("descriptor duplication returned descriptor has no identity")?;
        let mount_id = read_descriptor_mount_id(pid, descriptor)?;
        let replaced_file_value = match (
            pending.request.target_identity.as_ref(),
            pending.replaced_backing_file,
        ) {
            (Some(replaced), Some(file)) => Some(witness_regular_mapped_file(
                file,
                replaced.device,
                replaced.inode,
                witnesses,
            )?),
            (_, None) => None,
            (None, Some(_)) => {
                anyhow::bail!("descriptor replacement witness has no target identity")
            }
        };
        CapturedDup2Outcome::Duplicated {
            descriptor,
            identity,
            mount_id,
            replaced_file_value,
        }
    } else {
        anyhow::bail!("descriptor duplication returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::Dup2Attempt {
        process: process.id,
        attempt: CapturedDup2Attempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_signal_state_attempt(
    pid: Pid,
    process: &mut PendingProcess,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_signal_state
        .take()
        .context("signal-state syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        CapturedSignalStateOutcome::Failed(
            i32::try_from(-exit.return_value).context("signal-state errno exceeds i32")?,
        )
    } else if exit.is_error == 0 && exit.return_value == 0 {
        match &pending.request {
            CapturedSignalStateRequest::Action {
                old_action_is_null,
                signal_set_bytes,
                ..
            } => {
                let previous = if *old_action_is_null {
                    None
                } else {
                    let bytes = signal_set_bytes
                        .checked_add(LINUX_RT_SIGACTION_PREFIX_BYTES)
                        .context("returned signal-action size overflowed")?;
                    Some(copy_tracee_bytes(
                        pid,
                        pending.old_state_address,
                        usize::try_from(bytes).context("signal-action size exceeds usize")?,
                        "copy returned signal action",
                    )?)
                };
                CapturedSignalStateOutcome::ActionReturned { previous }
            }
            CapturedSignalStateRequest::Mask {
                old_mask_is_null,
                signal_set_bytes,
                ..
            } => {
                let previous = if *old_mask_is_null {
                    None
                } else {
                    Some(copy_tracee_bytes(
                        pid,
                        pending.old_state_address,
                        usize::try_from(*signal_set_bytes)
                            .context("signal-mask size exceeds usize")?,
                        "copy returned signal mask",
                    )?)
                };
                CapturedSignalStateOutcome::MaskReturned { previous }
            }
        }
    } else {
        anyhow::bail!("signal-state syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::SignalStateAttempt {
        process: process.id,
        attempt: CapturedSignalStateAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_umask_attempt(
    process: &mut PendingProcess,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_umask
        .take()
        .context("umask syscall exit has no captured request")?;
    let previous = u32::try_from(exit.return_value).ok();
    if exit.is_error != 0 || previous.is_none_or(|mask| mask & !0o777 != 0) {
        anyhow::bail!("umask syscall returned an inconsistent result");
    }
    sink.record(ProcessCaptureEvent::UmaskAttempt {
        process: process.id,
        attempt: CapturedUmaskAttempt {
            syscall_index: pending.syscall_index,
            requested: pending.requested,
            outcome: CapturedUmaskOutcome::Changed(previous.unwrap()),
        },
    })?;
    Ok(())
}

fn finish_credential_attempt(
    pid: Pid,
    process: &mut PendingProcess,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_credential
        .take()
        .context("credential syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        CapturedCredentialOutcome::Failed(
            i32::try_from(-exit.return_value).context("credential errno exceeds i32")?,
        )
    } else if exit.is_error == 0 {
        match pending.request {
            CapturedCredentialRequest::Getuid
            | CapturedCredentialRequest::Geteuid
            | CapturedCredentialRequest::Getgid
            | CapturedCredentialRequest::Getegid
                if exit.return_value >= 0 =>
            {
                CapturedCredentialOutcome::Identity(
                    u32::try_from(exit.return_value)
                        .context("credential identity exceeds u32")?,
                )
            }
            CapturedCredentialRequest::Getgroups { capacity: 0, .. }
                if (0..=65_536).contains(&exit.return_value) =>
            {
                CapturedCredentialOutcome::SupplementaryGroupCount(exit.return_value as u32)
            }
            CapturedCredentialRequest::Getgroups {
                capacity,
                output_is_null,
            } if exit.return_value >= 0
                && exit.return_value <= i64::from(capacity)
                && (!output_is_null || exit.return_value == 0) =>
            {
                let count = usize::try_from(exit.return_value)
                    .context("supplementary group count exceeds usize")?;
                let byte_count = count
                    .checked_mul(std::mem::size_of::<u32>())
                    .context("supplementary group result bytes overflowed")?;
                let bytes = if byte_count == 0 {
                    Vec::new()
                } else {
                    read_tracee_bytes(pid, pending.output_address, byte_count)
                        .context("read supplementary group result")?
                };
                let mut groups = bytes
                    .chunks_exact(std::mem::size_of::<u32>())
                    .map(|bytes| u32::from_ne_bytes(bytes.try_into().unwrap()))
                    .collect::<Vec<_>>();
                groups.sort_unstable();
                CapturedCredentialOutcome::SupplementaryGroups(groups)
            }
            CapturedCredentialRequest::CapabilityBoundingSetRead { .. }
                if matches!(exit.return_value, 0 | 1) =>
            {
                CapturedCredentialOutcome::CapabilityBoundingSetMembership(
                    exit.return_value == 1,
                )
            }
            CapturedCredentialRequest::Setresuid { .. }
            | CapturedCredentialRequest::Setresgid { .. }
                if exit.return_value == 0 =>
            {
                CapturedCredentialOutcome::State(read_process_credentials(&PathBuf::from(
                    format!("/proc/{pid}"),
                ))?)
            }
            _ => anyhow::bail!("credential syscall returned an inconsistent result"),
        }
    } else {
        anyhow::bail!("credential syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::CredentialAttempt {
        process: process.id,
        attempt: CapturedCredentialAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_futex_wake_attempt(
    process: &mut PendingProcess,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_futex_wake
        .take()
        .context("futex-wake syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        CapturedFutexWakeOutcome::Failed(
            i32::try_from(-exit.return_value).context("futex-wake errno exceeds i32")?,
        )
    } else if exit.is_error == 0 && exit.return_value >= 0 {
        CapturedFutexWakeOutcome::Woken(
            u32::try_from(exit.return_value).context("futex wake count exceeds u32")?,
        )
    } else {
        anyhow::bail!("futex-wake syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::FutexWakeAttempt {
        process: process.id,
        attempt: CapturedFutexWakeAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_process_creation_attempt(
    process: &mut PendingProcess,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_process_creation
        .take()
        .context("process-creation syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 && pending.child.is_none() {
        failed_process_creation_outcome(exit.return_value)?
    } else if exit.is_error == 0 && exit.return_value > 0 {
        let (child_pid, child) = pending
            .child
            .context("successful process creation has no ptrace child event")?;
        if exit.return_value != i64::from(child_pid.as_raw()) {
            anyhow::bail!("process-creation result disagrees with its ptrace child event");
        }
        CapturedProcessCreationOutcome::Created(child)
    } else {
        anyhow::bail!("process-creation syscall returned an inconsistent result");
    };
    if pending.child.is_none() {
        sink.record(ProcessCaptureEvent::ProcessCreationAttempt {
            process: process.id,
            attempt: CapturedProcessCreationAttempt {
                syscall_index: pending.syscall_index,
                request: pending.request,
                outcome,
            },
        })?;
    }
    Ok(())
}

fn failed_process_creation_outcome(return_value: i64) -> Result<CapturedProcessCreationOutcome> {
    if return_value >= 0 {
        anyhow::bail!("failed process creation has a non-negative result");
    }
    let error =
        i32::try_from(-return_value).context("process-creation error exceeds i32")?;
    Ok(if error == 513 {
        CapturedProcessCreationOutcome::Restarted(error)
    } else {
        CapturedProcessCreationOutcome::Failed(error)
    })
}

fn finish_child_wait_attempt(
    pid: Pid,
    process: &mut PendingProcess,
    exit: SyscallExit,
    receipt_ids_by_pid: &BTreeMap<Pid, CapturedProcessId>,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_child_wait
        .take()
        .context("child-wait syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno =
            i32::try_from(-exit.return_value).context("child-wait errno exceeds i32")?;
        if pending.request.status_is_null && errno != libc::ECHILD {
            anyhow::bail!("null-status child-wait probe failed unexpectedly");
        }
        CapturedChildWaitOutcome::Failed(errno)
    } else if exit.is_error == 0 && exit.return_value == 0 && pending.request.nonblocking {
        CapturedChildWaitOutcome::NoChildReady
    } else if exit.is_error == 0 && exit.return_value > 0 {
        if pending.request.status_is_null {
            anyhow::bail!("null-status child-wait probe unexpectedly reaped a child");
        }
        let child_pid = Pid::from_raw(
            i32::try_from(exit.return_value).context("reaped child PID exceeds i32")?,
        );
        let child = *receipt_ids_by_pid
            .get(&child_pid)
            .context("reaped child has no receipt-local identity")?;
        let status = read_tracee_bytes(pid, pending.status_address, std::mem::size_of::<u32>())
            .context("read child-wait status")?;
        let status = u32::from_ne_bytes(status.as_slice().try_into().unwrap());
        CapturedChildWaitOutcome::Reaped { child, status }
    } else {
        anyhow::bail!("child-wait syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::ChildWaitAttempt {
        process: process.id,
        attempt: CapturedChildWaitAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_process_identity_attempt(
    pid: Pid,
    process: &mut PendingProcess,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_process_identity
        .take()
        .context("process-identity syscall exit has no captured request")?;
    let outcome = match (pending.request, pending.expected_process) {
        (
            CapturedProcessIdentityRequest::Getpid
            | CapturedProcessIdentityRequest::Gettid
            | CapturedProcessIdentityRequest::Getppid,
            Some((expected_pid, identity)),
        ) if exit.is_error == 0 && exit.return_value == i64::from(expected_pid.as_raw()) => {
            CapturedProcessIdentityOutcome::Identity(identity)
        }
        (CapturedProcessIdentityRequest::Getpgrp, None) if exit.is_error == 0 => {
            // SAFETY: getpgid only reads kernel process state for the stopped
            // tracee identity and does not dereference userspace memory.
            let expected_group = unsafe { libc::getpgid(pid.as_raw()) };
            if expected_group <= 0 || exit.return_value != i64::from(expected_group) {
                anyhow::bail!("getpgrp result disagrees with the stopped tracee state");
            }
            CapturedProcessIdentityOutcome::ProcessGroup(
                u32::try_from(exit.return_value).context("process-group identity exceeds u32")?,
            )
        }
        _ => anyhow::bail!("process-identity result disagrees with the traced process tree"),
    };
    sink.record(ProcessCaptureEvent::ProcessIdentityAttempt {
        process: process.id,
        attempt: CapturedProcessIdentityAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_memory_state_attempt(
    process: &mut PendingProcess,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_memory_state
        .take()
        .context("memory-state syscall exit has no captured request")?;
    let outcome = match pending.request {
        CapturedMemoryStateRequest::Brk { .. } if exit.is_error == 0 && exit.return_value >= 0 => {
            CapturedMemoryStateOutcome::Break(
                u64::try_from(exit.return_value).context("brk address is negative")?,
            )
        }
        CapturedMemoryStateRequest::Mprotect { .. } | CapturedMemoryStateRequest::Munmap { .. }
            if exit.is_error == 0 && exit.return_value == 0 =>
        {
            CapturedMemoryStateOutcome::Completed
        }
        CapturedMemoryStateRequest::Mprotect { .. } | CapturedMemoryStateRequest::Munmap { .. }
            if exit.is_error != 0 && exit.return_value < 0 =>
        {
            CapturedMemoryStateOutcome::Failed(
                i32::try_from(-exit.return_value).context("memory-state errno exceeds i32")?,
            )
        }
        _ => anyhow::bail!("memory-state syscall returned an inconsistent result"),
    };
    sink.record(ProcessCaptureEvent::MemoryStateAttempt {
        process: process.id,
        attempt: CapturedMemoryStateAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_thread_state_attempt(
    process: &mut PendingProcess,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_thread_state
        .take()
        .context("thread-state syscall exit has no captured request")?;
    let outcome = thread_state_outcome(&pending, exit)?;
    sink.record(ProcessCaptureEvent::ThreadStateAttempt {
        process: process.id,
        attempt: CapturedThreadStateAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn thread_state_outcome(
    pending: &PendingThreadState,
    exit: SyscallExit,
) -> Result<CapturedThreadStateOutcome> {
    Ok(match pending.request {
        CapturedThreadStateRequest::SetTidAddress { .. }
            if exit.is_error == 0 && exit.return_value > 0 =>
        {
            CapturedThreadStateOutcome::ThreadId(
                i32::try_from(exit.return_value).context("thread identifier exceeds i32")?,
            )
        }
        CapturedThreadStateRequest::ArchPrctl { .. }
        | CapturedThreadStateRequest::SetRobustList { .. }
        | CapturedThreadStateRequest::Rseq { .. }
        | CapturedThreadStateRequest::SignalStack { .. }
            if exit.is_error == 0 && exit.return_value == 0 =>
        {
            CapturedThreadStateOutcome::Completed
        }
        CapturedThreadStateRequest::SignalReturn {
            restored_accumulator,
            ..
        } if pending.restored_return_value == Some(restored_accumulator)
            && restored_accumulator == exit.return_value =>
        {
            CapturedThreadStateOutcome::SignalContextRestored
        }
        CapturedThreadStateRequest::SignalReturn { .. } => {
            anyhow::bail!(
                "signal-return syscall did not restore its saved accumulator"
            )
        }
        _ if exit.is_error != 0 && exit.return_value < 0 => CapturedThreadStateOutcome::Failed(
            i32::try_from(-exit.return_value).context("thread-state errno exceeds i32")?,
        ),
        _ => anyhow::bail!("thread-state syscall returned an inconsistent result"),
    })
}

fn captured_file_metadata(metadata: libc::stat) -> crate::CapturedFileMetadata {
    // These native fields are already wide on x86-64 but are narrower on
    // supported aarch64 and riscv64 libc ABIs.
    #[allow(clippy::useless_conversion)]
    let hard_links = u64::from(metadata.st_nlink);
    #[allow(clippy::useless_conversion)]
    let block_size = i64::from(metadata.st_blksize);
    crate::CapturedFileMetadata {
        device: metadata.st_dev,
        inode: metadata.st_ino,
        mode: metadata.st_mode,
        hard_links,
        user: metadata.st_uid,
        group: metadata.st_gid,
        special_device: metadata.st_rdev,
        size: metadata.st_size,
        block_size,
        blocks: metadata.st_blocks,
        access_time_seconds: metadata.st_atime,
        access_time_nanoseconds: metadata.st_atime_nsec,
        modification_time_seconds: metadata.st_mtime,
        modification_time_nanoseconds: metadata.st_mtime_nsec,
        change_time_seconds: metadata.st_ctime,
        change_time_nanoseconds: metadata.st_ctime_nsec,
    }
}

fn captured_statx_timestamp(timestamp: NativeStatxTimestamp) -> CapturedStatxTimestamp {
    CapturedStatxTimestamp {
        seconds: timestamp.seconds,
        nanoseconds: timestamp.nanoseconds,
        reserved: timestamp.reserved,
    }
}

fn finish_statx_attempt(
    process: &mut PendingProcess,
    pid: Pid,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_statx
        .take()
        .context("statx syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno = i32::try_from(-exit.return_value).context("statx errno exceeds i32")?;
        CapturedStatxOutcome::Failed(errno)
    } else if exit.is_error == 0 && exit.return_value == 0 {
        require_native_statx_layout()?;
        let bytes = copy_tracee_bytes(
            pid,
            pending.metadata_address,
            NATIVE_STATX_BYTES,
            "copy returned statx metadata",
        )?;
        // SAFETY: the fixed UAPI layout was checked above and `bytes` contains
        // exactly one initialized record. `read_unaligned` copies it before
        // fields are accessed.
        let metadata = unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<NativeStatx>()) };
        CapturedStatxOutcome::Metadata(Box::new(CapturedStatxMetadata {
            mask: metadata.mask,
            block_size: metadata.block_size,
            attributes: metadata.attributes,
            hard_links: metadata.hard_links,
            user: metadata.user,
            group: metadata.group,
            mode: metadata.mode,
            spare0: metadata.spare0[0],
            inode: metadata.inode,
            size: metadata.size,
            blocks: metadata.blocks,
            attributes_mask: metadata.attributes_mask,
            access_time: captured_statx_timestamp(metadata.access_time),
            birth_time: captured_statx_timestamp(metadata.birth_time),
            change_time: captured_statx_timestamp(metadata.change_time),
            modification_time: captured_statx_timestamp(metadata.modification_time),
            special_device_major: metadata.special_device_major,
            special_device_minor: metadata.special_device_minor,
            device_major: metadata.device_major,
            device_minor: metadata.device_minor,
            mount_id: metadata.mount_id,
            direct_io_memory_alignment: metadata.direct_io_memory_alignment,
            direct_io_offset_alignment: metadata.direct_io_offset_alignment,
            subvolume: metadata.subvolume,
            atomic_write_unit_minimum: metadata.atomic_write_unit_minimum,
            atomic_write_unit_maximum: metadata.atomic_write_unit_maximum,
            atomic_write_segments_maximum: metadata.atomic_write_segments_maximum,
            direct_io_read_offset_alignment: metadata.direct_io_read_offset_alignment,
            atomic_write_unit_maximum_optimized: metadata.atomic_write_unit_maximum_optimized,
            spare2: metadata.spare2[0],
            spare3: metadata.spare3,
        }))
    } else {
        anyhow::bail!("statx syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::StatxAttempt {
        process: process.id,
        attempt: CapturedStatxAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn require_native_statx_layout() -> Result<()> {
    if std::mem::size_of::<NativeStatxTimestamp>() != 16
        || std::mem::align_of::<NativeStatxTimestamp>() != 8
        || std::mem::size_of::<NativeStatx>() != NATIVE_STATX_BYTES
        || std::mem::align_of::<NativeStatx>() != 8
        || std::mem::offset_of!(NativeStatx, mode) != 28
        || std::mem::offset_of!(NativeStatx, inode) != 32
        || std::mem::offset_of!(NativeStatx, access_time) != 64
        || std::mem::offset_of!(NativeStatx, special_device_major) != 128
        || std::mem::offset_of!(NativeStatx, mount_id) != 144
        || std::mem::offset_of!(NativeStatx, subvolume) != 160
        || std::mem::offset_of!(NativeStatx, atomic_write_segments_maximum) != 176
        || std::mem::offset_of!(NativeStatx, spare3) != 192
    {
        anyhow::bail!("unsupported native statx layout");
    }
    Ok(())
}

fn finish_xattr_attempt(
    process: &mut PendingProcess,
    pid: Pid,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_xattr
        .take()
        .context("xattr syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno = i32::try_from(-exit.return_value).context("xattr errno exceeds i32")?;
        CapturedXattrOutcome::Failed(errno)
    } else if pending.request.system_call == CapturedXattrSystemCall::Fsetxattr
        && exit.is_error == 0
        && exit.return_value == 0
    {
        CapturedXattrOutcome::Changed
    } else if exit.is_error == 0 && exit.return_value >= 0 {
        let returned = u64::try_from(exit.return_value).context("xattr byte count is negative")?;
        if returned > MAX_XATTR_RESULT_BYTES {
            anyhow::bail!("xattr result exceeds the capture byte limit");
        }
        let bytes = if pending.request.requested_bytes == 0 || pending.request.value_is_null {
            None
        } else {
            if returned > pending.request.requested_bytes {
                anyhow::bail!("xattr returned more bytes than requested");
            }
            Some(copy_tracee_bytes(
                pid,
                pending.value_address,
                usize::try_from(returned).context("xattr byte count exceeds usize")?,
                "copy delivered xattr value",
            )?)
        };
        CapturedXattrOutcome::Value {
            returned_bytes: returned,
            bytes,
        }
    } else {
        anyhow::bail!("xattr syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::XattrAttempt {
        process: process.id,
        attempt: CapturedXattrAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_readlink_attempt(
    process: &mut PendingProcess,
    pid: Pid,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_readlink
        .take()
        .context("readlink syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno = i32::try_from(-exit.return_value).context("readlink errno exceeds i32")?;
        CapturedReadlinkOutcome::Failed(errno)
    } else if exit.is_error == 0 && exit.return_value > 0 {
        let returned =
            u64::try_from(exit.return_value).context("readlink byte count is negative")?;
        if returned > pending.request.requested_bytes {
            anyhow::bail!("readlink returned more bytes than requested");
        }
        if returned > MAX_READLINK_RESULT_BYTES {
            anyhow::bail!("readlink result exceeds the capture byte limit");
        }
        let bytes = copy_tracee_bytes(
            pid,
            pending.buffer_address,
            usize::try_from(returned).context("readlink byte count exceeds usize")?,
            "copy delivered readlink bytes",
        )?;
        CapturedReadlinkOutcome::Target {
            bytes,
            possibly_truncated: returned == pending.request.requested_bytes,
        }
    } else {
        anyhow::bail!("readlink syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::ReadlinkAttempt {
        process: process.id,
        attempt: CapturedReadlinkAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_access_attempt(
    process: &mut PendingProcess,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_access
        .take()
        .context("access syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno = i32::try_from(-exit.return_value).context("access errno exceeds i32")?;
        CapturedAccessOutcome::Failed(errno)
    } else if exit.is_error == 0 && exit.return_value == 0 {
        CapturedAccessOutcome::Accessible
    } else {
        anyhow::bail!("access syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::AccessAttempt {
        process: process.id,
        attempt: CapturedAccessAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_getcwd_attempt(
    process: &mut PendingProcess,
    pid: Pid,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_getcwd
        .take()
        .context("getcwd syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno = i32::try_from(-exit.return_value).context("getcwd errno exceeds i32")?;
        CapturedGetcwdOutcome::Failed(errno)
    } else if exit.is_error == 0 && exit.return_value > 0 {
        let returned = u64::try_from(exit.return_value).context("getcwd byte count is negative")?;
        if returned > pending.request.requested_bytes {
            anyhow::bail!("getcwd returned more bytes than requested");
        }
        if returned > MAX_GETCWD_RESULT_BYTES {
            anyhow::bail!("getcwd result exceeds the capture byte limit");
        }
        let mut bytes = copy_tracee_bytes(
            pid,
            pending.buffer_address,
            usize::try_from(returned).context("getcwd byte count exceeds usize")?,
            "copy delivered getcwd bytes",
        )?;
        if bytes.pop() != Some(0) || bytes.contains(&0) {
            anyhow::bail!("getcwd returned an invalid terminated path");
        }
        CapturedGetcwdOutcome::Directory(bytes)
    } else {
        anyhow::bail!("getcwd syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::GetcwdAttempt {
        process: process.id,
        attempt: CapturedGetcwdAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_cwd_change_attempt(
    process: &mut PendingProcess,
    pid: Pid,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_cwd_change
        .take()
        .context("cwd-change syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno = i32::try_from(-exit.return_value).context("cwd-change errno exceeds i32")?;
        CapturedCwdChangeOutcome::Failed(errno)
    } else if exit.is_error == 0 && exit.return_value == 0 {
        CapturedCwdChangeOutcome::Changed(read_process_link(
            pid,
            "cwd",
            "read changed working directory",
        )?)
    } else {
        anyhow::bail!("cwd-change syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::CwdChangeAttempt {
        process: process.id,
        attempt: CapturedCwdChangeAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_path_removal_attempt(
    process: &mut PendingProcess,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_path_removal
        .take()
        .context("path-removal syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno = i32::try_from(-exit.return_value).context("path-removal errno exceeds i32")?;
        CapturedPathRemovalOutcome::Failed(errno)
    } else if exit.is_error == 0 && exit.return_value == 0 {
        CapturedPathRemovalOutcome::Removed
    } else {
        anyhow::bail!("path-removal syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::PathRemovalAttempt {
        process: process.id,
        attempt: CapturedPathRemovalAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_directory_creation_attempt(
    process: &mut PendingProcess,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_directory_creation
        .take()
        .context("directory-creation syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno =
            i32::try_from(-exit.return_value).context("directory-creation errno exceeds i32")?;
        CapturedDirectoryCreationOutcome::Failed(errno)
    } else if exit.is_error == 0 && exit.return_value == 0 {
        CapturedDirectoryCreationOutcome::Created
    } else {
        anyhow::bail!("directory-creation syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::DirectoryCreationAttempt {
        process: process.id,
        attempt: CapturedDirectoryCreationAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_path_rename_attempt(
    process: &mut PendingProcess,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_path_rename
        .take()
        .context("path-rename syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno = i32::try_from(-exit.return_value).context("path-rename errno exceeds i32")?;
        CapturedPathRenameOutcome::Failed(errno)
    } else if exit.is_error == 0 && exit.return_value == 0 {
        CapturedPathRenameOutcome::Completed
    } else {
        anyhow::bail!("path-rename syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::PathRenameAttempt {
        process: process.id,
        attempt: CapturedPathRenameAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_path_mode_change_attempt(
    process: &mut PendingProcess,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_path_mode_change
        .take()
        .context("path-mode-change syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno =
            i32::try_from(-exit.return_value).context("path-mode-change errno exceeds i32")?;
        CapturedPathModeChangeOutcome::Failed(errno)
    } else if exit.is_error == 0 && exit.return_value == 0 {
        CapturedPathModeChangeOutcome::Changed
    } else {
        anyhow::bail!("path-mode-change syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::PathModeChangeAttempt {
        process: process.id,
        attempt: CapturedPathModeChangeAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_descriptor_timestamp_change_attempt(
    process: &mut PendingProcess,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_descriptor_timestamp_change
        .take()
        .context("utimensat exit has no captured descriptor request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno = i32::try_from(-exit.return_value).context("utimensat errno exceeds i32")?;
        CapturedDescriptorTimestampChangeOutcome::Failed(errno)
    } else if exit.is_error == 0 && exit.return_value == 0 {
        CapturedDescriptorTimestampChangeOutcome::Changed
    } else {
        anyhow::bail!("utimensat returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::DescriptorTimestampChangeAttempt {
        process: process.id,
        attempt: CapturedDescriptorTimestampChangeAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_symbolic_link_creation_attempt(
    process: &mut PendingProcess,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_symbolic_link_creation
        .take()
        .context("symbolic-link syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno = i32::try_from(-exit.return_value).context("symbolic-link errno exceeds i32")?;
        CapturedSymbolicLinkCreationOutcome::Failed(errno)
    } else if exit.is_error == 0 && exit.return_value == 0 {
        CapturedSymbolicLinkCreationOutcome::Created
    } else {
        anyhow::bail!("symbolic-link syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::SymbolicLinkCreationAttempt {
        process: process.id,
        attempt: CapturedSymbolicLinkCreationAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_filesystem_status_attempt(
    process: &mut PendingProcess,
    pid: Pid,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_filesystem_status
        .take()
        .context("filesystem-status syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno =
            i32::try_from(-exit.return_value).context("filesystem-status errno exceeds i32")?;
        CapturedFilesystemStatusOutcome::Failed(errno)
    } else if exit.is_error == 0 && exit.return_value == 0 {
        require_native_filesystem_status_layout()?;
        let bytes = copy_tracee_bytes(
            pid,
            pending.buffer_address,
            NATIVE_FILESYSTEM_STATUS_BYTES,
            "copy returned filesystem status",
        )?;
        CapturedFilesystemStatusOutcome::Observed(decode_native_filesystem_status(&bytes)?)
    } else {
        anyhow::bail!("filesystem-status syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::FilesystemStatusAttempt {
        process: process.id,
        attempt: CapturedFilesystemStatusAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn require_native_filesystem_status_layout() -> Result<()> {
    let valid = std::mem::size_of::<usize>() == 8
        && std::mem::size_of::<NativeFilesystemStatus>() == NATIVE_FILESYSTEM_STATUS_BYTES
        && std::mem::align_of::<NativeFilesystemStatus>() == 8
        && std::mem::size_of::<libc::statfs>() == NATIVE_FILESYSTEM_STATUS_BYTES
        && std::mem::align_of::<libc::statfs>() == 8
        && std::mem::offset_of!(NativeFilesystemStatus, filesystem_type) == 0
        && std::mem::offset_of!(NativeFilesystemStatus, block_size) == 8
        && std::mem::offset_of!(NativeFilesystemStatus, blocks) == 16
        && std::mem::offset_of!(NativeFilesystemStatus, blocks_free) == 24
        && std::mem::offset_of!(NativeFilesystemStatus, blocks_available) == 32
        && std::mem::offset_of!(NativeFilesystemStatus, files) == 40
        && std::mem::offset_of!(NativeFilesystemStatus, files_free) == 48
        && std::mem::offset_of!(NativeFilesystemStatus, filesystem_id) == 56
        && std::mem::offset_of!(NativeFilesystemStatus, maximum_name_length) == 64
        && std::mem::offset_of!(NativeFilesystemStatus, fragment_size) == 72
        && std::mem::offset_of!(NativeFilesystemStatus, mount_flags) == 80
        && std::mem::offset_of!(NativeFilesystemStatus, spare) == 88;
    if !valid {
        anyhow::bail!("native filesystem-status layout is unsupported");
    }
    Ok(())
}

fn decode_native_filesystem_status(bytes: &[u8]) -> Result<CapturedFilesystemStatus> {
    if bytes.len() != NATIVE_FILESYSTEM_STATUS_BYTES {
        anyhow::bail!("native filesystem-status result has an invalid size");
    }
    let i64_at = |offset: usize| {
        i64::from_ne_bytes(
            bytes[offset..offset + 8]
                .try_into()
                .expect("checked layout"),
        )
    };
    let u64_at = |offset: usize| {
        u64::from_ne_bytes(
            bytes[offset..offset + 8]
                .try_into()
                .expect("checked layout"),
        )
    };
    let i32_at = |offset: usize| {
        i32::from_ne_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("checked layout"),
        )
    };
    Ok(CapturedFilesystemStatus {
        filesystem_type: i64_at(0),
        block_size: i64_at(8),
        blocks: u64_at(16),
        blocks_free: u64_at(24),
        blocks_available: u64_at(32),
        files: u64_at(40),
        files_free: u64_at(48),
        filesystem_id: [i32_at(56), i32_at(60)],
        maximum_name_length: i64_at(64),
        fragment_size: i64_at(72),
        mount_flags: i64_at(80),
        reserved_words: [i64_at(88), i64_at(96), i64_at(104), i64_at(112)],
    })
}

fn finish_read_attempt(
    process: &mut PendingProcess,
    pid: Pid,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_read
        .take()
        .context("read syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno = i32::try_from(-exit.return_value).context("read errno exceeds i32")?;
        CapturedReadOutcome::Failed(errno)
    } else if exit.is_error == 0 && exit.return_value >= 0 {
        let bytes = u64::try_from(exit.return_value).context("read byte count is negative")?;
        if bytes > pending.request.requested_bytes {
            anyhow::bail!("read returned more bytes than requested");
        }
        let digest =
            hash_tracee_buffer_prefix(pid, &pending.buffers, bytes, "copy delivered read bytes")?;
        let file_value = match (
            pending.request.identity.as_ref(),
            pending.request.offset,
            pending.backing_file,
        ) {
            (Some(identity), Some(offset), Some(backing_file))
                if identity.kind == CapturedDescriptorKind::RegularFile =>
            {
                let value = witness_regular_file_read(
                    backing_file,
                    identity.device,
                    identity.inode,
                    offset,
                    bytes,
                    digest,
                )?;
                if let CapturedMappedFileValue::WholeFile {
                    bytes: file_bytes, ..
                } = value
                {
                    if bytes == 0 && pending.request.requested_bytes != 0 && offset < file_bytes {
                        anyhow::bail!("zero-byte read does not match the witnessed file size");
                    }
                }
                Some(value)
            }
            _ => None,
        };
        CapturedReadOutcome::Read {
            bytes,
            digest,
            file_value,
        }
    } else {
        anyhow::bail!("read syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::ReadAttempt {
        process: process.id,
        attempt: CapturedReadAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_getdents64_attempt(
    process: &mut PendingProcess,
    pid: Pid,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_getdents64
        .take()
        .context("getdents64 syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno = i32::try_from(-exit.return_value).context("getdents64 errno exceeds i32")?;
        CapturedGetdents64Outcome::Failed(errno)
    } else if exit.is_error == 0 && exit.return_value >= 0 {
        if !pending
            .request
            .identity
            .as_ref()
            .is_some_and(|identity| identity.kind == CapturedDescriptorKind::Directory)
        {
            anyhow::bail!("successful getdents64 request has no directory identity");
        }
        let returned_bytes =
            checked_getdents64_result_size(exit.return_value, pending.request.requested_bytes)?;
        let bytes = copy_tracee_bytes(
            pid,
            pending.buffer_address,
            usize::try_from(returned_bytes).context("getdents64 byte count exceeds usize")?,
            "copy delivered getdents64 bytes",
        )?;
        let entries = parse_linux_dirent64(&bytes)?;
        let position_after = read_descriptor_position(pid, pending.request.descriptor)?;
        let identity = pending.request.identity.as_ref().unwrap();
        let mut backing = pending
            .backing_directory
            .context("successful getdents64 has no directory identity")?
            .with_context(|| {
                format!(
                    "prepare stable directory handle for descriptor {}",
                    pending.request.descriptor
                )
            })?;
        if backing.key.device != identity.device || backing.key.inode != identity.inode {
            anyhow::bail!("successful getdents64 directory identity changed at entry");
        }
        let directory_value = witness_complete_directory(&mut backing.directory, backing.key)?;
        if directory_value != backing.entry_value {
            anyhow::bail!("enumerated directory changed across the getdents64 boundary");
        }
        CapturedGetdents64Outcome::Enumerated {
            returned_bytes,
            position_after,
            entries,
            directory_value,
        }
    } else {
        anyhow::bail!("getdents64 syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::Getdents64Attempt {
        process: process.id,
        attempt: CapturedGetdents64Attempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn checked_getdents64_result_size(returned: i64, requested: u32) -> Result<u32> {
    let returned = u32::try_from(returned).context("getdents64 byte count exceeds u32")?;
    if returned > requested {
        anyhow::bail!("getdents64 returned more bytes than requested");
    }
    if returned > MAX_GETDENTS64_RESULT_BYTES {
        anyhow::bail!("getdents64 result exceeds the capture byte limit");
    }
    Ok(returned)
}

fn finish_mmap_attempt(
    process: &mut PendingProcess,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
    witnesses: &mut MappedFileWitnessCache,
) -> Result<()> {
    let pending = process
        .pending_mmap
        .take()
        .context("mmap syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno = i32::try_from(-exit.return_value).context("mmap errno exceeds i32")?;
        CapturedMmapOutcome::Failed(errno)
    } else if exit.is_error == 0 {
        let anonymous = pending.request.flags & LINUX_MAP_ANONYMOUS != 0;
        let file_value = if anonymous {
            None
        } else {
            let identity = pending
                .request
                .identity
                .as_ref()
                .context("successful file-backed mmap has no descriptor identity")?;
            let backing_file = pending
                .backing_file
                .context("successful file-backed mmap has no stable backing file")?;
            Some(witness_regular_mapped_file(
                backing_file,
                identity.device,
                identity.inode,
                witnesses,
            )?)
        };
        CapturedMmapOutcome::Mapped {
            address: exit.return_value as u64,
            file_value,
        }
    } else {
        anyhow::bail!("mmap syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::MmapAttempt {
        process: process.id,
        attempt: CapturedMmapAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_write_attempt(
    process: &mut PendingProcess,
    pid: Pid,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_write
        .take()
        .context("write syscall exit has no captured request")?;
    let outcome = if exit.is_error != 0 && exit.return_value < 0 {
        let errno = i32::try_from(-exit.return_value).context("write errno exceeds i32")?;
        CapturedWriteOutcome::Failed(errno)
    } else if exit.is_error == 0 && exit.return_value >= 0 {
        let bytes = u64::try_from(exit.return_value).context("write byte count is negative")?;
        if bytes > pending.request.requested_bytes {
            anyhow::bail!("write consumed more bytes than requested");
        }
        let digest =
            hash_tracee_buffer_prefix(pid, &[pending.buffer], bytes, "copy consumed write bytes")?;
        CapturedWriteOutcome::Wrote { bytes, digest }
    } else {
        anyhow::bail!("write syscall returned an inconsistent result");
    };
    sink.record(ProcessCaptureEvent::WriteAttempt {
        process: process.id,
        attempt: CapturedWriteAttempt {
            syscall_index: pending.syscall_index,
            request: pending.request,
            outcome,
        },
    })?;
    Ok(())
}

fn finish_unmodeled_syscall_attempt(
    process: &mut PendingProcess,
    exit: SyscallExit,
    sink: &mut impl ProcessCaptureSink,
) -> Result<()> {
    let pending = process
        .pending_unmodeled_syscall
        .take()
        .context("unmodeled syscall exit has no captured request")?;
    sink.record(ProcessCaptureEvent::UnmodeledSyscallAttempt {
        process: process.id,
        attempt: CapturedUnmodeledSyscallAttempt {
            syscall_index: pending.syscall_index,
            number: pending.number,
            arguments: pending.arguments,
            outcome: CapturedUnmodeledSyscallOutcome::Returned {
                return_value: exit.return_value,
                is_error: exit.is_error != 0,
            },
        },
    })
}

fn read_exec_request(pid: Pid, entry: SyscallEntry) -> Result<CapturedExecRequest> {
    let (dirfd, path_address, argv_address, environment_address, flags) =
        if entry.number == libc::SYS_execve as u64 {
            (
                libc::AT_FDCWD,
                entry.arguments[0],
                entry.arguments[1],
                entry.arguments[2],
                0,
            )
        } else if entry.number == libc::SYS_execveat as u64 {
            (
                entry.arguments[0] as i32,
                entry.arguments[1],
                entry.arguments[2],
                entry.arguments[3],
                entry.arguments[4] as u32,
            )
        } else {
            anyhow::bail!("non-exec syscall cannot produce an exec request");
        };
    let mut budget = MAX_EXEC_REQUEST_BYTES;
    let (executable, directory) =
        read_requested_path(pid, dirfd, path_address, &mut budget, "executable")?;
    let argv = read_tracee_string_vector(pid, argv_address, &mut budget)
        .context("read requested exec arguments")?;
    let environment = read_tracee_string_vector(pid, environment_address, &mut budget)
        .context("read requested exec environment")?;
    Ok(CapturedExecRequest {
        executable,
        directory,
        argv,
        environment,
        flags,
    })
}

fn read_open_request(pid: Pid, entry: SyscallEntry) -> Result<Option<CapturedOpenRequest>> {
    let Some(system_call) = open_system_call(entry.number) else {
        return Ok(None);
    };
    let (dirfd, path_address, flags, raw_mode, resolve) = match system_call {
        CapturedOpenSystemCall::Open => (
            libc::AT_FDCWD,
            entry.arguments[0],
            entry.arguments[1] & u64::from(u32::MAX),
            entry.arguments[2] & u64::from(u32::MAX),
            0,
        ),
        CapturedOpenSystemCall::OpenAt => (
            entry.arguments[0] as i32,
            entry.arguments[1],
            entry.arguments[2] & u64::from(u32::MAX),
            entry.arguments[3] & u64::from(u32::MAX),
            0,
        ),
        CapturedOpenSystemCall::OpenAt2 => {
            if entry.arguments[3] != OPEN_HOW_SIZE {
                anyhow::bail!("openat2 request uses an unsupported open_how size");
            }
            let bytes = read_tracee_bytes(pid, entry.arguments[2], OPEN_HOW_SIZE as usize)
                .context("read requested openat2 arguments")?;
            (
                entry.arguments[0] as i32,
                entry.arguments[1],
                u64::from_ne_bytes(bytes[0..8].try_into().context("decode openat2 flags")?),
                u64::from_ne_bytes(bytes[8..16].try_into().context("decode openat2 mode")?),
                u64::from_ne_bytes(bytes[16..24].try_into().context("decode openat2 resolve")?),
            )
        }
        CapturedOpenSystemCall::Creat => (
            libc::AT_FDCWD,
            entry.arguments[0],
            u64::from((libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC) as u32),
            entry.arguments[1] & u64::from(u32::MAX),
            0,
        ),
    };
    let mode = if system_call == CapturedOpenSystemCall::Creat || open_mode_is_used(flags) {
        raw_mode
    } else {
        0
    };
    let mut budget = MAX_EXEC_REQUEST_BYTES;
    let (path, directory) = read_requested_path(pid, dirfd, path_address, &mut budget, "open")?;
    Ok(Some(CapturedOpenRequest {
        system_call,
        path,
        directory,
        flags,
        mode,
        resolve,
    }))
}

fn read_newfstatat_request(
    pid: Pid,
    entry: SyscallEntry,
) -> Result<Option<(CapturedNewfstatatRequest, u64)>> {
    if entry.number != libc::SYS_newfstatat as u64 {
        return Ok(None);
    }
    let directory_descriptor = entry.arguments[0] as i32;
    let path_address = entry.arguments[1];
    let path_is_null = path_address == 0;
    let path = if path_is_null {
        Vec::new()
    } else {
        let mut budget = LINUX_PATH_MAX;
        read_tracee_c_string(pid, path_address, &mut budget)
            .context("read requested newfstatat path")?
    };
    let base = if path.first() == Some(&b'/') {
        None
    } else {
        let proc_path = if directory_descriptor == libc::AT_FDCWD {
            PathBuf::from(format!("/proc/{pid}/cwd"))
        } else {
            PathBuf::from(format!("/proc/{pid}/fd/{directory_descriptor}"))
        };
        match std::fs::read_link(proc_path) {
            Ok(target) => Some(target.as_os_str().as_bytes().to_vec()),
            Err(error)
                if directory_descriptor != libc::AT_FDCWD
                    && error.raw_os_error() == Some(libc::ENOENT) =>
            {
                None
            }
            Err(error) => {
                return Err(error).context("resolve requested newfstatat directory");
            }
        }
    };
    Ok(Some((
        CapturedNewfstatatRequest {
            directory_descriptor,
            path,
            path_is_null,
            base,
            flags: entry.arguments[3] as u32,
        },
        entry.arguments[2],
    )))
}

fn read_fstat_request(
    pid: Pid,
    entry: SyscallEntry,
) -> Result<Option<(CapturedFstatRequest, u64)>> {
    if entry.number != libc::SYS_fstat as u64 {
        return Ok(None);
    }
    let descriptor = entry.arguments[0] as i32;
    let identity = read_descriptor_identity(pid, descriptor)?;
    let mount_id = identity
        .as_ref()
        .map(|_| read_descriptor_mount_id(pid, descriptor))
        .transpose()?;
    Ok(Some((
        CapturedFstatRequest {
            descriptor,
            identity,
            mount_id,
            output_is_null: entry.arguments[1] == 0,
        },
        entry.arguments[1],
    )))
}

fn read_getrandom_request(entry: SyscallEntry) -> Option<(CapturedGetrandomRequest, ReadBuffer)> {
    if entry.number != libc::SYS_getrandom as u64 {
        return None;
    }
    Some((
        CapturedGetrandomRequest {
            requested_bytes: entry.arguments[1],
            flags: entry.arguments[2] as u32,
            output_is_null: entry.arguments[0] == 0,
        },
        ReadBuffer {
            address: entry.arguments[0],
            length: entry.arguments[1],
        },
    ))
}

fn runtime_observation_system_call(number: u64) -> bool {
    number == libc::SYS_getrusage as u64
        || number == libc::SYS_sysinfo as u64
        || number == libc::SYS_uname as u64
        || number == libc::SYS_sched_getaffinity as u64
        || legacy_poll_system_call(number)
}

#[cfg(target_arch = "x86_64")]
fn legacy_poll_system_call(number: u64) -> bool {
    number == libc::SYS_poll as u64
}

#[cfg(not(target_arch = "x86_64"))]
fn legacy_poll_system_call(_number: u64) -> bool {
    false
}

fn read_runtime_observation_request(
    pid: Pid,
    entry: SyscallEntry,
) -> Option<(CapturedRuntimeObservationRequest, u64)> {
    if std::mem::size_of::<libc::c_long>() != 8 {
        return None;
    }
    if entry.number == libc::SYS_getrusage as u64
        && signed_i32_syscall_argument(entry.arguments[0]) == Some(libc::RUSAGE_SELF)
    {
        Some((
            CapturedRuntimeObservationRequest::CallingProcessResourceUsage {
                output_is_null: entry.arguments[1] == 0,
            },
            entry.arguments[1],
        ))
    } else if entry.number == libc::SYS_sysinfo as u64 {
        Some((
            CapturedRuntimeObservationRequest::SystemInformation {
                output_is_null: entry.arguments[0] == 0,
            },
            entry.arguments[0],
        ))
    } else if entry.number == libc::SYS_uname as u64 {
        Some((
            CapturedRuntimeObservationRequest::SystemIdentity {
                output_is_null: entry.arguments[0] == 0,
            },
            entry.arguments[0],
        ))
    } else if entry.number == libc::SYS_sched_getaffinity as u64
        && (entry.arguments[0] == 0
            || i32::try_from(entry.arguments[0]).ok() == Some(pid.as_raw()))
        && entry.arguments[1] != 0
        && entry.arguments[1] <= 128
        && entry.arguments[1] % std::mem::size_of::<libc::c_ulong>() as u64 == 0
    {
        Some((
            CapturedRuntimeObservationRequest::CallingThreadAffinity {
                capacity: entry.arguments[1],
                output_is_null: entry.arguments[2] == 0,
            },
            entry.arguments[2],
        ))
    } else {
        None
    }
}

#[cfg(target_arch = "x86_64")]
fn read_poll_runtime_observation_request(
    pid: Pid,
    entry: SyscallEntry,
) -> Result<Option<(CapturedRuntimeObservationRequest, u64)>> {
    if entry.number != libc::SYS_poll as u64
        || std::mem::size_of::<libc::pollfd>() != 8
        || entry.arguments[0] == 0
        || !matches!(entry.arguments[1], 3 | 4)
        || signed_i32_syscall_argument(entry.arguments[2]) != Some(0)
    {
        return Ok(None);
    }
    let descriptor_count = usize::try_from(entry.arguments[1])
        .context("poll descriptor count exceeds usize")?;
    let byte_count = descriptor_count
        .checked_mul(std::mem::size_of::<libc::pollfd>())
        .context("poll request bytes overflowed")?;
    let bytes = copy_tracee_bytes(
        pid,
        entry.arguments[0],
        byte_count,
        "copy descriptor-readiness request",
    )?;
    let mut descriptors = Vec::with_capacity(descriptor_count);
    for bytes in bytes.chunks_exact(std::mem::size_of::<libc::pollfd>()) {
        // SAFETY: each chunk contains one copied native pollfd; read_unaligned
        // copies it before any field is inspected.
        let requested =
            unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<libc::pollfd>()) };
        let identity = read_descriptor_identity(pid, requested.fd)?;
        let mount_id = identity
            .as_ref()
            .map(|_| read_descriptor_mount_id(pid, requested.fd))
            .transpose()?;
        descriptors.push(CapturedPollDescriptor {
            descriptor: requested.fd,
            identity,
            mount_id,
            requested_events: requested.events,
        });
    }
    let request = CapturedRuntimeObservationRequest::DescriptorReadiness {
        descriptors,
        timeout_milliseconds: 0,
    };
    if !crate::valid_captured_poll_request(&request) {
        return Ok(None);
    }
    Ok(Some((request, entry.arguments[0])))
}

#[cfg(not(target_arch = "x86_64"))]
fn read_poll_runtime_observation_request(
    _pid: Pid,
    _entry: SyscallEntry,
) -> Result<Option<(CapturedRuntimeObservationRequest, u64)>> {
    Ok(None)
}

fn read_prlimit_request(
    pid: Pid,
    entry: SyscallEntry,
) -> Result<Option<(CapturedPrlimitRequest, Option<CapturedResourceLimit>, u64)>> {
    if entry.number != libc::SYS_prlimit64 as u64 {
        return Ok(None);
    }
    let new_limit = if entry.arguments[2] == 0 {
        None
    } else {
        Some(copy_resource_limit(
            pid,
            entry.arguments[2],
            "copy requested prlimit value",
        )?)
    };
    Ok(Some((
        CapturedPrlimitRequest {
            process: entry.arguments[0] as i32,
            resource: entry.arguments[1] as u32,
            new_limit_is_null: entry.arguments[2] == 0,
            old_limit_is_null: entry.arguments[3] == 0,
        },
        new_limit,
        entry.arguments[3],
    )))
}

fn read_close_request(
    pid: Pid,
    entry: SyscallEntry,
) -> Result<Option<(CapturedCloseRequest, Option<File>)>> {
    if entry.number != libc::SYS_close as u64 {
        return Ok(None);
    }
    let descriptor = entry.arguments[0] as i32;
    let identity = read_descriptor_identity(pid, descriptor)?;
    let mount_id = identity
        .as_ref()
        .map(|_| read_descriptor_mount_id(pid, descriptor))
        .transpose()?;
    let backing_file = match identity.as_ref() {
        Some(identity)
            if identity.kind == CapturedDescriptorKind::RegularFile
                && regular_file_value_target(&identity.target) =>
        {
            open_tracee_descriptor_for_witness(pid, descriptor)?
        }
        _ => None,
    };
    Ok(Some((
        CapturedCloseRequest {
            descriptor,
            identity,
            mount_id,
        },
        backing_file,
    )))
}

fn read_seek_request(pid: Pid, entry: SyscallEntry) -> Result<Option<CapturedSeekRequest>> {
    if entry.number != libc::SYS_lseek as u64 {
        return Ok(None);
    }
    let descriptor = entry.arguments[0] as i32;
    let identity = read_descriptor_identity(pid, descriptor)?;
    let mount_id = identity
        .as_ref()
        .map(|_| read_descriptor_mount_id(pid, descriptor))
        .transpose()?;
    Ok(Some(CapturedSeekRequest {
        descriptor,
        identity,
        mount_id,
        offset: entry.arguments[1] as i64,
        whence: entry.arguments[2],
    }))
}

#[cfg(target_arch = "x86_64")]
fn read_file_advice_request(
    pid: Pid,
    entry: SyscallEntry,
) -> Result<Option<CapturedFileAdviceRequest>> {
    if entry.number != libc::SYS_fadvise64 as u64
        || entry.arguments[1] != 0
        || entry.arguments[2] != 0
        || entry.arguments[3] != libc::POSIX_FADV_SEQUENTIAL as u64
    {
        return Ok(None);
    }
    let descriptor = entry.arguments[0] as i32;
    let identity = read_descriptor_identity(pid, descriptor)?;
    let mount_id = identity
        .as_ref()
        .map(|_| read_descriptor_mount_id(pid, descriptor))
        .transpose()?;
    Ok(Some(CapturedFileAdviceRequest {
        descriptor,
        identity,
        mount_id,
        offset: entry.arguments[1],
        length: entry.arguments[2],
        advice: entry.arguments[3] as i32,
    }))
}

#[cfg(not(target_arch = "x86_64"))]
fn read_file_advice_request(
    _pid: Pid,
    _entry: SyscallEntry,
) -> Result<Option<CapturedFileAdviceRequest>> {
    Ok(None)
}

fn read_fcntl_request(pid: Pid, entry: SyscallEntry) -> Result<Option<CapturedFcntlRequest>> {
    if entry.number != libc::SYS_fcntl as u64 {
        return Ok(None);
    }
    let descriptor = entry.arguments[0] as i32;
    let identity = read_descriptor_identity(pid, descriptor)?;
    let mount_id = identity
        .as_ref()
        .map(|_| read_descriptor_mount_id(pid, descriptor))
        .transpose()?;
    let command = entry.arguments[1] as u32;
    let record_lock = if command == libc::F_SETLKW as u32 && entry.arguments[2] != 0 {
        let bytes = match read_tracee_bytes(
            pid,
            entry.arguments[2],
            std::mem::size_of::<libc::flock>(),
        ) {
            Ok(bytes) => bytes,
            Err(_) => Vec::new(),
        };
        if bytes.is_empty() {
            None
        } else {
            // SAFETY: `bytes` contains exactly one native `libc::flock`
            // copied from the stopped tracee. Every field is an integer and
            // `read_unaligned` does not impose alignment on the byte vector.
            let native = unsafe {
                std::ptr::read_unaligned(bytes.as_ptr().cast::<libc::flock>())
            };
            Some(CapturedFcntlRecordLock {
                lock_type: native.l_type,
                whence: native.l_whence,
                start: native.l_start,
                length: native.l_len,
            })
        }
    } else {
        None
    };
    Ok(Some(CapturedFcntlRequest {
        descriptor,
        identity,
        mount_id,
        command,
        argument: entry.arguments[2],
        record_lock,
    }))
}

fn read_ioctl_request(pid: Pid, entry: SyscallEntry) -> Result<Option<CapturedIoctlRequest>> {
    if entry.number != libc::SYS_ioctl as u64
        || (entry.arguments[1] != libc::TCGETS as u64
            && entry.arguments[1] != libc::TIOCGWINSZ as u64)
    {
        return Ok(None);
    }
    let descriptor = entry.arguments[0] as i32;
    let Some(identity) = read_descriptor_identity(pid, descriptor)? else {
        return Ok(None);
    };
    let mount_id = read_descriptor_mount_id(pid, descriptor)?;
    let request = CapturedIoctlRequest {
        descriptor,
        identity,
        mount_id,
        command: entry.arguments[1],
    };
    Ok(request.is_terminal_probe().then_some(request))
}

fn read_pipe_request(entry: SyscallEntry) -> Option<(CapturedPipeRequest, u64)> {
    (entry.number == libc::SYS_pipe2 as u64).then_some((
        CapturedPipeRequest {
            flags: entry.arguments[1] as u32,
        },
        entry.arguments[0],
    ))
}

fn read_epoll_create_request(entry: SyscallEntry) -> Option<CapturedEpollCreateRequest> {
    (entry.number == libc::SYS_epoll_create1 as u64
        && (entry.arguments[0] == 0 || entry.arguments[0] == libc::EPOLL_CLOEXEC as u64))
        .then_some(CapturedEpollCreateRequest {
            flags: entry.arguments[0] as u32,
        })
}

fn read_socket_create_request(
    entry: SyscallEntry,
) -> Option<CapturedSocketCreateRequest> {
    if entry.number != libc::SYS_socket as u64 {
        return None;
    }
    let domain = signed_i32_syscall_argument(entry.arguments[0])?;
    let socket_type = signed_i32_syscall_argument(entry.arguments[1])?;
    let protocol = signed_i32_syscall_argument(entry.arguments[2])?;
    let allowed_flags = libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK;
    (domain == libc::AF_UNIX
        && socket_type & !allowed_flags == libc::SOCK_STREAM
        && protocol == 0)
        .then_some(CapturedSocketCreateRequest {
            domain,
            socket_type,
            protocol,
        })
}

fn read_socket_connect_request(
    pid: Pid,
    entry: SyscallEntry,
) -> Result<Option<CapturedSocketConnectRequest>> {
    if entry.number != libc::SYS_connect as u64 {
        return Ok(None);
    }
    let Some(descriptor) = signed_i32_syscall_argument(entry.arguments[0])
    else {
        return Ok(None);
    };
    let Ok(address_length) = u32::try_from(entry.arguments[2]) else {
        return Ok(None);
    };
    let family_bytes = std::mem::size_of::<libc::sa_family_t>();
    let address_bytes = std::mem::size_of::<libc::sockaddr_un>();
    let Ok(length) = usize::try_from(address_length) else {
        return Ok(None);
    };
    if !(family_bytes..=address_bytes).contains(&length) {
        return Ok(None);
    }
    let Some(identity) = read_descriptor_identity(pid, descriptor)? else {
        return Ok(None);
    };
    if identity.kind != CapturedDescriptorKind::Socket {
        return Ok(None);
    }
    let mount_id = read_descriptor_mount_id(pid, descriptor)?;
    let bytes = match read_tracee_bytes(pid, entry.arguments[1], length) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let family = libc::sa_family_t::from_ne_bytes(
        bytes[..family_bytes].try_into().unwrap(),
    );
    if i32::from(family) != libc::AF_UNIX {
        return Ok(None);
    }
    let path = &bytes[family_bytes..];
    let address = if path.is_empty() {
        CapturedUnixSocketAddress::Unnamed
    } else if path[0] == 0 {
        CapturedUnixSocketAddress::Abstract {
            name: path[1..].to_vec(),
        }
    } else if let Some(terminator) = path.iter().position(|byte| *byte == 0) {
        CapturedUnixSocketAddress::Pathname {
            path: path[..terminator].to_vec(),
            terminated: true,
        }
    } else {
        CapturedUnixSocketAddress::Pathname {
            path: path.to_vec(),
            terminated: false,
        }
    };
    Ok(Some(CapturedSocketConnectRequest {
        descriptor,
        identity,
        mount_id,
        address_length,
        address,
    }))
}

fn descriptor_duplication_system_call(
    number: u64,
) -> Option<CapturedDescriptorDuplicationSystemCall> {
    if number == libc::SYS_dup as u64 {
        Some(CapturedDescriptorDuplicationSystemCall::Dup)
    } else if number == libc::SYS_dup2 as u64 {
        Some(CapturedDescriptorDuplicationSystemCall::Dup2)
    } else if number == libc::SYS_dup3 as u64 {
        Some(CapturedDescriptorDuplicationSystemCall::Dup3)
    } else {
        None
    }
}

fn read_duplication_request(
    pid: Pid,
    entry: SyscallEntry,
) -> Result<Option<(CapturedDup2Request, Option<File>)>> {
    let Some(system_call) = descriptor_duplication_system_call(entry.number) else {
        return Ok(None);
    };
    let source_descriptor = match system_call {
        CapturedDescriptorDuplicationSystemCall::Dup3 => {
            let Some(source) = signed_i32_syscall_argument(entry.arguments[0]) else {
                return Ok(None);
            };
            source
        }
        _ => entry.arguments[0] as i32,
    };
    if system_call == CapturedDescriptorDuplicationSystemCall::Dup3
        && entry.arguments[2] != 0
    {
        return Ok(None);
    }
    let source_identity = read_descriptor_identity(pid, source_descriptor)?;
    let source_mount_id = source_identity
        .as_ref()
        .map(|_| read_descriptor_mount_id(pid, source_descriptor))
        .transpose()?;
    let target_descriptor = match system_call {
        CapturedDescriptorDuplicationSystemCall::Dup => lowest_available_descriptor(pid)?,
        CapturedDescriptorDuplicationSystemCall::Dup2 => entry.arguments[1] as i32,
        CapturedDescriptorDuplicationSystemCall::Dup3 => {
            let Some(target) = signed_i32_syscall_argument(entry.arguments[1]) else {
                return Ok(None);
            };
            if target == source_descriptor {
                return Ok(None);
            }
            target
        }
    };
    let target_identity = match system_call {
        CapturedDescriptorDuplicationSystemCall::Dup => None,
        CapturedDescriptorDuplicationSystemCall::Dup2
        | CapturedDescriptorDuplicationSystemCall::Dup3 => {
            read_descriptor_identity(pid, target_descriptor)?
        }
    };
    let target_mount_id = match (system_call, target_identity.as_ref()) {
        (
            CapturedDescriptorDuplicationSystemCall::Dup2
            | CapturedDescriptorDuplicationSystemCall::Dup3,
            Some(_),
        ) => {
            Some(read_descriptor_mount_id(pid, target_descriptor)?)
        }
        _ => None,
    };
    let replaced_backing_file = match target_identity.as_ref() {
        Some(identity)
            if source_descriptor != target_descriptor
                && identity.kind == CapturedDescriptorKind::RegularFile
                && regular_file_value_target(&identity.target) =>
        {
            open_tracee_descriptor_for_witness(pid, target_descriptor)?
        }
        _ => None,
    };
    Ok(Some((
        CapturedDup2Request {
            system_call,
            source_descriptor,
            source_identity,
            source_mount_id,
            target_descriptor,
            target_identity,
            target_mount_id,
        },
        replaced_backing_file,
    )))
}

fn lowest_available_descriptor(pid: Pid) -> Result<i32> {
    const MAX_DUPLICATION_DESCRIPTORS: usize = 4096;

    let directory = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .context("enumerate process descriptors before dup")?;
    let mut descriptors = BTreeSet::new();
    for entry in directory {
        let entry = entry.context("read process descriptor before dup")?;
        let name = entry.file_name();
        let name = std::str::from_utf8(name.as_bytes())
            .context("process descriptor name before dup is not ASCII")?;
        let descriptor = name
            .parse::<i32>()
            .context("process descriptor name before dup is not numeric")?;
        if descriptor < 0 {
            anyhow::bail!("process descriptor before dup is negative");
        }
        descriptors.insert(descriptor);
        if descriptors.len() > MAX_DUPLICATION_DESCRIPTORS {
            anyhow::bail!("process has too many descriptors to model dup");
        }
    }

    let mut candidate = 0_i32;
    for descriptor in descriptors {
        if descriptor == candidate {
            candidate = candidate
                .checked_add(1)
                .context("lowest available descriptor exceeds i32")?;
        } else if descriptor > candidate {
            break;
        }
    }
    Ok(candidate)
}

fn signal_state_system_call(number: u64) -> bool {
    number == libc::SYS_rt_sigaction as u64 || number == libc::SYS_rt_sigprocmask as u64
}

#[cfg(target_arch = "x86_64")]
fn read_signal_state_request(
    pid: Pid,
    entry: SyscallEntry,
) -> Result<Option<(CapturedSignalStateRequest, u64)>> {
    if !signal_state_system_call(entry.number) {
        return Ok(None);
    }
    let signal_set_bytes = entry.arguments[3];
    if signal_set_bytes == 0 || signal_set_bytes > LINUX_SIGNAL_SET_BYTES_LIMIT {
        return Ok(None);
    }
    if entry.number == libc::SYS_rt_sigaction as u64 {
        let action = if entry.arguments[1] == 0 {
            None
        } else {
            let bytes = signal_set_bytes
                .checked_add(LINUX_RT_SIGACTION_PREFIX_BYTES)
                .context("requested signal-action size overflowed")?;
            Some(copy_tracee_bytes(
                pid,
                entry.arguments[1],
                usize::try_from(bytes).context("signal-action size exceeds usize")?,
                "copy requested signal action",
            )?)
        };
        Ok(Some((
            CapturedSignalStateRequest::Action {
                signal: entry.arguments[0] as u32,
                action,
                old_action_is_null: entry.arguments[2] == 0,
                signal_set_bytes,
            },
            entry.arguments[2],
        )))
    } else {
        let mask = if entry.arguments[1] == 0 {
            None
        } else {
            Some(copy_tracee_bytes(
                pid,
                entry.arguments[1],
                usize::try_from(signal_set_bytes).context("signal-mask size exceeds usize")?,
                "copy requested signal mask",
            )?)
        };
        Ok(Some((
            CapturedSignalStateRequest::Mask {
                operation: entry.arguments[0] as i32,
                mask,
                old_mask_is_null: entry.arguments[2] == 0,
                signal_set_bytes,
            },
            entry.arguments[2],
        )))
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn read_signal_state_request(
    _pid: Pid,
    _entry: SyscallEntry,
) -> Result<Option<(CapturedSignalStateRequest, u64)>> {
    Ok(None)
}

fn read_umask_request(entry: SyscallEntry) -> Option<u32> {
    (entry.number == libc::SYS_umask as u64).then_some(entry.arguments[0] as u32)
}

fn read_credential_request(entry: SyscallEntry) -> Option<(CapturedCredentialRequest, u64)> {
    let id = |value: u64| {
        let value = value as u32;
        (value != u32::MAX).then_some(value)
    };
    if entry.number == libc::SYS_getuid as u64 {
        Some((CapturedCredentialRequest::Getuid, 0))
    } else if entry.number == libc::SYS_geteuid as u64 {
        Some((CapturedCredentialRequest::Geteuid, 0))
    } else if entry.number == libc::SYS_getgid as u64 {
        Some((CapturedCredentialRequest::Getgid, 0))
    } else if entry.number == libc::SYS_getegid as u64 {
        Some((CapturedCredentialRequest::Getegid, 0))
    } else if entry.number == libc::SYS_getgroups as u64 {
        let capacity = signed_i32_syscall_argument(entry.arguments[0])?;
        if !(0..=65_536).contains(&capacity) {
            return None;
        }
        Some((
            CapturedCredentialRequest::Getgroups {
                capacity: capacity as u32,
                output_is_null: entry.arguments[1] == 0,
            },
            entry.arguments[1],
        ))
    } else if entry.number == libc::SYS_prctl as u64
        && entry.arguments[0] == libc::PR_CAPBSET_READ as u64
    {
        let capability = u32::try_from(entry.arguments[1]).ok()?;
        crate::supported_capability_bounding_set_query(capability).then_some((
            CapturedCredentialRequest::CapabilityBoundingSetRead { capability },
            0,
        ))
    } else if entry.number == libc::SYS_setresuid as u64 {
        Some((
            CapturedCredentialRequest::Setresuid {
                real: id(entry.arguments[0]),
                effective: id(entry.arguments[1]),
                saved: id(entry.arguments[2]),
            },
            0,
        ))
    } else if entry.number == libc::SYS_setresgid as u64 {
        Some((
            CapturedCredentialRequest::Setresgid {
                real: id(entry.arguments[0]),
                effective: id(entry.arguments[1]),
                saved: id(entry.arguments[2]),
            },
            0,
        ))
    } else {
        None
    }
}

fn read_futex_wake_request(entry: SyscallEntry) -> Option<CapturedFutexWakeRequest> {
    const FUTEX_WAKE_PRIVATE: u64 = 1 | 128;
    if entry.number != libc::SYS_futex as u64
        || entry.arguments[1] != FUTEX_WAKE_PRIVATE
        || entry.arguments[0] == 0
        || entry.arguments[0] % std::mem::align_of::<u32>() as u64 != 0
        || entry.arguments[2] > i32::MAX as u64
    {
        return None;
    }
    Some(CapturedFutexWakeRequest {
        address: entry.arguments[0],
        maximum_waiters: entry.arguments[2] as u32,
    })
}

fn read_process_creation_request(
    pid: Pid,
    entry: SyscallEntry,
) -> Result<Option<CapturedProcessCreationRequest>> {
    const CLONE_ARGS_BYTES: u64 = 88;
    const CLONE_VM: u64 = 0x0000_0100;
    const CLONE_VFORK: u64 = 0x0000_4000;
    const CLONE_CHILD_CLEARTID: u64 = 0x0020_0000;
    const CLONE_CHILD_SETTID: u64 = 0x0100_0000;
    const CLONE_CLEAR_SIGHAND: u64 = 0x0000_0001_0000_0000;
    const CLONE_SETTLS: u64 = 0x0008_0000;
    const CLONE_PARENT_SETTID: u64 = 0x0010_0000;

    #[cfg(target_arch = "x86_64")]
    if entry.number == libc::SYS_clone as u64 {
        let flags = entry.arguments[0];
        let fork_flags = CLONE_CHILD_CLEARTID | CLONE_CHILD_SETTID | libc::SIGCHLD as u64;
        if flags == fork_flags
            && entry.arguments[1] == 0
            && entry.arguments[2] == 0
            && entry.arguments[3] != 0
            && entry.arguments[4] == 0
        {
            return Ok(Some(CapturedProcessCreationRequest::Clone { flags }));
        }
        if supported_thread_clone_flags(flags)
            && entry.arguments[1] != 0
            && (flags & CLONE_PARENT_SETTID == 0 || entry.arguments[2] != 0)
            && (flags & (CLONE_CHILD_CLEARTID | CLONE_CHILD_SETTID) == 0
                || entry.arguments[3] != 0)
            && (flags & CLONE_SETTLS == 0 || entry.arguments[4] != 0)
        {
            return Ok(Some(CapturedProcessCreationRequest::Clone { flags }));
        }
        return Ok(None);
    }
    #[cfg(target_arch = "x86_64")]
    if entry.number == libc::SYS_vfork as u64 {
        return Ok(Some(CapturedProcessCreationRequest::Vfork));
    }
    if entry.number != libc::SYS_clone3 as u64
        || entry.arguments[0] == 0
        || entry.arguments[1] != CLONE_ARGS_BYTES
    {
        return Ok(None);
    }
    let bytes = match read_tracee_bytes(pid, entry.arguments[0], CLONE_ARGS_BYTES as usize) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let mut fields = [0u64; 11];
    for (field, bytes) in fields.iter_mut().zip(bytes.chunks_exact(8)) {
        *field = u64::from_ne_bytes(bytes.try_into().unwrap());
    }
    let [flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size, tls, set_tid, set_tid_size, cgroup] =
        fields;
    let vfork_flags = CLONE_VM | CLONE_VFORK;
    if supported_thread_clone_flags(flags)
        && exit_signal == 0
        && (flags & CLONE_CHILD_CLEARTID == 0 || child_tid != 0)
        && (flags & CLONE_PARENT_SETTID == 0 || parent_tid != 0)
        && stack != 0
        && stack_size != 0
        && (flags & CLONE_SETTLS == 0 || tls != 0)
        && set_tid == 0
        && set_tid_size == 0
        && cgroup == 0
    {
        return Ok(Some(CapturedProcessCreationRequest::Clone3 {
            flags,
            exit_signal: 0,
        }));
    }
    if supported_thread_clone_flags(flags) {
        anyhow::bail!(
            "unsupported pthread clone3 fields: pidfd={pidfd:#x}, \
             child_tid={child_tid:#x}, parent_tid={parent_tid:#x}, \
             exit_signal={exit_signal:#x}, stack={stack:#x}, \
             stack_size={stack_size:#x}, tls={tls:#x}, set_tid={set_tid:#x}, \
             set_tid_size={set_tid_size:#x}, cgroup={cgroup:#x}"
        );
    }
    if (flags != vfork_flags && flags != vfork_flags | CLONE_CLEAR_SIGHAND)
        || pidfd != 0
        || child_tid != 0
        || parent_tid != 0
        || exit_signal != libc::SIGCHLD as u64
        || stack == 0
        || stack_size == 0
        || tls != 0
        || set_tid != 0
        || set_tid_size != 0
        || cgroup != 0
    {
        return Ok(None);
    }
    Ok(Some(CapturedProcessCreationRequest::Clone3 {
        flags,
        exit_signal: exit_signal as u32,
    }))
}

fn supported_thread_clone_flags(flags: u64) -> bool {
    let required = libc::CLONE_VM as u64
        | libc::CLONE_FS as u64
        | libc::CLONE_FILES as u64
        | libc::CLONE_SIGHAND as u64
        | libc::CLONE_THREAD as u64
        | libc::CLONE_SYSVSEM as u64;
    let allowed = required
        | libc::CLONE_SETTLS as u64
        | libc::CLONE_PARENT_SETTID as u64
        | libc::CLONE_CHILD_CLEARTID as u64
        | libc::CLONE_CHILD_SETTID as u64;
    flags & required == required && flags & !allowed == 0
}

fn process_creation_shares_address_space(request: CapturedProcessCreationRequest) -> bool {
    match request {
        CapturedProcessCreationRequest::Clone { flags }
        | CapturedProcessCreationRequest::Clone3 { flags, .. } => {
            supported_thread_clone_flags(flags)
        }
        CapturedProcessCreationRequest::Vfork => false,
    }
}

fn process_creation_entry_matches_request(
    entry: SyscallEntry,
    request: CapturedProcessCreationRequest,
) -> bool {
    match request {
        CapturedProcessCreationRequest::Vfork => {
            #[cfg(target_arch = "x86_64")]
            {
                entry.number == libc::SYS_vfork as u64
            }
            #[cfg(not(target_arch = "x86_64"))]
            {
                false
            }
        }
        CapturedProcessCreationRequest::Clone { .. } => {
            #[cfg(target_arch = "x86_64")]
            {
                entry.number == libc::SYS_clone as u64
            }
            #[cfg(not(target_arch = "x86_64"))]
            {
                false
            }
        }
        CapturedProcessCreationRequest::Clone3 { .. } => entry.number == libc::SYS_clone3 as u64,
    }
}

fn read_child_wait_request(
    entry: SyscallEntry,
    process: CapturedProcessId,
    receipt_ids_by_pid: &BTreeMap<Pid, CapturedProcessId>,
    parents_by_receipt_id: &BTreeMap<CapturedProcessId, Option<CapturedProcessId>>,
) -> Option<(CapturedChildWaitRequest, u64)> {
    if entry.number != libc::SYS_wait4 as u64
        || (entry.arguments[2] != 0 && entry.arguments[2] != libc::WNOHANG as u64)
        || entry.arguments[3] != 0
    {
        return None;
    }
    let selector = match signed_i32_syscall_argument(entry.arguments[0])? {
        -1 => CapturedChildWaitSelector::AnyChild,
        child if child > 0 => {
            let child = *receipt_ids_by_pid.get(&Pid::from_raw(child))?;
            if parents_by_receipt_id.get(&child) != Some(&Some(process)) {
                return None;
            }
            CapturedChildWaitSelector::Child(child)
        }
        _ => return None,
    };
    let status_is_null = entry.arguments[1] == 0;
    if status_is_null
        && (selector != CapturedChildWaitSelector::AnyChild || entry.arguments[2] != 0)
    {
        return None;
    }
    Some((
        CapturedChildWaitRequest {
            selector,
            nonblocking: entry.arguments[2] == libc::WNOHANG as u64,
            status_is_null,
        },
        entry.arguments[1],
    ))
}

fn read_process_identity_request(
    pid: Pid,
    entry: SyscallEntry,
    process: CapturedProcessId,
    thread_group: CapturedProcessId,
    receipt_ids_by_pid: &BTreeMap<Pid, CapturedProcessId>,
    parents_by_receipt_id: &BTreeMap<CapturedProcessId, Option<CapturedProcessId>>,
    thread_groups_by_receipt_id: &BTreeMap<CapturedProcessId, CapturedProcessId>,
) -> Option<(
    CapturedProcessIdentityRequest,
    Option<(Pid, CapturedProcessId)>,
)> {
    if entry.number == libc::SYS_getpid as u64 {
        let group_pid = receipt_ids_by_pid
            .iter()
            .find_map(|(pid, identity)| (*identity == thread_group).then_some(*pid))?;
        return Some((
            CapturedProcessIdentityRequest::Getpid,
            Some((group_pid, thread_group)),
        ));
    }
    if entry.number == libc::SYS_gettid as u64 {
        return Some((
            CapturedProcessIdentityRequest::Gettid,
            Some((pid, process)),
        ));
    }
    if entry.number == libc::SYS_getpgrp as u64 {
        return Some((CapturedProcessIdentityRequest::Getpgrp, None));
    }
    if entry.number != libc::SYS_getppid as u64 {
        return None;
    }
    let creator = parents_by_receipt_id
        .get(&thread_group)
        .copied()
        .flatten()?;
    let parent = *thread_groups_by_receipt_id.get(&creator)?;
    let parent_pid = receipt_ids_by_pid
        .iter()
        .find_map(|(pid, identity)| (*identity == parent).then_some(*pid))?;
    Some((
        CapturedProcessIdentityRequest::Getppid,
        Some((parent_pid, parent)),
    ))
}

fn process_identity_entry_matches_request(
    entry: SyscallEntry,
    request: CapturedProcessIdentityRequest,
) -> bool {
    match request {
        CapturedProcessIdentityRequest::Getpid => entry.number == libc::SYS_getpid as u64,
        CapturedProcessIdentityRequest::Gettid => entry.number == libc::SYS_gettid as u64,
        CapturedProcessIdentityRequest::Getppid => entry.number == libc::SYS_getppid as u64,
        CapturedProcessIdentityRequest::Getpgrp => entry.number == libc::SYS_getpgrp as u64,
    }
}

fn signed_i32_syscall_argument(raw: u64) -> Option<i32> {
    let low = raw as u32;
    let signed = low as i32;
    (raw == u64::from(low) || raw == signed as i64 as u64).then_some(signed)
}

fn read_memory_state_request(entry: SyscallEntry) -> Option<CapturedMemoryStateRequest> {
    if entry.number == libc::SYS_brk as u64 {
        Some(CapturedMemoryStateRequest::Brk {
            address: entry.arguments[0],
        })
    } else if entry.number == libc::SYS_mprotect as u64 {
        Some(CapturedMemoryStateRequest::Mprotect {
            address: entry.arguments[0],
            length: entry.arguments[1],
            protection: entry.arguments[2] as u32,
        })
    } else if entry.number == libc::SYS_munmap as u64 {
        Some(CapturedMemoryStateRequest::Munmap {
            address: entry.arguments[0],
            length: entry.arguments[1],
        })
    } else {
        None
    }
}

fn read_thread_state_request(entry: SyscallEntry) -> Option<CapturedThreadStateRequest> {
    #[cfg(target_arch = "x86_64")]
    if entry.number == libc::SYS_arch_prctl as u64 {
        return Some(CapturedThreadStateRequest::ArchPrctl {
            operation: entry.arguments[0] as u32,
            address: entry.arguments[1],
        });
    }
    if entry.number == libc::SYS_set_tid_address as u64 {
        Some(CapturedThreadStateRequest::SetTidAddress {
            address: entry.arguments[0],
        })
    } else if entry.number == libc::SYS_set_robust_list as u64 {
        Some(CapturedThreadStateRequest::SetRobustList {
            address: entry.arguments[0],
            length: entry.arguments[1],
        })
    } else if entry.number == libc::SYS_rseq as u64 {
        Some(CapturedThreadStateRequest::Rseq {
            address: entry.arguments[0],
            length: entry.arguments[1] as u32,
            flags: entry.arguments[2] as u32,
            signature: entry.arguments[3] as u32,
        })
    } else {
        None
    }
}

fn thread_state_request_matches(entry: SyscallEntry, request: CapturedThreadStateRequest) -> bool {
    match request {
        CapturedThreadStateRequest::SignalReturn { .. } =>
            entry.number == libc::SYS_rt_sigreturn as u64,
        CapturedThreadStateRequest::SignalStack { .. } =>
            entry.number == libc::SYS_sigaltstack as u64,
        _ => read_thread_state_request(entry) == Some(request),
    }
}

#[cfg(target_arch = "x86_64")]
fn read_signal_stack_request(
    pid: Pid,
    entry: SyscallEntry,
) -> Result<Option<CapturedThreadStateRequest>> {
    const STACK_T_BYTES: usize = 24;
    if entry.number != libc::SYS_sigaltstack as u64
        || entry.arguments[0] == 0
        || entry.arguments[1] != 0
    {
        return Ok(None);
    }
    let bytes = match read_tracee_bytes(pid, entry.arguments[0], STACK_T_BYTES) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let address = u64::from_ne_bytes(bytes[0..8].try_into().unwrap());
    let flags = i32::from_ne_bytes(bytes[8..12].try_into().unwrap());
    let stack_bytes = u64::from_ne_bytes(bytes[16..24].try_into().unwrap());
    if address == 0 || flags != 0 || !matches!(stack_bytes, 16_384 | 65_536) {
        return Ok(None);
    }
    Ok(Some(CapturedThreadStateRequest::SignalStack {
        address,
        flags,
        bytes: stack_bytes,
        old_state_is_null: true,
    }))
}

#[cfg(not(target_arch = "x86_64"))]
fn read_signal_stack_request(
    _pid: Pid,
    _entry: SyscallEntry,
) -> Result<Option<CapturedThreadStateRequest>> {
    Ok(None)
}

#[cfg(target_arch = "x86_64")]
fn read_signal_return_request(
    pid: Pid,
    entry: SyscallEntry,
    stack_pointer: u64,
    caught_signal_frames: &mut Vec<u64>,
) -> Result<Option<(CapturedThreadStateRequest, i64)>> {
    if entry.number != libc::SYS_rt_sigreturn as u64 {
        return Ok(None);
    }

    // Native x86-64 enters rt_sigreturn with RSP immediately after the
    // frame's pretcode word. Saved RAX is 104 bytes into the sigcontext,
    // whose fixed prefix starts 40 bytes after that restorer RSP.
    const SAVED_RAX_FROM_RESTORER_RSP: u64 = 144;
    let address = stack_pointer
        .checked_add(SAVED_RAX_FROM_RESTORER_RSP)
        .context("signal-return saved-result address overflowed")?;
    let address = usize::try_from(address).context("signal-return address exceeds usize")?;
    let restored_return_value = read_tracee_word(pid, address)? as i64;
    let delivery = caught_signal_frames
        .pop()
        .context("signal return has no caught signal frame")?;
    Ok(Some((
        CapturedThreadStateRequest::SignalReturn {
            delivery,
            restored_accumulator: restored_return_value,
        },
        restored_return_value,
    )))
}

#[cfg(not(target_arch = "x86_64"))]
fn read_signal_return_request(
    _pid: Pid,
    _entry: SyscallEntry,
    _stack_pointer: u64,
    _caught_signal_frames: &mut Vec<u64>,
) -> Result<Option<(CapturedThreadStateRequest, i64)>> {
    Ok(None)
}

fn read_lifecycle_exit_request(entry: SyscallEntry) -> Option<CapturedLifecycleExitRequest> {
    let system_call = if entry.number == libc::SYS_exit as u64 {
        CapturedLifecycleExitSystemCall::Exit
    } else if entry.number == libc::SYS_exit_group as u64 {
        CapturedLifecycleExitSystemCall::ExitGroup
    } else {
        return None;
    };
    Some(CapturedLifecycleExitRequest {
        system_call,
        status: entry.arguments[0] as i32,
    })
}

fn capture_exit_file_values(
    pid: Pid,
    witnesses: &mut MappedFileWitnessCache,
) -> Result<Vec<CapturedExitFileValue>> {
    const MAX_EXIT_DESCRIPTORS: usize = 4096;

    let directory = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .context("enumerate exiting process descriptors")?;
    let mut descriptors = Vec::new();
    for entry in directory {
        let entry = entry.context("read exiting process descriptor entry")?;
        let name = entry.file_name();
        let name = std::str::from_utf8(name.as_bytes())
            .context("exiting process descriptor name is not ASCII")?;
        let descriptor = name
            .parse::<i32>()
            .context("exiting process descriptor name is not numeric")?;
        descriptors.push(descriptor);
        if descriptors.len() > MAX_EXIT_DESCRIPTORS {
            anyhow::bail!("exiting process has too many descriptors to witness");
        }
    }
    descriptors.sort_unstable();

    let mut values = Vec::new();
    for descriptor in descriptors {
        let Some(identity) = read_descriptor_identity(pid, descriptor)? else {
            anyhow::bail!("exiting process descriptor disappeared while stopped");
        };
        if identity.kind != CapturedDescriptorKind::RegularFile
            || !regular_file_value_target(&identity.target)
        {
            continue;
        }
        let mount_id = read_descriptor_mount_id(pid, descriptor)?;
        let file = open_tracee_descriptor_for_witness(pid, descriptor)?
            .context("reopen exiting regular descriptor")?;
        let value = witness_regular_mapped_file(
            file,
            identity.device,
            identity.inode,
            witnesses,
        )?;
        values.push(CapturedExitFileValue {
            descriptor,
            identity,
            mount_id,
            value,
        });
    }
    Ok(values)
}

fn read_statx_request(
    pid: Pid,
    entry: SyscallEntry,
) -> Result<Option<(CapturedStatxRequest, u64)>> {
    if entry.number != libc::SYS_statx as u64 {
        return Ok(None);
    }
    let directory_descriptor = entry.arguments[0] as i32;
    let path_address = entry.arguments[1];
    let flags = entry.arguments[2] as u32;
    let path_is_null = path_address == 0;
    let (path, path_terminated) = if path_is_null {
        (Vec::new(), false)
    } else {
        read_tracee_bounded_c_string(pid, path_address, LINUX_PATH_MAX)
            .context("read requested statx path")?
    };
    let meaningful_relative =
        !path_is_null && path_terminated && !path.is_empty() && path.first() != Some(&b'/');
    let cwd_empty_path = flags & crate::LINUX_AT_EMPTY_PATH != 0
        && directory_descriptor == libc::AT_FDCWD
        && !path_is_null
        && path_terminated
        && path.is_empty();
    let base = if meaningful_relative || cwd_empty_path {
        let proc_link = if directory_descriptor == libc::AT_FDCWD {
            "cwd".to_string()
        } else {
            format!("fd/{directory_descriptor}")
        };
        match read_process_link_with_limit(
            pid,
            &proc_link,
            "resolve requested statx directory",
            MAX_IDENTITY_BYTES,
        ) {
            Ok(target) => Some(target),
            Err(error)
                if directory_descriptor != libc::AT_FDCWD
                    && error
                        .downcast_ref::<std::io::Error>()
                        .and_then(std::io::Error::raw_os_error)
                        == Some(libc::ENOENT) =>
            {
                None
            }
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    let descriptor_form = flags & crate::LINUX_AT_EMPTY_PATH != 0
        && directory_descriptor >= 0
        && (path_is_null || (path_terminated && path.is_empty()));
    let descriptor_identity = if descriptor_form {
        read_descriptor_identity(pid, directory_descriptor)?
    } else {
        None
    };
    if descriptor_identity.as_ref().is_some_and(|identity| {
        identity.target.len() > MAX_IDENTITY_BYTES || identity.target.contains(&0)
    }) {
        anyhow::bail!("statx descriptor identity exceeds its byte budget");
    }
    let descriptor_mount_id = descriptor_identity
        .as_ref()
        .map(|_| read_descriptor_mount_id(pid, directory_descriptor))
        .transpose()?;
    Ok(Some((
        CapturedStatxRequest {
            directory_descriptor,
            path,
            path_is_null,
            path_terminated,
            base,
            descriptor_identity,
            descriptor_mount_id,
            flags,
            mask: entry.arguments[3] as u32,
            output_is_null: entry.arguments[4] == 0,
        },
        entry.arguments[4],
    )))
}

fn read_xattr_request(
    pid: Pid,
    entry: SyscallEntry,
) -> Result<Option<(CapturedXattrRequest, u64)>> {
    let Some(system_call) = xattr_system_call(entry.number) else {
        return Ok(None);
    };
    let name_address = entry.arguments[1];
    let value_address = entry.arguments[2];
    let (path, path_is_null, path_terminated, base, descriptor, descriptor_identity, descriptor_mount_id) =
        if matches!(
            system_call,
            CapturedXattrSystemCall::Fgetxattr | CapturedXattrSystemCall::Fsetxattr
        ) {
            let descriptor = entry.arguments[0] as i32;
            let identity = read_descriptor_identity(pid, descriptor)?;
            let mount_id = identity
                .as_ref()
                .map(|_| read_descriptor_mount_id(pid, descriptor))
                .transpose()?;
            (
                Vec::new(),
                true,
                false,
                None,
                Some(descriptor),
                identity,
                mount_id,
            )
        } else {
            let path_address = entry.arguments[0];
            let path_is_null = path_address == 0;
            let (path, path_terminated) = if path_is_null {
                (Vec::new(), false)
            } else {
                read_tracee_bounded_c_string(pid, path_address, LINUX_PATH_MAX)
                    .context("read requested xattr path")?
            };
            let meaningful_relative = !path_is_null
                && path_terminated
                && !path.is_empty()
                && path.first() != Some(&b'/');
            let base = if meaningful_relative {
                Some(read_process_link_with_limit(
                    pid,
                    "cwd",
                    "resolve requested xattr cwd",
                    MAX_IDENTITY_BYTES,
                )?)
            } else {
                None
            };
            (
                path,
                path_is_null,
                path_terminated,
                base,
                None,
                None,
                None,
            )
        };
    let name_is_null = name_address == 0;
    let (name, name_terminated) = if name_is_null {
        (Vec::new(), false)
    } else {
        read_tracee_bounded_c_string(pid, name_address, crate::LINUX_XATTR_NAME_MAX + 1)
            .context("read requested xattr name")?
    };
    let set_value = if system_call == CapturedXattrSystemCall::Fsetxattr {
        let requested_bytes = entry.arguments[3];
        if requested_bytes > MAX_XATTR_RESULT_BYTES {
            anyhow::bail!("xattr set value exceeds the capture byte limit");
        }
        if value_address == 0 {
            (requested_bytes == 0).then(Vec::new)
        } else {
            Some(copy_tracee_bytes(
                pid,
                value_address,
                usize::try_from(requested_bytes).context("xattr set size exceeds usize")?,
                "copy supplied xattr value",
            )?)
        }
    } else {
        None
    };
    Ok(Some((
        CapturedXattrRequest {
            system_call,
            path,
            path_is_null,
            path_terminated,
            base,
            descriptor,
            descriptor_identity,
            descriptor_mount_id,
            name,
            name_is_null,
            name_terminated,
            requested_bytes: entry.arguments[3],
            value_is_null: value_address == 0,
            set_value,
            flags: if system_call == CapturedXattrSystemCall::Fsetxattr {
                entry.arguments[4] as u32
            } else {
                0
            },
        },
        value_address,
    )))
}

fn read_readlink_request(
    pid: Pid,
    entry: SyscallEntry,
) -> Result<Option<(CapturedReadlinkRequest, u64)>> {
    let Some(system_call) = readlink_system_call(entry.number) else {
        return Ok(None);
    };
    let (directory_descriptor, path_address, buffer_address, requested_bytes) = match system_call {
        CapturedReadlinkSystemCall::Readlink => (
            libc::AT_FDCWD,
            entry.arguments[0],
            entry.arguments[1],
            entry.arguments[2],
        ),
        CapturedReadlinkSystemCall::ReadlinkAt => (
            entry.arguments[0] as i32,
            entry.arguments[1],
            entry.arguments[2],
            entry.arguments[3],
        ),
    };
    let path_is_null = path_address == 0;
    let path = if path_is_null {
        Vec::new()
    } else {
        let mut budget = LINUX_PATH_MAX;
        read_tracee_c_string(pid, path_address, &mut budget)
            .context("read requested readlink path")?
    };
    let base = if path.first() == Some(&b'/') {
        None
    } else {
        let proc_path = if directory_descriptor == libc::AT_FDCWD {
            PathBuf::from(format!("/proc/{pid}/cwd"))
        } else {
            PathBuf::from(format!("/proc/{pid}/fd/{directory_descriptor}"))
        };
        match std::fs::read_link(proc_path) {
            Ok(target) => Some(target.as_os_str().as_bytes().to_vec()),
            Err(error)
                if directory_descriptor != libc::AT_FDCWD
                    && error.raw_os_error() == Some(libc::ENOENT) =>
            {
                None
            }
            Err(error) => return Err(error).context("resolve requested readlink directory"),
        }
    };
    Ok(Some((
        CapturedReadlinkRequest {
            system_call,
            directory_descriptor,
            path,
            path_is_null,
            base,
            requested_bytes,
        },
        buffer_address,
    )))
}

fn read_access_request(pid: Pid, entry: SyscallEntry) -> Result<Option<CapturedAccessRequest>> {
    let Some(system_call) = access_system_call(entry.number) else {
        return Ok(None);
    };
    let (directory_descriptor, path_address, mode, flags) = match system_call {
        CapturedAccessSystemCall::Access => (
            libc::AT_FDCWD,
            entry.arguments[0],
            entry.arguments[1] as u32,
            0,
        ),
        CapturedAccessSystemCall::Faccessat => (
            entry.arguments[0] as i32,
            entry.arguments[1],
            entry.arguments[2] as u32,
            0,
        ),
        CapturedAccessSystemCall::Faccessat2 => (
            entry.arguments[0] as i32,
            entry.arguments[1],
            entry.arguments[2] as u32,
            entry.arguments[3] as u32,
        ),
    };
    let path_is_null = path_address == 0;
    let path = if path_is_null {
        Vec::new()
    } else {
        let mut budget = LINUX_PATH_MAX;
        read_tracee_c_string(pid, path_address, &mut budget)
            .context("read requested access path")?
    };
    let base = if path.first() == Some(&b'/') {
        None
    } else {
        let proc_path = if directory_descriptor == libc::AT_FDCWD {
            PathBuf::from(format!("/proc/{pid}/cwd"))
        } else {
            PathBuf::from(format!("/proc/{pid}/fd/{directory_descriptor}"))
        };
        match std::fs::read_link(proc_path) {
            Ok(target) => Some(target.as_os_str().as_bytes().to_vec()),
            Err(error)
                if directory_descriptor != libc::AT_FDCWD
                    && error.raw_os_error() == Some(libc::ENOENT) =>
            {
                None
            }
            Err(error) => return Err(error).context("resolve requested access directory"),
        }
    };
    Ok(Some(CapturedAccessRequest {
        system_call,
        directory_descriptor,
        path,
        path_is_null,
        base,
        mode,
        flags,
    }))
}

fn cwd_change_system_call(number: u64) -> Option<CapturedCwdChangeSystemCall> {
    if number == libc::SYS_chdir as u64 {
        return Some(CapturedCwdChangeSystemCall::Chdir);
    }
    if number == libc::SYS_fchdir as u64 {
        return Some(CapturedCwdChangeSystemCall::Fchdir);
    }
    None
}

fn read_cwd_change_request(
    pid: Pid,
    entry: SyscallEntry,
) -> Result<Option<CapturedCwdChangeRequest>> {
    let Some(system_call) = cwd_change_system_call(entry.number) else {
        return Ok(None);
    };
    let prior_working_directory = read_process_link(pid, "cwd", "read prior working directory")?;
    let (path, path_is_null, directory_descriptor, descriptor_target) = match system_call {
        CapturedCwdChangeSystemCall::Chdir => {
            let path_address = entry.arguments[0];
            let path_is_null = path_address == 0;
            let path = if path_is_null {
                Vec::new()
            } else {
                let mut budget = LINUX_PATH_MAX;
                read_tracee_c_string(pid, path_address, &mut budget)
                    .context("read requested chdir path")?
            };
            (path, path_is_null, libc::AT_FDCWD, None)
        }
        CapturedCwdChangeSystemCall::Fchdir => {
            let descriptor = entry.arguments[0] as i32;
            let proc_link = format!("fd/{descriptor}");
            let descriptor_target = match read_process_link(pid, &proc_link, "read fchdir target") {
                Ok(target) => Some(target),
                Err(error)
                    if error
                        .downcast_ref::<std::io::Error>()
                        .and_then(std::io::Error::raw_os_error)
                        == Some(libc::ENOENT) =>
                {
                    None
                }
                Err(error) => return Err(error),
            };
            (Vec::new(), false, descriptor, descriptor_target)
        }
    };
    Ok(Some(CapturedCwdChangeRequest {
        system_call,
        path,
        path_is_null,
        directory_descriptor,
        descriptor_target,
        prior_working_directory,
    }))
}

fn path_removal_system_call(number: u64) -> Option<CapturedPathRemovalSystemCall> {
    #[cfg(target_arch = "x86_64")]
    if number == libc::SYS_unlink as u64 {
        return Some(CapturedPathRemovalSystemCall::Unlink);
    }
    if number == libc::SYS_unlinkat as u64 {
        return Some(CapturedPathRemovalSystemCall::UnlinkAt);
    }
    None
}

fn read_path_removal_request(
    pid: Pid,
    entry: SyscallEntry,
) -> Result<Option<CapturedPathRemovalRequest>> {
    let Some(system_call) = path_removal_system_call(entry.number) else {
        return Ok(None);
    };
    let (directory_descriptor, path_address, flags) = match system_call {
        CapturedPathRemovalSystemCall::Unlink => (libc::AT_FDCWD, entry.arguments[0], 0),
        CapturedPathRemovalSystemCall::UnlinkAt => (
            entry.arguments[0] as i32,
            entry.arguments[1],
            entry.arguments[2] as u32,
        ),
    };
    let path_is_null = path_address == 0;
    let path = if path_is_null {
        Vec::new()
    } else {
        let mut budget = LINUX_PATH_MAX;
        read_tracee_c_string(pid, path_address, &mut budget)
            .context("read requested path-removal path")?
    };
    let base = if path.first() == Some(&b'/') {
        None
    } else {
        let proc_link = if directory_descriptor == libc::AT_FDCWD {
            "cwd".to_string()
        } else {
            format!("fd/{directory_descriptor}")
        };
        match read_process_link(pid, &proc_link, "resolve requested path-removal directory") {
            Ok(target) => Some(target),
            Err(error)
                if directory_descriptor != libc::AT_FDCWD
                    && error
                        .downcast_ref::<std::io::Error>()
                        .and_then(std::io::Error::raw_os_error)
                        == Some(libc::ENOENT) =>
            {
                None
            }
            Err(error) => return Err(error),
        }
    };
    Ok(Some(CapturedPathRemovalRequest {
        system_call,
        directory_descriptor,
        path,
        path_is_null,
        base,
        flags,
    }))
}

fn directory_creation_system_call(number: u64) -> Option<CapturedDirectoryCreationSystemCall> {
    #[cfg(target_arch = "x86_64")]
    if number == libc::SYS_mkdir as u64 {
        return Some(CapturedDirectoryCreationSystemCall::Mkdir);
    }
    if number == libc::SYS_mkdirat as u64 {
        return Some(CapturedDirectoryCreationSystemCall::MkdirAt);
    }
    None
}

fn read_directory_creation_request(
    pid: Pid,
    entry: SyscallEntry,
) -> Result<Option<CapturedDirectoryCreationRequest>> {
    let Some(system_call) = directory_creation_system_call(entry.number) else {
        return Ok(None);
    };
    let (directory_descriptor, path_address, mode) = match system_call {
        CapturedDirectoryCreationSystemCall::Mkdir => (
            libc::AT_FDCWD,
            entry.arguments[0],
            entry.arguments[1] as u32,
        ),
        CapturedDirectoryCreationSystemCall::MkdirAt => (
            entry.arguments[0] as i32,
            entry.arguments[1],
            entry.arguments[2] as u32,
        ),
    };
    let path_is_null = path_address == 0;
    let path = if path_is_null {
        Vec::new()
    } else {
        let mut budget = LINUX_PATH_MAX;
        read_tracee_c_string(pid, path_address, &mut budget)
            .context("read requested directory-creation path")?
    };
    let base = if path.first() == Some(&b'/') {
        None
    } else {
        let proc_link = if directory_descriptor == libc::AT_FDCWD {
            "cwd".to_string()
        } else {
            format!("fd/{directory_descriptor}")
        };
        match read_process_link(
            pid,
            &proc_link,
            "resolve requested directory-creation directory",
        ) {
            Ok(target) => Some(target),
            Err(error)
                if directory_descriptor != libc::AT_FDCWD
                    && error
                        .downcast_ref::<std::io::Error>()
                        .and_then(std::io::Error::raw_os_error)
                        == Some(libc::ENOENT) =>
            {
                None
            }
            Err(error) => return Err(error),
        }
    };
    Ok(Some(CapturedDirectoryCreationRequest {
        system_call,
        directory_descriptor,
        path,
        path_is_null,
        base,
        mode,
    }))
}

fn path_rename_system_call(number: u64) -> Option<CapturedPathRenameSystemCall> {
    #[cfg(target_arch = "x86_64")]
    if number == libc::SYS_rename as u64 {
        return Some(CapturedPathRenameSystemCall::Rename);
    }
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    if number == libc::SYS_renameat as u64 {
        return Some(CapturedPathRenameSystemCall::RenameAt);
    }
    if number == libc::SYS_renameat2 as u64 {
        return Some(CapturedPathRenameSystemCall::RenameAt2);
    }
    None
}

fn read_path_rename_operand(
    pid: Pid,
    directory_descriptor: i32,
    path_address: u64,
    description: &str,
) -> Result<PathRenameOperand> {
    let path_is_null = path_address == 0;
    let (path, path_terminated) = if path_is_null {
        (Vec::new(), false)
    } else {
        read_tracee_bounded_c_string(pid, path_address, LINUX_PATH_MAX)
            .with_context(|| format!("read requested path-rename {description}"))?
    };
    let base =
        if !path_is_null && path_terminated && !path.is_empty() && path.first() != Some(&b'/') {
            let proc_link = if directory_descriptor == libc::AT_FDCWD {
                "cwd".to_string()
            } else {
                format!("fd/{directory_descriptor}")
            };
            match read_process_link_with_limit(
                pid,
                &proc_link,
                &format!("resolve requested path-rename {description} directory"),
                MAX_IDENTITY_BYTES,
            ) {
                Ok(target) => Some(target),
                Err(error)
                    if directory_descriptor != libc::AT_FDCWD
                        && error
                            .downcast_ref::<std::io::Error>()
                            .and_then(std::io::Error::raw_os_error)
                            == Some(libc::ENOENT) =>
                {
                    None
                }
                Err(error) => return Err(error),
            }
        } else {
            None
        };
    Ok(PathRenameOperand {
        path,
        path_is_null,
        path_terminated,
        base,
    })
}

fn read_path_rename_request(
    pid: Pid,
    entry: SyscallEntry,
) -> Result<Option<CapturedPathRenameRequest>> {
    let Some(system_call) = path_rename_system_call(entry.number) else {
        return Ok(None);
    };
    let (
        old_directory_descriptor,
        old_path_address,
        new_directory_descriptor,
        new_path_address,
        flags,
    ) = match system_call {
        CapturedPathRenameSystemCall::Rename => (
            libc::AT_FDCWD,
            entry.arguments[0],
            libc::AT_FDCWD,
            entry.arguments[1],
            0,
        ),
        CapturedPathRenameSystemCall::RenameAt => (
            entry.arguments[0] as i32,
            entry.arguments[1],
            entry.arguments[2] as i32,
            entry.arguments[3],
            0,
        ),
        CapturedPathRenameSystemCall::RenameAt2 => (
            entry.arguments[0] as i32,
            entry.arguments[1],
            entry.arguments[2] as i32,
            entry.arguments[3],
            entry.arguments[4] as u32,
        ),
    };
    let old = read_path_rename_operand(pid, old_directory_descriptor, old_path_address, "source")?;
    let new = read_path_rename_operand(
        pid,
        new_directory_descriptor,
        new_path_address,
        "destination",
    )?;
    Ok(Some(CapturedPathRenameRequest {
        system_call,
        old_directory_descriptor,
        old_path: old.path,
        old_path_is_null: old.path_is_null,
        old_path_terminated: old.path_terminated,
        old_base: old.base,
        new_directory_descriptor,
        new_path: new.path,
        new_path_is_null: new.path_is_null,
        new_path_terminated: new.path_terminated,
        new_base: new.base,
        flags,
    }))
}

fn path_mode_change_system_call(number: u64) -> Option<CapturedPathModeChangeSystemCall> {
    if number == libc::SYS_fchmod as u64 {
        return Some(CapturedPathModeChangeSystemCall::Fchmod);
    }
    #[cfg(target_arch = "x86_64")]
    if number == libc::SYS_chmod as u64 {
        return Some(CapturedPathModeChangeSystemCall::Chmod);
    }
    if number == libc::SYS_fchmodat as u64 {
        return Some(CapturedPathModeChangeSystemCall::FchmodAt);
    }
    None
}

fn read_path_mode_change_request(
    pid: Pid,
    entry: SyscallEntry,
) -> Result<Option<CapturedPathModeChangeRequest>> {
    let Some(system_call) = path_mode_change_system_call(entry.number) else {
        return Ok(None);
    };
    if system_call == CapturedPathModeChangeSystemCall::Fchmod {
        let Some(descriptor) = signed_i32_syscall_argument(entry.arguments[0]) else {
            return Ok(None);
        };
        let descriptor_identity = read_descriptor_identity(pid, descriptor)?;
        let descriptor_mount_id = descriptor_identity
            .as_ref()
            .map(|_| read_descriptor_mount_id(pid, descriptor))
            .transpose()?;
        return Ok(Some(CapturedPathModeChangeRequest {
            system_call,
            descriptor: Some(descriptor),
            descriptor_identity,
            descriptor_mount_id,
            directory_descriptor: libc::AT_FDCWD,
            path: Vec::new(),
            path_is_null: true,
            path_terminated: false,
            base: None,
            mode: entry.arguments[1] as u32,
        }));
    }
    let (directory_descriptor, path_address, mode) = match system_call {
        CapturedPathModeChangeSystemCall::Chmod => (
            libc::AT_FDCWD,
            entry.arguments[0],
            entry.arguments[1] as u32,
        ),
        CapturedPathModeChangeSystemCall::FchmodAt => (
            entry.arguments[0] as i32,
            entry.arguments[1],
            entry.arguments[2] as u32,
        ),
        CapturedPathModeChangeSystemCall::Fchmod => unreachable!(),
    };
    let path_is_null = path_address == 0;
    let (path, path_terminated) = if path_is_null {
        (Vec::new(), false)
    } else {
        read_tracee_bounded_c_string(pid, path_address, LINUX_PATH_MAX)
            .context("read requested path-mode-change path")?
    };
    let base =
        if !path_is_null && path_terminated && !path.is_empty() && path.first() != Some(&b'/') {
            let proc_link = if directory_descriptor == libc::AT_FDCWD {
                "cwd".to_string()
            } else {
                format!("fd/{directory_descriptor}")
            };
            match read_process_link_with_limit(
                pid,
                &proc_link,
                "resolve requested path-mode-change directory",
                MAX_IDENTITY_BYTES,
            ) {
                Ok(target) => Some(target),
                Err(error)
                    if directory_descriptor != libc::AT_FDCWD
                        && error
                            .downcast_ref::<std::io::Error>()
                            .and_then(std::io::Error::raw_os_error)
                            == Some(libc::ENOENT) =>
                {
                    None
                }
                Err(error) => return Err(error),
            }
        } else {
            None
        };
    Ok(Some(CapturedPathModeChangeRequest {
        system_call,
        descriptor: None,
        descriptor_identity: None,
        descriptor_mount_id: None,
        directory_descriptor,
        path,
        path_is_null,
        path_terminated,
        base,
        mode,
    }))
}

fn read_descriptor_timestamp_change_request(
    pid: Pid,
    entry: SyscallEntry,
) -> Result<Option<CapturedDescriptorTimestampChangeRequest>> {
    if entry.number != libc::SYS_utimensat as u64 {
        return Ok(None);
    }
    if entry.arguments[1] != 0 {
        anyhow::bail!("pathname-form utimensat is not modeled by process capture");
    }
    let descriptor = entry.arguments[0] as i32;
    let times_address = entry.arguments[2];
    let times_is_null = times_address == 0;
    let times = if times_is_null {
        None
    } else {
        require_native_kernel_timestamp_layout()?;
        let bytes = copy_tracee_bytes(
            pid,
            times_address,
            NATIVE_KERNEL_TIMESTAMP_PAIR_BYTES,
            "utimensat timestamps",
        )?;
        Some([
            CapturedTimestamp {
                seconds: i64::from_ne_bytes(bytes[0..8].try_into().unwrap()),
                nanoseconds: i64::from_ne_bytes(bytes[8..16].try_into().unwrap()),
            },
            CapturedTimestamp {
                seconds: i64::from_ne_bytes(bytes[16..24].try_into().unwrap()),
                nanoseconds: i64::from_ne_bytes(bytes[24..32].try_into().unwrap()),
            },
        ])
    };
    let identity = read_descriptor_identity(pid, descriptor)?;
    let mount_id = identity
        .as_ref()
        .map(|_| read_descriptor_mount_id(pid, descriptor))
        .transpose()?;
    Ok(Some(CapturedDescriptorTimestampChangeRequest {
        descriptor,
        identity,
        mount_id,
        times_is_null,
        times,
        flags: entry.arguments[3] as u32,
    }))
}

fn require_native_kernel_timestamp_layout() -> Result<()> {
    if std::mem::size_of::<NativeKernelTimestamp>() != 16
        || std::mem::align_of::<NativeKernelTimestamp>() != 8
        || std::mem::offset_of!(NativeKernelTimestamp, seconds) != 0
        || std::mem::offset_of!(NativeKernelTimestamp, nanoseconds) != 8
    {
        anyhow::bail!("unsupported native kernel timestamp layout");
    }
    Ok(())
}

fn symbolic_link_creation_system_call(
    number: u64,
) -> Option<CapturedSymbolicLinkCreationSystemCall> {
    #[cfg(target_arch = "x86_64")]
    if number == libc::SYS_symlink as u64 {
        return Some(CapturedSymbolicLinkCreationSystemCall::Symlink);
    }
    if number == libc::SYS_symlinkat as u64 {
        return Some(CapturedSymbolicLinkCreationSystemCall::SymlinkAt);
    }
    None
}

fn read_symbolic_link_creation_request(
    pid: Pid,
    entry: SyscallEntry,
) -> Result<Option<CapturedSymbolicLinkCreationRequest>> {
    let Some(system_call) = symbolic_link_creation_system_call(entry.number) else {
        return Ok(None);
    };
    let (target_address, directory_descriptor, link_path_address) = match system_call {
        CapturedSymbolicLinkCreationSystemCall::Symlink => {
            (entry.arguments[0], libc::AT_FDCWD, entry.arguments[1])
        }
        CapturedSymbolicLinkCreationSystemCall::SymlinkAt => (
            entry.arguments[0],
            entry.arguments[1] as i32,
            entry.arguments[2],
        ),
    };
    let target_is_null = target_address == 0;
    let (target, target_terminated) = if target_is_null {
        (Vec::new(), false)
    } else {
        read_tracee_bounded_c_string(pid, target_address, LINUX_PATH_MAX)
            .context("read symbolic-link target")?
    };
    let link_path_is_null = link_path_address == 0;
    let (link_path, link_path_terminated) = if link_path_is_null {
        (Vec::new(), false)
    } else {
        read_tracee_bounded_c_string(pid, link_path_address, LINUX_PATH_MAX)
            .context("read symbolic-link name")?
    };
    let base = if !link_path_is_null
        && link_path_terminated
        && !link_path.is_empty()
        && link_path.first() != Some(&b'/')
    {
        let proc_link = if directory_descriptor == libc::AT_FDCWD {
            "cwd".to_string()
        } else {
            format!("fd/{directory_descriptor}")
        };
        match read_process_link_with_limit(
            pid,
            &proc_link,
            "resolve symbolic-link destination directory",
            MAX_IDENTITY_BYTES,
        ) {
            Ok(target) => Some(target),
            Err(error)
                if directory_descriptor != libc::AT_FDCWD
                    && error
                        .downcast_ref::<std::io::Error>()
                        .and_then(std::io::Error::raw_os_error)
                        == Some(libc::ENOENT) =>
            {
                None
            }
            Err(error) => return Err(error),
        }
    } else {
        None
    };
    Ok(Some(CapturedSymbolicLinkCreationRequest {
        system_call,
        target,
        target_is_null,
        target_terminated,
        directory_descriptor,
        link_path,
        link_path_is_null,
        link_path_terminated,
        base,
    }))
}

fn filesystem_status_system_call(number: u64) -> Option<CapturedFilesystemStatusSystemCall> {
    if number == libc::SYS_statfs as u64 {
        return Some(CapturedFilesystemStatusSystemCall::Statfs);
    }
    if number == libc::SYS_fstatfs as u64 {
        return Some(CapturedFilesystemStatusSystemCall::Fstatfs);
    }
    None
}

fn read_filesystem_status_request(
    pid: Pid,
    entry: SyscallEntry,
) -> Result<Option<(CapturedFilesystemStatusRequest, u64)>> {
    let Some(system_call) = filesystem_status_system_call(entry.number) else {
        return Ok(None);
    };
    let buffer_address = entry.arguments[1];
    let request = match system_call {
        CapturedFilesystemStatusSystemCall::Statfs => {
            let path_address = entry.arguments[0];
            let path_is_null = path_address == 0;
            let (path, path_terminated) = if path_is_null {
                (Vec::new(), false)
            } else {
                read_tracee_bounded_c_string(pid, path_address, LINUX_PATH_MAX)
                    .context("read requested statfs path")?
            };
            let base = if !path_is_null
                && path_terminated
                && !path.is_empty()
                && path.first() != Some(&b'/')
            {
                Some(read_process_link_with_limit(
                    pid,
                    "cwd",
                    "resolve requested statfs working directory",
                    MAX_IDENTITY_BYTES,
                )?)
            } else {
                None
            };
            CapturedFilesystemStatusRequest {
                system_call,
                descriptor: libc::AT_FDCWD,
                path,
                path_is_null,
                path_terminated,
                output_is_null: buffer_address == 0,
                base,
                descriptor_identity: None,
                descriptor_mount_id: None,
            }
        }
        CapturedFilesystemStatusSystemCall::Fstatfs => {
            let descriptor = entry.arguments[0] as i32;
            let descriptor_identity = read_descriptor_identity(pid, descriptor)?;
            if descriptor_identity.as_ref().is_some_and(|identity| {
                identity.target.len() > MAX_IDENTITY_BYTES || identity.target.contains(&0)
            }) {
                anyhow::bail!("fstatfs descriptor identity exceeds its byte budget");
            };
            let descriptor_mount_id = descriptor_identity
                .as_ref()
                .map(|_| read_descriptor_mount_id(pid, descriptor))
                .transpose()?;
            CapturedFilesystemStatusRequest {
                system_call,
                descriptor,
                path: Vec::new(),
                path_is_null: false,
                path_terminated: false,
                output_is_null: buffer_address == 0,
                base: None,
                descriptor_identity,
                descriptor_mount_id,
            }
        }
    };
    Ok(Some((request, buffer_address)))
}

fn read_process_link(pid: Pid, link: &str, description: &str) -> Result<Vec<u8>> {
    read_process_link_with_limit(pid, link, description, LINUX_PATH_MAX - 1)
}

fn read_process_link_with_limit(
    pid: Pid,
    link: &str,
    description: &str,
    maximum_bytes: usize,
) -> Result<Vec<u8>> {
    let target = std::fs::read_link(PathBuf::from(format!("/proc/{pid}/{link}")))
        .with_context(|| description.to_string())?;
    let bytes = target.as_os_str().as_bytes().to_vec();
    if bytes.is_empty() || bytes.len() > maximum_bytes || bytes.contains(&0) {
        anyhow::bail!("procfs returned an invalid process link target");
    }
    Ok(bytes)
}

fn read_read_request(pid: Pid, entry: SyscallEntry) -> Result<Option<ReadEntryState>> {
    let Some(system_call) = read_system_call(entry.number) else {
        return Ok(None);
    };
    let descriptor = entry.arguments[0] as i32;
    let identity = read_descriptor_identity(pid, descriptor)?;
    let mount_id = identity
        .as_ref()
        .map(|_| read_descriptor_mount_id(pid, descriptor))
        .transpose()?;
    let buffers = match system_call {
        CapturedReadSystemCall::Read | CapturedReadSystemCall::Pread64 => vec![ReadBuffer {
            address: entry.arguments[1],
            length: entry.arguments[2],
        }],
        CapturedReadSystemCall::Readv
        | CapturedReadSystemCall::Preadv
        | CapturedReadSystemCall::Preadv2 => {
            read_tracee_iovecs(pid, entry.arguments[1], entry.arguments[2])?
        }
    };
    let requested_bytes = buffers
        .iter()
        .try_fold(0u64, |total, buffer| total.checked_add(buffer.length));
    let requested_bytes = requested_bytes.context("read request byte count overflowed")?;
    let uses_descriptor_offset = match system_call {
        CapturedReadSystemCall::Read | CapturedReadSystemCall::Readv => true,
        CapturedReadSystemCall::Pread64 | CapturedReadSystemCall::Preadv => false,
        CapturedReadSystemCall::Preadv2 => native_vectored_offset(entry) == u64::MAX,
    };
    let offset = if uses_descriptor_offset {
        match &identity {
            Some(identity) if identity.kind == CapturedDescriptorKind::RegularFile => {
                Some(read_descriptor_offset(pid, descriptor)?)
            }
            _ => None,
        }
    } else {
        Some(match system_call {
            CapturedReadSystemCall::Pread64 => entry.arguments[3],
            CapturedReadSystemCall::Preadv | CapturedReadSystemCall::Preadv2 => {
                native_vectored_offset(entry)
            }
            CapturedReadSystemCall::Read | CapturedReadSystemCall::Readv => {
                anyhow::bail!("descriptor-position read lost its semantic marker")
            }
        })
    };
    let flags = if system_call == CapturedReadSystemCall::Preadv2 {
        entry.arguments[5] as u32
    } else {
        0
    };
    let backing_file = identity
        .as_ref()
        .filter(|identity| {
            identity.kind == CapturedDescriptorKind::RegularFile
                && regular_file_value_target(&identity.target)
        })
        .map(|_| open_tracee_descriptor_for_witness(pid, descriptor))
        .transpose()?
        .flatten();
    Ok(Some((
        CapturedReadRequest {
            system_call,
            descriptor,
            identity,
            mount_id,
            uses_descriptor_offset,
            offset,
            buffer_lengths: buffers.iter().map(|buffer| buffer.length).collect(),
            requested_bytes,
            flags,
        },
        buffers,
        backing_file,
    )))
}

fn read_getdents64_request(pid: Pid, entry: SyscallEntry) -> Result<Option<Getdents64EntryState>> {
    if entry.number != libc::SYS_getdents64 as u64 {
        return Ok(None);
    }
    let descriptor = entry.arguments[0] as i32;
    let identity = read_descriptor_identity(pid, descriptor)?;
    let mount_id = identity
        .as_ref()
        .map(|_| read_descriptor_mount_id(pid, descriptor))
        .transpose()?;
    let position_before = match &identity {
        Some(identity) if identity.kind == CapturedDescriptorKind::Directory => {
            Some(read_descriptor_position(pid, descriptor)?)
        }
        _ => None,
    };
    let backing_directory = identity
        .as_ref()
        .filter(|identity| identity.kind == CapturedDescriptorKind::Directory)
        .map(|identity| {
            let descriptor_path = format!("/proc/{pid}/fd/{descriptor}");
            let mut directory = File::open(&descriptor_path)
                .with_context(|| format!("reopen traced directory {descriptor_path}"))?;
            let metadata = directory
                .metadata()
                .with_context(|| format!("stat traced directory {descriptor_path}"))?;
            let key = directory_witness_key(&metadata);
            if !(metadata.file_type().is_dir()
                && key.device == identity.device
                && key.inode == identity.inode
                && key.inode != 0)
            {
                anyhow::bail!("reopened traced directory changed identity");
            }
            let entry_value = witness_complete_directory(&mut directory, key)
                .context("witness traced directory at getdents64 entry")?;
            Ok(PendingDirectoryWitness {
                directory,
                key,
                entry_value,
            })
        });
    Ok(Some((
        CapturedGetdents64Request {
            descriptor,
            identity,
            mount_id,
            position_before,
            requested_bytes: entry.arguments[2] as u32,
        },
        entry.arguments[1],
        backing_directory,
    )))
}

fn read_mmap_request(
    pid: Pid,
    entry: SyscallEntry,
) -> Result<Option<(CapturedMmapRequest, Option<File>)>> {
    if entry.number != libc::SYS_mmap as u64 {
        return Ok(None);
    }
    let flags = entry.arguments[3];
    let descriptor = entry.arguments[4] as i32;
    let identity = if flags & LINUX_MAP_ANONYMOUS != 0 {
        None
    } else {
        read_descriptor_identity(pid, descriptor)?
    };
    let mount_id = identity
        .as_ref()
        .map(|_| read_descriptor_mount_id(pid, descriptor))
        .transpose()?;
    let backing_file = identity
        .as_ref()
        .filter(|identity| identity.kind == CapturedDescriptorKind::RegularFile)
        .and_then(|_| File::open(format!("/proc/{pid}/fd/{descriptor}")).ok());
    Ok(Some((
        CapturedMmapRequest {
            address: entry.arguments[0],
            length: entry.arguments[1],
            protection: entry.arguments[2],
            flags,
            descriptor,
            identity,
            mount_id,
            offset: entry.arguments[5],
        },
        backing_file,
    )))
}

fn read_write_request(
    pid: Pid,
    entry: SyscallEntry,
) -> Result<Option<(CapturedWriteRequest, ReadBuffer)>> {
    let Some(system_call) = write_system_call(entry.number) else {
        return Ok(None);
    };
    let descriptor = entry.arguments[0] as i32;
    let identity = read_descriptor_identity(pid, descriptor)?;
    let mount_id = identity
        .as_ref()
        .map(|_| read_descriptor_mount_id(pid, descriptor))
        .transpose()?;
    let uses_descriptor_offset = system_call == CapturedWriteSystemCall::Write;
    let offset = if uses_descriptor_offset {
        match &identity {
            Some(identity) if identity.kind == CapturedDescriptorKind::RegularFile => {
                Some(read_descriptor_offset(pid, descriptor)?)
            }
            _ => None,
        }
    } else {
        Some(entry.arguments[3])
    };
    let append = if identity.is_some() {
        read_descriptor_status_flags(pid, descriptor)? & libc::O_APPEND as u64 != 0
    } else {
        false
    };
    let buffer = ReadBuffer {
        address: entry.arguments[1],
        length: entry.arguments[2],
    };
    Ok(Some((
        CapturedWriteRequest {
            system_call,
            descriptor,
            identity,
            mount_id,
            uses_descriptor_offset,
            offset,
            requested_bytes: buffer.length,
            append,
        },
        buffer,
    )))
}

fn write_system_call(number: u64) -> Option<CapturedWriteSystemCall> {
    if number == libc::SYS_write as u64 {
        return Some(CapturedWriteSystemCall::Write);
    }
    if number == libc::SYS_pwrite64 as u64 {
        return Some(CapturedWriteSystemCall::Pwrite64);
    }
    None
}

fn parse_linux_dirent64(bytes: &[u8]) -> Result<Vec<CapturedDirectoryEntry>> {
    let mut entries = Vec::new();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        let remaining = bytes.len() - cursor;
        if remaining < LINUX_DIRENT64_HEADER_SIZE {
            anyhow::bail!("getdents64 result has a truncated record header");
        }
        let inode = u64::from_ne_bytes(
            bytes[cursor..cursor + 8]
                .try_into()
                .context("decode directory inode")?,
        );
        let next_position = i64::from_ne_bytes(
            bytes[cursor + 8..cursor + 16]
                .try_into()
                .context("decode directory position")?,
        );
        let record_length = u16::from_ne_bytes(
            bytes[cursor + 16..cursor + 18]
                .try_into()
                .context("decode directory record length")?,
        );
        let record_length_usize = usize::from(record_length);
        if record_length_usize < LINUX_DIRENT64_MINIMUM_RECORD_SIZE
            || record_length_usize % LINUX_DIRENT64_ALIGNMENT != 0
            || record_length_usize > remaining
        {
            anyhow::bail!("getdents64 result has an invalid record length");
        }
        let name_and_padding =
            &bytes[cursor + LINUX_DIRENT64_HEADER_SIZE..cursor + record_length_usize];
        let name_length = name_and_padding
            .iter()
            .position(|byte| *byte == 0)
            .context("getdents64 result has an unterminated entry name")?;
        let name = &name_and_padding[..name_length];
        if name.is_empty() || name.len() >= LINUX_PATH_MAX || name.contains(&b'/') {
            anyhow::bail!("getdents64 result has an invalid entry name");
        }
        entries.push(CapturedDirectoryEntry {
            inode,
            next_position,
            record_length,
            file_type: bytes[cursor + 18],
            name: name.to_vec(),
        });
        cursor = cursor
            .checked_add(record_length_usize)
            .ok_or_else(|| anyhow::anyhow!("getdents64 result position overflowed"))?;
    }
    Ok(entries)
}

fn native_vectored_offset(entry: SyscallEntry) -> u64 {
    // Every supported native ABI is LP64. Its preadv compatibility signature
    // carries the complete offset in pos_l and leaves pos_h unused.
    entry.arguments[3]
}

fn read_system_call(number: u64) -> Option<CapturedReadSystemCall> {
    if number == libc::SYS_read as u64 {
        return Some(CapturedReadSystemCall::Read);
    }
    if number == libc::SYS_pread64 as u64 {
        return Some(CapturedReadSystemCall::Pread64);
    }
    if number == libc::SYS_readv as u64 {
        return Some(CapturedReadSystemCall::Readv);
    }
    if number == libc::SYS_preadv as u64 {
        return Some(CapturedReadSystemCall::Preadv);
    }
    if number == libc::SYS_preadv2 as u64 {
        return Some(CapturedReadSystemCall::Preadv2);
    }
    None
}

fn read_tracee_iovecs(pid: Pid, address: u64, count: u64) -> Result<Vec<ReadBuffer>> {
    let count = usize::try_from(count).context("read vector count exceeds usize")?;
    if count > MAX_READ_VECTOR_ENTRIES {
        anyhow::bail!("read vector exceeds the kernel entry limit");
    }
    if count == 0 {
        return Ok(Vec::new());
    }
    if address == 0 {
        anyhow::bail!("read vector pointer is null");
    }
    if std::mem::size_of::<libc::iovec>() != 2 * std::mem::size_of::<usize>() {
        anyhow::bail!("process capture does not support this native iovec layout");
    }
    let stride = std::mem::size_of::<libc::iovec>();
    let length_offset = std::mem::size_of::<usize>();
    let byte_length = count
        .checked_mul(stride)
        .ok_or_else(|| anyhow::anyhow!("read vector byte count overflowed"))?;
    let bytes = read_tracee_bytes(pid, address, byte_length).context("read requested vector")?;
    let mut buffers = Vec::with_capacity(count);
    for vector in bytes.chunks_exact(stride) {
        buffers.push(ReadBuffer {
            address: usize::from_ne_bytes(
                vector[..length_offset]
                    .try_into()
                    .context("decode read vector address")?,
            ) as u64,
            length: usize::from_ne_bytes(
                vector[length_offset..]
                    .try_into()
                    .context("decode read vector length")?,
            ) as u64,
        });
    }
    Ok(buffers)
}

fn reject_unsupported_clone_sharing(pid: Pid, entry: SyscallEntry) -> Result<()> {
    let flags = if entry.number == libc::SYS_clone as u64 {
        Some(entry.arguments[0])
    } else if entry.number == libc::SYS_clone3 as u64 {
        if entry.arguments[1] < std::mem::size_of::<u64>() as u64 {
            anyhow::bail!("clone3 request has a truncated argument structure");
        }
        let bytes = read_tracee_bytes(pid, entry.arguments[0], std::mem::size_of::<u64>())
            .context("read clone3 flags")?;
        Some(u64::from_ne_bytes(
            bytes.as_slice().try_into().context("decode clone3 flags")?,
        ))
    } else {
        None
    };
    let Some(flags) = flags else {
        return Ok(());
    };
    if flags & libc::CLONE_THREAD as u64 != 0 {
        if supported_thread_clone_flags(flags) {
            return Ok(());
        }
        anyhow::bail!("unsupported multithreaded clone contract");
    }
    if flags & libc::CLONE_FS as u64 != 0 {
        anyhow::bail!("shared working-directory state is unsupported for exact capture");
    }
    // A vfork-style clone shares the address space only while the kernel keeps
    // the parent suspended. That serialization lets us read child buffers
    // exactly until the child execs or exits, without concurrent parent writes.
    // Keep rejecting every other shared-address-space clone.
    if flags & libc::CLONE_VM as u64 != 0 && flags & libc::CLONE_VFORK as u64 == 0 {
        anyhow::bail!("shared address spaces are unsupported for exact buffer capture");
    }
    if flags & libc::CLONE_FILES as u64 != 0 {
        anyhow::bail!("shared descriptor tables are unsupported for exact descriptor capture");
    }
    Ok(())
}

fn read_descriptor_identity(
    pid: Pid,
    descriptor: i32,
) -> Result<Option<CapturedDescriptorIdentity>> {
    let path = PathBuf::from(format!("/proc/{pid}/fd/{descriptor}"));
    let host_target = match std::fs::read_link(&path) {
        Ok(target) => target,
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
        Err(error) => return Err(error).context("resolve read descriptor target"),
    };
    let target = tracee_visible_absolute_target(pid, host_target)?;
    let metadata = std::fs::metadata(&path).context("stat read descriptor target")?;
    let file_type = metadata.mode() & libc::S_IFMT;
    let kind = if file_type == libc::S_IFREG {
        CapturedDescriptorKind::RegularFile
    } else if file_type == libc::S_IFDIR {
        CapturedDescriptorKind::Directory
    } else if file_type == libc::S_IFCHR {
        CapturedDescriptorKind::CharacterDevice
    } else if file_type == libc::S_IFBLK {
        CapturedDescriptorKind::BlockDevice
    } else if file_type == libc::S_IFIFO {
        CapturedDescriptorKind::Fifo
    } else if file_type == libc::S_IFSOCK {
        CapturedDescriptorKind::Socket
    } else {
        CapturedDescriptorKind::Other
    };
    Ok(Some(CapturedDescriptorIdentity {
        target: target.as_os_str().as_bytes().to_vec(),
        device: metadata.dev(),
        inode: metadata.ino(),
        kind,
    }))
}

fn tracee_visible_absolute_target(pid: Pid, target: PathBuf) -> Result<PathBuf> {
    if !target.is_absolute() {
        return Ok(target);
    }
    let tracee_root = cached_tracee_root(pid)?;
    if tracee_root == std::path::Path::new("/") {
        return Ok(target);
    }
    match target.strip_prefix(&tracee_root) {
        Ok(relative) => Ok(std::path::Path::new("/").join(relative)),
        Err(_) => Ok(target),
    }
}

fn cached_tracee_root(pid: Pid) -> Result<PathBuf> {
    TRACEE_ROOT_CACHE.with(|cache| {
        if let Some(root) = cache.borrow().get(&pid).cloned() {
            return Ok(root);
        }
        let root = std::fs::read_link(format!("/proc/{pid}/root"))
            .context("resolve observed process root")?;
        cache.borrow_mut().insert(pid, root.clone());
        Ok(root)
    })
}

fn read_descriptor_offset(pid: Pid, descriptor: i32) -> Result<u64> {
    u64::try_from(read_descriptor_position(pid, descriptor)?)
        .context("descriptor position is negative")
}

fn read_descriptor_position(pid: Pid, descriptor: i32) -> Result<i64> {
    let contents = std::fs::read_to_string(format!("/proc/{pid}/fdinfo/{descriptor}"))
        .context("read descriptor position")?;
    let position = contents
        .lines()
        .find_map(|line| line.strip_prefix("pos:\t"))
        .context("descriptor position record is missing")?;
    position.parse().context("descriptor position is invalid")
}

fn read_descriptor_mount_id(pid: Pid, descriptor: i32) -> Result<u64> {
    let path = PathBuf::from(format!("/proc/{pid}/fdinfo/{descriptor}"));
    read_descriptor_mount_id_file(&path, MAX_FDINFO_BYTES)
}

fn read_descriptor_mount_id_file(path: &std::path::Path, limit: usize) -> Result<u64> {
    let bytes = read_bounded_file(path, limit, "descriptor mount identity")?;
    let contents = std::str::from_utf8(&bytes).context("descriptor fdinfo is not UTF-8")?;
    let mount_id = contents
        .lines()
        .find_map(|line| line.strip_prefix("mnt_id:\t"))
        .context("descriptor mount identity record is missing")?;
    mount_id
        .parse()
        .context("descriptor mount identity is invalid")
}

fn read_descriptor_status_flags(pid: Pid, descriptor: i32) -> Result<u64> {
    let contents = std::fs::read_to_string(format!("/proc/{pid}/fdinfo/{descriptor}"))
        .context("read descriptor status flags")?;
    let flags = contents
        .lines()
        .find_map(|line| line.strip_prefix("flags:\t"))
        .context("descriptor status flags record is missing")?;
    u64::from_str_radix(flags, 8).context("descriptor status flags are invalid")
}

fn shared_descriptor_read_requires_progress(
    pid: Pid,
    read_descriptor: i32,
    identity: &CapturedDescriptorIdentity,
) -> Result<bool> {
    let directory = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .context("enumerate shared descriptor table")?;
    for entry in directory {
        let entry = entry.context("enumerate shared descriptor")?;
        let Some(descriptor) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        if read_descriptor_identity(pid, descriptor)?.as_ref() != Some(identity) {
            continue;
        }
        let flags = read_descriptor_status_flags(pid, descriptor)?;
        if flags & libc::O_ACCMODE as u64 != libc::O_RDONLY as u64 {
            if identity.kind == CapturedDescriptorKind::Socket {
                return Ok(true);
            }
            let path = PathBuf::from(format!("/proc/{pid}/fd/{read_descriptor}"));
            use std::os::unix::fs::OpenOptionsExt as _;
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_CLOEXEC | libc::O_NONBLOCK)
                .open(path)
                .context("open shared FIFO read endpoint")?;
            let mut available = 0i32;
            let result = unsafe { libc::ioctl(file.as_raw_fd(), libc::FIONREAD, &mut available) };
            if result != 0 {
                return Err(std::io::Error::last_os_error())
                    .context("query shared FIFO readable bytes");
            }
            return Ok(available == 0);
        }
    }
    Ok(false)
}

fn open_mode_is_used(flags: u64) -> bool {
    flags & u64::from((libc::O_CREAT | libc::O_TMPFILE) as u32) != 0
}

fn open_system_call(number: u64) -> Option<CapturedOpenSystemCall> {
    #[cfg(target_arch = "x86_64")]
    if number == libc::SYS_open as u64 {
        return Some(CapturedOpenSystemCall::Open);
    }
    if number == libc::SYS_openat as u64 {
        return Some(CapturedOpenSystemCall::OpenAt);
    }
    if number == libc::SYS_openat2 as u64 {
        return Some(CapturedOpenSystemCall::OpenAt2);
    }
    #[cfg(target_arch = "x86_64")]
    if number == libc::SYS_creat as u64 {
        return Some(CapturedOpenSystemCall::Creat);
    }
    None
}

fn readlink_system_call(number: u64) -> Option<CapturedReadlinkSystemCall> {
    #[cfg(target_arch = "x86_64")]
    if number == libc::SYS_readlink as u64 {
        return Some(CapturedReadlinkSystemCall::Readlink);
    }
    if number == libc::SYS_readlinkat as u64 {
        return Some(CapturedReadlinkSystemCall::ReadlinkAt);
    }
    None
}

fn xattr_system_call(number: u64) -> Option<CapturedXattrSystemCall> {
    if number == libc::SYS_getxattr as u64 {
        return Some(CapturedXattrSystemCall::Getxattr);
    }
    if number == libc::SYS_lgetxattr as u64 {
        return Some(CapturedXattrSystemCall::Lgetxattr);
    }
    if number == libc::SYS_fgetxattr as u64 {
        return Some(CapturedXattrSystemCall::Fgetxattr);
    }
    if number == libc::SYS_fsetxattr as u64 {
        return Some(CapturedXattrSystemCall::Fsetxattr);
    }
    None
}

fn access_system_call(number: u64) -> Option<CapturedAccessSystemCall> {
    #[cfg(target_arch = "x86_64")]
    if number == libc::SYS_access as u64 {
        return Some(CapturedAccessSystemCall::Access);
    }
    if number == libc::SYS_faccessat as u64 {
        return Some(CapturedAccessSystemCall::Faccessat);
    }
    if number == libc::SYS_faccessat2 as u64 {
        return Some(CapturedAccessSystemCall::Faccessat2);
    }
    None
}

fn read_requested_path(
    pid: Pid,
    dirfd: i32,
    path_address: u64,
    budget: &mut usize,
    description: &str,
) -> Result<(Vec<u8>, Option<Vec<u8>>)> {
    let path = read_tracee_c_string(pid, path_address, budget)
        .with_context(|| format!("read requested {description} path"))?;
    let directory = if path.first() == Some(&b'/') {
        None
    } else {
        let proc_path = if dirfd == libc::AT_FDCWD {
            PathBuf::from(format!("/proc/{pid}/cwd"))
        } else {
            PathBuf::from(format!("/proc/{pid}/fd/{dirfd}"))
        };
        Some(
            std::fs::read_link(proc_path)
                .with_context(|| format!("resolve requested {description} directory"))?
                .as_os_str()
                .as_bytes()
                .to_vec(),
        )
    };
    Ok((path, directory))
}

fn read_tracee_string_vector(pid: Pid, address: u64, budget: &mut usize) -> Result<Vec<Vec<u8>>> {
    if address == 0 {
        return Ok(Vec::new());
    }
    let pointer_size = std::mem::size_of::<usize>();
    let mut result = Vec::new();
    for index in 0..MAX_EXEC_VECTOR_ENTRIES {
        let offset = index
            .checked_mul(pointer_size)
            .and_then(|offset| usize::try_from(address).ok()?.checked_add(offset))
            .ok_or_else(|| anyhow::anyhow!("exec vector address overflowed"))?;
        let pointer = read_tracee_word(pid, offset)?;
        if pointer == 0 {
            return Ok(result);
        }
        result.push(read_tracee_c_string(pid, pointer as u64, budget)?);
    }
    anyhow::bail!("exec vector exceeded the capture entry limit")
}

fn read_tracee_c_string(pid: Pid, address: u64, budget: &mut usize) -> Result<Vec<u8>> {
    if address == 0 {
        anyhow::bail!("exec string pointer is null");
    }
    let word_size = std::mem::size_of::<libc::c_long>();
    let mut cursor = usize::try_from(address).context("exec string address exceeds usize")?;
    let mut result = Vec::new();
    loop {
        let aligned = cursor & !(word_size - 1);
        let word = read_tracee_word(pid, aligned)?.to_ne_bytes();
        for byte in &word[cursor - aligned..] {
            if *byte == 0 {
                return Ok(result);
            }
            if *budget == 0 {
                anyhow::bail!("exec request exceeded the capture byte limit");
            }
            result.push(*byte);
            *budget -= 1;
        }
        cursor = aligned
            .checked_add(word_size)
            .ok_or_else(|| anyhow::anyhow!("exec string address overflowed"))?;
    }
}

fn read_tracee_bounded_c_string(
    pid: Pid,
    address: u64,
    maximum_bytes: usize,
) -> Result<(Vec<u8>, bool)> {
    if address == 0 {
        anyhow::bail!("bounded tracee string pointer is null");
    }
    let word_size = std::mem::size_of::<libc::c_long>();
    let mut cursor = usize::try_from(address).context("bounded string address exceeds usize")?;
    let mut result = Vec::new();
    while result.len() < maximum_bytes {
        let aligned = cursor & !(word_size - 1);
        let word = read_tracee_word(pid, aligned)?.to_ne_bytes();
        for byte in &word[cursor - aligned..] {
            if *byte == 0 {
                return Ok((result, true));
            }
            result.push(*byte);
            if result.len() == maximum_bytes {
                return Ok((result, false));
            }
        }
        cursor = aligned
            .checked_add(word_size)
            .ok_or_else(|| anyhow::anyhow!("bounded string address overflowed"))?;
    }
    Ok((result, false))
}

fn read_tracee_bytes(pid: Pid, address: u64, length: usize) -> Result<Vec<u8>> {
    if address == 0 {
        anyhow::bail!("tracee byte pointer is null");
    }
    let word_size = std::mem::size_of::<libc::c_long>();
    let mut cursor = usize::try_from(address).context("tracee byte address exceeds usize")?;
    let end = cursor
        .checked_add(length)
        .ok_or_else(|| anyhow::anyhow!("tracee byte range overflowed"))?;
    let mut result = Vec::with_capacity(length);
    while cursor < end {
        let aligned = cursor & !(word_size - 1);
        let word = read_tracee_word(pid, aligned)?.to_ne_bytes();
        let available = (end - cursor).min(word_size - (cursor - aligned));
        result.extend_from_slice(&word[cursor - aligned..cursor - aligned + available]);
        cursor += available;
    }
    Ok(result)
}

fn copy_tracee_bytes(pid: Pid, address: u64, length: usize, description: &str) -> Result<Vec<u8>> {
    let mut result = vec![0u8; length];
    copy_tracee_range(pid, address, &mut result, description)?;
    Ok(result)
}

fn copy_tracee_range(
    pid: Pid,
    address: u64,
    destination: &mut [u8],
    description: &str,
) -> Result<()> {
    if destination.is_empty() {
        return Ok(());
    }
    if address == 0 {
        anyhow::bail!("successful syscall returned bytes through a null buffer");
    }
    let remote = usize::try_from(address).context("tracee buffer address exceeds usize")?;
    let mut copied = 0usize;
    while copied < destination.len() {
        let chunk_end = copied
            .checked_add((destination.len() - copied).min(64 * 1024))
            .ok_or_else(|| anyhow::anyhow!("tracee buffer byte count overflowed"))?;
        let mut chunk_copied = copied;
        while chunk_copied < chunk_end {
            let local = libc::iovec {
                iov_base: destination[chunk_copied..chunk_end].as_mut_ptr().cast(),
                iov_len: chunk_end - chunk_copied,
            };
            let remote_iovec = libc::iovec {
                iov_base: remote
                    .checked_add(chunk_copied)
                    .ok_or_else(|| anyhow::anyhow!("tracee buffer address overflowed"))?
                    as *mut libc::c_void,
                iov_len: chunk_end - chunk_copied,
            };
            // SAFETY: both iovec arrays contain one live range. The tracee is
            // stopped, and the kernel checks the remote userspace mapping.
            let result =
                unsafe { libc::process_vm_readv(pid.as_raw(), &local, 1, &remote_iovec, 1, 0) };
            if result < 0 {
                return Err(nix::errno::Errno::last()).with_context(|| description.to_string());
            }
            let result = usize::try_from(result).context("negative tracee-buffer byte count")?;
            if result == 0 || result > chunk_end - chunk_copied {
                anyhow::bail!("copying tracee bytes returned an invalid byte count");
            }
            chunk_copied = chunk_copied
                .checked_add(result)
                .ok_or_else(|| anyhow::anyhow!("tracee buffer byte count overflowed"))?;
        }
        copied = chunk_end;
    }
    Ok(())
}

fn hash_tracee_buffer_prefix(
    pid: Pid,
    buffers: &[ReadBuffer],
    length: u64,
    description: &str,
) -> Result<[u8; 32]> {
    if length == 0 {
        return Ok(*blake3::hash(&[]).as_bytes());
    }
    let mut remaining = length;
    let mut hasher = blake3::Hasher::new();
    let mut scratch = vec![0u8; 64 * 1024];
    for buffer in buffers {
        let bytes = remaining.min(buffer.length);
        hash_tracee_range(
            pid,
            buffer.address,
            bytes,
            &mut scratch,
            &mut hasher,
            description,
        )?;
        remaining -= bytes;
        if remaining == 0 {
            break;
        }
    }
    if remaining != 0 {
        anyhow::bail!("syscall result exceeds its captured buffers");
    }
    Ok(*hasher.finalize().as_bytes())
}

fn hash_tracee_range(
    pid: Pid,
    address: u64,
    length: u64,
    scratch: &mut [u8],
    hasher: &mut blake3::Hasher,
    description: &str,
) -> Result<()> {
    if length == 0 {
        return Ok(());
    }
    let mut remaining = usize::try_from(length).context("tracee byte count exceeds usize")?;
    let mut remote = address;
    while remaining != 0 {
        let chunk = remaining.min(scratch.len());
        copy_tracee_range(pid, remote, &mut scratch[..chunk], description)?;
        hasher.update(&scratch[..chunk]);
        remote = remote
            .checked_add(u64::try_from(chunk).context("tracee chunk size exceeds u64")?)
            .ok_or_else(|| anyhow::anyhow!("tracee buffer address overflowed"))?;
        remaining -= chunk;
    }
    Ok(())
}

fn read_tracee_word(pid: Pid, address: usize) -> Result<usize> {
    let address = address as ptrace::AddressType;
    Ok(ptrace::read(pid, address).context("read traced process memory")? as usize)
}

fn syscall_stop(pid: Pid) -> Result<SyscallStop> {
    let mut info = std::mem::MaybeUninit::<SyscallInfo>::zeroed();
    // SAFETY: `info` points to a correctly sized UAPI-compatible buffer. The
    // stopped tracee remains stable for the duration of this ptrace request.
    let result = unsafe {
        libc::ptrace(
            libc::PTRACE_GET_SYSCALL_INFO,
            pid.as_raw(),
            std::mem::size_of::<SyscallInfo>(),
            info.as_mut_ptr(),
        )
    };
    if result < 0 {
        return Err(nix::errno::Errno::last()).context("classify traced syscall stop");
    }
    // SAFETY: a successful request initialized the fixed header and the union
    // member selected by `operation`.
    let info = unsafe { info.assume_init() };
    let native_architecture = NATIVE_AUDIT_ARCHITECTURE
        .context("process capture does not support this native syscall architecture")?;
    if info.architecture != native_architecture {
        anyhow::bail!("process capture does not support a compat syscall architecture");
    }
    match info.operation {
        libc::PTRACE_SYSCALL_INFO_ENTRY => {
            require_syscall_info_size(
                result,
                std::mem::size_of::<u64>() + 6 * std::mem::size_of::<u64>(),
            )?;
            // SAFETY: the kernel selected the entry union member.
            let entry = unsafe { info.data.entry };
            reject_compat_syscall_number(entry.number)?;
            Ok(SyscallStop::Entry {
                entry,
                stack_pointer: info.stack_pointer,
            })
        }
        libc::PTRACE_SYSCALL_INFO_EXIT => {
            require_syscall_info_size(
                result,
                std::mem::size_of::<i64>() + std::mem::size_of::<u8>(),
            )?;
            // SAFETY: the kernel selected the exit union member.
            let exit = unsafe { info.data.exit };
            if exit.is_error > 1 {
                anyhow::bail!("kernel returned an invalid syscall error marker");
            }
            Ok(SyscallStop::Exit(exit))
        }
        libc::PTRACE_SYSCALL_INFO_SECCOMP => {
            anyhow::bail!("unexpected seccomp syscall stop in process capture")
        }
        _ => anyhow::bail!("kernel did not classify a traced syscall stop"),
    }
}

fn reject_compat_syscall_number(number: u64) -> Result<()> {
    #[cfg(target_arch = "x86_64")]
    if number & 0x4000_0000 != 0 {
        anyhow::bail!("process capture does not support the x32 syscall ABI");
    }
    let _ = number;
    Ok(())
}

fn require_syscall_info_size(result: libc::c_long, member_size: usize) -> Result<()> {
    let returned = usize::try_from(result).context("negative syscall-info byte count")?;
    let required = std::mem::offset_of!(SyscallInfo, data)
        .checked_add(member_size)
        .ok_or_else(|| anyhow::anyhow!("syscall-info size overflowed"))?;
    if returned < required {
        anyhow::bail!(
            "kernel returned a truncated syscall-info record ({returned} of {required} bytes)"
        );
    }
    Ok(())
}

fn directory_witness_key(metadata: &std::fs::Metadata) -> DirectoryWitnessKey {
    DirectoryWitnessKey {
        device: metadata.dev(),
        inode: metadata.ino(),
        bytes: metadata.size(),
        mode: metadata.mode(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    }
}

fn witness_complete_directory(
    directory: &mut File,
    expected: DirectoryWitnessKey,
) -> Result<CapturedDirectoryValue> {
    let before = directory.metadata().context("stat enumerated directory")?;
    let key = directory_witness_key(&before);
    if !before.file_type().is_dir() || key != expected || key.inode == 0 {
        anyhow::bail!("enumerated directory identity changed before witnessing");
    }
    directory
        .seek(SeekFrom::Start(0))
        .context("rewind enumerated directory witness")?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(COMPLETE_DIRECTORY_DIGEST_DOMAIN);
    let mut scratch = [0u8; 64 * 1024];
    let mut entry_count = 0u64;
    let mut name_bytes = 0u64;
    let mut members = Vec::new();
    loop {
        let returned = loop {
            let result = unsafe {
                libc::syscall(
                    libc::SYS_getdents64,
                    directory.as_raw_fd(),
                    scratch.as_mut_ptr(),
                    scratch.len(),
                )
            };
            if result >= 0 {
                break usize::try_from(result)
                    .context("complete directory result exceeds usize")?;
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINTR) {
                return Err(error).context("enumerate complete directory");
            }
        };
        if returned == 0 {
            break;
        }
        if returned > scratch.len() {
            anyhow::bail!("complete directory returned more bytes than requested");
        }
        for entry in parse_linux_dirent64(&scratch[..returned])? {
            entry_count = entry_count
                .checked_add(1)
                .context("complete directory entry count overflowed")?;
            if entry_count > MAX_COMPLETE_DIRECTORY_ENTRIES {
                anyhow::bail!("complete directory exceeds the entry limit");
            }
            let name_length = u32::try_from(entry.name.len())
                .context("complete directory entry name exceeds u32")?;
            name_bytes = name_bytes
                .checked_add(u64::from(name_length))
                .context("complete directory name bytes overflowed")?;
            if name_bytes > MAX_COMPLETE_DIRECTORY_NAME_BYTES {
                anyhow::bail!("complete directory exceeds the name-byte limit");
            }
            hasher.update(&entry.inode.to_le_bytes());
            hasher.update(&entry.next_position.to_le_bytes());
            hasher.update(&[entry.file_type]);
            hasher.update(&name_length.to_le_bytes());
            hasher.update(&entry.name);
            if entry.name.as_slice() != b"." && entry.name.as_slice() != b".." {
                members.push(entry.name);
            }
        }
    }
    let after = directory
        .metadata()
        .context("restat enumerated directory")?;
    if directory_witness_key(&after) != key {
        anyhow::bail!("enumerated directory changed while it was witnessed");
    }
    members.sort();
    if members.windows(2).any(|pair| pair[0] == pair[1]) {
        anyhow::bail!("complete directory contains duplicate member names");
    }
    let value = CapturedDirectoryValue::Complete {
        entries: entry_count,
        digest: *hasher.finalize().as_bytes(),
        members,
    };
    Ok(value)
}

fn mapped_file_witness_key(metadata: &std::fs::Metadata) -> MappedFileWitnessKey {
    MappedFileWitnessKey {
        device: metadata.dev(),
        inode: metadata.ino(),
        bytes: metadata.size(),
        mode: metadata.mode(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    }
}

fn mapped_file_synthetic_size_target(file: &File) -> Result<(bool, PathBuf)> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::ffi::OsStrExt as _;

    let target = std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
        .context("resolve mapped backing file target")?;
    Ok((
        crate::regular_file_value_has_synthetic_size(target.as_os_str().as_bytes()),
        target,
    ))
}

fn witness_regular_mapped_file(
    mut file: File,
    expected_device: u64,
    expected_inode: u64,
    witnesses: &mut MappedFileWitnessCache,
) -> Result<CapturedMappedFileValue> {
    let before = file.metadata().context("stat mapped backing file")?;
    let key = mapped_file_witness_key(&before);
    if !before.file_type().is_file()
        || key.device != expected_device
        || key.inode != expected_inode
        || key.inode == 0
    {
        anyhow::bail!("mapped backing file identity changed before witnessing");
    }
    let (synthetic_size, target) = mapped_file_synthetic_size_target(&file)?;
    if !synthetic_size {
        if let Some(value) = witnesses.get(&key) {
            return Ok(*value);
        }
    }

    let mut hasher = blake3::Hasher::new();
    let mut scratch = [0u8; 64 * 1024];
    let mut bytes = 0u64;
    loop {
        let read = file
            .read(&mut scratch)
            .context("read mapped backing file")?;
        if read == 0 {
            break;
        }
        hasher.update(&scratch[..read]);
        bytes = bytes
            .checked_add(u64::try_from(read).context("mapped read size exceeds u64")?)
            .context("mapped backing file size overflowed")?;
    }
    let after = file.metadata().context("restat mapped backing file")?;
    if !synthetic_size && bytes != key.bytes {
        anyhow::bail!(
            "mapped backing file length changed while it was witnessed: target {}, read {bytes}, expected {}",
            target.display(),
            key.bytes,
        );
    }
    let after_key = mapped_file_witness_key(&after);
    if after_key != key {
        anyhow::bail!(
            "mapped backing file metadata changed while it was witnessed: before {key:?}, after {after_key:?}"
        );
    }
    let value = CapturedMappedFileValue::WholeFile {
        bytes,
        digest: *hasher.finalize().as_bytes(),
        mode: key.mode,
    };
    if !synthetic_size {
        witnesses.insert(key, value);
    }
    Ok(value)
}

fn witness_regular_file_read(
    file: File,
    expected_device: u64,
    expected_inode: u64,
    offset: u64,
    bytes: u64,
    delivered_digest: [u8; 32],
) -> Result<CapturedMappedFileValue> {
    let before = file.metadata().context("stat read backing file")?;
    let key = mapped_file_witness_key(&before);
    if !before.file_type().is_file()
        || key.device != expected_device
        || key.inode != expected_inode
        || key.inode == 0
    {
        anyhow::bail!("read backing file identity changed before witnessing");
    }
    let (synthetic_size, _) = mapped_file_synthetic_size_target(&file)?;
    let range_end = offset
        .checked_add(bytes)
        .context("read witness range overflowed")?;
    if bytes != 0 && range_end > key.bytes {
        anyhow::bail!("read witness range exceeds the backing file");
    }

    // A metadata tuple is not a content version: always derive both digests
    // from this fresh pass over the retained descriptor.
    use std::os::unix::fs::FileExt as _;
    let mut whole_file_hasher = blake3::Hasher::new();
    let mut delivered_range_hasher = blake3::Hasher::new();
    let mut scratch = [0u8; 64 * 1024];
    let mut file_bytes = 0u64;
    let mut range_bytes = 0u64;
    loop {
        let read = file
            .read_at(&mut scratch, file_bytes)
            .context("read backing file for read witness")?;
        if read == 0 {
            break;
        }
        let read_u64 = u64::try_from(read).context("read witness size exceeds u64")?;
        let chunk_end = file_bytes
            .checked_add(read_u64)
            .context("read witness byte count overflowed")?;
        whole_file_hasher.update(&scratch[..read]);
        let overlap_start = file_bytes.max(offset);
        let overlap_end = chunk_end.min(range_end);
        if overlap_start < overlap_end {
            let start = usize::try_from(overlap_start - file_bytes)
                .context("read witness overlap start exceeds usize")?;
            let end = usize::try_from(overlap_end - file_bytes)
                .context("read witness overlap end exceeds usize")?;
            delivered_range_hasher.update(&scratch[start..end]);
            range_bytes = range_bytes
                .checked_add(overlap_end - overlap_start)
                .context("read witness range byte count overflowed")?;
        }
        file_bytes = chunk_end;
    }
    let after = file.metadata().context("restat read backing file")?;
    if (!synthetic_size && file_bytes != key.bytes) || mapped_file_witness_key(&after) != key {
        anyhow::bail!("read backing file changed while it was witnessed");
    }
    if range_bytes != bytes || *delivered_range_hasher.finalize().as_bytes() != delivered_digest {
        anyhow::bail!("read backing file range does not match the delivered bytes");
    }
    Ok(CapturedMappedFileValue::WholeFile {
        bytes: file_bytes,
        digest: *whole_file_hasher.finalize().as_bytes(),
        mode: key.mode,
    })
}

fn read_invocation(
    pid: Pid,
    sink_limit: Option<u64>,
    witnesses: &mut MappedFileWitnessCache,
) -> Result<CapturedInvocation> {
    let sink_limit = sink_limit
        .map(usize::try_from)
        .transpose()
        .context("process capture event limit exceeds usize")?
        .unwrap_or(MAX_INVOCATION_EVENT_BYTES);
    let mut budget = MAX_INVOCATION_EVENT_BYTES.min(sink_limit);
    let proc_root = PathBuf::from(format!("/proc/{pid}"));
    let executable = std::fs::read_link(proc_root.join("exe"))
        .context("read observed executable")?
        .as_os_str()
        .as_bytes()
        .to_vec();
    charge_capture_budget(&mut budget, executable.len(), "executable path")?;
    let working_directory = std::fs::read_link(proc_root.join("cwd"))
        .context("read observed working directory")?
        .as_os_str()
        .as_bytes()
        .to_vec();
    charge_capture_budget(
        &mut budget,
        working_directory.len(),
        "working-directory path",
    )?;
    let argv = read_nul_vector(
        &proc_root.join("cmdline"),
        "argument vector",
        true,
        &mut budget,
    )?;
    let environment = read_nul_vector(
        &proc_root.join("environ"),
        "environment",
        false,
        &mut budget,
    )?;
    let (signal_state, file_creation_mask, credentials) = read_exec_boundary_state(&proc_root)?;
    let file_mappings = read_exec_file_mappings(pid, &mut budget, witnesses)?;
    if argv.first().is_none_or(Vec::is_empty) {
        anyhow::bail!("observed successful exec has an empty argument zero");
    }
    Ok(CapturedInvocation {
        executable,
        argv,
        environment,
        working_directory,
        signal_state,
        file_creation_mask,
        credentials,
        file_mappings,
    })
}

fn read_exec_boundary_state(
    proc_root: &std::path::Path,
) -> Result<(CapturedExecSignalState, u32, CapturedCredentialState)> {
    let contents = read_bounded_file(
        &proc_root.join("status"),
        MAX_FDINFO_BYTES,
        "exec signal state",
    )?;
    let contents = std::str::from_utf8(&contents).context("decode exec signal state")?;
    let field = |name: &str, radix: u32| -> Result<u64> {
        let mut values = contents.lines().filter_map(|line| {
            line.strip_prefix(name)
                .and_then(|value| value.strip_prefix(':'))
                .map(str::trim)
        });
        let value = values
            .next()
            .with_context(|| format!("exec signal state omits {name}"))?;
        if values.next().is_some() {
            anyhow::bail!("exec signal state repeats {name}");
        }
        u64::from_str_radix(value, radix)
            .with_context(|| format!("decode exec boundary state {name}"))
    };
    let file_creation_mask =
        u32::try_from(field("Umask", 8)?).context("exec file-creation mask exceeds u32")?;
    if file_creation_mask & !0o777 != 0 {
        anyhow::bail!("exec file-creation mask contains unsupported bits");
    }
    let identities = |name: &str| -> Result<[u32; 4]> {
        let mut lines = contents.lines().filter_map(|line| {
            line.strip_prefix(name)
                .and_then(|value| value.strip_prefix(':'))
                .map(str::trim)
        });
        let line = lines
            .next()
            .with_context(|| format!("exec credential state omits {name}"))?;
        if lines.next().is_some() {
            anyhow::bail!("exec credential state repeats {name}");
        }
        let values = line
            .split_ascii_whitespace()
            .map(str::parse::<u32>)
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| format!("decode exec credential state {name}"))?;
        values
            .try_into()
            .map_err(|_| anyhow::anyhow!("exec credential state {name} does not have four values"))
    };
    let users = identities("Uid")?;
    let groups = identities("Gid")?;
    let mut supplementary_groups = contents
        .lines()
        .find_map(|line| {
            line.strip_prefix("Groups")
                .and_then(|value| value.strip_prefix(':'))
                .map(str::trim)
        })
        .context("exec credential state omits Groups")?
        .split_ascii_whitespace()
        .map(str::parse::<u32>)
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("decode exec credential state Groups")?;
    supplementary_groups.sort_unstable();
    Ok((
        CapturedExecSignalState {
            blocked: field("SigBlk", 16)?,
            ignored: field("SigIgn", 16)?,
            caught: field("SigCgt", 16)?,
            pending: field("SigPnd", 16)?,
            shared_pending: field("ShdPnd", 16)?,
        },
        file_creation_mask,
        CapturedCredentialState {
            real_user: users[0],
            effective_user: users[1],
            saved_user: users[2],
            filesystem_user: users[3],
            real_group: groups[0],
            effective_group: groups[1],
            saved_group: groups[2],
            filesystem_group: groups[3],
            supplementary_groups,
            inheritable_capabilities: field("CapInh", 16)?,
            permitted_capabilities: field("CapPrm", 16)?,
            effective_capabilities: field("CapEff", 16)?,
            bounding_capabilities: field("CapBnd", 16)?,
            ambient_capabilities: field("CapAmb", 16)?,
            no_new_privileges: match field("NoNewPrivs", 10)? {
                0 => false,
                1 => true,
                _ => anyhow::bail!("exec no-new-privileges state is invalid"),
            },
        },
    ))
}

fn read_process_credentials(proc_root: &std::path::Path) -> Result<CapturedCredentialState> {
    let (_, _, credentials) = read_exec_boundary_state(proc_root)?;
    Ok(credentials)
}

fn read_exec_file_mappings(
    pid: Pid,
    budget: &mut usize,
    witnesses: &mut MappedFileWitnessCache,
) -> Result<Vec<CapturedExecFileMapping>> {
    let proc_root = PathBuf::from(format!("/proc/{pid}"));
    let contents = read_bounded_file(&proc_root.join("maps"), *budget, "exec memory mappings")?;
    charge_capture_budget(budget, contents.len(), "exec memory mappings")?;
    let tracee_root = cached_tracee_root(pid)?;
    let mut mappings = Vec::new();
    for line in contents.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Some(mapping) = parse_proc_file_mapping(line)? else {
            continue;
        };
        if mappings.len() >= MAX_EXEC_FILE_MAPPINGS {
            anyhow::bail!("exec file mapping count exceeds capture limit");
        }
        let mapping_path = proc_root
            .join("map_files")
            .join(format!("{:x}-{:x}", mapping.start, mapping.end));
        let host_target =
            std::fs::read_link(&mapping_path).context("resolve exec file mapping target")?;
        let target_path = match host_target.strip_prefix(&tracee_root) {
            Ok(relative) => std::path::Path::new("/").join(relative),
            Err(_) => host_target,
        };
        let target = target_path.as_os_str().as_bytes().to_vec();
        if target.is_empty() {
            anyhow::bail!("exec file mapping has an empty target");
        }
        // Resolve the absolute mapping target through the tracee's root. This
        // follows its mount namespace without requiring the elevated kernel
        // capability needed to open a `map_files` symlink directly.
        let relative_target = target_path
            .strip_prefix("/")
            .context("exec file mapping target is not absolute")?;
        let rooted_target = proc_root.join("root").join(relative_target);
        let backing_file = File::open(rooted_target)
            .with_context(|| format!("open exec file mapping target {}", target_path.display()))?;
        let mount_id = read_descriptor_mount_id(Pid::this(), backing_file.as_raw_fd())?;
        let value =
            witness_regular_mapped_file(backing_file, mapping.device, mapping.inode, witnesses)
                .with_context(|| format!("witness exec file mapping {}", target_path.display()))?;
        charge_capture_budget(
            budget,
            target
                .len()
                .checked_add(std::mem::size_of::<CapturedExecFileMapping>())
                .context("exec file mapping size overflowed")?,
            "exec file mapping inventory",
        )?;
        mappings
            .try_reserve(1)
            .context("reserve exec file mapping inventory")?;
        mappings.push(CapturedExecFileMapping {
            start: mapping.start,
            end: mapping.end,
            file_offset: mapping.file_offset,
            readable: mapping.readable,
            writable: mapping.writable,
            executable: mapping.executable,
            shared: mapping.shared,
            identity: CapturedMappedFileIdentity {
                target,
                device: mapping.device,
                inode: mapping.inode,
                mount_id,
            },
            value,
        });
    }
    Ok(mappings)
}

fn parse_proc_file_mapping(line: &[u8]) -> Result<Option<ProcFileMapping>> {
    let mut fields = line
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty());
    let addresses = fields
        .next()
        .context("proc maps record has no address range")?;
    let permissions = fields
        .next()
        .context("proc maps record has no permissions")?;
    let file_offset = fields
        .next()
        .context("proc maps record has no file offset")?;
    let device = fields.next().context("proc maps record has no device")?;
    let inode = fields.next().context("proc maps record has no inode")?;
    let (start, end) = split_ascii_number(addresses, b'-', 16, "mapping address")?;
    if start >= end {
        anyhow::bail!("proc maps record has an invalid address range");
    }
    if permissions.len() != 4
        || !matches!(permissions[0], b'r' | b'-')
        || !matches!(permissions[1], b'w' | b'-')
        || !matches!(permissions[2], b'x' | b'-')
        || !matches!(permissions[3], b'p' | b's')
    {
        anyhow::bail!("proc maps record has invalid permissions");
    }
    let file_offset = parse_ascii_number(file_offset, 16, "mapping file offset")?;
    let (device_major, device_minor) = split_ascii_number(device, b':', 16, "mapping device")?;
    let device_major = u32::try_from(device_major).context("mapping device major exceeds u32")?;
    let device_minor = u32::try_from(device_minor).context("mapping device minor exceeds u32")?;
    let inode = parse_ascii_number(inode, 10, "mapping inode")?;
    if inode == 0 {
        return Ok(None);
    }
    Ok(Some(ProcFileMapping {
        start,
        end,
        file_offset,
        readable: permissions[0] == b'r',
        writable: permissions[1] == b'w',
        executable: permissions[2] == b'x',
        shared: permissions[3] == b's',
        device: libc::makedev(device_major, device_minor),
        inode,
    }))
}

fn split_ascii_number(
    value: &[u8],
    separator: u8,
    radix: u32,
    description: &str,
) -> Result<(u64, u64)> {
    let separator = value
        .iter()
        .position(|byte| *byte == separator)
        .with_context(|| format!("{description} has no separator"))?;
    Ok((
        parse_ascii_number(&value[..separator], radix, description)?,
        parse_ascii_number(&value[separator + 1..], radix, description)?,
    ))
}

fn parse_ascii_number(value: &[u8], radix: u32, description: &str) -> Result<u64> {
    let value =
        std::str::from_utf8(value).with_context(|| format!("{description} is not ASCII"))?;
    u64::from_str_radix(value, radix).with_context(|| format!("{description} is invalid"))
}

fn read_nul_vector(
    path: &std::path::Path,
    description: &str,
    allow_empty_noninitial_entries: bool,
    budget: &mut usize,
) -> Result<Vec<Vec<u8>>> {
    let mut file = File::open(path).with_context(|| format!("open observed {description}"))?;
    let mut values = Vec::new();
    let mut value = Vec::new();
    let mut scratch = [0u8; 4096];
    loop {
        let read = file
            .read(&mut scratch)
            .with_context(|| format!("read observed {description}"))?;
        if read == 0 {
            break;
        }
        for byte in &scratch[..read] {
            if *byte == 0 {
                if value.is_empty()
                    && (!allow_empty_noninitial_entries || values.is_empty())
                {
                    anyhow::bail!("observed {description} contains an empty entry");
                }
                if values.len() >= MAX_EXEC_VECTOR_ENTRIES {
                    anyhow::bail!("observed {description} exceeds its entry limit");
                }
                charge_capture_budget(budget, std::mem::size_of::<Vec<u8>>(), description)?;
                values
                    .try_reserve(1)
                    .with_context(|| format!("reserve observed {description}"))?;
                values.push(std::mem::take(&mut value));
            } else {
                charge_capture_budget(budget, 1, description)?;
                value
                    .try_reserve(1)
                    .with_context(|| format!("reserve observed {description} entry"))?;
                value.push(*byte);
            }
        }
    }
    if !value.is_empty() {
        if values.len() >= MAX_EXEC_VECTOR_ENTRIES {
            anyhow::bail!("observed {description} exceeds its entry limit");
        }
        charge_capture_budget(budget, std::mem::size_of::<Vec<u8>>(), description)?;
        values
            .try_reserve(1)
            .with_context(|| format!("reserve observed {description}"))?;
        values.push(value);
    }
    Ok(values)
}

fn read_bounded_file(path: &std::path::Path, limit: usize, description: &str) -> Result<Vec<u8>> {
    let limit = u64::try_from(limit).context("capture file limit exceeds u64")?;
    let mut file = File::open(path).with_context(|| format!("open observed {description}"))?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("read observed {description}"))?;
    if u64::try_from(bytes.len()).context("observed file size exceeds u64")? > limit {
        anyhow::bail!("observed {description} exceeds its byte limit");
    }
    Ok(bytes)
}

fn charge_capture_budget(budget: &mut usize, bytes: usize, description: &str) -> Result<()> {
    *budget = budget
        .checked_sub(bytes)
        .with_context(|| format!("observed {description} exceeds its capture budget"))?;
    Ok(())
}

fn wait_status_pid(status: WaitStatus) -> Option<Pid> {
    match status {
        WaitStatus::Exited(pid, _)
        | WaitStatus::Signaled(pid, _, _)
        | WaitStatus::Stopped(pid, _)
        | WaitStatus::PtraceEvent(pid, _, _)
        | WaitStatus::PtraceSyscall(pid)
        | WaitStatus::Continued(pid) => Some(pid),
        WaitStatus::StillAlive => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    fn whole_file_value_matches(value: Option<CapturedMappedFileValue>, contents: &[u8]) -> bool {
        matches!(
            value,
            Some(CapturedMappedFileValue::WholeFile { bytes, digest, .. })
                if bytes == contents.len() as u64
                    && digest == *blake3::hash(contents).as_bytes()
        )
    }

    #[test]
    fn witnesses_only_supported_regular_file_targets() {
        for target in [
            b"/tmp/input".as_slice(),
            b"/dev/shm/build/output",
            b"/sys/devices/system/cpu/online",
            b"/sys/kernel/mm/transparent_hugepage/enabled",
        ] {
            assert!(regular_file_value_target(target));
        }
        for target in [
            b"relative/input".as_slice(),
            b"/dev/null",
            b"/dev/shm",
            b"/proc/filesystems",
            b"/sys/devices/system/cpu/offline",
            b"/sys/kernel/notes",
            b"/sys/kernel/mm/transparent_hugepage/defrag",
        ] {
            assert!(!regular_file_value_target(target));
        }
    }

    fn assert_syscall_census(receipt: &ProcessCaptureReceipt) {
        receipt.validate_complete().unwrap();
        assert!(receipt.syscall_entries().unwrap() > 0);
        assert_eq!(
            receipt.syscall_entries().unwrap(),
            receipt.syscall_exits().unwrap() + receipt.terminal_syscalls().unwrap()
        );
    }

    #[test]
    fn bounds_descriptor_mount_identity_fdinfo() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fdinfo");
        let mut contents = b"mnt_id:\t42\n".to_vec();
        contents.resize(MAX_FDINFO_BYTES, b'x');
        std::fs::write(&path, &contents).unwrap();
        assert_eq!(
            read_descriptor_mount_id_file(&path, MAX_FDINFO_BYTES).unwrap(),
            42
        );

        contents.push(b'x');
        std::fs::write(&path, contents).unwrap();
        assert!(read_descriptor_mount_id_file(&path, MAX_FDINFO_BYTES).is_err());
    }

    #[test]
    fn recognizes_terminal_probe_evidence_candidates() {
        let (read_end, _write_end) = nix::unistd::pipe().unwrap();
        let entry = SyscallEntry {
            number: libc::SYS_ioctl as u64,
            arguments: [read_end.as_raw_fd() as u64, libc::TCGETS as u64, 0, 0, 0, 0],
        };
        let request = read_ioctl_request(Pid::this(), entry).unwrap().unwrap();
        assert_eq!(request.descriptor, read_end.as_raw_fd());
        assert_eq!(request.identity.kind, CapturedDescriptorKind::Fifo);
        assert_eq!(request.command, libc::TCGETS as u64);

        let entry = SyscallEntry {
            number: libc::SYS_ioctl as u64,
            arguments: [
                read_end.as_raw_fd() as u64,
                libc::TIOCGWINSZ as u64,
                0,
                0,
                0,
                0,
            ],
        };
        assert!(read_ioctl_request(Pid::this(), entry).unwrap().is_some());

        let null = File::open("/dev/null").unwrap();
        let entry = SyscallEntry {
            number: libc::SYS_ioctl as u64,
            arguments: [null.as_raw_fd() as u64, libc::TCGETS as u64, 0, 0, 0, 0],
        };
        let request = read_ioctl_request(Pid::this(), entry).unwrap().unwrap();
        assert_eq!(
            request.identity.kind,
            CapturedDescriptorKind::CharacterDevice
        );
        assert_eq!(request.identity.target, b"/dev/null");

        let regular = tempfile::tempfile().unwrap();
        let entry = SyscallEntry {
            number: libc::SYS_ioctl as u64,
            arguments: [regular.as_raw_fd() as u64, libc::TCGETS as u64, 0, 0, 0, 0],
        };
        let request = read_ioctl_request(Pid::this(), entry).unwrap().unwrap();
        assert_eq!(request.identity.kind, CapturedDescriptorKind::RegularFile);

        let entry = SyscallEntry {
            number: libc::SYS_ioctl as u64,
            arguments: [regular.as_raw_fd() as u64, 0, 0, 0, 0, 0],
        };
        assert!(read_ioctl_request(Pid::this(), entry).unwrap().is_none());
    }

    fn native_dirent64(
        inode: u64,
        next_position: i64,
        file_type: u8,
        name: &[u8],
        padding: u8,
    ) -> Vec<u8> {
        let unaligned = LINUX_DIRENT64_HEADER_SIZE + name.len() + 1;
        let record_length = unaligned.next_multiple_of(LINUX_DIRENT64_ALIGNMENT);
        let mut record = vec![padding; record_length];
        record[..8].copy_from_slice(&inode.to_ne_bytes());
        record[8..16].copy_from_slice(&next_position.to_ne_bytes());
        record[16..18].copy_from_slice(&(record_length as u16).to_ne_bytes());
        record[18] = file_type;
        record[19..19 + name.len()].copy_from_slice(name);
        record[19 + name.len()] = 0;
        record
    }

    #[test]
    fn parses_semantic_dirents_without_padding_bytes() {
        let mut bytes = native_dirent64(11, -7, libc::DT_REG, b"alpha", 0xa5);
        bytes.extend(native_dirent64(12, 99, libc::DT_UNKNOWN, b"\xff", 0x5a));

        let entries = parse_linux_dirent64(&bytes).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].inode, 11);
        assert_eq!(entries[0].next_position, -7);
        assert_eq!(entries[0].file_type, libc::DT_REG);
        assert_eq!(entries[0].name, b"alpha");
        assert_eq!(entries[1].inode, 12);
        assert_eq!(entries[1].next_position, 99);
        assert_eq!(entries[1].file_type, libc::DT_UNKNOWN);
        assert_eq!(entries[1].name, b"\xff");
    }

    #[test]
    fn rejects_malformed_native_dirents() {
        assert!(parse_linux_dirent64(&[0; 18]).is_err());

        for record_length in [0u16, 16, 23, 32] {
            let mut record = vec![0u8; 24];
            record[16..18].copy_from_slice(&record_length.to_ne_bytes());
            record[19] = b'a';
            record[20] = 0;
            assert!(parse_linux_dirent64(&record).is_err());
        }

        let mut unterminated = native_dirent64(1, 1, libc::DT_REG, b"a", 1);
        unterminated[20..].fill(1);
        assert!(parse_linux_dirent64(&unterminated).is_err());

        let mut empty = native_dirent64(1, 1, libc::DT_REG, b"a", 0);
        empty[19] = 0;
        assert!(parse_linux_dirent64(&empty).is_err());

        let slash = native_dirent64(1, 1, libc::DT_REG, b"a/b", 0);
        assert!(parse_linux_dirent64(&slash).is_err());

        let mut trailing = native_dirent64(1, 1, libc::DT_REG, b"a", 0);
        trailing.push(0);
        assert!(parse_linux_dirent64(&trailing).is_err());
    }

    #[test]
    fn bounds_getdents64_result_chunks() {
        assert_eq!(checked_getdents64_result_size(48, 64).unwrap(), 48);
        assert!(checked_getdents64_result_size(65, 64).is_err());
        assert!(checked_getdents64_result_size(
            i64::from(MAX_GETDENTS64_RESULT_BYTES) + 1,
            MAX_GETDENTS64_RESULT_BYTES + 1,
        )
        .is_err());
        assert!(checked_getdents64_result_size(-1, 64).is_err());
    }

    #[test]
    fn parses_inode_backed_proc_mapping_origins() {
        let mapping =
            parse_proc_file_mapping(b"00400000-00452000 r-xp 00001000 08:02 123 /path with spaces")
                .unwrap()
                .unwrap();
        assert_eq!(mapping.start, 0x0040_0000);
        assert_eq!(mapping.end, 0x0045_2000);
        assert_eq!(mapping.file_offset, 0x1000);
        assert!(mapping.readable);
        assert!(!mapping.writable);
        assert!(mapping.executable);
        assert!(!mapping.shared);
        assert_eq!(mapping.device, libc::makedev(8, 2));
        assert_eq!(mapping.inode, 123);

        assert!(
            parse_proc_file_mapping(b"70000000-70001000 rw-p 00000000 00:00 0")
                .unwrap()
                .is_none()
        );
        for malformed in [
            b"broken".as_slice(),
            b"00400000-00400000 r-xp 0 08:02 1 /x".as_slice(),
            b"00400000-00401000 r-xq 0 08:02 1 /x".as_slice(),
            b"00400000-00401000 r-xp 0 broken 1 /x".as_slice(),
        ] {
            assert!(parse_proc_file_mapping(malformed).is_err());
        }
    }

    #[test]
    fn witnesses_complete_directories_by_stable_identity_and_content() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("alpha"), b"a").unwrap();
        let metadata = directory.path().metadata().unwrap();
        let key = directory_witness_key(&metadata);
        let mut first = File::open(directory.path()).unwrap();
        let value = witness_complete_directory(&mut first, key).unwrap();
        assert!(matches!(
            &value,
            CapturedDirectoryValue::Complete { entries, members, .. }
                if *entries >= 3 && members == &[b"alpha".to_vec()]
        ));
        let mut second = File::open(directory.path()).unwrap();
        assert_eq!(witness_complete_directory(&mut second, key).unwrap(), value);

        let mut wrong_key = key;
        wrong_key.inode += 1;
        let mut wrong = File::open(directory.path()).unwrap();
        assert!(witness_complete_directory(&mut wrong, wrong_key).is_err());
        let regular = directory.path().join("alpha");
        let regular_key = directory_witness_key(&regular.metadata().unwrap());
        let mut regular_file = File::open(&regular).unwrap();
        assert!(witness_complete_directory(&mut regular_file, regular_key).is_err());

        std::fs::write(directory.path().join("beta"), b"b").unwrap();
        let changed_key = directory_witness_key(&directory.path().metadata().unwrap());
        let mut changed_directory = File::open(directory.path()).unwrap();
        let changed = witness_complete_directory(&mut changed_directory, changed_key).unwrap();
        assert_ne!(changed, value);
    }

    #[test]
    fn witnesses_regular_files_by_stable_identity_and_content() {
        let directory = tempfile::tempdir().unwrap();
        let nonempty = directory.path().join("nonempty");
        let empty = directory.path().join("empty");
        std::fs::write(&nonempty, b"mapped bytes\n").unwrap();
        std::fs::write(&empty, b"").unwrap();

        let metadata = std::fs::metadata(&nonempty).unwrap();
        let mut witnesses = MappedFileWitnessCache::new();
        let value = witness_regular_mapped_file(
            File::open(&nonempty).unwrap(),
            metadata.dev(),
            metadata.ino(),
            &mut witnesses,
        )
        .unwrap();
        assert_eq!(
            value,
            CapturedMappedFileValue::WholeFile {
                bytes: 13,
                digest: *blake3::hash(b"mapped bytes\n").as_bytes(),
                mode: metadata.mode(),
            }
        );
        assert_eq!(witnesses.len(), 1);
        assert_eq!(
            witness_regular_mapped_file(
                File::open(&nonempty).unwrap(),
                metadata.dev(),
                metadata.ino(),
                &mut witnesses,
            )
            .unwrap(),
            value
        );
        assert_eq!(witnesses.len(), 1);
        assert_eq!(
            witness_regular_file_read(
                File::open(&nonempty).unwrap(),
                metadata.dev(),
                metadata.ino(),
                7,
                5,
                *blake3::hash(b"bytes").as_bytes(),
            )
            .unwrap(),
            value
        );
        let rewritten = b"forged bytes\n";
        std::fs::write(&nonempty, rewritten).unwrap();
        let rewritten_value = witness_regular_file_read(
            File::open(&nonempty).unwrap(),
            metadata.dev(),
            metadata.ino(),
            7,
            5,
            *blake3::hash(b"bytes").as_bytes(),
        )
        .unwrap();
        assert_eq!(
            rewritten_value,
            CapturedMappedFileValue::WholeFile {
                bytes: rewritten.len() as u64,
                digest: *blake3::hash(rewritten).as_bytes(),
                mode: std::fs::metadata(&nonempty).unwrap().mode(),
            }
        );
        assert_ne!(rewritten_value, value);
        assert!(witness_regular_file_read(
            File::open(&nonempty).unwrap(),
            metadata.dev(),
            metadata.ino(),
            7,
            5,
            [0; 32],
        )
        .is_err());
        assert!(witness_regular_file_read(
            File::open(&nonempty).unwrap(),
            metadata.dev(),
            metadata.ino(),
            12,
            2,
            [0; 32],
        )
        .is_err());

        assert!(witness_regular_mapped_file(
            File::open(&nonempty).unwrap(),
            metadata.dev(),
            metadata.ino() + 1,
            &mut witnesses,
        )
        .is_err());
        assert!(witness_regular_mapped_file(
            File::open(directory.path()).unwrap(),
            directory.path().metadata().unwrap().dev(),
            directory.path().metadata().unwrap().ino(),
            &mut witnesses,
        )
        .is_err());

        let empty_metadata = std::fs::metadata(&empty).unwrap();
        assert_eq!(
            witness_regular_mapped_file(
                File::open(&empty).unwrap(),
                empty_metadata.dev(),
                empty_metadata.ino(),
                &mut witnesses,
            )
            .unwrap(),
            CapturedMappedFileValue::WholeFile {
                bytes: 0,
                digest: *blake3::hash(&[]).as_bytes(),
                mode: empty_metadata.mode(),
            }
        );
    }

    #[test]
    fn captures_stable_whole_file_values_for_mappings() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("mapped-input");
        std::fs::write(&input, vec![0x5au8; 4096]).unwrap();
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, os, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "libc.munmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t]\n",
                "number = ctypes.c_long(int(sys.argv[2]))\n",
                "fd = os.open(os.fsencode(sys.argv[1]), os.O_RDONLY)\n",
                "mapped = libc.syscall(number, ctypes.c_void_p(0), ",
                "ctypes.c_size_t(4096), ctypes.c_ulong(1), ctypes.c_ulong(2), ",
                "ctypes.c_int(fd), ctypes.c_ulong(0))\n",
                "assert mapped != -1, ctypes.get_errno()\n",
                "assert libc.munmap(ctypes.c_void_p(mapped), 4096) == 0\n",
                "writer = os.open(os.fsencode(sys.argv[1]), os.O_WRONLY)\n",
                "assert os.write(writer, b'k' * 4096) == 4096\n",
                "os.close(writer)\n",
                "mapped = libc.syscall(number, ctypes.c_void_p(0), ",
                "ctypes.c_size_t(4096), ctypes.c_ulong(1), ctypes.c_ulong(2), ",
                "ctypes.c_int(fd), ctypes.c_ulong(0))\n",
                "assert mapped != -1, ctypes.get_errno()\n",
                "assert libc.munmap(ctypes.c_void_p(mapped), 4096) == 0\n",
                "anonymous = libc.syscall(number, ctypes.c_void_p(0), ",
                "ctypes.c_size_t(4096), ctypes.c_ulong(3), ctypes.c_ulong(34), ",
                "ctypes.c_int(-1), ctypes.c_ulong(0))\n",
                "assert anonymous != -1, ctypes.get_errno()\n",
                "assert libc.munmap(ctypes.c_void_p(anonymous), 4096) == 0\n",
                "ctypes.set_errno(0)\n",
                "failed = libc.syscall(number, ctypes.c_void_p(0), ",
                "ctypes.c_size_t(4096), ctypes.c_ulong(1), ctypes.c_ulong(2), ",
                "ctypes.c_int(-1), ctypes.c_ulong(0))\n",
                "assert failed == -1 and ctypes.get_errno() == 9\n",
                "ctypes.set_errno(0)\n",
                "empty = libc.syscall(number, ctypes.c_void_p(0), ",
                "ctypes.c_size_t(0), ctypes.c_ulong(3), ctypes.c_ulong(34), ",
                "ctypes.c_int(-1), ctypes.c_ulong(0))\n",
                "assert empty == -1 and ctypes.get_errno() == 22\n",
                "os.close(fd)\n",
            ))
            .arg(&input)
            .arg(libc::SYS_mmap.to_string())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(receipt.mmap_syscalls().unwrap(), process.mmap_syscalls);
        assert_eq!(
            usize::try_from(process.mmap_syscalls).unwrap(),
            process.mmap_attempts.len()
        );
        assert!(process.invocations.iter().all(|invocation| {
            !invocation.file_mappings.is_empty()
                && invocation
                    .file_mappings
                    .iter()
                    .any(|mapping| mapping.executable)
                && invocation.file_mappings.iter().all(|mapping| {
                    mapping.identity.mount_id != 0
                        && matches!(mapping.value, CapturedMappedFileValue::WholeFile { .. })
                })
        }));
        let invocation = &process.invocations[0];
        let executable_mapping = invocation
            .file_mappings
            .iter()
            .find(|mapping| mapping.identity.target == invocation.executable)
            .unwrap();
        let executable_metadata = std::fs::metadata(std::path::Path::new(
            std::ffi::OsStr::from_bytes(&invocation.executable),
        ))
        .unwrap();
        assert_eq!(
            executable_mapping.identity.device,
            executable_metadata.dev()
        );
        assert_eq!(executable_mapping.identity.inode, executable_metadata.ino());
        assert_eq!(
            executable_mapping.value,
            CapturedMappedFileValue::WholeFile {
                bytes: executable_metadata.size(),
                digest: *blake3::hash(
                    &std::fs::read(std::path::Path::new(std::ffi::OsStr::from_bytes(
                        &invocation.executable,
                    )))
                    .unwrap(),
                )
                .as_bytes(),
                mode: executable_metadata.mode(),
            }
        );
        let input_target = input.as_os_str().as_bytes();
        let mapped_values: Vec<_> = process
            .mmap_attempts
            .iter()
            .enumerate()
            .filter_map(|(index, attempt)| {
                let matching = attempt
                    .request
                    .identity
                    .as_ref()
                    .is_some_and(|identity| identity.target == input_target);
                match (matching, attempt.outcome) {
                    (
                        true,
                        CapturedMmapOutcome::Mapped {
                            file_value: Some(value),
                            ..
                        },
                    ) => Some((index, value)),
                    _ => None,
                }
            })
            .collect();
        assert_eq!(mapped_values.len(), 2);
        assert_eq!(
            mapped_values[0].1,
            CapturedMappedFileValue::WholeFile {
                bytes: 4096,
                digest: *blake3::hash(&vec![0x5a; 4096]).as_bytes(),
                mode: std::fs::metadata(&input).unwrap().mode(),
            }
        );
        assert_eq!(
            mapped_values[1].1,
            CapturedMappedFileValue::WholeFile {
                bytes: 4096,
                digest: *blake3::hash(&vec![b'k'; 4096]).as_bytes(),
                mode: std::fs::metadata(&input).unwrap().mode(),
            }
        );
        let mapped_index = mapped_values[0].0;
        let mapped = &process.mmap_attempts[mapped_index];
        assert_eq!(mapped.request.length, 4096);
        assert_eq!(mapped.request.protection, 1);
        assert_eq!(mapped.request.flags, 2);
        assert_eq!(mapped.request.offset, 0);
        assert!(mapped.request.descriptor >= 0);
        assert!(mapped.request.mount_id.is_some_and(|mount_id| mount_id != 0));
        let anonymous_index = process
            .mmap_attempts
            .iter()
            .position(|attempt| {
                attempt.request.length == 4096
                    && attempt.request.flags == 34
                    && attempt.request.descriptor == -1
                    && attempt.request.mount_id.is_none()
                    && matches!(
                        attempt.outcome,
                        CapturedMmapOutcome::Mapped {
                            file_value: None,
                            ..
                        }
                    )
            })
            .unwrap();
        assert!(process.mmap_attempts.iter().any(|attempt| {
            attempt.request.length == 4096
                && attempt.request.flags == 2
                && attempt.request.descriptor == -1
                && attempt.request.identity.is_none()
                && attempt.request.mount_id.is_none()
                && attempt.outcome == CapturedMmapOutcome::Failed(libc::EBADF)
        }));
        assert!(process.mmap_attempts.iter().any(|attempt| {
            attempt.request.length == 0
                && attempt.request.flags == 34
                && attempt.outcome == CapturedMmapOutcome::Failed(libc::EINVAL)
        }));
        assert_eq!(receipt.unwitnessed_file_mappings().unwrap(), 0);
        receipt.require_witnessed_file_mappings().unwrap();

        let mut missing = serde_json::to_value(&receipt).unwrap();
        missing["processes"][0]["mmap_attempts"] = serde_json::json!([]);
        let missing: ProcessCaptureReceipt = serde_json::from_value(missing).unwrap();
        assert!(missing.validate_complete().is_err());

        let mut false_witness = serde_json::to_value(&receipt).unwrap();
        false_witness["processes"][0]["mmap_attempts"][mapped_index]["outcome"]["mapped"]
            ["file_value"] = serde_json::json!("unwitnessed");
        let false_witness: ProcessCaptureReceipt = serde_json::from_value(false_witness).unwrap();
        false_witness.validate_complete().unwrap();
        assert!(false_witness.require_witnessed_file_mappings().is_err());

        let mut missing_mmap_mount = serde_json::to_value(&receipt).unwrap();
        missing_mmap_mount["processes"][0]["mmap_attempts"][mapped_index]["request"]
            ["mount_id"] = serde_json::Value::Null;
        let missing_mmap_mount: ProcessCaptureReceipt =
            serde_json::from_value(missing_mmap_mount).unwrap();
        assert!(missing_mmap_mount.validate_complete().is_err());

        let mut anonymous_value = serde_json::to_value(&receipt).unwrap();
        anonymous_value["processes"][0]["mmap_attempts"][anonymous_index]["outcome"]["mapped"]
            ["file_value"] = serde_json::json!("unwitnessed");
        let anonymous_value: ProcessCaptureReceipt =
            serde_json::from_value(anonymous_value).unwrap();
        assert!(anonymous_value.validate_complete().is_err());

        let mut anonymous_identity = serde_json::to_value(&receipt).unwrap();
        anonymous_identity["processes"][0]["mmap_attempts"][anonymous_index]["request"]
            ["identity"] = serde_json::to_value(
            process.mmap_attempts[mapped_index]
                .request
                .identity
                .as_ref()
                .unwrap(),
        )
        .unwrap();
        let anonymous_identity: ProcessCaptureReceipt =
            serde_json::from_value(anonymous_identity).unwrap();
        assert!(anonymous_identity.validate_complete().is_err());

        let mut overflowing_range = serde_json::to_value(&receipt).unwrap();
        overflowing_range["processes"][0]["mmap_attempts"][mapped_index]["request"]["length"] =
            serde_json::json!(u64::MAX);
        overflowing_range["processes"][0]["mmap_attempts"][mapped_index]["outcome"]["mapped"]
            ["address"] = serde_json::json!(2);
        let overflowing_range: ProcessCaptureReceipt =
            serde_json::from_value(overflowing_range).unwrap();
        assert!(overflowing_range.validate_complete().is_err());

        let mut collision = serde_json::to_value(&receipt).unwrap();
        collision["processes"][0]["mmap_attempts"][mapped_index]["syscall_index"] =
            serde_json::json!(process.open_attempts[0].syscall_index);
        let collision: ProcessCaptureReceipt = serde_json::from_value(collision).unwrap();
        assert!(collision.validate_complete().is_err());

        let mut no_exec_mapping = serde_json::to_value(&receipt).unwrap();
        no_exec_mapping["processes"][0]["invocations"][0]["file_mappings"] = serde_json::json!([]);
        let no_exec_mapping: ProcessCaptureReceipt =
            serde_json::from_value(no_exec_mapping).unwrap();
        assert!(no_exec_mapping.validate_complete().is_err());

        let mut malformed_exec_mapping = serde_json::to_value(&receipt).unwrap();
        malformed_exec_mapping["processes"][0]["invocations"][0]["file_mappings"][0]["identity"]
            ["inode"] = serde_json::json!(0);
        let malformed_exec_mapping: ProcessCaptureReceipt =
            serde_json::from_value(malformed_exec_mapping).unwrap();
        assert!(malformed_exec_mapping.validate_complete().is_err());
        let mut missing_exec_mount = serde_json::to_value(&receipt).unwrap();
        missing_exec_mount["processes"][0]["invocations"][0]["file_mappings"][0]
            ["identity"]["mount_id"] = serde_json::json!(0);
        let missing_exec_mount: ProcessCaptureReceipt =
            serde_json::from_value(missing_exec_mount).unwrap();
        assert!(missing_exec_mount.validate_complete().is_err());
        let mut malformed_empty_value = serde_json::to_value(&receipt).unwrap();
        malformed_empty_value["processes"][0]["invocations"][0]["file_mappings"][0]["value"]
            ["whole_file"]["bytes"] = serde_json::json!(0u64);
        let malformed_empty_value: ProcessCaptureReceipt =
            serde_json::from_value(malformed_empty_value).unwrap();
        assert!(malformed_empty_value.validate_complete().is_err());
    }

    #[test]
    fn captures_scalar_write_bytes_offsets_and_append_state() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("output");
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, os, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "write_nr = ctypes.c_long(int(sys.argv[2]))\n",
                "pwrite_nr = ctypes.c_long(int(sys.argv[3]))\n",
                "fd = os.open(os.fsencode(sys.argv[1]), ",
                "os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)\n",
                "first = ctypes.create_string_buffer(b'abc')\n",
                "assert libc.syscall(write_nr, fd, ctypes.byref(first), 3) == 3\n",
                "second = ctypes.create_string_buffer(b'XY')\n",
                "assert libc.syscall(pwrite_nr, fd, ctypes.byref(second), 2, 1) == 2\n",
                "os.close(fd)\n",
                "fd = os.open(os.fsencode(sys.argv[1]), os.O_WRONLY | os.O_APPEND)\n",
                "last = ctypes.create_string_buffer(b'Z')\n",
                "assert libc.syscall(pwrite_nr, fd, ctypes.byref(last), 1, 0) == 1\n",
                "ctypes.set_errno(0)\n",
                "assert libc.syscall(write_nr, -1, ctypes.byref(last), 1) == -1\n",
                "assert ctypes.get_errno() == 9\n",
                "ctypes.set_errno(0)\n",
                "assert libc.syscall(write_nr, fd, ctypes.c_void_p(1), 1) == -1\n",
                "assert ctypes.get_errno() == 14\n",
                "assert libc.syscall(write_nr, fd, ctypes.c_void_p(0), 0) == 0\n",
                "os.close(fd)\n",
            ))
            .arg(&output)
            .arg(libc::SYS_write.to_string())
            .arg(libc::SYS_pwrite64.to_string())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(receipt.write_syscalls().unwrap(), process.write_syscalls);
        assert_eq!(
            usize::try_from(process.write_syscalls).unwrap(),
            process.write_attempts.len()
        );
        let output_target = output.as_os_str().as_bytes();
        assert!(process.open_attempts.iter().all(|attempt| {
            !matches!(
                &attempt.outcome,
                CapturedOpenOutcome::Opened {
                    identity,
                    file_value: Some(_),
                    ..
                } if identity.target == output_target
            )
        }));
        let attempts = process
            .write_attempts
            .iter()
            .filter(|attempt| {
                attempt
                    .request
                    .identity
                    .as_ref()
                    .is_some_and(|identity| identity.target == output_target)
            })
            .collect::<Vec<_>>();
        let write = attempts
            .iter()
            .find(|attempt| {
                attempt.request.system_call == CapturedWriteSystemCall::Write
                    && attempt.request.requested_bytes == 3
            })
            .unwrap();
        assert!(write.request.uses_descriptor_offset);
        assert!(write.request.mount_id.is_some_and(|mount_id| mount_id != 0));
        assert_eq!(write.request.offset, Some(0));
        assert!(!write.request.append);
        assert!(matches!(
            write.outcome,
            CapturedWriteOutcome::Wrote { bytes: 3, digest }
                if digest == *blake3::hash(b"abc").as_bytes()
        ));
        let pwrite_index = process
            .write_attempts
            .iter()
            .position(|attempt| {
                attempt
                    .request
                    .identity
                    .as_ref()
                    .is_some_and(|identity| identity.target == output_target)
                    && attempt.request.system_call == CapturedWriteSystemCall::Pwrite64
                    && !attempt.request.append
            })
            .unwrap();
        let pwrite = &process.write_attempts[pwrite_index];
        assert!(!pwrite.request.uses_descriptor_offset);
        assert_eq!(pwrite.request.offset, Some(1));
        assert!(matches!(
            pwrite.outcome,
            CapturedWriteOutcome::Wrote { bytes: 2, digest }
                if digest == *blake3::hash(b"XY").as_bytes()
        ));
        let append = attempts
            .iter()
            .find(|attempt| {
                attempt.request.system_call == CapturedWriteSystemCall::Pwrite64
                    && attempt.request.append
            })
            .unwrap();
        assert_eq!(append.request.offset, Some(0));
        assert!(matches!(
            append.outcome,
            CapturedWriteOutcome::Wrote { bytes: 1, digest }
                if digest == *blake3::hash(b"Z").as_bytes()
        ));
        let empty_index = process
            .write_attempts
            .iter()
            .position(|attempt| {
                attempt
                    .request
                    .identity
                    .as_ref()
                    .is_some_and(|identity| identity.target == output_target)
                    && matches!(
                        attempt.outcome,
                        CapturedWriteOutcome::Wrote { bytes: 0, .. }
                    )
            })
            .unwrap();
        assert!(matches!(
            process.write_attempts[empty_index].outcome,
            CapturedWriteOutcome::Wrote { digest, .. }
                if digest == *blake3::hash(&[]).as_bytes()
        ));
        assert!(attempts
            .iter()
            .any(|attempt| { attempt.outcome == CapturedWriteOutcome::Failed(libc::EFAULT) }));
        let bad_descriptor_index = process
            .write_attempts
            .iter()
            .position(|attempt| attempt.request.descriptor == -1)
            .unwrap();
        assert_eq!(
            process.write_attempts[bad_descriptor_index].outcome,
            CapturedWriteOutcome::Failed(libc::EBADF)
        );
        assert!(process.write_attempts[bad_descriptor_index]
            .request
            .mount_id
            .is_none());
        assert_eq!(std::fs::read(&output).unwrap(), b"aXYZ");

        let mut missing = serde_json::to_value(&receipt).unwrap();
        missing["processes"][0]["write_attempts"] = serde_json::json!([]);
        let missing: ProcessCaptureReceipt = serde_json::from_value(missing).unwrap();
        assert!(missing.validate_complete().is_err());

        let mut wrong_empty_digest = serde_json::to_value(&receipt).unwrap();
        wrong_empty_digest["processes"][0]["write_attempts"][empty_index]["outcome"]["wrote"]
            ["digest"][0] = serde_json::json!(1);
        let wrong_empty_digest: ProcessCaptureReceipt =
            serde_json::from_value(wrong_empty_digest).unwrap();
        assert!(wrong_empty_digest.validate_complete().is_err());

        let mut missing_offset = serde_json::to_value(&receipt).unwrap();
        missing_offset["processes"][0]["write_attempts"][pwrite_index]["request"]["offset"] =
            serde_json::Value::Null;
        let missing_offset: ProcessCaptureReceipt = serde_json::from_value(missing_offset).unwrap();
        assert!(missing_offset.validate_complete().is_err());

        let mut missing_mount = serde_json::to_value(&receipt).unwrap();
        missing_mount["processes"][0]["write_attempts"][pwrite_index]["request"]
            ["mount_id"] = serde_json::Value::Null;
        let missing_mount: ProcessCaptureReceipt =
            serde_json::from_value(missing_mount).unwrap();
        assert!(missing_mount.validate_complete().is_err());

        let mut invalid_append = serde_json::to_value(&receipt).unwrap();
        invalid_append["processes"][0]["write_attempts"][bad_descriptor_index]["request"]
            ["append"] = serde_json::json!(true);
        let invalid_append: ProcessCaptureReceipt = serde_json::from_value(invalid_append).unwrap();
        assert!(invalid_append.validate_complete().is_err());

        let mut collision = serde_json::to_value(&receipt).unwrap();
        collision["processes"][0]["write_attempts"][pwrite_index]["syscall_index"] =
            serde_json::json!(process.open_attempts[0].syscall_index);
        let collision: ProcessCaptureReceipt = serde_json::from_value(collision).unwrap();
        assert!(collision.validate_complete().is_err());
    }

    #[test]
    fn captures_getdents64_chunks_failures_and_denominator() {
        let directory = tempfile::tempdir().unwrap();
        let regular = directory.path().join("regular");
        std::fs::write(&regular, b"entry\n").unwrap();
        std::fs::create_dir(directory.path().join("subdirectory")).unwrap();
        std::os::unix::fs::symlink("regular", directory.path().join("symlink")).unwrap();
        let non_utf8 = std::ffi::OsString::from_vec(b"non-utf8-\xff".to_vec());
        std::fs::write(directory.path().join(&non_utf8), b"raw name\n").unwrap();

        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, os, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "number = int(sys.argv[2])\n",
                "fd = os.open(os.fsencode(sys.argv[1]), ",
                "os.O_RDONLY | os.O_DIRECTORY)\n",
                "calls = 0\n",
                "while True:\n",
                "    buffer = ctypes.create_string_buffer(48)\n",
                "    result = libc.syscall(number, fd, ctypes.byref(buffer), 48)\n",
                "    assert result >= 0, (result, ctypes.get_errno())\n",
                "    calls += 1\n",
                "    assert calls < 100\n",
                "    if result == 0:\n",
                "        break\n",
                "buffer = ctypes.create_string_buffer(48)\n",
                "ctypes.set_errno(0)\n",
                "result = libc.syscall(number, -1, ctypes.byref(buffer), 48)\n",
                "assert result == -1 and ctypes.get_errno() == 9\n",
                "regular = os.open(os.fsencode(sys.argv[3]), os.O_RDONLY)\n",
                "ctypes.set_errno(0)\n",
                "result = libc.syscall(number, regular, ctypes.byref(buffer), 48)\n",
                "assert result == -1 and ctypes.get_errno() == 20\n",
                "os.lseek(fd, 0, os.SEEK_SET)\n",
                "ctypes.set_errno(0)\n",
                "result = libc.syscall(number, fd, ctypes.c_void_p(1), 48)\n",
                "assert result == -1 and ctypes.get_errno() == 14\n",
                "os.lseek(fd, 0, os.SEEK_SET)\n",
                "ctypes.set_errno(0)\n",
                "result = libc.syscall(number, fd, ctypes.byref(buffer), 1)\n",
                "assert result == -1 and ctypes.get_errno() == 22\n",
                "os.close(regular)\n",
                "os.close(fd)\n",
            ))
            .arg(directory.path())
            .arg(libc::SYS_getdents64.to_string())
            .arg(&regular)
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(
            receipt.getdents64_syscalls().unwrap(),
            process.getdents64_syscalls
        );
        assert_eq!(
            usize::try_from(process.getdents64_syscalls).unwrap(),
            process.getdents64_attempts.len()
        );
        let directory_target = directory.path().as_os_str().as_bytes();
        let attempts = process
            .getdents64_attempts
            .iter()
            .filter(|attempt| {
                attempt
                    .request
                    .identity
                    .as_ref()
                    .is_some_and(|identity| identity.target == directory_target)
            })
            .collect::<Vec<_>>();
        assert!(attempts.len() > 2);
        assert!(attempts.iter().all(|attempt| {
            attempt.request.mount_id.is_some_and(|mount_id| mount_id != 0)
        }));
        assert_eq!(attempts[0].request.position_before, Some(0));
        let enumerated = attempts
            .iter()
            .filter_map(|attempt| match &attempt.outcome {
                CapturedGetdents64Outcome::Enumerated { entries, .. } => Some(entries),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(enumerated.last().is_some_and(|entries| entries.is_empty()));
        let names = enumerated
            .iter()
            .flat_map(|entries| entries.iter().map(|entry| entry.name.as_slice()))
            .collect::<BTreeSet<_>>();
        assert!(names.contains(b".".as_slice()));
        assert!(names.contains(b"..".as_slice()));
        assert!(names.contains(b"regular".as_slice()));
        assert!(names.contains(b"subdirectory".as_slice()));
        assert!(names.contains(b"symlink".as_slice()));
        assert!(names.contains(b"non-utf8-\xff".as_slice()));
        assert!(enumerated
            .iter()
            .flat_map(|entries| entries.iter())
            .all(|entry| {
                entry.record_length >= LINUX_DIRENT64_MINIMUM_RECORD_SIZE as u16
                    && usize::from(entry.record_length) % LINUX_DIRENT64_ALIGNMENT == 0
            }));
        let directory_values = attempts
            .iter()
            .filter_map(|attempt| match &attempt.outcome {
                CapturedGetdents64Outcome::Enumerated {
                    directory_value, ..
                } => Some(directory_value),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(!directory_values.is_empty());
        assert!(matches!(
            directory_values[0],
            CapturedDirectoryValue::Complete { entries, .. } if *entries >= 6
        ));
        assert!(directory_values
            .iter()
            .all(|value| *value == directory_values[0]));
        assert_eq!(receipt.unwitnessed_directory_enumerations().unwrap(), 0);
        receipt.require_witnessed_directory_enumerations().unwrap();
        assert!(process.getdents64_attempts.iter().any(|attempt| {
            attempt.request.identity.is_none()
                && attempt.request.mount_id.is_none()
                && attempt.outcome == CapturedGetdents64Outcome::Failed(libc::EBADF)
        }));
        assert!(process.getdents64_attempts.iter().any(|attempt| {
            attempt.request.identity.as_ref().is_some_and(|identity| {
                identity.target == regular.as_os_str().as_bytes()
                    && identity.kind == CapturedDescriptorKind::RegularFile
            }) && attempt.outcome == CapturedGetdents64Outcome::Failed(libc::ENOTDIR)
        }));
        assert!(attempts
            .iter()
            .any(|attempt| { attempt.outcome == CapturedGetdents64Outcome::Failed(libc::EFAULT) }));
        assert!(attempts.iter().any(|attempt| {
            attempt.request.requested_bytes == 1
                && attempt.outcome == CapturedGetdents64Outcome::Failed(libc::EINVAL)
        }));

        let first_index = process
            .getdents64_attempts
            .iter()
            .position(|attempt| {
                matches!(
                    attempt.outcome,
                    CapturedGetdents64Outcome::Enumerated {
                        returned_bytes,
                        ..
                    } if returned_bytes != 0
                )
            })
            .unwrap();
        let mut false_witness = serde_json::to_value(&receipt).unwrap();
        false_witness["processes"][0]["getdents64_attempts"][first_index]["outcome"]
            ["enumerated"]["directory_value"] = serde_json::json!("unwitnessed");
        let false_witness: ProcessCaptureReceipt = serde_json::from_value(false_witness).unwrap();
        false_witness.validate_complete().unwrap();
        assert!(false_witness
            .require_witnessed_directory_enumerations()
            .is_err());

        let mut missing_directory_mount = serde_json::to_value(&receipt).unwrap();
        missing_directory_mount["processes"][0]["getdents64_attempts"][first_index]
            ["request"]["mount_id"] = serde_json::Value::Null;
        let missing_directory_mount: ProcessCaptureReceipt =
            serde_json::from_value(missing_directory_mount).unwrap();
        assert!(missing_directory_mount.validate_complete().is_err());

        let mut malformed_empty = serde_json::to_value(&receipt).unwrap();
        malformed_empty["processes"][0]["getdents64_attempts"][first_index]["outcome"]
            ["enumerated"]["directory_value"]["complete"]["entries"] = serde_json::json!(0u64);
        let malformed_empty: ProcessCaptureReceipt =
            serde_json::from_value(malformed_empty).unwrap();
        assert!(malformed_empty.validate_complete().is_err());

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["getdents64_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());

        let mut bad_position = serde_json::to_value(&receipt).unwrap();
        bad_position["processes"][0]["getdents64_attempts"][first_index]["request"]
            ["position_before"] = serde_json::Value::Null;
        let bad_position: ProcessCaptureReceipt = serde_json::from_value(bad_position).unwrap();
        assert!(bad_position.validate_complete().is_err());

        let mut bad_position_after = serde_json::to_value(&receipt).unwrap();
        bad_position_after["processes"][0]["getdents64_attempts"][first_index]["outcome"]
            ["enumerated"]["position_after"] = serde_json::json!(-1);
        let bad_position_after: ProcessCaptureReceipt =
            serde_json::from_value(bad_position_after).unwrap();
        assert!(bad_position_after.validate_complete().is_err());

        let mut bad_bytes = serde_json::to_value(&receipt).unwrap();
        bad_bytes["processes"][0]["getdents64_attempts"][first_index]["outcome"]["enumerated"]
            ["returned_bytes"] = serde_json::json!(1);
        let bad_bytes: ProcessCaptureReceipt = serde_json::from_value(bad_bytes).unwrap();
        assert!(bad_bytes.validate_complete().is_err());

        let mut collision = serde_json::to_value(&receipt).unwrap();
        collision["processes"][0]["getdents64_attempts"][first_index]["syscall_index"] =
            serde_json::json!(process.open_attempts[0].syscall_index);
        let collision: ProcessCaptureReceipt = serde_json::from_value(collision).unwrap();
        assert!(collision.validate_complete().is_err());
    }

    #[test]
    fn captures_fork_exec_and_terminal_lifecycles() {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg("(/bin/true); /bin/false || :");
        command.env_clear().env("RUSH_OBSERVE_TEST", "present");

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        assert_eq!(receipt.root(), 1);
        assert!(receipt.processes().len() >= 3);
        assert!(receipt.processes().iter().all(|process| matches!(
            process.outcome,
            CapturedProcessOutcome::Exited(_) | CapturedProcessOutcome::Signaled(_)
        )));
        let invocations = receipt
            .processes()
            .iter()
            .flat_map(|process| &process.invocations)
            .collect::<Vec<_>>();
        assert!(invocations.iter().any(|invocation| invocation
            .environment
            .iter()
            .any(|entry| entry == b"RUSH_OBSERVE_TEST=present")));
        assert!(invocations
            .iter()
            .any(|invocation| invocation.executable.ends_with(b"/true")));
        assert!(invocations
            .iter()
            .any(|invocation| invocation.executable.ends_with(b"/false")));
        let loaded_requests = receipt
            .processes()
            .iter()
            .flat_map(|process| &process.exec_attempts)
            .filter(|attempt| matches!(attempt.outcome, CapturedExecOutcome::Loaded(_)))
            .collect::<Vec<_>>();
        assert!(loaded_requests
            .iter()
            .any(|attempt| attempt.request.executable.ends_with(b"/true")));
        assert!(loaded_requests
            .iter()
            .any(|attempt| attempt.request.executable.ends_with(b"/false")));
        assert!(loaded_requests.iter().all(|attempt| attempt
            .request
            .environment
            .iter()
            .any(|entry| entry == b"RUSH_OBSERVE_TEST=present")));
        let signal_deliveries = receipt
            .processes()
            .iter()
            .flat_map(|process| &process.signal_deliveries)
            .collect::<Vec<_>>();
        assert_eq!(
            receipt.signal_deliveries().unwrap(),
            signal_deliveries.len() as u64
        );
        assert!(signal_deliveries.iter().any(|delivery| {
            delivery.signal == libc::SIGCHLD as u32
                && delivery.child_status == Some(0)
                && matches!(delivery.source, CapturedSignalSource::TracedProcess { .. })
        }));

        let mut unknown_source = receipt.clone();
        let delivery = unknown_source
            .processes
            .iter_mut()
            .flat_map(|process| &mut process.signal_deliveries)
            .next()
            .unwrap();
        delivery.source = CapturedSignalSource::TracedProcess { process: u64::MAX };
        assert!(unknown_source
            .validate_complete()
            .unwrap_err()
            .to_string()
            .contains("unknown signal-delivery source"));

        let mut invalid_position = receipt.clone();
        let process = invalid_position
            .processes
            .iter_mut()
            .find(|process| !process.signal_deliveries.is_empty())
            .unwrap();
        process.signal_deliveries[0].active_syscall = Some(process.syscall_entries);
        assert!(invalid_position
            .validate_complete()
            .unwrap_err()
            .to_string()
            .contains("invalid signal delivery"));
    }

    #[test]
    fn captures_failed_exec_requests() {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("exec /proc/self/rush-observe-missing-executable");

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let attempts = &receipt.processes()[0].exec_attempts;
        assert_eq!(receipt.exec_syscalls().unwrap(), 1);
        assert_eq!(receipt.processes()[0].exec_syscalls, 1);
        assert_eq!(attempts.len(), 1);
        assert_eq!(
            attempts[0].request.executable,
            b"/proc/self/rush-observe-missing-executable"
        );
        assert_eq!(
            attempts[0].outcome,
            CapturedExecOutcome::Failed(libc::ENOENT)
        );
        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["exec_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());
        let mut misplaced = serde_json::to_value(&receipt).unwrap();
        misplaced["processes"][0]["exec_attempts"][0]["syscall_index"] =
            serde_json::json!(receipt.processes()[0].syscall_entries);
        let misplaced: ProcessCaptureReceipt = serde_json::from_value(misplaced).unwrap();
        assert!(misplaced.validate_complete().is_err());
    }

    #[test]
    fn captures_open_paths_results_and_denominator() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input");
        let contents = b"observed input\n";
        std::fs::write(&input, contents).unwrap();
        let mut command = Command::new("/bin/cat");
        command
            .args(["input", "missing"])
            .current_dir(directory.path())
            .env_clear()
            .stdout(std::process::Stdio::null());

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(receipt.open_syscalls().unwrap(), process.open_syscalls);
        assert_eq!(
            usize::try_from(process.open_syscalls).unwrap(),
            process.open_attempts.len()
        );
        let opened_index = process
            .open_attempts
            .iter()
            .position(|attempt| attempt.request.path == b"input")
            .unwrap();
        let opened = &process.open_attempts[opened_index];
        assert_eq!(
            opened.request.directory.as_deref(),
            Some(directory.path().as_os_str().as_bytes())
        );
        assert_eq!(opened.request.flags & libc::O_ACCMODE as u64, 0);
        assert!(matches!(
            &opened.outcome,
            CapturedOpenOutcome::Opened {
                descriptor,
                identity,
                mount_id,
                file_value: Some(CapturedMappedFileValue::WholeFile { bytes, digest, .. }),
            } if *descriptor >= 0
                && identity.target == input.as_os_str().as_bytes()
                && identity.kind == CapturedDescriptorKind::RegularFile
                && *mount_id != 0
                && *bytes == contents.len() as u64
                && *digest == *blake3::hash(contents).as_bytes()
        ));
        let input_metadata = std::fs::metadata(&input).unwrap();
        let CapturedOpenOutcome::Opened { identity, .. } = &opened.outcome else {
            unreachable!();
        };
        assert_eq!(identity.device, input_metadata.dev());
        assert_eq!(identity.inode, input_metadata.ino());
        let mut unwitnessed = serde_json::to_value(&receipt).unwrap();
        unwitnessed["processes"][0]["open_attempts"][opened_index]["outcome"]["opened"]
            ["file_value"] = serde_json::json!("unwitnessed");
        let unwitnessed: ProcessCaptureReceipt = serde_json::from_value(unwitnessed).unwrap();
        assert!(unwitnessed.validate_complete().is_err());
        let mut empty_identity = serde_json::to_value(&receipt).unwrap();
        empty_identity["processes"][0]["open_attempts"][opened_index]["outcome"]["opened"]
            ["identity"]["target"] = serde_json::json!([]);
        let empty_identity: ProcessCaptureReceipt =
            serde_json::from_value(empty_identity).unwrap();
        assert!(empty_identity.validate_complete().is_err());
        let mut wrong_kind = serde_json::to_value(&receipt).unwrap();
        wrong_kind["processes"][0]["open_attempts"][opened_index]["outcome"]["opened"]
            ["identity"]["kind"] = serde_json::json!("directory");
        let wrong_kind: ProcessCaptureReceipt = serde_json::from_value(wrong_kind).unwrap();
        assert!(wrong_kind.validate_complete().is_err());
        let missing = process
            .open_attempts
            .iter()
            .find(|attempt| attempt.request.path == b"missing")
            .unwrap();
        assert_eq!(missing.outcome, CapturedOpenOutcome::Failed(libc::ENOENT));
        let read_index = process
            .read_attempts
            .iter()
            .position(|attempt| {
                attempt.request.identity.as_ref().is_some_and(|identity| {
                    identity.target == input.as_os_str().as_bytes()
                }) && matches!(attempt.outcome, CapturedReadOutcome::Read { bytes, .. } if bytes != 0)
            })
            .unwrap();
        let read = &process.read_attempts[read_index];
        assert_eq!(read.request.system_call, CapturedReadSystemCall::Read);
        assert!(read.request.mount_id.is_some_and(|mount_id| mount_id != 0));
        assert!(read.request.uses_descriptor_offset);
        assert_eq!(read.request.offset, Some(0));
        assert_eq!(read.request.buffer_lengths.len(), 1);
        assert_eq!(read.request.buffer_lengths[0], read.request.requested_bytes);
        assert_eq!(read.request.flags, 0);
        assert!(matches!(
            read.outcome,
            CapturedReadOutcome::Read {
                bytes,
                digest,
                file_value,
            }
                if bytes == contents.len() as u64
                    && digest == *blake3::hash(contents).as_bytes()
                    && whole_file_value_matches(file_value, contents)
        ));
        let mut unwitnessed_read = serde_json::to_value(&receipt).unwrap();
        unwitnessed_read["processes"][0]["read_attempts"][read_index]["outcome"]["read"]
            ["file_value"] = serde_json::json!("unwitnessed");
        let unwitnessed_read: ProcessCaptureReceipt =
            serde_json::from_value(unwitnessed_read).unwrap();
        assert!(unwitnessed_read.validate_complete().is_err());
        let mut short_file = serde_json::to_value(&receipt).unwrap();
        short_file["processes"][0]["read_attempts"][read_index]["outcome"]["read"]["file_value"]
            ["whole_file"]["bytes"] = serde_json::json!(0);
        short_file["processes"][0]["read_attempts"][read_index]["outcome"]["read"]["file_value"]
            ["whole_file"]["digest"] = serde_json::to_value(*blake3::hash(&[]).as_bytes()).unwrap();
        let short_file: ProcessCaptureReceipt = serde_json::from_value(short_file).unwrap();
        assert!(short_file.validate_complete().is_err());
        let mut missing_read_mount = serde_json::to_value(&receipt).unwrap();
        missing_read_mount["processes"][0]["read_attempts"][read_index]["request"]
            ["mount_id"] = serde_json::Value::Null;
        let missing_read_mount: ProcessCaptureReceipt =
            serde_json::from_value(missing_read_mount).unwrap();
        assert!(missing_read_mount.validate_complete().is_err());
        assert_eq!(receipt.read_syscalls().unwrap(), process.read_syscalls);

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["open_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());
        let mut misplaced = serde_json::to_value(&receipt).unwrap();
        misplaced["processes"][0]["open_attempts"][0]["syscall_index"] =
            serde_json::json!(process.syscall_entries);
        let misplaced: ProcessCaptureReceipt = serde_json::from_value(misplaced).unwrap();
        assert!(misplaced.validate_complete().is_err());
    }

    #[test]
    fn captures_prlimit_queries_and_exact_mutations() {
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, errno, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "number = int(sys.argv[1])\n",
                "resource = 7\n",
                "class Limit(ctypes.Structure):\n",
                "    _fields_ = [('current', ctypes.c_uint64), ",
                "('maximum', ctypes.c_uint64)]\n",
                "def call(resource, new, old):\n",
                "    ctypes.set_errno(0)\n",
                "    value = libc.syscall(number, ctypes.c_int(0), ",
                "ctypes.c_uint(resource), new, old)\n",
                "    return value, ctypes.get_errno()\n",
                "null = ctypes.c_void_p(0)\n",
                "old = Limit()\n",
                "assert call(resource, null, ctypes.byref(old)) == (0, 0)\n",
                "assert old.current <= old.maximum\n",
                "assert call(resource, null, null) == (0, 0)\n",
                "assert call(0xffffffff, null, ctypes.byref(old)) == (-1, errno.EINVAL)\n",
                "same = Limit(old.current, old.maximum)\n",
                "assert call(resource, ctypes.byref(same), ctypes.byref(old)) == (0, 0)\n",
                "assert call(resource, ctypes.byref(same), null) == (0, 0)\n",
                "assert call(0xffffffff, ctypes.byref(same), null) == (-1, errno.EINVAL)\n",
            ))
            .arg(libc::SYS_prlimit64.to_string())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(
            receipt.prlimit_syscalls().unwrap(),
            process.prlimit_syscalls
        );
        assert_eq!(
            usize::try_from(process.prlimit_syscalls).unwrap(),
            process.prlimit_attempts.len()
        );
        let query_index = process
            .prlimit_attempts
            .iter()
            .position(|attempt| {
                attempt.request.process == 0
                    && attempt.request.resource == libc::RLIMIT_NOFILE
                    && attempt.request.new_limit_is_null
                    && !attempt.request.old_limit_is_null
                    && matches!(attempt.outcome, CapturedPrlimitOutcome::Limit(_))
            })
            .unwrap();
        let CapturedPrlimitOutcome::Limit(limit) = process.prlimit_attempts[query_index].outcome
        else {
            unreachable!();
        };
        assert!(limit.current <= limit.maximum);
        assert!(process.prlimit_attempts.iter().any(|attempt| {
            attempt.request.resource == libc::RLIMIT_NOFILE
                && attempt.request.new_limit_is_null
                && attempt.request.old_limit_is_null
                && attempt.outcome == CapturedPrlimitOutcome::Checked
        }));
        assert!(process.prlimit_attempts.iter().any(|attempt| {
            attempt.request.resource == u32::MAX
                && attempt.request.new_limit_is_null
                && attempt.outcome == CapturedPrlimitOutcome::Failed(libc::EINVAL)
        }));
        let successful_mutations = process
            .prlimit_attempts
            .iter()
            .filter_map(|attempt| match attempt.outcome {
                CapturedPrlimitOutcome::Mutated {
                    requested,
                    previous,
                } if attempt.request.resource == libc::RLIMIT_NOFILE => {
                    Some((attempt.request.old_limit_is_null, requested, previous))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(successful_mutations.len(), 2);
        assert!(successful_mutations
            .iter()
            .any(|(old_is_null, requested, previous)| {
                !old_is_null && *requested == limit && *previous == Some(limit)
            }));
        assert!(successful_mutations
            .iter()
            .any(|(old_is_null, requested, previous)| {
                *old_is_null && *requested == limit && previous.is_none()
            }));
        assert!(process.prlimit_attempts.iter().any(|attempt| {
            attempt.request.resource == u32::MAX
                && !attempt.request.new_limit_is_null
                && attempt.outcome
                    == (CapturedPrlimitOutcome::MutationFailed {
                        requested: limit,
                        errno: libc::EINVAL,
                    })
        }));

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["prlimit_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());

        let mut invalid_limits = serde_json::to_value(&receipt).unwrap();
        invalid_limits["processes"][0]["prlimit_attempts"][query_index]["outcome"]["limit"]
            ["current"] = serde_json::json!(1);
        invalid_limits["processes"][0]["prlimit_attempts"][query_index]["outcome"]["limit"]
            ["maximum"] = serde_json::json!(0);
        let invalid_limits: ProcessCaptureReceipt = serde_json::from_value(invalid_limits).unwrap();
        assert!(invalid_limits.validate_complete().is_err());

        let self_mutation_index = process
            .prlimit_attempts
            .iter()
            .position(|attempt| {
                attempt.request.resource == libc::RLIMIT_NOFILE
                    && !attempt.request.new_limit_is_null
                    && attempt.request.old_limit_is_null
                    && matches!(attempt.outcome, CapturedPrlimitOutcome::Mutated { .. })
            })
            .unwrap();
        let mut invalid_requested = serde_json::to_value(&receipt).unwrap();
        invalid_requested["processes"][0]["prlimit_attempts"][self_mutation_index]["outcome"]
            ["mutated"]["requested"]["current"] = serde_json::json!(2);
        invalid_requested["processes"][0]["prlimit_attempts"][self_mutation_index]["outcome"]
            ["mutated"]["requested"]["maximum"] = serde_json::json!(1);
        let invalid_requested: ProcessCaptureReceipt =
            serde_json::from_value(invalid_requested).unwrap();
        assert!(invalid_requested.validate_complete().is_err());

        let mut unexpected_previous = serde_json::to_value(&receipt).unwrap();
        unexpected_previous["processes"][0]["prlimit_attempts"][self_mutation_index]["outcome"]
            ["mutated"]["previous"] = serde_json::to_value(limit).unwrap();
        let unexpected_previous: ProcessCaptureReceipt =
            serde_json::from_value(unexpected_previous).unwrap();
        assert!(unexpected_previous.validate_complete().is_err());

        let failed_mutation_index = process
            .prlimit_attempts
            .iter()
            .position(|attempt| {
                matches!(
                    attempt.outcome,
                    CapturedPrlimitOutcome::MutationFailed { .. }
                )
            })
            .unwrap();
        let mut zero_errno = serde_json::to_value(&receipt).unwrap();
        zero_errno["processes"][0]["prlimit_attempts"][failed_mutation_index]["outcome"]
            ["mutation_failed"]["errno"] = serde_json::json!(0);
        let zero_errno: ProcessCaptureReceipt = serde_json::from_value(zero_errno).unwrap();
        assert!(zero_errno.validate_complete().is_err());

        let mut collision = serde_json::to_value(&receipt).unwrap();
        collision["processes"][0]["prlimit_attempts"][query_index]["syscall_index"] =
            serde_json::json!(process.open_attempts[0].syscall_index);
        let collision: ProcessCaptureReceipt = serde_json::from_value(collision).unwrap();
        assert!(collision.validate_complete().is_err());
    }

    #[test]
    fn captures_close_identity_failures_and_denominator() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("close-value");
        std::fs::write(&path, b"old\n").unwrap();
        let final_contents = b"final close value\n";
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, errno, os, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "number = int(sys.argv[1])\n",
                "path = os.fsencode(sys.argv[2])\n",
                "fd = os.open(path, os.O_WRONLY | os.O_TRUNC)\n",
                "assert os.write(fd, b'final close value\\n') == 18\n",
                "assert libc.syscall(number, ctypes.c_int(fd)) == 0\n",
                "ctypes.set_errno(0)\n",
                "assert libc.syscall(number, ctypes.c_int(-1)) == -1\n",
                "assert ctypes.get_errno() == errno.EBADF\n",
            ))
            .arg(libc::SYS_close.to_string())
            .arg(&path)
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(receipt.close_syscalls().unwrap(), process.close_syscalls);
        assert_eq!(
            usize::try_from(process.close_syscalls).unwrap(),
            process.close_attempts.len()
        );
        let closed_index = process
            .close_attempts
            .iter()
            .position(|attempt| {
                attempt.request.descriptor >= 0
                    && attempt.request.identity.as_ref().is_some_and(|identity| {
                        identity.target == path.as_os_str().as_bytes()
                    })
                    && attempt.request.mount_id.is_some()
                    && matches!(attempt.outcome, CapturedCloseOutcome::Closed { .. })
            })
            .unwrap();
        let CapturedCloseOutcome::Closed { file_value } =
            process.close_attempts[closed_index].outcome
        else {
            unreachable!();
        };
        assert!(whole_file_value_matches(file_value, final_contents));

        let mut malformed_value = serde_json::to_value(&receipt).unwrap();
        malformed_value["processes"][0]["close_attempts"][closed_index]["outcome"]["closed"]
            ["file_value"]["whole_file"]["mode"] =
            serde_json::json!(libc::S_IFDIR | 0o755);
        let malformed_value: ProcessCaptureReceipt =
            serde_json::from_value(malformed_value).unwrap();
        assert!(malformed_value.validate_complete().is_err());
        assert!(process.close_attempts.iter().any(|attempt| {
            attempt.request.descriptor == -1
                && attempt.request.identity.is_none()
                && attempt.request.mount_id.is_none()
                && attempt.outcome == CapturedCloseOutcome::Failed(libc::EBADF)
        }));

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["close_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());

        let mut missing_identity = serde_json::to_value(&receipt).unwrap();
        missing_identity["processes"][0]["close_attempts"][closed_index]["request"]["identity"] =
            serde_json::Value::Null;
        missing_identity["processes"][0]["close_attempts"][closed_index]["request"]["mount_id"] =
            serde_json::Value::Null;
        let missing_identity: ProcessCaptureReceipt =
            serde_json::from_value(missing_identity).unwrap();
        assert!(missing_identity.validate_complete().is_err());

        let mut collision = serde_json::to_value(&receipt).unwrap();
        collision["processes"][0]["close_attempts"][closed_index]["syscall_index"] =
            serde_json::json!(process.open_attempts[0].syscall_index);
        let collision: ProcessCaptureReceipt = serde_json::from_value(collision).unwrap();
        assert!(collision.validate_complete().is_err());

        let mut terminal = serde_json::to_value(&receipt).unwrap();
        terminal["processes"][0]["close_attempts"][closed_index]["outcome"] =
            serde_json::json!("terminated");
        terminal["processes"][0]["syscall_exits"] = serde_json::json!(process.syscall_exits - 1);
        terminal["processes"][0]["terminal_syscalls"] =
            serde_json::json!(process.terminal_syscalls + 1);
        let terminal: ProcessCaptureReceipt = serde_json::from_value(terminal).unwrap();
        terminal.validate_complete().unwrap();
    }

    #[test]
    fn captures_seek_identity_results_and_denominator() {
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, errno, os, sys, tempfile\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "number = int(sys.argv[1])\n",
                "fd, path = tempfile.mkstemp()\n",
                "assert os.write(fd, b'0123456789') == 10\n",
                "assert libc.syscall(number, fd, -7, os.SEEK_CUR) == 3\n",
                "ctypes.set_errno(0)\n",
                "assert libc.syscall(number, -1, 0, os.SEEK_SET) == -1\n",
                "assert ctypes.get_errno() == errno.EBADF\n",
                "os.close(fd)\n",
                "os.unlink(path)\n",
            ))
            .arg(libc::SYS_lseek.to_string())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(receipt.seek_syscalls().unwrap(), process.seek_syscalls);
        assert_eq!(
            usize::try_from(process.seek_syscalls).unwrap(),
            process.seek_attempts.len()
        );
        let positioned_index = process
            .seek_attempts
            .iter()
            .position(|attempt| {
                attempt.request.offset == -7
                    && attempt.request.whence == libc::SEEK_CUR as u64
                    && attempt.request.identity.is_some()
                    && attempt.request.mount_id.is_some()
                    && attempt.outcome == CapturedSeekOutcome::Positioned(3)
            })
            .unwrap();
        assert!(process.seek_attempts.iter().any(|attempt| {
            attempt.request.descriptor == -1
                && attempt.request.identity.is_none()
                && attempt.request.mount_id.is_none()
                && attempt.outcome == CapturedSeekOutcome::Failed(libc::EBADF)
        }));

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["seek_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());

        let mut missing_identity = serde_json::to_value(&receipt).unwrap();
        missing_identity["processes"][0]["seek_attempts"][positioned_index]["request"]
            ["identity"] = serde_json::Value::Null;
        missing_identity["processes"][0]["seek_attempts"][positioned_index]["request"]
            ["mount_id"] = serde_json::Value::Null;
        let missing_identity: ProcessCaptureReceipt =
            serde_json::from_value(missing_identity).unwrap();
        assert!(missing_identity.validate_complete().is_err());

        let mut unsupported = serde_json::to_value(&receipt).unwrap();
        unsupported["processes"][0]["seek_attempts"][positioned_index]["request"]["whence"] =
            serde_json::json!(u64::MAX);
        let unsupported: ProcessCaptureReceipt = serde_json::from_value(unsupported).unwrap();
        assert!(unsupported.validate_complete().is_err());

        let mut impossible_position = serde_json::to_value(&receipt).unwrap();
        impossible_position["processes"][0]["seek_attempts"][positioned_index]["outcome"]
            ["positioned"] = serde_json::json!(u64::MAX);
        let impossible_position: ProcessCaptureReceipt =
            serde_json::from_value(impossible_position).unwrap();
        assert!(impossible_position.validate_complete().is_err());
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn captures_bounded_file_advice_contract() {
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, errno, os, sys, tempfile\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "number = int(sys.argv[1])\n",
                "fd, path = tempfile.mkstemp()\n",
                "assert libc.syscall(number, fd, 0, 0, 2) == 0\n",
                "ctypes.set_errno(0)\n",
                "assert libc.syscall(number, -1, 0, 0, 2) == -1\n",
                "assert ctypes.get_errno() == errno.EBADF\n",
                "assert libc.syscall(number, fd, 0, 0, 0) == 0\n",
                "os.close(fd)\n",
                "os.unlink(path)\n",
            ))
            .arg(libc::SYS_fadvise64.to_string())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(
            receipt.file_advice_syscalls().unwrap(),
            process.file_advice_syscalls
        );
        assert_eq!(process.file_advice_attempts.len(), 2);
        let completed_index = process
            .file_advice_attempts
            .iter()
            .position(|attempt| {
                attempt.request.descriptor >= 0
                    && attempt.request.identity.as_ref().is_some_and(|identity| {
                        identity.kind == CapturedDescriptorKind::RegularFile
                    })
                    && attempt.request.mount_id.is_some()
                    && attempt.request.offset == 0
                    && attempt.request.length == 0
                    && attempt.request.advice == libc::POSIX_FADV_SEQUENTIAL
                    && attempt.outcome == CapturedFileAdviceOutcome::Completed
            })
            .unwrap();
        assert!(process.file_advice_attempts.iter().any(|attempt| {
            attempt.request.descriptor == -1
                && attempt.request.identity.is_none()
                && attempt.request.mount_id.is_none()
                && attempt.outcome == CapturedFileAdviceOutcome::Failed(libc::EBADF)
        }));
        assert!(process.unmodeled_syscall_attempts.iter().any(|attempt| {
            attempt.number == libc::SYS_fadvise64 as u64
                && attempt.arguments[3] == libc::POSIX_FADV_NORMAL as u64
        }));

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["file_advice_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());

        for (field, value) in [
            ("offset", serde_json::json!(1)),
            ("length", serde_json::json!(1)),
            ("advice", serde_json::json!(libc::POSIX_FADV_NORMAL)),
        ] {
            let mut unsupported = serde_json::to_value(&receipt).unwrap();
            unsupported["processes"][0]["file_advice_attempts"][completed_index]["request"]
                [field] = value;
            let unsupported: ProcessCaptureReceipt =
                serde_json::from_value(unsupported).unwrap();
            assert!(unsupported.validate_complete().is_err());
        }

        let mut missing_identity = serde_json::to_value(&receipt).unwrap();
        missing_identity["processes"][0]["file_advice_attempts"][completed_index]["request"]
            ["identity"] = serde_json::Value::Null;
        missing_identity["processes"][0]["file_advice_attempts"][completed_index]["request"]
            ["mount_id"] = serde_json::Value::Null;
        let missing_identity: ProcessCaptureReceipt =
            serde_json::from_value(missing_identity).unwrap();
        assert!(missing_identity.validate_complete().is_err());
    }

    #[test]
    fn captures_fcntl_identity_results_and_denominator() {
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, errno, fcntl, os, sys, tempfile\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "number = int(sys.argv[1])\n",
                "fd, path = tempfile.mkstemp()\n",
                "assert libc.syscall(number, fd, fcntl.F_GETFL, 0) >= 0\n",
                "duplicate = libc.syscall(number, fd, fcntl.F_DUPFD, 10)\n",
                "assert duplicate >= 10\n",
                "os.close(duplicate)\n",
                "cloexec = libc.syscall(number, fd, fcntl.F_DUPFD_CLOEXEC, 20)\n",
                "assert cloexec >= 20\n",
                "assert libc.syscall(number, cloexec, fcntl.F_GETFD, 0) == ",
                "fcntl.FD_CLOEXEC\n",
                "os.close(cloexec)\n",
                "fcntl.lockf(fd, fcntl.LOCK_EX)\n",
                "fcntl.lockf(fd, fcntl.LOCK_UN)\n",
                "ctypes.set_errno(0)\n",
                "assert libc.syscall(number, fd, fcntl.F_SETLKW, 0) == -1\n",
                "assert ctypes.get_errno() == errno.EFAULT\n",
                "ctypes.set_errno(0)\n",
                "assert libc.syscall(number, -1, fcntl.F_GETFD, 0) == -1\n",
                "assert ctypes.get_errno() == errno.EBADF\n",
                "os.close(fd)\n",
                "os.unlink(path)\n",
            ))
            .arg(libc::SYS_fcntl.to_string())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(receipt.fcntl_syscalls().unwrap(), process.fcntl_syscalls);
        assert_eq!(
            usize::try_from(process.fcntl_syscalls).unwrap(),
            process.fcntl_attempts.len()
        );
        let duplicate_index = process
            .fcntl_attempts
            .iter()
            .position(|attempt| {
                attempt.request.command == libc::F_DUPFD as u32
                    && attempt.request.argument == 10
                    && matches!(
                        &attempt.outcome,
                        CapturedFcntlOutcome::Returned {
                            value,
                            result_identity: Some(identity),
                            result_mount_id: Some(_),
                        } if *value >= 10
                            && Some(identity) == attempt.request.identity.as_ref()
                    )
            })
            .unwrap();
        let cloexec_index = process
            .fcntl_attempts
            .iter()
            .position(|attempt| {
                attempt.request.command == libc::F_DUPFD_CLOEXEC as u32
                    && attempt.request.argument == 20
                    && matches!(
                        &attempt.outcome,
                        CapturedFcntlOutcome::Returned {
                            value,
                            result_identity: Some(identity),
                            result_mount_id: Some(_),
                        } if *value >= 20
                            && Some(identity) == attempt.request.identity.as_ref()
                    )
            })
            .unwrap();
        let CapturedFcntlOutcome::Returned {
            value: cloexec_descriptor,
            ..
        } = &process.fcntl_attempts[cloexec_index].outcome
        else {
            unreachable!();
        };
        assert!(process.fcntl_attempts.iter().any(|attempt| {
            attempt.request.command == libc::F_GETFD as u32
                && i64::from(attempt.request.descriptor) == *cloexec_descriptor
                && attempt.outcome
                    == CapturedFcntlOutcome::Returned {
                        value: i64::from(libc::FD_CLOEXEC),
                        result_identity: None,
                        result_mount_id: None,
                    }
        }));
        assert!(process.fcntl_attempts.iter().any(|attempt| {
            attempt.request.command == libc::F_GETFL as u32
                && matches!(
                    &attempt.outcome,
                    CapturedFcntlOutcome::Returned {
                        result_identity: None,
                        result_mount_id: None,
                        ..
                    }
                )
        }));
        let record_lock_index = process
            .fcntl_attempts
            .iter()
            .position(|attempt| {
                attempt.request.command == libc::F_SETLKW as u32
                    && attempt.request.argument != 0
                    && attempt.request.record_lock
                        == Some(CapturedFcntlRecordLock {
                            lock_type: libc::F_WRLCK as i16,
                            whence: libc::SEEK_SET as i16,
                            start: 0,
                            length: 0,
                        })
                    && attempt.outcome
                        == CapturedFcntlOutcome::Returned {
                            value: 0,
                            result_identity: None,
                            result_mount_id: None,
                        }
            })
            .unwrap();
        assert!(process.fcntl_attempts.iter().any(|attempt| {
            attempt.request.command == libc::F_SETLKW as u32
                && attempt.request.record_lock
                    == Some(CapturedFcntlRecordLock {
                        lock_type: libc::F_UNLCK as i16,
                        whence: libc::SEEK_SET as i16,
                        start: 0,
                        length: 0,
                    })
                && attempt.outcome
                    == CapturedFcntlOutcome::Returned {
                        value: 0,
                        result_identity: None,
                        result_mount_id: None,
                    }
        }));
        assert!(process.fcntl_attempts.iter().any(|attempt| {
            attempt.request.command == libc::F_SETLKW as u32
                && attempt.request.argument == 0
                && attempt.request.record_lock.is_none()
                && attempt.outcome == CapturedFcntlOutcome::Failed(libc::EFAULT)
        }));
        assert!(process.fcntl_attempts.iter().any(|attempt| {
            attempt.request.descriptor == -1
                && attempt.request.identity.is_none()
                && attempt.outcome == CapturedFcntlOutcome::Failed(libc::EBADF)
        }));

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["fcntl_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());

        let mut missing_result = serde_json::to_value(&receipt).unwrap();
        missing_result["processes"][0]["fcntl_attempts"][duplicate_index]["outcome"]["returned"]
            ["result_identity"] = serde_json::Value::Null;
        missing_result["processes"][0]["fcntl_attempts"][duplicate_index]["outcome"]["returned"]
            ["result_mount_id"] = serde_json::Value::Null;
        let missing_result: ProcessCaptureReceipt = serde_json::from_value(missing_result).unwrap();
        assert!(missing_result.validate_complete().is_err());

        let mut below_minimum = serde_json::to_value(&receipt).unwrap();
        below_minimum["processes"][0]["fcntl_attempts"][cloexec_index]["outcome"]["returned"]
            ["value"] = serde_json::json!(19);
        let below_minimum: ProcessCaptureReceipt =
            serde_json::from_value(below_minimum).unwrap();
        assert!(below_minimum.validate_complete().is_err());

        let mut source_collision = serde_json::to_value(&receipt).unwrap();
        let source_descriptor =
            source_collision["processes"][0]["fcntl_attempts"][cloexec_index]["request"]
                ["descriptor"]
                .clone();
        source_collision["processes"][0]["fcntl_attempts"][cloexec_index]["request"]
            ["argument"] = serde_json::json!(0);
        source_collision["processes"][0]["fcntl_attempts"][cloexec_index]["outcome"]
            ["returned"]["value"] = source_descriptor;
        let source_collision: ProcessCaptureReceipt =
            serde_json::from_value(source_collision).unwrap();
        assert!(source_collision.validate_complete().is_err());

        let mut missing_lock = serde_json::to_value(&receipt).unwrap();
        missing_lock["processes"][0]["fcntl_attempts"][record_lock_index]["request"]
            ["record_lock"] = serde_json::Value::Null;
        let missing_lock: ProcessCaptureReceipt = serde_json::from_value(missing_lock).unwrap();
        assert!(missing_lock.validate_complete().is_err());

        let mut invalid_lock = serde_json::to_value(&receipt).unwrap();
        invalid_lock["processes"][0]["fcntl_attempts"][record_lock_index]["request"]
            ["record_lock"]["lock_type"] = serde_json::json!(i16::MAX);
        let invalid_lock: ProcessCaptureReceipt = serde_json::from_value(invalid_lock).unwrap();
        assert!(invalid_lock.validate_complete().is_err());
    }

    #[test]
    fn captures_terminal_probe_failures_and_descriptor_identity() {
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, errno, os, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "number, tcgets, tiocgwinsz = map(int, sys.argv[1:])\n",
                "null = os.open('/dev/null', os.O_RDONLY)\n",
                "read_end, write_end = os.pipe()\n",
                "buf = ctypes.create_string_buffer(64)\n",
                "for fd, request in ((null, tcgets), (read_end, tcgets), ",
                "(read_end, tiocgwinsz)):\n",
                "    ctypes.set_errno(0)\n",
                "    assert libc.syscall(number, fd, request, buf) == -1\n",
                "    assert ctypes.get_errno() == errno.ENOTTY\n",
                "os.close(null)\n",
                "os.close(read_end)\n",
                "os.close(write_end)\n",
            ))
            .arg(libc::SYS_ioctl.to_string())
            .arg(libc::TCGETS.to_string())
            .arg(libc::TIOCGWINSZ.to_string())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(receipt.ioctl_syscalls().unwrap(), process.ioctl_syscalls);
        assert!(process.ioctl_attempts.iter().any(|attempt| {
            attempt.request.identity.kind == CapturedDescriptorKind::CharacterDevice
                && attempt.request.identity.target == b"/dev/null"
                && attempt.request.command == libc::TCGETS as u64
                && attempt.outcome == CapturedIoctlOutcome::Failed(libc::ENOTTY)
        }));
        for command in [libc::TCGETS as u64, libc::TIOCGWINSZ as u64] {
            assert!(process.ioctl_attempts.iter().any(|attempt| {
                attempt.request.identity.kind == CapturedDescriptorKind::Fifo
                    && attempt.request.command == command
                    && attempt.outcome == CapturedIoctlOutcome::Failed(libc::ENOTTY)
            }));
        }
    }

    #[test]
    fn captures_pipe_descriptors_failures_and_denominator() {
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, errno, os, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "number = int(sys.argv[1])\n",
                "pair = (ctypes.c_int * 2)()\n",
                "assert libc.syscall(number, ctypes.byref(pair), os.O_CLOEXEC) == 0\n",
                "os.close(pair[0])\n",
                "os.close(pair[1])\n",
                "ctypes.set_errno(0)\n",
                "assert libc.syscall(number, 0, 0) == -1\n",
                "assert ctypes.get_errno() == errno.EFAULT\n",
            ))
            .arg(libc::SYS_pipe2.to_string())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(receipt.pipe_syscalls().unwrap(), process.pipe_syscalls);
        assert_eq!(
            usize::try_from(process.pipe_syscalls).unwrap(),
            process.pipe_attempts.len()
        );
        let created_index = process
            .pipe_attempts
            .iter()
            .position(|attempt| {
                attempt.request.flags == libc::O_CLOEXEC as u32
                    && matches!(
                        &attempt.outcome,
                        CapturedPipeOutcome::Created {
                            read_descriptor,
                            read_identity,
                            read_mount_id,
                            write_descriptor,
                            write_identity,
                            write_mount_id,
                        } if *read_descriptor >= 0
                            && *write_descriptor >= 0
                            && read_descriptor != write_descriptor
                            && read_identity.kind == CapturedDescriptorKind::Fifo
                            && read_identity == write_identity
                            && read_mount_id == write_mount_id
                    )
            })
            .unwrap();
        assert!(process.pipe_attempts.iter().any(|attempt| {
            attempt.request.flags == 0
                && attempt.outcome == CapturedPipeOutcome::Failed(libc::EFAULT)
        }));

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["pipe_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());

        let mut mismatched = serde_json::to_value(&receipt).unwrap();
        mismatched["processes"][0]["pipe_attempts"][created_index]["outcome"]["created"]
            ["write_identity"]["inode"] = serde_json::json!(u64::MAX);
        let mismatched: ProcessCaptureReceipt = serde_json::from_value(mismatched).unwrap();
        assert!(mismatched.validate_complete().is_err());

        let mut unsupported = serde_json::to_value(&receipt).unwrap();
        unsupported["processes"][0]["pipe_attempts"][created_index]["request"]["flags"] =
            serde_json::json!(u32::MAX);
        let unsupported: ProcessCaptureReceipt = serde_json::from_value(unsupported).unwrap();
        assert!(unsupported.validate_complete().is_err());
    }

    #[test]
    fn captures_event_poll_descriptors_and_denominator() {
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, errno, os, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "number, cloexec = map(int, sys.argv[1:])\n",
                "for flags in (0, cloexec):\n",
                "    descriptor = libc.syscall(number, flags)\n",
                "    assert descriptor >= 0\n",
                "    os.close(descriptor)\n",
                "descriptor = libc.syscall(number, ctypes.c_ulonglong(1 << 32))\n",
                "assert descriptor >= 0\n",
                "os.close(descriptor)\n",
                "ctypes.set_errno(0)\n",
                "assert libc.syscall(number, 1) == -1\n",
                "assert ctypes.get_errno() == errno.EINVAL\n",
            ))
            .arg(libc::SYS_epoll_create1.to_string())
            .arg(libc::EPOLL_CLOEXEC.to_string())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(
            receipt.epoll_create_syscalls().unwrap(),
            process.epoll_create_syscalls
        );
        assert_eq!(process.epoll_create_attempts.len(), 2);
        for flags in [0, libc::EPOLL_CLOEXEC as u32] {
            assert!(process.epoll_create_attempts.iter().any(|attempt| {
                attempt.request.flags == flags
                    && matches!(
                        &attempt.outcome,
                        CapturedEpollCreateOutcome::Created {
                            descriptor,
                            identity,
                            ..
                        } if *descriptor >= 0
                            && identity.target == b"anon_inode:[eventpoll]"
                            && identity.kind == CapturedDescriptorKind::Other
                    )
            }));
        }
        assert_eq!(
            process
                .unmodeled_syscall_attempts
                .iter()
                .filter(|attempt| attempt.number == libc::SYS_epoll_create1 as u64)
                .count(),
            2
        );
        assert!(process.unmodeled_syscall_attempts.iter().any(|attempt| {
            attempt.number == libc::SYS_epoll_create1 as u64
                && attempt.arguments[0] == 1_u64 << 32
                && matches!(
                    attempt.outcome,
                    CapturedUnmodeledSyscallOutcome::Returned {
                        return_value,
                        is_error: false,
                    } if return_value >= 0
                )
        }));
        assert!(process.unmodeled_syscall_attempts.iter().any(|attempt| {
            attempt.number == libc::SYS_epoll_create1 as u64
                && attempt.arguments[0] == 1
                && attempt.outcome
                    == CapturedUnmodeledSyscallOutcome::Returned {
                        return_value: -i64::from(libc::EINVAL),
                        is_error: true,
                    }
        }));

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["epoll_create_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());
    }

    #[test]
    fn captures_local_stream_socket_failures_and_denominators() {
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, errno, os, struct, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "socket_number, connect_number, flags = ",
                "map(int, sys.argv[1:])\n",
                "address = ctypes.create_string_buffer(110)\n",
                "path = b'/definitely-missing-rush-socket\\0'\n",
                "payload = struct.pack('H', 1) + path\n",
                "ctypes.memmove(address, payload, len(payload))\n",
                "for length in (110, (1 << 32) | 110):\n",
                "    fd = libc.syscall(socket_number, 1, 1 | flags, 0)\n",
                "    assert fd >= 0\n",
                "    ctypes.set_errno(0)\n",
                "    result = libc.syscall(connect_number, fd, ",
                "ctypes.c_void_p(ctypes.addressof(address)), ",
                "ctypes.c_ulonglong(length))\n",
                "    assert result == -1 and ",
                "ctypes.get_errno() == errno.ENOENT\n",
                "    os.close(fd)\n",
                "fd = libc.syscall(socket_number, 1, ",
                "ctypes.c_ulonglong((1 << 32) | 1 | flags), 0)\n",
                "assert fd >= 0\n",
                "os.close(fd)\n",
            ))
            .arg(libc::SYS_socket.to_string())
            .arg(libc::SYS_connect.to_string())
            .arg((libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK).to_string())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(
            usize::try_from(receipt.socket_create_syscalls().unwrap()).unwrap(),
            process.socket_create_attempts.len()
        );
        assert_eq!(
            usize::try_from(receipt.socket_connect_syscalls().unwrap())
                .unwrap(),
            process.socket_connect_attempts.len()
        );
        assert!(process.socket_create_attempts.len() >= 2);
        assert!(!process.socket_connect_attempts.is_empty());
        for attempt in &process.socket_create_attempts {
            assert_eq!(attempt.request.domain, libc::AF_UNIX);
            assert_eq!(attempt.request.protocol, 0);
            assert!(matches!(
                &attempt.outcome,
                CapturedSocketCreateOutcome::Created {
                    descriptor,
                    identity,
                    mount_id,
                } if *descriptor >= 0
                    && identity.kind == CapturedDescriptorKind::Socket
                    && identity.target.starts_with(b"socket:[")
                    && *mount_id > 0
            ));
        }
        let connect = process
            .socket_connect_attempts
            .iter()
            .find(|attempt| {
                matches!(
                    &attempt.request.address,
                    CapturedUnixSocketAddress::Pathname { path, .. }
                        if path == b"/definitely-missing-rush-socket"
                )
            })
            .unwrap();
        assert_eq!(connect.request.address_length, 110);
        assert!(matches!(
            &connect.request.address,
            CapturedUnixSocketAddress::Pathname {
                path,
                terminated: true,
            } if path == b"/definitely-missing-rush-socket"
        ));
        assert!(matches!(
            &connect.outcome,
            CapturedSocketConnectOutcome::Failed {
                errno,
                identity,
                mount_id,
            } if *errno == libc::ENOENT
                && identity == &connect.request.identity
                && *mount_id == connect.request.mount_id
        ));
        assert!(process.unmodeled_syscall_attempts.iter().any(|attempt| {
            attempt.number == libc::SYS_socket as u64
                && attempt.arguments[1]
                    == (1_u64 << 32)
                        | u64::try_from(
                            libc::SOCK_STREAM
                                | libc::SOCK_CLOEXEC
                                | libc::SOCK_NONBLOCK,
                        )
                        .unwrap()
        }));
        assert!(process.unmodeled_syscall_attempts.iter().any(|attempt| {
            attempt.number == libc::SYS_connect as u64
                && attempt.arguments[2] == (1_u64 << 32) | 110
        }));

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["socket_connect_attempts"] =
            serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt =
            serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());
    }

    #[test]
    fn captures_descriptor_duplication_topology_failures_and_denominator() {
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, errno, os, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "dup2_number = int(sys.argv[1])\n",
                "dup_number = int(sys.argv[2])\n",
                "dup3_number = int(sys.argv[3])\n",
                "source, writer = os.pipe()\n",
                "target = os.open('/dev/null', os.O_RDONLY)\n",
                "assert libc.syscall(dup2_number, source, target) == target\n",
                "assert libc.syscall(dup3_number, source, target, 0) == target\n",
                "allocated = libc.syscall(dup_number, source)\n",
                "assert allocated >= 0 and allocated != source\n",
                "os.close(allocated)\n",
                "ctypes.set_errno(0)\n",
                "assert libc.syscall(dup2_number, -1, target) == -1\n",
                "assert ctypes.get_errno() == errno.EBADF\n",
                "ctypes.set_errno(0)\n",
                "assert libc.syscall(dup3_number, -1, target, 0) == -1\n",
                "assert ctypes.get_errno() == errno.EBADF\n",
                "assert libc.syscall(dup3_number, source, target, os.O_CLOEXEC) == target\n",
                "ctypes.set_errno(0)\n",
                "assert libc.syscall(dup_number, -1) == -1\n",
                "assert ctypes.get_errno() == errno.EBADF\n",
                "os.close(source)\n",
                "os.close(writer)\n",
                "os.close(target)\n",
            ))
            .arg(libc::SYS_dup2.to_string())
            .arg(libc::SYS_dup.to_string())
            .arg(libc::SYS_dup3.to_string())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(receipt.dup2_syscalls().unwrap(), process.dup2_syscalls);
        assert_eq!(
            usize::try_from(process.dup2_syscalls).unwrap(),
            process.dup2_attempts.len()
        );
        let duplicated_index = process
            .dup2_attempts
            .iter()
            .position(|attempt| {
                matches!(
                    &attempt.outcome,
                    CapturedDup2Outcome::Duplicated {
                        descriptor,
                        identity,
                        mount_id,
                        ..
                    } if attempt.request.system_call
                            == CapturedDescriptorDuplicationSystemCall::Dup2
                        && attempt.request.target_descriptor == *descriptor
                        && attempt.request.source_descriptor >= 0
                        && attempt.request.target_descriptor >= 0
                        && attempt.request.source_identity.as_ref() == Some(identity)
                        && attempt.request.source_mount_id == Some(*mount_id)
                        && identity.kind == CapturedDescriptorKind::Fifo
                        && attempt.request.target_identity.as_ref().is_some_and(|prior| {
                            prior.kind == CapturedDescriptorKind::CharacterDevice
                        })
                )
            })
            .unwrap();
        assert!(process.dup2_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedDescriptorDuplicationSystemCall::Dup
                && attempt.request.target_descriptor >= 0
                && attempt.request.target_identity.is_none()
                && attempt.request.target_mount_id.is_none()
                && matches!(
                    &attempt.outcome,
                    CapturedDup2Outcome::Duplicated {
                        descriptor,
                        identity,
                        mount_id,
                        ..
                    } if *descriptor == attempt.request.target_descriptor
                        && *descriptor != attempt.request.source_descriptor
                        && attempt.request.source_identity.as_ref() == Some(identity)
                        && attempt.request.source_mount_id == Some(*mount_id)
                )
        }));
        assert!(process.dup2_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedDescriptorDuplicationSystemCall::Dup3
                && attempt.request.source_descriptor != attempt.request.target_descriptor
                && matches!(
                    &attempt.outcome,
                    CapturedDup2Outcome::Duplicated {
                        descriptor,
                        identity,
                        mount_id,
                        ..
                    } if *descriptor == attempt.request.target_descriptor
                        && attempt.request.source_identity.as_ref() == Some(identity)
                        && attempt.request.source_mount_id == Some(*mount_id)
                )
        }));
        for system_call in [
            CapturedDescriptorDuplicationSystemCall::Dup,
            CapturedDescriptorDuplicationSystemCall::Dup2,
            CapturedDescriptorDuplicationSystemCall::Dup3,
        ] {
            assert!(process.dup2_attempts.iter().any(|attempt| {
                attempt.request.system_call == system_call
                    && attempt.request.source_descriptor == -1
                    && attempt.outcome == CapturedDup2Outcome::Failed(libc::EBADF)
            }));
        }
        assert!(process.unmodeled_syscall_attempts.iter().any(|attempt| {
            attempt.number == libc::SYS_dup3 as u64
                && attempt.arguments[2] == libc::O_CLOEXEC as u64
        }));

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["dup2_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());

        let mut mismatched = serde_json::to_value(&receipt).unwrap();
        mismatched["processes"][0]["dup2_attempts"][duplicated_index]["outcome"]["duplicated"]
            ["identity"]["inode"] = serde_json::json!(u64::MAX);
        let mismatched: ProcessCaptureReceipt = serde_json::from_value(mismatched).unwrap();
        assert!(mismatched.validate_complete().is_err());

        let mut mismatched_request = serde_json::to_value(&receipt).unwrap();
        mismatched_request["processes"][0]["dup2_attempts"][duplicated_index]["request"]
            ["system_call"] = serde_json::json!("dup");
        let mismatched_request: ProcessCaptureReceipt =
            serde_json::from_value(mismatched_request).unwrap();
        assert!(mismatched_request.validate_complete().is_err());
    }

    #[test]
    fn captures_regular_file_value_at_descriptor_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("dup2-replaced-value");
        let final_contents = b"final replacement value\n";
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, errno, os, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "number = int(sys.argv[1])\n",
                "path = os.fsencode(sys.argv[2])\n",
                "target = 50\n",
                "output = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)\n",
                "assert libc.syscall(number, output, target) == target\n",
                "os.close(output)\n",
                "assert os.write(target, b'final replacement value\\n') == 24\n",
                "ctypes.set_errno(0)\n",
                "assert libc.syscall(number, -1, target) == -1\n",
                "assert ctypes.get_errno() == errno.EBADF\n",
                "assert libc.syscall(number, target, target) == target\n",
                "source = os.open('/dev/null', os.O_WRONLY)\n",
                "assert libc.syscall(number, source, target) == target\n",
                "os.close(source)\n",
                "os.close(target)\n",
            ))
            .arg(libc::SYS_dup2.to_string())
            .arg(&path)
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        let replacement_index = process
            .dup2_attempts
            .iter()
            .position(|attempt| {
                attempt.request.source_descriptor != attempt.request.target_descriptor
                    && attempt
                        .request
                        .target_identity
                        .as_ref()
                        .is_some_and(|identity| identity.target == path.as_os_str().as_bytes())
                    && matches!(
                        attempt.outcome,
                        CapturedDup2Outcome::Duplicated {
                            replaced_file_value: Some(_),
                            ..
                        }
                    )
            })
            .unwrap();
        let CapturedDup2Outcome::Duplicated {
            replaced_file_value:
                Some(CapturedMappedFileValue::WholeFile {
                    bytes,
                    digest,
                    mode,
                }),
            ..
        } = &process.dup2_attempts[replacement_index].outcome
        else {
            unreachable!();
        };
        assert_eq!(*bytes, u64::try_from(final_contents.len()).unwrap());
        assert_eq!(*digest, *blake3::hash(final_contents).as_bytes());
        assert_eq!(*mode & libc::S_IFMT, libc::S_IFREG);
        assert!(process.dup2_attempts.iter().any(|attempt| {
            attempt.request.source_descriptor == -1
                && attempt.request.target_descriptor == 50
                && attempt.outcome == CapturedDup2Outcome::Failed(libc::EBADF)
        }));
        assert!(process.dup2_attempts.iter().any(|attempt| {
            attempt.request.source_descriptor == attempt.request.target_descriptor
                && matches!(
                    &attempt.outcome,
                    CapturedDup2Outcome::Duplicated {
                        replaced_file_value: None,
                        ..
                    }
                )
        }));

        let mut missing = serde_json::to_value(&receipt).unwrap();
        missing["processes"][0]["dup2_attempts"][replacement_index]["outcome"]["duplicated"]
            ["replaced_file_value"] = serde_json::Value::Null;
        let missing: ProcessCaptureReceipt = serde_json::from_value(missing).unwrap();
        assert!(missing.validate_complete().is_err());

        let mut unwitnessed = serde_json::to_value(&receipt).unwrap();
        unwitnessed["processes"][0]["dup2_attempts"][replacement_index]["outcome"]["duplicated"]
            ["replaced_file_value"] = serde_json::json!("unwitnessed");
        let unwitnessed: ProcessCaptureReceipt = serde_json::from_value(unwitnessed).unwrap();
        assert!(unwitnessed.validate_complete().is_err());

        let round_trip: ProcessCaptureReceipt =
            serde_json::from_value(serde_json::to_value(&receipt).unwrap()).unwrap();
        round_trip.validate_complete().unwrap();
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn captures_signal_state_values_failures_and_denominator() {
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, errno, signal, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "action, mask = map(int, sys.argv[1:3])\n",
                "old_action = (ctypes.c_ubyte * 32)()\n",
                "assert libc.syscall(action, signal.SIGUSR1, 0, ",
                "ctypes.byref(old_action), 8) == 0\n",
                "new_action = (ctypes.c_ulong * 4)(1, 0, 0, 0)\n",
                "saved_action = (ctypes.c_ubyte * 32)()\n",
                "assert libc.syscall(action, signal.SIGUSR2, ",
                "ctypes.byref(new_action), ctypes.byref(saved_action), 8) == 0\n",
                "assert libc.syscall(action, signal.SIGUSR2, ",
                "ctypes.byref(saved_action), 0, 8) == 0\n",
                "old_mask = (ctypes.c_ubyte * 8)()\n",
                "assert libc.syscall(mask, signal.SIG_SETMASK, 0, ",
                "ctypes.byref(old_mask), 8) == 0\n",
                "new_mask = (ctypes.c_ubyte * 8)()\n",
                "new_mask[(signal.SIGUSR1 - 1) // 8] = ",
                "1 << ((signal.SIGUSR1 - 1) % 8)\n",
                "saved_mask = (ctypes.c_ubyte * 8)()\n",
                "assert libc.syscall(mask, signal.SIG_BLOCK, ",
                "ctypes.byref(new_mask), ctypes.byref(saved_mask), 8) == 0\n",
                "assert libc.syscall(mask, signal.SIG_SETMASK, ",
                "ctypes.byref(saved_mask), 0, 8) == 0\n",
                "ctypes.set_errno(0)\n",
                "assert libc.syscall(action, 0, 0, 0, 8) == -1\n",
                "assert ctypes.get_errno() == errno.EINVAL\n",
            ))
            .arg(libc::SYS_rt_sigaction.to_string())
            .arg(libc::SYS_rt_sigprocmask.to_string())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(
            receipt.signal_state_syscalls().unwrap(),
            process.signal_state_syscalls
        );
        assert_eq!(
            usize::try_from(process.signal_state_syscalls).unwrap(),
            process.signal_state_attempts.len()
        );
        let action_query = process
            .signal_state_attempts
            .iter()
            .position(|attempt| {
                matches!(
                    &attempt.request,
                    CapturedSignalStateRequest::Action {
                        signal,
                        action: None,
                        old_action_is_null: false,
                        signal_set_bytes: 8,
                    } if *signal == libc::SIGUSR1 as u32
                ) && matches!(
                    &attempt.outcome,
                    CapturedSignalStateOutcome::ActionReturned {
                        previous: Some(previous),
                    } if previous.len() == 32
                )
            })
            .unwrap();
        assert!(process.signal_state_attempts.iter().any(|attempt| {
            matches!(
                &attempt.request,
                CapturedSignalStateRequest::Action {
                    signal,
                    action: Some(action),
                    signal_set_bytes: 8,
                    ..
                } if *signal == libc::SIGUSR2 as u32 && action.len() == 32
            ) && matches!(
                &attempt.outcome,
                CapturedSignalStateOutcome::ActionReturned {
                    previous: Some(previous),
                } if previous.len() == 32
            )
        }));
        assert!(process.signal_state_attempts.iter().any(|attempt| {
            matches!(
                &attempt.request,
                CapturedSignalStateRequest::Mask {
                    operation,
                    mask: None,
                    old_mask_is_null: false,
                    signal_set_bytes: 8,
                } if *operation == libc::SIG_SETMASK
            ) && matches!(
                &attempt.outcome,
                CapturedSignalStateOutcome::MaskReturned {
                    previous: Some(previous),
                } if previous.len() == 8
            )
        }));
        assert!(process.signal_state_attempts.iter().any(|attempt| {
            matches!(
                &attempt.request,
                CapturedSignalStateRequest::Mask {
                    operation,
                    mask: Some(mask),
                    signal_set_bytes: 8,
                    ..
                } if *operation == libc::SIG_BLOCK && mask.len() == 8
            ) && matches!(
                &attempt.outcome,
                CapturedSignalStateOutcome::MaskReturned {
                    previous: Some(previous),
                } if previous.len() == 8
            )
        }));
        assert!(process.signal_state_attempts.iter().any(|attempt| {
            matches!(
                attempt.request,
                CapturedSignalStateRequest::Action { signal: 0, .. }
            ) && attempt.outcome == CapturedSignalStateOutcome::Failed(libc::EINVAL)
        }));
        assert!(!process.unmodeled_syscall_attempts.iter().any(|attempt| {
            attempt.number == libc::SYS_rt_sigaction as u64
                || attempt.number == libc::SYS_rt_sigprocmask as u64
        }));

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["signal_state_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());

        let mut wrong_previous = serde_json::to_value(&receipt).unwrap();
        wrong_previous["processes"][0]["signal_state_attempts"][action_query]["outcome"]
            ["action_returned"]["previous"] = serde_json::to_value(vec![0; 31]).unwrap();
        let wrong_previous: ProcessCaptureReceipt = serde_json::from_value(wrong_previous).unwrap();
        assert!(wrong_previous.validate_complete().is_err());
    }

    #[test]
    fn captures_memory_state_results_and_denominator() {
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, errno, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "brk, mprotect, munmap = map(int, sys.argv[1:4])\n",
                "assert libc.syscall(brk, ctypes.c_void_p(0)) > 0\n",
                "ctypes.set_errno(0)\n",
                "assert libc.syscall(mprotect, ctypes.c_void_p(1), 4096, 0) == -1\n",
                "assert ctypes.get_errno() == errno.EINVAL\n",
                "ctypes.set_errno(0)\n",
                "assert libc.syscall(munmap, ctypes.c_void_p(0), 0) == -1\n",
                "assert ctypes.get_errno() == errno.EINVAL\n",
                "assert libc.syscall(munmap, ctypes.c_void_p(0), 4096) == 0\n",
            ))
            .arg(libc::SYS_brk.to_string())
            .arg(libc::SYS_mprotect.to_string())
            .arg(libc::SYS_munmap.to_string())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(
            receipt.memory_state_syscalls().unwrap(),
            process.memory_state_syscalls
        );
        assert_eq!(
            usize::try_from(process.memory_state_syscalls).unwrap(),
            process.memory_state_attempts.len()
        );
        assert!(process.memory_state_attempts.iter().any(|attempt| {
            matches!(attempt.request, CapturedMemoryStateRequest::Brk { address: 0 })
                && matches!(attempt.outcome, CapturedMemoryStateOutcome::Break(address) if address != 0)
        }));
        let mprotect_index = process
            .memory_state_attempts
            .iter()
            .position(|attempt| {
                matches!(attempt.request, CapturedMemoryStateRequest::Mprotect { .. })
                    && attempt.outcome == CapturedMemoryStateOutcome::Completed
            })
            .unwrap();
        assert!(process.memory_state_attempts.iter().any(|attempt| {
            matches!(
                attempt.request,
                CapturedMemoryStateRequest::Mprotect { address: 1, .. }
            ) && attempt.outcome == CapturedMemoryStateOutcome::Failed(libc::EINVAL)
        }));
        assert!(process.memory_state_attempts.iter().any(|attempt| {
            matches!(
                attempt.request,
                CapturedMemoryStateRequest::Munmap { length: 4096, .. }
            ) && attempt.outcome == CapturedMemoryStateOutcome::Completed
        }));
        assert!(process.memory_state_attempts.iter().any(|attempt| {
            matches!(
                attempt.request,
                CapturedMemoryStateRequest::Munmap { length: 0, .. }
            ) && attempt.outcome == CapturedMemoryStateOutcome::Failed(libc::EINVAL)
        }));

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["memory_state_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());

        let mut invalid_protection = serde_json::to_value(&receipt).unwrap();
        invalid_protection["processes"][0]["memory_state_attempts"][mprotect_index]["request"]
            ["mprotect"]["protection"] = serde_json::json!(u32::MAX);
        let invalid_protection: ProcessCaptureReceipt =
            serde_json::from_value(invalid_protection).unwrap();
        assert!(invalid_protection.validate_complete().is_err());

        let mut collision = serde_json::to_value(&receipt).unwrap();
        collision["processes"][0]["memory_state_attempts"][mprotect_index]["syscall_index"] =
            serde_json::json!(process.open_attempts[0].syscall_index);
        let collision: ProcessCaptureReceipt = serde_json::from_value(collision).unwrap();
        assert!(collision.validate_complete().is_err());

        let mut terminal = serde_json::to_value(&receipt).unwrap();
        terminal["processes"][0]["memory_state_attempts"][mprotect_index]["outcome"] =
            serde_json::json!("terminated");
        terminal["processes"][0]["syscall_exits"] = serde_json::json!(process.syscall_exits - 1);
        terminal["processes"][0]["terminal_syscalls"] =
            serde_json::json!(process.terminal_syscalls + 1);
        let terminal: ProcessCaptureReceipt = serde_json::from_value(terminal).unwrap();
        terminal.validate_complete().unwrap();
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn captures_signal_returns_without_restored_child_assumptions() {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("/bin/true; /bin/true; /bin/true")
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let shell = receipt.processes().first().unwrap();
        let returns = shell
            .thread_state_attempts
            .iter()
            .filter_map(|attempt| match (attempt.request, attempt.outcome) {
                (
                    CapturedThreadStateRequest::SignalReturn {
                        delivery,
                        restored_accumulator,
                    },
                    CapturedThreadStateOutcome::SignalContextRestored,
                ) => Some((delivery, restored_accumulator)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(!returns.is_empty());
        for (delivery, _) in &returns {
            let delivery = shell.signal_deliveries
                [usize::try_from(*delivery).unwrap()];
            assert_eq!(delivery.disposition, CapturedSignalDisposition::Caught);
            assert!(matches!(
                delivery.source,
                CapturedSignalSource::TracedProcess { .. }
            ));
        }
        assert!(!shell.unmodeled_syscall_attempts.iter().any(|attempt| {
            attempt.number == libc::SYS_rt_sigreturn as u64
        }));

        let signal_return_index = shell
            .thread_state_attempts
            .iter()
            .position(|attempt| {
                matches!(
                    attempt.request,
                    CapturedThreadStateRequest::SignalReturn { .. }
                )
            })
            .unwrap();
        let mut wrong_delivery = serde_json::to_value(&receipt).unwrap();
        wrong_delivery["processes"][0]["thread_state_attempts"]
            [signal_return_index]["request"]["signal_return"]["delivery"] =
            serde_json::json!(u64::MAX);
        let wrong_delivery: ProcessCaptureReceipt =
            serde_json::from_value(wrong_delivery).unwrap();
        assert!(wrong_delivery.validate_complete().is_err());
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn captures_zero_accumulator_signal_returns() {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("trap ':' USR1; kill -USR1 $$")
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let returns = receipt
            .processes()
            .iter()
            .flat_map(|process| &process.thread_state_attempts)
            .filter_map(|attempt| match (attempt.request, attempt.outcome) {
                (
                    CapturedThreadStateRequest::SignalReturn {
                        restored_accumulator,
                        ..
                    },
                    CapturedThreadStateOutcome::SignalContextRestored,
                ) => Some(restored_accumulator),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(returns.contains(&0));
        assert!(receipt.processes().iter().all(|process| {
            process.unmodeled_syscall_attempts.iter().all(|attempt| {
                attempt.number != libc::SYS_rt_sigreturn as u64
            })
        }));
    }

    #[test]
    fn accepts_negative_restored_signal_accumulators() {
        let pending = PendingThreadState {
            syscall_index: 0,
            request: CapturedThreadStateRequest::SignalReturn {
                delivery: 0,
                restored_accumulator: -512,
            },
            restored_return_value: Some(-512),
        };
        assert_eq!(
            thread_state_outcome(
                &pending,
                SyscallExit {
                    return_value: -512,
                    is_error: 1,
                },
            )
            .unwrap(),
            CapturedThreadStateOutcome::SignalContextRestored
        );
        assert!(thread_state_outcome(
            &pending,
            SyscallExit {
                return_value: -511,
                is_error: 1,
            },
        )
        .is_err());
    }

    #[test]
    fn captures_thread_state_results_and_denominator() {
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, errno, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "set_tid, robust, rseq = map(int, sys.argv[1:4])\n",
                "assert libc.syscall(set_tid, ctypes.c_void_p(0)) > 0\n",
                "ctypes.set_errno(0)\n",
                "assert libc.syscall(robust, ctypes.c_void_p(0), 0) == -1\n",
                "assert ctypes.get_errno() == errno.EINVAL\n",
                "ctypes.set_errno(0)\n",
                "assert libc.syscall(rseq, ctypes.c_void_p(0), 0, 0, 0) == -1\n",
                "assert ctypes.get_errno() == errno.EINVAL\n",
                "if len(sys.argv) == 5:\n",
                "    ctypes.set_errno(0)\n",
                "    assert libc.syscall(int(sys.argv[4]), 0xffffffff, 0) == -1\n",
                "    assert ctypes.get_errno() == errno.EINVAL\n",
            ))
            .arg(libc::SYS_set_tid_address.to_string())
            .arg(libc::SYS_set_robust_list.to_string())
            .arg(libc::SYS_rseq.to_string())
            .env_clear();
        #[cfg(target_arch = "x86_64")]
        command.arg(libc::SYS_arch_prctl.to_string());

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(
            receipt.thread_state_syscalls().unwrap(),
            process.thread_state_syscalls
        );
        assert_eq!(
            usize::try_from(process.thread_state_syscalls).unwrap(),
            process.thread_state_attempts.len()
        );
        assert!(process.thread_state_attempts.iter().any(|attempt| {
            matches!(
                attempt.request,
                CapturedThreadStateRequest::SetTidAddress { address: 0 }
            ) && matches!(attempt.outcome, CapturedThreadStateOutcome::ThreadId(thread) if thread > 0)
        }));
        assert!(process.thread_state_attempts.iter().any(|attempt| {
            matches!(
                attempt.request,
                CapturedThreadStateRequest::SetRobustList { length: 0, .. }
            ) && attempt.outcome == CapturedThreadStateOutcome::Failed(libc::EINVAL)
        }));
        let failed_rseq = process
            .thread_state_attempts
            .iter()
            .position(|attempt| {
                matches!(
                    attempt.request,
                    CapturedThreadStateRequest::Rseq { length: 0, .. }
                ) && attempt.outcome == CapturedThreadStateOutcome::Failed(libc::EINVAL)
            })
            .unwrap();
        #[cfg(target_arch = "x86_64")]
        assert!(process.thread_state_attempts.iter().any(|attempt| {
            matches!(
                attempt.request,
                CapturedThreadStateRequest::ArchPrctl {
                    operation: u32::MAX,
                    ..
                }
            ) && attempt.outcome == CapturedThreadStateOutcome::Failed(libc::EINVAL)
        }));

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["thread_state_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());

        let mut invalid = serde_json::to_value(&receipt).unwrap();
        invalid["processes"][0]["thread_state_attempts"][failed_rseq]["outcome"] =
            serde_json::json!({"thread_id": 0});
        let invalid: ProcessCaptureReceipt = serde_json::from_value(invalid).unwrap();
        assert!(invalid.validate_complete().is_err());

        let mut collision = serde_json::to_value(&receipt).unwrap();
        collision["processes"][0]["thread_state_attempts"][failed_rseq]["syscall_index"] =
            serde_json::json!(process.open_attempts[0].syscall_index);
        let collision: ProcessCaptureReceipt = serde_json::from_value(collision).unwrap();
        assert!(collision.validate_complete().is_err());

        let mut terminal = serde_json::to_value(&receipt).unwrap();
        terminal["processes"][0]["thread_state_attempts"][failed_rseq]["outcome"] =
            serde_json::json!("terminated");
        terminal["processes"][0]["syscall_exits"] = serde_json::json!(process.syscall_exits - 1);
        terminal["processes"][0]["terminal_syscalls"] =
            serde_json::json!(process.terminal_syscalls + 1);
        let terminal: ProcessCaptureReceipt = serde_json::from_value(terminal).unwrap();
        terminal.validate_complete().unwrap();
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn captures_measured_signal_stack_install() {
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, sys\n",
                "class Stack(ctypes.Structure):\n",
                "    _fields_ = [('sp', ctypes.c_void_p), ",
                "('flags', ctypes.c_int), ('bytes', ctypes.c_size_t)]\n",
                "storage = ctypes.create_string_buffer(16384)\n",
                "stack = Stack(ctypes.addressof(storage), 0, len(storage))\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "assert libc.syscall(int(sys.argv[1]), ctypes.byref(stack), 0) == 0\n",
            ))
            .arg(libc::SYS_sigaltstack.to_string())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes[0];
        assert!(process.thread_state_attempts.iter().any(|attempt| {
            matches!(
                attempt,
                CapturedThreadStateAttempt {
                    request: CapturedThreadStateRequest::SignalStack {
                        address,
                        flags: 0,
                        bytes: 16_384,
                        old_state_is_null: true,
                    },
                    outcome: CapturedThreadStateOutcome::Completed,
                    ..
                } if *address != 0
            )
        }));
        assert!(!process.unmodeled_syscall_attempts.iter().any(|attempt| {
            attempt.number == libc::SYS_sigaltstack as u64
        }));

        let signal_stack = process
            .thread_state_attempts
            .iter()
            .find(|attempt| {
                matches!(attempt.request, CapturedThreadStateRequest::SignalStack { .. })
            })
            .unwrap();
        let mut malformed = *signal_stack;
        malformed.request = CapturedThreadStateRequest::SignalStack {
            address: 0,
            flags: 0,
            bytes: 16_384,
            old_state_is_null: true,
        };
        assert!(crate::validate_captured_thread_state_attempt(&malformed).is_err());
        malformed.request = CapturedThreadStateRequest::SignalStack {
            address: 1,
            flags: libc::SS_DISABLE,
            bytes: 16_384,
            old_state_is_null: true,
        };
        assert!(crate::validate_captured_thread_state_attempt(&malformed).is_err());
        malformed.request = CapturedThreadStateRequest::SignalStack {
            address: 1,
            flags: 0,
            bytes: 32_768,
            old_state_is_null: true,
        };
        assert!(crate::validate_captured_thread_state_attempt(&malformed).is_err());
        malformed.request = CapturedThreadStateRequest::SignalStack {
            address: 1,
            flags: 0,
            bytes: 16_384,
            old_state_is_null: false,
        };
        assert!(crate::validate_captured_thread_state_attempt(&malformed).is_err());
    }

    #[test]
    fn captures_lifecycle_exit_status_and_denominator() {
        let mut command = Command::new("/bin/true");
        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(process.lifecycle_exit_syscalls, 1);
        assert_eq!(process.lifecycle_exit_attempts.len(), 1);
        let attempt = &process.lifecycle_exit_attempts[0];
        assert_eq!(
            attempt.request.system_call,
            CapturedLifecycleExitSystemCall::ExitGroup
        );
        assert_eq!(attempt.request.status, 0);
        assert_eq!(attempt.outcome, CapturedProcessOutcome::Exited(0));
        assert_eq!(process.terminal_syscalls, 1);

        let mut raw_exit = Command::new("/usr/bin/python3");
        raw_exit
            .arg("-c")
            .arg(concat!(
                "import ctypes, sys\n",
                "libc = ctypes.CDLL(None)\n",
                "libc.syscall(int(sys.argv[1]), 0x1234)\n",
                "raise AssertionError('exit returned')\n",
            ))
            .arg(libc::SYS_exit.to_string())
            .env_clear();
        let raw_receipt = capture_process_tree(&mut raw_exit).unwrap();
        assert_syscall_census(&raw_receipt);
        let raw_process = &raw_receipt.processes()[0];
        let raw_attempt = &raw_process.lifecycle_exit_attempts[0];
        assert_eq!(
            raw_attempt.request.system_call,
            CapturedLifecycleExitSystemCall::Exit
        );
        assert_eq!(raw_attempt.request.status, 0x1234);
        assert_eq!(raw_attempt.outcome, CapturedProcessOutcome::Exited(0x34));

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["lifecycle_exit_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());

        let mut wrong_status = serde_json::to_value(&receipt).unwrap();
        wrong_status["processes"][0]["lifecycle_exit_attempts"][0]["request"]["status"] =
            serde_json::json!(1);
        let wrong_status: ProcessCaptureReceipt = serde_json::from_value(wrong_status).unwrap();
        assert!(wrong_status.validate_complete().is_err());

        let mut wrong_outcome = serde_json::to_value(&receipt).unwrap();
        wrong_outcome["processes"][0]["lifecycle_exit_attempts"][0]["outcome"] =
            serde_json::json!({"exited": 1});
        let wrong_outcome: ProcessCaptureReceipt = serde_json::from_value(wrong_outcome).unwrap();
        assert!(wrong_outcome.validate_complete().is_err());

        let mut collision = serde_json::to_value(&receipt).unwrap();
        collision["processes"][0]["lifecycle_exit_attempts"][0]["syscall_index"] =
            serde_json::json!(process.open_attempts[0].syscall_index);
        let collision: ProcessCaptureReceipt = serde_json::from_value(collision).unwrap();
        assert!(collision.validate_complete().is_err());
    }

    #[test]
    fn captures_inherited_open_file_values_at_process_exit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("exit-value");
        std::fs::write(&path, b"old\n").unwrap();
        let final_contents = b"inherited exit value\n";
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import os, sys\n",
                "path = os.fsencode(sys.argv[1])\n",
                "fd = os.open(path, os.O_WRONLY | os.O_TRUNC)\n",
                "pid = os.fork()\n",
                "if pid == 0:\n",
                "    assert os.write(fd, b'inherited exit value\\n') == 21\n",
                "    os._exit(0)\n",
                "os.close(fd)\n",
                "assert os.waitpid(pid, 0)[1] == 0\n",
            ))
            .arg(&path)
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let child = receipt
            .processes()
            .iter()
            .find(|process| process.parent.is_some())
            .unwrap();
        let value = child
            .lifecycle_exit_attempts
            .iter()
            .flat_map(|attempt| &attempt.open_file_values)
            .find(|value| value.identity.target == path.as_os_str().as_bytes())
            .unwrap();
        assert!(whole_file_value_matches(Some(value.value), final_contents));

        let mut malformed = child.lifecycle_exit_attempts[0].clone();
        malformed.open_file_values[0].descriptor = -1;
        assert!(crate::validate_captured_lifecycle_exit_attempt(&malformed, child.outcome).is_err());
    }

    #[test]
    fn captures_getrandom_values_failures_and_denominator() {
        let directory = tempfile::tempdir().unwrap();
        let delivered = directory.path().join("delivered");
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, errno, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "number = int(sys.argv[1])\n",
                "def call(pointer, length, flags):\n",
                "    ctypes.set_errno(0)\n",
                "    value = libc.syscall(number, pointer, ctypes.c_size_t(length), ",
                "ctypes.c_uint(flags))\n",
                "    return value, ctypes.get_errno()\n",
                "buffer = (ctypes.c_ubyte * 32)()\n",
                "assert call(ctypes.byref(buffer), len(buffer), 0)[0] == len(buffer)\n",
                "with open(sys.argv[2], 'wb') as output:\n",
                "    output.write(bytes(buffer))\n",
                "null = ctypes.c_void_p(0)\n",
                "assert call(null, 0, 0) == (0, 0)\n",
                "assert call(null, 1, 0) == (-1, errno.EFAULT)\n",
                "assert call(ctypes.byref(buffer), 1, 1 << 31) == (-1, errno.EINVAL)\n",
            ))
            .arg(libc::SYS_getrandom.to_string())
            .arg(&delivered)
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(
            receipt.getrandom_syscalls().unwrap(),
            process.getrandom_syscalls
        );
        assert_eq!(
            usize::try_from(process.getrandom_syscalls).unwrap(),
            process.getrandom_attempts.len()
        );
        let delivered = std::fs::read(delivered).unwrap();
        let expected_digest = *blake3::hash(&delivered).as_bytes();
        let delivered_index = process
            .getrandom_attempts
            .iter()
            .position(|attempt| {
                attempt.request.requested_bytes == 32
                    && attempt.request.flags == 0
                    && attempt.outcome
                        == (CapturedGetrandomOutcome::Bytes {
                            returned_bytes: 32,
                            digest: expected_digest,
                        })
            })
            .unwrap();
        assert!(
            !process.getrandom_attempts[delivered_index]
                .request
                .output_is_null
        );
        let zero_index = process
            .getrandom_attempts
            .iter()
            .position(|attempt| {
                attempt.request.requested_bytes == 0
                    && attempt.request.output_is_null
                    && attempt.outcome
                        == (CapturedGetrandomOutcome::Bytes {
                            returned_bytes: 0,
                            digest: *blake3::hash(&[]).as_bytes(),
                        })
            })
            .unwrap();
        assert!(process.getrandom_attempts.iter().any(|attempt| {
            attempt.request.requested_bytes == 1
                && attempt.request.output_is_null
                && attempt.outcome == CapturedGetrandomOutcome::Failed(libc::EFAULT)
        }));
        assert!(process.getrandom_attempts.iter().any(|attempt| {
            attempt.request.flags == 1 << 31
                && attempt.outcome == CapturedGetrandomOutcome::Failed(libc::EINVAL)
        }));

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["getrandom_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());

        let mut oversized = serde_json::to_value(&receipt).unwrap();
        oversized["processes"][0]["getrandom_attempts"][delivered_index]["outcome"]["bytes"]
            ["returned_bytes"] = serde_json::json!(33);
        let oversized: ProcessCaptureReceipt = serde_json::from_value(oversized).unwrap();
        assert!(oversized.validate_complete().is_err());

        let mut invalid_zero_digest = serde_json::to_value(&receipt).unwrap();
        invalid_zero_digest["processes"][0]["getrandom_attempts"][zero_index]["outcome"]["bytes"]
            ["digest"][0] = serde_json::json!(1);
        let invalid_zero_digest: ProcessCaptureReceipt =
            serde_json::from_value(invalid_zero_digest).unwrap();
        assert!(invalid_zero_digest.validate_complete().is_err());

        let mut invalid_success_flags = serde_json::to_value(&receipt).unwrap();
        invalid_success_flags["processes"][0]["getrandom_attempts"][delivered_index]["request"]
            ["flags"] = serde_json::json!(1u32 << 31);
        let invalid_success_flags: ProcessCaptureReceipt =
            serde_json::from_value(invalid_success_flags).unwrap();
        assert!(invalid_success_flags.validate_complete().is_err());

        let mut collision = serde_json::to_value(&receipt).unwrap();
        collision["processes"][0]["getrandom_attempts"][delivered_index]["syscall_index"] =
            serde_json::json!(process.open_attempts[0].syscall_index);
        let collision: ProcessCaptureReceipt = serde_json::from_value(collision).unwrap();
        assert!(collision.validate_complete().is_err());
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn captures_runtime_observations_failures_and_denominator() {
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, errno, os, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "getrusage = int(sys.argv[1])\n",
                "sysinfo = int(sys.argv[2])\n",
                "uname = int(sys.argv[3])\n",
                "sched_getaffinity = int(sys.argv[4])\n",
                "usage = ctypes.create_string_buffer(256)\n",
                "information = ctypes.create_string_buffer(256)\n",
                "identity = ctypes.create_string_buffer(512)\n",
                "affinity = ctypes.create_string_buffer(32)\n",
                "assert libc.syscall(getrusage, ctypes.c_long(0), ",
                "ctypes.byref(usage)) == 0\n",
                "assert libc.syscall(sysinfo, ctypes.byref(information)) == 0\n",
                "assert libc.syscall(uname, ctypes.byref(identity)) == 0\n",
                "affinity_bytes = libc.syscall(sched_getaffinity, os.getpid(), 32, ",
                "ctypes.byref(affinity))\n",
                "assert 0 < affinity_bytes <= 32\n",
                "assert any(affinity.raw[:affinity_bytes])\n",
                "ctypes.set_errno(0)\n",
                "assert libc.syscall(getrusage, ctypes.c_long(0), ",
                "ctypes.c_void_p(0)) == -1\n",
                "assert ctypes.get_errno() == errno.EFAULT\n",
                "ctypes.set_errno(0)\n",
                "assert libc.syscall(sysinfo, ctypes.c_void_p(0)) == -1\n",
                "assert ctypes.get_errno() == errno.EFAULT\n",
                "ctypes.set_errno(0)\n",
                "assert libc.syscall(uname, ctypes.c_void_p(0)) == -1\n",
                "assert ctypes.get_errno() == errno.EFAULT\n",
                "ctypes.set_errno(0)\n",
                "assert libc.syscall(sched_getaffinity, os.getpid(), 32, ",
                "ctypes.c_void_p(0)) == -1\n",
                "assert ctypes.get_errno() == errno.EFAULT\n",
                "assert libc.syscall(getrusage, ctypes.c_long(-1), ",
                "ctypes.byref(usage)) == 0\n",
            ))
            .arg(libc::SYS_getrusage.to_string())
            .arg(libc::SYS_sysinfo.to_string())
            .arg(libc::SYS_uname.to_string())
            .arg(libc::SYS_sched_getaffinity.to_string())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(
            receipt.runtime_observation_syscalls().unwrap(),
            process.runtime_observation_syscalls
        );
        assert_eq!(
            usize::try_from(process.runtime_observation_syscalls).unwrap(),
            process.runtime_observation_attempts.len()
        );
        assert!(process.runtime_observation_attempts.iter().any(|attempt| {
            matches!(
                &attempt.outcome,
                CapturedRuntimeObservationOutcome::ResourceUsage(_)
            )
        }));
        assert!(process.runtime_observation_attempts.iter().any(|attempt| {
            matches!(
                (&attempt.request, &attempt.outcome),
                (
                    CapturedRuntimeObservationRequest::CallingThreadAffinity {
                        capacity: 32,
                        output_is_null: false,
                    },
                    CapturedRuntimeObservationOutcome::CpuAffinity(mask),
                ) if !mask.is_empty()
                    && mask.len() <= 32
                    && mask.iter().any(|byte| *byte != 0)
            )
        }));
        assert!(process.runtime_observation_attempts.iter().any(|attempt| {
            matches!(
                &attempt.outcome,
                CapturedRuntimeObservationOutcome::SystemInformation(_)
            )
        }));
        assert!(process.runtime_observation_attempts.iter().any(|attempt| {
            matches!(
                &attempt.outcome,
                CapturedRuntimeObservationOutcome::SystemIdentity(identity)
                    if identity.system_name == b"Linux"
                        && !identity.release.is_empty()
                        && !identity.version.is_empty()
                        && !identity.machine.is_empty()
            )
        }));
        assert!(process.runtime_observation_attempts.iter().any(|attempt| {
            matches!(
                &attempt.request,
                CapturedRuntimeObservationRequest::CallingProcessResourceUsage {
                    output_is_null: true
                }
            ) && attempt.outcome == CapturedRuntimeObservationOutcome::Failed(libc::EFAULT)
        }));
        assert!(process.runtime_observation_attempts.iter().any(|attempt| {
            matches!(
                &attempt.request,
                CapturedRuntimeObservationRequest::CallingThreadAffinity {
                    capacity: 32,
                    output_is_null: true,
                }
            ) && attempt.outcome == CapturedRuntimeObservationOutcome::Failed(libc::EFAULT)
        }));
        assert!(process.runtime_observation_attempts.iter().any(|attempt| {
            matches!(
                &attempt.request,
                CapturedRuntimeObservationRequest::SystemIdentity {
                    output_is_null: true
                }
            ) && attempt.outcome == CapturedRuntimeObservationOutcome::Failed(libc::EFAULT)
        }));
        assert!(process.runtime_observation_attempts.iter().any(|attempt| {
            matches!(
                &attempt.request,
                CapturedRuntimeObservationRequest::SystemInformation {
                    output_is_null: true
                }
            ) && attempt.outcome == CapturedRuntimeObservationOutcome::Failed(libc::EFAULT)
        }));
        assert!(process.unmodeled_syscall_attempts.iter().any(|attempt| {
            attempt.number == libc::SYS_getrusage as u64 && attempt.arguments[0] == u64::MAX
        }));

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["runtime_observation_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());

        let mut collision = serde_json::to_value(&receipt).unwrap();
        collision["processes"][0]["runtime_observation_attempts"][0]["syscall_index"] =
            serde_json::json!(process.open_attempts[0].syscall_index);
        let collision: ProcessCaptureReceipt = serde_json::from_value(collision).unwrap();
        assert!(collision.validate_complete().is_err());
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn captures_measured_xargs_descriptor_readiness_probe() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("output");
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "printf 'x\\n' | /usr/bin/xargs /bin/true >\"$1\" 2>&1",
                "sh",
            ])
            .arg(&output)
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let xargs = receipt
            .processes()
            .iter()
            .find(|process| {
                process
                    .invocations
                    .iter()
                    .any(|invocation| invocation.executable.ends_with(b"/xargs"))
            })
            .unwrap();
        let probes = xargs
            .runtime_observation_attempts
            .iter()
            .filter(|attempt| {
                matches!(
                    &attempt.request,
                    CapturedRuntimeObservationRequest::DescriptorReadiness { .. }
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(probes.len(), 1);
        let CapturedRuntimeObservationRequest::DescriptorReadiness {
            descriptors,
            timeout_milliseconds,
        } = &probes[0].request
        else {
            unreachable!();
        };
        assert_eq!(*timeout_milliseconds, 0);
        assert_eq!(descriptors.len(), 4);
        assert_eq!(
            descriptors
                .iter()
                .map(|descriptor| descriptor.descriptor)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
        assert_eq!(
            descriptors
                .iter()
                .map(|descriptor| descriptor
                    .identity
                    .as_ref()
                    .map(|identity| identity.kind))
                .collect::<Vec<_>>(),
            vec![
                Some(CapturedDescriptorKind::Fifo),
                Some(CapturedDescriptorKind::RegularFile),
                Some(CapturedDescriptorKind::RegularFile),
                None,
            ]
        );
        assert!(descriptors.iter().all(|descriptor| {
            descriptor.requested_events == libc::POLLIN | libc::POLLOUT
                && descriptor.identity.is_some() == descriptor.mount_id.is_some()
        }));
        assert_eq!(
            probes[0].outcome,
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
        assert!(receipt.processes().iter().all(|process| {
            process
                .unmodeled_syscall_attempts
                .iter()
                .all(|attempt| attempt.number != libc::SYS_poll as u64)
        }));
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn captures_rustc_zero_event_descriptor_readiness_probe() {
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, os, sys\n",
                "class Pollfd(ctypes.Structure):\n",
                "    _fields_ = [('fd', ctypes.c_int), ('events', ctypes.c_short), ('revents', ctypes.c_short)]\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "fds = (Pollfd * 3)(Pollfd(0, 0, 0), Pollfd(1, 0, 0), Pollfd(2, 0, 0))\n",
                "assert libc.syscall(int(sys.argv[1]), fds, 3, 0) == 0\n",
                "assert [fd.revents for fd in fds] == [0, 0, 0]\n",
                "read_fd, write_fd = os.pipe()\n",
                "os.dup2(read_fd, 0)\n",
                "os.close(read_fd)\n",
                "os.close(write_fd)\n",
                "fds = (Pollfd * 3)(Pollfd(0, 0, 0), Pollfd(1, 0, 0), Pollfd(2, 0, 0))\n",
                "assert libc.syscall(int(sys.argv[1]), fds, 3, 0) == 1\n",
                "assert [fd.revents for fd in fds] == [16, 0, 0]\n",
            ))
            .arg(libc::SYS_poll.to_string())
            .env_clear()
            .stdin(std::process::Stdio::from(
                std::fs::File::open("/dev/null").unwrap(),
            ))
            .stdout(std::process::Stdio::from(
                std::fs::File::options()
                    .write(true)
                    .open("/dev/null")
                    .unwrap(),
            ))
            .stderr(std::process::Stdio::from(
                std::fs::File::options()
                    .write(true)
                    .open("/dev/null")
                    .unwrap(),
            ));

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        assert!(receipt.processes().iter().any(|process| {
            process.runtime_observation_attempts.iter().any(|attempt| {
                matches!(
                    &attempt.request,
                    CapturedRuntimeObservationRequest::DescriptorReadiness {
                        descriptors,
                        timeout_milliseconds: 0,
                    } if descriptors.len() == 3
                        && descriptors.iter().enumerate().all(|(index, descriptor)| {
                            descriptor.descriptor == index as i32
                                && descriptor.requested_events == 0
                                && descriptor.identity.is_some()
                                && descriptor.mount_id.is_some()
                        })
                ) && attempt.outcome
                    == (CapturedRuntimeObservationOutcome::DescriptorReadiness {
                        ready: 0,
                        returned_events: vec![0, 0, 0],
                    })
            })
        }));
        assert!(receipt.processes().iter().any(|process| {
            process.runtime_observation_attempts.iter().any(|attempt| {
                matches!(
                    &attempt.request,
                    CapturedRuntimeObservationRequest::DescriptorReadiness {
                        descriptors,
                        timeout_milliseconds: 0,
                    } if descriptors.len() == 3
                        && descriptors[0].identity.as_ref().is_some_and(|identity| {
                            identity.kind == CapturedDescriptorKind::Fifo
                        })
                ) && attempt.outcome
                    == (CapturedRuntimeObservationOutcome::DescriptorReadiness {
                        ready: 1,
                        returned_events: vec![libc::POLLHUP, 0, 0],
                    })
            })
        }));
        assert!(receipt.processes().iter().all(|process| {
            process
                .unmodeled_syscall_attempts
                .iter()
                .all(|attempt| attempt.number != libc::SYS_poll as u64)
        }));
    }

    #[test]
    fn captures_fstat_identity_metadata_failures_and_denominator() {
        let directory = tempfile::tempdir().unwrap();
        let regular = directory.path().join("regular");
        std::fs::write(&regular, b"metadata\n").unwrap();
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, os, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "number = int(sys.argv[2])\n",
                "fd = os.open(sys.argv[1], os.O_RDONLY)\n",
                "def call(descriptor, output):\n",
                "    pointer = ctypes.c_void_p(0) if output is None ",
                "else ctypes.byref(output)\n",
                "    ctypes.set_errno(0)\n",
                "    value = libc.syscall(number, ctypes.c_int(descriptor), pointer)\n",
                "    return value, ctypes.get_errno()\n",
                "result = ctypes.create_string_buffer(512)\n",
                "assert call(fd, result)[0] == 0\n",
                "assert call(-1, result) == (-1, 9)\n",
                "assert call(fd, None) == (-1, 14)\n",
                "os.close(fd)\n",
            ))
            .arg(&regular)
            .arg(libc::SYS_fstat.to_string())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(receipt.fstat_syscalls().unwrap(), process.fstat_syscalls);
        assert_eq!(
            usize::try_from(process.fstat_syscalls).unwrap(),
            process.fstat_attempts.len()
        );
        let regular_bytes = regular.as_os_str().as_bytes();
        let success_index = process
            .fstat_attempts
            .iter()
            .position(|attempt| {
                attempt
                    .request
                    .identity
                    .as_ref()
                    .is_some_and(|identity| identity.target == regular_bytes)
                    && matches!(attempt.outcome, CapturedFstatOutcome::Metadata(_))
            })
            .unwrap();
        let success = &process.fstat_attempts[success_index];
        assert!(success.request.descriptor >= 0);
        assert!(success.request.mount_id.is_some());
        assert!(!success.request.output_is_null);
        let identity = success.request.identity.as_ref().unwrap();
        let CapturedFstatOutcome::Metadata(metadata) = success.outcome else {
            panic!("fstat did not return metadata");
        };
        let expected = std::fs::metadata(&regular).unwrap();
        assert_eq!(identity.device, expected.dev());
        assert_eq!(identity.inode, expected.ino());
        assert_eq!(metadata.device, expected.dev());
        assert_eq!(metadata.inode, expected.ino());
        assert_eq!(metadata.mode, expected.mode());
        assert_eq!(metadata.size, expected.size() as i64);
        assert!(process.fstat_attempts.iter().any(|attempt| {
            attempt.request.descriptor == -1
                && attempt.outcome == CapturedFstatOutcome::Failed(libc::EBADF)
        }));
        assert!(process.fstat_attempts.iter().any(|attempt| {
            attempt.request.output_is_null
                && attempt.outcome == CapturedFstatOutcome::Failed(libc::EFAULT)
        }));

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["fstat_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());

        let mut wrong_identity = serde_json::to_value(&receipt).unwrap();
        wrong_identity["processes"][0]["fstat_attempts"][success_index]["request"]["identity"]
            ["device"] = serde_json::json!(identity.device.wrapping_add(1));
        let wrong_identity: ProcessCaptureReceipt = serde_json::from_value(wrong_identity).unwrap();
        assert!(wrong_identity.validate_complete().is_err());

        let mut wrong_metadata = serde_json::to_value(&receipt).unwrap();
        wrong_metadata["processes"][0]["fstat_attempts"][success_index]["outcome"]["metadata"]
            ["inode"] = serde_json::json!(metadata.inode.wrapping_add(1));
        let wrong_metadata: ProcessCaptureReceipt = serde_json::from_value(wrong_metadata).unwrap();
        assert!(wrong_metadata.validate_complete().is_err());

        let mut collision = serde_json::to_value(&receipt).unwrap();
        collision["processes"][0]["fstat_attempts"][success_index]["syscall_index"] =
            serde_json::json!(process.open_attempts[0].syscall_index);
        let collision: ProcessCaptureReceipt = serde_json::from_value(collision).unwrap();
        assert!(collision.validate_complete().is_err());
    }

    #[test]
    fn captures_newfstatat_paths_metadata_failures_and_denominator() {
        let directory = tempfile::tempdir().unwrap();
        let regular = directory.path().join("regular");
        let symlink = directory.path().join("symlink");
        std::fs::write(&regular, b"metadata\n").unwrap();
        std::os::unix::fs::symlink("regular", &symlink).unwrap();
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, os, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "number = int(sys.argv[2])\n",
                "absolute = os.fsencode(sys.argv[1])\n",
                "def call(dirfd, path, flags):\n",
                "    result = ctypes.create_string_buffer(512)\n",
                "    pointer = ctypes.c_void_p(0) if path is None ",
                "else ctypes.c_char_p(path)\n",
                "    ctypes.set_errno(0)\n",
                "    value = libc.syscall(number, ctypes.c_int(dirfd), pointer, ",
                "ctypes.byref(result), ctypes.c_int(flags))\n",
                "    return value, ctypes.get_errno()\n",
                "assert call(-100, absolute, 0)[0] == 0\n",
                "assert call(-100, b'regular', 0)[0] == 0\n",
                "directory = os.open(b'.', os.O_RDONLY | os.O_DIRECTORY)\n",
                "assert call(directory, b'regular', 0)[0] == 0\n",
                "assert call(directory, b'symlink', 0)[0] == 0\n",
                "assert call(directory, b'symlink', 0x100)[0] == 0\n",
                "file = os.open(b'regular', os.O_RDONLY)\n",
                "assert call(file, b'', 0x1000)[0] == 0\n",
                "pipe_read, pipe_write = os.pipe()\n",
                "assert call(pipe_read, b'', 0x1000)[0] == 0\n",
                "null_empty = call(file, None, 0x1000)\n",
                "assert null_empty[0] == 0 or null_empty == (-1, 14)\n",
                "assert call(-100, b'missing', 0) == (-1, 2)\n",
                "assert call(-1, b'regular', 0) == (-1, 9)\n",
                "assert call(-100, None, 0) == (-1, 14)\n",
                "assert call(-100, absolute, 0x80000000) == (-1, 22)\n",
                "os.close(pipe_read)\n",
                "os.close(pipe_write)\n",
                "os.close(file)\n",
                "os.close(directory)\n",
            ))
            .arg(&regular)
            .arg(libc::SYS_newfstatat.to_string())
            .current_dir(directory.path())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(
            receipt.newfstatat_syscalls().unwrap(),
            process.newfstatat_syscalls
        );
        assert_eq!(
            usize::try_from(process.newfstatat_syscalls).unwrap(),
            process.newfstatat_attempts.len()
        );
        let regular_bytes = regular.as_os_str().as_bytes();
        let regular_metadata = std::fs::metadata(&regular).unwrap();
        let absolute_index = process
            .newfstatat_attempts
            .iter()
            .position(|attempt| attempt.request.path == regular_bytes)
            .unwrap();
        let absolute = &process.newfstatat_attempts[absolute_index];
        assert_eq!(absolute.request.base, None);
        let CapturedNewfstatatOutcome::Metadata(metadata) = absolute.outcome else {
            panic!("absolute newfstatat did not return metadata");
        };
        assert_eq!(metadata.device, regular_metadata.dev());
        assert_eq!(metadata.inode, regular_metadata.ino());
        assert_eq!(metadata.mode, regular_metadata.mode());
        assert_eq!(metadata.hard_links, regular_metadata.nlink());
        assert_eq!(metadata.user, regular_metadata.uid());
        assert_eq!(metadata.group, regular_metadata.gid());
        assert_eq!(metadata.special_device, regular_metadata.rdev());
        assert_eq!(metadata.size, regular_metadata.size() as i64);
        assert_eq!(metadata.block_size, regular_metadata.blksize() as i64);
        assert_eq!(metadata.blocks, regular_metadata.blocks() as i64);
        assert_eq!(metadata.access_time_seconds, regular_metadata.atime());
        assert_eq!(
            metadata.access_time_nanoseconds,
            regular_metadata.atime_nsec()
        );
        assert_eq!(metadata.modification_time_seconds, regular_metadata.mtime());
        assert_eq!(
            metadata.modification_time_nanoseconds,
            regular_metadata.mtime_nsec()
        );
        assert_eq!(metadata.change_time_seconds, regular_metadata.ctime());
        assert_eq!(
            metadata.change_time_nanoseconds,
            regular_metadata.ctime_nsec()
        );

        let relative_index = process
            .newfstatat_attempts
            .iter()
            .position(|attempt| {
                attempt.request.directory_descriptor == libc::AT_FDCWD
                    && attempt.request.path == b"regular"
                    && matches!(attempt.outcome, CapturedNewfstatatOutcome::Metadata(_))
            })
            .unwrap();
        let relative = &process.newfstatat_attempts[relative_index];
        assert_eq!(
            relative.request.base.as_deref(),
            Some(directory.path().as_os_str().as_bytes())
        );
        let descriptor_relative = process
            .newfstatat_attempts
            .iter()
            .find(|attempt| {
                attempt.request.directory_descriptor >= 0
                    && attempt.request.path == b"regular"
                    && matches!(attempt.outcome, CapturedNewfstatatOutcome::Metadata(_))
            })
            .unwrap();
        assert_eq!(
            descriptor_relative.request.base.as_deref(),
            Some(directory.path().as_os_str().as_bytes())
        );
        let followed = process
            .newfstatat_attempts
            .iter()
            .find(|attempt| attempt.request.path == b"symlink" && attempt.request.flags == 0)
            .unwrap();
        assert!(matches!(
            followed.outcome,
            CapturedNewfstatatOutcome::Metadata(value)
                if value.inode == regular_metadata.ino()
                    && value.mode & libc::S_IFMT == libc::S_IFREG
        ));
        let nofollow = process
            .newfstatat_attempts
            .iter()
            .find(|attempt| attempt.request.path == b"symlink" && attempt.request.flags == 0x100)
            .unwrap();
        assert!(matches!(
            nofollow.outcome,
            CapturedNewfstatatOutcome::Metadata(value)
                if value.inode == std::fs::symlink_metadata(&symlink).unwrap().ino()
                    && value.mode & libc::S_IFMT == libc::S_IFLNK
        ));
        assert!(process.newfstatat_attempts.iter().any(|attempt| {
            attempt.request.path.is_empty()
                && !attempt.request.path_is_null
                && attempt.request.flags == 0x1000
                && attempt.request.base.as_deref() == Some(regular_bytes)
                && matches!(attempt.outcome, CapturedNewfstatatOutcome::Metadata(_))
        }));
        assert!(process.newfstatat_attempts.iter().any(|attempt| {
            attempt.request.path.is_empty()
                && !attempt.request.path_is_null
                && attempt.request.flags == 0x1000
                && attempt
                    .request
                    .base
                    .as_deref()
                    .is_some_and(|base| base.starts_with(b"pipe:["))
                && matches!(
                    attempt.outcome,
                    CapturedNewfstatatOutcome::Metadata(value)
                        if value.mode & libc::S_IFMT == libc::S_IFIFO
                )
        }));
        assert!(process.newfstatat_attempts.iter().any(|attempt| {
            attempt.request.path_is_null
                && attempt.request.flags == 0x1000
                && matches!(
                    attempt.outcome,
                    CapturedNewfstatatOutcome::Metadata(_)
                        | CapturedNewfstatatOutcome::Failed(libc::EFAULT)
                )
        }));
        assert!(process.newfstatat_attempts.iter().any(|attempt| {
            attempt.request.path == b"missing"
                && attempt.outcome == CapturedNewfstatatOutcome::Failed(libc::ENOENT)
        }));
        assert!(process.newfstatat_attempts.iter().any(|attempt| {
            attempt.request.directory_descriptor == -1
                && attempt.request.base.is_none()
                && attempt.outcome == CapturedNewfstatatOutcome::Failed(libc::EBADF)
        }));
        assert!(process.newfstatat_attempts.iter().any(|attempt| {
            attempt.request.path_is_null
                && attempt.request.flags == 0
                && attempt.outcome == CapturedNewfstatatOutcome::Failed(libc::EFAULT)
        }));
        assert!(process.newfstatat_attempts.iter().any(|attempt| {
            attempt.request.path == regular_bytes
                && attempt.request.flags == 0x8000_0000
                && attempt.outcome == CapturedNewfstatatOutcome::Failed(libc::EINVAL)
        }));

        let metadata_index = absolute_index;
        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["newfstatat_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());
        let mut bad_nanoseconds = serde_json::to_value(&receipt).unwrap();
        bad_nanoseconds["processes"][0]["newfstatat_attempts"][metadata_index]["outcome"]
            ["metadata"]["access_time_nanoseconds"] = serde_json::json!(1_000_000_000);
        let bad_nanoseconds: ProcessCaptureReceipt =
            serde_json::from_value(bad_nanoseconds).unwrap();
        assert!(bad_nanoseconds.validate_complete().is_err());
        let mut invalid_directory_descriptor = serde_json::to_value(&receipt).unwrap();
        invalid_directory_descriptor["processes"][0]["newfstatat_attempts"][relative_index]
            ["request"]["directory_descriptor"] = serde_json::json!(-1);
        let invalid_directory_descriptor: ProcessCaptureReceipt =
            serde_json::from_value(invalid_directory_descriptor).unwrap();
        assert!(invalid_directory_descriptor.validate_complete().is_err());
        let mut unknown_file_type = serde_json::to_value(&receipt).unwrap();
        unknown_file_type["processes"][0]["newfstatat_attempts"][metadata_index]["outcome"]
            ["metadata"]["mode"] = serde_json::json!(0x3000 | 0o600);
        let unknown_file_type: ProcessCaptureReceipt =
            serde_json::from_value(unknown_file_type).unwrap();
        assert!(unknown_file_type.validate_complete().is_err());
        let mut high_mode_bits = serde_json::to_value(&receipt).unwrap();
        high_mode_bits["processes"][0]["newfstatat_attempts"][metadata_index]["outcome"]
            ["metadata"]["mode"] = serde_json::json!(metadata.mode | 0x8000_0000);
        let high_mode_bits: ProcessCaptureReceipt = serde_json::from_value(high_mode_bits).unwrap();
        assert!(high_mode_bits.validate_complete().is_err());
        let mut invalid_null = serde_json::to_value(&receipt).unwrap();
        invalid_null["processes"][0]["newfstatat_attempts"][metadata_index]["request"]
            ["path_is_null"] = serde_json::json!(true);
        let invalid_null: ProcessCaptureReceipt = serde_json::from_value(invalid_null).unwrap();
        assert!(invalid_null.validate_complete().is_err());
        let mut collision = serde_json::to_value(&receipt).unwrap();
        collision["processes"][0]["newfstatat_attempts"][metadata_index]["syscall_index"] =
            serde_json::json!(process.open_attempts[0].syscall_index);
        let collision: ProcessCaptureReceipt = serde_json::from_value(collision).unwrap();
        assert!(collision.validate_complete().is_err());
    }

    #[test]
    fn captures_readlink_paths_targets_failures_and_denominator() {
        let directory = tempfile::tempdir().unwrap();
        let regular = directory.path().join("regular");
        let link = directory.path().join("link");
        let raw_link = directory
            .path()
            .join(std::ffi::OsString::from_vec(b"raw-link-\xff".to_vec()));
        std::fs::write(&regular, b"not a link\n").unwrap();
        std::os::unix::fs::symlink("target", &link).unwrap();
        std::os::unix::fs::symlink(
            std::ffi::OsString::from_vec(b"raw-target-\xfe".to_vec()),
            &raw_link,
        )
        .unwrap();
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, os, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.readlink.restype = ctypes.c_ssize_t\n",
                "libc.readlinkat.restype = ctypes.c_ssize_t\n",
                "absolute = os.fsencode(sys.argv[1])\n",
                "raw_link = os.fsencode(sys.argv[2])\n",
                "def call(at, dirfd, path, size):\n",
                "    result = ctypes.create_string_buffer(max(size, 1))\n",
                "    pointer = ctypes.c_void_p(0) if path is None ",
                "else ctypes.c_char_p(path)\n",
                "    ctypes.set_errno(0)\n",
                "    if at:\n",
                "        value = libc.readlinkat(ctypes.c_int(dirfd), pointer, ",
                "ctypes.byref(result), ctypes.c_size_t(size))\n",
                "    else:\n",
                "        value = libc.readlink(pointer, ctypes.byref(result), ",
                "ctypes.c_size_t(size))\n",
                "    return value, ctypes.get_errno(), result.raw[:max(value, 0)]\n",
                "assert call(False, -100, absolute, 128) == (6, 0, b'target')\n",
                "assert call(False, -100, b'link', 128) == (6, 0, b'target')\n",
                "assert call(True, -100, absolute, 128) == (6, 0, b'target')\n",
                "assert call(True, -100, b'link', 128) == (6, 0, b'target')\n",
                "directory = os.open(b'.', os.O_RDONLY | os.O_DIRECTORY)\n",
                "assert call(True, directory, b'link', 128) == (6, 0, b'target')\n",
                "linkfd = os.open(b'link', os.O_PATH | os.O_NOFOLLOW)\n",
                "assert call(True, linkfd, b'', 128) == (6, 0, b'target')\n",
                "assert call(True, -100, raw_link, 128) == ",
                "(12, 0, b'raw-target-\\xfe')\n",
                "assert call(True, -100, b'link', 3) == (3, 0, b'tar')\n",
                "assert call(True, -100, b'regular', 128)[:2] == (-1, 22)\n",
                "assert call(True, -100, b'missing', 128)[:2] == (-1, 2)\n",
                "assert call(True, -1, b'link', 128)[:2] == (-1, 9)\n",
                "assert call(True, -100, None, 128)[:2] == (-1, 14)\n",
                "assert call(True, -100, b'link', 0)[:2] == (-1, 22)\n",
                "pipe_read, pipe_write = os.pipe()\n",
                "assert call(True, pipe_read, b'', 128)[:2] == (-1, 2)\n",
                "os.close(pipe_read)\n",
                "os.close(pipe_write)\n",
                "os.close(linkfd)\n",
                "os.close(directory)\n",
            ))
            .arg(&link)
            .arg(&raw_link)
            .current_dir(directory.path())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(
            receipt.readlink_syscalls().unwrap(),
            process.readlink_syscalls
        );
        assert_eq!(
            usize::try_from(process.readlink_syscalls).unwrap(),
            process.readlink_attempts.len()
        );
        let link_bytes = link.as_os_str().as_bytes();
        let absolute_index = process
            .readlink_attempts
            .iter()
            .position(|attempt| {
                attempt.request.path == link_bytes
                    && attempt.request.system_call == CapturedReadlinkSystemCall::ReadlinkAt
            })
            .unwrap();
        let absolute = &process.readlink_attempts[absolute_index];
        assert_eq!(absolute.request.directory_descriptor, libc::AT_FDCWD);
        assert_eq!(absolute.request.base, None);
        assert!(matches!(
            &absolute.outcome,
            CapturedReadlinkOutcome::Target { bytes, possibly_truncated: false }
                if bytes == b"target"
        ));
        let cwd_relative_index = process
            .readlink_attempts
            .iter()
            .position(|attempt| {
                attempt.request.system_call == CapturedReadlinkSystemCall::ReadlinkAt
                    && attempt.request.directory_descriptor == libc::AT_FDCWD
                    && attempt.request.path == b"link"
            })
            .unwrap();
        let cwd_relative = &process.readlink_attempts[cwd_relative_index];
        assert_eq!(
            cwd_relative.request.base.as_deref(),
            Some(directory.path().as_os_str().as_bytes())
        );
        #[cfg(target_arch = "x86_64")]
        {
            let legacy_absolute = process
                .readlink_attempts
                .iter()
                .find(|attempt| {
                    attempt.request.system_call == CapturedReadlinkSystemCall::Readlink
                        && attempt.request.path == link_bytes
                })
                .unwrap();
            assert_eq!(legacy_absolute.request.directory_descriptor, libc::AT_FDCWD);
            assert!(process.readlink_attempts.iter().any(|attempt| {
                attempt.request.system_call == CapturedReadlinkSystemCall::Readlink
                    && attempt.request.path == b"link"
                    && attempt.request.base.as_deref()
                        == Some(directory.path().as_os_str().as_bytes())
            }));
        }
        let descriptor_relative = process
            .readlink_attempts
            .iter()
            .find(|attempt| {
                attempt.request.system_call == CapturedReadlinkSystemCall::ReadlinkAt
                    && attempt.request.directory_descriptor >= 0
                    && attempt.request.path == b"link"
            })
            .unwrap();
        assert_eq!(
            descriptor_relative.request.base.as_deref(),
            Some(directory.path().as_os_str().as_bytes())
        );
        assert!(process.readlink_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedReadlinkSystemCall::ReadlinkAt
                && attempt.request.path.is_empty()
                && !attempt.request.path_is_null
                && attempt.request.base.as_deref() == Some(link_bytes)
                && matches!(
                    &attempt.outcome,
                    CapturedReadlinkOutcome::Target { bytes, possibly_truncated: false }
                        if bytes == b"target"
                )
        }));
        assert!(process.readlink_attempts.iter().any(|attempt| {
            attempt.request.path == raw_link.as_os_str().as_bytes()
                && matches!(
                    &attempt.outcome,
                    CapturedReadlinkOutcome::Target { bytes, possibly_truncated: false }
                        if bytes == b"raw-target-\xfe"
                )
        }));
        let truncated_index = process
            .readlink_attempts
            .iter()
            .position(|attempt| {
                attempt.request.path == b"link"
                    && attempt.request.requested_bytes == 3
                    && matches!(
                        &attempt.outcome,
                        CapturedReadlinkOutcome::Target { bytes, possibly_truncated: true }
                            if bytes == b"tar"
                    )
            })
            .unwrap();
        assert!(process.readlink_attempts.iter().any(|attempt| {
            attempt.request.path == b"regular"
                && attempt.outcome == CapturedReadlinkOutcome::Failed(libc::EINVAL)
        }));
        assert!(process.readlink_attempts.iter().any(|attempt| {
            attempt.request.path == b"missing"
                && attempt.outcome == CapturedReadlinkOutcome::Failed(libc::ENOENT)
        }));
        assert!(process.readlink_attempts.iter().any(|attempt| {
            attempt.request.directory_descriptor == -1
                && attempt.request.base.is_none()
                && attempt.outcome == CapturedReadlinkOutcome::Failed(libc::EBADF)
        }));
        assert!(process.readlink_attempts.iter().any(|attempt| {
            attempt.request.path_is_null
                && attempt.outcome == CapturedReadlinkOutcome::Failed(libc::EFAULT)
        }));
        assert!(process.readlink_attempts.iter().any(|attempt| {
            attempt.request.path == b"link"
                && attempt.request.requested_bytes == 0
                && attempt.outcome == CapturedReadlinkOutcome::Failed(libc::EINVAL)
        }));
        assert!(process.readlink_attempts.iter().any(|attempt| {
            attempt.request.path.is_empty()
                && !attempt.request.path_is_null
                && attempt
                    .request
                    .base
                    .as_deref()
                    .is_some_and(|base| base.starts_with(b"pipe:["))
                && attempt.outcome == CapturedReadlinkOutcome::Failed(libc::ENOENT)
        }));

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["readlink_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());
        let mut bad_truncation = serde_json::to_value(&receipt).unwrap();
        bad_truncation["processes"][0]["readlink_attempts"][truncated_index]["outcome"]["target"]
            ["possibly_truncated"] = serde_json::json!(false);
        let bad_truncation: ProcessCaptureReceipt = serde_json::from_value(bad_truncation).unwrap();
        assert!(bad_truncation.validate_complete().is_err());
        let mut invalid_directory_descriptor = serde_json::to_value(&receipt).unwrap();
        invalid_directory_descriptor["processes"][0]["readlink_attempts"][cwd_relative_index]
            ["request"]["directory_descriptor"] = serde_json::json!(-1);
        let invalid_directory_descriptor: ProcessCaptureReceipt =
            serde_json::from_value(invalid_directory_descriptor).unwrap();
        assert!(invalid_directory_descriptor.validate_complete().is_err());
        let mut collision = serde_json::to_value(&receipt).unwrap();
        collision["processes"][0]["readlink_attempts"][absolute_index]["syscall_index"] =
            serde_json::json!(process.open_attempts[0].syscall_index);
        let collision: ProcessCaptureReceipt = serde_json::from_value(collision).unwrap();
        assert!(collision.validate_complete().is_err());
    }

    #[test]
    fn captures_access_paths_results_failures_and_denominator() {
        let directory = tempfile::tempdir().unwrap();
        let raw_name = std::ffi::OsString::from_vec(b"access-\xff".to_vec());
        let regular = directory.path().join(&raw_name);
        let symlink = directory.path().join("link");
        std::fs::write(&regular, b"predicate\n").unwrap();
        std::os::unix::fs::symlink(&raw_name, &symlink).unwrap();
        #[cfg(target_arch = "x86_64")]
        let access_number = libc::SYS_access;
        #[cfg(not(target_arch = "x86_64"))]
        let access_number = -1;
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, os, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "access_nr, faccessat_nr, faccessat2_nr = map(int, sys.argv[3:])\n",
                "absolute = os.fsencode(sys.argv[1])\n",
                "raw_name = os.fsencode(sys.argv[2])\n",
                "def call(number, dirfd, path, mode, flags=0):\n",
                "    pointer = ctypes.c_void_p(0) if path is None ",
                "else ctypes.c_char_p(path)\n",
                "    ctypes.set_errno(0)\n",
                "    if number == access_nr:\n",
                "        value = libc.syscall(number, pointer, ctypes.c_int(mode))\n",
                "    elif number == faccessat_nr:\n",
                "        value = libc.syscall(number, ctypes.c_int(dirfd), pointer, ",
                "ctypes.c_int(mode))\n",
                "    else:\n",
                "        value = libc.syscall(number, ctypes.c_int(dirfd), pointer, ",
                "ctypes.c_int(mode), ctypes.c_int(flags))\n",
                "    return value, ctypes.get_errno()\n",
                "if access_nr >= 0:\n",
                "    assert call(access_nr, -100, absolute, 0) == (0, 0)\n",
                "    assert call(access_nr, -100, b'missing', 0) == (-1, 2)\n",
                "assert call(faccessat_nr, -100, raw_name, 0) == (0, 0)\n",
                "directory = os.open(b'.', os.O_RDONLY | os.O_DIRECTORY)\n",
                "assert call(faccessat_nr, directory, raw_name, 0) == (0, 0)\n",
                "assert call(faccessat_nr, -1, raw_name, 0) == (-1, 9)\n",
                "assert call(faccessat_nr, -100, None, 0) == (-1, 14)\n",
                "file = os.open(raw_name, os.O_RDONLY)\n",
                "assert call(faccessat2_nr, file, b'', 0, 0x1000) == (0, 0)\n",
                "pipe_read, pipe_write = os.pipe()\n",
                "assert call(faccessat2_nr, pipe_read, b'', 0, 0x1000) == (0, 0)\n",
                "assert call(faccessat2_nr, -100, b'link', 0, 0x100) == (0, 0)\n",
                "assert call(faccessat2_nr, -100, absolute, 0, 0x200) == (0, 0)\n",
                "assert call(faccessat2_nr, -100, absolute, 8) == (-1, 22)\n",
                "assert call(faccessat2_nr, -100, absolute, 0, 0x80000000) ",
                "== (-1, 22)\n",
                "os.close(pipe_read)\n",
                "os.close(pipe_write)\n",
                "os.close(file)\n",
                "os.close(directory)\n",
            ))
            .arg(&regular)
            .arg(&raw_name)
            .arg(access_number.to_string())
            .arg(libc::SYS_faccessat.to_string())
            .arg(libc::SYS_faccessat2.to_string())
            .current_dir(directory.path())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(receipt.access_syscalls().unwrap(), process.access_syscalls);
        assert_eq!(
            usize::try_from(process.access_syscalls).unwrap(),
            process.access_attempts.len()
        );
        let regular_bytes = regular.as_os_str().as_bytes();
        #[cfg(target_arch = "x86_64")]
        assert!(process.access_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedAccessSystemCall::Access
                && attempt.request.path == regular_bytes
                && attempt.request.directory_descriptor == libc::AT_FDCWD
                && attempt.request.base.is_none()
                && attempt.outcome == CapturedAccessOutcome::Accessible
        }));
        assert!(process.access_attempts.iter().any(|attempt| {
            attempt.request.path == regular_bytes
                && attempt.request.base.is_none()
                && attempt.outcome == CapturedAccessOutcome::Accessible
        }));
        assert!(process.access_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedAccessSystemCall::Faccessat
                && attempt.request.path == raw_name.as_bytes()
                && attempt.request.directory_descriptor == libc::AT_FDCWD
                && attempt.request.base.as_deref() == Some(directory.path().as_os_str().as_bytes())
                && attempt.outcome == CapturedAccessOutcome::Accessible
        }));
        assert!(process.access_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedAccessSystemCall::Faccessat
                && attempt.request.path == raw_name.as_bytes()
                && attempt.request.directory_descriptor >= 0
                && attempt.request.base.as_deref() == Some(directory.path().as_os_str().as_bytes())
                && attempt.outcome == CapturedAccessOutcome::Accessible
        }));
        assert!(process.access_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedAccessSystemCall::Faccessat2
                && attempt.request.path.is_empty()
                && !attempt.request.path_is_null
                && attempt.request.flags == 0x1000
                && attempt.request.base.as_deref() == Some(regular_bytes)
                && attempt.outcome == CapturedAccessOutcome::Accessible
        }));
        let pipe_index = process
            .access_attempts
            .iter()
            .position(|attempt| {
                attempt.request.system_call == CapturedAccessSystemCall::Faccessat2
                    && attempt.request.path.is_empty()
                    && attempt.request.flags == 0x1000
                    && attempt
                        .request
                        .base
                        .as_deref()
                        .is_some_and(|base| base.starts_with(b"pipe:["))
                    && attempt.outcome == CapturedAccessOutcome::Accessible
            })
            .unwrap();
        assert!(process.access_attempts.iter().any(|attempt| {
            attempt.request.directory_descriptor == -1
                && attempt.request.base.is_none()
                && attempt.outcome == CapturedAccessOutcome::Failed(libc::EBADF)
        }));
        assert!(process.access_attempts.iter().any(|attempt| {
            attempt.request.path_is_null
                && attempt.outcome == CapturedAccessOutcome::Failed(libc::EFAULT)
        }));
        assert!(process.access_attempts.iter().any(|attempt| {
            attempt.request.mode == 8
                && attempt.outcome == CapturedAccessOutcome::Failed(libc::EINVAL)
        }));
        assert!(process.access_attempts.iter().any(|attempt| {
            attempt.request.flags == 0x8000_0000
                && attempt.outcome == CapturedAccessOutcome::Failed(libc::EINVAL)
        }));

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["access_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());
        let mut invalid_success = serde_json::to_value(&receipt).unwrap();
        let invalid_index = process
            .access_attempts
            .iter()
            .position(|attempt| attempt.request.mode == 8)
            .unwrap();
        invalid_success["processes"][0]["access_attempts"][invalid_index]["outcome"] =
            serde_json::json!("accessible");
        let invalid_success: ProcessCaptureReceipt =
            serde_json::from_value(invalid_success).unwrap();
        assert!(invalid_success.validate_complete().is_err());
        let mut nonpath_relative = serde_json::to_value(&receipt).unwrap();
        nonpath_relative["processes"][0]["access_attempts"][pipe_index]["request"]["path"] =
            serde_json::json!([b'x']);
        let nonpath_relative: ProcessCaptureReceipt =
            serde_json::from_value(nonpath_relative).unwrap();
        assert!(nonpath_relative.validate_complete().is_err());
    }

    #[test]
    fn captures_getcwd_bytes_failures_and_denominator() {
        let directory = tempfile::tempdir().unwrap();
        let raw_directory = directory
            .path()
            .join(std::ffi::OsString::from_vec(b"cwd-\xff".to_vec()));
        std::fs::create_dir(&raw_directory).unwrap();
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, os, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "number = int(sys.argv[1])\n",
                "expected = os.fsencode(os.getcwd())\n",
                "def call(size, null=False):\n",
                "    result = ctypes.create_string_buffer(max(size, 1))\n",
                "    pointer = ctypes.c_void_p(0) if null else ctypes.byref(result)\n",
                "    ctypes.set_errno(0)\n",
                "    value = libc.syscall(number, pointer, ctypes.c_ulong(size))\n",
                "    return value, ctypes.get_errno(), result.raw[:max(value, 0)]\n",
                "assert call(7777) == (len(expected) + 1, 0, expected + b'\\0')\n",
                "assert call(len(expected) + 1) == ",
                "(len(expected) + 1, 0, expected + b'\\0')\n",
                "assert call(len(expected))[:2] == (-1, 34)\n",
                "assert call(7778, True)[:2] == (-1, 14)\n",
                "assert call(0)[:2] == (-1, 34)\n",
            ))
            .arg(libc::SYS_getcwd.to_string())
            .current_dir(&raw_directory)
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(receipt.getcwd_syscalls().unwrap(), process.getcwd_syscalls);
        assert_eq!(
            usize::try_from(process.getcwd_syscalls).unwrap(),
            process.getcwd_attempts.len()
        );
        let expected = raw_directory.as_os_str().as_bytes();
        let success_index = process
            .getcwd_attempts
            .iter()
            .position(|attempt| {
                attempt.request.requested_bytes == 7777
                    && attempt.outcome == CapturedGetcwdOutcome::Directory(expected.to_vec())
            })
            .unwrap();
        assert!(process.getcwd_attempts.iter().any(|attempt| {
            attempt.request.requested_bytes == expected.len() as u64 + 1
                && attempt.outcome == CapturedGetcwdOutcome::Directory(expected.to_vec())
        }));
        assert!(process.getcwd_attempts.iter().any(|attempt| {
            attempt.request.requested_bytes == expected.len() as u64
                && attempt.outcome == CapturedGetcwdOutcome::Failed(libc::ERANGE)
        }));
        assert!(process.getcwd_attempts.iter().any(|attempt| {
            attempt.request.requested_bytes == 7778
                && attempt.outcome == CapturedGetcwdOutcome::Failed(libc::EFAULT)
        }));
        assert!(process.getcwd_attempts.iter().any(|attempt| {
            attempt.request.requested_bytes == 0
                && attempt.outcome == CapturedGetcwdOutcome::Failed(libc::ERANGE)
        }));

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["getcwd_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());
        let mut relative = serde_json::to_value(&receipt).unwrap();
        relative["processes"][0]["getcwd_attempts"][success_index]["outcome"]["directory"] =
            serde_json::json!([b'r', b'e', b'l']);
        let relative: ProcessCaptureReceipt = serde_json::from_value(relative).unwrap();
        assert!(relative.validate_complete().is_err());
        let mut unterminated_shape = serde_json::to_value(&receipt).unwrap();
        unterminated_shape["processes"][0]["getcwd_attempts"][success_index]["outcome"]
            ["directory"] = serde_json::json!([b'/', b'a', 0, b'b']);
        let unterminated_shape: ProcessCaptureReceipt =
            serde_json::from_value(unterminated_shape).unwrap();
        assert!(unterminated_shape.validate_complete().is_err());
    }

    #[test]
    fn captures_chdir_and_fchdir_mutations_failures_and_denominator() {
        let root = tempfile::tempdir().unwrap();
        let raw_directory = root
            .path()
            .join(std::ffi::OsString::from_vec(b"cwd-\xff".to_vec()));
        std::fs::create_dir(&raw_directory).unwrap();
        let deleted_directory = root.path().join("deleted");
        std::fs::create_dir(&deleted_directory).unwrap();
        let regular = root.path().join("regular");
        std::fs::write(&regular, b"value").unwrap();
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, errno, os, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "chdir_nr, fchdir_nr = map(int, sys.argv[1:3])\n",
                "root = os.fsencode(os.getcwd())\n",
                "raw = b'cwd-\\xff'\n",
                "root_fd = os.open(b'.', os.O_RDONLY | os.O_DIRECTORY)\n",
                "dir_fd = os.open(raw, os.O_RDONLY | os.O_DIRECTORY)\n",
                "deleted_fd = os.open(b'deleted', os.O_RDONLY | os.O_DIRECTORY)\n",
                "os.rmdir(b'deleted')\n",
                "file_fd = os.open(b'regular', os.O_RDONLY)\n",
                "def path_call(path):\n",
                "    ctypes.set_errno(0)\n",
                "    pointer = ctypes.c_void_p(0) if path is None else ctypes.c_char_p(path)\n",
                "    return libc.syscall(chdir_nr, pointer), ctypes.get_errno()\n",
                "def fd_call(fd):\n",
                "    ctypes.set_errno(0)\n",
                "    return libc.syscall(fchdir_nr, ctypes.c_int(fd)), ctypes.get_errno()\n",
                "assert path_call(raw) == (0, 0)\n",
                "assert os.fsencode(os.getcwd()) == root + b'/' + raw\n",
                "assert fd_call(root_fd) == (0, 0)\n",
                "assert fd_call(dir_fd) == (0, 0)\n",
                "assert fd_call(deleted_fd) == (0, 0)\n",
                "assert fd_call(root_fd) == (0, 0)\n",
                "assert path_call(root) == (0, 0)\n",
                "assert path_call(b'missing') == (-1, errno.ENOENT)\n",
                "assert path_call(b'') == (-1, errno.ENOENT)\n",
                "assert path_call(None) == (-1, errno.EFAULT)\n",
                "assert fd_call(-1) == (-1, errno.EBADF)\n",
                "assert fd_call(file_fd) == (-1, errno.ENOTDIR)\n",
                "os.close(file_fd); os.close(deleted_fd)\n",
                "os.close(dir_fd); os.close(root_fd)\n",
            ))
            .arg(libc::SYS_chdir.to_string())
            .arg(libc::SYS_fchdir.to_string())
            .current_dir(root.path())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(
            receipt.cwd_change_syscalls().unwrap(),
            process.cwd_change_syscalls
        );
        assert_eq!(process.cwd_change_attempts.len(), 11);
        let root_bytes = root.path().as_os_str().as_bytes();
        let raw_bytes = raw_directory.as_os_str().as_bytes();
        let mut deleted_bytes = deleted_directory.as_os_str().as_bytes().to_vec();
        deleted_bytes.extend_from_slice(b" (deleted)");
        let relative_index = process
            .cwd_change_attempts
            .iter()
            .position(|attempt| {
                attempt.request.system_call == CapturedCwdChangeSystemCall::Chdir
                    && attempt.request.path == b"cwd-\xff"
                    && attempt.request.prior_working_directory == root_bytes
                    && attempt.outcome == CapturedCwdChangeOutcome::Changed(raw_bytes.to_vec())
            })
            .unwrap();
        assert!(process.cwd_change_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedCwdChangeSystemCall::Fchdir
                && attempt.request.descriptor_target.as_deref() == Some(root_bytes)
                && attempt.request.prior_working_directory == raw_bytes
                && attempt.outcome == CapturedCwdChangeOutcome::Changed(root_bytes.to_vec())
        }));
        assert!(process.cwd_change_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedCwdChangeSystemCall::Fchdir
                && attempt.request.descriptor_target.as_deref() == Some(raw_bytes)
                && attempt.request.prior_working_directory == root_bytes
                && attempt.outcome == CapturedCwdChangeOutcome::Changed(raw_bytes.to_vec())
        }));
        assert!(process.cwd_change_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedCwdChangeSystemCall::Chdir
                && attempt.request.path == root_bytes
                && attempt.request.prior_working_directory == root_bytes
                && attempt.outcome == CapturedCwdChangeOutcome::Changed(root_bytes.to_vec())
        }));
        assert!(process.cwd_change_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedCwdChangeSystemCall::Fchdir
                && attempt.request.descriptor_target.as_deref() == Some(deleted_bytes.as_slice())
                && attempt.outcome == CapturedCwdChangeOutcome::Changed(deleted_bytes.clone())
        }));
        assert!(process.cwd_change_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedCwdChangeSystemCall::Chdir
                && attempt.request.path == b"missing"
                && attempt.outcome == CapturedCwdChangeOutcome::Failed(libc::ENOENT)
        }));
        assert!(process.cwd_change_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedCwdChangeSystemCall::Chdir
                && attempt.request.path_is_null
                && attempt.outcome == CapturedCwdChangeOutcome::Failed(libc::EFAULT)
        }));
        assert!(process.cwd_change_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedCwdChangeSystemCall::Chdir
                && attempt.request.path.is_empty()
                && !attempt.request.path_is_null
                && attempt.outcome == CapturedCwdChangeOutcome::Failed(libc::ENOENT)
        }));
        assert!(process.cwd_change_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedCwdChangeSystemCall::Fchdir
                && attempt.request.directory_descriptor == -1
                && attempt.request.descriptor_target.is_none()
                && attempt.outcome == CapturedCwdChangeOutcome::Failed(libc::EBADF)
        }));
        assert!(process.cwd_change_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedCwdChangeSystemCall::Fchdir
                && attempt.request.descriptor_target.as_deref()
                    == Some(regular.as_os_str().as_bytes())
                && attempt.outcome == CapturedCwdChangeOutcome::Failed(libc::ENOTDIR)
        }));

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["cwd_change_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());
        let mut invalid_success = serde_json::to_value(&receipt).unwrap();
        invalid_success["processes"][0]["cwd_change_attempts"][relative_index]["outcome"] =
            serde_json::json!({"changed": []});
        let invalid_success: ProcessCaptureReceipt =
            serde_json::from_value(invalid_success).unwrap();
        assert!(invalid_success.validate_complete().is_err());
        let mut relative_success = serde_json::to_value(&receipt).unwrap();
        relative_success["processes"][0]["cwd_change_attempts"][relative_index]["outcome"] =
            serde_json::json!({"changed": [b'r', b'e', b'l']});
        let relative_success: ProcessCaptureReceipt =
            serde_json::from_value(relative_success).unwrap();
        assert!(relative_success.validate_complete().is_err());
        let mut impossible_chdir_target = serde_json::to_value(&receipt).unwrap();
        impossible_chdir_target["processes"][0]["cwd_change_attempts"][relative_index]["request"]
            ["descriptor_target"] = serde_json::json!(root_bytes);
        let impossible_chdir_target: ProcessCaptureReceipt =
            serde_json::from_value(impossible_chdir_target).unwrap();
        assert!(impossible_chdir_target.validate_complete().is_err());
    }

    #[test]
    fn captures_unlink_variants_results_failures_and_denominator() {
        let root = tempfile::tempdir().unwrap();
        #[cfg(target_arch = "x86_64")]
        let unlink_number = libc::SYS_unlink;
        #[cfg(not(target_arch = "x86_64"))]
        let unlink_number = -1;
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, errno, os, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "unlink_nr, unlinkat_nr = map(int, sys.argv[1:3])\n",
                "root = os.fsencode(os.getcwd())\n",
                "raw = b'unlink-\\xff'\n",
                "def create(path):\n",
                "    fd = os.open(path, os.O_CREAT | os.O_WRONLY, 0o600)\n",
                "    os.close(fd)\n",
                "def call(number, dirfd, path, flags=0):\n",
                "    pointer = ctypes.c_void_p(0) if path is None ",
                "else ctypes.c_char_p(path)\n",
                "    ctypes.set_errno(0)\n",
                "    if number == unlink_nr:\n",
                "        value = libc.syscall(number, pointer)\n",
                "    else:\n",
                "        value = libc.syscall(number, ctypes.c_int(dirfd), pointer, ",
                "ctypes.c_int(flags))\n",
                "    return value, ctypes.get_errno()\n",
                "create(raw)\n",
                "assert call(unlinkat_nr, -100, raw) == (0, 0)\n",
                "directory = os.open(b'.', os.O_RDONLY | os.O_DIRECTORY)\n",
                "create(b'by-fd')\n",
                "assert call(unlinkat_nr, directory, b'by-fd') == (0, 0)\n",
                "os.mkdir(b'removed-directory')\n",
                "assert call(unlinkat_nr, -100, b'removed-directory', 0x200) == (0, 0)\n",
                "if unlink_nr >= 0:\n",
                "    absolute = root + b'/absolute'\n",
                "    create(absolute)\n",
                "    assert call(unlink_nr, -100, absolute) == (0, 0)\n",
                "os.symlink(b'target', b'link')\n",
                "assert call(unlinkat_nr, -100, b'link') == (0, 0)\n",
                "assert call(unlinkat_nr, -100, b'missing') == (-1, errno.ENOENT)\n",
                "assert call(unlinkat_nr, -100, b'') == (-1, errno.ENOENT)\n",
                "assert call(unlinkat_nr, -100, None) == (-1, errno.EFAULT)\n",
                "assert call(unlinkat_nr, -1, b'relative') == (-1, errno.EBADF)\n",
                "os.mkdir(b'not-a-file')\n",
                "assert call(unlinkat_nr, -100, b'not-a-file') == (-1, errno.EISDIR)\n",
                "create(b'not-a-directory')\n",
                "assert call(unlinkat_nr, -100, b'not-a-directory', 0x200) ",
                "== (-1, errno.ENOTDIR)\n",
                "create(b'bad-flags')\n",
                "assert call(unlinkat_nr, -100, b'bad-flags', 0x40000000) ",
                "== (-1, errno.EINVAL)\n",
                "assert call(unlinkat_nr, -100, b'x' * 4096) ",
                "== (-1, errno.ENAMETOOLONG)\n",
                "os.close(directory)\n",
            ))
            .arg(unlink_number.to_string())
            .arg(libc::SYS_unlinkat.to_string())
            .current_dir(root.path())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(
            receipt.path_removal_syscalls().unwrap(),
            process.path_removal_syscalls
        );
        assert_eq!(
            usize::try_from(process.path_removal_syscalls).unwrap(),
            process.path_removal_attempts.len()
        );
        let root_bytes = root.path().as_os_str().as_bytes();
        let relative_index = process
            .path_removal_attempts
            .iter()
            .position(|attempt| {
                attempt.request.system_call == CapturedPathRemovalSystemCall::UnlinkAt
                    && attempt.request.path == b"unlink-\xff"
                    && attempt.request.directory_descriptor == libc::AT_FDCWD
                    && attempt.request.base.as_deref() == Some(root_bytes)
                    && attempt.outcome == CapturedPathRemovalOutcome::Removed
            })
            .unwrap();
        assert!(process.path_removal_attempts.iter().any(|attempt| {
            attempt.request.path == b"by-fd"
                && attempt.request.directory_descriptor >= 0
                && attempt.request.base.as_deref() == Some(root_bytes)
                && attempt.outcome == CapturedPathRemovalOutcome::Removed
        }));
        assert!(process.path_removal_attempts.iter().any(|attempt| {
            attempt.request.path == b"removed-directory"
                && attempt.request.flags == crate::LINUX_AT_REMOVEDIR
                && attempt.outcome == CapturedPathRemovalOutcome::Removed
        }));
        #[cfg(target_arch = "x86_64")]
        assert!(process.path_removal_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedPathRemovalSystemCall::Unlink
                && attempt.request.path.ends_with(b"/absolute")
                && attempt.request.base.is_none()
                && attempt.outcome == CapturedPathRemovalOutcome::Removed
        }));
        assert!(process.path_removal_attempts.iter().any(|attempt| {
            attempt.request.path == b"link"
                && attempt.outcome == CapturedPathRemovalOutcome::Removed
        }));
        for (path, errno) in [
            (&b"missing"[..], libc::ENOENT),
            (&b""[..], libc::ENOENT),
            (&b"not-a-file"[..], libc::EISDIR),
            (&b"not-a-directory"[..], libc::ENOTDIR),
            (&b"bad-flags"[..], libc::EINVAL),
        ] {
            assert!(process.path_removal_attempts.iter().any(|attempt| {
                attempt.request.path == path
                    && attempt.outcome == CapturedPathRemovalOutcome::Failed(errno)
            }));
        }
        assert!(process.path_removal_attempts.iter().any(|attempt| {
            attempt.request.path_is_null
                && attempt.outcome == CapturedPathRemovalOutcome::Failed(libc::EFAULT)
        }));
        assert!(process.path_removal_attempts.iter().any(|attempt| {
            attempt.request.directory_descriptor == -1
                && attempt.request.base.is_none()
                && attempt.outcome == CapturedPathRemovalOutcome::Failed(libc::EBADF)
        }));
        assert!(process.path_removal_attempts.iter().any(|attempt| {
            attempt.request.path.len() == crate::LINUX_PATH_MAX
                && attempt.outcome == CapturedPathRemovalOutcome::Failed(libc::ENAMETOOLONG)
        }));

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["path_removal_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());
        let mut invalid_success = serde_json::to_value(&receipt).unwrap();
        let bad_flags_index = process
            .path_removal_attempts
            .iter()
            .position(|attempt| attempt.request.path == b"bad-flags")
            .unwrap();
        invalid_success["processes"][0]["path_removal_attempts"][bad_flags_index]["outcome"] =
            serde_json::json!("removed");
        let invalid_success: ProcessCaptureReceipt =
            serde_json::from_value(invalid_success).unwrap();
        assert!(invalid_success.validate_complete().is_err());
        let mut relative_base = serde_json::to_value(&receipt).unwrap();
        relative_base["processes"][0]["path_removal_attempts"][relative_index]["request"]["base"] =
            serde_json::json!([b'r', b'e', b'l']);
        let relative_base: ProcessCaptureReceipt = serde_json::from_value(relative_base).unwrap();
        assert!(relative_base.validate_complete().is_err());
    }

    #[test]
    fn captures_mkdir_variants_results_failures_and_denominator() {
        let root = tempfile::tempdir().unwrap();
        #[cfg(target_arch = "x86_64")]
        let mkdir_number = libc::SYS_mkdir;
        #[cfg(not(target_arch = "x86_64"))]
        let mkdir_number = -1;
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, errno, os, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "mkdir_nr, mkdirat_nr = map(int, sys.argv[1:3])\n",
                "root = os.fsencode(os.getcwd())\n",
                "raw = b'mkdir-\\xff'\n",
                "def call(number, dirfd, path, mode):\n",
                "    pointer = ctypes.c_void_p(0) if path is None ",
                "else ctypes.c_char_p(path)\n",
                "    ctypes.set_errno(0)\n",
                "    if number == mkdir_nr:\n",
                "        value = libc.syscall(number, pointer, ctypes.c_uint(mode))\n",
                "    else:\n",
                "        value = libc.syscall(number, ctypes.c_int(dirfd), pointer, ",
                "ctypes.c_uint(mode))\n",
                "    return value, ctypes.get_errno()\n",
                "assert call(mkdirat_nr, -100, raw, 0o751) == (0, 0)\n",
                "directory = os.open(b'.', os.O_RDONLY | os.O_DIRECTORY)\n",
                "assert call(mkdirat_nr, directory, b'by-fd', 0o700) == (0, 0)\n",
                "absolute_at = root + b'/absolute-at'\n",
                "assert call(mkdirat_nr, -1, absolute_at, 0o755) == (0, 0)\n",
                "if mkdir_nr >= 0:\n",
                "    absolute = root + b'/absolute'\n",
                "    assert call(mkdir_nr, -100, absolute, 0o711) == (0, 0)\n",
                "assert call(mkdirat_nr, -100, raw, 0o751) == (-1, errno.EEXIST)\n",
                "assert call(mkdirat_nr, -100, b'missing/child', 0o700) ",
                "== (-1, errno.ENOENT)\n",
                "assert call(mkdirat_nr, -100, b'', 0o700) == (-1, errno.ENOENT)\n",
                "assert call(mkdirat_nr, -100, None, 0o700) == (-1, errno.EFAULT)\n",
                "assert call(mkdirat_nr, -1, b'relative', 0o700) == (-1, errno.EBADF)\n",
                "fd = os.open(b'file', os.O_CREAT | os.O_WRONLY, 0o600)\n",
                "os.close(fd)\n",
                "assert call(mkdirat_nr, -100, b'file/child', 0o700) ",
                "== (-1, errno.ENOTDIR)\n",
                "assert call(mkdirat_nr, -100, b'x' * 4096, 0o700) ",
                "== (-1, errno.ENAMETOOLONG)\n",
                "os.close(directory)\n",
            ))
            .arg(mkdir_number.to_string())
            .arg(libc::SYS_mkdirat.to_string())
            .current_dir(root.path())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(
            receipt.directory_creation_syscalls().unwrap(),
            process.directory_creation_syscalls
        );
        assert_eq!(
            usize::try_from(process.directory_creation_syscalls).unwrap(),
            process.directory_creation_attempts.len()
        );
        let root_bytes = root.path().as_os_str().as_bytes();
        let relative_index = process
            .directory_creation_attempts
            .iter()
            .position(|attempt| {
                attempt.request.system_call == CapturedDirectoryCreationSystemCall::MkdirAt
                    && attempt.request.path == b"mkdir-\xff"
                    && attempt.request.directory_descriptor == libc::AT_FDCWD
                    && attempt.request.base.as_deref() == Some(root_bytes)
                    && attempt.request.mode == 0o751
                    && attempt.outcome == CapturedDirectoryCreationOutcome::Created
            })
            .unwrap();
        assert!(process.directory_creation_attempts.iter().any(|attempt| {
            attempt.request.path == b"by-fd"
                && attempt.request.directory_descriptor >= 0
                && attempt.request.base.as_deref() == Some(root_bytes)
                && attempt.request.mode == 0o700
                && attempt.outcome == CapturedDirectoryCreationOutcome::Created
        }));
        assert!(process.directory_creation_attempts.iter().any(|attempt| {
            attempt.request.path.ends_with(b"/absolute-at")
                && attempt.request.directory_descriptor == -1
                && attempt.request.base.is_none()
                && attempt.outcome == CapturedDirectoryCreationOutcome::Created
        }));
        #[cfg(target_arch = "x86_64")]
        assert!(process.directory_creation_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedDirectoryCreationSystemCall::Mkdir
                && attempt.request.path.ends_with(b"/absolute")
                && attempt.request.directory_descriptor == libc::AT_FDCWD
                && attempt.request.mode == 0o711
                && attempt.request.base.is_none()
                && attempt.outcome == CapturedDirectoryCreationOutcome::Created
        }));
        for (path, errno) in [
            (&b"mkdir-\xff"[..], libc::EEXIST),
            (&b"missing/child"[..], libc::ENOENT),
            (&b""[..], libc::ENOENT),
            (&b"file/child"[..], libc::ENOTDIR),
        ] {
            assert!(process.directory_creation_attempts.iter().any(|attempt| {
                attempt.request.path == path
                    && attempt.outcome == CapturedDirectoryCreationOutcome::Failed(errno)
            }));
        }
        assert!(process.directory_creation_attempts.iter().any(|attempt| {
            attempt.request.path_is_null
                && attempt.outcome == CapturedDirectoryCreationOutcome::Failed(libc::EFAULT)
        }));
        let invalid_index = process
            .directory_creation_attempts
            .iter()
            .position(|attempt| {
                attempt.request.directory_descriptor == -1
                    && attempt.request.path == b"relative"
                    && attempt.request.base.is_none()
                    && attempt.outcome == CapturedDirectoryCreationOutcome::Failed(libc::EBADF)
            })
            .unwrap();
        assert!(process.directory_creation_attempts.iter().any(|attempt| {
            attempt.request.path.len() == crate::LINUX_PATH_MAX
                && attempt.outcome == CapturedDirectoryCreationOutcome::Failed(libc::ENAMETOOLONG)
        }));

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["directory_creation_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());
        let mut invalid_success = serde_json::to_value(&receipt).unwrap();
        invalid_success["processes"][0]["directory_creation_attempts"][invalid_index]["outcome"] =
            serde_json::json!("created");
        let invalid_success: ProcessCaptureReceipt =
            serde_json::from_value(invalid_success).unwrap();
        assert!(invalid_success.validate_complete().is_err());
        let mut relative_base = serde_json::to_value(&receipt).unwrap();
        relative_base["processes"][0]["directory_creation_attempts"][relative_index]["request"]
            ["base"] = serde_json::json!([b'r', b'e', b'l']);
        let relative_base: ProcessCaptureReceipt = serde_json::from_value(relative_base).unwrap();
        assert!(relative_base.validate_complete().is_err());
    }

    #[test]
    fn captures_rename_variants_results_failures_and_denominator() {
        let root = tempfile::tempdir().unwrap();
        #[cfg(target_arch = "x86_64")]
        let rename_number = libc::SYS_rename;
        #[cfg(not(target_arch = "x86_64"))]
        let rename_number = -1;
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        let renameat_number = libc::SYS_renameat;
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        let renameat_number = -1;
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, errno, mmap, os, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "rename_nr, renameat_nr, renameat2_nr = map(int, sys.argv[1:4])\n",
                "root = os.fsencode(os.getcwd())\n",
                "def touch(path, dirfd=None):\n",
                "    args = {} if dirfd is None else {'dir_fd': dirfd}\n",
                "    fd = os.open(path, os.O_CREAT | os.O_WRONLY, 0o600, **args)\n",
                "    os.close(fd)\n",
                "def pointer(value):\n",
                "    if value is None:\n",
                "        return ctypes.c_void_p(0), None\n",
                "    if isinstance(value, int):\n",
                "        return ctypes.c_void_p(value), None\n",
                "    keep = ctypes.create_string_buffer(value)\n",
                "    return ctypes.cast(keep, ctypes.c_void_p), keep\n",
                "def call(number, oldfd, old, newfd, new, flags=0):\n",
                "    oldp, oldkeep = pointer(old)\n",
                "    newp, newkeep = pointer(new)\n",
                "    ctypes.set_errno(0)\n",
                "    if number == rename_nr:\n",
                "        value = libc.syscall(number, oldp, newp)\n",
                "    elif number == renameat_nr:\n",
                "        value = libc.syscall(number, ctypes.c_int(oldfd), oldp, ",
                "ctypes.c_int(newfd), newp)\n",
                "    else:\n",
                "        value = libc.syscall(number, ctypes.c_int(oldfd), oldp, ",
                "ctypes.c_int(newfd), newp, ctypes.c_uint(flags))\n",
                "    return value, ctypes.get_errno()\n",
                "os.mkdir(b'old-base')\n",
                "os.mkdir(b'new-base')\n",
                "oldfd = os.open(b'old-base', os.O_RDONLY | os.O_DIRECTORY)\n",
                "newfd = os.open(b'new-base', os.O_RDONLY | os.O_DIRECTORY)\n",
                "raw_old, raw_new = b'old-\\xff', b'new-\\xfe'\n",
                "touch(raw_old, oldfd)\n",
                "assert call(renameat2_nr, oldfd, raw_old, newfd, raw_new) == (0, 0)\n",
                "if renameat_nr >= 0:\n",
                "    touch(b'at-old', oldfd)\n",
                "    assert call(renameat_nr, oldfd, b'at-old', newfd, ",
                "b'at-new') == (0, 0)\n",
                "touch(b'noreplace-old')\n",
                "assert call(renameat2_nr, -100, b'noreplace-old', -100, ",
                "b'noreplace-new', 1) == (0, 0)\n",
                "touch(b'collision-old')\n",
                "touch(b'collision-new')\n",
                "assert call(renameat2_nr, -100, b'collision-old', -100, ",
                "b'collision-new', 1) == (-1, errno.EEXIST)\n",
                "touch(b'exchange-a')\n",
                "touch(b'exchange-b')\n",
                "assert call(renameat2_nr, -100, b'exchange-a', -100, ",
                "b'exchange-b', 2) == (0, 0)\n",
                "if rename_nr >= 0:\n",
                "    touch(b'legacy-old')\n",
                "    assert call(rename_nr, -100, b'legacy-old', -100, ",
                "b'legacy-new') == (0, 0)\n",
                "absolute_old = root + b'/absolute-old'\n",
                "absolute_new = root + b'/absolute-new'\n",
                "if renameat_nr >= 0:\n",
                "    touch(absolute_old)\n",
                "    assert call(renameat_nr, -1, absolute_old, -2, ",
                "absolute_new) == (0, 0)\n",
                "assert call(renameat2_nr, -100, b'missing', -100, b'dest') ",
                "== (-1, errno.ENOENT)\n",
                "assert call(renameat2_nr, -100, b'', -100, b'dest') ",
                "== (-1, errno.ENOENT)\n",
                "assert call(renameat2_nr, -100, None, -100, b'dest') ",
                "== (-1, errno.EFAULT)\n",
                "touch(b'null-new-source')\n",
                "assert call(renameat2_nr, -100, b'null-new-source', -100, None) ",
                "== (-1, errno.EFAULT)\n",
                "assert call(renameat2_nr, -1, b'relative', -100, b'dest') ",
                "== (-1, errno.EBADF)\n",
                "touch(b'invalid-new-source')\n",
                "assert call(renameat2_nr, -100, b'invalid-new-source', -1, ",
                "b'relative') == (-1, errno.EBADF)\n",
                "touch(b'not-a-directory')\n",
                "filefd = os.open(b'not-a-directory', os.O_RDONLY)\n",
                "assert call(renameat2_nr, filefd, b'child', -100, b'dest') ",
                "== (-1, errno.ENOTDIR)\n",
                "touch(b'bad-flags-source')\n",
                "assert call(renameat2_nr, -100, b'bad-flags-source', -100, ",
                "b'bad-flags-dest', 0x80000000) == (-1, errno.EINVAL)\n",
                "touch(b'conflict-source')\n",
                "touch(b'conflict-dest')\n",
                "assert call(renameat2_nr, -100, b'conflict-source', -100, ",
                "b'conflict-dest', 3) == (-1, errno.EINVAL)\n",
                "assert call(renameat2_nr, -100, b'x' * 4095, -100, b'dest') ",
                "== (-1, errno.ENAMETOOLONG)\n",
                "assert call(renameat2_nr, -100, b'x' * 4096, -100, b'dest') ",
                "== (-1, errno.ENAMETOOLONG)\n",
                "touch(b'long-new-source')\n",
                "assert call(renameat2_nr, -100, b'long-new-source', -100, ",
                "b'y' * 4096) == (-1, errno.ENAMETOOLONG)\n",
                "page = mmap.PAGESIZE\n",
                "libc.mmap.restype = ctypes.c_void_p\n",
                "touch(b'cross-old')\n",
                "cross = libc.mmap(None, page * 2, 3, 0x22, -1, 0)\n",
                "assert cross not in (None, ctypes.c_void_p(-1).value)\n",
                "ctypes.memmove(cross + page - 4, b'cross-old\\0', 10)\n",
                "assert call(renameat2_nr, -100, cross + page - 4, -100, ",
                "b'cross-new') == (0, 0)\n",
                "assert libc.munmap(ctypes.c_void_p(cross), page * 2) == 0\n",
                "region = libc.mmap(None, page * 2, 3, 0x22, -1, 0)\n",
                "assert region not in (None, ctypes.c_void_p(-1).value)\n",
                "ctypes.memset(region, ord('g'), page)\n",
                "assert libc.mprotect(ctypes.c_void_p(region + page), page, 0) == 0\n",
                "guard = region + page - 4096\n",
                "assert call(renameat2_nr, -100, guard, -100, b'dest') ",
                "== (-1, errno.ENAMETOOLONG)\n",
                "assert call(renameat2_nr, -100, b'long-new-source', -100, guard) ",
                "== (-1, errno.ENAMETOOLONG)\n",
                "assert libc.munmap(ctypes.c_void_p(region), page * 2) == 0\n",
                "os.close(filefd)\n",
                "os.close(newfd)\n",
                "os.close(oldfd)\n",
            ))
            .arg(rename_number.to_string())
            .arg(renameat_number.to_string())
            .arg(libc::SYS_renameat2.to_string())
            .current_dir(root.path())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(
            receipt.path_rename_syscalls().unwrap(),
            process.path_rename_syscalls
        );
        assert_eq!(
            usize::try_from(process.path_rename_syscalls).unwrap(),
            process.path_rename_attempts.len()
        );
        let old_base = root.path().join("old-base");
        let new_base = root.path().join("new-base");
        let raw_index = process
            .path_rename_attempts
            .iter()
            .position(|attempt| {
                attempt.request.system_call == CapturedPathRenameSystemCall::RenameAt2
                    && attempt.request.old_path == b"old-\xff"
                    && attempt.request.new_path == b"new-\xfe"
                    && attempt.request.old_path_terminated
                    && attempt.request.new_path_terminated
                    && attempt.request.old_base.as_deref() == Some(old_base.as_os_str().as_bytes())
                    && attempt.request.new_base.as_deref() == Some(new_base.as_os_str().as_bytes())
                    && attempt.outcome == CapturedPathRenameOutcome::Completed
            })
            .unwrap();
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        assert!(process.path_rename_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedPathRenameSystemCall::RenameAt
                && attempt.request.old_path == b"at-old"
                && attempt.request.new_path == b"at-new"
                && attempt.outcome == CapturedPathRenameOutcome::Completed
        }));
        #[cfg(target_arch = "x86_64")]
        assert!(process.path_rename_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedPathRenameSystemCall::Rename
                && attempt.request.old_path == b"legacy-old"
                && attempt.request.new_path == b"legacy-new"
                && attempt.outcome == CapturedPathRenameOutcome::Completed
        }));
        #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
        assert!(process.path_rename_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedPathRenameSystemCall::RenameAt
                && attempt.request.old_directory_descriptor == -1
                && attempt.request.new_directory_descriptor == -2
                && attempt.request.old_path.ends_with(b"/absolute-old")
                && attempt.request.new_path.ends_with(b"/absolute-new")
                && attempt.request.old_base.is_none()
                && attempt.request.new_base.is_none()
                && attempt.outcome == CapturedPathRenameOutcome::Completed
        }));
        for (old, new, flags, outcome) in [
            (
                &b"noreplace-old"[..],
                &b"noreplace-new"[..],
                1,
                CapturedPathRenameOutcome::Completed,
            ),
            (
                &b"collision-old"[..],
                &b"collision-new"[..],
                1,
                CapturedPathRenameOutcome::Failed(libc::EEXIST),
            ),
            (
                &b"exchange-a"[..],
                &b"exchange-b"[..],
                2,
                CapturedPathRenameOutcome::Completed,
            ),
            (
                &b"bad-flags-source"[..],
                &b"bad-flags-dest"[..],
                0x8000_0000,
                CapturedPathRenameOutcome::Failed(libc::EINVAL),
            ),
        ] {
            assert!(process.path_rename_attempts.iter().any(|attempt| {
                attempt.request.old_path == old
                    && attempt.request.new_path == new
                    && attempt.request.flags == flags
                    && attempt.outcome == outcome
            }));
        }
        assert!(process.path_rename_attempts.iter().any(|attempt| {
            attempt.request.old_path_is_null
                && attempt.outcome == CapturedPathRenameOutcome::Failed(libc::EFAULT)
        }));
        assert!(process.path_rename_attempts.iter().any(|attempt| {
            attempt.request.new_path_is_null
                && attempt.outcome == CapturedPathRenameOutcome::Failed(libc::EFAULT)
        }));
        assert!(process.path_rename_attempts.iter().any(|attempt| {
            attempt.request.old_directory_descriptor == -1
                && attempt.request.old_path == b"relative"
                && attempt.request.old_base.is_none()
                && attempt.outcome == CapturedPathRenameOutcome::Failed(libc::EBADF)
        }));
        assert!(process.path_rename_attempts.iter().any(|attempt| {
            attempt.request.new_directory_descriptor == -1
                && attempt.request.new_path == b"relative"
                && attempt.request.new_base.is_none()
                && attempt.outcome == CapturedPathRenameOutcome::Failed(libc::EBADF)
        }));
        assert!(process.path_rename_attempts.iter().any(|attempt| {
            attempt.request.old_path.len() == crate::LINUX_PATH_MAX - 1
                && attempt.request.old_path_terminated
                && attempt.outcome == CapturedPathRenameOutcome::Failed(libc::ENAMETOOLONG)
        }));
        assert!(
            process
                .path_rename_attempts
                .iter()
                .filter(|attempt| {
                    attempt.request.old_path.len() == crate::LINUX_PATH_MAX
                        && !attempt.request.old_path_terminated
                        && attempt.outcome == CapturedPathRenameOutcome::Failed(libc::ENAMETOOLONG)
                })
                .count()
                >= 2
        );
        assert!(process.path_rename_attempts.iter().any(|attempt| {
            attempt.request.new_path.len() == crate::LINUX_PATH_MAX
                && !attempt.request.new_path_terminated
                && attempt.outcome == CapturedPathRenameOutcome::Failed(libc::ENAMETOOLONG)
        }));
        assert!(process.path_rename_attempts.iter().any(|attempt| {
            attempt.request.old_path == b"cross-old"
                && attempt.request.new_path == b"cross-new"
                && attempt.outcome == CapturedPathRenameOutcome::Completed
        }));

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["path_rename_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());
        let mut missing_base = serde_json::to_value(&receipt).unwrap();
        missing_base["processes"][0]["path_rename_attempts"][raw_index]["request"]["old_base"] =
            serde_json::Value::Null;
        let missing_base: ProcessCaptureReceipt = serde_json::from_value(missing_base).unwrap();
        assert!(missing_base.validate_complete().is_err());
        let bad_flags_index = process
            .path_rename_attempts
            .iter()
            .position(|attempt| attempt.request.flags == 0x8000_0000)
            .unwrap();
        let mut forged_success = serde_json::to_value(&receipt).unwrap();
        forged_success["processes"][0]["path_rename_attempts"][bad_flags_index]["outcome"] =
            serde_json::json!("completed");
        let forged_success: ProcessCaptureReceipt = serde_json::from_value(forged_success).unwrap();
        assert!(forged_success.validate_complete().is_err());
    }

    #[test]
    fn captures_path_mode_changes_failures_bounds_and_denominator() {
        let root = tempfile::tempdir().unwrap();
        #[cfg(target_arch = "x86_64")]
        let chmod_number = libc::SYS_chmod;
        #[cfg(not(target_arch = "x86_64"))]
        let chmod_number = -1;
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, errno, mmap, os, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "chmod_nr, fchmodat_nr, fchmod_nr = map(int, sys.argv[1:4])\n",
                "root = os.fsencode(os.getcwd())\n",
                "def touch(path, dirfd=None):\n",
                "    args = {} if dirfd is None else {'dir_fd': dirfd}\n",
                "    fd = os.open(path, os.O_CREAT | os.O_WRONLY, 0o600, **args)\n",
                "    os.close(fd)\n",
                "def pointer(value):\n",
                "    if value is None:\n",
                "        return ctypes.c_void_p(0), None\n",
                "    if isinstance(value, int):\n",
                "        return ctypes.c_void_p(value), None\n",
                "    keep = ctypes.create_string_buffer(value)\n",
                "    return ctypes.cast(keep, ctypes.c_void_p), keep\n",
                "def call(number, dirfd, path, mode):\n",
                "    pathp, keep = pointer(path)\n",
                "    ctypes.set_errno(0)\n",
                "    if number == chmod_nr:\n",
                "        value = libc.syscall(number, pathp, ctypes.c_uint(mode))\n",
                "    else:\n",
                "        value = libc.syscall(number, ctypes.c_int(dirfd), pathp, ",
                "ctypes.c_uint(mode))\n",
                "    return value, ctypes.get_errno()\n",
                "def fchmod_call(fd, mode):\n",
                "    ctypes.set_errno(0)\n",
                "    value = libc.syscall(fchmod_nr, ctypes.c_int(fd), ",
                "ctypes.c_uint(mode))\n",
                "    return value, ctypes.get_errno()\n",
                "def fchmod_raw(raw_fd, mode):\n",
                "    ctypes.set_errno(0)\n",
                "    value = libc.syscall(fchmod_nr, ctypes.c_ulonglong(raw_fd), ",
                "ctypes.c_uint(mode))\n",
                "    return value, ctypes.get_errno()\n",
                "raw = b'mode-\\xff'\n",
                "touch(raw)\n",
                "assert call(fchmodat_nr, -100, raw, 0xdead01a0) == (0, 0)\n",
                "os.mkdir(b'base')\n",
                "basefd = os.open(b'base', os.O_RDONLY | os.O_DIRECTORY)\n",
                "touch(b'by-fd', basefd)\n",
                "assert call(fchmodat_nr, basefd, b'by-fd', 0o751) == (0, 0)\n",
                "absolute = root + b'/absolute'\n",
                "touch(absolute)\n",
                "assert call(fchmodat_nr, -1, absolute, 0o711) == (0, 0)\n",
                "if chmod_nr >= 0:\n",
                "    legacy = root + b'/legacy'\n",
                "    touch(legacy)\n",
                "    assert call(chmod_nr, -100, legacy, 0o700) == (0, 0)\n",
                "descriptor_fd = os.open(b'descriptor-mode', ",
                "os.O_CREAT | os.O_RDWR, 0o600)\n",
                "assert fchmod_call(descriptor_fd, 0o100600) == (0, 0)\n",
                "assert fchmod_call(-1, 0o600) == (-1, errno.EBADF)\n",
                "assert fchmod_raw(descriptor_fd + (1 << 32), 0o600) == (0, 0)\n",
                "assert call(fchmodat_nr, -100, b'missing', 0o600) ",
                "== (-1, errno.ENOENT)\n",
                "assert call(fchmodat_nr, -100, b'', 0o600) == (-1, errno.ENOENT)\n",
                "assert call(fchmodat_nr, -100, None, 0o600) == (-1, errno.EFAULT)\n",
                "assert call(fchmodat_nr, -1, b'relative', 0o600) ",
                "== (-1, errno.EBADF)\n",
                "touch(b'not-directory')\n",
                "filefd = os.open(b'not-directory', os.O_RDONLY)\n",
                "assert call(fchmodat_nr, filefd, b'child', 0o600) ",
                "== (-1, errno.ENOTDIR)\n",
                "assert call(fchmodat_nr, -100, b'x' * 4095, 0o600) ",
                "== (-1, errno.ENAMETOOLONG)\n",
                "assert call(fchmodat_nr, -100, b'x' * 4096, 0o600) ",
                "== (-1, errno.ENAMETOOLONG)\n",
                "page = mmap.PAGESIZE\n",
                "libc.mmap.restype = ctypes.c_void_p\n",
                "touch(b'cross')\n",
                "cross = libc.mmap(None, page * 2, 3, 0x22, -1, 0)\n",
                "assert cross not in (None, ctypes.c_void_p(-1).value)\n",
                "ctypes.memmove(cross + page - 3, b'cross\\0', 6)\n",
                "assert call(fchmodat_nr, -100, cross + page - 3, 0o744) == (0, 0)\n",
                "assert libc.munmap(ctypes.c_void_p(cross), page * 2) == 0\n",
                "region = libc.mmap(None, page * 2, 3, 0x22, -1, 0)\n",
                "assert region not in (None, ctypes.c_void_p(-1).value)\n",
                "ctypes.memset(region, ord('g'), page)\n",
                "assert libc.mprotect(ctypes.c_void_p(region + page), page, 0) == 0\n",
                "guard = region + page - 4096\n",
                "assert call(fchmodat_nr, -100, guard, 0o600) ",
                "== (-1, errno.ENAMETOOLONG)\n",
                "assert libc.munmap(ctypes.c_void_p(region), page * 2) == 0\n",
                "os.close(descriptor_fd)\n",
                "os.close(filefd)\n",
                "os.close(basefd)\n",
            ))
            .arg(chmod_number.to_string())
            .arg(libc::SYS_fchmodat.to_string())
            .arg(libc::SYS_fchmod.to_string())
            .current_dir(root.path())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(
            receipt.path_mode_change_syscalls().unwrap(),
            process.path_mode_change_syscalls
        );
        assert_eq!(
            usize::try_from(process.path_mode_change_syscalls).unwrap(),
            process.path_mode_change_attempts.len()
        );
        let root_bytes = root.path().as_os_str().as_bytes();
        let raw_index = process
            .path_mode_change_attempts
            .iter()
            .position(|attempt| {
                attempt.request.system_call == CapturedPathModeChangeSystemCall::FchmodAt
                    && attempt.request.path == b"mode-\xff"
                    && attempt.request.path_terminated
                    && attempt.request.base.as_deref() == Some(root_bytes)
                    && attempt.request.mode == 0xdead_01a0
                    && attempt.outcome == CapturedPathModeChangeOutcome::Changed
            })
            .unwrap();
        let base = root.path().join("base");
        assert!(process.path_mode_change_attempts.iter().any(|attempt| {
            attempt.request.path == b"by-fd"
                && attempt.request.directory_descriptor >= 0
                && attempt.request.base.as_deref() == Some(base.as_os_str().as_bytes())
                && attempt.request.mode == 0o751
                && attempt.outcome == CapturedPathModeChangeOutcome::Changed
        }));
        assert!(process.path_mode_change_attempts.iter().any(|attempt| {
            attempt.request.directory_descriptor == -1
                && attempt.request.path.ends_with(b"/absolute")
                && attempt.request.base.is_none()
                && attempt.outcome == CapturedPathModeChangeOutcome::Changed
        }));
        #[cfg(target_arch = "x86_64")]
        assert!(process.path_mode_change_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedPathModeChangeSystemCall::Chmod
                && attempt.request.directory_descriptor == libc::AT_FDCWD
                && attempt.request.path.ends_with(b"/legacy")
                && attempt.request.mode == 0o700
                && attempt.outcome == CapturedPathModeChangeOutcome::Changed
        }));
        assert!(process.path_mode_change_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedPathModeChangeSystemCall::Fchmod
                && attempt.request.descriptor.is_some_and(|descriptor| descriptor >= 0)
                && attempt
                    .request
                    .descriptor_identity
                    .as_ref()
                    .is_some_and(|identity| {
                        identity.kind == CapturedDescriptorKind::RegularFile
                            && identity.target.ends_with(b"/descriptor-mode")
                    })
                && attempt.request.descriptor_mount_id.is_some()
                && attempt.request.mode == 0o100600
                && attempt.outcome == CapturedPathModeChangeOutcome::Changed
        }));
        assert!(process.path_mode_change_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedPathModeChangeSystemCall::Fchmod
                && attempt.request.descriptor == Some(-1)
                && attempt.request.descriptor_identity.is_none()
                && attempt.request.descriptor_mount_id.is_none()
                && attempt.outcome == CapturedPathModeChangeOutcome::Failed(libc::EBADF)
        }));
        assert!(process.unmodeled_syscall_attempts.iter().any(|attempt| {
            attempt.number == libc::SYS_fchmod as u64
                && attempt.arguments[0] >> 32 == 1
                && attempt.outcome
                    == CapturedUnmodeledSyscallOutcome::Returned {
                        return_value: 0,
                        is_error: false,
                    }
        }));
        for (path, errno) in [
            (&b"missing"[..], libc::ENOENT),
            (&b""[..], libc::ENOENT),
            (&b"child"[..], libc::ENOTDIR),
        ] {
            assert!(process.path_mode_change_attempts.iter().any(|attempt| {
                attempt.request.path == path
                    && attempt.outcome == CapturedPathModeChangeOutcome::Failed(errno)
            }));
        }
        assert!(process.path_mode_change_attempts.iter().any(|attempt| {
            attempt.request.path_is_null
                && !attempt.request.path_terminated
                && attempt.outcome == CapturedPathModeChangeOutcome::Failed(libc::EFAULT)
        }));
        let invalid_index = process
            .path_mode_change_attempts
            .iter()
            .position(|attempt| {
                attempt.request.directory_descriptor == -1
                    && attempt.request.path == b"relative"
                    && attempt.request.base.is_none()
                    && attempt.outcome == CapturedPathModeChangeOutcome::Failed(libc::EBADF)
            })
            .unwrap();
        assert!(process.path_mode_change_attempts.iter().any(|attempt| {
            attempt.request.path.len() == crate::LINUX_PATH_MAX - 1
                && attempt.request.path_terminated
                && attempt.outcome == CapturedPathModeChangeOutcome::Failed(libc::ENAMETOOLONG)
        }));
        assert!(
            process
                .path_mode_change_attempts
                .iter()
                .filter(|attempt| {
                    attempt.request.path.len() == crate::LINUX_PATH_MAX
                        && !attempt.request.path_terminated
                        && attempt.outcome
                            == CapturedPathModeChangeOutcome::Failed(libc::ENAMETOOLONG)
                })
                .count()
                >= 2
        );
        assert!(process.path_mode_change_attempts.iter().any(|attempt| {
            attempt.request.path == b"cross"
                && attempt.outcome == CapturedPathModeChangeOutcome::Changed
        }));

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["path_mode_change_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());
        let mut missing_base = serde_json::to_value(&receipt).unwrap();
        missing_base["processes"][0]["path_mode_change_attempts"][raw_index]["request"]["base"] =
            serde_json::Value::Null;
        let missing_base: ProcessCaptureReceipt = serde_json::from_value(missing_base).unwrap();
        assert!(missing_base.validate_complete().is_err());
        let mut forged_success = serde_json::to_value(&receipt).unwrap();
        forged_success["processes"][0]["path_mode_change_attempts"][invalid_index]["outcome"] =
            serde_json::json!("changed");
        let forged_success: ProcessCaptureReceipt = serde_json::from_value(forged_success).unwrap();
        assert!(forged_success.validate_complete().is_err());
    }

    #[test]
    fn captures_descriptor_utimensat_results_arguments_and_denominator() {
        let root = tempfile::tempdir().unwrap();
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, errno, mmap, os, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "number = int(sys.argv[1])\n",
                "class TS(ctypes.Structure):\n",
                "    _fields_ = [('sec', ctypes.c_long), ('nsec', ctypes.c_long)]\n",
                "Pair = TS * 2\n",
                "def call(fd, times, flags=0):\n",
                "    if times is None:\n",
                "        pointer = ctypes.c_void_p(0)\n",
                "    elif isinstance(times, int):\n",
                "        pointer = ctypes.c_void_p(times)\n",
                "    else:\n",
                "        pointer = ctypes.cast(times, ctypes.c_void_p)\n",
                "    ctypes.set_errno(0)\n",
                "    value = libc.syscall(number, ctypes.c_int(fd), ctypes.c_void_p(0), ",
                "pointer, ctypes.c_int(flags))\n",
                "    return value, ctypes.get_errno()\n",
                "fd = os.open('value', os.O_CREAT | os.O_RDWR, 0o600)\n",
                "assert call(fd, None) == (0, 0)\n",
                "explicit = Pair(TS(123, 456), TS(789, 1073741822))\n",
                "assert call(fd, explicit) == (0, 0)\n",
                "now = Pair(TS(-7, 1073741823), TS(9, 1073741823))\n",
                "assert call(fd, now) == (0, 0)\n",
                "omit = Pair(TS(1, 1073741822), TS(2, 1073741822))\n",
                "assert call(-1, omit, 0x40000000) == (0, 0)\n",
                "assert call(-1, None) == (-1, errno.EBADF)\n",
                "assert call(fd, None, 0x100) == (-1, errno.EINVAL)\n",
                "assert call(-100, None) == (-1, errno.EFAULT)\n",
                "invalid = Pair(TS(1, -1), TS(2, 0))\n",
                "assert call(fd, invalid) == (-1, errno.EINVAL)\n",
                "page = mmap.PAGESIZE\n",
                "libc.mmap.restype = ctypes.c_void_p\n",
                "cross = libc.mmap(None, page * 2, 3, 0x22, -1, 0)\n",
                "assert cross not in (None, ctypes.c_void_p(-1).value)\n",
                "raw = Pair(TS(11, 12), TS(13, 14))\n",
                "ctypes.memmove(cross + page - 16, ctypes.byref(raw), 32)\n",
                "assert call(fd, cross + page - 16) == (0, 0)\n",
                "assert libc.munmap(ctypes.c_void_p(cross), page * 2) == 0\n",
                "guarded = libc.mmap(None, page * 2, 3, 0x22, -1, 0)\n",
                "assert guarded not in (None, ctypes.c_void_p(-1).value)\n",
                "ctypes.memmove(guarded + page - 32, ctypes.byref(raw), 32)\n",
                "assert libc.mprotect(ctypes.c_void_p(guarded + page), page, 0) == 0\n",
                "assert call(fd, guarded + page - 32) == (0, 0)\n",
                "assert libc.munmap(ctypes.c_void_p(guarded), page * 2) == 0\n",
                "os.close(fd)\n",
            ))
            .arg(libc::SYS_utimensat.to_string())
            .current_dir(root.path())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(
            receipt.descriptor_timestamp_change_syscalls().unwrap(),
            process.descriptor_timestamp_change_syscalls
        );
        assert_eq!(
            usize::try_from(process.descriptor_timestamp_change_syscalls).unwrap(),
            process.descriptor_timestamp_change_attempts.len()
        );
        assert!(process
            .descriptor_timestamp_change_attempts
            .iter()
            .any(|attempt| {
                attempt.request.descriptor >= 0
                    && attempt.request.identity.as_ref().is_some_and(|identity| {
                        identity.target.ends_with(b"/value")
                            && identity.kind == CapturedDescriptorKind::RegularFile
                    })
                    && attempt.request.mount_id.is_some()
                    && attempt.request.times_is_null
                    && attempt.request.times.is_none()
                    && attempt.outcome == CapturedDescriptorTimestampChangeOutcome::Changed
            }));
        assert!(process
            .descriptor_timestamp_change_attempts
            .iter()
            .any(|attempt| {
                attempt.request.times
                    == Some([
                        CapturedTimestamp {
                            seconds: 123,
                            nanoseconds: 456,
                        },
                        CapturedTimestamp {
                            seconds: 789,
                            nanoseconds: crate::LINUX_UTIME_OMIT,
                        },
                    ])
                    && attempt.outcome == CapturedDescriptorTimestampChangeOutcome::Changed
            }));
        let omit_index = process
            .descriptor_timestamp_change_attempts
            .iter()
            .position(|attempt| {
                attempt.request.descriptor == -1
                    && attempt.request.flags == 0x4000_0000
                    && attempt.request.identity.is_none()
                    && attempt.request.mount_id.is_none()
                    && attempt.request.times.is_some_and(|times| {
                        times
                            .iter()
                            .all(|time| time.nanoseconds == crate::LINUX_UTIME_OMIT)
                    })
                    && attempt.outcome == CapturedDescriptorTimestampChangeOutcome::Changed
            })
            .unwrap();
        assert!(process
            .descriptor_timestamp_change_attempts
            .iter()
            .any(|attempt| {
                attempt.request.descriptor == -1
                    && attempt.request.times_is_null
                    && attempt.outcome
                        == CapturedDescriptorTimestampChangeOutcome::Failed(libc::EBADF)
            }));
        assert!(process
            .descriptor_timestamp_change_attempts
            .iter()
            .any(|attempt| {
                attempt.request.flags == 0x100
                    && attempt.outcome
                        == CapturedDescriptorTimestampChangeOutcome::Failed(libc::EINVAL)
            }));
        let invalid_index = process
            .descriptor_timestamp_change_attempts
            .iter()
            .position(|attempt| {
                attempt
                    .request
                    .times
                    .is_some_and(|times| times[0].nanoseconds == -1)
            })
            .unwrap();

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["descriptor_timestamp_change_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());
        let mut forged_invalid = serde_json::to_value(&receipt).unwrap();
        forged_invalid["processes"][0]["descriptor_timestamp_change_attempts"][invalid_index]
            ["outcome"] = serde_json::json!("changed");
        let forged_invalid: ProcessCaptureReceipt = serde_json::from_value(forged_invalid).unwrap();
        assert!(forged_invalid.validate_complete().is_err());
        let mut omit_without_shortcut = serde_json::to_value(&receipt).unwrap();
        omit_without_shortcut["processes"][0]["descriptor_timestamp_change_attempts"][omit_index]
            ["request"]["times"][0]["nanoseconds"] = serde_json::json!(0);
        let omit_without_shortcut: ProcessCaptureReceipt =
            serde_json::from_value(omit_without_shortcut).unwrap();
        assert!(omit_without_shortcut.validate_complete().is_err());
    }

    #[test]
    fn rejects_unmodeled_pathname_utimensat() {
        let root = tempfile::tempdir().unwrap();
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, os, sys\n",
                "open('value', 'wb').close()\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall(int(sys.argv[1]), -100, ctypes.c_char_p(b'value'), ",
                "ctypes.c_void_p(0), 0)\n",
            ))
            .arg(libc::SYS_utimensat.to_string())
            .current_dir(root.path())
            .env_clear();
        let error = capture_process_tree(&mut command).unwrap_err().to_string();
        assert!(error.contains("pathname-form utimensat is not modeled"));
    }

    #[test]
    fn captures_symbolic_link_creations_failures_bounds_and_denominator() {
        let root = tempfile::tempdir().unwrap();
        #[cfg(target_arch = "x86_64")]
        let symlink_number = libc::SYS_symlink;
        #[cfg(not(target_arch = "x86_64"))]
        let symlink_number = -1;
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, errno, mmap, os, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "symlink_nr, symlinkat_nr = map(int, sys.argv[1:3])\n",
                "root = os.fsencode(os.getcwd())\n",
                "def pointer(value):\n",
                "    if value is None:\n",
                "        return ctypes.c_void_p(0), None\n",
                "    if isinstance(value, int):\n",
                "        return ctypes.c_void_p(value), None\n",
                "    keep = ctypes.create_string_buffer(value)\n",
                "    return ctypes.cast(keep, ctypes.c_void_p), keep\n",
                "def call(number, target, directory_fd, link):\n",
                "    targetp, targetkeep = pointer(target)\n",
                "    linkp, linkkeep = pointer(link)\n",
                "    ctypes.set_errno(0)\n",
                "    if number == symlink_nr:\n",
                "        value = libc.syscall(number, targetp, linkp)\n",
                "    else:\n",
                "        value = libc.syscall(number, targetp, ctypes.c_int(directory_fd), linkp)\n",
                "    return value, ctypes.get_errno()\n",
                "os.mkdir(b'base')\n",
                "basefd = os.open(b'base', os.O_RDONLY | os.O_DIRECTORY)\n",
                "assert call(symlinkat_nr, b'target-\\xff', basefd, b'link-\\xfe') == (0, 0)\n",
                "absolute = root + b'/absolute-link'\n",
                "assert call(symlinkat_nr, root + b'/opaque-target', -1, absolute) == (0, 0)\n",
                "if symlink_nr >= 0:\n",
                "    assert call(symlink_nr, b'legacy-target', -100, b'legacy-link') == (0, 0)\n",
                "assert call(symlinkat_nr, b'again', basefd, b'link-\\xfe') == (-1, errno.EEXIST)\n",
                "assert call(symlinkat_nr, b'target', -100, b'missing/child') == (-1, errno.ENOENT)\n",
                "assert call(symlinkat_nr, b'', -100, b'empty-target') == (-1, errno.ENOENT)\n",
                "assert call(symlinkat_nr, None, -100, b'null-target') == (-1, errno.EFAULT)\n",
                "assert call(symlinkat_nr, b'target', -100, None) == (-1, errno.EFAULT)\n",
                "assert call(symlinkat_nr, b'target', -1, b'relative') == (-1, errno.EBADF)\n",
                "fd = os.open(b'plain-file', os.O_CREAT | os.O_RDONLY, 0o600)\n",
                "assert call(symlinkat_nr, b'target', fd, b'child') == (-1, errno.ENOTDIR)\n",
                "assert call(symlinkat_nr, b't' * 4095, -100, b'long-target-a') == (0, 0)\n",
                "assert call(symlinkat_nr, b't' * 4096, -100, b'long-target-b') == (-1, errno.ENAMETOOLONG)\n",
                "assert call(symlinkat_nr, b'target', -100, b'l' * 4095) == (-1, errno.ENAMETOOLONG)\n",
                "assert call(symlinkat_nr, b'target', -100, b'l' * 4096) == (-1, errno.ENAMETOOLONG)\n",
                "page = mmap.PAGESIZE\n",
                "libc.mmap.restype = ctypes.c_void_p\n",
                "cross = libc.mmap(None, page * 2, 3, 0x22, -1, 0)\n",
                "assert cross not in (None, ctypes.c_void_p(-1).value)\n",
                "ctypes.memmove(cross + page - 3, b'cross-target\\0', 13)\n",
                "ctypes.memmove(cross + page + 64 - 2, b'cross-link\\0', 11)\n",
                "assert call(symlinkat_nr, cross + page - 3, -100, cross + page + 64 - 2) == (0, 0)\n",
                "assert libc.munmap(ctypes.c_void_p(cross), page * 2) == 0\n",
                "guarded = libc.mmap(None, page * 2, 3, 0x22, -1, 0)\n",
                "assert guarded not in (None, ctypes.c_void_p(-1).value)\n",
                "ctypes.memset(guarded, ord('g'), page)\n",
                "assert libc.mprotect(ctypes.c_void_p(guarded + page), page, 0) == 0\n",
                "assert call(symlinkat_nr, guarded, -100, b'guard-target') == (-1, errno.ENAMETOOLONG)\n",
                "assert call(symlinkat_nr, b'target', -100, guarded) == (-1, errno.ENAMETOOLONG)\n",
                "assert libc.munmap(ctypes.c_void_p(guarded), page * 2) == 0\n",
                "os.close(fd)\n",
                "os.close(basefd)\n",
            ))
            .arg(symlink_number.to_string())
            .arg(libc::SYS_symlinkat.to_string())
            .current_dir(root.path())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(
            receipt.symbolic_link_creation_syscalls().unwrap(),
            process.symbolic_link_creation_syscalls
        );
        assert_eq!(
            usize::try_from(process.symbolic_link_creation_syscalls).unwrap(),
            process.symbolic_link_creation_attempts.len()
        );
        let base = root.path().join("base");
        let raw_index = process
            .symbolic_link_creation_attempts
            .iter()
            .position(|attempt| {
                attempt.request.system_call == CapturedSymbolicLinkCreationSystemCall::SymlinkAt
                    && attempt.request.target == b"target-\xff"
                    && attempt.request.target_terminated
                    && attempt.request.link_path == b"link-\xfe"
                    && attempt.request.link_path_terminated
                    && attempt.request.base.as_deref() == Some(base.as_os_str().as_bytes())
                    && attempt.outcome == CapturedSymbolicLinkCreationOutcome::Created
            })
            .unwrap();
        assert!(process
            .symbolic_link_creation_attempts
            .iter()
            .any(|attempt| {
                attempt.request.target.ends_with(b"/opaque-target")
                    && attempt.request.directory_descriptor == -1
                    && attempt.request.link_path.ends_with(b"/absolute-link")
                    && attempt.request.base.is_none()
                    && attempt.outcome == CapturedSymbolicLinkCreationOutcome::Created
            }));
        #[cfg(target_arch = "x86_64")]
        assert!(process
            .symbolic_link_creation_attempts
            .iter()
            .any(|attempt| {
                attempt.request.system_call == CapturedSymbolicLinkCreationSystemCall::Symlink
                    && attempt.request.target == b"legacy-target"
                    && attempt.request.link_path == b"legacy-link"
                    && attempt.request.directory_descriptor == libc::AT_FDCWD
                    && attempt.outcome == CapturedSymbolicLinkCreationOutcome::Created
            }));
        for (link, errno) in [
            (&b"link-\xfe"[..], libc::EEXIST),
            (&b"missing/child"[..], libc::ENOENT),
            (&b"child"[..], libc::ENOTDIR),
        ] {
            assert!(process
                .symbolic_link_creation_attempts
                .iter()
                .any(|attempt| {
                    attempt.request.link_path == link
                        && attempt.outcome == CapturedSymbolicLinkCreationOutcome::Failed(errno)
                }));
        }
        assert!(process
            .symbolic_link_creation_attempts
            .iter()
            .any(|attempt| {
                attempt.request.target_is_null
                    && !attempt.request.target_terminated
                    && attempt.outcome == CapturedSymbolicLinkCreationOutcome::Failed(libc::EFAULT)
            }));
        assert!(process
            .symbolic_link_creation_attempts
            .iter()
            .any(|attempt| {
                attempt.request.link_path_is_null
                    && !attempt.request.link_path_terminated
                    && attempt.outcome == CapturedSymbolicLinkCreationOutcome::Failed(libc::EFAULT)
            }));
        let invalid_index = process
            .symbolic_link_creation_attempts
            .iter()
            .position(|attempt| {
                attempt.request.directory_descriptor == -1
                    && attempt.request.link_path == b"relative"
                    && attempt.request.base.is_none()
                    && attempt.outcome == CapturedSymbolicLinkCreationOutcome::Failed(libc::EBADF)
            })
            .unwrap();
        assert!(process
            .symbolic_link_creation_attempts
            .iter()
            .any(|attempt| {
                attempt.request.target.len() == crate::LINUX_PATH_MAX - 1
                    && attempt.request.target_terminated
                    && attempt.outcome == CapturedSymbolicLinkCreationOutcome::Created
            }));
        assert!(
            process
                .symbolic_link_creation_attempts
                .iter()
                .filter(|attempt| {
                    (attempt.request.target.len() == crate::LINUX_PATH_MAX
                        && !attempt.request.target_terminated)
                        || (attempt.request.link_path.len() == crate::LINUX_PATH_MAX
                            && !attempt.request.link_path_terminated)
                })
                .count()
                >= 4
        );
        assert!(process
            .symbolic_link_creation_attempts
            .iter()
            .any(|attempt| {
                attempt.request.target == b"cross-target"
                    && attempt.request.link_path == b"cross-link"
                    && attempt.outcome == CapturedSymbolicLinkCreationOutcome::Created
            }));

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["symbolic_link_creation_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());
        let mut missing_base = serde_json::to_value(&receipt).unwrap();
        missing_base["processes"][0]["symbolic_link_creation_attempts"][raw_index]["request"]
            ["base"] = serde_json::Value::Null;
        let missing_base: ProcessCaptureReceipt = serde_json::from_value(missing_base).unwrap();
        assert!(missing_base.validate_complete().is_err());
        let mut forged_success = serde_json::to_value(&receipt).unwrap();
        forged_success["processes"][0]["symbolic_link_creation_attempts"][invalid_index]
            ["outcome"] = serde_json::json!("created");
        let forged_success: ProcessCaptureReceipt = serde_json::from_value(forged_success).unwrap();
        assert!(forged_success.validate_complete().is_err());
    }

    #[test]
    fn captures_statx_results_requests_failures_and_bounds() {
        let root = tempfile::tempdir().unwrap();
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, errno, mmap, os, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "number = int(sys.argv[1])\n",
                "def pointer(value):\n",
                "    if value is None:\n",
                "        return ctypes.c_void_p(0), None\n",
                "    if isinstance(value, int):\n",
                "        return ctypes.c_void_p(value), None\n",
                "    keep = ctypes.create_string_buffer(value)\n",
                "    return ctypes.cast(keep, ctypes.c_void_p), keep\n",
                "def call(directory_fd, path, flags, mask, output=None):\n",
                "    pathp, pathkeep = pointer(path)\n",
                "    if output is None:\n",
                "        outputkeep = ctypes.create_string_buffer(256)\n",
                "        outputp = ctypes.cast(outputkeep, ctypes.c_void_p)\n",
                "    else:\n",
                "        outputp, outputkeep = pointer(output)\n",
                "    ctypes.set_errno(0)\n",
                "    value = libc.syscall(number, ctypes.c_int(directory_fd), pathp, ",
                "ctypes.c_uint(flags), ctypes.c_uint(mask), outputp)\n",
                "    return value, ctypes.get_errno()\n",
                "fd = os.open(b'value', os.O_CREAT | os.O_RDWR, 0o640)\n",
                "os.write(fd, b'data')\n",
                "os.mkdir(b'base')\n",
                "basefd = os.open(b'base', os.O_RDONLY | os.O_DIRECTORY)\n",
                "raw = b'raw-\\xff'\n",
                "rawfd = os.open(raw, os.O_CREAT | os.O_WRONLY, 0o600, dir_fd=basefd)\n",
                "os.close(rawfd)\n",
                "assert call(-100, b'value', 0x900, 0x7ff) == (0, 0)\n",
                "assert call(basefd, raw, 0x800, 0x7ff) == (0, 0)\n",
                "absolute = os.fsencode(os.getcwd()) + b'/value'\n",
                "assert call(-1, absolute, 0, 0x7ff) == (0, 0)\n",
                "assert call(fd, b'', 0x1000, 0xfff) == (0, 0)\n",
                "assert call(fd, None, 0x1000, 0xfff) in ((0, 0), (-1, errno.EFAULT))\n",
                "assert call(-100, b'', 0x1000, 0x7ff) == (0, 0)\n",
                "os.symlink(b'value', b'link')\n",
                "assert call(-100, b'link', 0x100, 0x7ff) == (0, 0)\n",
                "assert call(-100, b'missing', 0, 0x7ff) == (-1, errno.ENOENT)\n",
                "assert call(-100, None, 0, 0x7ff) == (-1, errno.EFAULT)\n",
                "assert call(-100, b'', 0, 0x7ff) == (-1, errno.ENOENT)\n",
                "assert call(-1, b'', 0x1000, 0x7ff) == (-1, errno.EBADF)\n",
                "plainfd = os.open(b'plain', os.O_CREAT | os.O_RDONLY, 0o600)\n",
                "assert call(plainfd, b'child', 0, 0x7ff) == (-1, errno.ENOTDIR)\n",
                "assert call(-100, b'value', 0x6000, 0x7ff) == (-1, errno.EINVAL)\n",
                "assert call(-100, b'value', 0, 0x80000000) == (-1, errno.EINVAL)\n",
                "assert call(-100, b'value', 0, 0x7ff, 0) == (-1, errno.EFAULT)\n",
                "assert call(-100, b'x' * 4095, 0, 0x7ff) == (-1, errno.ENAMETOOLONG)\n",
                "assert call(-100, b'x' * 4096, 0, 0x7ff) == (-1, errno.ENAMETOOLONG)\n",
                "page = mmap.PAGESIZE\n",
                "libc.mmap.restype = ctypes.c_void_p\n",
                "cross = libc.mmap(None, page * 2, 3, 0x22, -1, 0)\n",
                "assert cross not in (None, ctypes.c_void_p(-1).value)\n",
                "ctypes.memmove(cross + page - 2, b'value\\0', 6)\n",
                "assert call(-100, cross + page - 2, 0, 0x7ff) == (0, 0)\n",
                "assert libc.munmap(ctypes.c_void_p(cross), page * 2) == 0\n",
                "guarded = libc.mmap(None, page * 2, 3, 0x22, -1, 0)\n",
                "assert guarded not in (None, ctypes.c_void_p(-1).value)\n",
                "ctypes.memset(guarded, ord('g'), page)\n",
                "assert libc.mprotect(ctypes.c_void_p(guarded + page), page, 0) == 0\n",
                "assert call(-100, guarded, 0, 0x7ff) == (-1, errno.ENAMETOOLONG)\n",
                "assert libc.munmap(ctypes.c_void_p(guarded), page * 2) == 0\n",
                "output = libc.mmap(None, page * 2, 3, 0x22, -1, 0)\n",
                "assert output not in (None, ctypes.c_void_p(-1).value)\n",
                "assert libc.mprotect(ctypes.c_void_p(output + page), page, 0) == 0\n",
                "assert call(-100, b'value', 0, 0x7ff, output + page - 256) == (0, 0)\n",
                "assert libc.munmap(ctypes.c_void_p(output), page * 2) == 0\n",
                "os.close(plainfd)\n",
                "os.close(basefd)\n",
                "os.close(fd)\n",
            ))
            .arg(libc::SYS_statx.to_string())
            .current_dir(root.path())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(receipt.statx_syscalls().unwrap(), process.statx_syscalls);
        assert_eq!(
            usize::try_from(process.statx_syscalls).unwrap(),
            process.statx_attempts.len()
        );
        let relative_index = process
            .statx_attempts
            .iter()
            .position(|attempt| {
                attempt.request.directory_descriptor == libc::AT_FDCWD
                    && attempt.request.path == b"value"
                    && attempt.request.path_terminated
                    && attempt.request.base.as_deref() == Some(root.path().as_os_str().as_bytes())
                    && attempt.request.flags == 0x900
                    && attempt.request.mask == 0x7ff
                    && matches!(attempt.outcome, CapturedStatxOutcome::Metadata(_))
            })
            .unwrap();
        let CapturedStatxOutcome::Metadata(relative_metadata) =
            &process.statx_attempts[relative_index].outcome
        else {
            unreachable!();
        };
        let expected = std::fs::metadata(root.path().join("value")).unwrap();
        assert_eq!(u32::from(relative_metadata.mode), expected.mode());
        assert_eq!(relative_metadata.inode, expected.ino());
        assert_eq!(relative_metadata.size, expected.size());
        assert_eq!(relative_metadata.hard_links as u64, expected.nlink());
        assert!(relative_metadata.mask & 0x7ff == 0x7ff);
        assert!(relative_metadata.mount_id > 0);
        assert_eq!(relative_metadata.spare0, 0);
        assert_eq!(relative_metadata.spare2, 0);
        assert_eq!(relative_metadata.spare3, [0; 8]);
        let base = root.path().join("base");
        assert!(process.statx_attempts.iter().any(|attempt| {
            attempt.request.path == b"raw-\xff"
                && attempt.request.base.as_deref() == Some(base.as_os_str().as_bytes())
                && matches!(attempt.outcome, CapturedStatxOutcome::Metadata(_))
        }));
        assert!(process.statx_attempts.iter().any(|attempt| {
            attempt.request.directory_descriptor == -1
                && attempt.request.path.ends_with(b"/value")
                && attempt.request.base.is_none()
                && matches!(attempt.outcome, CapturedStatxOutcome::Metadata(_))
        }));
        let descriptor_index = process
            .statx_attempts
            .iter()
            .position(|attempt| {
                attempt.request.directory_descriptor >= 0
                    && attempt.request.path.is_empty()
                    && !attempt.request.path_is_null
                    && attempt.request.path_terminated
                    && attempt
                        .request
                        .descriptor_identity
                        .as_ref()
                        .is_some_and(|identity| {
                            identity.target.ends_with(b"/value")
                                && identity.kind == CapturedDescriptorKind::RegularFile
                        })
                    && attempt.request.descriptor_mount_id.is_some()
                    && matches!(attempt.outcome, CapturedStatxOutcome::Metadata(_))
            })
            .unwrap();
        let CapturedStatxOutcome::Metadata(descriptor_metadata) =
            &process.statx_attempts[descriptor_index].outcome
        else {
            unreachable!();
        };
        assert_eq!(
            process.statx_attempts[descriptor_index]
                .request
                .descriptor_mount_id,
            Some(descriptor_metadata.mount_id)
        );
        assert!(process.statx_attempts.iter().any(|attempt| {
            attempt.request.directory_descriptor >= 0
                && attempt.request.path_is_null
                && !attempt.request.path_terminated
                && attempt.request.descriptor_identity.is_some()
                && matches!(
                    &attempt.outcome,
                    CapturedStatxOutcome::Metadata(_) | CapturedStatxOutcome::Failed(libc::EFAULT)
                )
        }));
        assert!(process.statx_attempts.iter().any(|attempt| {
            attempt.request.directory_descriptor == libc::AT_FDCWD
                && attempt.request.path.is_empty()
                && attempt.request.path_terminated
                && attempt.request.base.as_deref() == Some(root.path().as_os_str().as_bytes())
                && matches!(attempt.outcome, CapturedStatxOutcome::Metadata(_))
        }));
        assert!(process.statx_attempts.iter().any(|attempt| {
            attempt.request.path == b"link"
                && attempt.request.flags == 0x100
                && matches!(
                    &attempt.outcome,
                    CapturedStatxOutcome::Metadata(metadata)
                        if u32::from(metadata.mode) & libc::S_IFMT == libc::S_IFLNK
                )
        }));
        for (path, errno) in [
            (&b"missing"[..], libc::ENOENT),
            (&b""[..], libc::ENOENT),
            (&b"child"[..], libc::ENOTDIR),
        ] {
            assert!(process.statx_attempts.iter().any(|attempt| {
                attempt.request.path == path
                    && attempt.outcome == CapturedStatxOutcome::Failed(errno)
            }));
        }
        assert!(process.statx_attempts.iter().any(|attempt| {
            attempt.request.path_is_null
                && attempt.request.flags == 0
                && attempt.outcome == CapturedStatxOutcome::Failed(libc::EFAULT)
        }));
        let invalid_descriptor_index = process
            .statx_attempts
            .iter()
            .position(|attempt| {
                attempt.request.directory_descriptor == -1
                    && attempt.request.path.is_empty()
                    && attempt.request.flags == 0x1000
                    && attempt.request.descriptor_identity.is_none()
                    && attempt.outcome == CapturedStatxOutcome::Failed(libc::EBADF)
            })
            .unwrap();
        assert!(process.statx_attempts.iter().any(|attempt| {
            attempt.request.flags == 0x6000
                && attempt.outcome == CapturedStatxOutcome::Failed(libc::EINVAL)
        }));
        assert!(process.statx_attempts.iter().any(|attempt| {
            attempt.request.mask == 0x8000_0000
                && attempt.outcome == CapturedStatxOutcome::Failed(libc::EINVAL)
        }));
        assert!(process.statx_attempts.iter().any(|attempt| {
            attempt.request.output_is_null
                && attempt.outcome == CapturedStatxOutcome::Failed(libc::EFAULT)
        }));
        assert!(process.statx_attempts.iter().any(|attempt| {
            attempt.request.path.len() == crate::LINUX_PATH_MAX - 1
                && attempt.request.path_terminated
                && attempt.outcome == CapturedStatxOutcome::Failed(libc::ENAMETOOLONG)
        }));
        assert!(
            process
                .statx_attempts
                .iter()
                .filter(|attempt| {
                    attempt.request.path.len() == crate::LINUX_PATH_MAX
                        && !attempt.request.path_terminated
                        && attempt.outcome == CapturedStatxOutcome::Failed(libc::ENAMETOOLONG)
                })
                .count()
                >= 2
        );
        assert!(process.statx_attempts.iter().any(|attempt| {
            attempt.request.path == b"value"
                && matches!(attempt.outcome, CapturedStatxOutcome::Metadata(_))
        }));

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["statx_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());
        let mut missing_identity = serde_json::to_value(&receipt).unwrap();
        missing_identity["processes"][0]["statx_attempts"][descriptor_index]["request"]
            ["descriptor_identity"] = serde_json::Value::Null;
        let missing_identity: ProcessCaptureReceipt =
            serde_json::from_value(missing_identity).unwrap();
        assert!(missing_identity.validate_complete().is_err());
        let mut wrong_mount = serde_json::to_value(&receipt).unwrap();
        wrong_mount["processes"][0]["statx_attempts"][descriptor_index]["request"]
            ["descriptor_mount_id"] = serde_json::json!(descriptor_metadata.mount_id + 1);
        let wrong_mount: ProcessCaptureReceipt = serde_json::from_value(wrong_mount).unwrap();
        assert!(wrong_mount.validate_complete().is_err());
        let mut wrong_device = serde_json::to_value(&receipt).unwrap();
        wrong_device["processes"][0]["statx_attempts"][descriptor_index]["request"]
            ["descriptor_identity"]["device"] = serde_json::json!(u64::MAX);
        let wrong_device: ProcessCaptureReceipt = serde_json::from_value(wrong_device).unwrap();
        assert!(wrong_device.validate_complete().is_err());
        let mut wrong_inode = serde_json::to_value(&receipt).unwrap();
        wrong_inode["processes"][0]["statx_attempts"][descriptor_index]["request"]
            ["descriptor_identity"]["inode"] = serde_json::json!(u64::MAX);
        let wrong_inode: ProcessCaptureReceipt = serde_json::from_value(wrong_inode).unwrap();
        assert!(wrong_inode.validate_complete().is_err());
        let mut forged_success = serde_json::to_value(&receipt).unwrap();
        forged_success["processes"][0]["statx_attempts"][invalid_descriptor_index]["outcome"] =
            serde_json::to_value(CapturedStatxOutcome::Metadata(relative_metadata.clone()))
                .unwrap();
        let forged_success: ProcessCaptureReceipt = serde_json::from_value(forged_success).unwrap();
        assert!(forged_success.validate_complete().is_err());
        let mut bad_reserved = serde_json::to_value(&receipt).unwrap();
        bad_reserved["processes"][0]["statx_attempts"][relative_index]["outcome"]["metadata"]
            ["spare3"][0] = serde_json::json!(1);
        let bad_reserved: ProcessCaptureReceipt = serde_json::from_value(bad_reserved).unwrap();
        assert!(bad_reserved.validate_complete().is_err());
        let mut bad_nanoseconds = serde_json::to_value(&receipt).unwrap();
        bad_nanoseconds["processes"][0]["statx_attempts"][relative_index]["outcome"]["metadata"]
            ["modification_time"]["nanoseconds"] = serde_json::json!(1_000_000_000u32);
        let bad_nanoseconds: ProcessCaptureReceipt =
            serde_json::from_value(bad_nanoseconds).unwrap();
        assert!(bad_nanoseconds.validate_complete().is_err());
        let mut unknown_result = serde_json::to_value(&receipt).unwrap();
        unknown_result["processes"][0]["statx_attempts"][relative_index]["outcome"]["metadata"]
            ["mask"] = serde_json::json!(0x0004_0000u32);
        let unknown_result: ProcessCaptureReceipt = serde_json::from_value(unknown_result).unwrap();
        assert!(unknown_result.validate_complete().is_err());
        let mut unsupported_attribute = serde_json::to_value(&receipt).unwrap();
        unsupported_attribute["processes"][0]["statx_attempts"][relative_index]["outcome"]
            ["metadata"]["attributes"] = serde_json::json!(1u64);
        let unsupported_attribute: ProcessCaptureReceipt =
            serde_json::from_value(unsupported_attribute).unwrap();
        assert!(unsupported_attribute.validate_complete().is_err());
    }

    #[test]
    fn captures_xattr_values_queries_failures_and_bounds() {
        let root = tempfile::tempdir().unwrap();
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, errno, mmap, os, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "get_number, lget_number, fget_number, fset_number = map(int, sys.argv[1:5])\n",
                "def pointer(value):\n",
                "    if value is None:\n",
                "        return ctypes.c_void_p(0), None\n",
                "    if isinstance(value, int):\n",
                "        return ctypes.c_void_p(value), None\n",
                "    keep = ctypes.create_string_buffer(value)\n",
                "    return ctypes.cast(keep, ctypes.c_void_p), keep\n",
                "def call(number, path, name, size, output='allocate'):\n",
                "    pathp, pathkeep = pointer(path)\n",
                "    namep, namekeep = pointer(name)\n",
                "    if output == 'allocate':\n",
                "        outputkeep = ctypes.create_string_buffer(max(size, 1))\n",
                "        outputp = ctypes.cast(outputkeep, ctypes.c_void_p)\n",
                "    else:\n",
                "        outputp, outputkeep = pointer(output)\n",
                "    ctypes.set_errno(0)\n",
                "    value = libc.syscall(number, pathp, namep, outputp, ctypes.c_size_t(size))\n",
                "    return value, ctypes.get_errno()\n",
                "def fcall(number, fd, name, size):\n",
                "    namep, namekeep = pointer(name)\n",
                "    outputkeep = ctypes.create_string_buffer(max(size, 1))\n",
                "    outputp = ctypes.cast(outputkeep, ctypes.c_void_p)\n",
                "    ctypes.set_errno(0)\n",
                "    value = libc.syscall(number, fd, namep, outputp, ctypes.c_size_t(size))\n",
                "    return value, ctypes.get_errno()\n",
                "def fsetcall(number, fd, name, value, flags=0):\n",
                "    namep, namekeep = pointer(name)\n",
                "    valuep, valuekeep = pointer(value)\n",
                "    ctypes.set_errno(0)\n",
                "    result = libc.syscall(number, fd, namep, valuep, ctypes.c_size_t(len(value)), flags)\n",
                "    return result, ctypes.get_errno()\n",
                "value = b'\\x00a\\xff'\n",
                "fd = os.open(b'value', os.O_CREAT | os.O_WRONLY, 0o600)\n",
                "os.close(fd)\n",
                "os.setxattr(b'value', b'user.rush', value)\n",
                "os.setxattr(b'value', b'user.empty', b'')\n",
                "fd = os.open(b'value', os.O_RDWR)\n",
                "assert fcall(fget_number, fd, b'user.rush', 3) == (3, 0)\n",
                "assert fcall(fget_number, fd, b'user.missing', 132) == (-1, errno.ENODATA)\n",
                "copied = b'\\xffcopy\\0'\n",
                "assert fsetcall(fset_number, fd, b'user.copy', copied) == (0, 0)\n",
                "assert os.getxattr(fd, b'user.copy') == copied\n",
                "os.close(fd)\n",
                "os.symlink(b'value', b'link')\n",
                "assert call(get_number, b'value', b'user.rush', 3) == (3, 0)\n",
                "assert call(lget_number, b'value', b'user.rush', 3) == (3, 0)\n",
                "assert call(get_number, b'link', b'user.rush', 3) == (3, 0)\n",
                "assert call(lget_number, b'link', b'user.rush', 3)[0] == -1\n",
                "assert call(get_number, b'value', b'user.rush', 0, None) == (3, 0)\n",
                "assert call(get_number, b'value', b'user.rush', 0) == (3, 0)\n",
                "assert call(get_number, b'value', b'user.empty', 1) == (0, 0)\n",
                "assert call(get_number, b'value', b'user.empty', 1, None) == (0, 0)\n",
                "assert call(get_number, b'value', b'user.rush', 2) == (-1, errno.ERANGE)\n",
                "raw = b'raw-\\xff'\n",
                "fd = os.open(raw, os.O_CREAT | os.O_WRONLY, 0o600)\n",
                "os.close(fd)\n",
                "os.setxattr(raw, b'user.rush', value)\n",
                "assert call(get_number, raw, b'user.rush', 3) == (3, 0)\n",
                "absolute = os.fsencode(os.getcwd()) + b'/value'\n",
                "assert call(get_number, absolute, b'user.rush', 3) == (3, 0)\n",
                "assert call(get_number, b'value', b'user.missing', 3) == (-1, errno.ENODATA)\n",
                "assert call(get_number, b'missing', b'user.rush', 3) == (-1, errno.ENOENT)\n",
                "assert call(get_number, None, b'user.rush', 3) == (-1, errno.EFAULT)\n",
                "assert call(get_number, b'value', None, 3) == (-1, errno.EFAULT)\n",
                "assert call(get_number, b'', b'user.rush', 3) == (-1, errno.ENOENT)\n",
                "empty_name = call(get_number, b'value', b'', 3)\n",
                "assert empty_name[0] == -1 and empty_name[1] > 0\n",
                "assert call(get_number, b'value', b'user.rush', 3, None) == (-1, errno.EFAULT)\n",
                "assert call(get_number, b'x' * 4095, b'user.rush', 3)[0] == -1\n",
                "assert call(get_number, b'x' * 4096, b'user.rush', 3) == (-1, errno.ENAMETOOLONG)\n",
                "name255 = b'user.' + b'x' * 250\n",
                "name256 = b'user.' + b'x' * 251\n",
                "assert call(get_number, b'value', name255, 3)[0] == -1\n",
                "assert call(get_number, b'value', name256, 3)[0] == -1\n",
                "page = mmap.PAGESIZE\n",
                "libc.mmap.restype = ctypes.c_void_p\n",
                "cross_path = libc.mmap(None, page * 2, 3, 0x22, -1, 0)\n",
                "assert cross_path not in (None, ctypes.c_void_p(-1).value)\n",
                "ctypes.memmove(cross_path + page - 2, b'value\\0', 6)\n",
                "assert call(get_number, cross_path + page - 2, b'user.rush', 3) == (3, 0)\n",
                "assert libc.munmap(ctypes.c_void_p(cross_path), page * 2) == 0\n",
                "cross_name = libc.mmap(None, page * 2, 3, 0x22, -1, 0)\n",
                "assert cross_name not in (None, ctypes.c_void_p(-1).value)\n",
                "ctypes.memmove(cross_name + page - 3, b'user.rush\\0', 10)\n",
                "assert call(get_number, b'value', cross_name + page - 3, 3) == (3, 0)\n",
                "assert libc.munmap(ctypes.c_void_p(cross_name), page * 2) == 0\n",
                "guard_path = libc.mmap(None, page * 2, 3, 0x22, -1, 0)\n",
                "assert guard_path not in (None, ctypes.c_void_p(-1).value)\n",
                "ctypes.memset(guard_path, ord('p'), page)\n",
                "assert libc.mprotect(ctypes.c_void_p(guard_path + page), page, 0) == 0\n",
                "assert call(get_number, guard_path, b'user.rush', 3) == (-1, errno.ENAMETOOLONG)\n",
                "assert libc.munmap(ctypes.c_void_p(guard_path), page * 2) == 0\n",
                "guard_name = libc.mmap(None, page * 2, 3, 0x22, -1, 0)\n",
                "assert guard_name not in (None, ctypes.c_void_p(-1).value)\n",
                "ctypes.memset(guard_name + page - 256, ord('n'), 256)\n",
                "assert libc.mprotect(ctypes.c_void_p(guard_name + page), page, 0) == 0\n",
                "assert call(get_number, b'value', guard_name + page - 256, 3)[0] == -1\n",
                "assert libc.munmap(ctypes.c_void_p(guard_name), page * 2) == 0\n",
                "output = libc.mmap(None, page * 2, 3, 0x22, -1, 0)\n",
                "assert output not in (None, ctypes.c_void_p(-1).value)\n",
                "assert libc.mprotect(ctypes.c_void_p(output + page), page, 0) == 0\n",
                "assert call(get_number, b'value', b'user.rush', 3, output + page - 3) == (3, 0)\n",
                "assert libc.munmap(ctypes.c_void_p(output), page * 2) == 0\n",
            ))
            .arg(libc::SYS_getxattr.to_string())
            .arg(libc::SYS_lgetxattr.to_string())
            .arg(libc::SYS_fgetxattr.to_string())
            .arg(libc::SYS_fsetxattr.to_string())
            .current_dir(root.path())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(receipt.xattr_syscalls().unwrap(), process.xattr_syscalls);
        assert_eq!(
            usize::try_from(process.xattr_syscalls).unwrap(),
            process.xattr_attempts.len()
        );
        let value_index = process
            .xattr_attempts
            .iter()
            .position(|attempt| {
                attempt.request.system_call == CapturedXattrSystemCall::Getxattr
                    && attempt.request.path == b"value"
                    && attempt.request.path_terminated
                    && attempt.request.base.as_deref() == Some(root.path().as_os_str().as_bytes())
                    && attempt.request.name == b"user.rush"
                    && attempt.request.name_terminated
                    && attempt.request.requested_bytes == 3
                    && !attempt.request.value_is_null
                    && attempt.outcome
                        == CapturedXattrOutcome::Value {
                            returned_bytes: 3,
                            bytes: Some(b"\0a\xff".to_vec()),
                        }
            })
            .unwrap();
        assert!(process.xattr_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedXattrSystemCall::Fgetxattr
                && attempt.request.descriptor.is_some_and(|descriptor| descriptor >= 0)
                && attempt
                    .request
                    .descriptor_identity
                    .as_ref()
                    .is_some_and(|identity| identity.kind == CapturedDescriptorKind::RegularFile)
                && attempt.request.descriptor_mount_id.is_some()
                && attempt.request.name == b"user.missing"
                && attempt.request.requested_bytes == 132
                && attempt.outcome == CapturedXattrOutcome::Failed(libc::ENODATA)
        }));
        assert!(process.xattr_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedXattrSystemCall::Fsetxattr
                && attempt.request.descriptor.is_some_and(|descriptor| descriptor >= 0)
                && attempt
                    .request
                    .descriptor_identity
                    .as_ref()
                    .is_some_and(|identity| identity.kind == CapturedDescriptorKind::RegularFile)
                && attempt.request.descriptor_mount_id.is_some()
                && attempt.request.name == b"user.copy"
                && attempt.request.requested_bytes == 6
                && !attempt.request.value_is_null
                && attempt.request.set_value.as_deref() == Some(b"\xffcopy\0")
                && attempt.request.flags == 0
                && attempt.outcome == CapturedXattrOutcome::Changed
        }));
        assert!(process.xattr_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedXattrSystemCall::Lgetxattr
                && attempt.request.path == b"value"
                && matches!(
                    &attempt.outcome,
                    CapturedXattrOutcome::Value {
                        returned_bytes: 3,
                        bytes: Some(bytes),
                    } if bytes == b"\0a\xff"
                )
        }));
        assert!(process.xattr_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedXattrSystemCall::Getxattr
                && attempt.request.path == b"link"
                && matches!(attempt.outcome, CapturedXattrOutcome::Value { .. })
        }));
        assert!(process.xattr_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedXattrSystemCall::Lgetxattr
                && attempt.request.path == b"link"
                && attempt.outcome == CapturedXattrOutcome::Failed(libc::ENODATA)
        }));
        assert_eq!(
            process
                .xattr_attempts
                .iter()
                .filter(|attempt| {
                    attempt.request.path == b"value"
                        && attempt.request.name == b"user.rush"
                        && attempt.request.requested_bytes == 0
                        && matches!(
                            attempt.outcome,
                            CapturedXattrOutcome::Value {
                                returned_bytes: 3,
                                bytes: None,
                            }
                        )
                })
                .count(),
            2
        );
        assert!(process.xattr_attempts.iter().any(|attempt| {
            attempt.request.name == b"user.empty"
                && !attempt.request.value_is_null
                && attempt.outcome
                    == (CapturedXattrOutcome::Value {
                        returned_bytes: 0,
                        bytes: Some(Vec::new()),
                    })
        }));
        assert!(process.xattr_attempts.iter().any(|attempt| {
            attempt.request.name == b"user.empty"
                && attempt.request.value_is_null
                && attempt.outcome
                    == (CapturedXattrOutcome::Value {
                        returned_bytes: 0,
                        bytes: None,
                    })
        }));
        assert!(process.xattr_attempts.iter().any(|attempt| {
            attempt.request.path == b"raw-\xff"
                && attempt.request.base.as_deref() == Some(root.path().as_os_str().as_bytes())
                && matches!(attempt.outcome, CapturedXattrOutcome::Value { .. })
        }));
        assert!(process.xattr_attempts.iter().any(|attempt| {
            attempt.request.path.ends_with(b"/value")
                && attempt.request.path.first() == Some(&b'/')
                && attempt.request.base.is_none()
                && matches!(attempt.outcome, CapturedXattrOutcome::Value { .. })
        }));
        assert!(process.xattr_attempts.iter().any(|attempt| {
            attempt.request.path_is_null
                && !attempt.request.path_terminated
                && attempt.outcome == CapturedXattrOutcome::Failed(libc::EFAULT)
        }));
        let null_path_index = process
            .xattr_attempts
            .iter()
            .position(|attempt| {
                attempt.request.system_call != CapturedXattrSystemCall::Fgetxattr
                    && attempt.request.path_is_null
            })
            .unwrap();
        let size_only_index = process
            .xattr_attempts
            .iter()
            .position(|attempt| {
                attempt.request.path == b"value"
                    && attempt.request.name == b"user.rush"
                    && attempt.request.requested_bytes == 0
                    && attempt.request.value_is_null
            })
            .unwrap();
        assert!(process.xattr_attempts.iter().any(|attempt| {
            attempt.request.name_is_null
                && !attempt.request.name_terminated
                && attempt.outcome == CapturedXattrOutcome::Failed(libc::EFAULT)
        }));
        assert!(process.xattr_attempts.iter().any(|attempt| {
            attempt.request.value_is_null
                && attempt.request.requested_bytes == 3
                && attempt.outcome == CapturedXattrOutcome::Failed(libc::EFAULT)
        }));
        assert!(process.xattr_attempts.iter().any(|attempt| {
            attempt.request.path.len() == crate::LINUX_PATH_MAX - 1
                && attempt.request.path_terminated
                && matches!(attempt.outcome, CapturedXattrOutcome::Failed(_))
        }));
        assert!(
            process
                .xattr_attempts
                .iter()
                .filter(|attempt| {
                    attempt.request.path.len() == crate::LINUX_PATH_MAX
                        && !attempt.request.path_terminated
                        && attempt.outcome == CapturedXattrOutcome::Failed(libc::ENAMETOOLONG)
                })
                .count()
                >= 2
        );
        assert!(process.xattr_attempts.iter().any(|attempt| {
            attempt.request.name.len() == crate::LINUX_XATTR_NAME_MAX
                && attempt.request.name_terminated
                && matches!(attempt.outcome, CapturedXattrOutcome::Failed(_))
        }));
        assert!(
            process
                .xattr_attempts
                .iter()
                .filter(|attempt| {
                    attempt.request.name.len() == crate::LINUX_XATTR_NAME_MAX + 1
                        && !attempt.request.name_terminated
                        && matches!(attempt.outcome, CapturedXattrOutcome::Failed(_))
                })
                .count()
                >= 2
        );

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["xattr_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());
        let mut missing_base = serde_json::to_value(&receipt).unwrap();
        missing_base["processes"][0]["xattr_attempts"][value_index]["request"]["base"] =
            serde_json::Value::Null;
        let missing_base: ProcessCaptureReceipt = serde_json::from_value(missing_base).unwrap();
        assert!(missing_base.validate_complete().is_err());
        let mut forged_null_path = serde_json::to_value(&receipt).unwrap();
        forged_null_path["processes"][0]["xattr_attempts"][null_path_index]["outcome"] =
            serde_json::to_value(CapturedXattrOutcome::Value {
                returned_bytes: 0,
                bytes: Some(Vec::new()),
            })
            .unwrap();
        let forged_null_path: ProcessCaptureReceipt =
            serde_json::from_value(forged_null_path).unwrap();
        assert!(forged_null_path.validate_complete().is_err());
        let mut forged_size_query = serde_json::to_value(&receipt).unwrap();
        forged_size_query["processes"][0]["xattr_attempts"][size_only_index]["outcome"]["value"]
            ["bytes"] = serde_json::json!([]);
        let forged_size_query: ProcessCaptureReceipt =
            serde_json::from_value(forged_size_query).unwrap();
        assert!(forged_size_query.validate_complete().is_err());
        let mut wrong_return = serde_json::to_value(&receipt).unwrap();
        wrong_return["processes"][0]["xattr_attempts"][value_index]["outcome"]["value"]
            ["returned_bytes"] = serde_json::json!(2u64);
        let wrong_return: ProcessCaptureReceipt = serde_json::from_value(wrong_return).unwrap();
        assert!(wrong_return.validate_complete().is_err());
        let mut missing_value = serde_json::to_value(&receipt).unwrap();
        missing_value["processes"][0]["xattr_attempts"][value_index]["outcome"]["value"]["bytes"] =
            serde_json::Value::Null;
        let missing_value: ProcessCaptureReceipt = serde_json::from_value(missing_value).unwrap();
        assert!(missing_value.validate_complete().is_err());
        let mut oversized = serde_json::to_value(&receipt).unwrap();
        oversized["processes"][0]["xattr_attempts"][value_index]["outcome"]["value"]
            ["returned_bytes"] = serde_json::json!(65_537u64);
        let oversized: ProcessCaptureReceipt = serde_json::from_value(oversized).unwrap();
        assert!(oversized.validate_complete().is_err());
    }

    #[test]
    fn decodes_the_native_filesystem_status_uapi_layout() {
        require_native_filesystem_status_layout().unwrap();
        let mut bytes = [0u8; NATIVE_FILESYSTEM_STATUS_BYTES];
        let i64_fields = [
            (0, -0x0102_0304_0506_0708i64),
            (8, 0x1112_1314_1516_1718i64),
            (64, -0x2122_2324_2526_2728i64),
            (72, 0x3132_3334_3536_3738i64),
            (80, -0x4142_4344_4546_4748i64),
            (88, 0x5152_5354_5556_5758i64),
            (96, -0x6162_6364_6566_6768i64),
            (104, 0x7172_7374_7576_7778i64),
            (112, i64::MIN + 79),
        ];
        for (offset, value) in i64_fields {
            bytes[offset..offset + 8].copy_from_slice(&value.to_ne_bytes());
        }
        let u64_fields = [
            (16, u64::MAX - 1),
            (24, 0x8182_8384_8586_8788),
            (32, 0x9192_9394_9596_9798),
            (40, 0xa1a2_a3a4_a5a6_a7a8),
            (48, 0xb1b2_b3b4_b5b6_b7b8),
        ];
        for (offset, value) in u64_fields {
            bytes[offset..offset + 8].copy_from_slice(&value.to_ne_bytes());
        }
        bytes[56..60].copy_from_slice(&(-0x0123_4567_i32).to_ne_bytes());
        bytes[60..64].copy_from_slice(&i32::MAX.to_ne_bytes());

        assert_eq!(
            decode_native_filesystem_status(&bytes).unwrap(),
            CapturedFilesystemStatus {
                filesystem_type: -0x0102_0304_0506_0708,
                block_size: 0x1112_1314_1516_1718,
                blocks: u64::MAX - 1,
                blocks_free: 0x8182_8384_8586_8788,
                blocks_available: 0x9192_9394_9596_9798,
                files: 0xa1a2_a3a4_a5a6_a7a8,
                files_free: 0xb1b2_b3b4_b5b6_b7b8,
                filesystem_id: [-0x0123_4567, i32::MAX],
                maximum_name_length: -0x2122_2324_2526_2728,
                fragment_size: 0x3132_3334_3536_3738,
                mount_flags: -0x4142_4344_4546_4748,
                reserved_words: [
                    0x5152_5354_5556_5758,
                    -0x6162_6364_6566_6768,
                    0x7172_7374_7576_7778,
                    i64::MIN + 79,
                ],
            }
        );
        assert!(decode_native_filesystem_status(&bytes[..119]).is_err());
    }

    #[test]
    fn captures_statfs_and_fstatfs_results_failures_and_denominator() {
        let root = tempfile::tempdir().unwrap();
        let expected_path = root.path().join("expected-statfs.bin");
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, errno, mmap, os, sys\n",
                "statfs_nr, fstatfs_nr = map(int, sys.argv[1:3])\n",
                "expected = os.fsencode(sys.argv[3])\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "def statfs(path, output=None):\n",
                "    keep = None\n",
                "    if isinstance(path, bytes):\n",
                "        keep = ctypes.create_string_buffer(path)\n",
                "        path = ctypes.cast(keep, ctypes.c_void_p)\n",
                "    elif path is None:\n",
                "        path = ctypes.c_void_p(0)\n",
                "    else:\n",
                "        path = ctypes.c_void_p(path)\n",
                "    if output is None:\n",
                "        output = ctypes.create_string_buffer(120)\n",
                "    pointer = output if isinstance(output, ctypes.Array) ",
                "else ctypes.c_void_p(output)\n",
                "    ctypes.set_errno(0)\n",
                "    value = libc.syscall(statfs_nr, path, pointer)\n",
                "    return value, ctypes.get_errno(), bytes(output) ",
                "if isinstance(output, ctypes.Array) else b''\n",
                "def fstatfs(fd, output=None):\n",
                "    if output is None:\n",
                "        output = ctypes.create_string_buffer(120)\n",
                "    pointer = output if isinstance(output, ctypes.Array) ",
                "else ctypes.c_void_p(output)\n",
                "    ctypes.set_errno(0)\n",
                "    value = libc.syscall(fstatfs_nr, ctypes.c_int(fd), pointer)\n",
                "    return value, ctypes.get_errno(), bytes(output) ",
                "if isinstance(output, ctypes.Array) else b''\n",
                "raw = b'status-\\xff'\n",
                "os.mkdir(raw)\n",
                "value, error, result = statfs(raw)\n",
                "assert (value, error) == (0, 0)\n",
                "fd = os.open(expected, os.O_CREAT | os.O_WRONLY, 0o600)\n",
                "assert os.write(fd, result) == 120\n",
                "os.close(fd)\n",
                "root = os.fsencode(os.getcwd())\n",
                "assert statfs(root)[0:2] == (0, 0)\n",
                "directory = os.open(b'.', os.O_RDONLY | os.O_DIRECTORY)\n",
                "assert fstatfs(directory)[0:2] == (0, 0)\n",
                "regular = os.open(expected, os.O_RDONLY)\n",
                "assert fstatfs(regular)[0:2] == (0, 0)\n",
                "closed = os.dup(regular)\n",
                "os.close(closed)\n",
                "assert fstatfs(-1)[0:2] == (-1, errno.EBADF)\n",
                "assert fstatfs(closed)[0:2] == (-1, errno.EBADF)\n",
                "assert statfs(b'missing')[0:2] == (-1, errno.ENOENT)\n",
                "assert statfs(b'')[0:2] == (-1, errno.ENOENT)\n",
                "assert statfs(None)[0:2] == (-1, errno.EFAULT)\n",
                "file_fd = os.open(b'file', os.O_CREAT | os.O_WRONLY, 0o600)\n",
                "os.close(file_fd)\n",
                "assert statfs(b'file/child')[0:2] == (-1, errno.ENOTDIR)\n",
                "assert statfs(raw, 0)[0:2] == (-1, errno.EFAULT)\n",
                "assert statfs(raw, 1)[0:2] == (-1, errno.EFAULT)\n",
                "assert fstatfs(regular, 0)[0:2] == (-1, errno.EFAULT)\n",
                "assert statfs(b'x' * 4095)[0] == -1\n",
                "assert statfs(b'x' * 4096)[0:2] == (-1, errno.ENAMETOOLONG)\n",
                "page = mmap.PAGESIZE\n",
                "libc.mmap.restype = ctypes.c_void_p\n",
                "region = libc.mmap(None, page * 2, 3, 0x22, -1, 0)\n",
                "assert region not in (None, ctypes.c_void_p(-1).value)\n",
                "ctypes.memset(region, ord('g'), page)\n",
                "assert libc.mprotect(ctypes.c_void_p(region + page), page, 0) == 0\n",
                "assert statfs(region)[0:2] == (-1, errno.ENAMETOOLONG)\n",
                "assert libc.munmap(ctypes.c_void_p(region), page * 2) == 0\n",
                "cross = libc.mmap(None, page * 2, 3, 0x22, -1, 0)\n",
                "assert cross not in (None, ctypes.c_void_p(-1).value)\n",
                "assert statfs(raw, cross + page - 60)[0:2] == (0, 0)\n",
                "assert libc.munmap(ctypes.c_void_p(cross), page * 2) == 0\n",
                "os.close(regular)\n",
                "os.close(directory)\n",
            ))
            .arg(libc::SYS_statfs.to_string())
            .arg(libc::SYS_fstatfs.to_string())
            .arg(&expected_path)
            .current_dir(root.path())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(
            receipt.filesystem_status_syscalls().unwrap(),
            process.filesystem_status_syscalls
        );
        assert_eq!(
            usize::try_from(process.filesystem_status_syscalls).unwrap(),
            process.filesystem_status_attempts.len()
        );
        let expected =
            decode_native_filesystem_status(&std::fs::read(&expected_path).unwrap()).unwrap();
        let relative_index = process
            .filesystem_status_attempts
            .iter()
            .position(|attempt| {
                attempt.request.system_call == CapturedFilesystemStatusSystemCall::Statfs
                    && attempt.request.path == b"status-\xff"
                    && attempt.request.path_terminated
                    && attempt.request.base.as_deref() == Some(root.path().as_os_str().as_bytes())
                    && attempt.outcome
                        == CapturedFilesystemStatusOutcome::Observed(expected.clone())
            })
            .unwrap();
        assert!(process.filesystem_status_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedFilesystemStatusSystemCall::Statfs
                && attempt.request.path == root.path().as_os_str().as_bytes()
                && attempt.request.base.is_none()
                && matches!(
                    attempt.outcome,
                    CapturedFilesystemStatusOutcome::Observed(_)
                )
        }));
        assert!(process.filesystem_status_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedFilesystemStatusSystemCall::Fstatfs
                && attempt
                    .request
                    .descriptor_identity
                    .as_ref()
                    .is_some_and(|identity| {
                        identity.kind == crate::CapturedDescriptorKind::Directory
                            && identity.target == root.path().as_os_str().as_bytes()
                    })
                && attempt.request.descriptor_mount_id.is_some()
                && matches!(
                    attempt.outcome,
                    CapturedFilesystemStatusOutcome::Observed(_)
                )
        }));
        assert!(process.filesystem_status_attempts.iter().any(|attempt| {
            attempt.request.system_call == CapturedFilesystemStatusSystemCall::Fstatfs
                && attempt
                    .request
                    .descriptor_identity
                    .as_ref()
                    .is_some_and(|identity| {
                        identity.kind == crate::CapturedDescriptorKind::RegularFile
                            && identity.target == expected_path.as_os_str().as_bytes()
                    })
                && attempt.request.descriptor_mount_id.is_some()
                && matches!(
                    attempt.outcome,
                    CapturedFilesystemStatusOutcome::Observed(_)
                )
        }));
        for (path, errno) in [
            (&b"missing"[..], libc::ENOENT),
            (&b""[..], libc::ENOENT),
            (&b"file/child"[..], libc::ENOTDIR),
        ] {
            assert!(process.filesystem_status_attempts.iter().any(|attempt| {
                attempt.request.path == path
                    && attempt.outcome == CapturedFilesystemStatusOutcome::Failed(errno)
            }));
        }
        assert!(process.filesystem_status_attempts.iter().any(|attempt| {
            attempt.request.path_is_null
                && attempt.outcome == CapturedFilesystemStatusOutcome::Failed(libc::EFAULT)
        }));
        assert_eq!(
            process
                .filesystem_status_attempts
                .iter()
                .filter(|attempt| {
                    attempt.request.system_call == CapturedFilesystemStatusSystemCall::Fstatfs
                        && attempt.request.descriptor_identity.is_none()
                        && attempt.outcome == CapturedFilesystemStatusOutcome::Failed(libc::EBADF)
                })
                .count(),
            2
        );
        assert_eq!(
            process
                .filesystem_status_attempts
                .iter()
                .filter(|attempt| {
                    attempt.request.output_is_null
                        && attempt.outcome == CapturedFilesystemStatusOutcome::Failed(libc::EFAULT)
                })
                .count(),
            2
        );
        assert!(process.filesystem_status_attempts.iter().any(|attempt| {
            attempt.request.path.len() == crate::LINUX_PATH_MAX - 1
                && attempt.request.path_terminated
        }));
        assert_eq!(
            process
                .filesystem_status_attempts
                .iter()
                .filter(|attempt| {
                    attempt.request.path.len() == crate::LINUX_PATH_MAX
                        && !attempt.request.path_terminated
                        && attempt.outcome
                            == CapturedFilesystemStatusOutcome::Failed(libc::ENAMETOOLONG)
                })
                .count(),
            2
        );

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["filesystem_status_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());
        let mut missing_base = serde_json::to_value(&receipt).unwrap();
        missing_base["processes"][0]["filesystem_status_attempts"][relative_index]["request"]
            ["base"] = serde_json::Value::Null;
        let missing_base: ProcessCaptureReceipt = serde_json::from_value(missing_base).unwrap();
        assert!(missing_base.validate_complete().is_err());
        let null_output_index = process
            .filesystem_status_attempts
            .iter()
            .position(|attempt| attempt.request.output_is_null)
            .unwrap();
        let mut forged_output = serde_json::to_value(&receipt).unwrap();
        forged_output["processes"][0]["filesystem_status_attempts"][null_output_index]["outcome"] =
            serde_json::to_value(CapturedFilesystemStatusOutcome::Observed(expected)).unwrap();
        let forged_output: ProcessCaptureReceipt = serde_json::from_value(forged_output).unwrap();
        assert!(forged_output.validate_complete().is_err());
    }

    #[test]
    fn captures_pread_bytes_and_failed_reads() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input");
        let contents = b"0123456789";
        std::fs::write(&input, contents).unwrap();
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(
                "import ctypes, os, sys; \
                 fd = os.open(sys.argv[1], os.O_RDONLY); \
                 assert os.pread(fd, 4, 3) == b'3456'; \
                 buffer = ctypes.create_string_buffer(1); \
                 libc = ctypes.CDLL(None, use_errno=True); \
                 assert libc.read(-1, buffer, 1) == -1; \
                 assert ctypes.get_errno() == 9; \
                 os.close(fd)",
            )
            .arg(&input)
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        let pread = process
            .read_attempts
            .iter()
            .find(|attempt| {
                attempt.request.system_call == CapturedReadSystemCall::Pread64
                    && attempt
                        .request
                        .identity
                        .as_ref()
                        .is_some_and(|identity| identity.target == input.as_os_str().as_bytes())
            })
            .unwrap();
        assert_eq!(pread.request.offset, Some(3));
        assert!(!pread.request.uses_descriptor_offset);
        assert_eq!(pread.request.buffer_lengths, [4]);
        assert_eq!(pread.request.requested_bytes, 4);
        assert_eq!(pread.request.flags, 0);
        assert!(matches!(
            pread.outcome,
            CapturedReadOutcome::Read {
                bytes: 4,
                digest,
                file_value,
            }
                if digest == *blake3::hash(&contents[3..7]).as_bytes()
                    && whole_file_value_matches(file_value, contents)
        ));
        let failed = process
            .read_attempts
            .iter()
            .find(|attempt| attempt.request.descriptor == -1)
            .unwrap();
        assert_eq!(failed.request.identity, None);
        assert_eq!(failed.outcome, CapturedReadOutcome::Failed(libc::EBADF));

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["read_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());
    }

    #[test]
    fn captures_vectored_read_bytes_and_positions() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input");
        let contents = b"0123456789abcdef";
        std::fs::write(&input, contents).unwrap();
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(
                "import ctypes, os, sys\n\
                 fd = os.open(sys.argv[1], os.O_RDONLY)\n\
                 buffers = [bytearray(2), bytearray(3)]\n\
                 assert os.readv(fd, buffers) == 5\n\
                 assert b''.join(buffers) == b'01234'\n\
                 Iovec = type('Iovec', (ctypes.Structure,),\n\
                     {'_fields_': [('base', ctypes.c_void_p),\n\
                                   ('length', ctypes.c_size_t)]})\n\
                 left = ctypes.create_string_buffer(1)\n\
                 right = ctypes.create_string_buffer(4)\n\
                 vectors = (Iovec * 2)(\n\
                     Iovec(ctypes.addressof(left), 1),\n\
                     Iovec(ctypes.addressof(right), 4))\n\
                 libc = ctypes.CDLL(None, use_errno=True)\n\
                 libc.syscall.restype = ctypes.c_long\n\
                 result = libc.syscall(int(sys.argv[2]), ctypes.c_ulong(fd),\n\
                     ctypes.byref(vectors), ctypes.c_ulong(2),\n\
                     ctypes.c_ulong(7), ctypes.c_ulong(0))\n\
                 assert result == 5, (result, ctypes.get_errno())\n\
                 assert left.raw + right.raw == b'789ab'\n\
                 os.lseek(fd, 10, os.SEEK_SET)\n\
                 left = ctypes.create_string_buffer(2)\n\
                 right = ctypes.create_string_buffer(3)\n\
                 vectors = (Iovec * 2)(\n\
                     Iovec(ctypes.addressof(left), 2),\n\
                     Iovec(ctypes.addressof(right), 3))\n\
                 result = libc.syscall(int(sys.argv[3]), ctypes.c_ulong(fd),\n\
                     ctypes.byref(vectors), ctypes.c_ulong(2),\n\
                     ctypes.c_ulong(-1).value, ctypes.c_ulong(0),\n\
                     ctypes.c_uint(0))\n\
                 assert result == 5, (result, ctypes.get_errno())\n\
                 assert left.raw + right.raw == b'abcde'\n\
                 rejected = (Iovec * 1)(Iovec(ctypes.addressof(left), 1))\n\
                 result = libc.syscall(int(sys.argv[3]), ctypes.c_ulong(fd),\n\
                     ctypes.byref(rejected), ctypes.c_ulong(1),\n\
                     ctypes.c_ulong(0), ctypes.c_ulong(0),\n\
                     ctypes.c_uint(0x80000000))\n\
                 assert result == -1 and ctypes.get_errno() != 0\n\
                 os.close(fd)",
            )
            .arg(&input)
            .arg(libc::SYS_preadv.to_string())
            .arg(libc::SYS_preadv2.to_string())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        let attempts = process
            .read_attempts
            .iter()
            .filter(|attempt| {
                attempt
                    .request
                    .identity
                    .as_ref()
                    .is_some_and(|identity| identity.target == input.as_os_str().as_bytes())
            })
            .collect::<Vec<_>>();
        let readv = attempts
            .iter()
            .find(|attempt| attempt.request.system_call == CapturedReadSystemCall::Readv)
            .unwrap();
        assert!(readv.request.uses_descriptor_offset);
        assert_eq!(readv.request.offset, Some(0));
        assert_eq!(readv.request.buffer_lengths, [2, 3]);
        assert_eq!(readv.request.requested_bytes, 5);
        assert_eq!(readv.request.flags, 0);
        assert!(matches!(
            readv.outcome,
            CapturedReadOutcome::Read {
                bytes: 5,
                digest,
                file_value,
            }
                if digest == *blake3::hash(&contents[..5]).as_bytes()
                    && whole_file_value_matches(file_value, contents)
        ));
        let preadv = attempts
            .iter()
            .find(|attempt| attempt.request.system_call == CapturedReadSystemCall::Preadv)
            .unwrap();
        assert!(!preadv.request.uses_descriptor_offset);
        assert_eq!(preadv.request.offset, Some(7));
        assert_eq!(preadv.request.buffer_lengths, [1, 4]);
        assert!(matches!(
            preadv.outcome,
            CapturedReadOutcome::Read {
                bytes: 5,
                digest,
                file_value,
            }
                if digest == *blake3::hash(&contents[7..12]).as_bytes()
                    && whole_file_value_matches(file_value, contents)
        ));
        let preadv2 = attempts
            .iter()
            .find(|attempt| attempt.request.system_call == CapturedReadSystemCall::Preadv2)
            .unwrap();
        assert!(preadv2.request.uses_descriptor_offset);
        assert_eq!(preadv2.request.offset, Some(10));
        assert_eq!(preadv2.request.buffer_lengths, [2, 3]);
        assert_eq!(preadv2.request.flags, 0);
        assert!(matches!(
            preadv2.outcome,
            CapturedReadOutcome::Read {
                bytes: 5,
                digest,
                file_value,
            }
                if digest == *blake3::hash(&contents[10..15]).as_bytes()
                    && whole_file_value_matches(file_value, contents)
        ));
        let rejected = attempts
            .iter()
            .find(|attempt| {
                attempt.request.system_call == CapturedReadSystemCall::Preadv2
                    && attempt.request.flags == 0x8000_0000
            })
            .unwrap();
        assert!(!rejected.request.uses_descriptor_offset);
        assert_eq!(rejected.request.offset, Some(0));
        assert_eq!(rejected.request.buffer_lengths, [1]);
        assert!(matches!(rejected.outcome, CapturedReadOutcome::Failed(errno) if errno > 0));

        let readv_index = process
            .read_attempts
            .iter()
            .position(|attempt| attempt.request.system_call == CapturedReadSystemCall::Readv)
            .unwrap();
        let mut malformed = serde_json::to_value(&receipt).unwrap();
        malformed["processes"][0]["read_attempts"][readv_index]["request"]["buffer_lengths"] =
            serde_json::json!([2]);
        let malformed: ProcessCaptureReceipt = serde_json::from_value(malformed).unwrap();
        assert!(malformed.validate_complete().is_err());
    }

    #[test]
    fn captures_vectored_read_boundaries_and_failures() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input");
        let contents = b"abcde";
        std::fs::write(&input, contents).unwrap();
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(
                "import ctypes, os, sys\n\
                 fd = os.open(sys.argv[1], os.O_RDONLY)\n\
                 Iovec = type('Iovec', (ctypes.Structure,),\n\
                     {'_fields_': [('base', ctypes.c_void_p),\n\
                                   ('length', ctypes.c_size_t)]})\n\
                 first = ctypes.create_string_buffer(2)\n\
                 middle = ctypes.create_string_buffer(3)\n\
                 last = ctypes.create_string_buffer(4)\n\
                 vectors = (Iovec * 4)(\n\
                     Iovec(ctypes.addressof(first), 2), Iovec(None, 0),\n\
                     Iovec(ctypes.addressof(middle), 3),\n\
                     Iovec(ctypes.addressof(last), 4))\n\
                 libc = ctypes.CDLL(None, use_errno=True)\n\
                 libc.syscall.restype = ctypes.c_long\n\
                 result = libc.syscall(int(sys.argv[2]), ctypes.c_ulong(fd),\n\
                     ctypes.byref(vectors), ctypes.c_ulong(4))\n\
                 assert result == 5 and first.raw + middle.raw == b'abcde'\n\
                 result = libc.syscall(int(sys.argv[2]), ctypes.c_ulong(fd),\n\
                     ctypes.c_void_p(0), ctypes.c_ulong(0))\n\
                 assert result == 0\n\
                 valid = (Iovec * 1)(Iovec(ctypes.addressof(first), 1))\n\
                 result = libc.syscall(int(sys.argv[2]), ctypes.c_ulong(-1).value,\n\
                     ctypes.byref(valid), ctypes.c_ulong(1))\n\
                 assert result == -1 and ctypes.get_errno() == 9\n\
                 os.lseek(fd, 0, os.SEEK_SET)\n\
                 fault = (Iovec * 1)(Iovec(None, 1))\n\
                 result = libc.syscall(int(sys.argv[2]), ctypes.c_ulong(fd),\n\
                     ctypes.byref(fault), ctypes.c_ulong(1))\n\
                 assert result == -1 and ctypes.get_errno() == 14\n\
                 os.close(fd)",
            )
            .arg(&input)
            .arg(libc::SYS_readv.to_string())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        let attempts = &process.read_attempts;
        let short = attempts
            .iter()
            .find(|attempt| attempt.request.buffer_lengths == [2, 0, 3, 4])
            .unwrap();
        assert_eq!(short.request.requested_bytes, 9);
        assert!(matches!(
            short.outcome,
            CapturedReadOutcome::Read {
                bytes: 5,
                digest,
                file_value,
            }
                if digest == *blake3::hash(contents).as_bytes()
                    && whole_file_value_matches(file_value, contents)
        ));
        let empty_index = attempts
            .iter()
            .position(|attempt| {
                attempt.request.system_call == CapturedReadSystemCall::Readv
                    && attempt.request.buffer_lengths.is_empty()
            })
            .unwrap();
        let empty = &attempts[empty_index];
        assert_eq!(empty.request.requested_bytes, 0);
        assert!(matches!(
            empty.outcome,
            CapturedReadOutcome::Read {
                bytes: 0,
                digest,
                file_value,
            }
                if digest == *blake3::hash(&[]).as_bytes()
                    && whole_file_value_matches(file_value, contents)
        ));
        let bad_descriptor = attempts
            .iter()
            .find(|attempt| attempt.request.descriptor == -1)
            .unwrap();
        assert_eq!(bad_descriptor.request.identity, None);
        assert_eq!(bad_descriptor.request.buffer_lengths, [1]);
        assert_eq!(
            bad_descriptor.outcome,
            CapturedReadOutcome::Failed(libc::EBADF)
        );
        let fault = attempts
            .iter()
            .find(|attempt| {
                attempt.request.buffer_lengths == [1]
                    && attempt.request.descriptor >= 0
                    && attempt.outcome == CapturedReadOutcome::Failed(libc::EFAULT)
            })
            .unwrap();
        assert!(fault.request.identity.is_some());

        let short_index = attempts
            .iter()
            .position(|attempt| attempt.request.buffer_lengths == [2, 0, 3, 4])
            .unwrap();
        let mut flags = serde_json::to_value(&receipt).unwrap();
        flags["processes"][0]["read_attempts"][short_index]["request"]["flags"] =
            serde_json::json!(1);
        let flags: ProcessCaptureReceipt = serde_json::from_value(flags).unwrap();
        assert!(flags.validate_complete().is_err());
        let mut position = serde_json::to_value(&receipt).unwrap();
        position["processes"][0]["read_attempts"][short_index]["request"]
            ["uses_descriptor_offset"] = serde_json::json!(false);
        let position: ProcessCaptureReceipt = serde_json::from_value(position).unwrap();
        assert!(position.validate_complete().is_err());
        let mut oversized = serde_json::to_value(&receipt).unwrap();
        oversized["processes"][0]["read_attempts"][short_index]["request"]["buffer_lengths"] =
            serde_json::json!(vec![0u64; MAX_READ_VECTOR_ENTRIES + 1]);
        oversized["processes"][0]["read_attempts"][short_index]["request"]["requested_bytes"] =
            serde_json::json!(0);
        let oversized: ProcessCaptureReceipt = serde_json::from_value(oversized).unwrap();
        assert!(oversized.validate_complete().is_err());
        let mut false_eof = serde_json::to_value(&receipt).unwrap();
        false_eof["processes"][0]["read_attempts"][empty_index]["request"]["buffer_lengths"] =
            serde_json::json!([1]);
        false_eof["processes"][0]["read_attempts"][empty_index]["request"]["requested_bytes"] =
            serde_json::json!(1);
        false_eof["processes"][0]["read_attempts"][empty_index]["request"]["offset"] =
            serde_json::json!(0);
        let false_eof: ProcessCaptureReceipt = serde_json::from_value(false_eof).unwrap();
        assert!(false_eof.validate_complete().is_err());
    }

    #[test]
    fn rejects_unsupported_vectored_read_requests() {
        let mut oversized = Command::new("/usr/bin/python3");
        oversized
            .arg("-c")
            .arg(
                "import ctypes, sys; libc = ctypes.CDLL(None); \
                 libc.syscall(int(sys.argv[1]), 0, 0, 1025)",
            )
            .arg(libc::SYS_readv.to_string())
            .env_clear();
        let error = capture_process_tree(&mut oversized).unwrap_err();
        assert!(error.to_string().contains("kernel entry limit"));

        let mut unreadable = Command::new("/usr/bin/python3");
        unreadable
            .arg("-c")
            .arg(
                "import ctypes, os, sys; fd = os.open('/dev/null', os.O_RDONLY); \
                 libc = ctypes.CDLL(None); \
                 libc.syscall(int(sys.argv[1]), fd, ctypes.c_void_p(1), 1)",
            )
            .arg(libc::SYS_readv.to_string())
            .env_clear();
        let error = capture_process_tree(&mut unreadable).unwrap_err();
        assert!(error.to_string().contains("read requested vector"));

        let mut overflowed = Command::new("/usr/bin/python3");
        overflowed
            .arg("-c")
            .arg(
                "import ctypes, os, sys; \
                 Iovec = type('Iovec', (ctypes.Structure,), \
                     {'_fields_': [('base', ctypes.c_void_p), \
                                   ('length', ctypes.c_size_t)]}); \
                 vectors = (Iovec * 2)(Iovec(None, ctypes.c_size_t(-1).value), \
                                        Iovec(None, 1)); \
                 fd = os.open('/dev/null', os.O_RDONLY); \
                 libc = ctypes.CDLL(None); \
                 libc.syscall(int(sys.argv[1]), fd, ctypes.byref(vectors), 2)",
            )
            .arg(libc::SYS_readv.to_string())
            .env_clear();
        let error = capture_process_tree(&mut overflowed).unwrap_err();
        assert!(error.to_string().contains("byte count overflowed"));
    }

    #[test]
    fn captures_multithreaded_tracees() {
        let mut command = Command::new("/usr/bin/python3");
        command.arg("-c").arg(
            "import os, threading; \
             barrier = threading.Barrier(33); \
             threads = [threading.Thread(target=lambda: (os.getpid(), barrier.wait())) \
                        for _ in range(32)]; \
             [thread.start() for thread in threads]; \
             barrier.wait(); \
             [thread.join() for thread in threads]",
        );

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_eq!(receipt.processes.len(), 33);
        assert_eq!(
            receipt.process_creation_syscalls().unwrap(),
            32,
            "unmodeled syscalls: {:#?}",
            receipt.processes[0].unmodeled_syscall_attempts
        );
    }

    #[test]
    fn captures_shared_thread_read_buffers_exactly() {
        let temp = tempfile::tempdir().unwrap();
        let input = temp.path().join("input");
        let contents = vec![b'a'; 64 * 1024];
        std::fs::write(&input, &contents).unwrap();
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, os, sys, threading\n",
                "size = 65536\n",
                "buffer = (ctypes.c_ubyte * size)()\n",
                "stop = ctypes.c_int(0)\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "def mutate():\n",
                "    while not stop.value:\n",
                "        ctypes.memset(buffer, ord('z'), size)\n",
                "thread = threading.Thread(target=mutate)\n",
                "thread.start()\n",
                "fd = os.open(sys.argv[1], os.O_RDONLY)\n",
                "for _ in range(100):\n",
                "    assert libc.syscall(int(sys.argv[2]), fd, buffer, size, 0) == size\n",
                "os.close(fd)\n",
                "stop.value = 1\n",
                "thread.join()\n",
            ))
            .arg(&input)
            .arg(libc::SYS_pread64.to_string())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        let reads = receipt
            .processes()
            .iter()
            .flat_map(|process| &process.read_attempts)
            .filter(|attempt| {
                attempt
                    .request
                    .identity
                    .as_ref()
                    .is_some_and(|identity| identity.target == input.as_os_str().as_bytes())
            })
            .collect::<Vec<_>>();
        assert_eq!(
            reads.len(),
            100,
            "total reads={}, per-task={:?}, unmodeled preads={}",
            receipt.read_syscalls().unwrap(),
            receipt
                .processes()
                .iter()
                .map(|process| process.read_attempts.len())
                .collect::<Vec<_>>(),
            receipt
                .processes()
                .iter()
                .flat_map(|process| &process.unmodeled_syscall_attempts)
                .filter(|attempt| attempt.number == libc::SYS_pread64 as u64)
                .count()
        );
        let digest = *blake3::hash(&contents).as_bytes();
        assert!(reads.iter().all(|attempt| matches!(
            attempt.outcome,
            CapturedReadOutcome::Read {
                bytes: 65536,
                digest: observed,
                ..
            } if observed == digest
        )));
    }

    #[test]
    fn rejects_shared_thread_fifo_reads_without_blocking() {
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import os, threading\n",
                "read_fd, write_fd = os.pipe()\n",
                "ready = threading.Event()\n",
                "def write_later():\n",
                "    ready.set()\n",
                "    threading.Event().wait()\n",
                "    os.write(write_fd, b'x')\n",
                "thread = threading.Thread(target=write_later)\n",
                "thread.start()\n",
                "ready.wait()\n",
                "os.read(read_fd, 1)\n",
                "thread.join()\n",
            ))
            .env_clear();

        let error = capture_process_tree(&mut command).unwrap_err().to_string();
        assert!(error.contains(
            "shared-address-space read with a writable sibling endpoint is not safely observable"
        ));
    }

    #[test]
    fn rejects_named_rustc_fifo_read_with_a_racing_buffer_sibling() {
        let temp = tempfile::tempdir().unwrap();
        let rustc = temp.path().join("rustc");
        std::fs::copy("/usr/bin/python3", &rustc).unwrap();
        let mut command = Command::new(&rustc);
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, os, sys, threading, time\n",
                "read_fd, write_fd = os.pipe()\n",
                "buffer = (ctypes.c_ubyte * 1)()\n",
                "stop = ctypes.c_int(0)\n",
                "ready = threading.Event()\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "main_tid = threading.get_native_id()\n",
                "def write_later():\n",
                "    ready.set()\n",
                "    syscall_path = f'/proc/self/task/{main_tid}/syscall'\n",
                "    status_path = f'/proc/self/task/{main_tid}/stat'\n",
                "    while True:\n",
                "        syscall = open(syscall_path).read().split()\n",
                "        state = open(status_path).read().rsplit(')', 1)[1].split()[0]\n",
                "        if syscall and syscall[0] == sys.argv[1] and state == 'S':\n",
                "            break\n",
                "        time.sleep(0.001)\n",
                "    os.write(write_fd, b'x')\n",
                "def mutate():\n",
                "    while not stop.value:\n",
                "        ctypes.memset(buffer, ord('z'), 1)\n",
                "writer = threading.Thread(target=write_later)\n",
                "mutator = threading.Thread(target=mutate)\n",
                "writer.start()\n",
                "mutator.start()\n",
                "ready.wait()\n",
                "assert libc.read(read_fd, buffer, 1) == 1\n",
                "stop.value = 1\n",
                "writer.join()\n",
                "mutator.join()\n",
            ))
            .arg(libc::SYS_read.to_string())
            .env_clear();

        let error = capture_process_tree(&mut command).unwrap_err().to_string();
        assert!(error.contains(
            "shared-address-space read with a writable sibling endpoint is not safely observable"
        ));
    }

    #[test]
    fn captures_shared_thread_fifo_reads_from_a_process() {
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import os, threading, time\n",
                "read_fd, write_fd = os.pipe()\n",
                "writer = os.fork()\n",
                "if writer == 0:\n",
                "    os.close(read_fd)\n",
                "    time.sleep(0.05)\n",
                "    os.write(write_fd, b'x')\n",
                "    os._exit(0)\n",
                "os.close(write_fd)\n",
                "stop = threading.Event()\n",
                "thread = threading.Thread(target=stop.wait)\n",
                "thread.start()\n",
                "assert os.read(read_fd, 1) == b'x'\n",
                "stop.set()\n",
                "thread.join()\n",
                "assert os.waitpid(writer, 0)[1] == 0\n",
            ))
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert!(receipt.processes().iter().any(|process| {
            process.read_attempts.iter().any(|attempt| {
                attempt.request.identity.as_ref().is_some_and(|identity| {
                    identity.kind == CapturedDescriptorKind::Fifo
                        && matches!(attempt.outcome, CapturedReadOutcome::Read { bytes: 1, .. })
                })
            })
        }));
    }

    #[test]
    fn captures_shared_thread_runtime_buffers_exactly() {
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, sys, threading\n",
                "buffer = (ctypes.c_ubyte * 256)()\n",
                "stop = ctypes.c_int(0)\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "def mutate():\n",
                "    while not stop.value:\n",
                "        ctypes.memset(buffer, 255, len(buffer))\n",
                "thread = threading.Thread(target=mutate)\n",
                "thread.start()\n",
                "for _ in range(100):\n",
                "    assert libc.syscall(int(sys.argv[1]), 0, buffer) == 0\n",
                "stop.value = 1\n",
                "thread.join()\n",
            ))
            .arg(libc::SYS_getrusage.to_string())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert!(receipt.runtime_observation_syscalls().unwrap() >= 100);
    }

    #[test]
    fn captures_process_trees_concurrently() {
        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        let capture = |own: PathBuf, peer: PathBuf| {
            std::thread::spawn(move || {
                let mut command = Command::new("/usr/bin/python3");
                command
                    .arg("-c")
                    .arg(concat!(
                        "import pathlib, sys, time\n",
                        "own = pathlib.Path(sys.argv[1])\n",
                        "peer = pathlib.Path(sys.argv[2])\n",
                        "fd = own.open('wb')\n",
                        "fd.close()\n",
                        "deadline = time.monotonic() + 5\n",
                        "while not peer.exists():\n",
                        "    if time.monotonic() >= deadline:\n",
                        "        raise SystemExit(3)\n",
                        "    time.sleep(0.01)\n",
                    ))
                    .arg(own)
                    .arg(peer)
                    .env_clear();
                capture_process_tree(&mut command)
            })
        };

        let first_capture = capture(first.clone(), second.clone());
        let second_capture = capture(second, first);
        assert_eq!(first_capture.join().unwrap().unwrap().processes.len(), 1);
        assert_eq!(second_capture.join().unwrap().unwrap().processes.len(), 1);
    }

    #[test]
    fn nul_vectors_allow_only_empty_noninitial_arguments() {
        let temp = tempfile::tempdir().unwrap();
        let arguments = temp.path().join("arguments");
        std::fs::write(&arguments, b"tool\0\0last\0").unwrap();
        let mut budget = 1024;
        assert_eq!(
            read_nul_vector(&arguments, "argument vector", true, &mut budget).unwrap(),
            [b"tool".to_vec(), Vec::new(), b"last".to_vec()]
        );

        std::fs::write(&arguments, b"\0last\0").unwrap();
        let mut budget = 1024;
        assert!(read_nul_vector(&arguments, "argument vector", true, &mut budget).is_err());

        let environment = temp.path().join("environment");
        std::fs::write(&environment, b"A=1\0\0").unwrap();
        let mut budget = 1024;
        assert!(read_nul_vector(&environment, "environment", false, &mut budget).is_err());
    }

    #[test]
    fn rejects_clone_and_clone3_shared_process_state() {
        let cases = [
            (
                libc::CLONE_FS as u64,
                "shared working-directory state is unsupported",
            ),
            (
                libc::CLONE_VM as u64,
                "shared address spaces are unsupported",
            ),
            (
                libc::CLONE_FILES as u64,
                "shared descriptor tables are unsupported",
            ),
        ];
        for (flags, message) in cases {
            let mut clone = Command::new("/usr/bin/python3");
            clone
                .arg("-c")
                .arg(concat!(
                    "import ctypes, signal, sys\n",
                    "libc = ctypes.CDLL(None)\n",
                    "flags = int(sys.argv[2]) | signal.SIGCHLD\n",
                    "libc.syscall(int(sys.argv[1]), flags, 0, 0, 0, 0)\n",
                ))
                .arg(libc::SYS_clone.to_string())
                .arg(flags.to_string())
                .env_clear();
            let error = capture_process_tree(&mut clone).unwrap_err();
            assert!(error.to_string().contains(message), "{error:#}");

            let mut clone3 = Command::new("/usr/bin/python3");
            clone3
                .arg("-c")
                .arg(concat!(
                    "import ctypes, sys\n",
                    "libc = ctypes.CDLL(None)\n",
                    "flags = ctypes.c_ulonglong(int(sys.argv[2]))\n",
                    "libc.syscall(int(sys.argv[1]), ctypes.byref(flags), 8)\n",
                ))
                .arg(libc::SYS_clone3.to_string())
                .arg(flags.to_string())
                .env_clear();
            let error = capture_process_tree(&mut clone3).unwrap_err();
            assert!(error.to_string().contains(message), "{error:#}");
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn captures_legacy_clone_fork_contract() {
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, os, signal, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "child_tid = ctypes.c_int()\n",
                "flags = int(sys.argv[2]) | signal.SIGCHLD\n",
                "result = libc.syscall(int(sys.argv[1]), flags, 0, 0, ",
                "ctypes.byref(child_tid), 0)\n",
                "if result == 0:\n",
                "    os._exit(0)\n",
                "assert result > 0\n",
                "assert os.waitpid(result, 0)[1] == 0\n",
            ))
            .arg(libc::SYS_clone.to_string())
            .arg((libc::CLONE_CHILD_CLEARTID as u64 | libc::CLONE_CHILD_SETTID as u64).to_string())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_eq!(receipt.processes.len(), 2);
        assert_eq!(receipt.process_creation_syscalls().unwrap(), 1);
        let attempt = &receipt.processes[0].process_creation_attempts[0];
        assert_eq!(
            attempt.request,
            CapturedProcessCreationRequest::Clone { flags: 0x0120_0011 }
        );
        let CapturedProcessCreationOutcome::Created(child) = attempt.outcome else {
            panic!("legacy clone did not create a receipt-local child");
        };
        assert_eq!(
            receipt.processes[usize::try_from(child - 1).unwrap()].parent,
            Some(1)
        );
    }

    #[test]
    fn distinguishes_internal_process_creation_restarts() {
        assert_eq!(
            failed_process_creation_outcome(-513).unwrap(),
            CapturedProcessCreationOutcome::Restarted(513)
        );
        assert_eq!(
            failed_process_creation_outcome(-i64::from(libc::EAGAIN)).unwrap(),
            CapturedProcessCreationOutcome::Failed(libc::EAGAIN)
        );
        assert!(failed_process_creation_outcome(0).is_err());
    }

    #[test]
    fn captures_vfork_style_posix_spawn() {
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import os\n",
                "pids = [os.posix_spawn('/usr/bin/true', ['true'], {}) ",
                "for _ in range(32)]\n",
                "statuses = [os.waitpid(pid, 0)[1] for pid in pids]\n",
                "raise SystemExit(any(os.waitstatus_to_exitcode(status) ",
                "for status in statuses))\n",
            ))
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_eq!(receipt.processes.len(), 33);
        assert_eq!(receipt.process_creation_syscalls().unwrap(), 32);
        assert_eq!(receipt.processes[0].process_creation_attempts.len(), 32);
        for attempt in &receipt.processes[0].process_creation_attempts {
            let CapturedProcessCreationOutcome::Created(child) = attempt.outcome else {
                panic!("posix_spawn process creation did not produce a child");
            };
            assert_eq!(
                receipt.processes[usize::try_from(child - 1).unwrap()].parent,
                Some(1)
            );
        }
        let mut reaped = BTreeSet::new();
        for attempt in &receipt.processes[0].child_wait_attempts {
            match attempt.outcome {
                CapturedChildWaitOutcome::Reaped { child, status: 0 } => {
                    assert_eq!(
                        attempt.request.selector,
                        CapturedChildWaitSelector::Child(child)
                    );
                    assert!(reaped.insert(child));
                }
                CapturedChildWaitOutcome::Failed(_) => {}
                outcome => panic!("unexpected posix_spawn child-wait outcome: {outcome:?}"),
            }
        }
        assert_eq!(reaped.len(), 32);
        let mut wrong_edge = serde_json::to_value(&receipt).unwrap();
        wrong_edge["processes"][0]["process_creation_attempts"][0]["outcome"]["created"] =
            serde_json::json!(1);
        let wrong_edge: ProcessCaptureReceipt = serde_json::from_value(wrong_edge).unwrap();
        assert!(wrong_edge.validate_complete().is_err());
        let mut wrong_wait_edge = serde_json::to_value(&receipt).unwrap();
        wrong_wait_edge["processes"][0]["child_wait_attempts"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|attempt| attempt["outcome"].get("reaped").is_some())
            .unwrap()["outcome"]["reaped"]["child"] = serde_json::json!(1);
        let wrong_wait_edge: ProcessCaptureReceipt =
            serde_json::from_value(wrong_wait_edge).unwrap();
        assert!(wrong_wait_edge.validate_complete().is_err());
    }

    #[test]
    fn captures_child_wait_results_failures_and_denominator() {
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, errno, os, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "read_fd, write_fd = os.pipe()\n",
                "child = os.fork()\n",
                "if child == 0:\n",
                "    os.close(write_fd)\n",
                "    os.read(read_fd, 1)\n",
                "    os._exit(0)\n",
                "os.close(read_fd)\n",
                "assert os.waitpid(child, os.WNOHANG) == (0, 0)\n",
                "os.write(write_fd, b'x')\n",
                "os.close(write_fd)\n",
                "assert os.waitpid(child, 0)[1] == 0\n",
                "try:\n",
                "    os.waitpid(-1, os.WNOHANG)\n",
                "except ChildProcessError:\n",
                "    pass\n",
                "else:\n",
                "    raise AssertionError('expected ECHILD')\n",
                "ctypes.set_errno(0)\n",
                "assert libc.syscall(int(sys.argv[1]), -1, 0, 0, 0) == -1\n",
                "assert ctypes.get_errno() == errno.ECHILD\n",
            ))
            .arg(libc::SYS_wait4.to_string())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_eq!(receipt.child_wait_syscalls().unwrap(), 4);
        let attempts = &receipt.processes[0].child_wait_attempts;
        assert_eq!(attempts.len(), 4);
        assert_eq!(
            attempts[0].request,
            CapturedChildWaitRequest {
                selector: CapturedChildWaitSelector::Child(2),
                nonblocking: true,
                status_is_null: false,
            }
        );
        assert_eq!(attempts[0].outcome, CapturedChildWaitOutcome::NoChildReady);
        assert_eq!(
            attempts[1].outcome,
            CapturedChildWaitOutcome::Reaped {
                child: 2,
                status: 0,
            }
        );
        assert_eq!(
            attempts[2].request,
            CapturedChildWaitRequest {
                selector: CapturedChildWaitSelector::AnyChild,
                nonblocking: true,
                status_is_null: false,
            }
        );
        assert_eq!(
            attempts[2].outcome,
            CapturedChildWaitOutcome::Failed(libc::ECHILD)
        );
        assert_eq!(
            attempts[3].request,
            CapturedChildWaitRequest {
                selector: CapturedChildWaitSelector::AnyChild,
                nonblocking: false,
                status_is_null: true,
            }
        );
        assert_eq!(
            attempts[3].outcome,
            CapturedChildWaitOutcome::Failed(libc::ECHILD)
        );
    }

    #[test]
    fn captures_exact_process_task_and_group_identities() {
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, os, sys\n",
                "libc = ctypes.CDLL(None)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "child = os.fork()\n",
                "if child == 0:\n",
                "    own = libc.syscall(int(sys.argv[1]))\n",
                "    task = libc.syscall(int(sys.argv[2]))\n",
                "    parent = libc.syscall(int(sys.argv[3]))\n",
                "    group = libc.syscall(int(sys.argv[4]))\n",
                "    good = own > 0 and own == task and parent > 0 and group > 0\n",
                "    os._exit(0 if good else 1)\n",
                "raise SystemExit(os.waitpid(child, 0)[1])\n",
            ))
            .arg(libc::SYS_getpid.to_string())
            .arg(libc::SYS_gettid.to_string())
            .arg(libc::SYS_getppid.to_string())
            .arg(libc::SYS_getpgrp.to_string())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        let child = &receipt.processes[1];
        assert!(child.process_identity_attempts.iter().any(|attempt| {
            attempt.request == CapturedProcessIdentityRequest::Getpid
                && attempt.outcome == CapturedProcessIdentityOutcome::Identity(child.id)
        }));
        assert!(child.process_identity_attempts.iter().any(|attempt| {
            attempt.request == CapturedProcessIdentityRequest::Gettid
                && attempt.outcome == CapturedProcessIdentityOutcome::Identity(child.id)
        }));
        assert!(child.process_identity_attempts.iter().any(|attempt| {
            attempt.request == CapturedProcessIdentityRequest::Getppid
                && attempt.outcome == CapturedProcessIdentityOutcome::Identity(1)
        }));
        assert!(child.process_identity_attempts.iter().any(|attempt| {
            attempt.request == CapturedProcessIdentityRequest::Getpgrp
                && matches!(
                    attempt.outcome,
                    CapturedProcessIdentityOutcome::ProcessGroup(group) if group != 0
                )
        }));
        assert!(child
            .unmodeled_syscall_attempts
            .iter()
            .all(|attempt| {
                !matches!(
                    attempt.number,
                    number if number == libc::SYS_gettid as u64
                        || number == libc::SYS_getpgrp as u64
                )
            }));

        let mut wrong_identity = serde_json::to_value(&receipt).unwrap();
        let attempts = wrong_identity["processes"][1]["process_identity_attempts"]
            .as_array_mut()
            .unwrap();
        attempts
            .iter_mut()
            .find(|attempt| attempt["request"] == "getppid")
            .unwrap()["outcome"]["identity"] = serde_json::json!(child.id);
        let wrong_identity: ProcessCaptureReceipt = serde_json::from_value(wrong_identity).unwrap();
        assert!(wrong_identity.validate_complete().is_err());

        let mut wrong_task = serde_json::to_value(&receipt).unwrap();
        let attempts = wrong_task["processes"][1]["process_identity_attempts"]
            .as_array_mut()
            .unwrap();
        attempts
            .iter_mut()
            .find(|attempt| attempt["request"] == "gettid")
            .unwrap()["outcome"]["identity"] = serde_json::json!(1);
        let wrong_task: ProcessCaptureReceipt = serde_json::from_value(wrong_task).unwrap();
        assert!(wrong_task.validate_complete().is_err());

        let mut wrong_group = serde_json::to_value(&receipt).unwrap();
        let attempts = wrong_group["processes"][1]["process_identity_attempts"]
            .as_array_mut()
            .unwrap();
        attempts
            .iter_mut()
            .find(|attempt| attempt["request"] == "getpgrp")
            .unwrap()["outcome"]["process_group"] = serde_json::json!(0);
        let wrong_group: ProcessCaptureReceipt = serde_json::from_value(wrong_group).unwrap();
        assert!(wrong_group.validate_complete().is_err());
    }

    #[test]
    fn decodes_only_canonical_signed_syscall_arguments() {
        assert_eq!(signed_i32_syscall_argument(0), Some(0));
        assert_eq!(signed_i32_syscall_argument(1), Some(1));
        assert_eq!(signed_i32_syscall_argument(u32::MAX as u64), Some(-1));
        assert_eq!(signed_i32_syscall_argument(u64::MAX), Some(-1));
        assert_eq!(signed_i32_syscall_argument(1_u64 << 32), None);
        assert_eq!(signed_i32_syscall_argument(0xffff_ffff_0000_0001), None);

        let entry = SyscallEntry {
            number: libc::SYS_fchmod as u64,
            arguments: [1_u64 << 32, 0o600, 0, 0, 0, 0],
        };
        let pid = Pid::from_raw(std::process::id() as i32);
        assert!(read_path_mode_change_request(pid, entry).unwrap().is_none());
    }

    #[test]
    fn recognizes_only_bounded_getgroups_requests() {
        let mut entry = SyscallEntry {
            number: libc::SYS_getgroups as u64,
            arguments: [65_536, 0x1234, 0, 0, 0, 0],
        };
        assert_eq!(
            read_credential_request(entry),
            Some((
                CapturedCredentialRequest::Getgroups {
                    capacity: 65_536,
                    output_is_null: false,
                },
                0x1234,
            ))
        );

        entry.arguments[0] = 65_537;
        assert_eq!(read_credential_request(entry), None);
        entry.arguments[0] = u64::MAX;
        assert_eq!(read_credential_request(entry), None);
        entry.arguments[0] = 1_u64 << 32;
        assert_eq!(read_credential_request(entry), None);
    }

    #[test]
    fn recognizes_only_measured_capability_bounding_set_queries() {
        let mut entry = SyscallEntry {
            number: libc::SYS_prctl as u64,
            arguments: [
                libc::PR_CAPBSET_READ as u64,
                32,
                0,
                u64::MAX,
                u64::MAX,
                0,
            ],
        };
        assert_eq!(
            read_credential_request(entry),
            Some((
                CapturedCredentialRequest::CapabilityBoundingSetRead { capability: 32 },
                0,
            ))
        );
        for capability in [40, 41, 42, 44, 48] {
            entry.arguments[1] = capability.into();
            assert!(matches!(
                read_credential_request(entry),
                Some((
                    CapturedCredentialRequest::CapabilityBoundingSetRead {
                        capability: observed,
                    },
                    0,
                )) if observed == capability
            ));
        }

        entry.arguments[1] = 39;
        assert_eq!(read_credential_request(entry), None);
        entry.arguments[1] = 1_u64 << 32;
        assert_eq!(read_credential_request(entry), None);
        entry.arguments[1] = 32;
        entry.arguments[0] = libc::PR_CAPBSET_DROP as u64;
        assert_eq!(read_credential_request(entry), None);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn rejects_x32_syscall_numbers() {
        reject_compat_syscall_number(0x4000_0000 | libc::SYS_readv as u64).unwrap_err();
        reject_compat_syscall_number(libc::SYS_readv as u64).unwrap();
    }

    #[test]
    fn ignores_native_vectored_offset_compatibility_slot() {
        let entry = SyscallEntry {
            number: libc::SYS_preadv as u64,
            arguments: [0, 0, 0, 7, 0x1234_5678, 0],
        };
        assert_eq!(native_vectored_offset(entry), 7);
    }

    #[test]
    fn recognizes_only_aligned_process_private_futex_wakes() {
        let mut entry = SyscallEntry {
            number: libc::SYS_futex as u64,
            arguments: [0x1000, 129, i32::MAX as u64, u64::MAX, u64::MAX, u64::MAX],
        };
        assert_eq!(
            read_futex_wake_request(entry),
            Some(CapturedFutexWakeRequest {
                address: 0x1000,
                maximum_waiters: i32::MAX as u32,
            })
        );

        entry.arguments[1] = 1;
        assert_eq!(read_futex_wake_request(entry), None);
        entry.arguments[1] = 129;
        entry.arguments[0] = 0x1001;
        assert_eq!(read_futex_wake_request(entry), None);
        entry.arguments[0] = 0x1000;
        entry.arguments[2] = u32::MAX as u64;
        assert_eq!(read_futex_wake_request(entry), None);
        entry.number = libc::SYS_getpid as u64;
        entry.arguments[2] = 1;
        assert_eq!(read_futex_wake_request(entry), None);
    }

    #[test]
    fn captures_openat2_semantics() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("input");
        std::fs::write(&input, b"openat2 input\n").unwrap();
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(
                "import ctypes, os, sys; \
                 OpenHow = type('OpenHow', (ctypes.Structure,), \
                     {'_fields_': [('flags', ctypes.c_uint64), \
                                   ('mode', ctypes.c_uint64), \
                                   ('resolve', ctypes.c_uint64)]}); \
                 libc = ctypes.CDLL(None, use_errno=True); \
                 how = OpenHow(os.O_RDONLY, 0, 0); \
                 path = ctypes.c_char_p(os.fsencode(sys.argv[1])); \
                 fd = libc.syscall(437, -100, path, ctypes.byref(how), \
                                   ctypes.sizeof(how)); \
                 assert fd >= 0, os.strerror(ctypes.get_errno()); \
                 os.close(fd)",
            )
            .arg(&input)
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let attempt = receipt.processes()[0]
            .open_attempts
            .iter()
            .find(|attempt| attempt.request.system_call == CapturedOpenSystemCall::OpenAt2)
            .unwrap();
        assert_eq!(attempt.request.path, input.as_os_str().as_bytes());
        assert_eq!(attempt.request.directory, None);
        assert_eq!(attempt.request.flags, libc::O_RDONLY as u64);
        assert_eq!(attempt.request.mode, 0);
        assert_eq!(attempt.request.resolve, 0);
        assert!(matches!(
            &attempt.outcome,
            CapturedOpenOutcome::Opened { identity, mount_id, .. }
                if identity.target == input.as_os_str().as_bytes()
                    && identity.kind == CapturedDescriptorKind::RegularFile
                    && *mount_id != 0
        ));
    }

    #[test]
    fn captures_relative_execveat_requests() {
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(
                "import ctypes, os\n\
                 libc = ctypes.CDLL(None, use_errno=True)\n\
                 strings = ctypes.c_char_p\n\
                 libc.execveat.argtypes = [ctypes.c_int, strings, \
                     ctypes.POINTER(strings), ctypes.POINTER(strings), ctypes.c_int]\n\
                 fd = os.open('/bin', os.O_PATH | os.O_DIRECTORY)\n\
                 argv = (strings * 2)(b'true', None)\n\
                 envp = (strings * 2)(b'RUSH_OBSERVE_EXECVEAT_CHILD=1', None)\n\
                 result = libc.execveat(fd, b'true', argv, envp, 0)\n\
                 raise OSError(ctypes.get_errno(), os.strerror(ctypes.get_errno()))",
            )
            .env_clear()
            .env("RUSH_OBSERVE_EXECVEAT_CHILD", "1");

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let attempts = &receipt.processes()[0].exec_attempts;
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].request.executable, b"true");
        assert!(attempts[0]
            .request
            .directory
            .as_ref()
            .is_some_and(|directory| directory.first() == Some(&b'/')));
        assert_eq!(attempts[0].request.flags, 0);
        assert_eq!(attempts[0].request.argv, [b"true"]);
        assert_eq!(
            attempts[0].request.environment,
            [b"RUSH_OBSERVE_EXECVEAT_CHILD=1"]
        );
        assert_eq!(attempts[0].outcome, CapturedExecOutcome::Loaded(1));
    }

    #[test]
    fn pairs_parallel_descendant_syscalls() {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("for i in 1 2 3 4 5 6 7 8; do /bin/true & done; wait");

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        assert_eq!(receipt.processes().len(), 9);
    }

    #[test]
    fn captures_exec_boundary_signal_file_creation_and_credential_state() {
        let mut command = Command::new("/bin/true");
        // SAFETY: this child-only hook uses async-signal-safe signal syscalls
        // and touches only stack-owned state before exec.
        unsafe {
            command.pre_exec(|| {
                let mut blocked: libc::sigset_t = std::mem::zeroed();
                if libc::sigemptyset(&mut blocked) != 0
                    || libc::sigaddset(&mut blocked, libc::SIGUSR1) != 0
                    || libc::sigprocmask(libc::SIG_BLOCK, &blocked, std::ptr::null_mut()) != 0
                    || libc::raise(libc::SIGUSR1) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                let mut ignored: libc::sigaction = std::mem::zeroed();
                ignored.sa_sigaction = libc::SIG_IGN;
                if libc::sigemptyset(&mut ignored.sa_mask) != 0
                    || libc::sigaction(libc::SIGUSR2, &ignored, std::ptr::null_mut()) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                libc::syscall(libc::SYS_umask, 0o077_u32);
                Ok(())
            });
        }

        let receipt = capture_process_tree(&mut command).unwrap();
        let state = receipt.processes()[0].invocations[0].signal_state;
        assert_ne!(state.blocked & (1_u64 << (libc::SIGUSR1 - 1)), 0);
        assert_ne!(state.ignored & (1_u64 << (libc::SIGUSR2 - 1)), 0);
        assert_eq!(state.caught, 0);
        assert_ne!(state.pending & (1_u64 << (libc::SIGUSR1 - 1)), 0);
        assert_eq!(state.shared_pending, 0);
        assert_eq!(
            receipt.processes()[0].invocations[0].file_creation_mask,
            0o077
        );
        let credentials = &receipt.processes()[0].invocations[0].credentials;
        assert_eq!(credentials.real_user, unsafe { libc::getuid() });
        assert_eq!(credentials.effective_user, unsafe { libc::geteuid() });
        assert_eq!(credentials.real_group, unsafe { libc::getgid() });
        assert_eq!(credentials.effective_group, unsafe { libc::getegid() });
    }

    #[test]
    fn captures_umask_transitions_and_denominator() {
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "number = int(sys.argv[1])\n",
                "old = libc.syscall(number, 0)\n",
                "assert 0 <= old <= 0o777\n",
                "assert libc.syscall(number, old) == 0\n",
            ))
            .arg(libc::SYS_umask.to_string())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(process.umask_syscalls, 2);
        assert_eq!(process.umask_attempts.len(), 2);
        assert_eq!(process.umask_attempts[0].requested, 0);
        assert!(matches!(
            process.umask_attempts[0].outcome,
            CapturedUmaskOutcome::Changed(previous) if previous <= 0o777
        ));
        assert_eq!(
            process.umask_attempts[1].outcome,
            CapturedUmaskOutcome::Changed(0)
        );

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["umask_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());
    }

    #[test]
    fn captures_credentials_queries_noop_transitions_and_denominator() {
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, os, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "numbers = [int(value) for value in sys.argv[1:]]\n",
                "for number in numbers[:4]: assert libc.syscall(number) >= 0\n",
                "unchanged = ctypes.c_uint(-1).value\n",
                "assert libc.syscall(numbers[4], unchanged, os.geteuid(), unchanged) == 0\n",
                "assert libc.syscall(numbers[5], unchanged, os.getegid(), unchanged) == 0\n",
                "count = libc.syscall(numbers[6], 0, 0)\n",
                "assert 0 <= count <= 65536\n",
                "groups = (ctypes.c_uint * max(count, 1))()\n",
                "assert libc.syscall(numbers[6], count, groups) == count\n",
            ))
            .args([
                libc::SYS_getuid.to_string(),
                libc::SYS_geteuid.to_string(),
                libc::SYS_getgid.to_string(),
                libc::SYS_getegid.to_string(),
                libc::SYS_setresuid.to_string(),
                libc::SYS_setresgid.to_string(),
                libc::SYS_getgroups.to_string(),
            ])
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(
            usize::try_from(process.credential_syscalls).unwrap(),
            process.credential_attempts.len()
        );
        let mut state = process.invocations[0].credentials.clone();
        let mut explicit = [false; 8];
        for attempt in &process.credential_attempts {
            match (attempt.request, attempt.outcome.clone()) {
                (CapturedCredentialRequest::Getuid, CapturedCredentialOutcome::Identity(id)) => {
                    assert_eq!(id, state.real_user);
                    explicit[0] = true;
                }
                (CapturedCredentialRequest::Geteuid, CapturedCredentialOutcome::Identity(id)) => {
                    assert_eq!(id, state.effective_user);
                    explicit[1] = true;
                }
                (CapturedCredentialRequest::Getgid, CapturedCredentialOutcome::Identity(id)) => {
                    assert_eq!(id, state.real_group);
                    explicit[2] = true;
                }
                (CapturedCredentialRequest::Getegid, CapturedCredentialOutcome::Identity(id)) => {
                    assert_eq!(id, state.effective_group);
                    explicit[3] = true;
                }
                (
                    CapturedCredentialRequest::Getgroups { capacity: 0, .. },
                    CapturedCredentialOutcome::SupplementaryGroupCount(count),
                ) => {
                    assert_eq!(count as usize, state.supplementary_groups.len());
                    explicit[4] = true;
                }
                (
                    CapturedCredentialRequest::Getgroups {
                        capacity,
                        output_is_null: false,
                    },
                    CapturedCredentialOutcome::SupplementaryGroups(groups),
                ) => {
                    assert!(groups.len() <= capacity as usize);
                    assert_eq!(groups, state.supplementary_groups);
                    explicit[5] = true;
                }
                (
                    CapturedCredentialRequest::Setresuid { .. },
                    CapturedCredentialOutcome::State(next),
                ) => {
                    assert_eq!(next, state);
                    state = next;
                    explicit[6] = true;
                }
                (
                    CapturedCredentialRequest::Setresgid { .. },
                    CapturedCredentialOutcome::State(next),
                ) => {
                    assert_eq!(next, state);
                    state = next;
                    explicit[7] = true;
                }
                other => panic!("unexpected credential attempt: {other:?}"),
            }
        }
        assert!(explicit.into_iter().all(|seen| seen));
        let credential_numbers = [
            libc::SYS_getuid as u64,
            libc::SYS_geteuid as u64,
            libc::SYS_getgid as u64,
            libc::SYS_getegid as u64,
            libc::SYS_setresuid as u64,
            libc::SYS_setresgid as u64,
            libc::SYS_getgroups as u64,
        ];
        assert!(process
            .unmodeled_syscall_attempts
            .iter()
            .all(|attempt| !credential_numbers.contains(&attempt.number)));

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["credential_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());
    }

    #[test]
    fn captures_measured_capability_bounding_set_queries() {
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "number = int(sys.argv[1])\n",
                "option = int(sys.argv[2])\n",
                "for capability in (32, 40, 41, 42, 44, 48):\n",
                "  result = libc.syscall(number, option, capability, 0, 0, 0)\n",
                "  assert result in (-1, 0, 1)\n",
                "  if result == -1: assert ctypes.get_errno() > 0\n",
            ))
            .arg(libc::SYS_prctl.to_string())
            .arg(libc::PR_CAPBSET_READ.to_string())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        let queries = process
            .credential_attempts
            .iter()
            .filter(|attempt| {
                matches!(
                    attempt.request,
                    CapturedCredentialRequest::CapabilityBoundingSetRead { .. }
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(queries.len(), 6);
        for (attempt, capability) in queries.into_iter().zip([32_u32, 40, 41, 42, 44, 48]) {
            assert_eq!(
                attempt.request,
                CapturedCredentialRequest::CapabilityBoundingSetRead { capability }
            );
            match attempt.outcome {
                CapturedCredentialOutcome::CapabilityBoundingSetMembership(member)
                    if capability <= 40 =>
                {
                    assert_eq!(
                        member,
                        process.invocations[0].credentials.bounding_capabilities
                            & (1_u64 << capability)
                            != 0
                    );
                }
                CapturedCredentialOutcome::Failed(libc::EINVAL) if capability > 40 => {}
                ref other => panic!("unexpected capability query outcome: {other:?}"),
            }
        }
        assert!(process
            .unmodeled_syscall_attempts
            .iter()
            .all(|attempt| attempt.number != libc::SYS_prctl as u64));
    }

    #[test]
    fn captures_process_private_futex_wakes_and_denominator() {
        let mut command = Command::new("/usr/bin/python3");
        command
            .arg("-c")
            .arg(concat!(
                "import ctypes, sys\n",
                "libc = ctypes.CDLL(None, use_errno=True)\n",
                "libc.syscall.restype = ctypes.c_long\n",
                "word = ctypes.c_int(0)\n",
                "result = libc.syscall(int(sys.argv[1]), ctypes.byref(word), ",
                "129, 2147483647, 0, 0, 0)\n",
                "assert result == 0, ctypes.get_errno()\n",
            ))
            .arg(libc::SYS_futex.to_string())
            .env_clear();

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        let process = &receipt.processes()[0];
        assert_eq!(
            usize::try_from(process.futex_wake_syscalls).unwrap(),
            process.futex_wake_attempts.len()
        );
        assert!(process.futex_wake_attempts.iter().any(|attempt| {
            attempt.request.address != 0
                && attempt.request.address % std::mem::align_of::<u32>() as u64 == 0
                && attempt.request.maximum_waiters == i32::MAX as u32
                && attempt.outcome == CapturedFutexWakeOutcome::Woken(0)
        }));

        let mut incomplete = serde_json::to_value(&receipt).unwrap();
        incomplete["processes"][0]["futex_wake_attempts"] = serde_json::json!([]);
        let incomplete: ProcessCaptureReceipt = serde_json::from_value(incomplete).unwrap();
        assert!(incomplete.validate_complete().is_err());
    }

    #[test]
    fn preserves_signal_termination() {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg("kill -TERM $$");

        let receipt = capture_process_tree(&mut command).unwrap();
        assert_syscall_census(&receipt);
        assert_eq!(
            receipt.processes()[0].outcome,
            CapturedProcessOutcome::Signaled(libc::SIGTERM)
        );
        assert!(receipt.processes()[0]
            .signal_deliveries
            .iter()
            .any(|delivery| {
                delivery.signal == libc::SIGTERM as u32
                    && delivery.source == CapturedSignalSource::TracedProcess { process: 1 }
            }));
    }

    #[test]
    fn observed_output_is_drained_and_tail_bounded() {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("printf 123456789; printf abcdefghi >&2");

        let output = capture_process_tree_with_output(&mut command, 4).unwrap();

        assert_eq!(output.stdout_tail, b"6789");
        assert_eq!(output.stderr_tail, b"fghi");
        assert_eq!(
            output
                .receipt
                .processes()
                .iter()
                .find(|process| process.id == output.receipt.root())
                .unwrap()
                .outcome,
            CapturedProcessOutcome::Exited(0)
        );
    }
}
