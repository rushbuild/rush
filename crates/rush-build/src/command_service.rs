//! [`BuildCommandService`] — the build-aware [`CommandService`] for `rushd`.
//!
//! This is the payoff of the persistent-daemon Run-wiring: a [`CommandService`]
//! impl that holds a LONG-LIVED [`BuildOrchestrator`] so the
//! [`SnapshotCache`](crate::analysis_snapshot::SnapshotCache) persists ACROSS
//! requests. The second identical `Run` reuses the daemon's retained analyzed
//! graph (skipping `.bzl` eval / `BUILD` parse / PASS-A) and runs only execution
//! — the warm no-op collapse, but now across separate CLIENT PROCESSES.
//!
//! It lives in `rush-build` (NOT `rush-command-core`) for a dependency-direction
//! reason: the build orchestrator is the heavy thing, and `rush-command-core`
//! must stay transport-and-build agnostic (it only defines the trait + the
//! per-command semantics). The tonic adapter in `rush-transport-grpc` wraps a
//! `dyn CommandService`, so it picks up this impl without either crate depending
//! on the other.
//!
//! ## Correctness
//!
//! The per-command [`CommandSemantics`] is reconstructed from the CLIENT's
//! [`RunRequest`] via [`CommandSemantics::from_run_request`] — never from the
//! daemon's own `std::env`. That is the whole point of `CommandSemantics`: a
//! resident daemon must use the client's argv/env/cwd/workspace-config so build
//! A cannot contaminate build B, and so the reuse key
//! ([`AnalysisKey`](crate::analysis_snapshot::AnalysisKey)) matches the
//! equivalent in-process command exactly.
//!
//! ## One command at a time
//!
//! The design specifies a coordinator that runs one command at a time (internal
//! build parallelism is retained inside the orchestrator). We serialize `run`
//! with an async mutex so the shared orchestrator's per-command tracked-IO slot
//! and snapshot cache are not raced by concurrent clients. This is sufficient
//! for the Phase-1 milestone; finer-grained concurrency is a later phase.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream;
use tokio::sync::Notify;

use rush_command_core::{CommandError, CommandSemantics, CommandService, RunEventStream};
use rush_protocol::command::v1::diagnostic::Severity;
use rush_protocol::command::v1::run_event::Event;
use rush_protocol::command::v1::{
    CancelRequest, CancelResponse, CommandFinished, Diagnostic, PingRequest, PingResponse,
    RunEvent, RunRequest, ShutdownRequest, ShutdownResponse,
};

use crate::BuildOrchestrator;

/// `EX_USAGE` from `sysexits.h` — command-line usage error (bad/empty request).
const EX_USAGE: i32 = 64;
/// `EX_SOFTWARE` from `sysexits.h` — the build raised an internal error.
const EX_SOFTWARE: i32 = 70;

/// The build-aware [`CommandService`]. Holds the persistent orchestrator whose
/// snapshot cache survives across requests.
pub struct BuildCommandService {
    /// The long-lived orchestrator. Its [`SnapshotCache`] +
    /// [`ReuseCounters`] are `Arc` fields shared across `Clone`, so cloning to
    /// run a command still hits the SAME retained graph cache — the reuse engine.
    orchestrator: Arc<BuildOrchestrator>,
    /// Daemon start instant (drives `ping` uptime).
    started: Instant,
    /// Number of `Run` commands that actually executed a build (monotonic).
    builds_executed: AtomicU64,
    /// Serializes commands: one build at a time (the orchestrator's shared
    /// tracked-IO slot + snapshot cache must not be raced).
    command_lock: tokio::sync::Mutex<()>,
    /// Signalled when a client requests shutdown; the serve loop awaits it.
    shutdown: Arc<Notify>,
    /// Seconds-since-start of the LAST client activity (any RPC). Drives the
    /// daemon's true inactivity-reset idle timeout: the serve loop shuts the
    /// daemon down only after `idle_timeout` seconds with no activity, so an
    /// actively-used always-on daemon never dies mid-session. Shared (`Arc`)
    /// so the serve loop reads the same counter the service bumps.
    last_activity: Arc<AtomicU64>,
    /// Long-lived build executor for the daemon. The build future is `!Send`, so
    /// it still runs on a dedicated OS thread, but the expensive Tokio runtime is
    /// created once instead of once per client command.
    build_worker: std::sync::Mutex<Option<mpsc::Sender<BuildJob>>>,
}

