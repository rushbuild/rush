//! Shared metadata + subprocess-probe primitives.
//!
//! Both the recording side ([`crate::tracked_io::RealTrackedIo`]) and the
//! validation side ([`crate::DependencyManifest::validate`]) compute file
//! metadata and run subprocess probes through these helpers, so a re-stat / re-run
//! during validation produces values directly comparable to what was recorded.

use std::io::Read;
use std::path::Path;
use std::process::Command;

use rush_command_core::CommandSemantics;

use crate::dependency::process_stream_digest;

/// A comparable snapshot of a path's stat metadata.
///
/// Unix-only fields (`mode`, `dev`, `ino`, `ctime_ns`) are 0 on non-unix targets.
/// An absent path is `present == false` with every numeric field 0, so a path
/// observed-absent that later appears (or vice versa) compares unequal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MetaSnapshot {
    pub present: bool,
    pub len: u64,
    pub mode: u32,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
    pub dev: u64,
    pub ino: u64,
}

impl MetaSnapshot {
    fn absent() -> Self {
        MetaSnapshot {
            present: false,
            len: 0,
            mode: 0,
            mtime_ns: 0,
            ctime_ns: 0,
            dev: 0,
            ino: 0,
        }
    }
}

/// Stat a path (without following the final symlink) into a [`MetaSnapshot`].
///
/// `symlink_metadata` is used so a `chmod`/swap on the path node itself is seen
/// rather than its target — symlink identity matters for tool resolution.
pub(crate) fn stat_snapshot(path: &Path) -> MetaSnapshot {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => snapshot_from_meta(&meta),
        Err(_) => MetaSnapshot::absent(),
    }
}

#[cfg(unix)]
fn snapshot_from_meta(meta: &std::fs::Metadata) -> MetaSnapshot {
    use std::os::unix::fs::MetadataExt;
    MetaSnapshot {
        present: true,
        len: meta.len(),
        mode: meta.mode(),
        // mtime/ctime in whole seconds * 1e9 + nanos; both are i64-derived, widen
        // to i128 so the multiply can't overflow.
        mtime_ns: (meta.mtime() as i128) * 1_000_000_000 + (meta.mtime_nsec() as i128),
        ctime_ns: (meta.ctime() as i128) * 1_000_000_000 + (meta.ctime_nsec() as i128),
        dev: meta.dev(),
        ino: meta.ino(),
    }
}

#[cfg(not(unix))]
fn snapshot_from_meta(meta: &std::fs::Metadata) -> MetaSnapshot {
    use std::time::UNIX_EPOCH;
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0);
    MetaSnapshot {
        present: true,
        len: meta.len(),
        mode: 0,
        mtime_ns,
        ctime_ns: 0,
        dev: 0,
        ino: 0,
    }
}

/// A public, comparable snapshot of a path's stat metadata, for callers outside
/// this crate (e.g. the build orchestrator's persistent action-freshness
/// ledger). Carries the same fields as the internal [`MetaSnapshot`] and is
/// produced by the same `symlink_metadata`-based stat, so two observations of an
/// unchanged path compare `==`.
///
/// This is deliberately the SAME metadata universe the manifest's `FileMetadata`
/// dependency uses (len/mode/mtime_ns/ctime_ns/dev/ino + presence), so a
/// freshness ledger built from it is a faithful, equally-strong replay of the
/// action layer's input/output freshness proof — not a weaker one. A metadata-
/// only comparison treats `touch` (mtime change, identical content) as unequal;
/// higher layers may add a separate content proof for bounded regular files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileMeta {
    pub present: bool,
    pub len: u64,
    pub mode: u32,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
    pub dev: u64,
    pub ino: u64,
}

/// Stat a path (without following the final symlink) into a comparable
/// [`FileMeta`]. An absent path returns `present == false` with zeroed fields,
/// so an output that was present at record time and later deleted compares
/// unequal (forces a rebuild).
pub fn stat_meta(path: &Path) -> FileMeta {
    let s = stat_snapshot(path);
    FileMeta {
        present: s.present,
        len: s.len,
        mode: s.mode,
        mtime_ns: s.mtime_ns,
        ctime_ns: s.ctime_ns,
        dev: s.dev,
        ino: s.ino,
    }
}

