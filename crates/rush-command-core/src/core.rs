//! [`CoreService`] — the real, stateless-per-command [`CommandService`] impl.
//!
//! It holds only *daemon-lifetime* state (start instant + a builds-executed
//! counter). `ping` and `shutdown` are fully implemented. `run` is **honestly
//! unwired**: it does NOT fabricate a build (no sleep-then-fake-success — that was
//! the prior `daemon.rs` stub this design replaces). Instead it emits a real
//! error [`Diagnostic`] and a `CommandFinished{exit_code=70}` (EX_SOFTWARE), then
//! ends the stream, and does not bump the builds-executed counter (no build ran).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream;

use rush_protocol::command::v1::diagnostic::Severity;
use rush_protocol::command::v1::run_event::Event;
use rush_protocol::command::v1::{
    CancelRequest, CancelResponse, CommandFinished, Diagnostic, PingRequest, PingResponse,
    RunEvent, RunRequest, ShutdownRequest, ShutdownResponse,
};

use crate::error::CommandError;
use crate::semantics::CommandSemantics;
use crate::service::{CommandService, RunEventStream};

/// `EX_SOFTWARE` from `sysexits.h` — "internal software error". Used as the exit
/// code for the honestly-unwired `run` path.
const EX_SOFTWARE: i32 = 70;

/// Daemon-lifetime state behind the [`CommandService`] impl.
///
/// Cheap to construct; safe to share behind an `Arc`. Per-command state lives in
/// the [`CommandSemantics`] derived inside `run`, never on this struct.
pub struct CoreService {
    started: Instant,
    builds_executed: AtomicU64,
}

impl CoreService {
    /// Create a fresh service. `started` is captured now and drives `ping`'s
    /// uptime.
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            builds_executed: AtomicU64::new(0),
        }
    }

    /// Number of builds executed so far (monotonic).
    pub fn builds_executed(&self) -> u64 {
        self.builds_executed.load(Ordering::Relaxed)
    }

    /// Seconds elapsed since the daemon started.
    pub fn uptime_secs(&self) -> u64 {
        self.started.elapsed().as_secs()
    }
}

impl Default for CoreService {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CommandService for CoreService {
    async fn run(&self, request: RunRequest) -> RunEventStream {
        // Build the per-command context up front. Even though the build itself is
        // not wired, validating the request here is real work and exercises the
        // CommandSemantics path that future builds will depend on.
        let events: Vec<Result<RunEvent, CommandError>> =
            match CommandSemantics::from_run_request(&request) {
                Ok(_semantics) => {
                    // Honest unwired path: report the real reason and finish with
                    // EX_SOFTWARE. We do NOT increment builds_executed because no
                    // build ran.
                    vec![
                        Ok(RunEvent {
                            event: Some(Event::Diagnostic(Diagnostic {
                                severity: Severity::Error as i32,
                                message: Bytes::from_static(
                                    b"run not yet wired to the build orchestrator",
                                ),
                                location: String::new(),
                            })),
                        }),
                        Ok(RunEvent {
                            event: Some(Event::Finished(CommandFinished {
                                exit_code: EX_SOFTWARE,
                                command_id: Bytes::new(),
                            })),
                        }),
                    ]
                }
                Err(e) => vec![Err(e)],
            };

        Box::pin(stream::iter(events))
    }

    async fn cancel(&self, _request: CancelRequest) -> CancelResponse {
        // No command machinery to cancel yet (run is unwired). Honestly report
        // "nothing was cancelled" rather than claiming a phantom cancellation.
        CancelResponse { cancelled: false }
    }

    async fn ping(&self, _request: PingRequest) -> PingResponse {
        PingResponse {
            uptime_secs: self.uptime_secs(),
            builds_executed: self.builds_executed(),
        }
    }

    async fn shutdown(&self, _request: ShutdownRequest) -> ShutdownResponse {
        // Acknowledge the request. Actual loop teardown is the transport layer's
        // job (a later brick); the service-layer contract is just the ack.
        ShutdownResponse { ack: true }
    }
}
