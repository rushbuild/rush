//! Transport-layer error type.

use thiserror::Error;

/// Errors raised by the gRPC-over-UDS transport (server bring-up, client
/// connection, cookie handling, metadata discovery).
///
/// Service-layer errors (`rush_command_core::CommandError`) are distinct: those
/// describe what the build logic decided; these describe what the wire/socket
/// did.
#[derive(Debug, Error)]
pub enum TransportError {
    /// The natural Unix-domain socket path exceeds the platform `sun_path` limit
    /// (~108 bytes on Linux). Carries the offending path and the limit.
    #[error("unix socket path too long ({len} bytes, limit {limit}): {path}")]
    SocketPathTooLong {
        path: String,
        len: usize,
        limit: usize,
    },

    /// A request arrived with a missing or incorrect auth cookie.
    #[error("authentication failed: {0}")]
    Auth(String),

    /// The daemon metadata file could not be read, parsed, or was for an
    /// incompatible protocol.
    #[error("daemon discovery failed: {0}")]
    Discovery(String),

    /// An I/O error (binding the socket, creating the private dir, etc.).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A tonic transport error (connect, serve).
    #[error("grpc transport error: {0}")]
    Tonic(#[from] tonic::transport::Error),

    /// A gRPC status returned by the server for a unary/streaming call.
    #[error("grpc status: {0}")]
    Status(#[from] tonic::Status),

    /// JSON (de)serialization of the metadata file failed.
    #[error("metadata serialization error: {0}")]
    Json(#[from] serde_json::Error),
}
