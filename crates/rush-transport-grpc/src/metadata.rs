//! [`DaemonMetadata`] — the daemon discovery file.
//!
//! A client never hardcodes a `.sock` path; it reads this JSON file to learn the
//! protocol version, transport, endpoint, and the cookie-file location. The file
//! is written atomically (temp + rename) so a client either sees a complete,
//! consistent record or the previous one — never a torn write.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::endpoint::LocalEndpoint;
use crate::error::TransportError;

/// The transport tag for the Unix-domain-socket backend.
pub const TRANSPORT_UNIX_SOCKET: &str = "unix_socket";

/// The JSON record a daemon publishes and a client discovers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonMetadata {
    /// `"major.minor"` of the protocol the daemon speaks.
    pub protocol_version: String,
    /// Transport tag, e.g. `"unix_socket"`.
    pub transport: String,
    /// Endpoint string: the socket path for `unix_socket`.
    pub endpoint: String,
    /// Path to the file holding the daemon's auth cookie (0600).
    pub cookie_file: String,
    /// PID of the daemon process (for owner/staleness diagnostics).
    pub pid: u32,
}

impl DaemonMetadata {
    /// Build a metadata record for a running daemon.
    pub fn new(endpoint: &LocalEndpoint, cookie_file: &Path, major: u32, minor: u32) -> Self {
        DaemonMetadata {
            protocol_version: format!("{major}.{minor}"),
            transport: endpoint.transport_tag().to_string(),
            endpoint: endpoint.endpoint_string(),
            cookie_file: cookie_file.display().to_string(),
            pid: std::process::id(),
        }
    }

    /// Write the metadata atomically to `path` (temp file + rename).
    pub fn write_atomic(&self, path: &Path) -> Result<(), TransportError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(self)?;

        let mut tmp = path.as_os_str().to_os_string();
        tmp.push(format!(".tmp.{}", std::process::id()));
        let tmp = PathBuf::from(tmp);

        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&json)?;
            f.flush()?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Read and parse a metadata file.
    pub fn read(path: &Path) -> Result<Self, TransportError> {
        let bytes = fs::read(path).map_err(|e| {
            TransportError::Discovery(format!("cannot read metadata {}: {e}", path.display()))
        })?;
        let meta: DaemonMetadata = serde_json::from_slice(&bytes)?;
        Ok(meta)
    }

    /// Reconstruct the [`LocalEndpoint`] from the recorded transport/endpoint.
    pub fn local_endpoint(&self) -> Result<LocalEndpoint, TransportError> {
        match self.transport.as_str() {
            #[cfg(unix)]
            TRANSPORT_UNIX_SOCKET => Ok(LocalEndpoint::UnixSocket(PathBuf::from(&self.endpoint))),
            "tcp" => {
                let addr = self.endpoint.parse().map_err(|e| {
                    TransportError::Discovery(format!("bad tcp endpoint {}: {e}", self.endpoint))
                })?;
                Ok(LocalEndpoint::Tcp(addr))
            }
            other => Err(TransportError::Discovery(format!(
                "unsupported transport {other:?} in metadata"
            ))),
        }
    }

    /// The cookie-file path as a [`PathBuf`].
    pub fn cookie_path(&self) -> PathBuf {
        PathBuf::from(&self.cookie_file)
    }
}
