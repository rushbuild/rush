//! [`LocalEndpoint`] — the future-portable daemon endpoint.
//!
//! UDS is the Unix *backend*, not the contract. The enum exists so a Windows
//! named-pipe or a loopback-TCP backend can be added later without changing the
//! discovery/metadata format. Only the Unix-socket backend has a real impl today;
//! the other variants are declared (cfg-gated) but NOT given fake bodies (per the
//! NO-STUBS policy).

use std::net::SocketAddr;
use std::path::PathBuf;

/// Where a daemon listens. Platform backends differ; discovery is uniform.
#[derive(Debug, Clone)]
pub enum LocalEndpoint {
    /// A Unix-domain socket at the given filesystem path (Linux/macOS/BSD).
    #[cfg(unix)]
    UnixSocket(PathBuf),

    /// A Windows named pipe (e.g. `\\.\pipe\rush-<hash>`). Not implemented yet.
    #[cfg(windows)]
    NamedPipe(String),

    /// A loopback TCP address — the universal diagnostic fallback / Windows
    /// default. Not implemented as a server backend yet.
    Tcp(SocketAddr),
}

/// Maximum bytes addressable by `sockaddr_un.sun_path` on Linux, including the
/// trailing NUL. Conservative across common platforms (macOS is 104).
pub const UNIX_SUN_PATH_MAX: usize = 108;

impl LocalEndpoint {
    /// Validate that a Unix-socket path fits the platform `sun_path` limit.
    ///
    /// Returns the limit-aware error rather than letting `bind()` fail with an
    /// opaque `EINVAL`. The check counts bytes (paths are not required to be
    /// UTF-8) and reserves one byte for the terminating NUL.
    #[cfg(unix)]
    pub fn validate_unix_path(path: &std::path::Path) -> Result<(), crate::TransportError> {
        use std::os::unix::ffi::OsStrExt;
        let len = path.as_os_str().as_bytes().len();
        // sun_path must hold the path plus a NUL terminator.
        if len + 1 > UNIX_SUN_PATH_MAX {
            return Err(crate::TransportError::SocketPathTooLong {
                path: path.display().to_string(),
                len,
                limit: UNIX_SUN_PATH_MAX - 1,
            });
        }
        Ok(())
    }

    /// The transport tag written into the discovery metadata file.
    pub fn transport_tag(&self) -> &'static str {
        match self {
            #[cfg(unix)]
            LocalEndpoint::UnixSocket(_) => crate::metadata::TRANSPORT_UNIX_SOCKET,
            #[cfg(windows)]
            LocalEndpoint::NamedPipe(_) => "named_pipe",
            LocalEndpoint::Tcp(_) => "tcp",
        }
    }

    /// The endpoint rendered as the metadata file's `endpoint` string.
    pub fn endpoint_string(&self) -> String {
        match self {
            #[cfg(unix)]
            LocalEndpoint::UnixSocket(p) => p.display().to_string(),
            #[cfg(windows)]
            LocalEndpoint::NamedPipe(s) => s.clone(),
            LocalEndpoint::Tcp(a) => a.to_string(),
        }
    }
}
