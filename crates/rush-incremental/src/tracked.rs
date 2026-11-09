//! Context-scoped "current analysis IO" + fail-loud `tracked::*` free functions.
//!
//! # Why this exists (migration threading strategy)
//!
//! Analysis must read the world through [`TrackedIo`] so every filesystem/env
//! observation is recorded into a [`DependencyManifest`](crate::DependencyManifest).
//! Threading an explicit handle through ~595 read sites is infeasible, so instead
//! the analysis entry *installs* a shared `Arc<dyn TrackedIo + Send + Sync>` as the
//! thread-local "current analysis IO" via [`scope`], and migrated call sites call
//! the fail-loud free functions in this module (e.g. [`read_file`]) instead of
//! `std::fs::*` / `std::env::*`. Each free function delegates to
//! [`current_required`], which records into the installed IO.
//!
//! The thread-local is **lost** at concurrency boundaries that move work to a
//! fresh OS thread or task (`tokio::task::spawn_blocking`, `std::thread::spawn`,
//! `tokio::spawn`, parallel fetch workers). [`bind`] captures the current IO *now*
//! and returns a closure that re-installs it on the spawned thread, so the same
//! shared `Arc` keeps recording across the boundary. `buffer_unordered` /
//! `FuturesUnordered` need no help — they are polled on the current task.
//!
//! **Fail-loud:** the free functions NEVER fall back to raw `std::fs` / `std::env`.
//! If no IO is installed they panic, so an unmigrated/un-propagated parallel path
//! is caught in tests rather than silently serving an un-recorded (later stale)
//! graph.

use std::cell::RefCell;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;

use crate::tracked_io::{ProcessProbeResult, TrackedIo};

/// The shared analysis IO type installed for the current thread.
pub type SharedTrackedIo = Arc<dyn TrackedIo + Send + Sync>;

thread_local! {
    /// The currently-installed analysis IO for *this* thread, if any.
    static CURRENT: RefCell<Option<SharedTrackedIo>> = const { RefCell::new(None) };
}

/// RAII guard that restores the previously-installed IO when dropped, so a nested
/// [`scope`] restores its *caller's* IO (not `None`) and a panic in the body still
/// unwinds through the restore.
struct ScopeGuard {
    prev: Option<SharedTrackedIo>,
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        let prev = self.prev.take();
        CURRENT.with(|c| {
            *c.borrow_mut() = prev;
        });
    }
}

/// Install `io` as the current analysis IO for the duration of `f`, then restore
/// the previously-installed IO (if any) — even if `f` panics.
///
/// Nesting is supported: an inner `scope` restores the *outer* IO on exit, not
/// `None`. Returns `f`'s result.
pub fn scope<R>(io: SharedTrackedIo, f: impl FnOnce() -> R) -> R {
    let prev = CURRENT.with(|c| c.borrow_mut().replace(io));
    // The guard restores `prev` on drop, including across an unwinding panic.
    let _guard = ScopeGuard { prev };
    f()
}

/// Clone the currently-installed analysis IO, or `None` if no [`scope`] is active
/// on this thread.
pub fn current() -> Option<SharedTrackedIo> {
    CURRENT.with(|c| c.borrow().clone())
}

/// Clone the currently-installed analysis IO, panicking with a clear message if
/// none is installed.
///
/// This is the fail-loud guarantee backing every `tracked::*` free function: a
/// tracked read outside an active [`scope`] (an unmigrated / un-propagated
/// parallel path) panics rather than silently skipping the dependency record.
pub fn current_required() -> SharedTrackedIo {
    current().expect(
        "rush-incremental: no analysis TrackedIo installed for this thread — a \
         tracked read ran outside tracked::scope; an unmigrated/un-propagated \
         parallel path?",
    )
}

