// Rush Exec - Local action runner (phase 5.C.4.2)
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Phase 5.C.4.2 LocalActionRunner adapter.
//!
//! Phase 5.C.4 step 4 proper landed scheduler dispatch via direct
//! `tokio::process::Command` spawn (commit 3d9f4cc). ChatGPT Pro's
//! review flagged that bypassing the legacy execution substrate at
//! TF scale would "launch the compiler cavalry and hope Linux
//! forgives us" — the original 45 GB → 2.6 GiB memory cliff exists
//! because rush already coordinates child-process memory via
//! `RLIMIT_AS` caps, PSI throttling, and bounded stdout/stderr
//! retention. Direct spawn skips all of that.
//!
//! [`LocalActionRunner`] is the seam scheduler dispatch routes
//! through. The trait gives a fake test runner (no real subprocess
//! needed for unit tests) and leaves room for future remote /
//! sandbox impls. [`DefaultLocalActionRunner`] is the production
//! impl: applies the same `apply_compile_memory_cap` /
//! `BoundedCapture` / `output_log_dir` machinery `LocalStrategy` uses,
//! just keyed off [`ExecutionRequest`] (which carries scheduler-
//! native types from `rush_action::ids`) instead of the legacy
//! [`crate::Action`] type.
//!
//! ## What this is NOT (yet)
//!
//! - Not yet integrated with the broader `ResourceManager` job-permit
//!   /  PSI throttling that lives in `rush-build`. The runner takes a
//!   trait so a future commit can wrap it with permit-acquisition
//!   without changing scheduler dispatch.
//! - Not action-key/cache aware. Cache integration (OKR 4) lands
//!   separately and will introduce a canonical `ActionKeySpec` that
//!   both the legacy `Action` and `ExecutionRequest` paths adapt
//!   into.
//! - Not timeout-enforced. `ExecutionRequest.timeout` is honored
//!   via `tokio::time::timeout`; setting it to `None` means no
//!   wall-clock cap.

use crate::local_isolation::{prepare as prepare_isolation, LocalExecutionIsolation};
use anyhow::{Context, Result};
use async_trait::async_trait;
use rush_action::external_action_key::{ExternalCredentialState, ExternalSignalState};
use rush_action::ids::{
    ActionEnvPolicy, ArtifactId, DeclaredActionId, MaterializedCommand, TargetId,
};
use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "linux")]
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// One unit of work the scheduler asks the runner to execute.
///
/// Carries everything the runner needs without reaching back into
/// the registry — materialized in advance by the dispatcher so the
/// runner can spawn without holding any registry lock.
#[derive(Clone, Debug)]
pub struct ExecutionRequest {
    /// The declared-action handle. Used as a stable string ID for
    /// log filenames since phase 5.C.4 actions don't have a content
    /// hash yet.
    pub declared_action_id: DeclaredActionId,
    /// Action mnemonic (`CppCompile`, `CcLink`, `MlirTblgen`, …).
    /// This is the fallback for resource and compile-cap classification
    /// when `resource_class` is absent.
    pub mnemonic: String,
    /// Explicit resource class supplied by a graph importer. When
    /// present, permit acquisition uses this class instead of
    /// inferring one from `mnemonic`.
    pub resource_class: Option<rush_resource::ResourceClass>,
    /// Target this action belongs to. Surfaces in error messages
    /// so a failure points back at the rule label.
    pub owner_target: TargetId,
    /// Resolved executable + argv + env + working_dir. Materialized
    /// from `ExecutableAction` by the dispatcher.
    pub command: MaterializedCommand,
    /// Declared input artifacts. Carried for future
    /// pre-execution input validation (phase 5.C.5 territory) and
    /// for cache-key derivation.
    pub declared_inputs: Vec<ArtifactId>,
    /// Declared output artifacts. The dispatcher uses these to
    /// synthesize the target's NestedSet after the runner returns.
    pub declared_outputs: Vec<ArtifactId>,
    /// Wall-clock cap. `None` means no timeout (the OS or
    /// downstream resource controls handle it).
    pub timeout: Option<Duration>,
    /// Optional zero-based command index appended to action log filenames.
    /// Multi-command imported actions use this to retain every command's
    /// stream instead of truncating the preceding command log.
    pub log_command_index: Option<u32>,
    /// Remove the output named by `CcArchive` argv before spawning.
    /// Existing native Rush actions enable this repair; exact
    /// imported commands can disable it to avoid hidden mutations.
    pub remove_stale_archive_before_execute: bool,
    /// Start a fresh process group and terminate it if execution is cancelled.
    /// External compound-shell actions enable this so child compilers do not
    /// survive Rush interruption; existing native actions preserve prior
    /// direct-child behavior.
    pub terminate_process_group_on_drop: bool,
    /// Connect the action's standard input to the null device. External replay
    /// enables this so actions cannot observe Rush's terminal or caller input.
    pub stdin_null: bool,
    /// Exact Linux signal state to install before exec. `None` preserves the
    /// native action runner's historical inheritance behavior.
    pub signal_state: Option<ExternalSignalState>,
    /// Exact Linux file-creation mask to install before exec. `None` preserves
    /// the native action runner's historical inheritance behavior.
    pub file_creation_mask: Option<u32>,
    /// Exact Linux credential state required before exec. `None` preserves the
    /// native action runner's historical inheritance behavior.
    pub credentials: Option<ExternalCredentialState>,
    /// Optional fail-closed local filesystem and namespace boundary.
    pub isolation: Option<LocalExecutionIsolation>,
}

/// Result of one action execution. Same shape as
/// [`crate::ActionResult`] minus the legacy `cached` /
/// `outputs: Vec<Artifact>` fields — those are dispatcher-side
/// concerns now.
#[derive(Debug)]
pub struct ActionExecutionResult {
    pub declared_action_id: DeclaredActionId,
    pub exit_code: i32,
    /// Stdout tail, bounded by [`crate::output_ring_cap`]. The
    /// full stream lands on disk when `RUSH_ACTION_LOG_DIR` is
    /// set.
    pub stdout_tail: Vec<u8>,
    /// Stderr tail, bounded same way.
    pub stderr_tail: Vec<u8>,
    pub duration: Duration,
    /// Peak sampled resident memory for the action process tree on Linux.
    /// Zero when resource tracing is disabled or unavailable.
    pub peak_rss_kb: u64,
}

/// One complete directory value observed while an action was executing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeDirectoryObservation {
    /// Absolute descriptor target observed by the process supervisor.
    pub path: PathBuf,
    /// Sorted member names excluding `.` and `..`.
    pub members: Vec<Vec<u8>>,
}

/// Normal action result paired with runtime directory observations.
#[derive(Debug)]
pub struct ObservedActionExecutionResult {
    /// Standard local action execution result.
    pub execution: ActionExecutionResult,
    /// Complete directory enumerations observed across the process tree.
    pub directory_observations: Vec<RuntimeDirectoryObservation>,
}

fn resource_trace_path() -> Option<PathBuf> {
    std::env::var_os("RUSH_RESOURCE_TRACE")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

static RESOURCE_TRACE_FILES: OnceLock<Mutex<HashMap<PathBuf, File>>> = OnceLock::new();

fn append_resource_trace(path: &Path, line: &str) {
    let files = RESOURCE_TRACE_FILES.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut files) = files.lock() else {
        return;
    };
    if !files.contains_key(path) {
        let Ok(file) = OpenOptions::new().create(true).append(true).open(path) else {
            return;
        };
        files.insert(path.to_path_buf(), file);
    }
    if let Some(file) = files.get_mut(path) {
        let _ = writeln!(file, "{}", line);
    }
}

