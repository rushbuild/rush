//! [`GrpcClient`] — a thin gRPC client that connects to the daemon over a
//! Unix-domain socket and exposes `ping`/`run`/`cancel`/`shutdown`.
//!
//! tonic's `Channel` is HTTP/2-over-some-transport; for UDS we hand it a custom
//! connector (`tower::service_fn`) that dials the socket and wraps the resulting
//! `tokio::net::UnixStream` in `hyper_util`'s Tokio IO adapter. The HTTP/2
//! authority is a dummy (`http://[::]:50051`) — it is never resolved because the
//! connector ignores it and always dials our socket.

use std::path::{Path, PathBuf};

use futures::Stream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

use rush_protocol::command::v1::{
    CancelRequest, CancelResponse, PingRequest, PingResponse, RunEvent, RunRequest,
    ShutdownRequest,
};

use crate::cookie::AuthCookie;
use crate::error::TransportError;
use crate::metadata::DaemonMetadata;
use crate::pb::rush_command_server_client::RushCommandServerClient;

/// A connected client to a rush daemon over gRPC/UDS.
pub struct GrpcClient {
    inner: RushCommandServerClient<Channel>,
    cookie: AuthCookie,
}

impl GrpcClient {
    /// Connect to the daemon at `socket_path`, presenting `cookie` on requests.
    pub async fn connect_unix(
        socket_path: impl AsRef<Path>,
        cookie: AuthCookie,
    ) -> Result<Self, TransportError> {
        let path: PathBuf = socket_path.as_ref().to_path_buf();

        // The URI is a placeholder; the connector below ignores it and always
        // dials the Unix socket.
        let endpoint = Endpoint::try_from("http://[::]:50051")
            .map_err(TransportError::from)?;

        let channel = endpoint
            .connect_with_connector(service_fn(move |_: Uri| {
                let path = path.clone();
                async move {
                    let stream = tokio::net::UnixStream::connect(path).await?;
                    Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(stream))
                }
            }))
            .await
            .map_err(TransportError::from)?;

        Ok(GrpcClient {
            inner: RushCommandServerClient::new(channel),
            cookie,
        })
    }

    /// Discover the daemon from its metadata file and connect.
    ///
    /// Reads the metadata, resolves the Unix-socket endpoint, loads the cookie
    /// from the recorded cookie file, and connects.
    pub async fn discover(metadata_path: impl AsRef<Path>) -> Result<Self, TransportError> {
        let meta = DaemonMetadata::read(metadata_path.as_ref())?;
        let endpoint = meta.local_endpoint()?;
        let cookie = AuthCookie::read_from(&meta.cookie_path())?;
        match endpoint {
            #[cfg(unix)]
            crate::endpoint::LocalEndpoint::UnixSocket(path) => {
                Self::connect_unix(path, cookie).await
            }
            other => Err(TransportError::Discovery(format!(
                "metadata endpoint {other:?} has no client backend yet"
            ))),
        }
    }

    /// Liveness probe. Carries no cookie (matches the server's unauthenticated
    /// `Ping`). Returns the daemon's reported uptime + build counter.
    pub async fn ping(&mut self) -> Result<PingResponse, TransportError> {
        let resp = self.inner.ping(PingRequest {}).await?;
        Ok(resp.into_inner())
    }

    /// Start a command, returning the server-streamed event sequence. The
    /// caller's cookie is injected into the request.
    pub async fn run(
        &mut self,
        mut request: RunRequest,
    ) -> Result<impl Stream<Item = Result<RunEvent, tonic::Status>>, TransportError> {
        request.auth_cookie = bytes::Bytes::copy_from_slice(self.cookie.as_bytes());
        let resp = self.inner.run(request).await?;
        Ok(resp.into_inner())
    }

    /// Request cancellation of an in-flight command. The cookie is injected.
    pub async fn cancel(
        &mut self,
        mut request: CancelRequest,
    ) -> Result<CancelResponse, TransportError> {
        request.auth_cookie = bytes::Bytes::copy_from_slice(self.cookie.as_bytes());
        let resp = self.inner.cancel(request).await?;
        Ok(resp.into_inner())
    }

    /// Ask the daemon to shut down. The cookie is injected.
    pub async fn shutdown(&mut self) -> Result<bool, TransportError> {
        let req = ShutdownRequest {
            auth_cookie: bytes::Bytes::copy_from_slice(self.cookie.as_bytes()),
        };
        let resp = self.inner.shutdown(req).await?;
        Ok(resp.into_inner().ack)
    }
}
