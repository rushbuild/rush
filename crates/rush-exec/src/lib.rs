// Rush Exec - Execution strategies
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Execution strategies for running actions.
//!
//! Provides pluggable execution backends: local, sandbox, remote, worker.

#![warn(missing_docs)]

pub mod local_action_runner;
pub mod local_isolation;
pub mod worker_pool;

use anyhow::Result;
use async_trait::async_trait;
pub use rush_action::{Action, ActionId, ActionResult, Artifact};
use std::path::PathBuf;
use std::time::Duration;

pub use worker_pool::WorkerPool;

/// Strategy for executing actions.
///
/// Different strategies provide different execution environments:
/// - Local: Direct execution on the host
/// - Sandbox: Hermetic execution in isolated environment
/// - Remote: Execution on remote build cluster
/// - Worker: Persistent process pool for fast tools
#[async_trait]
pub trait ExecutionStrategy: Send + Sync {
    /// Execute an action and return the result.
    async fn execute(&self, action: &Action) -> Result<ActionResult>;

    /// Check if this strategy can execute the given action.
    ///
    /// Some strategies may not support all actions (e.g., remote execution
    /// might not support network access).
    fn can_execute(&self, action: &Action) -> bool;

    /// Get a human-readable name for this strategy.
    fn name(&self) -> &str;
}

/// Apply per-action rlimits to a spawn-ready tokio Command before
/// `.spawn()` / `.output()`.
///
/// Honors `RUSH_COMPILE_MEMORY_CAP_MB`: if set to a positive integer and
/// the action is a compile (an explicit `CppCompile` resource class, or a
/// legacy mnemonic `CppCompile` or `CCompile`), each
/// spawned compiler gets `RLIMIT_AS` set to that number of megabytes.
/// A runaway translation unit then hits a clean OOM failure (non-zero
/// exit with `virtual memory exhausted` from g++'s allocator) instead of
/// silently swapping the entire host into a death spiral. On TF kernel
/// compiles we've observed single-TU peaks north of 15 GB; capping at
/// e.g. 8192 isolates those to their own action failure rather than
/// taking the whole build with them.
///
/// No-op on non-Linux, when the env var is unset / non-numeric, or for
/// non-compile mnemonics (link, archive, genrule, etc. have very
/// different profiles).
/// Apply per-action rlimits given a mnemonic string. Phase 5.C.4
/// added this signature so the LocalActionRunner adapter can apply
/// the same `RUSH_COMPILE_MEMORY_CAP_MB` policy without depending on
/// the legacy `Action` type. Mirrors the original
/// `apply_action_rlimits(cmd, &Action)` exactly when mnemonic is
/// `CppCompile` / `CCompile`.
#[cfg(target_os = "linux")]
pub(crate) fn apply_compile_memory_cap(
    cmd: &mut tokio::process::Command,
    mnemonic: &str,
    resource_class: Option<rush_resource::ResourceClass>,
) {
    if !uses_compile_memory_cap(mnemonic, resource_class) {
        return;
    }
    apply_compile_memory_cap_inner(cmd);
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn apply_compile_memory_cap(
    _cmd: &mut tokio::process::Command,
    _mnemonic: &str,
    _resource_class: Option<rush_resource::ResourceClass>,
) {
}

pub(crate) fn uses_compile_memory_cap(
    mnemonic: &str,
    resource_class: Option<rush_resource::ResourceClass>,
) -> bool {
    resource_class
        .map(|class| {
            matches!(
                class,
                rush_resource::ResourceClass::CppCompile
                    | rush_resource::ResourceClass::KernelCompile
            )
        })
        .unwrap_or_else(|| matches!(mnemonic, "CppCompile" | "CCompile"))
}

#[cfg(target_os = "linux")]
fn apply_compile_memory_cap_inner(cmd: &mut tokio::process::Command) {
    let cap_mb: u64 = match std::env::var("RUSH_COMPILE_MEMORY_CAP_MB")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        Some(n) if n > 0 => n,
        _ => return,
    };
    let cap_bytes: libc::rlim_t = cap_mb.saturating_mul(1024 * 1024);
    unsafe {
        cmd.pre_exec(move || {
            let rl = libc::rlimit {
                rlim_cur: cap_bytes,
                rlim_max: cap_bytes,
            };
            if libc::setrlimit(libc::RLIMIT_AS, &rl) != 0 {
                // Don't fail the exec — log to stderr so a tail of the
                // build output surfaces the issue, then let the compile
                // proceed uncapped rather than abort.
                let errno = *libc::__errno_location();
                let _ = libc::write(
                    2,
                    b"rush: setrlimit(RLIMIT_AS) failed; proceeding without cap\n"
                        as *const _ as *const libc::c_void,
                    62,
                );
                let _ = errno; // silence unused
            }
            Ok(())
        });
    }
}

