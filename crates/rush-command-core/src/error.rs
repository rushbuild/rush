//! Error channel for the [`CommandService`](crate::CommandService) trait.

use thiserror::Error;

/// Errors surfaced through the `CommandService` API.
///
/// These are *service-layer* errors (transport-independent). A transport adapter
/// (tonic, framed) maps these onto its own wire error representation; the build
/// logic only ever sees this enum.
#[derive(Debug, Error)]
pub enum CommandError {
    /// The client spoke a protocol major version this daemon does not implement.
    #[error("protocol version mismatch: client wants v{client_major}.{client_minor}, daemon speaks v{daemon_major}.{daemon_minor}")]
    ProtocolMismatch {
        client_major: u32,
        client_minor: u32,
        daemon_major: u32,
        daemon_minor: u32,
    },

    /// The request was malformed in a way that prevents building a
    /// [`CommandSemantics`](crate::CommandSemantics) (e.g. empty argv, a working
    /// directory that is not a valid path).
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// `Run` reached the service layer but the build orchestrator is not yet wired
    /// up. This is an *honest* not-implemented marker — it is NOT a fake build
    /// result. The `run` path that returns this must never pretend a build
    /// happened (no sleep-then-success).
    #[error("run not yet wired to the build orchestrator")]
    NotWired,

    /// The daemon is shutting down and cannot accept new work.
    #[error("daemon is shutting down")]
    ShuttingDown,
}
