// Rush Repo - HTTP Archive Repository Rule
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! HTTP archive repository rule implementation.
//!
//! Provides functionality to download, verify, and extract tarballs from HTTP(S)
//! sources with SHA256 verification and caching.

use anyhow::{anyhow, Context, Result};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use tar::Archive;
use tracing::{debug, info, warn};

/// HTTP archive repository rule.
///
/// Downloads an archive from one or more HTTP(S) URLs, verifies its SHA256
/// checksum, extracts it to a cache directory, and optionally strips leading
/// path components.
///
/// # Example
///
/// ```no_run
/// use rush_repo::http::HttpArchive;
///
/// # async fn example() -> anyhow::Result<()> {
/// let archive = HttpArchive::new(
///     "zlib".to_string(),
///     vec!["https://zlib.net/zlib-1.3.tar.gz".to_string()],
///     "ff0ba4c292013dbc27530b3a81e1f9a813cd39de01ca5e0f8bf355702efa593e".to_string(),
///     Some("zlib-1.3".to_string()),
/// );
///
/// let extracted_path = archive.download("/tmp/rush-cache").await?;
/// println!("Extracted to: {}", extracted_path.display());
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct HttpArchive {
    /// Name of the repository.
    name: String,

    /// List of mirror URLs to try in order until one succeeds.
    urls: Vec<String>,

    /// Expected SHA256 hash of the archive (hex-encoded).
    sha256: String,

    /// Optional path prefix to strip from extracted files.
    /// For example, if the tarball contains "zlib-1.3/..." and strip_prefix
    /// is "zlib-1.3", the files will be extracted without that prefix.
    strip_prefix: Option<String>,
}

impl HttpArchive {
    /// Create a new HTTP archive repository rule.
    ///
    /// # Arguments
    ///
    /// * `name` - Repository name
    /// * `urls` - List of mirror URLs (tried in order)
    /// * `sha256` - Expected SHA256 hash (hex-encoded)
    /// * `strip_prefix` - Optional path prefix to strip from extracted files
    pub fn new(
        name: String,
        urls: Vec<String>,
        sha256: String,
        strip_prefix: Option<String>,
    ) -> Self {
        HttpArchive {
            name,
            urls,
            sha256,
            strip_prefix,
        }
    }

