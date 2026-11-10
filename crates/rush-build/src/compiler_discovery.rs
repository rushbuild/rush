// Rush Build - Explicit uncached compiler discovery diagnostic
// Copyright © 2026 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Per-command compile policy, pending declarations and uncached diagnostics.
//!
//! Only the explicit diagnostic mode defers literal-header proof. It executes
//! real commands with no reusable cache/snapshot authority. A declaration/order
//! or parsed depfile is not proof of complete read-time inputs or SDK values.

use anyhow::{anyhow, ensure, Result};
use rush_action::action_dag::ActionDag;
use rush_action::ids::{ArtifactRegistry, DeclaredActionId};
use rush_command_core::CommandSemantics;
use rush_rules::context::{NativeCompileMode, PendingCompileDeclaration};
use std::collections::HashSet;

pub(crate) const COMPILE_DISCOVERY_ENV: &[u8] = b"RUSH_CPP_DISCOVERY";

/// Decode only this immutable client's bytes. Absence resets to Literal;
/// unknown, empty or non-UTF8 values refuse instead of inheriting daemon state.
/// Calling this does not enable any execution or change a cache policy.
pub(crate) fn requested_compile_mode(command: &CommandSemantics) -> Result<NativeCompileMode> {
    match command.getenv(COMPILE_DISCOVERY_ENV) {
        None | Some(b"literal") => Ok(NativeCompileMode::Literal),
        Some(b"uncached-diagnostic-v1") => Ok(NativeCompileMode::UncachedDiagnosticV1),
        Some(_) => Err(anyhow!(
            "RUSH_CPP_DISCOVERY requires literal or uncached-diagnostic-v1"
        )),
    }
}

/// Check a pending record against its build registry and a caller-reported
/// producer sequence. This only checks structural ordering: the later manager
/// must independently own execution/completion and post-producer file reads.
/// It returns no ready request, certificate, cache key or execution capability.
pub(crate) fn validate_reported_producer_order(
    registry: &ArtifactRegistry,
    pending: &PendingCompileDeclaration,
    completed: &[DeclaredActionId],
) -> Result<()> {
    let current = PendingCompileDeclaration::from_registry(
        registry,
        pending.action(),
        pending.target(),
        pending.config(),
        pending.source(),
        pending.object(),
    )?;
    ensure!(
        &current == pending,
        "pending compile declaration identity changed"
    );
    // The DAG includes incomplete actions, so it detects structural cycles
    // without pretending this pending compile is schedulable.
    ActionDag::build(registry)?;
    let mut seen = HashSet::new();
    for &id in completed {
        ensure!(
            (id.0 as usize) < registry.declared_action_count(),
            "reported producer is not in this registry"
        );
        ensure!(
            id != pending.action() && !seen.contains(&id),
            "reported producer sequence repeats or includes the pending compile"
        );
        let action = registry.declared_action(id);
        for &input in action.inputs.iter() {
            let data = registry.artifact(input);
            if !data.is_source {
                let producer = data.owner_action_declared.ok_or_else(|| {
                    anyhow!("reported producer consumes an input without a declared producer")
                })?;
                ensure!(
                    seen.contains(&producer),
                    "reported producer runs before its declared input producer"
                );
            }
        }
        seen.insert(id);
    }
    ensure!(
        pending
            .required_producers()
            .iter()
            .all(|id| seen.contains(id)),
        "pending compile still has unfinished declared producers"
    );
    Ok(())
}

/// Narrow exclusion among this binary's public native entrypoints. It does not
/// coordinate Make/old binaries. Reserved authority files are never replaced
/// via a pathname reached through a sampled parent directory.
#[cfg(unix)]
struct AdmissionDirectory {
    workspace: std::path::PathBuf,
    root: std::fs::File,
    directory: std::fs::File,
    created: bool,
}

#[cfg(unix)]
fn admission_name(name: &std::ffi::OsStr) -> Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    ensure!(
        !name.as_bytes().contains(&b'/') && !name.is_empty(),
        "invalid admission basename"
    );
    Ok(std::ffi::CString::new(name.as_bytes())?)
}

#[cfg(unix)]
fn admission_open(
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
    flags: i32,
    mode: u32,
) -> Result<std::fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let name = admission_name(name)?;
    // Every successful openat returns a new owned descriptor. The caller
    // supplies a single basename, never an untrusted multi-component path.
    let fd = unsafe {
        nix::libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC | nix::libc::O_NONBLOCK,
            mode as nix::libc::mode_t,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn admission_root(path: &std::path::Path) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    ensure!(path.is_absolute(), "admission root must be absolute");
    let mut root = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
        .open("/")?;
    for component in path.components() {
        match component {
            std::path::Component::RootDir => (),
            std::path::Component::Normal(name) => {
                root = admission_open(&root, name, nix::libc::O_RDONLY | nix::libc::O_DIRECTORY, 0)?
            }
            _ => anyhow::bail!("non-normal admission root"),
        }
    }
    Ok(root)
}

#[cfg(unix)]
fn admission_identity(meta: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

#[cfg(unix)]
fn admission_signature(
    meta: &std::fs::Metadata,
) -> (u64, u64, u32, u32, u32, u64, u64, i64, i64, i64, i64) {
    use std::os::unix::fs::MetadataExt;
    (
        meta.dev(),
        meta.ino(),
        meta.mode(),
        meta.uid(),
        meta.gid(),
        meta.nlink(),
        meta.len(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec(),
    )
}

#[cfg(unix)]
impl AdmissionDirectory {
    fn open(workspace: &std::path::Path, create: bool) -> Result<Self> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        let workspace = std::fs::canonicalize(workspace)?;
        let root = admission_root(&workspace)?;
        let name = std::ffi::CString::new(".rush-compiler-discovery")?;
        let mut created = false;
        if create {
            if unsafe { nix::libc::mkdirat(root.as_raw_fd(), name.as_ptr(), 0o700) } == 0 {
                created = true;
            } else {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(error.into());
                }
            }
        }
        let directory = admission_open(
            &root,
            std::ffi::OsStr::new(".rush-compiler-discovery"),
            nix::libc::O_RDONLY | nix::libc::O_DIRECTORY,
            0,
        )?;
        let meta = directory.metadata()?;
        ensure!(
            meta.is_dir()
                && meta.uid() == nix::unistd::geteuid().as_raw()
                && meta.mode() & 0o077 == 0,
            "native admission directory is not private"
        );
        let result = Self {
            workspace,
            root,
            directory,
            created,
        };
        result.validate_binding()?;
        if created {
            use std::io::Write;
            let mut marker = admission_open(
                &result.directory,
                std::ffi::OsStr::new("BUILD.bazel"),
                nix::libc::O_WRONLY | nix::libc::O_CREAT | nix::libc::O_EXCL,
                0o600,
            )?;
            marker.write_all(ADMISSION_PACKAGE_MARKER)?;
            marker.sync_all()?;
            result.directory.sync_all()?;
        }
        result.validate()?;
        Ok(result)
    }
    fn validate(&self) -> Result<()> {
        self.validate_binding()?;
        self.validate_marker()
    }
    fn validate_binding(&self) -> Result<()> {
        let root = admission_root(&self.workspace)?;
        let named = admission_open(
            &self.root,
            std::ffi::OsStr::new(".rush-compiler-discovery"),
            nix::libc::O_RDONLY | nix::libc::O_DIRECTORY,
            0,
        )?;
        ensure!(
            admission_identity(&root.metadata()?) == admission_identity(&self.root.metadata()?)
                && admission_identity(&named.metadata()?)
                    == admission_identity(&self.directory.metadata()?),
            "native admission parent directory was replaced"
        );
        Ok(())
    }
    fn validate_marker(&self) -> Result<()> {
        use std::io::Read;
        use std::os::unix::fs::MetadataExt;
        let name = std::ffi::OsStr::new("BUILD.bazel");
        let mut marker = admission_open(&self.directory, name, nix::libc::O_RDONLY, 0)?;
        let before = marker.metadata()?;
        ensure!(
            before.is_file()
                && before.uid() == nix::unistd::geteuid().as_raw()
                && before.nlink() == 1
                && before.mode() & 0o077 == 0
                && before.len() == ADMISSION_PACKAGE_MARKER.len() as u64,
            "foreign BUILD.bazel marker in native admission directory; refusing overwrite"
        );
        let mut bytes = vec![0; ADMISSION_PACKAGE_MARKER.len()];
        marker.read_exact(&mut bytes)?;
        let named = admission_open(&self.directory, name, nix::libc::O_RDONLY, 0)?;
        ensure!(bytes==ADMISSION_PACKAGE_MARKER && admission_signature(&before)==admission_signature(&marker.metadata()?)
            && admission_signature(&before)==admission_signature(&named.metadata()?),
            "foreign or changed BUILD.bazel marker in native admission directory; refusing overwrite");
        Ok(())
    }
    fn lease(&self, create: bool) -> Result<std::fs::File> {
        self.validate()?;
        // A missing lease in a previously initialized directory is corruption,
        // never a reason to mint a second independent lock authority.
        let flags = nix::libc::O_RDWR
            | if create && self.created {
                nix::libc::O_CREAT | nix::libc::O_EXCL
            } else {
                0
            };
        let file = admission_open(&self.directory, std::ffi::OsStr::new("lease"), flags, 0o600)?;
        self.validate_file("lease", &file, false)?;
        Ok(file)
    }
    fn validate_file(&self, name: &str, file: &std::fs::File, token: bool) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        self.validate()?;
        let meta = file.metadata()?;
        ensure!(
            meta.is_file()
                && meta.nlink() == 1
                && meta.uid() == nix::unistd::geteuid().as_raw()
                && meta.mode() & 0o077 == 0
                && (!token || meta.len() == TOKEN_RECORD_LEN as u64),
            "native admission file must be a private single-link regular file"
        );
        let named = admission_open(
            &self.directory,
            std::ffi::OsStr::new(name),
            nix::libc::O_RDONLY,
            0,
        )?;
        ensure!(
            admission_signature(&named.metadata()?) == admission_signature(&meta),
            "native admission named file was replaced"
        );
        Ok(())
    }
    fn read_token(&self, lease: &std::fs::File) -> Result<[u8; 32]> {
        use std::io::Read;
        self.validate_file("lease", lease, false)?;
        let mut file = admission_open(
            &self.directory,
            std::ffi::OsStr::new("token"),
            nix::libc::O_RDONLY,
            0,
        )?;
        self.validate_file("token", &file, true)?;
        let before = file.metadata()?;
        let mut bytes = vec![0; TOKEN_RECORD_LEN];
        file.read_exact(&mut bytes)?;
        ensure!(
            admission_signature(&before) == admission_signature(&file.metadata()?),
            "native admission token changed during read"
        );
        self.validate_file("token", &file, true)?;
        let lease_id = admission_identity(&lease.metadata()?);
        ensure!(
            &bytes[..TOKEN_MAGIC.len()] == TOKEN_MAGIC
                && u64::from_le_bytes(
                    bytes[TOKEN_MAGIC.len() + 32..TOKEN_MAGIC.len() + 40].try_into()?
                ) == lease_id.0
                && u64::from_le_bytes(bytes[TOKEN_MAGIC.len() + 40..].try_into()?) == lease_id.1,
            "native admission token/lease identity mismatch; refusing split authority"
        );
        self.validate_file("lease", lease, false)?;
        Ok(bytes[TOKEN_MAGIC.len()..TOKEN_MAGIC.len() + 32].try_into()?)
    }
}

const TOKEN_MAGIC: &[u8] = b"rush-cpp-admission-v1\n";
const ADMISSION_PACKAGE_MARKER: &[u8] =
    b"# Rush private native-admission metadata; not a user package.\n";
const TOKEN_RECORD_LEN: usize = TOKEN_MAGIC.len() + 32 + 16;

#[cfg(unix)]
struct AdmissionTemporary {
    directory: std::fs::File,
    name: std::ffi::CString,
}
#[cfg(unix)]
impl Drop for AdmissionTemporary {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        unsafe {
            nix::libc::unlinkat(self.directory.as_raw_fd(), self.name.as_ptr(), 0);
        }
    }
}

