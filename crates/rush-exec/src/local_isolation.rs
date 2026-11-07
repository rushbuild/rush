// Rush Exec - mandatory local action isolation
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Fail-closed Linux isolation for mandatory local actions.

use anyhow::{Context, Result};
use rush_action::external_action_key::ExternalCredentialState;
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::sync::OnceLock;

/// Descriptor topology installed before an isolated action is executed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalDescriptorTopology {
    /// Preserve only descriptors zero through two at direct command entry.
    StandardOnlyV1,
}

/// Filesystem and network boundary for one local action.
///
/// The host filesystem remains visible at its original absolute paths but is
/// recursively read-only. Each listed directory is rebound writable after the
/// read-only boundary is installed. Network, IPC, UTS, user, and mount state
/// live in fresh namespaces. Unsupported kernels fail the spawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalExecutionIsolation {
    writable_roots: Vec<PathBuf>,
    descriptor_topology: LocalDescriptorTopology,
}

impl LocalExecutionIsolation {
    /// Request a read-only host view with these writable directory subtrees.
    pub fn new(writable_roots: Vec<PathBuf>) -> Result<Self> {
        if writable_roots.is_empty() {
            anyhow::bail!("local execution isolation has no writable root");
        }
        Ok(Self {
            writable_roots,
            descriptor_topology: LocalDescriptorTopology::StandardOnlyV1,
        })
    }

    /// Writable directory subtrees requested by this policy.
    pub fn writable_roots(&self) -> &[PathBuf] {
        &self.writable_roots
    }

    /// Descriptor topology installed immediately before executing the action.
    pub fn descriptor_topology(&self) -> LocalDescriptorTopology {
        self.descriptor_topology
    }

    /// Check whether the isolated user namespace can preserve these IDs.
    pub fn validate_credentials(&self, credentials: &ExternalCredentialState) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            validate_mappable_credentials(credentials)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = credentials;
            anyhow::bail!("local execution isolation is unsupported on this platform")
        }
    }
}

