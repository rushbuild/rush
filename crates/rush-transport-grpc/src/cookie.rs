//! [`AuthCookie`] — a random per-daemon secret used to authenticate clients.
//!
//! A resident daemon on a Unix-domain socket is reachable by any local process
//! that can open the socket path. The cookie raises the bar to "can read a file
//! in the daemon owner's private directory" — the same scheme Bazel's
//! `CommandServer` uses. The socket also lives in a 0700 dir, but the cookie
//! defends in depth and is the only check that survives the loopback-TCP backend.
//!
//! The comparison is constant-time to avoid leaking the cookie via timing.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use rand::RngCore;

use crate::error::TransportError;

/// Number of random bytes in a cookie. 32 bytes = 256 bits of entropy.
pub const COOKIE_LEN: usize = 32;

/// A random authentication cookie held by the daemon and presented by clients.
#[derive(Clone)]
pub struct AuthCookie {
    bytes: Vec<u8>,
}

impl AuthCookie {
    /// Generate a fresh random cookie from the OS CSPRNG.
    pub fn generate() -> Self {
        let mut bytes = vec![0u8; COOKIE_LEN];
        rand::thread_rng().fill_bytes(&mut bytes);
        AuthCookie { bytes }
    }

    /// Construct from existing bytes (e.g. read back from disk by a client).
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        AuthCookie {
            bytes: bytes.into(),
        }
    }

    /// The raw cookie bytes (to place in a request's `auth_cookie` field).
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Persist the cookie to `path` with `0600` perms, ensuring the parent dir is
    /// `0700`. Written atomically (temp file + rename) so a reader never sees a
    /// half-written cookie.
    pub fn write_to(&self, path: &Path) -> Result<(), TransportError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
            restrict_dir(parent)?;
        }

        let tmp = temp_sibling(path);
        {
            let mut f = fs::File::create(&tmp)?;
            restrict_file(&f)?;
            f.write_all(&self.bytes)?;
            f.flush()?;
            f.sync_all()?;
        }
        // Re-assert perms on the final name after rename in case the create mode
        // was widened by umask before our fchmod (defensive).
        fs::rename(&tmp, path)?;
        let final_file = fs::OpenOptions::new().read(true).open(path)?;
        restrict_file(&final_file)?;
        Ok(())
    }

    /// Read a cookie back from `path`.
    pub fn read_from(path: &Path) -> Result<Self, TransportError> {
        let bytes = fs::read(path)?;
        if bytes.is_empty() {
            return Err(TransportError::Auth(format!(
                "cookie file {} is empty",
                path.display()
            )));
        }
        Ok(AuthCookie { bytes })
    }

    /// Constant-time comparison against a presented cookie.
    ///
    /// Returns true only if the lengths match and every byte matches. The loop
    /// does not short-circuit, so the time taken does not depend on where (or
    /// whether) the first mismatch occurs.
    pub fn verify(&self, presented: &[u8]) -> bool {
        if presented.len() != self.bytes.len() {
            return false;
        }
        let mut diff: u8 = 0;
        for (a, b) in self.bytes.iter().zip(presented.iter()) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

/// A deterministic temp sibling path next to `path` for atomic writes.
fn temp_sibling(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!(".tmp.{}", std::process::id()));
    match path.parent() {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    }
}

#[cfg(unix)]
fn restrict_dir(dir: &Path) -> Result<(), TransportError> {
    use std::os::unix::fs::PermissionsExt;
    let perms = fs::Permissions::from_mode(0o700);
    fs::set_permissions(dir, perms)?;
    Ok(())
}

#[cfg(unix)]
fn restrict_file(f: &fs::File) -> Result<(), TransportError> {
    use std::os::unix::fs::PermissionsExt;
    let perms = fs::Permissions::from_mode(0o600);
    f.set_permissions(perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn restrict_dir(_dir: &Path) -> Result<(), TransportError> {
    // Non-Unix perm hardening is handled by the (future) platform backend; the
    // cookie check itself is the portable guarantee.
    Ok(())
}

#[cfg(not(unix))]
fn restrict_file(_f: &fs::File) -> Result<(), TransportError> {
    Ok(())
}