#[cfg(unix)]
pub(crate) struct WorkspaceLease {
    authority: AdmissionDirectory,
    lock: nix::fcntl::Flock<std::fs::File>,
}
#[cfg(unix)]
impl WorkspaceLease {
    pub(crate) fn acquire(workspace: &std::path::Path, mode: NativeCompileMode) -> Result<Self> {
        Self::acquire_with_hook(workspace, mode, || Ok(()))
    }
    fn acquire_with_hook(
        workspace: &std::path::Path,
        mode: NativeCompileMode,
        hook: impl FnOnce() -> Result<()>,
    ) -> Result<Self> {
        ensure!(
            mode == NativeCompileMode::Literal || cfg!(target_os = "linux"),
            "diagnostic compiler discovery requires Linux before acquiring authority"
        );
        let authority = AdmissionDirectory::open(workspace, true)?;
        hook()?;
        let file = authority.lease(true)?;
        let operation = if mode == NativeCompileMode::UncachedDiagnosticV1 {
            nix::fcntl::FlockArg::LockExclusiveNonblock
        } else {
            nix::fcntl::FlockArg::LockSharedNonblock
        };
        let lock = nix::fcntl::Flock::lock(file, operation).map_err(|(_, e)| {
            anyhow!("native workspace is in use by another normal/diagnostic request: {e}")
        })?;
        authority.validate_file("lease", &lock, false)?;
        Ok(Self { authority, lock })
    }
    pub(crate) fn token(&self, rotate: bool) -> Result<[u8; 32]> {
        self.token_with_hook(rotate, || Ok(()))
    }
    fn token_with_hook(&self, rotate: bool, hook: impl FnOnce() -> Result<()>) -> Result<[u8; 32]> {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        let current = self.authority.read_token(&self.lock);
        if !rotate && current.is_ok() {
            return current;
        }
        if let Err(error) = current {
            if !error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
            {
                return Err(error);
            }
        }
        let token = rand::random::<[u8; 32]>();
        let name = format!(
            "token-new-{}",
            blake3::Hash::from(rand::random::<[u8; 32]>()).to_hex()
        );
        let mut file = admission_open(
            &self.authority.directory,
            std::ffi::OsStr::new(&name),
            nix::libc::O_WRONLY | nix::libc::O_CREAT | nix::libc::O_EXCL,
            0o600,
        )?;
        let temporary = AdmissionTemporary {
            directory: self.authority.directory.try_clone()?,
            name: admission_name(std::ffi::OsStr::new(&name))?,
        };
        let lease = admission_identity(&self.lock.metadata()?);
        file.write_all(TOKEN_MAGIC)?;
        file.write_all(&token)?;
        file.write_all(&lease.0.to_le_bytes())?;
        file.write_all(&lease.1.to_le_bytes())?;
        file.sync_all()?;
        hook()?;
        self.authority.validate_file("lease", &self.lock, false)?;
        let destination = std::ffi::CString::new("token")?;
        let fd = self.authority.directory.as_raw_fd();
        let result = if rotate {
            unsafe { nix::libc::renameat(fd, temporary.name.as_ptr(), fd, destination.as_ptr()) }
        } else {
            unsafe { nix::libc::linkat(fd, temporary.name.as_ptr(), fd, destination.as_ptr(), 0) }
        };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if !rotate && error.kind() == std::io::ErrorKind::AlreadyExists {
                drop(temporary);
                return self.authority.read_token(&self.lock);
            }
            return Err(error.into());
        }
        drop(temporary);
        self.authority.directory.sync_all()?;
        self.authority.read_token(&self.lock)
    }
}
#[cfg(not(unix))]
pub(crate) struct WorkspaceLease;
#[cfg(not(unix))]
impl WorkspaceLease {
    pub(crate) fn acquire(_: &std::path::Path, mode: NativeCompileMode) -> Result<Self> {
        ensure!(
            mode == NativeCompileMode::Literal,
            "diagnostic compiler discovery requires Linux"
        );
        Ok(Self)
    }
    pub(crate) fn token(&self, rotate: bool) -> Result<[u8; 32]> {
        ensure!(!rotate, "diagnostic token rotation requires Linux");
        // Legacy non-Unix literal execution has no shared/exclusive lease.
        // This sentinel preserves its historical uncoordinated behavior and
        // never enables the Linux-only diagnostic profile.
        Ok([0; 32])
    }
}

/// Fresh descriptor-anchored token observation. Equality revokes old admission;
/// it never establishes Freshness, read-time inputs or execution eligibility.
#[cfg(unix)]
pub(crate) fn read_admission_token(workspace: &std::path::Path) -> Result<[u8; 32]> {
    let authority = AdmissionDirectory::open(workspace, false)?;
    let lease = authority.lease(false)?;
    authority.read_token(&lease)
}
#[cfg(not(unix))]
pub(crate) fn read_admission_token(_: &std::path::Path) -> Result<[u8; 32]> {
    Ok([0; 32])
}

pub(crate) fn qualify_analysis_key(
    key: &mut crate::analysis_snapshot::AnalysisKey,
    token: &[u8; 32],
) {
    let mut hash = blake3::Hasher::new();
    hash.update(b"rush-native-admission-token-v1\0");
    hash.update(&key.daemon_compat_fingerprint);
    hash.update(token);
    key.daemon_compat_fingerprint = *hash.finalize().as_bytes();
}

/// Execution state exists for one diagnostic invocation only. Successful actual
/// actions/targets add completion facts; the table never leaves this build.
pub(crate) struct DiagnosticBuild {
    cancel: tokio::sync::watch::Sender<bool>,
    cancelled: tokio::sync::watch::Receiver<bool>,
    pub(crate) declarations: std::sync::Arc<rush_rules::context::PendingCompileDeclarations>,
    pub(crate) registry: std::sync::Arc<tokio::sync::Mutex<ArtifactRegistry>>,
    completed: std::sync::Arc<std::sync::Mutex<Vec<DeclaredActionId>>>,
    manager: std::sync::Arc<crate::action_manager::RushActionManager>,
    pub(crate) lease: std::sync::Arc<WorkspaceLease>,
    environment: Vec<(String, String)>,
    command: CommandSemantics,
    workspace: std::path::PathBuf,
    #[cfg(test)]
    inline_finished: tokio::sync::watch::Sender<u64>,
    #[cfg(test)]
    inline_hook: std::sync::Mutex<
        Option<std::sync::Arc<dyn Fn(InlineOptionsStage) -> Result<()> + Send + Sync>>,
    >,
}