#[cfg(target_os = "linux")]
fn process_tree_rss_kb(root_pid: u32) -> Option<u64> {
    let mut stack = vec![root_pid];
    let mut seen = HashSet::new();
    let mut total = 0u64;
    let mut root_seen = false;
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        let status = match std::fs::read_to_string(format!("/proc/{pid}/status")) {
            Ok(status) => status,
            Err(_) if pid == root_pid => return None,
            Err(_) => continue,
        };
        root_seen |= pid == root_pid;
        if let Some(kb) = status.lines().find_map(|line| {
            line.strip_prefix("VmRSS:")
                .and_then(|value| value.split_whitespace().next())
                .and_then(|value| value.parse::<u64>().ok())
        }) {
            total = total.saturating_add(kb);
        }
        let children_path = format!("/proc/{pid}/task/{pid}/children");
        if let Ok(children) = std::fs::read_to_string(children_path) {
            stack.extend(
                children
                    .split_whitespace()
                    .filter_map(|child| child.parse::<u32>().ok()),
            );
        }
    }
    root_seen.then_some(total)
}

#[cfg(target_os = "linux")]
struct RssMonitor {
    stop: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<u64>,
}

#[cfg(target_os = "linux")]
fn start_rss_monitor(pid: Option<u32>, enabled: bool) -> Option<RssMonitor> {
    let pid = pid.filter(|_| enabled)?;
    let stop = Arc::new(AtomicBool::new(false));
    let monitor_stop = Arc::clone(&stop);
    let task = tokio::spawn(async move {
        let mut peak = 0u64;
        while !monitor_stop.load(Ordering::Relaxed) {
            match process_tree_rss_kb(pid) {
                Some(rss) => peak = peak.max(rss),
                None => return peak,
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        peak
    });
    Some(RssMonitor { stop, task })
}

#[cfg(not(target_os = "linux"))]
struct RssMonitor {
    task: tokio::task::JoinHandle<u64>,
}

#[cfg(not(target_os = "linux"))]
fn start_rss_monitor(_pid: Option<u32>, _enabled: bool) -> Option<RssMonitor> {
    None
}

/// Trait scheduler dispatch routes through. Production code uses
/// [`DefaultLocalActionRunner`]; tests use [`FakeLocalActionRunner`]
/// (or any other mock).
#[async_trait]
pub trait LocalActionRunner: Send + Sync {
    async fn execute(&self, request: ExecutionRequest) -> Result<ActionExecutionResult>;

    /// Execute with process-tree observation, failing when unsupported.
    async fn execute_observed(
        &self,
        _request: ExecutionRequest,
    ) -> Result<ObservedActionExecutionResult> {
        anyhow::bail!("observed local action execution is unsupported")
    }
}

/// Production [`LocalActionRunner`] — applies the same compile-cap
/// rlimit, bounded-output-capture, and on-disk-log policy that
/// [`crate::LocalStrategy`] uses for the legacy `Action` path.
pub struct DefaultLocalActionRunner;

impl Default for DefaultLocalActionRunner {
    fn default() -> Self {
        Self
    }
}

#[async_trait]
impl LocalActionRunner for DefaultLocalActionRunner {
    async fn execute(&self, request: ExecutionRequest) -> Result<ActionExecutionResult> {
        use crate::{
            apply_compile_memory_cap, drain_into, open_log_file_for_id, output_log_dir,
            output_ring_cap, BoundedCapture,
        };
        use std::process::Stdio;

        let start = Instant::now();

        let mut cmd = tokio::process::Command::new(&request.command.executable);
        cmd.args(&request.command.argv)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if request.stdin_null {
            cmd.stdin(Stdio::null());
        }
        if let Some(argv0) = &request.command.argv0 {
            #[cfg(unix)]
            cmd.arg0(argv0);
            #[cfg(not(unix))]
            anyhow::bail!("explicit argv[0] is unsupported on this platform");
        }
        // Phase 5.C.4 commit 66: honor the action's
        // `ActionEnvPolicy`. `tokio::process::Command::new`
        // inherits the parent env by default; for
        // `ExplicitOnly` we MUST clear before applying
        // declared vars so the action sees only what was
        // declared. Pro's directive: "the action's
        // contract should be either 'inherit + declared'
        // or 'declared only'."
        match request.command.env_policy {
            ActionEnvPolicy::ExplicitOnly => {
                cmd.env_clear();
            }
            ActionEnvPolicy::InheritHost => {}
        }
        for (k, v) in &request.command.env {
            cmd.env(k, v);
        }
        if let Some(d) = &request.command.working_dir {
            cmd.current_dir(d);
        }
        if request.terminate_process_group_on_drop {
            cmd.kill_on_drop(true);
            #[cfg(unix)]
            cmd.process_group(0);
        }
        // Apply compile-cap rlimit. Same policy as
        // LocalStrategy::execute — `RUSH_COMPILE_MEMORY_CAP_MB`
        // env var, keyed by an explicit class when one is present and
        // otherwise by the legacy CppCompile / CCompile mnemonic.
        apply_compile_memory_cap(&mut cmd, &request.mnemonic, request.resource_class);
        if request.isolation.is_some() && request.remove_stale_archive_before_execute {
            anyhow::bail!("isolated execution cannot perform parent-side archive cleanup");
        }
        let isolation = request
            .isolation
            .as_ref()
            .map(|policy| {
                prepare_isolation(
                    policy,
                    request.command.working_dir.as_deref(),
                    request.credentials.as_ref(),
                )
            })
            .transpose()?;
        apply_external_process_state(
            &mut cmd,
            request.signal_state,
            request.file_creation_mask,
            request.credentials,
        )?;
        if let Some(isolation) = &isolation {
            isolation.install(&mut cmd);
        }

        // CcArchive hermeticity (#39): `ar rcs <lib> <objs...>` REPLACES
        // members by name but never REMOVES members absent from the
        // current obj list. After an object-naming change (e.g. the
        // cc_obj_key transition) an incrementally-rebuilt archive
        // accumulates BOTH the old- and new-named object → duplicate
        // symbols at link ("multiple definition of main" in
        // libtest_main.a). Remove the archive output first so `ar`
        // produces a fresh archive containing exactly the current
        // objects. The output is argv[1] by CcArchiveCommandSpec /
        // declare_action construction (["rcs", <lib>, <objs>...]); argv
        // itself is unchanged, so PASS-B verifier parity is preserved.
        if request.remove_stale_archive_before_execute && request.mnemonic == "CcArchive" {
            if let Some(lib) = request.command.argv.get(1) {
                let _ = std::fs::remove_file(lib);
            }
        }

        let mut child = cmd.spawn()?;
        let rss_monitor = start_rss_monitor(child.id(), resource_trace_path().is_some());
        #[cfg(unix)]
        let mut process_group_guard =
            ProcessGroupKillGuard::new(request.terminate_process_group_on_drop, child.id());
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");

        let cap = output_ring_cap();
        let log_dir = output_log_dir();
        let id_str = match request.log_command_index {
            Some(index) => format!("{:?}.command-{:04}", request.declared_action_id, index),
            None => format!("{:?}", request.declared_action_id),
        };
        let stdout_log = match &log_dir {
            Some(d) => open_log_file_for_id(d, &id_str, "stdout").await,
            None => None,
        };
        let stderr_log = match &log_dir {
            Some(d) => open_log_file_for_id(d, &id_str, "stderr").await,
            None => None,
        };
        let mut stdout_cap = BoundedCapture::new(cap, stdout_log);
        let mut stderr_cap = BoundedCapture::new(cap, stderr_log);

        // Drain pipes concurrently so a chatty failing tool's
        // stderr doesn't block on a full kernel pipe buffer while
        // we're waiting on stdout. Bounded retention by the ring
        // means this is memory-safe even if the tool spews
        // multi-GB.
        let stdout_drain = drain_into(stdout, &mut stdout_cap);
        let stderr_drain = drain_into(stderr, &mut stderr_cap);
        let wait = child.wait();
        let (status, _, _) = if let Some(timeout) = request.timeout {
            let combined = async { tokio::join!(wait, stdout_drain, stderr_drain) };
            match tokio::time::timeout(timeout, combined).await {
                Ok((s, o, e)) => (s?, o, e),
                Err(_) => {
                    return Err(anyhow::anyhow!(
                        "LocalActionRunner: action {} ({}) for target {:?} \
                         exceeded timeout of {:?}",
                        request.mnemonic,
                        id_str,
                        request.owner_target,
                        timeout
                    ));
                }
            }
        } else {
            let (s, o, e) = tokio::join!(wait, stdout_drain, stderr_drain);
            (s?, o, e)
        };

        let duration = start.elapsed();
        let peak_rss_kb = match rss_monitor {
            Some(monitor) => {
                #[cfg(target_os = "linux")]
                monitor.stop.store(true, Ordering::Relaxed);
                monitor.task.await.unwrap_or(0)
            }
            None => 0,
        };
        #[cfg(unix)]
        process_group_guard.disarm();
        Ok(ActionExecutionResult {
            declared_action_id: request.declared_action_id,
            exit_code: status.code().unwrap_or(-1),
            stdout_tail: stdout_cap.finish("stdout").await,
            stderr_tail: stderr_cap.finish("stderr").await,
            duration,
            peak_rss_kb,
        })
    }

    async fn execute_observed(
        &self,
        request: ExecutionRequest,
    ) -> Result<ObservedActionExecutionResult> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = request;
            anyhow::bail!("observed local action execution requires Linux")
        }
        #[cfg(target_os = "linux")]
        {
            use crate::{apply_compile_memory_cap, output_log_dir, output_ring_cap};
            use rush_observe::{
                CapturedDirectoryValue, CapturedGetdents64Outcome, CapturedProcessOutcome,
            };
            use std::ffi::OsString;
            use std::os::unix::ffi::OsStringExt;
            use std::process::Stdio;

            if request.timeout.is_some() {
                anyhow::bail!("observed local action execution does not support timeouts");
            }
            if output_log_dir().is_some() {
                anyhow::bail!("observed local action execution does not support full output logs");
            }
            let start = Instant::now();
            let declared_action_id = request.declared_action_id;
            let mut cmd = tokio::process::Command::new(&request.command.executable);
            cmd.args(&request.command.argv)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            if request.stdin_null {
                cmd.stdin(Stdio::null());
            }
            if let Some(argv0) = &request.command.argv0 {
                cmd.arg0(argv0);
            }
            match request.command.env_policy {
                ActionEnvPolicy::ExplicitOnly => {
                    cmd.env_clear();
                }
                ActionEnvPolicy::InheritHost => {}
            }
            for (key, value) in &request.command.env {
                cmd.env(key, value);
            }
            if let Some(directory) = &request.command.working_dir {
                cmd.current_dir(directory);
            }
            if request.terminate_process_group_on_drop {
                cmd.kill_on_drop(true);
                cmd.process_group(0);
            }
            apply_compile_memory_cap(&mut cmd, &request.mnemonic, request.resource_class);
            if request.isolation.is_some() && request.remove_stale_archive_before_execute {
                anyhow::bail!("isolated execution cannot perform parent-side archive cleanup");
            }
            let isolation = request
                .isolation
                .as_ref()
                .map(|policy| {
                    prepare_isolation(
                        policy,
                        request.command.working_dir.as_deref(),
                        request.credentials.as_ref(),
                    )
                })
                .transpose()?;
            apply_external_process_state(
                &mut cmd,
                request.signal_state,
                request.file_creation_mask,
                request.credentials,
            )?;
            rush_observe::install_process_tree_capture(cmd.as_std_mut());
            if let Some(isolation) = &isolation {
                isolation.install(&mut cmd);
            }
            if request.remove_stale_archive_before_execute && request.mnemonic == "CcArchive" {
                if let Some(archive) = request.command.argv.get(1) {
                    let _ = std::fs::remove_file(archive);
                }
            }

            let cap = output_ring_cap();
            let capture = tokio::task::spawn_blocking(move || {
                rush_observe::capture_prepared_process_tree_with_output(cmd.as_std_mut(), cap)
            })
            .await
            .map_err(|error| anyhow::anyhow!("observed process supervisor panicked: {error}"))??;
            let root = capture
                .receipt
                .processes()
                .iter()
                .find(|process| process.id == capture.receipt.root())
                .context("observed process receipt omits its root")?;
            let exit_code = match root.outcome {
                CapturedProcessOutcome::Exited(code) => code,
                CapturedProcessOutcome::Signaled(_) => -1,
            };
            let directory_observations = capture
                .receipt
                .processes()
                .iter()
                .flat_map(|process| &process.getdents64_attempts)
                .filter_map(|attempt| {
                    let identity = attempt.request.identity.as_ref()?;
                    let CapturedGetdents64Outcome::Enumerated {
                        directory_value: CapturedDirectoryValue::Complete { members, .. },
                        ..
                    } = &attempt.outcome
                    else {
                        return None;
                    };
                    Some(RuntimeDirectoryObservation {
                        path: PathBuf::from(OsString::from_vec(identity.target.clone())),
                        members: members.clone(),
                    })
                })
                .collect();
            Ok(ObservedActionExecutionResult {
                execution: ActionExecutionResult {
                    declared_action_id,
                    exit_code,
                    stdout_tail: capture.stdout_tail,
                    stderr_tail: capture.stderr_tail,
                    duration: start.elapsed(),
                    peak_rss_kb: 0,
                },
                directory_observations,
            })
        }
    }
}