/// Install a real tracked-IO scope for the duration of `f`, for TESTS.
///
/// Every `tracked::*` read panics outside a [`scope`] — the fail-loud
/// guarantee that no analysis read escapes the dependency manifest. That
/// leaves any test calling into analysis code needing a scope, and building
/// one by hand requires a `RunRequest` and `CommandSemantics`, which most
/// crates do not depend on. Several test modules simply failed instead.
///
/// Recording goes to a real [`RealTrackedIo`] rooted at `root`; the manifest
/// is discarded when `f` returns.
#[doc(hidden)]
pub fn test_scope<R>(root: &std::path::Path, f: impl FnOnce() -> R) -> R {
    use bytes::Bytes;
    use rush_command_core::CommandSemantics;
    use rush_protocol::command::v1::RunRequest;
    use rush_protocol::{PROTOCOL_MAJOR, PROTOCOL_MINOR};

    let request = RunRequest {
        auth_cookie: Bytes::new(),
        argv: vec![
            Bytes::from_static(b"rush"),
            Bytes::from_static(b"build"),
            Bytes::from_static(b"//:test"),
        ],
        working_directory: Bytes::from(root.display().to_string()),
        environment: Vec::new(),
        block_for_lock: true,
        client_build_id: "tracked-test-scope".to_string(),
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: PROTOCOL_MINOR,
        workspace_config: Vec::new(),
    };
    let semantics =
        CommandSemantics::from_run_request(&request).expect("test semantics");
    let io: SharedTrackedIo =
        Arc::new(crate::tracked_io::RealTrackedIo::new(semantics, root));
    scope(io, f)
}

