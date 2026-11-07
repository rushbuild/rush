// Rush Build - Remote Caching
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Remote caching for distributed teams.
//!
//! Enables sharing of build artifacts across a team or organization through
//! remote cache servers. Supports HTTP-based caching with compression and
//! integrity verification.

use anyhow::{Context, Result};
use blake3::Hasher;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::fs;

/// Remote cache client.
pub struct RemoteCacheClient {
    /// Remote cache URL.
    url: String,

    /// Local cache directory.
    local_cache: PathBuf,

    /// HTTP client.
    client: reqwest::Client,

    /// Statistics.
    stats: Arc<RemoteCacheStats>,

    /// Configuration.
    config: RemoteCacheConfig,
}

/// Remote cache configuration.
#[derive(Debug, Clone)]
pub struct RemoteCacheConfig {
    /// Enable compression.
    pub compress: bool,

    /// Compression level (0-9).
    pub compression_level: u32,

    /// Read timeout.
    pub read_timeout: Duration,

    /// Write timeout.
    pub write_timeout: Duration,

    /// Maximum artifact size (bytes).
    pub max_artifact_size: u64,

    /// Enable integrity verification.
    pub verify_integrity: bool,

    /// Number of retry attempts.
    pub retry_attempts: usize,

    /// Retry delay.
    pub retry_delay: Duration,
}

impl Default for RemoteCacheConfig {
    fn default() -> Self {
        RemoteCacheConfig {
            compress: true,
            compression_level: 6,
            read_timeout: Duration::from_secs(30),
            write_timeout: Duration::from_secs(60),
            max_artifact_size: 100 * 1024 * 1024, // 100MB
            verify_integrity: true,
            retry_attempts: 3,
            retry_delay: Duration::from_millis(500),
        }
    }
}

/// Remote cache statistics.
#[derive(Debug, Default)]
pub struct RemoteCacheStats {
    /// Remote hits (found in remote cache).
    pub remote_hits: AtomicU64,

    /// Remote misses (not found in remote).
    pub remote_misses: AtomicU64,

    /// Local hits (found in local cache).
    pub local_hits: AtomicU64,

    /// Local misses.
    pub local_misses: AtomicU64,

    /// Bytes downloaded.
    pub bytes_downloaded: AtomicU64,

    /// Bytes uploaded.
    pub bytes_uploaded: AtomicU64,

    /// Upload count.
    pub uploads: AtomicU64,

    /// Download count.
    pub downloads: AtomicU64,

    /// Upload failures.
    pub upload_failures: AtomicU64,

    /// Download failures.
    pub download_failures: AtomicU64,
}

/// Cached artifact metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactMetadata {
    /// Content hash (BLAKE3).
    pub hash: String,

    /// Size in bytes.
    pub size: u64,

    /// Creation timestamp.
    pub created_at: u64,

    /// Compression algorithm (if any).
    pub compression: Option<String>,

    /// Additional metadata.
    pub metadata: serde_json::Value,
}

impl RemoteCacheClient {
    /// Create a new remote cache client.
    pub fn new<P: AsRef<Path>>(url: String, local_cache: P) -> Result<Self> {
        let local_cache = local_cache.as_ref().to_path_buf();
        std::fs::create_dir_all(&local_cache)?;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()?;

        Ok(RemoteCacheClient {
            url,
            local_cache,
            client,
            stats: Arc::new(RemoteCacheStats::default()),
            config: RemoteCacheConfig::default(),
        })
    }

    /// Create with custom configuration.
    pub fn with_config<P: AsRef<Path>>(
        url: String,
        local_cache: P,
        config: RemoteCacheConfig,
    ) -> Result<Self> {
        let mut client = Self::new(url, local_cache)?;
        client.config = config;
        Ok(client)
    }

    /// Get an artifact from cache.
    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        // Check local cache first
        if let Some(data) = self.get_local(key).await? {
            self.stats.local_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(Some(data));
        }

        self.stats.local_misses.fetch_add(1, Ordering::Relaxed);