    /// Download and extract the archive.
    ///
    /// Tries each URL in order until one succeeds. Downloads to a temporary
    /// location, verifies the SHA256 hash, extracts the archive, and moves
    /// it to the final cache location.
    ///
    /// # Arguments
    ///
    /// * `cache_dir` - Base directory for caching downloaded archives
    ///
    /// # Returns
    ///
    /// Path to the extracted repository directory.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - All URLs fail to download
    /// - SHA256 verification fails
    /// - Archive extraction fails
    /// - Filesystem operations fail
    pub async fn download<P: AsRef<Path>>(&self, cache_dir: P) -> Result<PathBuf> {
        let cache_dir = cache_dir.as_ref();
        let repo_dir = cache_dir.join(&self.name);

        // Check if already cached
        if repo_dir.exists() {
            info!(
                repository = %self.name,
                path = %repo_dir.display(),
                "Repository already cached"
            );
            return Ok(repo_dir);
        }

        info!(
            repository = %self.name,
            urls = ?self.urls,
            "Downloading HTTP archive"
        );

        // Try each URL until one succeeds
        let mut last_error = None;
        for (idx, url) in self.urls.iter().enumerate() {
            match self.try_download_from_url(url, cache_dir).await {
                Ok(path) => {
                    info!(
                        repository = %self.name,
                        url = %url,
                        attempt = idx + 1,
                        total = self.urls.len(),
                        "Successfully downloaded archive"
                    );
                    return Ok(path);
                }
                Err(e) => {
                    warn!(
                        repository = %self.name,
                        url = %url,
                        attempt = idx + 1,
                        total = self.urls.len(),
                        error = %e,
                        "Failed to download from URL"
                    );
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow!("No URLs provided")))
    }

    /// Attempt to download and extract from a single URL.
    async fn try_download_from_url<P: AsRef<Path>>(
        &self,
        url: &str,
        cache_dir: P,
    ) -> Result<PathBuf> {
        let cache_dir = cache_dir.as_ref();
        fs::create_dir_all(cache_dir).with_context(|| {
            format!("Failed to create cache directory: {}", cache_dir.display())
        })?;

        // Download to temporary file
        let temp_file = tempfile::NamedTempFile::new_in(cache_dir)
            .context("Failed to create temporary file")?;

        debug!(url = %url, "Starting download");
        self.download_file(url, &temp_file).await?;

        // Verify SHA256
        debug!(
            expected_hash = %self.sha256,
            "Verifying SHA256 checksum"
        );
        self.verify_sha256(temp_file.path())?;

        // Extract to temporary directory
        let temp_extract_dir = tempfile::tempdir_in(cache_dir)
            .context("Failed to create temporary extraction directory")?;

        debug!(
            extract_dir = %temp_extract_dir.path().display(),
            "Extracting archive"
        );
        self.extract_archive(temp_file.path(), temp_extract_dir.path())?;

        // Move to final location
        let final_dir = cache_dir.join(&self.name);
        fs::rename(temp_extract_dir.path(), &final_dir).with_context(|| {
            format!(
                "Failed to move {} to {}",
                temp_extract_dir.path().display(),
                final_dir.display()
            )
        })?;

        info!(
            repository = %self.name,
            path = %final_dir.display(),
            "Archive extracted successfully"
        );

        Ok(final_dir)
    }

    /// Download a file from a URL to a local path.
    async fn download_file(&self, url: &str, dest: &tempfile::NamedTempFile) -> Result<()> {
        let response = reqwest::get(url)
            .await
            .with_context(|| format!("Failed to fetch URL: {}", url))?;

        if !response.status().is_success() {
            return Err(anyhow!(
                "HTTP request failed with status: {}",
                response.status()
            ));
        }

        let content_length = response.content_length();
        if let Some(len) = content_length {
            debug!(bytes = len, "Download size");
        }

        let bytes = response
            .bytes()
            .await
            .context("Failed to read response body")?;

        let mut file = dest.as_file();
        file.write_all(&bytes)
            .context("Failed to write downloaded content")?;
        file.sync_all().context("Failed to sync file")?;

        debug!(bytes = bytes.len(), "Download complete");
        Ok(())
    }

    /// Verify the SHA256 checksum of a file.
    fn verify_sha256(&self, path: &Path) -> Result<()> {
        let mut file = File::open(path)
            .with_context(|| format!("Failed to open file for hashing: {}", path.display()))?;

        let mut hasher = Sha256::new();
        io::copy(&mut file, &mut hasher)
            .with_context(|| format!("Failed to hash file: {}", path.display()))?;

        let hash = hasher.finalize();
        let hash_hex = hex::encode(hash);

        if hash_hex.to_lowercase() != self.sha256.to_lowercase() {
            return Err(anyhow!(
                "SHA256 mismatch: expected {}, got {}",
                self.sha256,
                hash_hex
            ));
        }

        debug!(hash = %hash_hex, "SHA256 verification successful");
        Ok(())
    }

    /// Extract a tar.gz archive to a directory.
    fn extract_archive(&self, archive_path: &Path, dest_dir: &Path) -> Result<()> {
        let tar_gz = File::open(archive_path)
            .with_context(|| format!("Failed to open archive: {}", archive_path.display()))?;

        let tar = GzDecoder::new(tar_gz);
        let mut archive = Archive::new(tar);

        if let Some(prefix) = &self.strip_prefix {
            self.extract_with_strip_prefix(&mut archive, dest_dir, prefix)?;
        } else {
            archive
                .unpack(dest_dir)
                .with_context(|| format!("Failed to extract archive to: {}", dest_dir.display()))?;
        }

        Ok(())
    }

    /// Extract archive while stripping a path prefix.
    fn extract_with_strip_prefix(
        &self,
        archive: &mut Archive<GzDecoder<File>>,
        dest_dir: &Path,
        prefix: &str,
    ) -> Result<()> {
        let prefix_path = Path::new(prefix);

        for entry in archive
            .entries()
            .context("Failed to read archive entries")?
        {
            let mut entry = entry.context("Failed to read archive entry")?;
            let path = entry.path().context("Failed to get entry path")?;

            // Strip the prefix if present
            let stripped_path = if let Ok(stripped) = path.strip_prefix(prefix_path) {
                stripped.to_path_buf()
            } else {
                // Entry doesn't have the prefix, skip it or keep as-is
                debug!(
                    path = %path.display(),
                    prefix = %prefix,
                    "Entry does not match strip_prefix, extracting as-is"
                );
                path.to_path_buf()
            };

            let dest_path = dest_dir.join(&stripped_path);

            // Create parent directories
            if let Some(parent) = dest_path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("Failed to create directory: {}", parent.display()))?;
            }

            // Extract the entry (store display string before mutable borrow)
            let path_display = stripped_path.display().to_string();
            entry.unpack(&dest_path).with_context(|| {
                format!(
                    "Failed to extract {} to {}",
                    path_display,
                    dest_path.display()
                )
            })?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_http_archive_construction() {
        let archive = HttpArchive::new(
            "test-repo".to_string(),
            vec!["https://example.com/archive.tar.gz".to_string()],
            "abc123".to_string(),
            Some("prefix".to_string()),
        );

        assert_eq!(archive.name, "test-repo");
        assert_eq!(archive.urls.len(), 1);
        assert_eq!(archive.sha256, "abc123");
        assert_eq!(archive.strip_prefix, Some("prefix".to_string()));
    }

    #[test]
    fn test_multiple_urls() {
        let archive = HttpArchive::new(
            "test-repo".to_string(),
            vec![
                "https://mirror1.com/archive.tar.gz".to_string(),
                "https://mirror2.com/archive.tar.gz".to_string(),
                "https://mirror3.com/archive.tar.gz".to_string(),
            ],
            "abc123".to_string(),
            None,
        );

        assert_eq!(archive.urls.len(), 3);
    }

    // TODO: Add integration tests with mock HTTP server using wiremock
    // These would test:
    // - Successful download and extraction
    // - SHA256 verification failure
    // - Network failure and fallback to mirror URLs
    // - strip_prefix functionality
    // - Caching behavior (not re-downloading)
}