/// Backwards-compatible wrapper around `apply_compile_memory_cap`
/// that pulls the mnemonic off `&Action`. Existing
/// `LocalStrategy::execute*` callers keep their shape; the new
/// `LocalActionRunner` adapter calls `apply_compile_memory_cap`
/// directly with a `&str`.
fn apply_action_rlimits(cmd: &mut tokio::process::Command, action: &Action) {
    apply_compile_memory_cap(cmd, &action.mnemonic, None);
}

/// Default ring-buffer cap per stream when `RUSH_ACTION_OUTPUT_RING_BYTES`
/// is unset or unparseable: 1 MiB.
const DEFAULT_OUTPUT_RING_BYTES: usize = 1 << 20;

pub(crate) fn output_ring_cap() -> usize {
    match std::env::var("RUSH_ACTION_OUTPUT_RING_BYTES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
    {
        Some(n) => n,
        None => DEFAULT_OUTPUT_RING_BYTES,
    }
}

/// Resolve the on-disk log directory for full action output.
///
/// Returns `Some(dir)` if `RUSH_ACTION_LOG_DIR` is set; the directory is
/// created lazily on first write. Returns `None` when unset, in which
/// case the bounded in-memory ring is the only retention (still safe —
/// nothing accumulates unboundedly).
pub(crate) fn output_log_dir() -> Option<PathBuf> {
    std::env::var_os("RUSH_ACTION_LOG_DIR").map(PathBuf::from)
}

async fn open_log_file(dir: &PathBuf, action_id: &ActionId, stream: &str) -> Option<tokio::fs::File> {
    open_log_file_for_id(dir, &action_id.to_string(), stream).await
}

/// Phase 5.C.4 LocalActionRunner companion: open a per-stream
/// log file using a free-form string identifier instead of the
/// content-addressed `ActionId`. Phase 5.C.4 actions don't have a
/// content hash yet (cache integration deferred), so the runner
/// uses `DeclaredActionId.0` as a stable string identifier.
pub(crate) async fn open_log_file_for_id(
    dir: &PathBuf,
    id: &str,
    stream: &str,
) -> Option<tokio::fs::File> {
    if let Err(_e) = tokio::fs::create_dir_all(dir).await {
        return None;
    }
    let path = dir.join(format!("{}.{}.log", id, stream));
    tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .await
        .ok()
}

/// Bounded capture of one stream (stdout or stderr) from a child process.
///
/// Holds at most `cap` bytes in memory. When the cap is exceeded, the
/// oldest bytes are dropped — the *tail* is what matters for diagnosing
/// failures, since compilers/tblgens print the actual error last.
///
/// All bytes are also written through to an optional on-disk log file
/// for full retention. The ring contents (with a header noting truncation
/// when applicable) are what we surface in `ActionResult.stdout/stderr`,
/// keeping per-action memory bounded regardless of how much a runaway
/// tool spews.
pub(crate) struct BoundedCapture {
    ring: Vec<u8>,
    cap: usize,
    total: u64,
    truncated: bool,
    log: Option<tokio::fs::File>,
}

impl BoundedCapture {
    pub(crate) fn new(cap: usize, log: Option<tokio::fs::File>) -> Self {
        Self {
            ring: Vec::with_capacity(cap.min(8192)),
            cap,
            total: 0,
            truncated: false,
            log,
        }
    }

    pub(crate) async fn append(&mut self, chunk: &[u8]) {
        self.total = self.total.saturating_add(chunk.len() as u64);
        let log_failed = if let Some(f) = self.log.as_mut() {
            use tokio::io::AsyncWriteExt;
            f.write_all(chunk).await.is_err()
        } else {
            false
        };
        if log_failed {
            self.log = None;
        }
        if self.cap == 0 {
            self.truncated = self.truncated || !chunk.is_empty();
            return;
        }
        if self.ring.len() + chunk.len() <= self.cap {
            self.ring.extend_from_slice(chunk);
        } else if chunk.len() >= self.cap {
            self.truncated = true;
            self.ring.clear();
            self.ring.extend_from_slice(&chunk[chunk.len() - self.cap..]);
        } else {
            self.truncated = true;
            let drop_n = self.ring.len() + chunk.len() - self.cap;
            self.ring.drain(..drop_n);
            self.ring.extend_from_slice(chunk);
        }
    }

    pub(crate) async fn finish(mut self, label: &str) -> Vec<u8> {
        let log_failed = if let Some(f) = self.log.as_mut() {
            use tokio::io::AsyncWriteExt;
            f.flush().await.is_err()
        } else {
            false
        };
        if log_failed {
            self.log = None;
        }
        if !self.truncated {
            return self.ring;
        }
        let location = if self.log.is_some() {
            " (full output in RUSH_ACTION_LOG_DIR)"
        } else {
            ""
        };
        let header = format!(
            "[rush] {} truncated: {} total bytes, kept last {}{}\n",
            label, self.total, self.cap, location
        );
        let mut out = header.into_bytes();
        out.extend_from_slice(&self.ring);
        out
    }
}

pub(crate) async fn drain_into<R>(mut reader: R, cap: &mut BoundedCapture) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        cap.append(&buf[..n]).await;
    }
}