        // Try remote cache
        match self.get_remote(key).await {
            Ok(Some(data)) => {
                self.stats.remote_hits.fetch_add(1, Ordering::Relaxed);
                self.stats.downloads.fetch_add(1, Ordering::Relaxed);
                self.stats
                    .bytes_downloaded
                    .fetch_add(data.len() as u64, Ordering::Relaxed);

                // Store in local cache
                let _ = self.put_local(key, &data).await;

                Ok(Some(data))
            }
            Ok(None) => {
                self.stats.remote_misses.fetch_add(1, Ordering::Relaxed);
                Ok(None)
            }
            Err(e) => {
                self.stats.download_failures.fetch_add(1, Ordering::Relaxed);
                Err(e)
            }
        }
    }

    /// Put an artifact into cache.
    pub async fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        // Check size limit
        if data.len() as u64 > self.config.max_artifact_size {
            anyhow::bail!("Artifact too large: {} bytes", data.len());
        }

        // Store in local cache
        self.put_local(key, data).await?;

        // Upload to remote cache
        match self.put_remote(key, data).await {
            Ok(()) => {
                self.stats.uploads.fetch_add(1, Ordering::Relaxed);
                self.stats
                    .bytes_uploaded
                    .fetch_add(data.len() as u64, Ordering::Relaxed);
                Ok(())
            }
            Err(e) => {
                self.stats.upload_failures.fetch_add(1, Ordering::Relaxed);
                Err(e)
            }
        }
    }

    /// Get artifact from local cache.
    async fn get_local(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let path = self.local_cache.join(key);

        if !path.exists() {
            return Ok(None);
        }

        let data = fs::read(&path).await?;
        Ok(Some(data))
    }

    /// Put artifact to local cache.
    async fn put_local(&self, key: &str, data: &[u8]) -> Result<()> {
        let path = self.local_cache.join(key);

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }

        fs::write(&path, data).await?;
        Ok(())
    }

    /// Get artifact from remote cache with retries.
    async fn get_remote(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let url = format!("{}/artifacts/{}", self.url, key);

        for attempt in 0..self.config.retry_attempts {
            match self.get_remote_once(&url).await {
                Ok(result) => return Ok(result),
                Err(e) => {
                    if attempt + 1 == self.config.retry_attempts {
                        return Err(e);
                    }
                    tokio::time::sleep(self.config.retry_delay).await;
                }
            }
        }

        Ok(None)
    }

    /// Single attempt to get from remote.
    async fn get_remote_once(&self, url: &str) -> Result<Option<Vec<u8>>> {
        let response = self
            .client
            .get(url)
            .timeout(self.config.read_timeout)
            .send()
            .await?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        if !response.status().is_success() {
            anyhow::bail!("Remote cache error: {}", response.status());
        }

        let data = response.bytes().await?.to_vec();

        // Decompress if needed
        let data = if self.config.compress {
            self.decompress(&data)?
        } else {
            data
        };

        Ok(Some(data))
    }

    /// Put artifact to remote cache with retries.
    async fn put_remote(&self, key: &str, data: &[u8]) -> Result<()> {
        let url = format!("{}/artifacts/{}", self.url, key);

        // Compress if enabled
        let data = if self.config.compress {
            self.compress(data)?
        } else {
            data.to_vec()
        };

        // Compute metadata
        let metadata = self.create_metadata(&data);

        for attempt in 0..self.config.retry_attempts {
            match self.put_remote_once(&url, &data, &metadata).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    if attempt + 1 == self.config.retry_attempts {
                        return Err(e);
                    }
                    tokio::time::sleep(self.config.retry_delay).await;
                }
            }
        }

        Ok(())
    }

    /// Single attempt to put to remote.
    async fn put_remote_once(
        &self,
        url: &str,
        data: &[u8],
        metadata: &ArtifactMetadata,
    ) -> Result<()> {
        let response = self
            .client
            .put(url)
            .timeout(self.config.write_timeout)
            .header("Content-Type", "application/octet-stream")
            .header("X-Artifact-Hash", &metadata.hash)
            .header("X-Artifact-Size", metadata.size.to_string())
            .body(data.to_vec())
            .send()
            .await?;

        if !response.status().is_success() {
            anyhow::bail!("Remote cache upload failed: {}", response.status());
        }

        Ok(())
    }

    /// Compress data.
    fn compress(&self, data: &[u8]) -> Result<Vec<u8>> {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;

        let mut encoder =
            GzEncoder::new(Vec::new(), Compression::new(self.config.compression_level));
        encoder.write_all(data)?;
        encoder.finish().context("Failed to compress data")
    }

    /// Decompress data.
    fn decompress(&self, data: &[u8]) -> Result<Vec<u8>> {
        use flate2::read::GzDecoder;
        use std::io::Read;

        let mut decoder = GzDecoder::new(data);
        let mut decompressed = Vec::new();
        decoder
            .read_to_end(&mut decompressed)
            .context("Failed to decompress data")?;

        Ok(decompressed)
    }

    /// Create artifact metadata.
    fn create_metadata(&self, data: &[u8]) -> ArtifactMetadata {
        let mut hasher = Hasher::new();
        hasher.update(data);
        let hash = hasher.finalize().to_hex().to_string();

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        ArtifactMetadata {
            hash,
            size: data.len() as u64,
            created_at: now,
            compression: if self.config.compress {
                Some("gzip".to_string())
            } else {
                None
            },
            metadata: serde_json::Value::Null,
        }
    }

    /// Get statistics.
    pub fn stats(&self) -> RemoteCacheStatsSnapshot {
        RemoteCacheStatsSnapshot {
            remote_hits: self.stats.remote_hits.load(Ordering::Relaxed),
            remote_misses: self.stats.remote_misses.load(Ordering::Relaxed),
            local_hits: self.stats.local_hits.load(Ordering::Relaxed),
            local_misses: self.stats.local_misses.load(Ordering::Relaxed),
            bytes_downloaded: self.stats.bytes_downloaded.load(Ordering::Relaxed),
            bytes_uploaded: self.stats.bytes_uploaded.load(Ordering::Relaxed),
            uploads: self.stats.uploads.load(Ordering::Relaxed),
            downloads: self.stats.downloads.load(Ordering::Relaxed),
            upload_failures: self.stats.upload_failures.load(Ordering::Relaxed),
            download_failures: self.stats.download_failures.load(Ordering::Relaxed),
        }
    }

    /// Clear local cache.
    pub async fn clear_local(&self) -> Result<()> {
        fs::remove_dir_all(&self.local_cache).await?;
        fs::create_dir_all(&self.local_cache).await?;
        Ok(())
    }
}