struct BuildJob {
    orchestrator: BuildOrchestrator,
    target_patterns: Vec<String>,
    semantics: CommandSemantics,
    result_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
}

impl BuildCommandService {
    /// Build the service over a persistent orchestrator and a shutdown notifier.
    pub fn new(orchestrator: Arc<BuildOrchestrator>, shutdown: Arc<Notify>) -> Self {
        Self {
            orchestrator,
            started: Instant::now(),
            builds_executed: AtomicU64::new(0),
            command_lock: tokio::sync::Mutex::new(()),
            shutdown,
            last_activity: Arc::new(AtomicU64::new(0)),
            build_worker: std::sync::Mutex::new(spawn_build_worker()),
        }
    }

    /// Number of builds executed so far.
    pub fn builds_executed(&self) -> u64 {
        self.builds_executed.load(Ordering::Relaxed)
    }

    /// Seconds since the daemon started.
    pub fn uptime_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    /// A handle to the shared last-activity counter (seconds-since-start of the
    /// most recent client RPC). The serve loop polls this to implement a true
    /// inactivity-reset idle timeout. Cloning is cheap (`Arc`).
    pub fn last_activity_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.last_activity)
    }

    /// Record that a client RPC just touched the daemon (resets the idle clock).
    fn mark_activity(&self) {
        self.last_activity
            .store(self.started.elapsed().as_secs(), Ordering::Relaxed);
    }

    fn submit_build_job(&self, job: BuildJob) -> Result<(), BuildJob> {
        let mut guard = self.build_worker.lock().unwrap_or_else(|p| p.into_inner());
        let Some(tx) = guard.as_ref() else {
            return Err(job);
        };
        match tx.send(job) {
            Ok(()) => Ok(()),
            Err(err) => {
                *guard = None;
                Err(err.0)
            }
        }
    }
}

fn spawn_build_worker() -> Option<mpsc::Sender<BuildJob>> {
    let (tx, rx) = mpsc::channel::<BuildJob>();
    let spawn = std::thread::Builder::new()
        .name("rushd-build-worker".to_string())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    while let Ok(job) = rx.recv() {
                        let _ = job
                            .result_tx
                            .send(Err(format!("failed to build command runtime: {e}")));
                    }
                    return;
                }
            };

            while let Ok(job) = rx.recv() {
                run_build_job_on_runtime(&rt, job);
            }
        });

    match spawn {
        Ok(_) => Some(tx),
        Err(e) => {
            eprintln!("⚠️  failed to spawn persistent daemon build worker: {e}");
            None
        }
    }
}

fn run_build_job_on_runtime(rt: &tokio::runtime::Runtime, job: BuildJob) {
    let BuildJob {
        orchestrator,
        target_patterns,
        semantics,
        result_tx,
    } = job;
    let res = rt.block_on(async move {
        let out = orchestrator
            .build_with_dependencies_with_semantics(target_patterns, semantics)
            .await;
        // Write the Chrome trace HERE. The CLI writes it at its completion
        // banner, which the daemon never prints - so RUSH_BUILD_TRACE
        // produced nothing at all for daemon builds, which is every build
        // the benchmark measures.
        orchestrator.write_build_trace_if_enabled();
        // Enforce the disk cache's own size limit. CacheConfig sets one
        // (10 GB by default) and DiskCache::gc() implements eviction, but
        // nothing ever called it: on this host the cache reached 433 GB,
        // 43x its configured limit, filled the disk and aborted a corpus
        // sweep. A limit that is never enforced is not a limit.
        orchestrator.gc_disk_cache_if_needed().await;
        out
    });
    let _ = result_tx.send(res.map_err(|e| format!("{e:#}")));
}

fn spawn_one_shot_build_thread(job: BuildJob) -> Result<(), String> {
    std::thread::Builder::new()
        .name("rushd-build".to_string())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = job
                        .result_tx
                        .send(Err(format!("failed to build command runtime: {e}")));
                    return;
                }
            };
            run_build_job_on_runtime(&rt, job);
        })
        .map(|_| ())
        .map_err(|e| format!("failed to spawn build thread: {e}"))
}