#[cfg(target_os = "linux")]
fn validate_mappable_credentials(credentials: &ExternalCredentialState) -> Result<()> {
    if [
        credentials.effective_user,
        credentials.saved_user,
        credentials.filesystem_user,
    ]
    .iter()
    .any(|identity| *identity != credentials.real_user)
    {
        anyhow::bail!("local isolation cannot preserve distinct user identities");
    }
    let distinct_fixed_group = [
        credentials.effective_group,
        credentials.saved_group,
        credentials.filesystem_group,
    ]
    .iter()
    .any(|identity| *identity != credentials.real_group);
    let overflow_group = kernel_overflow_group_identity()?;
    let unmappable_supplementary_group = credentials
        .supplementary_groups
        .iter()
        .any(|identity| *identity != credentials.real_group && *identity != overflow_group);
    if distinct_fixed_group || unmappable_supplementary_group {
        anyhow::bail!("local isolation cannot preserve distinct group identities");
    }
    if credentials.inheritable_capabilities
        | credentials.permitted_capabilities
        | credentials.effective_capabilities
        | credentials.ambient_capabilities
        != 0
    {
        anyhow::bail!("local isolation cannot preserve capability-bearing credentials");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn kernel_overflow_group_identity() -> Result<u32> {
    static IDENTITY: OnceLock<u32> = OnceLock::new();
    if let Some(identity) = IDENTITY.get() {
        return Ok(*identity);
    }
    let identity = std::fs::read_to_string("/proc/sys/kernel/overflowgid")
        .context("read kernel overflow group identity")?
        .trim()
        .parse::<u32>()
        .context("decode kernel overflow group identity")?;
    if identity == u32::MAX {
        anyhow::bail!("kernel overflow group identity is invalid");
    }
    let _ = IDENTITY.set(identity);
    Ok(identity)
}

#[cfg(all(test, target_os = "linux"))]
mod credential_tests {
    use super::*;

    #[test]
    fn accepts_only_primary_and_kernel_overflow_supplementary_groups() {
        let overflow = kernel_overflow_group_identity().unwrap();
        let mut credentials = ExternalCredentialState {
            real_user: 1000,
            effective_user: 1000,
            saved_user: 1000,
            filesystem_user: 1000,
            real_group: 1000,
            effective_group: 1000,
            saved_group: 1000,
            filesystem_group: 1000,
            supplementary_groups: vec![1000, overflow, overflow],
            ..ExternalCredentialState::default()
        };
        credentials.supplementary_groups.sort_unstable();
        validate_mappable_credentials(&credentials).unwrap();

        let unsupported = if overflow != 1001 { 1001 } else { 1002 };
        credentials.supplementary_groups = vec![1000, unsupported];
        assert!(validate_mappable_credentials(&credentials).is_err());
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::ffi::CString;
    use std::io;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::DirBuilderExt as _;
    use std::sync::atomic::{AtomicU64, Ordering};

    const AT_RECURSIVE: libc::c_uint = 0x8000;
    const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
    const MOUNT_ATTR_RDONLY: u64 = 0x0000_0001;
    const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
    const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
    const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
    const SECCOMP_DATA_ARCH_OFFSET: u32 = 4;
    const SECCOMP_DATA_NR_OFFSET: u32 = 0;
    const SECCOMP_DATA_ARG0_OFFSET: u32 = 16;
    const SECCOMP_DATA_ARG0_HIGH_OFFSET: u32 = SECCOMP_DATA_ARG0_OFFSET + 4;
    const SECCOMP_DATA_ARG2_OFFSET: u32 = SECCOMP_DATA_ARG0_OFFSET + 16;
    const SECCOMP_DATA_ARG2_HIGH_OFFSET: u32 = SECCOMP_DATA_ARG2_OFFSET + 4;
    const X32_SYSCALL_BIT: u32 = 0x4000_0000;
    const BPF_ABS: u16 = 0x20;
    const BPF_JEQ: u16 = 0x10;
    const BPF_JMP: u16 = 0x05;
    const BPF_JSET: u16 = 0x40;
    const BPF_K: u16 = 0x00;
    const BPF_LD: u16 = 0x00;
    const BPF_RET: u16 = 0x06;
    const BPF_W: u16 = 0x00;

    #[repr(C)]
    struct MountAttr {
        attr_set: u64,
        attr_clr: u64,
        propagation: u64,
        userns_fd: u64,
    }

    #[derive(Clone)]
    struct ChildIsolation {
        sandbox_root: CString,
        working_directory: CString,
        writable_mounts: Vec<(CString, CString)>,
        descriptor_topology: LocalDescriptorTopology,
        user_map: Vec<u8>,
        group_map: Vec<u8>,
    }

    /// Prepared parent-side resources retained until the child exits.
    pub(crate) struct PreparedLocalIsolation {
        root: PathBuf,
        child: ChildIsolation,
    }

    impl PreparedLocalIsolation {
        pub(crate) fn install(&self, command: &mut tokio::process::Command) {
            let child = self.child.clone();
            // SAFETY: every path and mapping buffer is allocated before fork.
            // The child hook uses raw syscalls only and returns an io::Error on
            // the first unsupported or rejected isolation operation.
            unsafe {
                command.pre_exec(move || child.apply());
            }
        }
    }

    impl Drop for PreparedLocalIsolation {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir(&self.root);
        }
    }

    pub(crate) fn prepare(
        policy: &LocalExecutionIsolation,
        working_directory: Option<&Path>,
        credentials: Option<&ExternalCredentialState>,
    ) -> Result<PreparedLocalIsolation> {
        if let Some(credentials) = credentials {
            validate_mappable_credentials(credentials)?;
        }
        let working_directory = working_directory.unwrap_or(Path::new("/"));
        let working_directory =
            canonical_absolute_directory(working_directory, "local isolation working directory")?;
        let mut writable_roots = policy
            .writable_roots
            .iter()
            .map(|path| canonical_absolute_directory(path, "local isolation writable root"))
            .collect::<Result<Vec<_>>>()?;
        writable_roots.sort();
        writable_roots.dedup();
        if writable_roots.iter().any(|path| path == Path::new("/")) {
            anyhow::bail!("local execution isolation cannot make the host root writable");
        }

        let temp_parent = std::env::temp_dir()
            .canonicalize()
            .context("canonicalize local isolation temporary directory")?;
        if writable_roots
            .iter()
            .any(|root| temp_parent.starts_with(root))
        {
            anyhow::bail!("local isolation temporary directory is inside a writable root");
        }
        let root = create_private_root(&temp_parent)?;
        let result = prepare_child(
            &root,
            &working_directory,
            &writable_roots,
            policy.descriptor_topology,
        );
        match result {
            Ok(child) => Ok(PreparedLocalIsolation { root, child }),
            Err(error) => {
                let _ = std::fs::remove_dir(&root);
                Err(error)
            }
        }
    }

    fn prepare_child(
        root: &Path,
        working_directory: &Path,
        writable_roots: &[PathBuf],
        descriptor_topology: LocalDescriptorTopology,
    ) -> Result<ChildIsolation> {
        let sandbox_root = path_cstring(root, "local isolation root")?;
        let working_directory =
            path_cstring(working_directory, "local isolation working directory")?;
        let writable_mounts = writable_roots
            .iter()
            .map(|source| {
                let relative = source
                    .strip_prefix("/")
                    .context("local isolation writable root is not absolute")?;
                let target = root.join(relative);
                Ok((
                    path_cstring(source, "local isolation writable root")?,
                    path_cstring(&target, "local isolation writable target")?,
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        let user = unsafe { libc::getuid() };
        let group = unsafe { libc::getgid() };
        Ok(ChildIsolation {
            sandbox_root,
            working_directory,
            writable_mounts,
            descriptor_topology,
            user_map: format!("{user} {user} 1\n").into_bytes(),
            group_map: format!("{group} {group} 1\n").into_bytes(),
        })
    }

    fn canonical_absolute_directory(path: &Path, description: &str) -> Result<PathBuf> {
        if !path.is_absolute() {
            anyhow::bail!("{description} is not absolute");
        }
        let canonical = path
            .canonicalize()
            .with_context(|| format!("canonicalize {description}"))?;
        if canonical != path {
            anyhow::bail!("{description} is not canonical");
        }
        if !std::fs::metadata(path)
            .with_context(|| format!("stat {description}"))?
            .is_dir()
        {
            anyhow::bail!("{description} is not a directory");
        }
        Ok(canonical)
    }

    fn create_private_root(parent: &Path) -> Result<PathBuf> {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);

        for _ in 0..100 {
            let root = parent.join(format!(
                "rush-local-isolation-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .context("read local isolation allocation time")?
                    .as_nanos(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700);
            match builder.create(&root) {
                Ok(()) => return Ok(root),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error).context("create local isolation root"),
            }
        }
        anyhow::bail!("could not allocate a unique local isolation root")
    }

    fn path_cstring(path: &Path, description: &str) -> Result<CString> {
        CString::new(path.as_os_str().as_bytes())
            .with_context(|| format!("{description} contains a null byte"))
    }

    impl ChildIsolation {
        fn apply(&self) -> io::Result<()> {
            // SAFETY: this runs after fork and before exec. All pointers refer
            // to immutable buffers owned by `self` for the closure lifetime.
            unsafe { self.apply_raw() }
        }

        unsafe fn apply_raw(&self) -> io::Result<()> {
            syscall_zero(libc::unshare(libc::CLONE_NEWUSER))?;
            write_proc_file(b"/proc/self/uid_map\0", &self.user_map)?;
            write_proc_file(b"/proc/self/setgroups\0", b"deny\n")?;
            write_proc_file(b"/proc/self/gid_map\0", &self.group_map)?;

            syscall_zero(libc::unshare(
                libc::CLONE_NEWNS | libc::CLONE_NEWNET | libc::CLONE_NEWIPC | libc::CLONE_NEWUTS,
            ))?;
            syscall_zero(libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                (libc::MS_REC | libc::MS_PRIVATE) as libc::c_ulong,
                std::ptr::null(),
            ))?;
            syscall_zero(libc::mount(
                c"/".as_ptr(),
                self.sandbox_root.as_ptr(),
                std::ptr::null(),
                (libc::MS_BIND | libc::MS_REC) as libc::c_ulong,
                std::ptr::null(),
            ))?;
            let readonly = MountAttr {
                attr_set: MOUNT_ATTR_RDONLY,
                attr_clr: 0,
                propagation: 0,
                userns_fd: 0,
            };
            syscall_zero(libc::syscall(
                libc::SYS_mount_setattr,
                libc::AT_FDCWD,
                self.sandbox_root.as_ptr(),
                AT_RECURSIVE,
                &readonly as *const MountAttr,
                std::mem::size_of::<MountAttr>(),
            ) as libc::c_int)?;
            for (source, target) in &self.writable_mounts {
                syscall_zero(libc::mount(
                    source.as_ptr(),
                    target.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND as libc::c_ulong,
                    std::ptr::null(),
                ))?;
            }
            syscall_zero(libc::chroot(self.sandbox_root.as_ptr()))?;
            syscall_zero(libc::chdir(self.working_directory.as_ptr()))?;
            match self.descriptor_topology {
                LocalDescriptorTopology::StandardOnlyV1 => syscall_zero(libc::syscall(
                    libc::SYS_close_range,
                    3_u32,
                    u32::MAX,
                    CLOSE_RANGE_CLOEXEC,
                )
                    as libc::c_int)?,
            }
            install_effect_filter()?;
            Ok(())
        }
    }

    #[cfg(target_arch = "x86_64")]
    unsafe fn install_effect_filter() -> io::Result<()> {
        const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
        const DENIED: &[libc::c_long] = &[
            libc::SYS_socket,
            libc::SYS_socketpair,
            libc::SYS_ptrace,
            libc::SYS_process_vm_writev,
            libc::SYS_process_madvise,
            libc::SYS_process_mrelease,
            libc::SYS_kill,
            libc::SYS_tkill,
            libc::SYS_tgkill,
            libc::SYS_pidfd_open,
            libc::SYS_pidfd_getfd,
            libc::SYS_pidfd_send_signal,
            libc::SYS_setpriority,
            libc::SYS_ioprio_set,
            libc::SYS_sched_setparam,
            libc::SYS_sched_setscheduler,
            libc::SYS_sched_setattr,
            libc::SYS_sched_setaffinity,
            libc::SYS_setrlimit,
            libc::SYS_mount,
            libc::SYS_umount2,
            libc::SYS_pivot_root,
            libc::SYS_move_mount,
            libc::SYS_open_tree,
            libc::SYS_fsopen,
            libc::SYS_fsmount,
            libc::SYS_fsconfig,
            libc::SYS_mount_setattr,
            libc::SYS_setns,
            libc::SYS_unshare,
            libc::SYS_reboot,
            libc::SYS_kexec_load,
            libc::SYS_kexec_file_load,
            libc::SYS_init_module,
            libc::SYS_finit_module,
            libc::SYS_delete_module,
            libc::SYS_bpf,
            libc::SYS_keyctl,
            libc::SYS_add_key,
            libc::SYS_request_key,
            libc::SYS_swapon,
            libc::SYS_swapoff,
            libc::SYS_acct,
            libc::SYS_ioperm,
            libc::SYS_iopl,
            libc::SYS_quotactl,
            libc::SYS_perf_event_open,
            libc::SYS_fanotify_init,
            libc::SYS_name_to_handle_at,
            libc::SYS_open_by_handle_at,
            libc::SYS_link,
            libc::SYS_linkat,
            libc::SYS_mknod,
            libc::SYS_mknodat,
            libc::SYS_syslog,
        ];
        const FORBIDDEN_CLONE_FLAGS: u32 = libc::CLONE_NEWCGROUP as u32
            | libc::CLONE_NEWIPC as u32
            | libc::CLONE_NEWNET as u32
            | libc::CLONE_NEWNS as u32
            | libc::CLONE_NEWPID as u32
            | libc::CLONE_NEWUSER as u32
            | libc::CLONE_NEWUTS as u32;

        let mut filters = Vec::with_capacity(23 + DENIED.len() * 2);
        filters.push(statement(
            BPF_LD | BPF_W | BPF_ABS,
            SECCOMP_DATA_ARCH_OFFSET,
        ));
        filters.push(jump(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_X86_64, 1, 0));
        filters.push(statement(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS));
        filters.push(statement(BPF_LD | BPF_W | BPF_ABS, SECCOMP_DATA_NR_OFFSET));
        filters.push(jump(BPF_JMP | BPF_JSET | BPF_K, X32_SYSCALL_BIT, 0, 1));
        filters.push(statement(
            BPF_RET | BPF_K,
            SECCOMP_RET_ERRNO | libc::EPERM as u32,
        ));
        for syscall in DENIED {
            filters.push(jump(BPF_JMP | BPF_JEQ | BPF_K, *syscall as u32, 0, 1));
            filters.push(statement(
                BPF_RET | BPF_K,
                SECCOMP_RET_ERRNO | libc::EPERM as u32,
            ));
        }
        filters.push(jump(
            BPF_JMP | BPF_JEQ | BPF_K,
            libc::SYS_prlimit64 as u32,
            0,
            13,
        ));
        for offset in [
            SECCOMP_DATA_ARG0_OFFSET,
            SECCOMP_DATA_ARG0_HIGH_OFFSET,
            SECCOMP_DATA_ARG2_OFFSET,
            SECCOMP_DATA_ARG2_HIGH_OFFSET,
        ] {
            filters.push(statement(BPF_LD | BPF_W | BPF_ABS, offset));
            filters.push(jump(BPF_JMP | BPF_JSET | BPF_K, u32::MAX, 0, 1));
            filters.push(statement(
                BPF_RET | BPF_K,
                SECCOMP_RET_ERRNO | libc::EPERM as u32,
            ));
        }
        filters.push(statement(BPF_RET | BPF_K, SECCOMP_RET_ALLOW));
        filters.push(jump(
            BPF_JMP | BPF_JEQ | BPF_K,
            libc::SYS_clone3 as u32,
            0,
            1,
        ));
        filters.push(statement(
            BPF_RET | BPF_K,
            SECCOMP_RET_ERRNO | libc::ENOSYS as u32,
        ));
        filters.push(jump(
            BPF_JMP | BPF_JEQ | BPF_K,
            libc::SYS_clone as u32,
            0,
            3,
        ));
        filters.push(statement(
            BPF_LD | BPF_W | BPF_ABS,
            SECCOMP_DATA_ARG0_OFFSET,
        ));
        filters.push(jump(
            BPF_JMP | BPF_JSET | BPF_K,
            FORBIDDEN_CLONE_FLAGS,
            0,
            1,
        ));
        filters.push(statement(
            BPF_RET | BPF_K,
            SECCOMP_RET_ERRNO | libc::EPERM as u32,
        ));
        filters.push(statement(BPF_RET | BPF_K, SECCOMP_RET_ALLOW));
        let program = libc::sock_fprog {
            len: u16::try_from(filters.len())
                .map_err(|_| io::Error::from_raw_os_error(libc::E2BIG))?,
            filter: filters.as_mut_ptr(),
        };
        syscall_zero(libc::prctl(
            libc::PR_SET_SECCOMP,
            libc::SECCOMP_MODE_FILTER,
            &program as *const libc::sock_fprog,
        ))
    }

    #[cfg(not(target_arch = "x86_64"))]
    unsafe fn install_effect_filter() -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::ENOTSUP))
    }

    fn statement(code: u16, value: u32) -> libc::sock_filter {
        libc::sock_filter {
            code,
            jt: 0,
            jf: 0,
            k: value,
        }
    }

    fn jump(code: u16, value: u32, yes: u8, no: u8) -> libc::sock_filter {
        libc::sock_filter {
            code,
            jt: yes,
            jf: no,
            k: value,
        }
    }

    #[cfg(all(test, target_arch = "x86_64"))]
    #[test]
    fn effect_filter_allows_only_read_only_self_limit_queries() {
        unsafe fn child_result() -> i32 {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return 1;
            }
            if install_effect_filter().is_err() {
                return 2;
            }

            let mut limit = std::mem::zeroed::<libc::rlimit>();
            if libc::getrlimit(libc::RLIMIT_STACK, &mut limit) != 0 {
                return 3;
            }
            let mutation = libc::syscall(
                libc::SYS_prlimit64,
                0,
                libc::RLIMIT_STACK,
                &limit as *const libc::rlimit,
                std::ptr::null_mut::<libc::rlimit>(),
            );
            if mutation != -1 || io::Error::last_os_error().raw_os_error() != Some(libc::EPERM) {
                return 4;
            }

            let cross_process_query = libc::syscall(
                libc::SYS_prlimit64,
                libc::getppid(),
                libc::RLIMIT_STACK,
                std::ptr::null::<libc::rlimit>(),
                &mut limit as *mut libc::rlimit,
            );
            if cross_process_query != -1
                || io::Error::last_os_error().raw_os_error() != Some(libc::EPERM)
            {
                return 5;
            }
            let raw_mutation = libc::syscall(
                libc::SYS_setrlimit,
                libc::RLIMIT_STACK,
                &limit as *const libc::rlimit,
            );
            if raw_mutation != -1 || io::Error::last_os_error().raw_os_error() != Some(libc::EPERM)
            {
                return 6;
            }
            0
        }

        unsafe {
            let child = libc::fork();
            assert!(child >= 0, "fork failed: {}", io::Error::last_os_error());
            if child == 0 {
                libc::_exit(child_result());
            }
            let mut status = 0;
            assert_eq!(libc::waitpid(child, &mut status, 0), child);
            assert!(libc::WIFEXITED(status));
            assert_eq!(libc::WEXITSTATUS(status), 0);
        }
    }

    #[cfg(all(test, target_arch = "x86_64"))]
    #[test]
    fn effect_filter_rejects_x32_syscall_numbers() {
        unsafe fn child_result() -> i32 {
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return 1;
            }
            if install_effect_filter().is_err() {
                return 2;
            }

            let result = libc::syscall(libc::SYS_getpid | X32_SYSCALL_BIT as libc::c_long);
            if result != -1 || io::Error::last_os_error().raw_os_error() != Some(libc::EPERM) {
                return 3;
            }
            0
        }

        unsafe {
            let child = libc::fork();
            assert!(child >= 0, "fork failed: {}", io::Error::last_os_error());
            if child == 0 {
                libc::_exit(child_result());
            }
            let mut status = 0;
            assert_eq!(libc::waitpid(child, &mut status, 0), child);
            assert!(libc::WIFEXITED(status));
            assert_eq!(libc::WEXITSTATUS(status), 0);
        }
    }

    unsafe fn write_proc_file(path: &'static [u8], value: &[u8]) -> io::Result<()> {
        let descriptor = libc::open(path.as_ptr().cast(), libc::O_WRONLY | libc::O_CLOEXEC);
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut written = 0;
        while written < value.len() {
            let count = libc::write(
                descriptor,
                value[written..].as_ptr().cast(),
                value.len() - written,
            );
            if count < 0 {
                let error = io::Error::last_os_error();
                let _ = libc::close(descriptor);
                return Err(error);
            }
            written += count as usize;
        }
        syscall_zero(libc::close(descriptor))
    }

    fn syscall_zero(result: libc::c_int) -> io::Result<()> {
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) use linux::prepare;

#[cfg(not(target_os = "linux"))]
pub(crate) struct PreparedLocalIsolation {
    _private: (),
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn prepare(
    _policy: &LocalExecutionIsolation,
    _working_directory: Option<&Path>,
    _credentials: Option<&ExternalCredentialState>,
) -> Result<PreparedLocalIsolation> {
    anyhow::bail!("local execution isolation is unsupported on this platform")
}

#[cfg(not(target_os = "linux"))]
impl PreparedLocalIsolation {
    pub(crate) fn install(&self, _command: &mut tokio::process::Command) {}
}