/// Capture the current analysis IO *now* and return a closure that re-installs it
/// (via [`scope`]) before running `f`.
///
/// Use this to carry the analysis IO across a concurrency boundary where the
/// thread-local would otherwise be empty:
///
/// ```ignore
/// tokio::task::spawn_blocking(tracked::bind(move || { /* tracked::* reads */ }));
/// std::thread::spawn(tracked::bind(move || { /* tracked::* reads */ }));
/// ```
///
/// Panics immediately (on the *current* thread, before spawning) if no IO is
/// installed — you are about to cross a boundary, so a missing context must fail
/// loud here rather than on the spawned thread. The returned closure shares the
/// same `Arc`, so reads on the spawned thread record into the same manifest.
///
/// This helper is intentionally tokio-free: it only wraps a closure.
pub fn bind<R>(f: impl FnOnce() -> R + Send + 'static) -> impl FnOnce() -> R + Send + 'static {
    let io = current_required();
    move || scope(io, f)
}

// ---------------------------------------------------------------------------
// Fail-loud free functions — one per `TrackedIo` method.
//
// Each delegates to `current_required()` so the read is recorded into the
// installed IO. Migrated call sites call these instead of `std::fs::*` /
// `std::env::*`. There is NO fallback to raw std::fs / std::env.
// ---------------------------------------------------------------------------

/// Read a file's full contents through the current analysis IO. See
/// [`TrackedIo::read_file`].
pub fn read_file(path: &Path) -> std::io::Result<Bytes> {
    current_required().read_file(path)
}

/// Read a file's contents as UTF-8 through the current analysis IO. See
/// [`TrackedIo::read_to_string`].
pub fn read_to_string(path: &Path) -> std::io::Result<String> {
    current_required().read_to_string(path)
}

/// Probe path existence through the current analysis IO. See
/// [`TrackedIo::exists`].
pub fn exists(path: &Path) -> bool {
    current_required().exists(path)
}

/// Probe path existence + type through the current analysis IO. See
/// [`TrackedIo::metadata_kind`].
pub fn metadata_kind(path: &Path) -> Option<crate::FileKind> {
    current_required().metadata_kind(path)
}

/// Probe a path's full stat metadata through the current analysis IO. See
/// [`TrackedIo::metadata_full`].
pub fn metadata_full(path: &Path) -> bool {
    current_required().metadata_full(path)
}

/// Read a symlink's target through the current analysis IO. See
/// [`TrackedIo::read_link`].
pub fn read_link(path: &Path) -> Option<PathBuf> {
    current_required().read_link(path)
}

/// Canonicalize `path` through the current analysis IO, resolving every symlink
/// hop via the tracked [`read_link`] so each resolved link records a
/// [`Dependency::Symlink`](crate::Dependency::Symlink). The result is the same
/// fully-resolved absolute path that `std::fs::canonicalize` would return.
///
/// # Why this exists
///
/// `std::fs::canonicalize` issues a `readlink(2)` on *every* path component to
/// resolve symlinks, but those probes are invisible to the tracked-IO manifest —
/// a symlink swap somewhere on a canonicalized analysis path (e.g. a `.bzl`
/// resolution or an external-repo root) could let a daemon reuse a stale graph.
/// This decomposes canonicalization into per-component tracked probes
/// ([`metadata_kind`] for existence/type, [`read_link`] for the link target) so
/// every hop becomes a recorded dependency. The *resolved path logic is
/// identical* to `fs::canonicalize`: components are walked left-to-right, `.` is
/// dropped, `..` pops the previous resolved component, and symlink targets are
/// spliced into the remaining work (absolute targets reset the result to root).
///
/// Returns `Err` (mirroring `fs::canonicalize`) when a component does not exist
/// or a symlink loop is detected, so existing `.unwrap_or(original)` call sites
/// keep their fallback behavior. A relative `path` is joined onto the process
/// cwd first, exactly as `fs::canonicalize` does.
///
/// Panics (fail-loud) if no analysis IO is installed on this thread, like every
/// other `tracked::*` free function.
pub fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    use crate::FileKind;
    use std::path::Component;

    let tracker = current_required();

    // Make absolute against the process cwd, matching fs::canonicalize.
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    if let Some(resolved) = tracker.canonicalize_cache_get(&abs) {
        return Ok(resolved);
    }

    // An owned representation of a single path component so the work list can
    // hold spliced symlink-target components without borrowing a `PathBuf` that
    // is dropped each iteration.
    enum OwnedComp {
        Prefix(OsString),
        RootDir,
        CurDir,
        ParentDir,
        Normal(OsString),
    }
    fn to_owned(comp: Component) -> OwnedComp {
        match comp {
            Component::Prefix(p) => OwnedComp::Prefix(p.as_os_str().to_os_string()),
            Component::RootDir => OwnedComp::RootDir,
            Component::CurDir => OwnedComp::CurDir,
            Component::ParentDir => OwnedComp::ParentDir,
            Component::Normal(n) => OwnedComp::Normal(n.to_os_string()),
        }
    }

    // Remaining components to process, in reverse so we can pop from the back
    // cheaply; a resolved symlink target is pushed back here to be re-walked.
    let mut pending: Vec<OwnedComp> = abs.components().rev().map(to_owned).collect();
    let mut result = PathBuf::new();
    // Bound symlink resolution to detect loops (ELOOP), like the kernel's limit.
    let mut hops = 0usize;
    const MAX_HOPS: usize = 64;

    while let Some(comp) = pending.pop() {
        match comp {
            OwnedComp::Prefix(p) => result.push(&p),
            OwnedComp::RootDir => {
                result.clear();
                result.push(Component::RootDir.as_os_str());
            }
            OwnedComp::CurDir => {}
            OwnedComp::ParentDir => {
                result.pop();
            }
            OwnedComp::Normal(name) => {
                let candidate = result.join(name);
                // Probe the component's kind (records a FileExistence dep). A
                // symlink reports `Symlink` because the tracked metadata uses
                // symlink_metadata (does NOT follow links).
                match metadata_kind(&candidate) {
                    Some(FileKind::Symlink) => {
                        hops += 1;
                        if hops > MAX_HOPS {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::Other,
                                format!(
                                    "too many symbolic links while canonicalizing {}",
                                    path.display()
                                ),
                            ));
                        }
                        // Record the link target (Symlink dep) and splice it in.
                        let target = read_link(&candidate).ok_or_else(|| {
                            std::io::Error::new(
                                std::io::ErrorKind::Other,
                                format!("failed to read symlink {}", candidate.display()),
                            )
                        })?;
                        if target.is_absolute() {
                            // Absolute target: restart resolution from root.
                            result.clear();
                        }
                        // Relative target resolves relative to the link's parent
                        // (the current `result`); absolute target was just reset
                        // to root above. Either way, splice the target's
                        // components into the remaining work, in reverse.
                        for c in target.components().rev() {
                            pending.push(to_owned(c));
                        }
                    }
                    Some(_) => {
                        // A regular file or directory: accept the component.
                        result = candidate;
                    }
                    None => {
                        // Component does not exist: fs::canonicalize would fail.
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::NotFound,
                            format!("no such file or directory: {}", candidate.display()),
                        ));
                    }
                }
            }
        }
    }

    tracker.canonicalize_cache_put(abs, result.clone());
    Ok(result)
}

/// List a directory's entry names through the current analysis IO. See
/// [`TrackedIo::read_dir_names`].
pub fn read_dir_names(path: &Path) -> std::io::Result<Vec<OsString>> {
    current_required().read_dir_names(path)
}