/// Extract the ordered target patterns from the client argv.
///
/// argv is `[program, verb, <targets...>, <flags...>]` interleaved. A target
/// pattern is any positional token (not starting with `-`) after the verb
/// (argv[1]). Flag VALUES of the space-separated form (e.g. `--define VALUE`)
/// must NOT be mistaken for targets, so we skip the token following a
/// known value-taking flag. This mirrors how the in-process CLI collects
/// positional target patterns.
fn target_patterns_from_argv(argv: &[Bytes]) -> Vec<String> {
    // Flags that consume the following argv token as their value (space form).
    // `--define X` is the common one in the TF build invocations.
    const VALUE_FLAGS: &[&str] = &[
        "--define",
        "--output_base",
        "--compilation_mode",
        "--compilation-mode",
        "--cpu",
        "--apple_platform_type",
        "--crosstool_top",
        "--copt",
        "--features",
        "--strip",
        "-c",
        "--jobs",
        "-j",
        "--config",
        "--daemon",
        "--watchfs",
    ];

    let mut out = Vec::new();
    let mut idx = 2usize; // skip program (0) + verb (1)
    while idx < argv.len() {
        let tok = &argv[idx];
        let tok_str = String::from_utf8_lossy(tok);
        if tok_str.starts_with('-') {
            // `--flag=value` is self-contained; `--flag value` eats the next tok.
            if !tok_str.contains('=') && VALUE_FLAGS.iter().any(|f| *f == tok_str) {
                idx += 1; // skip the value token
            }
            idx += 1;
            continue;
        }
        out.push(tok_str.into_owned());
        idx += 1;
    }
    out
}

#[async_trait]
impl CommandService for BuildCommandService {
    async fn run(&self, request: RunRequest) -> RunEventStream {
        // Touch the idle clock: any client run keeps the daemon alive.
        self.mark_activity();
        // Reconstruct the CLIENT's per-command semantics from the wire request.
        // This is the correctness premise: the daemon uses the client's
        // argv/env/cwd/workspace-config, never its own process globals.
        let semantics = match CommandSemantics::from_run_request(&request) {
            Ok(s) => s,
            Err(e) => return Box::pin(stream::iter(vec![Err(e)])),
        };

        // Only the `build` verb is wired through the orchestrator here. Other
        // verbs (test/query/…) are honestly unimplemented in the daemon path
        // for now — report it, do NOT fake a success.
        if semantics.command() != Some("build") {
            let verb = semantics
                .command()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "<none>".to_string());
            let events = vec![
                Ok(diagnostic_error(format!(
                    "daemon run: only the 'build' verb is wired today (got {verb:?})"
                ))),
                Ok(finished(EX_USAGE)),
            ];
            return Box::pin(stream::iter(events));
        }

        let target_patterns = target_patterns_from_argv(semantics.argv());
        if target_patterns.is_empty() {
            let events = vec![
                Ok(diagnostic_error(
                    "daemon run: no target patterns in request argv".to_string(),
                )),
                Ok(finished(EX_USAGE)),
            ];
            return Box::pin(stream::iter(events));
        }

        // One command at a time. The shared orchestrator's tracked-IO slot +
        // snapshot cache are not safe to race; serialize. (Internal build
        // parallelism is retained INSIDE build_with_dependencies_with_semantics.)
        let _guard = self.command_lock.lock().await;

        // The orchestrator clones cheaply — every Arc field (notably the
        // snapshot_cache + reuse_counters) is SHARED, so this clone hits the
        // SAME retained graph cache as the previous request. That is the
        // cross-request reuse: request 1 commits a snapshot into the shared
        // cache; request 2 (a fresh CLIENT PROCESS) finds it Fresh and skips
        // all analysis.
        let orchestrator = (*self.orchestrator).clone();

        // The orchestrator build future is `!Send` (it holds boxed non-Send
        // futures across `.await`, e.g. the recursive WORKSPACE loader), so it
        // cannot be held across the `async_trait` `run` boundary, whose future
        // must be `Send`. Drive it on the daemon's dedicated build worker; the
        // `run` future only awaits the `Send` oneshot carrying the result. The
        // command lock above guarantees one command at a time. Internal build
        // parallelism is unaffected (it uses the orchestrator's own worker pool /
        // rayon, not this dispatch thread).
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        let job = BuildJob {
            orchestrator,
            target_patterns,
            semantics,
            result_tx: tx,
        };
        let submitted = match self.submit_build_job(job) {
            Ok(()) => Ok(()),
            Err(job) => spawn_one_shot_build_thread(job),
        };

