//! The tonic gRPC server adapter.
//!
//! [`GrpcAdapter`] implements the generated tonic service trait by delegating to
//! a `dyn CommandService` (e.g. [`rush_command_core::CoreService`]). Every RPC
//! first verifies the request's `auth_cookie` against the daemon's cookie and
//! rejects mismatches with `Status::unauthenticated` BEFORE doing any expensive
//! work. The build logic never sees the socket — only the `CommandService` trait
//! and the `rush_protocol` message types.
//!
//! [`serve_unix`] / [`serve_unix_with_shutdown`] host the adapter on a
//! Unix-domain socket via `tokio::net::UnixListener`.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use futures::{Stream, StreamExt};
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::{Request, Response, Status};

use rush_command_core::CommandService;
use rush_protocol::command::v1::{
    CancelRequest, CancelResponse, PingRequest, PingResponse, RunEvent, RunRequest,
    ShutdownRequest, ShutdownResponse,
};

use crate::cookie::AuthCookie;
use crate::endpoint::LocalEndpoint;
use crate::error::TransportError;
use crate::pb::rush_command_server_server::{RushCommandServer, RushCommandServerServer};

/// Adapter bridging tonic's generated service trait to a `dyn CommandService`.
pub struct GrpcAdapter {
    service: Arc<dyn CommandService>,
    cookie: AuthCookie,
}

impl GrpcAdapter {
    /// Wrap a service + the cookie required on every request.
    pub fn new(service: Arc<dyn CommandService>, cookie: AuthCookie) -> Self {
        GrpcAdapter { service, cookie }
    }

    /// Verify a presented cookie; map a mismatch to `unauthenticated`.
    fn check_cookie(&self, presented: &[u8]) -> Result<(), Status> {
        if self.cookie.verify(presented) {
            Ok(())
        } else {
            Err(Status::unauthenticated(
                "invalid or missing auth cookie",
            ))
        }
    }
}

/// The server-streaming type tonic expects for `Run`.
type RunStream = Pin<Box<dyn Stream<Item = Result<RunEvent, Status>> + Send>>;

#[tonic::async_trait]
impl RushCommandServer for GrpcAdapter {
    type RunStream = RunStream;

    async fn run(
        &self,
        request: Request<RunRequest>,
    ) -> Result<Response<Self::RunStream>, Status> {
        let req = request.into_inner();
        self.check_cookie(&req.auth_cookie)?;

        // Delegate to the transport-independent service. CommandError on the
        // inner stream is mapped to a gRPC Status per event.
        let inner = self.service.run(req).await;
        let mapped = inner.map(|item| {
            item.map_err(|e| Status::internal(format!("command service error: {e}")))
        });
        Ok(Response::new(Box::pin(mapped) as RunStream))
    }

    async fn cancel(
        &self,
        request: Request<CancelRequest>,
    ) -> Result<Response<CancelResponse>, Status> {
        let req = request.into_inner();
        self.check_cookie(&req.auth_cookie)?;
        Ok(Response::new(self.service.cancel(req).await))
    }

    async fn ping(
        &self,
        request: Request<PingRequest>,
    ) -> Result<Response<PingResponse>, Status> {
        // Ping carries no cookie field in the proto (it is a liveness probe with
        // no side effects and no command body); it is intentionally unauthenticated
        // so a client can cheaply detect a stale socket before reading the cookie.
        let req = request.into_inner();
        Ok(Response::new(self.service.ping(req).await))
    }

    async fn shutdown(
        &self,
        request: Request<ShutdownRequest>,
    ) -> Result<Response<ShutdownResponse>, Status> {
        let req = request.into_inner();
        self.check_cookie(&req.auth_cookie)?;
        Ok(Response::new(self.service.shutdown(req).await))
    }
}

/// A handle to a running daemon server task.
#[derive(Debug)]
pub struct DaemonHandle {
    pub endpoint: LocalEndpoint,
    socket_path: PathBuf,
    join: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
}

impl DaemonHandle {
    /// Await the server task (it runs until shutdown is signalled or it errors).
    pub async fn join(self) -> Result<(), TransportError> {
        match self.join.await {
            Ok(res) => res.map_err(TransportError::from),
            Err(e) => Err(TransportError::Discovery(format!("server task panicked: {e}"))),
        }
    }

    /// The bound socket path.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Abort the server task and remove the socket file.
    pub fn abort(self) {
        self.join.abort();
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

/// Bind a Unix-domain socket at `socket_path`, removing any stale socket first,
/// and serve `adapter` on it. Runs until the returned task ends.
///
/// Stale-socket handling: if a file exists at `socket_path`, we try to connect.
/// A successful connect means a live daemon already owns it (error out); a failed
/// connect means it is stale and safe to remove + recreate.
pub async fn serve_unix(
    adapter: GrpcAdapter,
    socket_path: impl AsRef<Path>,
) -> Result<DaemonHandle, TransportError> {
    serve_unix_inner(adapter, socket_path.as_ref(), None).await
}

/// Like [`serve_unix`] but shuts down gracefully when `shutdown` resolves.
pub async fn serve_unix_with_shutdown<F>(
    adapter: GrpcAdapter,
    socket_path: impl AsRef<Path>,
    shutdown: F,
) -> Result<DaemonHandle, TransportError>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    serve_unix_inner(adapter, socket_path.as_ref(), Some(Box::pin(shutdown))).await
}

async fn serve_unix_inner(
    adapter: GrpcAdapter,
    socket_path: &Path,
    shutdown: Option<Pin<Box<dyn std::future::Future<Output = ()> + Send>>>,
) -> Result<DaemonHandle, TransportError> {
    LocalEndpoint::validate_unix_path(socket_path)?;

    // Stale-socket detection: a connectable socket means a live owner.
    if socket_path.exists() {
        match tokio::net::UnixStream::connect(socket_path).await {
            Ok(_) => {
                return Err(TransportError::Discovery(format!(
                    "a live daemon already owns {}",
                    socket_path.display()
                )));
            }
            Err(_) => {
                // Stale: previous daemon gone. Safe to reclaim.
                std::fs::remove_file(socket_path)?;
            }
        }
    }

    let listener = UnixListener::bind(socket_path)?;
    let incoming = UnixListenerStream::new(listener);

    let server = tonic::transport::Server::builder()
        .add_service(RushCommandServerServer::new(adapter));

    let socket_path_buf = socket_path.to_path_buf();
    let join = match shutdown {
        Some(sig) => tokio::spawn(async move {
            server.serve_with_incoming_shutdown(incoming, sig).await
        }),
        None => tokio::spawn(async move { server.serve_with_incoming(incoming).await }),
    };

    Ok(DaemonHandle {
        endpoint: LocalEndpoint::UnixSocket(socket_path_buf.clone()),
        socket_path: socket_path_buf,
        join,
    })
}
