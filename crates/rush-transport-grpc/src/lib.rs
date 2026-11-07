//! `rush-transport-grpc` — the tonic/gRPC-over-Unix-domain-socket transport for
//! the rush persistent daemon.
//!
//! This crate makes a resident daemon REACHABLE. It provides:
//!
//! * the tonic service stubs (client + server), generated at build time from the
//!   same `rush_command_v1.proto` `rush-protocol` uses for its messages (the
//!   messages are reused via `extern_path`, so `rush-protocol` stays tonic-free);
//! * [`serve_unix`] — a server adapter that hosts any `dyn CommandService`
//!   (e.g. [`rush_command_core::CoreService`]) over gRPC on a Unix-domain socket;
//! * [`GrpcClient`] — a thin client exposing `ping`/`run`/`cancel`/`shutdown`;
//! * [`AuthCookie`] — a random per-daemon cookie stored with 0600 perms in a
//!   0700 user-private dir, verified on every request before any expensive work;
//! * [`DaemonMetadata`] — the JSON discovery file (atomic write + rename) and the
//!   client-side discovery helper;
//! * [`LocalEndpoint`] — the (future-portable) endpoint enum; only the Unix-socket
//!   backend has a real implementation today.
//!
//! Scope guard: this crate is additive. It is NOT wired into the `rush` CLI and
//! changes no existing `rush build` behavior.
//!
//! Deferred (per the design doc): the raw-framed-protobuf challenger transport,
//! real Windows named-pipe / TCP server backends, and routing `rush build`
//! through the daemon.

mod client;
mod cookie;
mod endpoint;
mod error;
mod metadata;
mod server;

/// tonic-generated gRPC stubs for `rush.command.v1`.
///
/// Only the service traits and client/server stubs are generated here; the
/// message types are re-exported from [`rush_protocol::command::v1`] (see
/// `build.rs`).
pub mod pb {
    tonic::include_proto!("rush.command.v1");
}

pub use client::GrpcClient;
pub use cookie::AuthCookie;
pub use endpoint::LocalEndpoint;
pub use error::TransportError;
pub use metadata::{DaemonMetadata, TRANSPORT_UNIX_SOCKET};
pub use server::{serve_unix, serve_unix_with_shutdown, DaemonHandle, GrpcAdapter};