        if let Err(e) = submitted {
            let events = vec![
                Ok(diagnostic_error(format!(
                    "daemon run: failed to dispatch build: {e}"
                ))),
                Ok(finished(EX_SOFTWARE)),
            ];
            return Box::pin(stream::iter(events));
        }

        let result: Result<(), String> = match rx.await {
            Ok(r) => r,
            Err(_) => Err("build thread terminated without a result".to_string()),
        };

        let mut events: Vec<Result<RunEvent, CommandError>> = Vec::new();

        match result {
            Ok(()) => {
                self.builds_executed.fetch_add(1, Ordering::Relaxed);
                events.push(Ok(finished(0)));
            }
            Err(e) => {
                // The build failed: surface the real error as a diagnostic and
                // finish with a non-zero code. No fake success.
                self.builds_executed.fetch_add(1, Ordering::Relaxed);
                events.push(Ok(diagnostic_error(format!("build failed: {e}"))));
                events.push(Ok(finished(EX_SOFTWARE)));
            }
        }

        Box::pin(stream::iter(events))
    }

    async fn cancel(&self, _request: CancelRequest) -> CancelResponse {
        // In-flight cancellation is a later brick (the build runs to completion
        // under the command lock today). Honestly report "not cancelled".
        CancelResponse { cancelled: false }
    }

    async fn ping(&self, _request: PingRequest) -> PingResponse {
        // A ping is client activity too (a connecting client liveness-checks
        // before each build), so it resets the idle clock.
        self.mark_activity();
        PingResponse {
            uptime_secs: self.uptime_secs(),
            builds_executed: self.builds_executed(),
        }
    }

    async fn shutdown(&self, _request: ShutdownRequest) -> ShutdownResponse {
        // Signal the serve loop to begin orderly teardown, then ack.
        self.shutdown.notify_one();
        ShutdownResponse { ack: true }
    }
}

/// An ERROR-severity diagnostic event carrying `message`.
fn diagnostic_error(message: String) -> RunEvent {
    RunEvent {
        event: Some(Event::Diagnostic(Diagnostic {
            severity: Severity::Error as i32,
            message: Bytes::from(message.into_bytes()),
            location: String::new(),
        })),
    }
}

/// A `CommandFinished` event carrying `exit_code`.
fn finished(exit_code: i32) -> RunEvent {
    RunEvent {
        event: Some(Event::Finished(CommandFinished {
            exit_code,
            command_id: Bytes::new(),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(s: &str) -> Bytes {
        Bytes::from(s.as_bytes().to_vec())
    }

    #[test]
    fn extracts_targets_skipping_flags_and_value_flags() {
        // rush build //a:b //c:d --define X=1 --define Y --config opt
        // Targets are //a:b and //c:d; the space-form `--define Y` value `Y`
        // and `--config opt` value `opt` must NOT be captured as targets.
        let argv = vec![
            b("rush"),
            b("build"),
            b("//a:b"),
            b("//c:d"),
            b("--define=X=1"),
            b("--config"),
            b("opt"),
        ];
        let got = target_patterns_from_argv(&argv);
        assert_eq!(got, vec!["//a:b".to_string(), "//c:d".to_string()]);
    }

    #[test]
    fn space_form_define_value_not_a_target() {
        // `--define USE_MOLD=true` (space form): the value must be skipped.
        let argv = vec![
            b("rush"),
            b("build"),
            b("//tensorflow/core:framework"),
            b("--define"),
            b("framework_shared_object=false"),
        ];
        let got = target_patterns_from_argv(&argv);
        assert_eq!(got, vec!["//tensorflow/core:framework".to_string()]);
    }

    #[test]
    fn native_option_values_are_not_targets() {
        let argv = ["rush", "build", "--cpu", "aarch64", "--apple_platform_type", "ios",
            "--crosstool_top", "//tools:custom", "--copt", "-DVALUE", "//app:root"]
            .map(b).to_vec();
        assert_eq!(target_patterns_from_argv(&argv), ["//app:root"]);
    }

    #[test]
    fn no_targets_yields_empty() {
        let argv = vec![b("rush"), b("build"), b("--define=X=1")];
        assert!(target_patterns_from_argv(&argv).is_empty());
    }
}