/// Read an environment variable through the current analysis IO. See
/// [`TrackedIo::getenv`].
pub fn getenv(name: &[u8]) -> Option<Bytes> {
    current_required().getenv(name)
}

/// Return the captured client environment through the current analysis IO. See
/// [`TrackedIo::command_environment`]. No process-environment fallback is used.
pub fn command_environment() -> std::io::Result<Vec<(Vec<u8>, Vec<u8>)>> {
    current_required().command_environment()
}

/// Read an environment variable as UTF-8 through the current analysis IO,
/// recording the same `Env` dependency as [`getenv`]. Returns `None` if the
/// variable is unset *or* its value is not valid UTF-8 — mirroring
/// `std::env::var(..).ok()` so a migrated `env::var(k).as_deref() == Ok("1")`
/// check becomes `getenv_str(k).as_deref() == Some("1")` with identical
/// semantics. The dependency is still recorded for the unset/non-UTF-8 cases
/// (the read happened), so a later change to the variable invalidates the graph.
pub fn getenv_str(name: &str) -> Option<String> {
    getenv(name.as_bytes()).and_then(|b| String::from_utf8(b.to_vec()).ok())
}

/// Read a build option through the current analysis IO. See
/// [`TrackedIo::get_option`].
pub fn get_option(key: &str) -> Option<String> {
    current_required().get_option(key)
}

/// Run a subprocess probe through the current analysis IO. See
/// [`TrackedIo::probe_process`].
pub fn probe_process(
    program: &Path,
    args: &[Vec<u8>],
    env_keys: &[Vec<u8>],
    cwd: &Path,
) -> std::io::Result<ProcessProbeResult> {
    current_required().probe_process(program, args, env_keys, cwd)
}

/// Run a tracked subprocess probe whose semantic result is status + stdout.
/// Stderr remains available to the caller for failure diagnostics, but is not
/// compared during freshness validation.
pub fn probe_process_stdout(
    program: &Path,
    args: &[Vec<u8>],
    env_keys: &[Vec<u8>],
    cwd: &Path,
) -> std::io::Result<ProcessProbeResult> {
    current_required().probe_process_stdout(program, args, env_keys, cwd)
}

/// Consume the per-command configuration fingerprint through the current analysis
/// IO. See [`TrackedIo::config_fingerprint`].
pub fn config_fingerprint() -> [u8; 32] {
    current_required().config_fingerprint()
}

// ---------------------------------------------------------------------------
// Capture-since-mark + replay — the daemon `.bzl`/BUILD cache dep-set primitives.
//
// Unlike the read free functions above these are NOT fail-loud: they are called
// around cache load/hit boundaries that also run outside a command scope (setup,
// unit tests, analysis entered other than via a command). A mark/capture/replay
// outside a command scope is harmless — there is no manifest to record into — so
// these return `None` / empty / no-op instead of panicking.
// ---------------------------------------------------------------------------

/// Capture a mark (the current recorded-dependency length) on the currently
/// installed analysis IO, or `None` if no [`scope`] is active.
///
/// Pass the returned mark to [`captured_since`] *after* a sub-computation to read
/// back exactly the dependencies it recorded. See
/// [`TrackedIo::mark`](crate::TrackedIo::mark).
pub fn mark() -> Option<usize> {
    current().map(|io| io.mark())
}

/// Clone the dependencies recorded since `mark` on the currently installed
/// analysis IO, or an empty vec if no [`scope`] is active. See
/// [`TrackedIo::captured_since`](crate::TrackedIo::captured_since).
pub fn captured_since(mark: usize) -> Vec<crate::Dependency> {
    match current() {
        Some(io) => io.captured_since(mark),
        None => Vec::new(),
    }
}

/// Replay a pre-recorded dependency set into the currently installed analysis IO
/// (the cache-HIT primitive), or no-op if no [`scope`] is active.
///
/// Re-injecting a cached `.bzl`/BUILD entry's dependency set makes the current
/// command's manifest complete as if the load had happened, even though the value
/// came from the process-global cache without re-recording. Replaying outside a
/// command scope is harmless (nothing to record into). Duplicates are deduped at
/// manifest snapshot time. See
/// [`TrackedIo::record_deps`](crate::TrackedIo::record_deps).
pub fn replay_deps(deps: &[crate::Dependency]) {
    if let Some(io) = current() {
        io.record_deps(deps);
    }
}