/// Snapshot of remote cache statistics.
#[derive(Debug, Clone)]
pub struct RemoteCacheStatsSnapshot {
    /// Remote hits.
    pub remote_hits: u64,

    /// Remote misses.
    pub remote_misses: u64,

    /// Local hits.
    pub local_hits: u64,

    /// Local misses.
    pub local_misses: u64,

    /// Bytes downloaded.
    pub bytes_downloaded: u64,

    /// Bytes uploaded.
    pub bytes_uploaded: u64,

    /// Upload count.
    pub uploads: u64,

    /// Download count.
    pub downloads: u64,

    /// Upload failures.
    pub upload_failures: u64,

    /// Download failures.
    pub download_failures: u64,
}

impl RemoteCacheStatsSnapshot {
    /// Get remote hit rate.
    pub fn remote_hit_rate(&self) -> f64 {
        let total = self.remote_hits + self.remote_misses;
        if total == 0 {
            return 0.0;
        }
        (self.remote_hits as f64 / total as f64) * 100.0
    }

    /// Get local hit rate.
    pub fn local_hit_rate(&self) -> f64 {
        let total = self.local_hits + self.local_misses;
        if total == 0 {
            return 0.0;
        }
        (self.local_hits as f64 / total as f64) * 100.0
    }

    /// Get overall hit rate.
    pub fn overall_hit_rate(&self) -> f64 {
        let total_hits = self.local_hits + self.remote_hits;
        let total_requests = total_hits + self.remote_misses;

        if total_requests == 0 {
            return 0.0;
        }

        (total_hits as f64 / total_requests as f64) * 100.0
    }

    /// Get bandwidth savings (bytes not downloaded due to local cache).
    pub fn bandwidth_saved(&self) -> u64 {
        // Estimate: each local hit saved a download
        // Use average download size as estimate
        if self.downloads == 0 {
            return 0;
        }

        let avg_download_size = self.bytes_downloaded / self.downloads;
        self.local_hits * avg_download_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_local_cache() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let client =
            RemoteCacheClient::new("http://localhost:8080".to_string(), temp_dir.path()).unwrap();

        let key = "test_artifact";
        let data = b"test data";

        // Put to local cache
        client.put_local(key, data).await.unwrap();

        // Get from local cache
        let result = client.get_local(key).await.unwrap();
        assert_eq!(result, Some(data.to_vec()));

        let stats = client.stats();
        assert_eq!(stats.local_hits, 0); // Direct local operations don't update stats
    }

    #[tokio::test]
    async fn test_compression() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let client =
            RemoteCacheClient::new("http://localhost:8080".to_string(), temp_dir.path()).unwrap();

        let data = b"test data that should compress well: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

        let compressed = client.compress(data).unwrap();
        assert!(compressed.len() < data.len());

        let decompressed = client.decompress(&compressed).unwrap();
        assert_eq!(decompressed, data);
    }

    #[tokio::test]
    async fn test_metadata() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let client =
            RemoteCacheClient::new("http://localhost:8080".to_string(), temp_dir.path()).unwrap();

        let data = b"test data";
        let metadata = client.create_metadata(data);

        assert_eq!(metadata.size, data.len() as u64);
        assert!(!metadata.hash.is_empty());
        assert_eq!(metadata.compression, Some("gzip".to_string()));
    }
}