#[cfg(target_os = "linux")]
fn apply_external_process_state(
    command: &mut tokio::process::Command,
    state: Option<ExternalSignalState>,
    file_creation_mask: Option<u32>,
    credentials: Option<ExternalCredentialState>,
) -> Result<()> {
    if let Some(state) = state {
        state.validate()?;
    }
    if file_creation_mask.is_some_and(|mask| mask & !0o777 != 0) {
        anyhow::bail!("external file-creation mask is invalid");
    }
    if let Some(credentials) = &credentials {
        credentials.validate()?;
    }
    if state.is_none() && file_creation_mask.is_none() && credentials.is_none() {
        return Ok(());
    }
    let mut observed_groups = credentials
        .as_ref()
        .map(|state| vec![0 as libc::gid_t; state.supplementary_groups.len()]);
    // SAFETY: the child-only hook performs only raw syscalls over values and
    // buffers allocated before fork. No allocator or process-global lock is
    // touched.
    unsafe {
        command.pre_exec(move || {
            let prior_signal_mask = if credentials.is_some() {
                Some(block_all_external_signals()?)
            } else {
                None
            };
            if let Some(credentials) = &credentials {
                let groups = observed_groups
                    .as_mut()
                    .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EINVAL))?;
                verify_external_credential_state(credentials, groups)?;
            }
            if let Some(mask) = file_creation_mask {
                libc::syscall(libc::SYS_umask, mask as libc::mode_t);
            }
            if let Some(state) = state {
                // Signal installation blocks every blockable signal until its
                // final syscall, so install the mask first and leave no window
                // where the requested signal state can observe an ambient one.
                install_external_signal_state(state)?;
            } else if let Some(prior_signal_mask) = prior_signal_mask {
                set_external_signal_mask(&prior_signal_mask, std::ptr::null_mut())?;
            }
            Ok(())
        });
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn apply_external_process_state(
    _command: &mut tokio::process::Command,
    state: Option<ExternalSignalState>,
    file_creation_mask: Option<u32>,
    credentials: Option<ExternalCredentialState>,
) -> Result<()> {
    if state.is_some() || file_creation_mask.is_some() || credentials.is_some() {
        anyhow::bail!("external process state requires Linux");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn set_external_signal_mask(mask: &u64, old_mask: *mut u64) -> std::io::Result<()> {
    if unsafe {
        libc::syscall(
            libc::SYS_rt_sigprocmask,
            libc::SIG_SETMASK,
            mask,
            old_mask,
            std::mem::size_of::<u64>(),
        )
    } == -1
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn block_all_external_signals() -> std::io::Result<u64> {
    const SIGKILL_MASK: u64 = 1_u64 << (libc::SIGKILL - 1);
    const SIGSTOP_MASK: u64 = 1_u64 << (libc::SIGSTOP - 1);
    let all_blockable = u64::MAX & !(SIGKILL_MASK | SIGSTOP_MASK);
    let mut prior = 0_u64;
    set_external_signal_mask(&all_blockable, &mut prior)?;
    Ok(prior)
}

#[cfg(target_os = "linux")]
fn verify_external_credential_state(
    expected: &ExternalCredentialState,
    observed_groups: &mut [libc::gid_t],
) -> std::io::Result<()> {
    fn mismatch() -> std::io::Error {
        std::io::Error::from_raw_os_error(libc::EPERM)
    }

    fn syscall_failed(result: libc::c_long) -> std::io::Result<libc::c_long> {
        if result == -1 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(result)
        }
    }

    let mut real_user = 0 as libc::uid_t;
    let mut effective_user = 0 as libc::uid_t;
    let mut saved_user = 0 as libc::uid_t;
    let mut real_group = 0 as libc::gid_t;
    let mut effective_group = 0 as libc::gid_t;
    let mut saved_group = 0 as libc::gid_t;
    unsafe {
        syscall_failed(libc::syscall(
            libc::SYS_getresuid,
            &mut real_user,
            &mut effective_user,
            &mut saved_user,
        ))?;
        syscall_failed(libc::syscall(
            libc::SYS_getresgid,
            &mut real_group,
            &mut effective_group,
            &mut saved_group,
        ))?;
    }
    let filesystem_user = unsafe { libc::syscall(libc::SYS_setfsuid, u32::MAX) };
    let filesystem_group = unsafe { libc::syscall(libc::SYS_setfsgid, u32::MAX) };
    if real_user != expected.real_user
        || effective_user != expected.effective_user
        || saved_user != expected.saved_user
        || u32::try_from(filesystem_user).ok() != Some(expected.filesystem_user)
        || real_group != expected.real_group
        || effective_group != expected.effective_group
        || saved_group != expected.saved_group
        || u32::try_from(filesystem_group).ok() != Some(expected.filesystem_group)
    {
        return Err(mismatch());
    }

    let group_count = unsafe {
        syscall_failed(libc::syscall(
            libc::SYS_getgroups,
            0,
            std::ptr::null_mut::<libc::gid_t>(),
        ))?
    };
    if usize::try_from(group_count).ok() != Some(observed_groups.len()) {
        return Err(mismatch());
    }
    if !observed_groups.is_empty() {
        let observed = unsafe {
            syscall_failed(libc::syscall(
                libc::SYS_getgroups,
                observed_groups.len(),
                observed_groups.as_mut_ptr(),
            ))?
        };
        if usize::try_from(observed).ok() != Some(observed_groups.len()) {
            return Err(mismatch());
        }
        observed_groups.sort_unstable();
    }
    if observed_groups
        .iter()
        .copied()
        .map(|identity| identity as u32)
        .ne(expected.supplementary_groups.iter().copied())
    {
        return Err(mismatch());
    }

    #[repr(C)]
    struct CapabilityHeader {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CapabilityData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let mut header = CapabilityHeader {
        version: 0x2008_0522,
        pid: 0,
    };
    let mut data = [CapabilityData {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    unsafe {
        syscall_failed(libc::syscall(
            libc::SYS_capget,
            &mut header,
            data.as_mut_ptr(),
        ))?;
    }
    let join = |low: u32, high: u32| u64::from(low) | (u64::from(high) << 32);
    if join(data[0].inheritable, data[1].inheritable) != expected.inheritable_capabilities
        || join(data[0].permitted, data[1].permitted) != expected.permitted_capabilities
        || join(data[0].effective, data[1].effective) != expected.effective_capabilities
    {
        return Err(mismatch());
    }

    fn bounding_capability_mask() -> std::io::Result<u64> {
        let mut mask = 0_u64;
        for capability in 0..64 {
            let result = unsafe {
                libc::syscall(libc::SYS_prctl, libc::PR_CAPBSET_READ, capability, 0, 0, 0)
            };
            let error = (result == -1).then(std::io::Error::last_os_error);
            match result {
                0 => {}
                1 => mask |= 1_u64 << capability,
                -1 if error.as_ref().and_then(std::io::Error::raw_os_error)
                    == Some(libc::EINVAL) => {}
                -1 => return Err(error.unwrap()),
                _ => return Err(mismatch()),
            }
        }
        Ok(mask)
    }
    fn ambient_capability_mask() -> std::io::Result<u64> {
        let mut mask = 0_u64;
        for capability in 0..64 {
            let result = unsafe {
                libc::syscall(
                    libc::SYS_prctl,
                    libc::PR_CAP_AMBIENT,
                    libc::PR_CAP_AMBIENT_IS_SET,
                    capability,
                    0,
                    0,
                )
            };
            let error = (result == -1).then(std::io::Error::last_os_error);
            match result {
                0 => {}
                1 => mask |= 1_u64 << capability,
                -1 if error.as_ref().and_then(std::io::Error::raw_os_error)
                    == Some(libc::EINVAL) => {}
                -1 => return Err(error.unwrap()),
                _ => return Err(mismatch()),
            }
        }
        Ok(mask)
    }
    let bounding = bounding_capability_mask()?;
    let ambient = ambient_capability_mask()?;
    let no_new_privileges = unsafe {
        syscall_failed(libc::syscall(
            libc::SYS_prctl,
            libc::PR_GET_NO_NEW_PRIVS,
            0,
            0,
            0,
            0,
        ))?
    };
    if bounding != expected.bounding_capabilities
        || ambient != expected.ambient_capabilities
        || no_new_privileges != libc::c_long::from(expected.no_new_privileges)
    {
        return Err(mismatch());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_external_signal_state(state: ExternalSignalState) -> std::io::Result<()> {
    #[repr(C)]
    struct KernelSigaction {
        handler: usize,
        flags: usize,
        restorer: usize,
        mask: u64,
    }

    // Keep signals from observing a partially installed disposition table.
    block_all_external_signals()?;
    const SIGKILL_MASK: u64 = 1_u64 << (libc::SIGKILL - 1);
    const SIGSTOP_MASK: u64 = 1_u64 << (libc::SIGSTOP - 1);
    for signal in 1..=64 {
        let bit = 1_u64 << (signal - 1);
        if bit & (SIGKILL_MASK | SIGSTOP_MASK) != 0 {
            continue;
        }
        let action = KernelSigaction {
            handler: if state.ignored & bit != 0 {
                libc::SIG_IGN
            } else {
                libc::SIG_DFL
            },
            flags: 0,
            restorer: 0,
            mask: 0,
        };
        if unsafe {
            libc::syscall(
                libc::SYS_rt_sigaction,
                signal,
                &action,
                std::ptr::null_mut::<KernelSigaction>(),
                std::mem::size_of::<u64>(),
            )
        } == -1
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    set_external_signal_mask(&state.blocked, std::ptr::null_mut())
}

#[cfg(unix)]
struct ProcessGroupKillGuard {
    process_group: Option<i32>,
}

#[cfg(unix)]
impl ProcessGroupKillGuard {
    fn new(enabled: bool, child_id: Option<u32>) -> Self {
        let process_group = if enabled {
            child_id.and_then(|id| i32::try_from(id).ok())
        } else {
            None
        };
        Self { process_group }
    }

    fn disarm(&mut self) {
        self.process_group = None;
    }
}

#[cfg(unix)]
impl Drop for ProcessGroupKillGuard {
    fn drop(&mut self) {
        if let Some(process_group) = self.process_group {
            // A negative pid targets every process in the child's dedicated
            // group. Ignore ESRCH: the group may already have exited.
            unsafe {
                libc::kill(-process_group, libc::SIGKILL);
            }
        }
    }
}

/// Phase 5.C.4 commit 7: [`LocalActionRunner`] that acquires a
/// `ResourceManager` permit before delegating to an inner runner,
/// then releases it after. This is the symmetric fix to the
/// "ResourceManager permit gap at rule entry" Pro flagged: every
/// local subprocess action — legacy or scheduler — now goes
/// through the same permit acquisition path.
///
/// The wrapper is a decorator over any `LocalActionRunner`. The
/// inner runner stays focused on spawn-and-execute; the
/// permit-acquiring layer handles the throttling. Layered this way
/// so future runners (sandbox, remote, etc.) get the same permit
/// behavior automatically.
///
/// Resource request derivation uses an explicit class when the
/// request supplies one and otherwise classifies the mnemonic via
/// [`resource_request_for_request`]. Compile actions use
/// `cpp_compile()` (2.5 GB), links use `cpp_link()` (4 GB+), and
/// other classes use `minimal()` (512 MB). LTO and debug-info
/// heuristics adjust the compile/link memory budget matching what
/// the orchestrator's `resource_request_for_action` does for the
/// legacy `Action` path — same policy, different input shape.
pub struct PermitAcquiringRunner<R: LocalActionRunner> {
    pub inner: R,
    pub resource_manager: std::sync::Arc<rush_resource::ResourceManager>,
}

impl<R: LocalActionRunner> PermitAcquiringRunner<R> {
    pub fn new(inner: R, resource_manager: std::sync::Arc<rush_resource::ResourceManager>) -> Self {
        Self {
            inner,
            resource_manager,
        }
    }
}

#[async_trait]
impl<R: LocalActionRunner> LocalActionRunner for PermitAcquiringRunner<R> {
    async fn execute(&self, request: ExecutionRequest) -> Result<ActionExecutionResult> {
        let trace_path = resource_trace_path();
        let action_id = request.declared_action_id;
        let class = request
            .resource_class
            .unwrap_or_else(|| rush_resource::ResourceClass::from_mnemonic(&request.mnemonic));
        let resource_request = resource_request_for_request(&request);
        let handle = self
            .resource_manager
            .acquire(resource_request)
            .await
            .map_err(|e| anyhow::anyhow!("ResourceManager.acquire failed: {:?}", e))?;
        let admission = handle.admission_stats();
        let result = self.inner.execute(request).await;
        handle.release_async().await;
        if let Some(path) = trace_path {
            let (execution_us, peak_rss_kb, outcome) = match &result {
                Ok(result) => (
                    result.duration.as_micros(),
                    result.peak_rss_kb,
                    if result.exit_code == 0 {
                        "success"
                    } else {
                        "exit"
                    },
                ),
                Err(_) => (0, 0, "error"),
            };
            append_resource_trace(
                &path,
                &format!(
                    "command\tid={:?}\tclass={:?}\treserved_memory_mb={}\treserved_cpu_slots={}\tadmission_wait_us={}\tcpu_permit_wait_us={}\tpressure_retries={}\tmemory_retries={}\tcpu_limit_retries={}\tmemory_refresh_retries={}\texecution_us={}\tpeak_rss_kb={}\toutcome={}",
                    action_id,
                    class,
                    resource_request.memory_mb,
                    resource_request.cpu_slots,
                    admission.wait.as_micros(),
                    admission.cpu_permit_wait.as_micros(),
                    admission.pressure_retries,
                    admission.memory_retries,
                    admission.cpu_limit_retries,
                    admission.memory_refresh_retries,
                    execution_us,
                    peak_rss_kb,
                    outcome,
                ),
            );
        }
        result
    }

    async fn execute_observed(
        &self,
        request: ExecutionRequest,
    ) -> Result<ObservedActionExecutionResult> {
        let resource_request = resource_request_for_request(&request);
        let handle = self
            .resource_manager
            .acquire(resource_request)
            .await
            .map_err(|error| anyhow::anyhow!("ResourceManager.acquire failed: {error:?}"))?;
        let result = self.inner.execute_observed(request).await;
        handle.release_async().await;
        result
    }
}

/// Phase 5.C.4 commit 7 + 21: derive a
/// [`rush_resource::ResourceRequest`] from an [`ExecutionRequest`].
/// An explicit resource class on the request takes precedence.
/// Otherwise, mnemonic classification lives in
/// [`rush_resource::ResourceClass::from_mnemonic`] (commit 21 seam);
/// per-class defaults via
/// [`rush_resource::ResourceClass::default_request`]. The argv
/// heuristics below (LTO / debug-info budget bumps for compile,
/// obj-count-driven link memory) layer on top of the class
/// default. Both legacy and scheduler paths produce equivalent
/// requests for equivalent actions.
pub fn resource_request_for_request(request: &ExecutionRequest) -> rush_resource::ResourceRequest {
    let argv = &request.command.argv;
    let class = request
        .resource_class
        .unwrap_or_else(|| rush_resource::ResourceClass::from_mnemonic(&request.mnemonic));
    let mut req = class.default_request();
    match class {
        rush_resource::ResourceClass::CppCompile | rush_resource::ResourceClass::KernelCompile => {
            let has_lto = argv.iter().any(|a| a == "-flto" || a.contains("thinlto"));
            let has_debug = argv.iter().any(|a| a == "-g");
            if has_lto {
                req.memory_mb = req.memory_mb.max(4096);
            }
            if has_debug {
                req.memory_mb += 512;
            }
        }
        rush_resource::ResourceClass::Link | rush_resource::ResourceClass::KernelLink => {
            let obj_count = argv
                .iter()
                .filter(|a| a.ends_with(".o") || a.ends_with(".obj"))
                .count();
            if obj_count > 0 {
                let extra_chunks = obj_count / 400;
                let extra_mb = extra_chunks.saturating_mul(2048);
                req.memory_mb = (req.memory_mb + extra_mb).min(16 * 1024);
            }
            let has_lto = argv
                .iter()
                .any(|a| a == "-flto" || a.contains("thinlto") || a.contains("-Wl,--thinlto"));
            if has_lto {
                req.memory_mb = req.memory_mb.max(8 * 1024);
            }
        }
        // CodegenProtoc / CodegenTblgen / Archive / Other:
        // class default suffices (no argv heuristics yet).
        // Future per-class tuning (e.g. .grpc.pb.cc compile
        // memory bumps for tblgen) attaches here.
        _ => {}
    }
    req
}

/// Test-only [`LocalActionRunner`] that records every received
/// request and returns a synthetic success without spawning a
/// process. Lets unit tests prove scheduler dispatch routes
/// through the trait without `g++` / `ar` etc. on PATH.
pub struct FakeLocalActionRunner {
    pub calls: std::sync::Mutex<Vec<ExecutionRequest>>,
}

impl Default for FakeLocalActionRunner {
    fn default() -> Self {
        Self {
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl LocalActionRunner for FakeLocalActionRunner {
    async fn execute(&self, request: ExecutionRequest) -> Result<ActionExecutionResult> {
        let result = ActionExecutionResult {
            declared_action_id: request.declared_action_id,
            exit_code: 0,
            stdout_tail: Vec::new(),
            stderr_tail: Vec::new(),
            duration: Duration::from_millis(0),
            peak_rss_kb: 0,
        };
        self.calls.lock().unwrap().push(request);
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rush_action::ids::DeclaredActionId;
    use std::path::PathBuf;

    #[cfg(target_os = "linux")]
    fn current_credential_state() -> ExternalCredentialState {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        credential_state_from_status(&status)
    }

    #[cfg(target_os = "linux")]
    fn credential_state_from_status(status: &str) -> ExternalCredentialState {
        let field = |name: &str| {
            status
                .lines()
                .find_map(|line| {
                    line.strip_prefix(name)
                        .and_then(|line| line.strip_prefix(':'))
                })
                .unwrap()
                .trim()
        };
        let identities = |name: &str| {
            field(name)
                .split_ascii_whitespace()
                .map(|value| value.parse::<u32>().unwrap())
                .collect::<Vec<_>>()
        };
        let users = identities("Uid");
        let groups = identities("Gid");
        let mut supplementary_groups = field("Groups")
            .split_ascii_whitespace()
            .map(|value| value.parse::<u32>().unwrap())
            .collect::<Vec<_>>();
        supplementary_groups.sort_unstable();
        let capabilities = |name: &str| u64::from_str_radix(field(name), 16).unwrap();
        ExternalCredentialState {
            real_user: users[0],
            effective_user: users[1],
            saved_user: users[2],
            filesystem_user: users[3],
            real_group: groups[0],
            effective_group: groups[1],
            saved_group: groups[2],
            filesystem_group: groups[3],
            supplementary_groups,
            inheritable_capabilities: capabilities("CapInh"),
            permitted_capabilities: capabilities("CapPrm"),
            effective_capabilities: capabilities("CapEff"),
            bounding_capabilities: capabilities("CapBnd"),
            ambient_capabilities: capabilities("CapAmb"),
            no_new_privileges: field("NoNewPrivs") == "1",
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn process_tree_rss_reads_the_current_process() {
        assert!(process_tree_rss_kb(std::process::id()).unwrap_or(0) > 0);
    }

    #[test]
    fn resource_trace_writer_appends_complete_records() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("resource.tsv");
        append_resource_trace(&path, "command\tid=one");
        append_resource_trace(&path, "command\tid=two");
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "command\tid=one\ncommand\tid=two\n"
        );
    }

    fn req(id: u32, mnemonic: &str) -> ExecutionRequest {
        ExecutionRequest {
            declared_action_id: DeclaredActionId(id),
            mnemonic: mnemonic.to_string(),
            resource_class: None,
            owner_target: TargetId(1),
            command: MaterializedCommand {
                executable: PathBuf::from("/bin/true"),
                argv0: None,
                argv: vec![],
                env: vec![],
                working_dir: None,
                env_policy: rush_action::ids::ActionEnvPolicy::InheritHost,
            },
            declared_inputs: vec![],
            declared_outputs: vec![],
            timeout: None,
            log_command_index: None,
            remove_stale_archive_before_execute: true,
            terminate_process_group_on_drop: false,
            stdin_null: false,
            signal_state: None,
            file_creation_mask: None,
            credentials: None,
            isolation: None,
        }
    }

    #[tokio::test]
    async fn fake_runner_records_calls_and_succeeds() {
        let runner = FakeLocalActionRunner::default();
        let result = runner.execute(req(7, "CppCompile")).await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.declared_action_id, DeclaredActionId(7));
        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].mnemonic, "CppCompile");
    }

    #[tokio::test]
    async fn default_runner_executes_true_successfully() {
        let runner = DefaultLocalActionRunner::default();
        let result = runner.execute(req(0, "Test")).await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(result.stdout_tail.is_empty() || result.stdout_tail.iter().all(|&b| b == b'\n'));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn default_runner_can_isolate_standard_input() {
        let mut request = req(107, "ExternalTest");
        request.command.executable = PathBuf::from("/usr/bin/readlink");
        request.command.argv = vec!["/proc/self/fd/0".to_string()];
        request.stdin_null = true;

        let result = DefaultLocalActionRunner.execute(request).await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout_tail, b"/dev/null\n");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn default_runner_confines_writes_and_network_namespace() {
        use std::os::fd::AsRawFd as _;

        let temp = tempfile::tempdir().unwrap();
        let writable = temp.path().join("writable");
        let outside = temp.path().join("outside");
        let inherited = temp.path().join("inherited");
        std::fs::create_dir(&writable).unwrap();
        let inherited_file = std::fs::File::create(&inherited).unwrap();
        let inherited_descriptor = inherited_file.as_raw_fd();
        assert_eq!(
            unsafe { libc::fcntl(inherited_descriptor, libc::F_SETFD, 0) },
            0
        );
        let parent_network = std::fs::read_link("/proc/self/ns/net").unwrap();
        let mut request = req(108, "ExternalTest");
        request.command.executable = PathBuf::from("/bin/sh");
        request.command.working_dir = Some(writable.clone());
        request.command.argv = vec![
            "-c".to_string(),
            concat!(
                "printf allowed >\"$1/inside\"; ",
                "if printf denied >\"$2\" 2>/dev/null; then exit 71; fi; ",
                "id -u; id -g; readlink /proc/self/ns/net; ",
                "sed -n 's/^NoNewPrivs:[[:space:]]*//p' /proc/self/status; ",
                "if python3 -c 'import socket; socket.socket()' 2>/dev/null; ",
                "then exit 72; fi; ",
                "if kill -0 \"$3\" 2>/dev/null; then exit 73; fi; ",
                "if python3 -c 'import os,sys; ",
                "os.write(int(sys.argv[1]), b\"escaped\")' \"$4\" 2>/dev/null; ",
                "then exit 74; fi; ",
                "if ln \"$5\" \"$1/alias\" 2>/dev/null; then exit 75; fi; ",
                "if python3 -c 'import os,sys; pid=int(sys.argv[1]); ",
                "os.sched_setaffinity(pid, os.sched_getaffinity(pid))' ",
                "\"$3\" 2>/dev/null; then exit 76; fi"
            )
            .to_string(),
            "isolation-test".to_string(),
            writable.display().to_string(),
            outside.display().to_string(),
            std::process::id().to_string(),
            inherited_descriptor.to_string(),
            inherited.display().to_string(),
        ];
        let isolation = LocalExecutionIsolation::new(vec![writable.clone()]).unwrap();
        assert_eq!(
            isolation.descriptor_topology(),
            crate::local_isolation::LocalDescriptorTopology::StandardOnlyV1
        );
        request.isolation = Some(isolation);
        request.remove_stale_archive_before_execute = false;

        let result = DefaultLocalActionRunner.execute(request).await;
        assert_eq!(
            unsafe { libc::fcntl(inherited_descriptor, libc::F_SETFD, libc::FD_CLOEXEC) },
            0
        );
        let result = result.unwrap();
        assert_eq!(
            result.exit_code,
            0,
            "{}",
            String::from_utf8_lossy(&result.stderr_tail)
        );
        assert_eq!(std::fs::read(writable.join("inside")).unwrap(), b"allowed");
        assert!(!outside.exists());
        assert!(!writable.join("alias").exists());
        assert_eq!(std::fs::read(inherited).unwrap(), b"");
        let lines = String::from_utf8(result.stdout_tail)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect::<Vec<_>>();
        assert_eq!(lines[0], unsafe { libc::getuid() }.to_string());
        assert_eq!(lines[1], unsafe { libc::getgid() }.to_string());
        assert_ne!(PathBuf::from(&lines[2]), parent_network);
        let parent_no_new_privileges = std::fs::read_to_string("/proc/self/status")
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("NoNewPrivs:").map(str::trim))
            .unwrap()
            .to_string();
        assert_eq!(lines[3], parent_no_new_privileges);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn default_runner_rejects_a_writable_host_root() {
        let mut request = req(109, "ExternalTest");
        request.isolation = Some(LocalExecutionIsolation::new(vec![PathBuf::from("/")]).unwrap());
        request.remove_stale_archive_before_execute = false;
        let error = DefaultLocalActionRunner.execute(request).await.unwrap_err();
        assert!(
            error.to_string().contains("host root writable"),
            "{error:#}"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn default_runner_rejects_parent_archive_cleanup_under_isolation() {
        let temp = tempfile::tempdir().unwrap();
        let mut request = req(110, "ExternalTest");
        request.isolation =
            Some(LocalExecutionIsolation::new(vec![temp.path().to_path_buf()]).unwrap());
        let error = DefaultLocalActionRunner.execute(request).await.unwrap_err();
        assert!(
            error.to_string().contains("parent-side archive cleanup"),
            "{error:#}"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn default_runner_rejects_unmappable_external_credentials() {
        let temp = tempfile::tempdir().unwrap();
        let mut request = req(111, "ExternalTest");
        let mut credentials = current_credential_state();
        credentials.saved_user ^= 1;
        request.credentials = Some(credentials);
        request.isolation =
            Some(LocalExecutionIsolation::new(vec![temp.path().to_path_buf()]).unwrap());
        request.remove_stale_archive_before_execute = false;
        let error = DefaultLocalActionRunner.execute(request).await.unwrap_err();
        assert!(
            error.to_string().contains("distinct user identities"),
            "{error:#}"
        );

        let mut request = req(112, "ExternalTest");
        let mut credentials = current_credential_state();
        credentials.effective_user = credentials.real_user;
        credentials.saved_user = credentials.real_user;
        credentials.filesystem_user = credentials.real_user;
        credentials.effective_group = credentials.real_group;
        credentials.saved_group = credentials.real_group;
        credentials.filesystem_group = credentials.real_group;
        credentials.supplementary_groups = vec![credentials.real_group];
        credentials.effective_capabilities ^= 1;
        request.credentials = Some(credentials);
        request.isolation =
            Some(LocalExecutionIsolation::new(vec![temp.path().to_path_buf()]).unwrap());
        request.remove_stale_archive_before_execute = false;
        let error = DefaultLocalActionRunner.execute(request).await.unwrap_err();
        assert!(
            error.to_string().contains("capability-bearing credentials"),
            "{error:#}"
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn default_runner_preserves_mappable_external_credentials() {
        let expected = current_credential_state();
        let overflow_group = crate::local_isolation::kernel_overflow_group_identity().unwrap();
        let identities_are_mappable = [
            expected.effective_user,
            expected.saved_user,
            expected.filesystem_user,
        ]
        .iter()
        .all(|identity| *identity == expected.real_user)
            && [
                expected.effective_group,
                expected.saved_group,
                expected.filesystem_group,
            ]
            .iter()
            .all(|identity| *identity == expected.real_group)
            && expected
                .supplementary_groups
                .iter()
                .all(|identity| *identity == expected.real_group || *identity == overflow_group);
        if !identities_are_mappable {
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let mut request = req(113, "ExternalTest");
        request.command.executable = PathBuf::from("/bin/cat");
        request.command.argv = vec!["/proc/self/status".to_string()];
        request.command.working_dir = Some(temp.path().to_path_buf());
        request.credentials = Some(expected.clone());
        request.isolation =
            Some(LocalExecutionIsolation::new(vec![temp.path().to_path_buf()]).unwrap());
        request.remove_stale_archive_before_execute = false;

        let result = DefaultLocalActionRunner.execute(request).await.unwrap();
        assert_eq!(result.exit_code, 0);
        let observed =
            credential_state_from_status(std::str::from_utf8(&result.stdout_tail).unwrap());
        assert_eq!(observed, expected);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn default_runner_installs_exact_argv_zero() {
        let mut request = req(106, "ExternalTest");
        request.command.executable = PathBuf::from("/bin/sh");
        request.command.argv0 = Some("captured-shell".to_string());
        request.command.argv = vec!["-c".to_string(), "printf '%s' \"$0\"".to_string()];

        let result = DefaultLocalActionRunner.execute(request).await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.stdout_tail, b"captured-shell");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn default_runner_installs_exact_external_signal_state() {
        let mut request = req(105, "ExternalTest");
        request.command.executable = PathBuf::from("/bin/cat");
        request.command.argv = vec!["/proc/self/status".to_string()];
        let expected = ExternalSignalState {
            blocked: 1_u64 << (libc::SIGUSR1 - 1),
            ignored: (1_u64 << (libc::SIGUSR2 - 1)) | (1_u64 << 31) | (1_u64 << 32),
        };
        request.signal_state = Some(expected);
        request.file_creation_mask = Some(0o077);
        request.credentials = Some(current_credential_state());

        let result = DefaultLocalActionRunner.execute(request).await.unwrap();
        assert_eq!(result.exit_code, 0);
        let status = String::from_utf8(result.stdout_tail).unwrap();
        let mask = |name: &str| {
            let value = status
                .lines()
                .find_map(|line| {
                    line.strip_prefix(name)
                        .and_then(|line| line.strip_prefix(':'))
                })
                .unwrap();
            u64::from_str_radix(value.trim(), 16).unwrap()
        };
        assert_eq!(mask("SigBlk"), expected.blocked);
        assert_eq!(mask("SigIgn"), expected.ignored);
        let umask = status
            .lines()
            .find_map(|line| line.strip_prefix("Umask:"))
            .unwrap();
        assert_eq!(u32::from_str_radix(umask.trim(), 8).unwrap(), 0o077);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn default_runner_rejects_invalid_external_file_creation_mask() {
        let mut request = req(106, "ExternalTest");
        request.file_creation_mask = Some(0o1000);
        let error = DefaultLocalActionRunner
            .execute(request)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("file-creation mask is invalid"), "{error}");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn default_runner_requires_exact_external_credentials() {
        let mut request = req(107, "ExternalTest");
        request.credentials = Some(current_credential_state());
        assert_eq!(
            DefaultLocalActionRunner
                .execute(request.clone())
                .await
                .unwrap()
                .exit_code,
            0
        );

        request.credentials.as_mut().unwrap().effective_user ^= 1;
        let error = DefaultLocalActionRunner.execute(request).await.unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .and_then(|error| error.raw_os_error()),
            Some(libc::EPERM)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn external_credential_verifier_covers_every_state_class() {
        let current = current_credential_state();
        let verify = |expected: &ExternalCredentialState| {
            let mut groups = vec![0 as libc::gid_t; expected.supplementary_groups.len()];
            verify_external_credential_state(expected, &mut groups)
        };
        verify(&current).unwrap();

        let mut changed = current.clone();
        changed.real_user ^= 1;
        assert_eq!(
            verify(&changed).unwrap_err().raw_os_error(),
            Some(libc::EPERM)
        );
        changed = current.clone();
        changed.filesystem_group ^= 1;
        assert_eq!(
            verify(&changed).unwrap_err().raw_os_error(),
            Some(libc::EPERM)
        );
        changed = current.clone();
        let extra_group = (0..u32::MAX)
            .find(|identity| !changed.supplementary_groups.contains(identity))
            .unwrap();
        changed.supplementary_groups.push(extra_group);
        changed.supplementary_groups.sort_unstable();
        changed.supplementary_groups.dedup();
        assert_eq!(
            verify(&changed).unwrap_err().raw_os_error(),
            Some(libc::EPERM)
        );
        changed = current.clone();
        changed.permitted_capabilities ^= 1;
        assert_eq!(
            verify(&changed).unwrap_err().raw_os_error(),
            Some(libc::EPERM)
        );
        changed = current.clone();
        changed.bounding_capabilities ^= 1;
        assert_eq!(
            verify(&changed).unwrap_err().raw_os_error(),
            Some(libc::EPERM)
        );
        changed = current.clone();
        changed.ambient_capabilities ^= 1;
        assert_eq!(
            verify(&changed).unwrap_err().raw_os_error(),
            Some(libc::EPERM)
        );
        changed = current;
        changed.no_new_privileges = !changed.no_new_privileges;
        assert_eq!(
            verify(&changed).unwrap_err().raw_os_error(),
            Some(libc::EPERM)
        );
    }

    #[tokio::test]
    async fn permit_acquiring_runner_delegates_to_inner() {
        // Wrap FakeLocalActionRunner in PermitAcquiringRunner;
        // verify the inner runner saw the call (permit was
        // acquired and released).
        let rm = rush_resource::ResourceManager::new()
            .expect("ResourceManager::new must succeed in test");
        let inner = FakeLocalActionRunner::default();
        let wrapper = PermitAcquiringRunner::new(inner, rm);
        let result = wrapper.execute(req(42, "CppCompile")).await.unwrap();
        assert_eq!(result.declared_action_id, DeclaredActionId(42));
        let calls = wrapper.inner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].mnemonic, "CppCompile");
    }

    #[test]
    fn resource_request_for_request_compiles_use_cpp_compile() {
        let r = req(0, "CppCompile");
        let req = resource_request_for_request(&r);
        // cpp_compile baseline is 2560 MB.
        assert!(req.memory_mb >= 2560);
    }

    #[test]
    fn explicit_kernel_compile_class_has_an_independent_policy_seam() {
        let mut r = req(0, "CC");
        r.resource_class = Some(rush_resource::ResourceClass::KernelCompile);
        let req = resource_request_for_request(&r);
        assert_eq!(
            req.memory_mb,
            rush_resource::ResourceRequest::minimal().memory_mb
        );
        assert!(crate::uses_compile_memory_cap(
            "CC",
            Some(rush_resource::ResourceClass::KernelCompile),
        ));
    }

    #[test]
    fn resource_request_for_request_links_use_cpp_link() {
        let r = req(0, "CcLink");
        let req = resource_request_for_request(&r);
        // cpp_link baseline is 4096 MB.
        assert!(req.memory_mb >= 4096);
    }

    #[test]
    fn resource_request_for_request_unknown_mnemonic_uses_minimal() {
        let r = req(0, "MlirTblgen");
        let req = resource_request_for_request(&r);
        // minimal baseline is 512 MB.
        assert_eq!(req.memory_mb, 512);
    }

    #[test]
    fn explicit_resource_class_overrides_mnemonic() {
        let mut r = req(0, "CppCompile");
        r.resource_class = Some(rush_resource::ResourceClass::Link);
        let req = resource_request_for_request(&r);
        assert_eq!(req.memory_mb, 4096);
    }

    #[test]
    fn explicit_compile_class_drives_memory_cap_for_kbuild_mnemonic() {
        assert!(crate::uses_compile_memory_cap(
            "CC",
            Some(rush_resource::ResourceClass::CppCompile),
        ));
        assert!(!crate::uses_compile_memory_cap(
            "CC",
            Some(rush_resource::ResourceClass::Other),
        ));
    }

    #[tokio::test]
    async fn default_runner_honors_archive_cleanup_gate() {
        let output =
            std::env::temp_dir().join(format!("rush-exec-archive-cleanup-{}", std::process::id()));
        std::fs::write(&output, b"stale").unwrap();

        let runner = DefaultLocalActionRunner::default();
        let mut request = req(103, "CcArchive");
        request.command.argv = vec!["rcs".to_string(), output.display().to_string()];
        request.remove_stale_archive_before_execute = false;
        runner.execute(request.clone()).await.unwrap();
        assert!(output.exists(), "disabled cleanup must preserve the output");

        request.remove_stale_archive_before_execute = true;
        runner.execute(request).await.unwrap();
        assert!(!output.exists(), "enabled cleanup must remove the output");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_runner_terminates_the_compound_shell_process_group() {
        let temp = tempfile::tempdir().unwrap();
        let child_pid_path = temp.path().join("child.pid");
        let mut request = req(104, "ImportedShell");
        request.command.executable = PathBuf::from("/bin/sh");
        request.command.argv = vec![
            "-c".to_string(),
            format!(
                "sleep 30 & child=$!; printf '%s' \"$child\" > '{}'; wait",
                child_pid_path.display()
            ),
        ];
        request.terminate_process_group_on_drop = true;

        let task = tokio::spawn(async move { DefaultLocalActionRunner.execute(request).await });
        let mut child_pid = None;
        for _ in 0..500 {
            if let Ok(contents) = std::fs::read_to_string(&child_pid_path) {
                if let Ok(pid) = contents.parse::<i32>() {
                    child_pid = Some(pid);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let child_pid = match child_pid {
            Some(pid) => pid,
            None => {
                task.abort();
                let _ = task.await;
                panic!("compound shell did not report child pid");
            }
        };
        task.abort();
        let _ = task.await;

        let mut gone = false;
        for _ in 0..200 {
            let result = unsafe { libc::kill(child_pid, 0) };
            if result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            gone,
            "background child {} survived runner cancellation",
            child_pid
        );
    }

    /// Phase 5.C.4 commit 66: with `ExplicitOnly`, the runner
    /// must clear the host env before applying the declared
    /// overlay. Verified by spawning `/usr/bin/env` (or `env`)
    /// and checking that a host var like `HOME` does NOT
    /// appear in the output.
    ///
    /// Pro's directive: "If `ExplicitOnly`, the runner must
    /// call `env_clear()` and set only declared env vars."
    /// This test pins that contract: setting a guard env var on
    /// the parent (via `std::env::set_var`) and asserting the
    /// child does not see it under `ExplicitOnly`.
    #[tokio::test]
    async fn default_runner_explicit_only_clears_host_env() {
        // Set a sentinel on the parent process. With
        // ExplicitOnly the child must NOT see it.
        std::env::set_var("RUSH_TEST_HOST_SENTINEL", "leaked");
        let runner = DefaultLocalActionRunner::default();
        let request = ExecutionRequest {
            declared_action_id: DeclaredActionId(101),
            mnemonic: "Test".to_string(),
            resource_class: None,
            owner_target: TargetId(1),
            command: MaterializedCommand {
                executable: PathBuf::from("/usr/bin/env"),
                argv0: None,
                argv: vec![],
                env: vec![],
                working_dir: None,
                env_policy: ActionEnvPolicy::ExplicitOnly,
            },
            declared_inputs: vec![],
            declared_outputs: vec![],
            timeout: Some(Duration::from_secs(5)),
            log_command_index: None,
            remove_stale_archive_before_execute: true,
            terminate_process_group_on_drop: false,
            stdin_null: false,
            signal_state: None,
            file_creation_mask: None,
            credentials: None,
            isolation: None,
        };
        let result = runner.execute(request).await.unwrap();
        let stdout = String::from_utf8_lossy(&result.stdout_tail);
        assert!(
            !stdout.contains("RUSH_TEST_HOST_SENTINEL"),
            "ExplicitOnly must call env_clear() before applying \
             declared env; child saw host sentinel:\n{}",
            stdout
        );
        std::env::remove_var("RUSH_TEST_HOST_SENTINEL");
    }

    /// Phase 5.C.4 commit 66: with `InheritHost` (the default),
    /// the parent's env reaches the child. This is the
    /// "honestly inherited" case — the diagnostic /
    /// cacheability layer must classify these actions as
    /// non-remote-cacheable, but the runner-side behavior is
    /// to inherit.
    #[tokio::test]
    async fn default_runner_inherit_host_passes_parent_env() {
        std::env::set_var("RUSH_TEST_HOST_SENTINEL_2", "passed");
        let runner = DefaultLocalActionRunner::default();
        let request = ExecutionRequest {
            declared_action_id: DeclaredActionId(102),
            mnemonic: "Test".to_string(),
            resource_class: None,
            owner_target: TargetId(1),
            command: MaterializedCommand {
                executable: PathBuf::from("/usr/bin/env"),
                argv0: None,
                argv: vec![],
                env: vec![],
                working_dir: None,
                env_policy: ActionEnvPolicy::InheritHost,
            },
            declared_inputs: vec![],
            declared_outputs: vec![],
            timeout: Some(Duration::from_secs(5)),
            log_command_index: None,
            remove_stale_archive_before_execute: true,
            terminate_process_group_on_drop: false,
            stdin_null: false,
            signal_state: None,
            file_creation_mask: None,
            credentials: None,
            isolation: None,
        };
        let result = runner.execute(request).await.unwrap();
        let stdout = String::from_utf8_lossy(&result.stdout_tail);
        assert!(
            stdout.contains("RUSH_TEST_HOST_SENTINEL_2=passed"),
            "InheritHost must let the child see the parent's env; \
             stdout was:\n{}",
            stdout
        );
        std::env::remove_var("RUSH_TEST_HOST_SENTINEL_2");
    }

    #[tokio::test]
    async fn default_runner_propagates_nonzero_exit() {
        // /bin/false exits 1; runner must surface that.
        let runner = DefaultLocalActionRunner::default();
        let request = ExecutionRequest {
            declared_action_id: DeclaredActionId(99),
            mnemonic: "Test".to_string(),
            resource_class: None,
            owner_target: TargetId(1),
            command: MaterializedCommand {
                executable: PathBuf::from("/bin/false"),
                argv0: None,
                argv: vec![],
                env: vec![],
                working_dir: None,
                env_policy: rush_action::ids::ActionEnvPolicy::InheritHost,
            },
            declared_inputs: vec![],
            declared_outputs: vec![],
            timeout: None,
            log_command_index: None,
            remove_stale_archive_before_execute: true,
            terminate_process_group_on_drop: false,
            stdin_null: false,
            signal_state: None,
            file_creation_mask: None,
            credentials: None,
            isolation: None,
        };
        let result = runner.execute(request).await.unwrap();
        assert_ne!(result.exit_code, 0);
    }
}
