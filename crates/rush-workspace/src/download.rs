//! Common download utilities.

use anyhow::Result;
use std::path::Path;

/// Download progress callback.
pub type ProgressCallback = Box<dyn Fn(u64, u64) + Send + Sync>;

/// Download with progress reporting.
pub async fn download_with_progress(
    url: &str,
    dest: &Path,
    _progress: Option<ProgressCallback>,
) -> Result<()> {
    anyhow::bail!(
        "download_with_progress not yet implemented.\n\
         \n\
         This requires: HTTP client with progress reporting, chunked downloads,\n\
         and resume support for large files.\n\
         \n\
         URL: {}\n\
         Destination: {}",
        url,
        dest.display()
    )
}

/// Verify file integrity.
pub async fn verify_integrity(path: &Path, hash: &str, algorithm: HashAlgorithm) -> Result<bool> {
    anyhow::bail!(
        "verify_integrity not yet implemented.\n\
         \n\
         This requires: {:?} hash computation and comparison.\n\
         \n\
         File: {}\n\
         Expected hash: {}",
        algorithm,
        path.display(),
        hash
    )
}

/// Hash algorithm for verification.
#[derive(Debug, Clone, Copy)]
pub enum HashAlgorithm {
    Sha256,
    Sha512,
    Blake3,
}
