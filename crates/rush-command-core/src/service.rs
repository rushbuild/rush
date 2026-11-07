//! The transport-independent [`CommandService`] trait.
//!
//! This is the single seam both daemon transports call: the tonic/gRPC adapter
//! and the framed-protobuf challenger. **The build logic must never see the
//! socket** — it sees only this trait and the [`rush_protocol`] message types.

use std::pin::Pin;

use async_trait::async_trait;
use futures::Stream;

use rush_protocol::command::v1::{
    CancelRequest, CancelResponse, PingRequest, PingResponse, RunEvent, RunRequest,
    ShutdownRequest, ShutdownResponse,
};

use crate::error::CommandError;

/// The ordered, server-streamed event sequence produced by [`CommandService::run`].
///
/// `Pin<Box<dyn Stream + Send>>` is the object-safe shape so the trait stays
/// usable through `dyn CommandService`; concrete impls may build the stream any
/// way they like (`futures::stream::iter`, a channel receiver, etc.).
pub type RunEventStream = Pin<Box<dyn Stream<Item = Result<RunEvent, CommandError>> + Send>>;

/// Daemon RPC surface, mirroring `RushCommandServer` from `rush.command.v1`, but
/// with NO transport types in the signature.
#[async_trait]
pub trait CommandService: Send + Sync {
    /// Execute a command, streaming ordered [`RunEvent`]s (started → output/
    /// progress/diagnostics → finished). The per-command context is derived from
    /// `request` (see [`CommandSemantics`](crate::CommandSemantics)) — never from
    /// the daemon's process globals.
    async fn run(&self, request: RunRequest) -> RunEventStream;

    /// Request cancellation of an in-flight command.
    async fn cancel(&self, request: CancelRequest) -> CancelResponse;

    /// Liveness + lightweight stats probe.
    async fn ping(&self, request: PingRequest) -> PingResponse;

    /// Ask the daemon to begin an orderly shutdown.
    async fn shutdown(&self, request: ShutdownRequest) -> ShutdownResponse;
}