impl DiagnosticBuild {
    pub(crate) fn new(
        declarations: rush_rules::context::PendingCompileDeclarations,
        registry: std::sync::Arc<tokio::sync::Mutex<ArtifactRegistry>>,
        manager: std::sync::Arc<crate::action_manager::RushActionManager>,
        lease: std::sync::Arc<WorkspaceLease>,
        command: &CommandSemantics,
        workspace: std::path::PathBuf,
        cancel: tokio::sync::watch::Sender<bool>,
        cancelled: tokio::sync::watch::Receiver<bool>,
    ) -> Result<std::sync::Arc<Self>> {
        let environment = command
            .env()
            .iter()
            .map(|(name, value)| {
                Ok((
                    std::str::from_utf8(name)?.to_owned(),
                    std::str::from_utf8(value)?.to_owned(),
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(std::sync::Arc::new(Self {
            declarations: std::sync::Arc::new(declarations),
            registry,
            completed: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            manager,
            lease,
            environment,
            command: command.clone(),
            workspace,
            #[cfg(test)]
            inline_finished: tokio::sync::watch::channel(0).0,
            #[cfg(test)]
            inline_hook: std::sync::Mutex::new(None),
            cancel,
            cancelled,
        }))
    }

    pub(crate) fn manager_for(
        self: &std::sync::Arc<Self>,
        target: rush_action::ids::TargetId,
    ) -> std::sync::Arc<dyn rush_rules::context::ActionManager> {
        std::sync::Arc::new(DiagnosticManager {
            build: self.clone(),
            target,
        })
    }

    /// Inline completion has a stricter cancellation commit boundary than the
    /// existing process path. Acquire locks in the existing registry->completed
    /// order, then check cancellation immediately before synchronous insertion.
    async fn note_inline_action(
        &self,
        action: &rush_rules::context::InlineOptionsHeaderAction,
    ) -> Result<()> {
        let registry = self.registry.lock().await;
        action.validate_registry(&registry)?;
        ensure!(
            std::fs::symlink_metadata(action.output_path())
                .is_ok_and(|metadata| metadata.is_file()),
            "inline options output is not a completed regular file"
        );
        let mut completed = self.completed.lock().unwrap_or_else(|p| p.into_inner());
        ensure!(
            !*self.cancelled.borrow(),
            "inline options cancelled before producer completion"
        );
        // No await or lock acquisition between this check and publication.
        // Cancellation after this successful commit is not a rollback promise.
        if !completed.contains(&action.action()) {
            completed.push(action.action());
        }
        Ok(())
    }
    async fn note_action(&self, id: DeclaredActionId) -> Result<()> {
        let registry = self.registry.lock().await;
        let action = registry.declared_action(id);
        for &output in action.outputs.iter() {
            ensure!(
                std::fs::symlink_metadata(registry.artifact_path(output)).is_ok(),
                "diagnostic action did not create declared output {}",
                registry.artifact_path(output).display()
            );
        }
        let mut completed = self.completed.lock().unwrap_or_else(|p| p.into_inner());
        if !completed.contains(&id) {
            completed.push(id);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InlineOptionsStage {
    BeforeRead,
    BeforeWrite,
    BeforePublish,
    AfterPublish,
    #[cfg(test)]
    BeforeCompletionLock,
}

struct DiagnosticManager {
    build: std::sync::Arc<DiagnosticBuild>,
    target: rush_action::ids::TargetId,
}

pub(crate) struct CancelOwnedExecution(Option<tokio::sync::watch::Sender<bool>>);
impl CancelOwnedExecution {
    pub(crate) fn new(sender: tokio::sync::watch::Sender<bool>) -> Self {
        Self(Some(sender))
    }
    fn disarm(&mut self) {
        self.0.take();
    }
}
impl Drop for CancelOwnedExecution {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(true);
        }
    }
}

#[async_trait::async_trait]
impl rush_rules::context::ActionManager for DiagnosticManager {
    async fn execute(&self, action: rush_action::Action) -> Result<rush_action::ActionResult> {
        ensure!(
            !matches!(action.mnemonic.as_str(), "CppCompile" | "CCompile"),
            "diagnostic C/C++ execution requires a typed pending declaration"
        );
        self.execute_owned(action, None).await
    }
    async fn execute_inline_options_header(
        &self,
        action: rush_rules::context::InlineOptionsHeaderAction,
    ) -> Result<()> {
        ensure!(
            action.target() == self.target,
            "inline options belongs to another target"
        );
        ensure!(
            requested_compile_mode(&self.build.command)? == NativeCompileMode::UncachedDiagnosticV1,
            "inline options requires this build's diagnostic request"
        );
        {
            let registry = self.build.registry.lock().await;
            action.validate_registry(&registry)?;
        }
        ensure!(
            self.build.command.config_fingerprint() == &action.inputs().command_fingerprint,
            "inline options inputs belong to another captured command"
        );
        ensure!(
            !*self.build.cancelled.borrow(),
            "inline options build was cancelled"
        );
        let mut cancel_on_drop = CancelOwnedExecution::new(self.build.cancel.clone());
        let build = self.build.clone();
        let task = tokio::spawn(async move {
            #[cfg(test)]
            let finished = build.inline_finished.clone();
            let result = async {
                let blocking_build = build.clone();
                let blocking_action = action.clone();
                // This blocking operation holds its own build/lease Arc. Dropping
                // either caller or async waiter cannot detach its writer authority.
                tokio::task::spawn_blocking(move || {
                    let boundary = |stage| -> Result<()> {
                        ensure!(
                            !*blocking_build.cancelled.borrow(),
                            "inline options build was cancelled"
                        );
                        #[cfg(test)]
                        {
                            let hook = blocking_build
                                .inline_hook
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .clone();
                            if let Some(hook) = hook {
                                hook(stage)?;
                            }
                        }
                        #[cfg(not(test))]
                        let _ = stage;
                        ensure!(
                            !*blocking_build.cancelled.borrow(),
                            "inline options build was cancelled"
                        );
                        Ok(())
                    };
                    boundary(InlineOptionsStage::BeforeRead)?;
                    let source = blocking_action
                        .read_template(&blocking_build.command, &blocking_build.workspace)?;
                    let rendered = tensorflow_rules::tf_gen_options_header::render_options_header(
                        &source,
                        blocking_action.inputs(),
                    )?;
                    let text = std::str::from_utf8(&rendered)
                        .map_err(|_| anyhow!("inline options requires a UTF-8 SOURCE template"))?;
                    let mut checks = 0;
                    rush_rules::generated_io::write_generated_file_atomically(
                        blocking_action.output_path(),
                        text,
                        || {
                            let stage = match checks {
                                0 => InlineOptionsStage::BeforeWrite,
                                1 => InlineOptionsStage::BeforePublish,
                                _ => InlineOptionsStage::AfterPublish,
                            };
                            checks += 1;
                            boundary(stage)?;
                            let current = blocking_action.read_template(
                                &blocking_build.command,
                                &blocking_build.workspace,
                            )?;
                            ensure!(
                                current == source,
                                "inline options template changed during write"
                            );
                            Ok(())
                        },
                    )
                })
                .await
                .map_err(|error| anyhow!("owned inline options writer failed: {error}"))??;
                ensure!(
                    !*build.cancelled.borrow(),
                    "inline options cancelled before producer completion"
                );
                #[cfg(test)]
                {
                    let hook = build
                        .inline_hook
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .clone();
                    if let Some(hook) = hook {
                        hook(InlineOptionsStage::BeforeCompletionLock)?;
                    }
                }
                build.note_inline_action(&action).await?;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            // Weak::upgrade()==None alone is not a destructor/lease-release
            // acknowledgement. Tests observe this only after owned work and
            // the task's build reference have actually finished dropping.
            drop(build);
            #[cfg(test)]
            finished.send_modify(|count| *count += 1);
            result
        });
        let result = task
            .await
            .map_err(|error| anyhow!("owned inline options task failed: {error}"))?;
        cancel_on_drop.disarm();
        result
    }
    async fn execute_pending_compile(
        &self,
        action: rush_action::Action,
        pending: PendingCompileDeclaration,
    ) -> Result<rush_action::ActionResult> {
        ensure!(
            pending.target() == self.target,
            "pending compile belongs to another target"
        );
        {
            let registry = self.build.registry.lock().await;
            let completed = self
                .build
                .completed
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            validate_reported_producer_order(&registry, &pending, &completed)?;
            ensure!(
                self.build.declarations.get(pending.action()) == Some(&pending),
                "pending compile is not part of this diagnostic build"
            );
        }
        self.execute_owned(action, Some(pending)).await
    }
}

impl DiagnosticManager {
    async fn execute_owned(
        &self,
        action: rush_action::Action,
        pending: Option<PendingCompileDeclaration>,
    ) -> Result<rush_action::ActionResult> {
        let (id, declared_inputs, declared_outputs) = {
            let registry = self.build.registry.lock().await;
            let matches: Vec<_> = registry
                .declared_actions()
                .filter(|(_, decl)| {
                    decl.owner_target == self.target
                        && !decl.outputs.is_empty()
                        && decl.outputs.len() == action.outputs.len()
                        && decl.outputs.iter().all(|&id| {
                            action.outputs.iter().any(|out| {
                                artifact_path(out).as_os_str()
                                    == registry.artifact_path(id).as_os_str()
                            })
                        })
                })
                .map(|(id, _)| id)
                .collect();
            ensure!(
                matches.len() == 1,
                "diagnostic action requires one exact declared producer/output shape"
            );
            let id = matches[0];
            let declaration = registry.declared_action(id);
            (
                id,
                declaration.inputs.to_vec(),
                declaration.outputs.to_vec(),
            )
        };
        if let Some(pending) = &pending {
            ensure!(pending.action() == id, "pending action identity mismatch");
        }
        let cancelled = self.build.cancelled.clone();
        let mut cancel_on_drop = CancelOwnedExecution::new(self.build.cancel.clone());
        let build = self.build.clone();
        let target = self.target;
        // The worker is deliberately not aborted when the caller disappears.
        // Cancellation signals it; its owned scope keeps the lease until the
        // runner has killed/reaped its owned writers and drained both pipes.
        let task = tokio::spawn(async move {
            let result = build
                .manager
                .execute_diagnostic_owned(
                    action,
                    pending,
                    id,
                    target,
                    declared_inputs,
                    declared_outputs,
                    build.environment.clone(),
                    build.workspace.clone(),
                    cancelled,
                    Box::new(build.lease.clone()),
                )
                .await?;
            if result.exit_code == 0 {
                build.note_action(id).await?;
            }
            Ok::<_, anyhow::Error>(result)
        });
        let result = task
            .await
            .map_err(|e| anyhow!("owned diagnostic execution task failed: {e}"))?;
        cancel_on_drop.disarm();
        result
    }
}

fn artifact_path(artifact: &rush_action::Artifact) -> &std::path::Path {
    match artifact {
        rush_action::Artifact::Source(path) => path,
        rush_action::Artifact::Derived { output_path, .. } => output_path,
    }
}

/// Finalize dependency transport only after all native command rewrites. The
/// private directory's lifetime is retained by the caller through child cleanup.
pub(crate) fn prepare_diagnostic_transport(
    action: &mut rush_action::Action,
    pending: Option<&PendingCompileDeclaration>,
    environment: &[(String, String)],
    workspace: &std::path::Path,
) -> Result<
    Option<(
        tempfile::TempDir,
        rush_action::compiler_discovery::PendingCompileDiscovery,
    )>,
> {
    use rush_action::compiler_discovery::{
        CompilerFamily, DepfileRequest, PendingCompileDiscovery,
    };
    let Some(pending) = pending else {
        return Ok(None);
    };
    let name = action
        .command
        .executable
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    ensure!(
        !matches!(name, "sh" | "bash" | "dash" | "zsh"),
        "diagnostic discovery does not admit compound/sharded compiler commands"
    );
    ensure!(
        action.command.args.iter().any(|a| a == "-c")
            && action
                .command
                .args
                .iter()
                .any(|a| std::ffi::OsStr::new(a) == pending.source_path().as_os_str()),
        "diagnostic final argv does not name its exact source/direct compile"
    );
    ensure!(
        !action.command.args.iter().any(|a| a == "--"
            || a.starts_with('@')
            || matches!(
                a.as_str(),
                "-M" | "-MM" | "-MD" | "-MMD" | "-MF" | "-MT" | "-MQ" | "-MG" | "-MP"
            )
            || ["-MF", "-MT", "-MQ", "-Wp,-M"]
                .iter()
                .any(|p| a.starts_with(p))),
        "diagnostic compiler transport refuses preexisting dependency/response/terminator options"
    );
    ensure!(
        action.outputs.len() == 1
            && artifact_path(&action.outputs[0]).as_os_str() == pending.object_path().as_os_str(),
        "diagnostic object destination does not match pending declaration"
    );
    let private = private_depfile_directory()?;
    let depfile = private.path().join("dependencies.d");
    create_private_depfile(&depfile)?;
    let request = DepfileRequest::new(depfile, rand::random())?;
    let mut env = environment.to_vec();
    for (key, value) in &action.env {
        env.retain(|(k, _)| k != key);
        env.push((key.clone(), value.clone()));
    }
    let snapshot = rush_action::command_spec::NormalizedCommandSpec {
        mnemonic: action.mnemonic.clone(),
        executable: action.command.executable.clone(),
        cwd: Some(
            action
                .command
                .working_dir
                .clone()
                .unwrap_or_else(|| workspace.to_owned()),
        ),
        argv: action.command.args.clone(),
        env,
        declared_inputs: action
            .inputs
            .iter()
            .map(|a| artifact_path(a).to_owned())
            .collect(),
        declared_outputs: action
            .outputs
            .iter()
            .map(|a| artifact_path(a).to_owned())
            .collect(),
        toolchain_identity: None,
        param_files: Vec::new(),
        env_policy: rush_action::ids::ActionEnvPolicy::ExplicitOnly,
    };
    let family = if name.contains("clang") {
        CompilerFamily::Clang
    } else {
        CompilerFamily::Gcc
    };
    let ready = PendingCompileDiscovery::new(
        family,
        snapshot,
        pending.source_path().to_owned(),
        pending.object_path().to_owned(),
        request,
    )?;
    for argument in ready.depfile_request().dependency_arguments() {
        action.command.args.push(
            argument
                .into_string()
                .map_err(|_| anyhow!("non-UTF8 diagnostic depfile argument"))?,
        );
    }
    Ok(Some((private, ready)))
}

fn private_depfile_directory() -> Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    builder.prefix("rush-compiler-discovery-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    Ok(builder.tempdir()?)
}

// GCC and Clang truncate an existing -MF file. Create that file ourselves so
// its permissions do not depend on the caller's umask (notably 0002).
fn create_private_depfile(path: &std::path::Path) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    }
    options.open(path)?;
    Ok(())
}

pub(crate) fn read_diagnostic_depfile(
    pending: &rush_action::compiler_discovery::PendingCompileDiscovery,
) -> Result<()> {
    let bytes = read_private_depfile(pending.depfile_request().path(), || Ok(()))?;
    let entry = pending.depfile_request().parse(&bytes)?;
    ensure!(
        entry
            .dependencies
            .iter()
            .any(|p| p.as_os_str() == pending.source().as_os_str()),
        "diagnostic depfile omits its requested source"
    );
    ensure!(
        std::fs::metadata(pending.object_output())?.is_file(),
        "diagnostic compiler did not produce a regular object"
    );
    eprintln!(
        "compiler discovery diagnostic: {} positive names; no cache/read certificate",
        entry.dependencies.len()
    );
    Ok(())
}

#[cfg(unix)]
fn read_private_depfile(
    path: &std::path::Path,
    after_read: impl FnOnce() -> Result<()>,
) -> Result<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let parent = std::fs::symlink_metadata(
        path.parent()
            .ok_or_else(|| anyhow!("depfile has no private parent"))?,
    )?;
    ensure!(
        parent.is_dir()
            && !parent.file_type().is_symlink()
            && parent.uid() == nix::unistd::geteuid().as_raw()
            && parent.mode() & 0o077 == 0,
        "diagnostic depfile parent is not private"
    );
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)?;
    let before = file.metadata()?;
    ensure!(
        before.is_file()
            && before.nlink() == 1
            && before.uid() == parent.uid()
            && before.mode() & 0o022 == 0
            && before.len() <= rush_action::compiler_discovery::MAX_DEPFILE_BYTES as u64,
        "diagnostic depfile must be a private single-link bounded regular file: regular={}, mode={:o}, links={}, uid={}, parent_uid={}, bytes={}",
        before.is_file(),
        before.mode(),
        before.nlink(),
        before.uid(),
        parent.uid(),
        before.len()
    );
    let mut bytes = Vec::new();
    (&mut file)
        .take(rush_action::compiler_discovery::MAX_DEPFILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    after_read()?;
    let after = file.metadata()?;
    let named = std::fs::symlink_metadata(path)?;
    let signature = |m: &std::fs::Metadata| {
        (
            m.dev(),
            m.ino(),
            m.mode(),
            m.uid(),
            m.gid(),
            m.nlink(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
        )
    };
    ensure!(
        bytes.len() as u64 == before.len()
            && signature(&before) == signature(&after)
            && signature(&before) == signature(&named),
        "diagnostic depfile changed during its descriptor/named read bracket"
    );
    Ok(bytes)
}
#[cfg(not(unix))]
fn read_private_depfile(_: &std::path::Path, _: impl FnOnce() -> Result<()>) -> Result<Vec<u8>> {
    anyhow::bail!("private compiler depfile reads require Unix")
}

// Test-only observation of completed REAL runner invocations, never a policy,
// substitute result or public runtime feature. Counts are per owned workspace.
#[cfg(test)]
static DIAGNOSTIC_PROCESS_TRACE: std::sync::Mutex<
    Vec<(std::path::PathBuf, String, DeclaredActionId)>,
> = std::sync::Mutex::new(Vec::new());
#[cfg(test)]
pub(crate) fn record_completed_diagnostic_process(
    workspace: &std::path::Path,
    mnemonic: &str,
    id: DeclaredActionId,
) {
    DIAGNOSTIC_PROCESS_TRACE
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push((workspace.to_owned(), mnemonic.to_owned(), id));
}
#[cfg(test)]
fn take_completed_diagnostic_processes(
    workspace: &std::path::Path,
) -> Vec<(String, DeclaredActionId)> {
    let mut trace = DIAGNOSTIC_PROCESS_TRACE
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let selected = trace
        .iter()
        .filter(|(root, _, _)| root == workspace)
        .map(|(_, kind, id)| (kind.clone(), *id))
        .collect();
    trace.retain(|(root, _, _)| root != workspace);
    selected
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use rush_action::ids::{
        ActionDeclarationStatus, ArtifactId, ArtifactKey, ArtifactKind, ConfigId, DeclaredAction,
        TargetId,
    };
    use rush_protocol::command::v1::{EnvironmentVariable, RunRequest};
    use rush_rules::analysis_context::{AnalysisContext, StubIncludeResolver};
    use rush_rules::context::{BuildContext, PendingCompileDeclarations, Toolchain};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn request(value: Option<&[u8]>) -> CommandSemantics {
        CommandSemantics::from_run_request(&RunRequest {
            argv: vec![Bytes::from_static(b"rush"), Bytes::from_static(b"build")],
            environment: value
                .map(|value| {
                    vec![EnvironmentVariable {
                        name: Bytes::from_static(COMPILE_DISCOVERY_ENV),
                        value: Bytes::copy_from_slice(value),
                    }]
                })
                .unwrap_or_default(),
            protocol_major: rush_protocol::PROTOCOL_MAJOR,
            protocol_minor: rush_protocol::PROTOCOL_MINOR,
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn compiler_discovery_requests_isolate_mode_and_reset_without_daemon_environment() {
        let diagnostic = request(Some(b"uncached-diagnostic-v1"));
        let reset = request(None);
        let literal = request(Some(b"literal"));
        assert_eq!(
            requested_compile_mode(&diagnostic).unwrap(),
            NativeCompileMode::UncachedDiagnosticV1
        );
        assert_eq!(
            requested_compile_mode(&reset).unwrap(),
            NativeCompileMode::Literal
        );
        assert_eq!(
            requested_compile_mode(&literal).unwrap(),
            NativeCompileMode::Literal
        );
        assert_ne!(diagnostic.config_fingerprint(), reset.config_fingerprint());
        for value in [
            b"".as_slice(),
            b"diagnostic",
            b"UNCACHED-DIAGNOSTIC-V1",
            b"literal ",
            b"\xff",
        ] {
            assert!(requested_compile_mode(&request(Some(value))).is_err());
        }
        // Revisit the first request after others in the same process: no
        // process-global selection or sticky per-daemon mode exists.
        assert_eq!(
            requested_compile_mode(&diagnostic).unwrap(),
            NativeCompileMode::UncachedDiagnosticV1
        );
    }

    fn artifact(
        reg: &mut ArtifactRegistry,
        path: &str,
        source: bool,
        kind: ArtifactKind,
        target: TargetId,
        config: ConfigId,
    ) -> ArtifactId {
        let exec_path = reg.intern_path(Path::new(path));
        reg.intern_artifact(
            ArtifactKey { config, exec_path },
            kind,
            source,
            target,
            None,
        )
    }

    fn declare(
        reg: &mut ArtifactRegistry,
        owner: TargetId,
        mnemonic: &str,
        inputs: Vec<ArtifactId>,
        outputs: Vec<ArtifactId>,
        incomplete: bool,
    ) -> DeclaredActionId {
        let mnemonic = reg.intern_string(mnemonic);
        reg.declare_action(DeclaredAction {
            owner_target: owner,
            mnemonic,
            inputs: inputs.into_boxed_slice(),
            outputs: outputs.into_boxed_slice(),
            status: if incomplete {
                ActionDeclarationStatus::Incomplete {
                    reason: "pending compiler input discovery",
                }
            } else {
                ActionDeclarationStatus::Complete
            },
        })
        .unwrap()
    }

    struct Fixture {
        registry: ArtifactRegistry,
        source: ArtifactId,
        object: ArtifactId,
        compile: DeclaredActionId,
        source_producer: DeclaredActionId,
        header_producer: DeclaredActionId,
    }

    fn fixture(prefix: &str) -> Fixture {
        let mut registry = ArtifactRegistry::new();
        let config = ConfigId(7);
        let source = artifact(
            &mut registry,
            &format!("{prefix}/generated.cc"),
            false,
            ArtifactKind::Generated,
            TargetId(1),
            config,
        );
        let header = artifact(
            &mut registry,
            &format!("{prefix}/generated.h"),
            false,
            ArtifactKind::Header,
            TargetId(2),
            config,
        );
        let object = artifact(
            &mut registry,
            &format!("{prefix}/object.o"),
            false,
            ArtifactKind::Object,
            TargetId(3),
            config,
        );
        let source_producer = declare(
            &mut registry,
            TargetId(1),
            "Genrule",
            vec![],
            vec![source],
            false,
        );
        let header_producer = declare(
            &mut registry,
            TargetId(2),
            "Genrule",
            vec![source],
            vec![header],
            false,
        );
        let compile = declare(
            &mut registry,
            TargetId(3),
            "CppCompile",
            vec![source, header],
            vec![object],
            true,
        );
        Fixture {
            registry,
            source,
            object,
            compile,
            source_producer,
            header_producer,
        }
    }

    fn pending(f: &Fixture) -> PendingCompileDeclaration {
        PendingCompileDeclaration::from_registry(
            &f.registry,
            f.compile,
            TargetId(3),
            ConfigId(7),
            f.source,
            f.object,
        )
        .unwrap()
    }

    #[test]
    fn compiler_discovery_pending_identity_and_actual_producer_edges_remain_structural() {
        let f = fixture("/build-a");
        let p = pending(&f);
        assert_eq!(
            p.required_producers(),
            &[f.source_producer, f.header_producer]
        );
        validate_reported_producer_order(&f.registry, &p, &[f.source_producer, f.header_producer])
            .unwrap();
        for order in [
            vec![],
            vec![f.source_producer],
            vec![f.header_producer, f.source_producer],
            vec![f.source_producer, f.source_producer, f.header_producer],
            vec![f.source_producer, f.header_producer, f.compile],
        ] {
            assert!(validate_reported_producer_order(&f.registry, &p, &order).is_err());
        }
        assert!(PendingCompileDeclaration::from_registry(
            &f.registry,
            f.compile,
            TargetId(99),
            ConfigId(7),
            f.source,
            f.object
        )
        .is_err());
        assert!(PendingCompileDeclaration::from_registry(
            &f.registry,
            f.compile,
            TargetId(3),
            ConfigId(8),
            f.source,
            f.object
        )
        .is_err());
        let other = fixture("/build-b");
        assert!(validate_reported_producer_order(
            &other.registry,
            &p,
            &[other.source_producer, other.header_producer]
        )
        .is_err());
        let alias = fixture("/build-a/.");
        assert!(
            validate_reported_producer_order(
                &alias.registry,
                &p,
                &[alias.source_producer, alias.header_producer]
            )
            .is_err(),
            "pending declaration identity retains exact input/output spellings"
        );
        let mut records = PendingCompileDeclarations::default();
        records.record(p.clone()).unwrap();
        assert!(records.record(pending(&other)).is_err());
        assert_eq!(
            records.get(f.compile),
            Some(&p),
            "conflict must not replace the old request"
        );
        assert!(matches!(
            f.registry.declared_action(f.compile).status,
            ActionDeclarationStatus::Incomplete { .. }
        ));
    }

    #[test]
    fn compiler_discovery_context_transport_does_not_clear_fatal_namespace_errors() {
        let mut f = fixture("/build");
        let mut declarations = PendingCompileDeclarations::default();
        let root = Path::new("/build");
        let resolver = StubIncludeResolver::new(root.to_path_buf());
        let toolchain = Toolchain::from_environment_lookup(|_| None);
        {
            let mut analysis = AnalysisContext::new(
                TargetId(3),
                ConfigId(7),
                "",
                None,
                root,
                &mut f.registry,
                &resolver,
                &toolchain,
                root,
                &[],
            );
            assert_eq!(analysis.native_compile_mode(), NativeCompileMode::Literal);
            assert!(analysis
                .record_pending_compile(f.compile, f.source, f.object)
                .is_err());
            let mut analysis = analysis.with_native_compile_discovery(
                NativeCompileMode::UncachedDiagnosticV1,
                &mut declarations,
            );
            analysis.fail_native_namespace_analysis("literal proof failed");
            analysis
                .record_pending_compile(f.compile, f.source, f.object)
                .unwrap();
            assert_eq!(
                analysis.native_namespace_analysis_error(),
                Some("literal proof failed")
            );
        }
        let record = declarations.get(f.compile).unwrap();
        assert_eq!(record.source_path(), Path::new("/build/generated.cc"));
        let build = BuildContext::new(PathBuf::from("/build"), PathBuf::from("/out"));
        assert_eq!(build.native_compile_mode(), NativeCompileMode::Literal);
        assert!(build.pending_compile_declarations().is_none());
        let build = build.with_native_compile_discovery(
            NativeCompileMode::UncachedDiagnosticV1,
            Arc::new(declarations),
        );
        assert_eq!(
            build
                .pending_compile_declarations()
                .unwrap()
                .get(f.compile)
                .unwrap()
                .object_path(),
            Path::new("/build/object.o")
        );
    }

    #[test]
    fn compiler_discovery_pending_missing_producer_and_cycles_refuse() {
        let mut registry = ArtifactRegistry::new();
        let config = ConfigId(2);
        let source = artifact(
            &mut registry,
            "/missing.cc",
            false,
            ArtifactKind::Generated,
            TargetId(1),
            config,
        );
        let object = artifact(
            &mut registry,
            "/object.o",
            false,
            ArtifactKind::Object,
            TargetId(2),
            config,
        );
        let compile = declare(
            &mut registry,
            TargetId(2),
            "CppCompile",
            vec![source],
            vec![object],
            true,
        );
        assert!(PendingCompileDeclaration::from_registry(
            &registry,
            compile,
            TargetId(2),
            config,
            source,
            object
        )
        .is_err());
        // Completing the declarations can expose a real cycle, not readiness.
        let producer = declare(
            &mut registry,
            TargetId(1),
            "Genrule",
            vec![object],
            vec![source],
            false,
        );
        let pending = PendingCompileDeclaration::from_registry(
            &registry,
            compile,
            TargetId(2),
            config,
            source,
            object,
        )
        .unwrap();
        assert!(validate_reported_producer_order(&registry, &pending, &[producer]).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn compiler_discovery_workspace_lease_excludes_cross_process_writers() -> Result<()> {
        const CHILD: &str = "RUSH_TEST_DISCOVERY_LEASE_CHILD";
        if let Some(root) = std::env::var_os(CHILD) {
            let mode = if std::env::var("RUSH_TEST_DISCOVERY_LEASE_MODE").unwrap() == "exclusive" {
                NativeCompileMode::UncachedDiagnosticV1
            } else {
                NativeCompileMode::Literal
            };
            let expected = std::env::var("RUSH_TEST_DISCOVERY_LEASE_OK").unwrap() == "yes";
            assert_eq!(
                WorkspaceLease::acquire(Path::new(&root), mode).is_ok(),
                expected
            );
            return Ok(());
        }
        let temp = tempfile::tempdir()?;
        let run = |mode: &str, success: bool| -> Result<()> {
            let status = std::process::Command::new(std::env::current_exe()?)
                .args(["--exact","compiler_discovery::tests::compiler_discovery_workspace_lease_excludes_cross_process_writers","--nocapture"])
                .env(CHILD,temp.path()).env("RUSH_TEST_DISCOVERY_LEASE_MODE",mode)
                .env("RUSH_TEST_DISCOVERY_LEASE_OK",if success {"yes"} else {"no"}).status()?;
            ensure!(status.success(), "lease child failed");
            Ok(())
        };
        let shared = WorkspaceLease::acquire(temp.path(), NativeCompileMode::Literal)?;
        run("shared", true)?;
        run("exclusive", false)?;
        drop(shared);
        let exclusive =
            WorkspaceLease::acquire(temp.path(), NativeCompileMode::UncachedDiagnosticV1)?;
        run("shared", false)?;
        run("exclusive", false)?;
        drop(exclusive);
        run("exclusive", true)?;
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn compiler_discovery_token_deletion_reinitialization_never_restores_old_key() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let workspace = std::fs::canonicalize(temp.path())?;
        let first = WorkspaceLease::acquire(&workspace, NativeCompileMode::Literal)?;
        let before = first.token(false)?;
        let mut key = crate::analysis_snapshot::AnalysisKey::new(
            &request(None),
            workspace.clone(),
            vec!["//:a".into()],
        );
        let original = key.clone();
        drop(first);
        let exclusive =
            WorkspaceLease::acquire(&workspace, NativeCompileMode::UncachedDiagnosticV1)?;
        assert_ne!(exclusive.token(true)?, before);
        drop(exclusive);
        std::fs::remove_file(workspace.join(".rush-compiler-discovery/token"))?;
        assert!(read_admission_token(&workspace).is_err());
        let next = WorkspaceLease::acquire(&workspace, NativeCompileMode::Literal)?;
        assert_ne!(next.token(false)?, before);
        key = crate::analysis_snapshot::AnalysisKey::new(
            &request(None),
            workspace.clone(),
            vec!["//:a".into()],
        );
        assert_ne!(original, key);
        Ok(())
    }

    fn build_request(root: &Path, target: &str, mode: Option<&[u8]>) -> Result<CommandSemantics> {
        let mut request = RunRequest {
            argv: vec![
                Bytes::from_static(b"rush"),
                Bytes::from_static(b"build"),
                Bytes::copy_from_slice(target.as_bytes()),
            ],
            working_directory: Bytes::copy_from_slice(root.as_os_str().as_encoded_bytes()),
            environment: vec![
                EnvironmentVariable {
                    name: Bytes::from_static(b"PATH"),
                    value: Bytes::from_static(b"/usr/bin:/bin"),
                },
                EnvironmentVariable {
                    name: Bytes::from_static(b"RUSH_EFFECTIVE_BUILD_ARGUMENTS_V1"),
                    value: Bytes::from(rush_command_core::build_arguments::encode(&[])),
                },
            ],
            protocol_major: rush_protocol::PROTOCOL_MAJOR,
            protocol_minor: rush_protocol::PROTOCOL_MINOR,
            ..Default::default()
        };
        if let Some(mode) = mode {
            request.environment.push(EnvironmentVariable {
                name: Bytes::from_static(COMPILE_DISCOVERY_ENV),
                value: Bytes::copy_from_slice(mode),
            });
        }
        rush_command_core::populate_workspace_config(&mut request, root);
        Ok(CommandSemantics::from_run_request(&request)?)
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn compiler_discovery_real_generated_macro_build_is_uncached_and_revokes_old_admissions(
    ) -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("main");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&root)?;
        std::fs::create_dir_all(&repo)?;
        std::fs::write(
            root.join("WORKSPACE"),
            format!(
                "workspace(name='discovery')\nlocal_repository(name='producer',path={:?})\n",
                repo.to_str().unwrap()
            ),
        )?;
        std::fs::write(root.join("BUILD"),"cc_binary(name='plain',srcs=['plain.cc'])\ncc_binary(name='second',srcs=['plain.cc'])\n")?;
        std::fs::write(root.join("plain.cc"), "int main(){return 40;}\n")?;
        std::fs::write(
            repo.join("BUILD"),
            r#"
genrule(name="writer",srcs=["generated.cc.in","generated.h.in"],outs=["generated.cc","generated.h"],
    cmd="cp $(location generated.cc.in) $(@D)/generated.cc; cp $(location generated.h.in) $(@D)/generated.h")
cc_library(name="generated",srcs=[":generated.cc"],hdrs=[":generated.h"],deps=[":writer"])
cc_binary(name="app",srcs=["main.cc"],deps=[":generated"])
"#,
        )?;
        std::fs::write(
            repo.join("generated.cc.in"),
            "#define API \\\n\"generated.h\"\n#include API\nint generated_result(){return RESULT;}\n",
        )?;
        std::fs::write(repo.join("generated.h.in"), "#define RESULT 42\n")?;
        std::fs::write(
            repo.join("main.cc"),
            "int generated_result(); int main(){return generated_result();}\n",
        )?;
        let orchestrator = crate::BuildOrchestrator::with_workspace(root.clone());
        let normal = build_request(&root, "//:plain", None)?;
        orchestrator
            .build_with_dependencies_with_semantics(vec!["//:plain".into()], normal.clone())
            .await?;
        let canonical = std::fs::canonicalize(&root)?;
        let old_key = crate::analysis_snapshot::AnalysisKey::new(
            &normal,
            canonical.clone(),
            vec!["//:plain".into()],
        );
        let held = orchestrator
            .snapshot_cache
            .get(&old_key)
            .ok_or_else(|| anyhow!("normal build did not retain a snapshot"))?;
        let other_root = temp.path().join("other-workspace");
        std::fs::create_dir_all(&other_root)?;
        std::fs::write(other_root.join("WORKSPACE"), "workspace(name='other')\n")?;
        std::fs::write(
            other_root.join("BUILD"),
            "cc_binary(name='plain',srcs=['plain.cc'])\n",
        )?;
        std::fs::write(other_root.join("plain.cc"), "int main(){return 41;}\n")?;
        let other = crate::BuildOrchestrator::with_workspace(other_root.clone());
        let wrong_owner = other.execute_analyzed(&held).await.unwrap_err();
        ensure!(
            format!("{wrong_owner:#}").contains("different workspace"),
            "{wrong_owner:#}"
        );
        assert!(
            !other_root.join(".rush-compiler-discovery/token").exists(),
            "wrong-owner execution must refuse before creating admission authority"
        );
        let other_request = build_request(&other_root, "//:plain", None)?;
        other
            .build_with_dependencies_with_semantics(vec!["//:plain".into()], other_request.clone())
            .await?;
        let other_key = crate::analysis_snapshot::AnalysisKey::new(
            &other_request,
            other_root.canonicalize()?,
            vec!["//:plain".into()],
        );
        let other_token = read_admission_token(&other_root)?;
        let other_snapshot = other.snapshot_cache.get(&other_key).unwrap();
        let second = build_request(&root, "//:second", None)?;
        orchestrator
            .build_with_dependencies_with_semantics(vec!["//:second".into()], second.clone())
            .await?;
        let second_key = crate::analysis_snapshot::AnalysisKey::new(
            &second,
            canonical.clone(),
            vec!["//:second".into()],
        );
        assert!(
            crate::cold_noop_cache::load(&root, &old_key).is_some(),
            "normal seed requires an actual cold certificate"
        );
        let diagnostic = build_request(&root, "@producer//:app", Some(b"uncached-diagnostic-v1"))?;
        let mut previous = None;
        for expected in [42, 42, 43] {
            if expected == 43 {
                std::fs::write(repo.join("generated.h.in"), "#define RESULT 43\n")?;
            }
            orchestrator
                .build_with_dependencies_with_semantics(
                    vec!["@producer//:app".into()],
                    diagnostic.clone(),
                )
                .await?;
            let events = take_completed_diagnostic_processes(&canonical);
            let mut counts = std::collections::BTreeMap::new();
            for (mnemonic, _) in &events {
                let kind = match mnemonic.as_str() {
                    "GenRule" => "producer",
                    "CCompile" | "CppCompile" => "compile",
                    "CcArchive" | "CppArchive" => "archive",
                    "CcLink" | "CppLink" => "link",
                    other => other,
                };
                *counts.entry(kind.to_owned()).or_insert(0usize) += 1;
            }
            assert_eq!(
                counts,
                std::collections::BTreeMap::from([
                    ("producer".into(), 1usize),
                    ("compile".into(), 2),
                    ("archive".into(), 1),
                    ("link".into(), 1)
                ]),
                "each unchanged diagnostic must complete every real process family: {events:?}"
            );
            eprintln!("actual diagnostic completed process counts: {counts:?}");
            let key = crate::analysis_snapshot::AnalysisKey::new(
                &diagnostic,
                canonical.clone(),
                vec!["@producer//:app".into()],
            );
            assert!(orchestrator.snapshot_cache.get(&key).is_none());
            assert!(crate::cold_noop_cache::load(&root, &key).is_none());
            let binary = root.join("rush-out/external/repo-producer/app");
            assert_eq!(
                std::process::Command::new(&binary).status()?.code(),
                Some(expected)
            );
            let metadata = std::fs::metadata(&binary)?;
            let stamp = metadata.modified()?;
            if let Some(previous) = previous {
                assert_ne!(previous, stamp, "repeat diagnostic reused an executable");
            }
            previous = Some(stamp);
        }
        assert!(orchestrator.snapshot_cache.get(&old_key).is_none());
        assert!(orchestrator.snapshot_cache.get(&second_key).is_none());
        assert!(
            orchestrator.execute_analyzed(&held).await.is_err(),
            "held normal Arc must be revoked"
        );
        assert!(
            crate::cold_noop_cache::load(&root, &old_key).is_none(),
            "restart cannot load the old token domain"
        );
        assert_eq!(read_admission_token(&other_root)?, other_token);
        assert!(other.snapshot_cache.get(&other_key).is_some());
        assert!(
            crate::cold_noop_cache::load(&other_root, &other_key).is_some(),
            "another workspace's cold authority must survive"
        );
        other.execute_analyzed(&other_snapshot).await?;
        let literal = build_request(&root, "@producer//:app", None)?;
        let error = orchestrator
            .build_with_dependencies_with_semantics(vec!["@producer//:app".into()], literal)
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("does not admit"),
            "literal proof guard must remain: {error:#}"
        );
        // A seeded old executable cannot make a compiler error a success.
        std::fs::write(
            repo.join("generated.h.in"),
            "#error actual compile failure\n",
        )?;
        assert!(orchestrator
            .build_with_dependencies_with_semantics(vec!["@producer//:app".into()], diagnostic)
            .await
            .is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn compiler_discovery_depfile_rejects_same_size_mutation_replacement_and_aliases() -> Result<()>
    {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let temp = private_depfile_directory()?;
        let path = temp.path().join("owned.d");
        create_private_depfile(&path)?;
        std::fs::write(&path, b"target: old.h\n")?;
        assert_eq!(read_private_depfile(&path, || Ok(()))?, b"target: old.h\n");
        assert!(
            read_private_depfile(&path, || {
                std::fs::write(&path, b"target: new.h\n")?;
                Ok(())
            })
            .is_err(),
            "same-size mutation must change the descriptor bracket"
        );
        assert!(
            read_private_depfile(&path, || {
                let replacement = temp.path().join("replacement.d");
                create_private_depfile(&replacement)?;
                std::fs::write(&replacement, b"target: alt.h\n")?;
                std::fs::rename(replacement, &path)?;
                Ok(())
            })
            .is_err(),
            "same-size pathname replacement must not reuse the old fd"
        );
        let alias = temp.path().join("alias.d");
        std::fs::hard_link(&path, &alias)?;
        assert!(read_private_depfile(&path, || Ok(())).is_err());
        std::fs::remove_file(&alias)?;
        symlink(&path, &alias)?;
        assert!(read_private_depfile(&alias, || Ok(())).is_err());
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o755))?;
        assert!(
            read_private_depfile(&path, || Ok(())).is_err(),
            "public parent is not owned depfile transport"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[cfg(target_os = "linux")]
    #[test]
    fn compiler_discovery_authority_parent_swap_and_lease_replacement_refuse() -> Result<()> {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let temp = tempfile::tempdir()?;
        let workspace = temp.path().join("workspace");
        let foreign = temp.path().join("foreign");
        std::fs::create_dir(&workspace)?;
        std::fs::create_dir(&foreign)?;
        let authority = workspace.join(".rush-compiler-discovery");
        let saved = workspace.join("saved-authority");
        let result =
            WorkspaceLease::acquire_with_hook(&workspace, NativeCompileMode::Literal, || {
                std::fs::rename(&authority, &saved)?;
                symlink(&foreign, &authority)?;
                Ok(())
            });
        assert!(result.is_err());
        assert_eq!(
            std::fs::read_dir(&foreign)?.count(),
            0,
            "parent swap must not create a lease in the foreign directory"
        );
        std::fs::remove_file(&authority)?;
        std::fs::rename(&saved, &authority)?;
        // The first attempt failed before creating a lease. A partial authority
        // never silently grows an independent lock on a subsequent request.
        assert!(WorkspaceLease::acquire(&workspace, NativeCompileMode::Literal).is_err());
        std::fs::remove_file(authority.join("BUILD.bazel"))?;
        std::fs::remove_dir(&authority)?;
        let lease = WorkspaceLease::acquire(&workspace, NativeCompileMode::Literal)?;
        lease.token(false)?;
        std::fs::rename(authority.join("lease"), authority.join("old-lease"))?;
        std::fs::write(authority.join("lease"), b"")?;
        std::fs::set_permissions(
            authority.join("lease"),
            std::fs::Permissions::from_mode(0o600),
        )?;
        assert!(
            lease.token(false).is_err(),
            "held lease must reject a new named lock inode"
        );
        assert!(
            read_admission_token(&workspace).is_err(),
            "stored token binds the original lease inode"
        );
        drop(lease);
        let fresh = WorkspaceLease::acquire(&workspace, NativeCompileMode::Literal)?;
        assert!(
            fresh.token(false).is_err(),
            "new lock must not adopt the old token domain"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[cfg(target_os = "linux")]
    #[test]
    fn compiler_discovery_token_parent_swap_never_publishes_in_foreign_directory() -> Result<()> {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir()?;
        let foreign = temp.path().join("foreign");
        std::fs::create_dir(&foreign)?;
        let workspace = temp.path().join("workspace");
        std::fs::create_dir(&workspace)?;
        let lease = WorkspaceLease::acquire(&workspace, NativeCompileMode::UncachedDiagnosticV1)?;
        lease.token(false)?;
        let authority = workspace.join(".rush-compiler-discovery");
        let saved = workspace.join("saved-authority");
        assert!(lease
            .token_with_hook(true, || {
                std::fs::rename(&authority, &saved)?;
                symlink(&foreign, &authority)?;
                Ok(())
            })
            .is_err());
        assert_eq!(
            std::fs::read_dir(&foreign)?.count(),
            0,
            "token writes stay anchored to owned fd"
        );
        assert_eq!(
            std::fs::read_dir(&saved)?.count(),
            3,
            "temporary token cleanup uses the owned fd too"
        );
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn compiler_discovery_unaudited_gentbl_refuses_before_any_graph_writer() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        std::fs::write(root.join("WORKSPACE"), "workspace(name='guard')\n")?;
        std::fs::write(
            root.join("BUILD"),
            r#"
genrule(name="writer",outs=["never.txt"],cmd="echo must-not-run > $@")
gentbl_cc_library(name="blocked",td_file="input.td",tbl_outs=[(["-gen-op-decls"],"bad.inc")])
"#,
        )?;
        std::fs::write(
            root.join("input.td"),
            "// genuine declaration; no tblgen is invoked\n",
        )?;
        let orchestrator = crate::BuildOrchestrator::with_workspace(root.to_owned());
        let command = build_request(root, "//:writer", Some(b"uncached-diagnostic-v1"))?;
        let error = orchestrator
            .build_with_dependencies_with_semantics(
                vec!["//:writer".into(), "//:blocked".into()],
                command,
            )
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("does not audit rule gentbl_cc_library"),
            "{error:#}"
        );
        assert!(
            !root.join("rush-out/genfiles/never.txt").exists(),
            "audit must finish before the first writer"
        );
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn compiler_discovery_unsupported_diagnostic_platform_refuses_before_authority_mutation(
    ) -> Result<()> {
        let root = tempfile::tempdir()?;
        assert!(
            WorkspaceLease::acquire(root.path(), NativeCompileMode::UncachedDiagnosticV1).is_err()
        );
        assert!(!root.path().join(".rush-compiler-discovery").exists());
        #[cfg(not(unix))]
        {
            let literal = WorkspaceLease::acquire(root.path(), NativeCompileMode::Literal)?;
            assert_eq!(literal.token(false)?, [0; 32]);
            assert!(!root.path().join(".rush-compiler-discovery").exists());
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn compiler_discovery_real_compiler_faults_never_use_seeded_cache_cas_or_virtual_outputs(
    ) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir()?;
        let root = std::fs::canonicalize(temp.path())?;
        let source = root.join("source.c");
        let header = root.join("required.h");
        std::fs::write(&header, "#define UNUSED_REQUIRED_INPUT 1\n")?;
        let object = root.join("object.o");
        let wrapper = root.join("delegating-cc");
        let trace = root.join("actual-compiler-runs");
        let previous = root.join("previous.d");
        std::fs::write(&source, "int actual_value(void){return 42;}\n")?;
        // Every control ALWAYS delegates to the genuine compiler first. Only
        // after successful real codegen does it corrupt/remove owned transport
        // or output as an explicit negative control.
        std::fs::write(
            &wrapper,
            r#"#!/bin/sh
set -eu
depfile=; object=; next=
for argument in "$@"; do
  if [ "$next" = depfile ]; then depfile=$argument; next=; continue; fi
  if [ "$next" = object ]; then object=$argument; next=; continue; fi
  case "$argument" in -MF) next=depfile;; -o) next=object;; esac
done
printf 'actual gcc invocation\n' >> "$RUSH_TEST_COMPILER_TRACE"
/usr/bin/gcc "$@"
if [ -n "$depfile" ]; then
  test "$(stat -c %a "$depfile")" = 600
  case "$RUSH_TEST_COMPILER_FAULT" in
    save) cp "$depfile" "$RUSH_TEST_PREVIOUS_DEPFILE";;
    malformed) printf '\nsecond: unrelated.h\n' >> "$depfile";;
    stale) cp "$RUSH_TEST_PREVIOUS_DEPFILE" "$depfile";;
    nothing) rm "$object";;
  esac
fi
"#,
        )?;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))?;
        let cache = Arc::new(rush_cache::ActionCache::new());
        let cas = Arc::new(rush_cache::cas::CasStore::new(&root.join("owned-cas"))?);
        let manager = Arc::new(crate::action_manager::RushActionManager::new_with_cas(
            cache.clone(),
            Arc::new(rush_exec::LocalStrategy),
            rush_resource::ResourceManager::new()?,
            Some(cas.clone()),
        ));
        let mut registry = ArtifactRegistry::new();
        let config = ConfigId::DEFAULT;
        let target = TargetId(1);
        let source_id = artifact(
            &mut registry,
            source.to_str().unwrap(),
            true,
            ArtifactKind::Source,
            target,
            config,
        );
        let header_id = artifact(
            &mut registry,
            header.to_str().unwrap(),
            true,
            ArtifactKind::Header,
            target,
            config,
        );
        let object_id = artifact(
            &mut registry,
            object.to_str().unwrap(),
            false,
            ArtifactKind::Object,
            target,
            config,
        );
        let id = declare(
            &mut registry,
            target,
            "CCompile",
            vec![source_id, header_id],
            vec![object_id],
            true,
        );
        let pending = PendingCompileDeclaration::from_registry(
            &registry, id, target, config, source_id, object_id,
        )?;
        let mut declarations = PendingCompileDeclarations::default();
        declarations.record(pending.clone())?;
        let lease = Arc::new(WorkspaceLease::acquire(
            &root,
            NativeCompileMode::UncachedDiagnosticV1,
        )?);
        let registry = Arc::new(tokio::sync::Mutex::new(registry));
        let new_build = || -> Result<Arc<DiagnosticBuild>> {
            lease.token(true)?;
            let (cancel, cancelled) = tokio::sync::watch::channel(false);
            DiagnosticBuild::new(
                declarations.clone(),
                registry.clone(),
                manager.clone(),
                lease.clone(),
                &build_request(&root, "//:unit", Some(b"uncached-diagnostic-v1"))?,
                root.clone(),
                cancel,
                cancelled,
            )
        };
        let action_for = |fault: &str| -> Result<rush_action::Action> {
            let inputs = vec![
                rush_action::Artifact::Source(source.clone()),
                rush_action::Artifact::Source(header.clone()),
            ];
            let command = rush_action::Command {
                executable: wrapper.clone(),
                args: vec![
                    "-c".into(),
                    source.display().to_string(),
                    "-o".into(),
                    object.display().to_string(),
                ],
                working_dir: Some(root.clone()),
                execution_info: Default::default(),
            };
            let env = std::collections::HashMap::from([
                (
                    "RUSH_TEST_COMPILER_TRACE".into(),
                    trace.display().to_string(),
                ),
                (
                    "RUSH_TEST_PREVIOUS_DEPFILE".into(),
                    previous.display().to_string(),
                ),
                ("RUSH_TEST_COMPILER_FAULT".into(), fault.to_owned()),
            ]);
            Ok(rush_action::Action {
                id: rush_action::Action::compute_id(&inputs, &command, &env)?,
                inputs,
                outputs: vec![rush_action::Artifact::Source(object.clone())],
                command,
                env,
                mnemonic: "CCompile".into(),
            })
        };
        for fault in ["save", "malformed", "stale", "nothing"] {
            let action = action_for(fault)?;
            let seed = manager
                .execute_with_cache_policy(
                    action.clone(),
                    crate::action_manager::ActionCachePolicy::CACHEABLE,
                )
                .await?;
            ensure!(
                seed.exit_code == 0 && seed.output_records.len() == 1,
                "real normal compiler seed failed"
            );
            assert!(cas.has(seed.output_records[0].content_digest.as_ref().unwrap()));
            let cached = cache.get(&action.id).unwrap();
            rush_action::virtual_artifacts::register(&seed.output_records);
            std::fs::remove_file(&object)?;
            let before = std::fs::read_to_string(&trace)?.lines().count();
            let build = new_build()?;
            let diagnostic = build.manager_for(target);
            let result = diagnostic
                .execute_pending_compile(action.clone(), pending.clone())
                .await;
            assert_eq!(
                std::fs::read_to_string(&trace)?.lines().count(),
                before + 1,
                "diagnostic must really invoke gcc despite seeded cache/CAS/virtual metadata"
            );
            assert!(!rush_action::virtual_artifacts::is_registered(&object));
            assert_eq!(
                cache.get(&action.id),
                Some(cached),
                "diagnostic must not replace a cache entry"
            );
            if fault == "save" {
                let result = result?;
                assert!(!result.cached && result.output_records.is_empty());
            } else {
                let error = format!("{:#}", result.expect_err("fault must refuse"));
                let expected = match fault {
                    "malformed" => "another rule or trailing data",
                    "stale" => "target does not match",
                    "nothing" => "did not create output",
                    _ => unreachable!(),
                };
                assert!(
                    error.contains(expected),
                    "fault {fault} refused for the wrong cause: {error}"
                );
            }
            if fault == "nothing" {
                assert!(
                    !object.exists(),
                    "old virtual/CAS output must not satisfy produced-nothing success"
                );
            }
        }
        std::fs::write(&source, "int actual_value(void){return 43;}\n")?;
        let changed = action_for("save")?;
        assert!(cache.get(&changed.id).is_none());
        let build = new_build()?;
        let diagnostic = build.manager_for(target);
        let result = diagnostic
            .execute_pending_compile(changed.clone(), pending.clone())
            .await?;
        assert!(
            !result.cached && result.output_records.is_empty() && cache.get(&changed.id).is_none()
        );
        let changed_digest = rush_action::artifact_value::content_digest_of_file(&object)?;
        assert!(
            !cas.has(&changed_digest),
            "diagnostic changed bytes must not be published into CAS"
        );
        let required_record = cas.record_output_durable(&header)?;
        rush_action::virtual_artifacts::register(&[required_record]);
        struct InputVirtualCleanup(PathBuf);
        impl Drop for InputVirtualCleanup {
            fn drop(&mut self) {
                rush_action::virtual_artifacts::unregister(&[self.0.clone()]);
            }
        }
        let _cleanup = InputVirtualCleanup(header.clone());
        let missing = action_for("save")?;
        let before = std::fs::read(&object)?;
        let invocations = std::fs::read_to_string(&trace)?.lines().count();
        std::fs::remove_file(&header)?;
        let build = new_build()?;
        let error = build
            .manager_for(target)
            .execute_pending_compile(missing, pending)
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("requires a real regular input"),
            "{error:#}"
        );
        assert!(
            !header.exists(),
            "diagnostic must never JIT-restore a required input from virtual/CAS metadata"
        );
        assert!(
            rush_action::virtual_artifacts::is_registered(&header),
            "input metadata must not be globally cleared"
        );
        assert_eq!(
            std::fs::read(&object)?,
            before,
            "input refusal precedes output mutation"
        );
        assert_eq!(
            std::fs::read_to_string(&trace)?.lines().count(),
            invocations,
            "missing input must refuse before compiler spawn"
        );
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn compiler_discovery_graph_cancellation_keeps_lease_until_producer_cleanup() -> Result<()>
    {
        let temp = tempfile::tempdir()?;
        let root = temp.path().to_owned();
        let release = root.join("release-writer");
        std::fs::write(root.join("WORKSPACE"), "workspace(name='cancel')\n")?;
        std::fs::write(root.join("main.cc"), "int main(){return 42;}\n")?;
        std::fs::write(
            root.join("BUILD"),
            format!(
                r#"
genrule(name="writer",outs=["started.txt","late.txt"],
    cmd="echo started > $(@D)/started.txt; (while [ ! -e '{}' ]; do sleep 0.01; done; echo late > $(@D)/late.txt) & wait")
cc_binary(name="app",srcs=["main.cc"],deps=[":writer"])
"#,
                release.display()
            ),
        )?;
        let orchestrator = Arc::new(crate::BuildOrchestrator::with_workspace(root.clone()));
        let command = build_request(&root, "//:app", Some(b"uncached-diagnostic-v1"))?;
        let mut build = Box::pin(
            orchestrator.build_with_dependencies_with_semantics(vec!["//:app".into()], command),
        );
        let started = root.join("rush-out/genfiles/started.txt");
        let late = root.join("rush-out/genfiles/late.txt");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        tokio::select! {
            result = &mut build => {
                anyhow::bail!("producer finished/refused before cancellation control: {result:?}");
            }
            result = async {
                while !started.exists() {
                    ensure!(tokio::time::Instant::now() < deadline, "producer did not reach actual execution");
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                Ok::<_, anyhow::Error>(())
            } => result?,
        }
        assert!(
            WorkspaceLease::acquire(&root, NativeCompileMode::Literal).is_err(),
            "live producer must own exclusive lease"
        );
        drop(build);
        let after = loop {
            if let Ok(lease) = WorkspaceLease::acquire(&root, NativeCompileMode::Literal) {
                break lease;
            }
            ensure!(
                tokio::time::Instant::now() < deadline,
                "cancelled producer did not release after confirmed cleanup"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        };
        // The writer would act immediately after this real control file
        // appears. It must already be dead before another lease is granted.
        std::fs::write(&release, b"release")?;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(
            !late.exists(),
            "cancelled producer wrote after lease release"
        );
        assert!(
            !root.join("rush-out/app").exists(),
            "dependent compile/link must not run after cancellation"
        );
        let key = crate::analysis_snapshot::AnalysisKey::new(
            &build_request(&root, "//:app", Some(b"uncached-diagnostic-v1"))?,
            root.canonicalize()?,
            vec!["//:app".into()],
        );
        assert!(orchestrator.snapshot_cache.get(&key).is_none());
        assert!(crate::cold_noop_cache::load(&root, &key).is_none());
        drop(after);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn compiler_discovery_test_error_drains_real_child_before_lease_release() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir()?;
        let root = temp.path().to_owned();
        let started = root.join("started");
        let release = root.join("release");
        let finished = root.join("finished");
        let binary = root.join("real-test-child");
        std::fs::write(
            &binary,
            format!(
                "#!/bin/sh\nset -eu\necho started > '{}'\ncount=0\nwhile [ ! -e '{}' ]; do count=$((count+1)); [ $count -lt 500 ] || exit 71; sleep 0.01; done\necho finished > '{}'\n",
                started.display(), release.display(), finished.display()
            ),
        )?;
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700))?;
        let lease = WorkspaceLease::acquire(&root, NativeCompileMode::Literal)?;
        lease.token(false)?;
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(async move {
            crate::BuildOrchestrator::run_test_static(
                &binary,
                "//:actual_child",
                crate::TestOutputMode::Summary,
            )
            .await
        });
        let (failed, observed_failure) = tokio::sync::oneshot::channel();
        tasks.spawn(async move {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
            while !started.exists() {
                ensure!(
                    tokio::time::Instant::now() < deadline,
                    "test child did not start"
                );
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            let _ = failed.send(());
            anyhow::bail!("intentional sibling test error")
        });
        let collector = tokio::spawn(async move {
            let _lease = lease;
            crate::BuildOrchestrator::collect_started_test_results(
                &mut tasks,
                &crate::events::BuildEventStream::new(false),
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(3), observed_failure).await??;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let kept_lease =
            WorkspaceLease::acquire(&root, NativeCompileMode::UncachedDiagnosticV1).is_err();
        let still_waiting = !collector.is_finished();
        // Always release the real child before asserting, including on an old
        // broken collector implementation, so this control leaves no writer.
        std::fs::write(&release, b"release")?;
        let error = tokio::time::timeout(std::time::Duration::from_secs(3), collector).await??;
        assert!(error.is_err());
        assert!(format!("{:#}", error.unwrap_err()).contains("intentional sibling test error"));
        assert!(
            finished.exists(),
            "collector returned before actual child completion"
        );
        assert!(
            still_waiting && kept_lease,
            "sibling failure released shared writer authority early"
        );
        assert!(WorkspaceLease::acquire(&root, NativeCompileMode::UncachedDiagnosticV1).is_ok());
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn compiler_discovery_root_glob_excludes_owned_admission_subpackage() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        std::fs::write(root.join("WORKSPACE"), "workspace(name='glob')\n")?;
        std::fs::write(
            root.join("BUILD"),
            "filegroup(name='inputs',srcs=glob(['**']))\n",
        )?;
        std::fs::write(root.join("data.txt"), "real user source\n")?;
        let orchestrator = crate::BuildOrchestrator::with_workspace(root.to_owned());
        let command = build_request(root, "//:inputs", None)?;
        orchestrator
            .build_with_dependencies_with_semantics(vec!["//:inputs".into()], command.clone())
            .await?;
        let key = crate::analysis_snapshot::AnalysisKey::new(
            &command,
            root.canonicalize()?,
            vec!["//:inputs".into()],
        );
        let snapshot = orchestrator
            .snapshot_cache
            .get(&key)
            .ok_or_else(|| anyhow!("filegroup did not retain its normal snapshot"))?;
        let state = snapshot
            .retained_execution_state()
            .ok_or_else(|| anyhow!("filegroup artifacts missing"))?;
        let artifacts = state.artifact_forest["//:inputs"].flatten();
        assert!(
            artifacts
                .iter()
                .any(|artifact| artifact.path == root.join("data.txt")),
            "real glob source must be returned"
        );
        assert!(
            artifacts.iter().all(|artifact| !artifact
                .path
                .components()
                .any(|component| component.as_os_str()
                    == std::ffi::OsStr::new(".rush-compiler-discovery"))),
            "parent-package glob must not ingest token/lease/marker artifacts"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn compiler_discovery_foreign_subpackage_marker_is_preserved_and_refused() -> Result<()> {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir()?;
        let lease = WorkspaceLease::acquire(temp.path(), NativeCompileMode::Literal)?;
        lease.token(false)?;
        drop(lease);
        let marker = temp.path().join(".rush-compiler-discovery/BUILD.bazel");
        let foreign = b"# user-owned package marker\n";
        std::fs::write(&marker, foreign)?;
        assert!(WorkspaceLease::acquire(temp.path(), NativeCompileMode::Literal).is_err());
        assert_eq!(std::fs::read(&marker)?, foreign);
        std::fs::remove_file(&marker)?;
        let target = temp.path().join("foreign-BUILD");
        std::fs::write(&target, foreign)?;
        symlink(&target, &marker)?;
        assert!(WorkspaceLease::acquire(temp.path(), NativeCompileMode::Literal).is_err());
        assert_eq!(std::fs::read(&target)?, foreign);
        assert!(std::fs::symlink_metadata(&marker)?.file_type().is_symlink());
        Ok(())
    }
    #[cfg(target_os = "linux")]
    struct InlineOptionsFixture {
        build: Arc<DiagnosticBuild>,
        action: rush_rules::context::InlineOptionsHeaderAction,
        pending: PendingCompileDeclaration,
        compile: rush_action::Action,
    }

    #[cfg(target_os = "linux")]
    fn inline_options_fixture(root: &Path) -> Result<InlineOptionsFixture> {
        use rush_incremental::TrackedIo;
        use rush_rules::providers::{
            ConfiguredBuildSettingInfo, OptionsHeaderRequest, OptionsHeaderSource,
            VerifiedOptionsHeaderInputs,
        };
        let source = root.join("consumer.cc");
        let template = root.join("options.tpl");
        let header = root.join("genfiles/options.h");
        let object = root.join("consumer.o");
        std::fs::write(
            &source,
            "#include \"options.h\"\nint actual_option(){return TF_OPTION_VALUE() ? 43 : 42;}\n",
        )?;
        std::fs::write(&template, "#define_option VALUE\n")?;
        std::fs::create_dir(header.parent().unwrap())?;
        std::fs::write(&header, "previous output must not count as completed\n")?;
        let command = build_request(root, "//:consumer", Some(b"uncached-diagnostic-v1"))?;
        let io = rush_incremental::RealTrackedIo::new(command.clone(), root);
        let read = io
            .read_canonical_regular_file(&template)?
            .ok_or_else(|| anyhow!("bound read unsupported"))?;
        let provider_key = "@bazel_skylib//rules:common_settings.bzl%BuildSettingInfo".to_owned();
        // Structural lifecycle fixture only: genuine provider/query semantics
        // are separately tested by the real Bazel options integration.
        let inputs = Arc::new(VerifiedOptionsHeaderInputs {
            request: OptionsHeaderRequest {
                consumer_label: "//:header".into(),
                template_label: "//:options.tpl".into(),
                output_label: "//:options.h".into(),
                settings: std::collections::BTreeMap::from([("//:flag".into(), "VALUE".into())]),
                template_path: template.clone(),
                provider_key: provider_key.clone(),
                expected_implementation_hash: "b".repeat(64),
            },
            command_fingerprint: *command.config_fingerprint(),
            main_workspace_root: root.to_owned(),
            consumer_configuration: "a".repeat(64),
            consumer_configuration_id: 1,
            settings: std::collections::BTreeMap::from([(
                "//:flag".into(),
                ConfiguredBuildSettingInfo {
                    requested_label: "//:flag".into(),
                    producer_label: "//:flag".into(),
                    consumed_label: "@//:flag".into(),
                    provider_key,
                    value: true,
                    configuration_checksum: "a".repeat(64),
                    configuration_id: 1,
                    observed_occurrences: 1,
                },
            )]),
            template: OptionsHeaderSource {
                label: "//:options.tpl".into(),
                path: template.clone(),
                digest: *blake3::hash(&read.contents).as_bytes(),
                metadata: read.metadata,
            },
        });
        let mut registry = ArtifactRegistry::new();
        let config = ConfigId(7);
        let template_id = artifact(
            &mut registry,
            template.to_str().unwrap(),
            true,
            ArtifactKind::Source,
            TargetId(3),
            config,
        );
        let header_id = artifact(
            &mut registry,
            header.to_str().unwrap(),
            false,
            ArtifactKind::Header,
            TargetId(1),
            config,
        );
        let writer = declare(
            &mut registry,
            TargetId(1),
            "GenOptionsHeader",
            vec![template_id],
            vec![header_id],
            false,
        );
        let action = rush_rules::context::InlineOptionsHeaderAction::from_registry(
            &registry,
            writer,
            TargetId(1),
            config,
            inputs,
        )?;
        let source_id = artifact(
            &mut registry,
            source.to_str().unwrap(),
            true,
            ArtifactKind::Source,
            TargetId(2),
            config,
        );
        let object_id = artifact(
            &mut registry,
            object.to_str().unwrap(),
            false,
            ArtifactKind::Object,
            TargetId(2),
            config,
        );
        let compile_id = declare(
            &mut registry,
            TargetId(2),
            "CppCompile",
            vec![source_id, header_id],
            vec![object_id],
            true,
        );
        let pending = PendingCompileDeclaration::from_registry(
            &registry,
            compile_id,
            TargetId(2),
            config,
            source_id,
            object_id,
        )?;
        let mut declarations = PendingCompileDeclarations::default();
        declarations.record(pending.clone())?;
        let compile_inputs = vec![
            rush_action::Artifact::Source(source.clone()),
            rush_action::Artifact::Source(header.clone()),
        ];
        let compile_command = rush_action::Command {
            executable: "/usr/bin/g++".into(),
            args: vec![
                "-I".into(),
                header.parent().unwrap().display().to_string(),
                "-c".into(),
                source.display().to_string(),
                "-o".into(),
                object.display().to_string(),
            ],
            working_dir: Some(root.to_owned()),
            execution_info: Default::default(),
        };
        let env = std::collections::HashMap::new();
        let compile = rush_action::Action {
            id: rush_action::Action::compute_id(&compile_inputs, &compile_command, &env)?,
            inputs: compile_inputs,
            outputs: vec![rush_action::Artifact::Source(object)],
            command: compile_command,
            env,
            mnemonic: "CppCompile".into(),
        };
        let manager = Arc::new(crate::action_manager::RushActionManager::new_with_cas(
            Arc::new(rush_cache::ActionCache::new()),
            Arc::new(rush_exec::LocalStrategy),
            rush_resource::ResourceManager::new()?,
            None,
        ));
        let lease = Arc::new(WorkspaceLease::acquire(
            root,
            NativeCompileMode::UncachedDiagnosticV1,
        )?);
        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        let build = DiagnosticBuild::new(
            declarations,
            Arc::new(tokio::sync::Mutex::new(registry)),
            manager,
            lease,
            &command,
            root.to_owned(),
            cancel,
            cancelled,
        )?;
        Ok(InlineOptionsFixture {
            build,
            action,
            pending,
            compile,
        })
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inline_options_owned_write_releases_actual_pending_compile_only_after_completion(
    ) -> Result<()> {
        use rush_rules::context::ActionManager;
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let fixture = inline_options_fixture(&root)?;
        let compiler = fixture.build.manager_for(TargetId(2));
        assert!(
            compiler
                .execute_pending_compile(fixture.compile.clone(), fixture.pending.clone())
                .await
                .is_err(),
            "preexisting header bytes cannot report a producer complete"
        );
        assert!(!root.join("consumer.o").exists());
        assert!(
            fixture
                .build
                .manager
                .execute_inline_options_header(fixture.action.clone())
                .await
                .is_err(),
            "ordinary manager must not acquire inline authority"
        );
        assert!(fixture
            .build
            .manager_for(TargetId(99))
            .execute_inline_options_header(fixture.action.clone())
            .await
            .is_err());
        let inputs = Arc::new(fixture.action.inputs().clone());
        let context = || {
            BuildContext::new(root.clone(), root.clone())
                .with_options_header_command(fixture.build.command.clone())
                .with_artifact_registry(fixture.build.registry.clone())
                .with_action_manager(fixture.build.manager_for(TargetId(1)))
                .with_native_compile_discovery(
                    NativeCompileMode::UncachedDiagnosticV1,
                    fixture.build.declarations.clone(),
                )
        };
        let absent_config = context().with_options_header_inputs(inputs.clone())?;
        assert!(absent_config
            .execute_inline_options_header(fixture.action.output_path())
            .await
            .is_err());
        let wrong_config = context()
            .with_options_header_config(ConfigId(8))
            .with_options_header_inputs(inputs.clone())?;
        assert!(wrong_config
            .execute_inline_options_header(fixture.action.output_path())
            .await
            .is_err());
        let context = context()
            .with_options_header_config(ConfigId(7))
            .with_options_header_inputs(inputs)?;
        context
            .execute_inline_options_header(fixture.action.output_path())
            .await?;
        assert_eq!(
            std::fs::read(fixture.action.output_path())?,
            b"// @//:flag\n#define TF_OPTION_VALUE() true\n"
        );
        assert_eq!(
            *fixture.build.completed.lock().unwrap(),
            vec![fixture.action.action()]
        );
        let result = compiler
            .execute_pending_compile(fixture.compile.clone(), fixture.pending.clone())
            .await?;
        ensure!(
            result.exit_code == 0,
            "real dependent compiler failed: {:?}",
            result
        );
        assert!(!result.cached && result.output_records.is_empty());
        assert!(root.join("consumer.o").is_file());
        let processes = take_completed_diagnostic_processes(&root);
        assert_eq!(
            processes,
            vec![("CppCompile".to_owned(), fixture.pending.action())],
            "inline expansion must not invent a completed process"
        );
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inline_options_source_write_and_identity_failures_never_complete_producer(
    ) -> Result<()> {
        for failure in ["source", "write", "changed-source-after-render"] {
            let temp = tempfile::tempdir()?;
            let root = temp.path().canonicalize()?;
            let fixture = inline_options_fixture(&root)?;
            let previous = std::fs::read(fixture.action.output_path())?;
            if failure == "source" {
                std::fs::write(
                    &fixture.action.inputs().request.template_path,
                    "changed template\n",
                )?;
            } else {
                let template = fixture.action.inputs().request.template_path.clone();
                *fixture.build.inline_hook.lock().unwrap() = Some(Arc::new(move |stage| {
                    if stage == InlineOptionsStage::BeforePublish {
                        if failure == "write" {
                            anyhow::bail!("controlled inline writer failure");
                        }
                        std::fs::write(&template, "changed after rendering\n")?;
                    }
                    Ok(())
                }));
            }
            let error = fixture
                .build
                .manager_for(TargetId(1))
                .execute_inline_options_header(fixture.action.clone())
                .await
                .unwrap_err();
            let detail = format!("{error:#}");
            assert!(
                detail.contains(if failure == "write" {
                    "controlled inline writer failure"
                } else {
                    "SOURCE template changed"
                }),
                "{detail}"
            );
            assert!(fixture.build.completed.lock().unwrap().is_empty());
            assert_eq!(std::fs::read(fixture.action.output_path())?, previous);
            assert!(fixture
                .build
                .manager_for(TargetId(2))
                .execute_pending_compile(fixture.compile.clone(), fixture.pending.clone())
                .await
                .is_err());
            assert!(!root.join("consumer.o").exists());
            assert!(!std::fs::read_dir(root.join("genfiles"))?.any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".rush-inline-")));
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inline_options_caller_drop_holds_lease_until_blocking_writer_stops() -> Result<()> {
        for pause in [
            InlineOptionsStage::BeforePublish,
            InlineOptionsStage::AfterPublish,
        ] {
            let temp = tempfile::tempdir()?;
            let root = temp.path().canonicalize()?;
            let fixture = inline_options_fixture(&root)?;
            let previous = std::fs::read(fixture.action.output_path())?;
            let mut finished = fixture.build.inline_finished.subscribe();
            let completed = fixture.build.completed.clone();
            let weak_build = Arc::downgrade(&fixture.build);
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let entered_tx = std::sync::Mutex::new(Some(entered_tx));
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let release_rx = std::sync::Mutex::new(release_rx);
            *fixture.build.inline_hook.lock().unwrap() = Some(Arc::new(move |stage| {
                if stage == pause {
                    if let Some(tx) = entered_tx.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    release_rx
                        .lock()
                        .unwrap()
                        .recv_timeout(std::time::Duration::from_secs(10))
                        .map_err(|error| anyhow!("controlled writer release missing: {error}"))?;
                }
                Ok(())
            }));
            let manager = fixture.build.manager_for(TargetId(1));
            let mut call = Box::pin(manager.execute_inline_options_header(fixture.action.clone()));
            tokio::select! {
                result = &mut call => anyhow::bail!("writer returned before controlled boundary: {result:?}"),
                result = entered_rx => result?,
            }
            assert!(fixture.build.completed.lock().unwrap().is_empty());
            drop(call);
            assert!(
                *fixture.build.cancelled.borrow(),
                "caller Drop must signal existing build cancellation"
            );
            drop(manager);
            let output_path = fixture.action.output_path().to_owned();
            drop(fixture);
            // Only the owned worker may now retain build/lease authority.
            // The completion observer and acknowledgement retain neither.

            assert!(weak_build.upgrade().is_some());
            assert!(WorkspaceLease::acquire(&root, NativeCompileMode::Literal).is_err());
            release_tx.send(())?;
            tokio::time::timeout(std::time::Duration::from_secs(10), finished.changed()).await??;
            assert!(
                weak_build.upgrade().is_none(),
                "owned acknowledgement must follow the final build drop"
            );
            assert!(
                completed.lock().unwrap().is_empty(),
                "cancelled writer must never complete its producer"
            );
            let lease = WorkspaceLease::acquire(&root, NativeCompileMode::Literal)?;
            if pause == InlineOptionsStage::BeforePublish {
                assert_eq!(std::fs::read(&output_path)?, previous);
            } else {
                assert_eq!(
                    std::fs::read(&output_path)?,
                    b"// @//:flag\n#define TF_OPTION_VALUE() true\n"
                );
            }
            assert!(!root.join("consumer.o").exists());
            assert!(!std::fs::read_dir(root.join("genfiles"))?.any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".rush-inline-")));
            drop(lease);
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inline_options_cancel_while_final_registry_lock_waits_never_publishes_completion(
    ) -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().canonicalize()?;
        let fixture = inline_options_fixture(&root)?;
        let completed = fixture.build.completed.clone();
        let registry = fixture.build.registry.clone();
        let weak_build = Arc::downgrade(&fixture.build);
        let mut finished = fixture.build.inline_finished.subscribe();
        let (published_tx, published_rx) = tokio::sync::oneshot::channel();
        let published_tx = std::sync::Mutex::new(Some(published_tx));
        let (at_lock_tx, at_lock_rx) = tokio::sync::oneshot::channel();
        let at_lock_tx = std::sync::Mutex::new(Some(at_lock_tx));
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = std::sync::Mutex::new(release_rx);
        *fixture.build.inline_hook.lock().unwrap() = Some(Arc::new(move |stage| {
            if stage == InlineOptionsStage::AfterPublish {
                if let Some(tx) = published_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .map_err(|error| {
                        anyhow!("controlled final-lock writer release missing: {error}")
                    })?;
            } else if stage == InlineOptionsStage::BeforeCompletionLock {
                // This boundary is AFTER the old pre-await cancellation check.
                if let Some(tx) = at_lock_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
            }
            Ok(())
        }));
        let manager = fixture.build.manager_for(TargetId(1));
        let mut call = Box::pin(manager.execute_inline_options_header(fixture.action.clone()));
        tokio::select! {
            result = &mut call => anyhow::bail!("writer returned before publication control: {result:?}"),
            result = published_rx => result?,
        }
        assert_eq!(
            std::fs::read(fixture.action.output_path())?,
            b"// @//:flag\n#define TF_OPTION_VALUE() true\n"
        );
        let final_registry_lock = registry.lock().await;
        release_tx.send(())?;
        tokio::select! {
            result = &mut call => anyhow::bail!("writer bypassed held final registry lock: {result:?}"),
            result = at_lock_rx => result?,
        }
        assert!(completed.lock().unwrap().is_empty());
        drop(call);
        assert!(*fixture.build.cancelled.borrow());
        drop(manager);
        drop(fixture);
        assert!(
            weak_build.upgrade().is_some(),
            "owned worker must retain its lease while final validation waits"
        );
        assert!(WorkspaceLease::acquire(&root, NativeCompileMode::Literal).is_err());
        drop(final_registry_lock);
        tokio::time::timeout(std::time::Duration::from_secs(10), finished.changed()).await??;
        assert!(weak_build.upgrade().is_none());
        assert!(
            completed.lock().unwrap().is_empty(),
            "cancellation after the old pre-await check must prevent completion publication"
        );
        let _lease = WorkspaceLease::acquire(&root, NativeCompileMode::Literal)?;
        assert!(!root.join("consumer.o").exists());
        Ok(())
    }

}