/// Local execution strategy.
///
/// Executes actions directly on the host machine without isolation.
pub struct LocalStrategy;

impl LocalStrategy {
    /// Execute an action with real-time output streaming.
    ///
    /// This method spawns the process and streams stdout/stderr line-by-line
    /// to the provided callback, allowing real-time progress display.
    pub async fn execute_streaming<F>(
        &self,
        action: &Action,
        mut output_callback: F,
    ) -> Result<ActionResult>
    where
        F: FnMut(&str),
    {
        use std::process::Stdio;
        use tokio::io::{AsyncBufReadExt, BufReader};

        let execution_t0 = std::time::Instant::now();
        let mut cmd = tokio::process::Command::new(&action.command.executable);
        cmd.args(&action.command.args)
            .envs(&action.env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = &action.command.working_dir {
            cmd.current_dir(dir);
        }
        apply_action_rlimits(&mut cmd, action);
        let mut child = cmd.spawn()?;

        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        let mut stdout_reader = BufReader::new(stdout).lines();
        let mut stderr_reader = BufReader::new(stderr).lines();

        let cap = output_ring_cap();
        let log_dir = output_log_dir();
        let stdout_log = match &log_dir {
            Some(d) => open_log_file(d, &action.id, "stdout").await,
            None => None,
        };
        let stderr_log = match &log_dir {
            Some(d) => open_log_file(d, &action.id, "stderr").await,
            None => None,
        };
        let mut stdout_cap = BoundedCapture::new(cap, stdout_log);
        let mut stderr_cap = BoundedCapture::new(cap, stderr_log);
        let mut stdout_done = false;
        let mut stderr_done = false;

        // Read both stdout and stderr concurrently, surfacing lines via
        // the callback while accumulating into bounded ring buffers.
        while !stdout_done || !stderr_done {
            tokio::select! {
                line = stdout_reader.next_line(), if !stdout_done => {
                    match line? {
                        Some(line) => {
                            output_callback(&line);
                            stdout_cap.append(line.as_bytes()).await;
                            stdout_cap.append(b"\n").await;
                        }
                        None => stdout_done = true,
                    }
                }
                line = stderr_reader.next_line(), if !stderr_done => {
                    match line? {
                        Some(line) => {
                            output_callback(&line);
                            stderr_cap.append(line.as_bytes()).await;
                            stderr_cap.append(b"\n").await;
                        }
                        None => stderr_done = true,
                    }
                }
            }
        }

        let status = child.wait().await?;

        Ok(ActionResult {
            action_id: action.id.clone(),
            exit_code: status.code().unwrap_or(-1),
            stdout: stdout_cap.finish("stdout").await,
            stderr: stderr_cap.finish("stderr").await,
            outputs: action.outputs.clone(),
            output_records: Vec::new(),
            duration: execution_t0.elapsed(),
            cached: false,
        })
    }
}

#[async_trait]
impl ExecutionStrategy for LocalStrategy {
    async fn execute(&self, action: &Action) -> Result<ActionResult> {
        use std::process::Stdio;

        let execution_t0 = std::time::Instant::now();
        // Archive hermeticity (#39): `ar rcs <lib> <objs...>` REPLACES members
        // by name but never REMOVES members absent from the current obj list,
        // so a renamed object accumulates BOTH old- and new-named members →
        // duplicate symbols at link. Remove the archive output first so `ar`
        // produces a fresh archive containing exactly the current objects.
        // This runs ONLY here, immediately before spawning — i.e. only when the
        // legacy build path actually re-executes the archive (a cache MISS).
        // On a cache HIT the inputs are unchanged and RushActionManager returns
        // the cached result without reaching this strategy, so the existing
        // archive (whose obj set is identical) is left in place. Mirrors the
        // scheduler path's pre-remove in LocalActionRunner::execute.
        if action.mnemonic == "CppArchive" || action.mnemonic == "CcArchive" {
            for out in &action.outputs {
                if let Artifact::Source(p) = out {
                    let _ = std::fs::remove_file(p);
                }
            }
        }

        let mut cmd = tokio::process::Command::new(&action.command.executable);
        cmd.args(&action.command.args)
            .envs(&action.env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = &action.command.working_dir {
            cmd.current_dir(dir);
        }
        apply_action_rlimits(&mut cmd, action);
        let mut child = cmd.spawn()?;

        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        let cap = output_ring_cap();
        let log_dir = output_log_dir();
        let stdout_log = match &log_dir {
            Some(d) => open_log_file(d, &action.id, "stdout").await,
            None => None,
        };
        let stderr_log = match &log_dir {
            Some(d) => open_log_file(d, &action.id, "stderr").await,
            None => None,
        };
        let mut stdout_cap = BoundedCapture::new(cap, stdout_log);
        let mut stderr_cap = BoundedCapture::new(cap, stderr_log);

        // Drain both pipes concurrently with bounded retention. Without
        // this, a chatty failing tool (mlir-tblgen on a heavy .td, a
        // custom TF tblgen, or any compile that spirals into thousands
        // of error lines) would buffer its entire output in the kernel
        // pipe + our `Vec` and bloat us per-action by hundreds of MB.
        // Now memory is capped at `RUSH_ACTION_OUTPUT_RING_BYTES` (1 MiB
        // default) per stream regardless of how much the child spews.
        let (stdout_res, stderr_res) = tokio::join!(
            drain_into(stdout, &mut stdout_cap),
            drain_into(stderr, &mut stderr_cap),
        );
        stdout_res?;
        stderr_res?;

        let status = child.wait().await?;

        Ok(ActionResult {
            action_id: action.id.clone(),
            exit_code: status.code().unwrap_or(-1),
            stdout: stdout_cap.finish("stdout").await,
            stderr: stderr_cap.finish("stderr").await,
            outputs: action.outputs.clone(),
            output_records: Vec::new(),
            duration: execution_t0.elapsed(),
            cached: false,
        })
    }

    fn can_execute(&self, _action: &Action) -> bool {
        true // Local strategy can execute any action
    }

    fn name(&self) -> &str {
        "local"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bounded_capture_preserves_tail_when_truncated() {
        let mut cap = BoundedCapture::new(16, None);
        // Write 32 bytes; only the last 16 should survive.
        cap.append(b"AAAAAAAAAAAAAAAA").await; // 16
        cap.append(b"BBBBBBBBBBBBBBBB").await; // 16
        let out = cap.finish("stdout").await;
        // Tail of 16 'B' bytes plus the header marker
        assert!(out.starts_with(b"[rush] stdout truncated"));
        assert!(!String::from_utf8_lossy(&out).contains("RUSH_ACTION_LOG_DIR"));
        assert!(out.ends_with(b"BBBBBBBBBBBBBBBB"));
    }

    #[tokio::test]
    async fn bounded_capture_passthrough_when_under_cap() {
        let mut cap = BoundedCapture::new(64, None);
        cap.append(b"hello").await;
        cap.append(b" world").await;
        let out = cap.finish("stderr").await;
        assert_eq!(out, b"hello world");
    }

    #[tokio::test]
    async fn bounded_capture_handles_chunk_larger_than_cap() {
        let mut cap = BoundedCapture::new(8, None);
        cap.append(b"0123456789ABCDEF").await; // 16 bytes into 8-byte cap
        let out = cap.finish("stdout").await;
        assert!(out.starts_with(b"[rush] stdout truncated"));
        assert!(out.ends_with(b"89ABCDEF"));
    }
}