/// Content digest of a REGULAR file at `path`, for content-change pruning in
/// the freshness ledger (commit: content-change pruning).
///
/// Returns `Some(blake3-content-digest)` only when `path` is a present, regular
/// file (not a symlink, dir, or special file — checked via no-follow `lstat`)
/// whose length is `<= max_len`. Returns `None` for an absent/unreadable file,
/// a non-regular file, or a file larger than `max_len`. The `None` cases are
/// the CONSERVATIVE answer: the caller treats "no usable digest" as "cannot
/// prove content-unchanged" and falls back to the metadata-MISS rebuild, so a
/// missing digest can never serve a stale build. The size cap keeps the
/// pruning cost bounded — huge `.a`/`.so` outputs stay metadata-only (a touch
/// on them is rare and conservatively rebuilds).
pub fn content_digest_of_regular_file(path: &Path, max_len: u64) -> Option<[u8; 32]> {
    // No-follow metadata so a symlink is never silently content-pruned to its
    // target's bytes (matches the ledger's lstat-based `stat_meta`).
    let md = std::fs::symlink_metadata(path).ok()?;
    if !md.is_file() {
        return None;
    }
    if md.len() > max_len {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let limit = max_len.checked_add(1)?;
    let mut bytes = Vec::new();
    let mut limited = file.take(limit);
    limited.read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > max_len {
        return None;
    }
    Some(crate::dependency::content_digest(&bytes))
}

/// The recorded/observed result of a subprocess probe.
pub(crate) struct ProbeOutcome {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_digest: [u8; 32],
    pub stderr_digest: [u8; 32],
}

/// Run a subprocess probe deterministically and capture its outcome.
///
/// The child is run with a *cleared* environment, then only `env_keys` (resolved
/// from the passed [`CommandSemantics`], never `std::env`) are re-injected — so
/// the probe depends on exactly the env the manifest declared, and validation
/// re-runs under an identical environment. `args` and env values are raw bytes
/// (paths/values are not guaranteed UTF-8); on unix they are passed losslessly.
///
/// The exit status is normalized to an `i32`: the real exit code when the process
/// exited normally, or `-(signal)` when it was killed by a signal (unix), or `-1`
/// when neither is available.
pub(crate) fn run_probe(
    program: &Path,
    args: &[Vec<u8>],
    env_keys: &[Vec<u8>],
    cwd: &Path,
    sem: &CommandSemantics,
) -> std::io::Result<ProbeOutcome> {
    let mut cmd = Command::new(program);
    cmd.current_dir(cwd);
    cmd.env_clear();
    for key in env_keys {
        if let Some(value) = sem.getenv(key) {
            cmd.env(os_from_bytes(key), os_from_bytes(value));
        }
    }
    for arg in args {
        cmd.arg(os_from_bytes(arg));
    }
    let output = cmd.output()?;
    Ok(ProbeOutcome {
        status: normalize_status(&output.status),
        stdout_digest: process_stream_digest(&output.stdout),
        stderr_digest: process_stream_digest(&output.stderr),
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

#[cfg(unix)]
fn normalize_status(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    if let Some(code) = status.code() {
        code
    } else if let Some(sig) = status.signal() {
        -sig
    } else {
        -1
    }
}

#[cfg(not(unix))]
fn normalize_status(status: &std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(-1)
}

/// Build an `OsString` from raw bytes losslessly on unix; lossy elsewhere.
fn os_from_bytes(bytes: &[u8]) -> std::ffi::OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        std::ffi::OsStr::from_bytes(bytes).to_os_string()
    }
    #[cfg(not(unix))]
    {
        std::ffi::OsString::from(String::from_utf8_lossy(bytes).into_owned())
    }
}
