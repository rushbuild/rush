// Rush Remote - Content-Addressable Storage
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Content-addressable storage for efficient file transfers.
//!
//! Implements CAS protocol for uploading/downloading files by content hash,
//! enabling deduplication and efficient remote execution.

use anyhow::Result;
use blake3::Hasher;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Content digest (BLAKE3 hash).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Digest {
    /// Hash value (hex-encoded).
    pub hash: String,

    /// File size in bytes.
    pub size_bytes: u64,
}

impl Digest {
    /// Create a digest from file contents.
    pub fn from_bytes(data: &[u8]) -> Self {
        let mut hasher = Hasher::new();
        hasher.update(data);
        Digest {
            hash: hasher.finalize().to_hex().to_string(),
            size_bytes: data.len() as u64,
        }
    }

    /// Create a digest from a file.
    pub fn from_file(path: &Path) -> Result<Self> {
        let data = fs::read(path)?;
        Ok(Self::from_bytes(&data))
    }

    /// Short hash for display (first 12 chars).
    pub fn short_hash(&self) -> &str {
        &self.hash[..12.min(self.hash.len())]
    }
}

/// Content-addressable storage client.
pub struct CasClient {
    /// Remote CAS endpoint URL.
    endpoint: String,

    /// HTTP client.
    client: reqwest::Client,

    /// Local cache directory for downloaded blobs.
    cache_dir: PathBuf,

    /// Upload statistics.
    stats: CasStats,
}

/// CAS operation statistics.
#[derive(Debug, Clone, Default)]
pub struct CasStats {
    /// Total bytes uploaded.
    pub bytes_uploaded: u64,

    /// Total bytes downloaded.
    pub bytes_downloaded: u64,

    /// Number of upload operations.
    pub uploads: usize,

    /// Number of download operations.
    pub downloads: usize,

    /// Cache hits (already present remotely).
    pub remote_hits: usize,

    /// Local cache hits.
    pub local_hits: usize,
}

impl CasClient {
    /// Create a new CAS client.
    pub fn new(endpoint: String, cache_dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&cache_dir)?;

        Ok(CasClient {
            endpoint,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(300))
                .build()?,
            cache_dir,
            stats: CasStats::default(),
        })
    }

    /// Upload a file to CAS and return its digest.
    pub async fn upload_file(&mut self, path: &Path) -> Result<Digest> {
        let data = fs::read(path)?;
        self.upload_bytes(&data).await
    }

    /// Upload bytes to CAS and return digest.
    pub async fn upload_bytes(&mut self, data: &[u8]) -> Result<Digest> {
        let digest = Digest::from_bytes(data);

        // Check if blob already exists remotely
        if self.has_blob(&digest).await? {
            self.stats.remote_hits += 1;
            return Ok(digest);
        }

        // Upload blob
        let url = format!("{}/cas/blobs/{}", self.endpoint, digest.hash);
        let response = self.client.put(&url).body(data.to_vec()).send().await?;

        if !response.status().is_success() {
            anyhow::bail!("Failed to upload blob: {}", response.status());
        }

        self.stats.uploads += 1;
        self.stats.bytes_uploaded += data.len() as u64;

        // Cache locally
        self.cache_blob(&digest, data)?;

        Ok(digest)
    }

    /// Download a blob by digest.
    pub async fn download_blob(&mut self, digest: &Digest) -> Result<Vec<u8>> {
        // Check local cache first
        if let Some(data) = self.get_cached_blob(digest)? {
            self.stats.local_hits += 1;
            return Ok(data);
        }

        // Download from remote
        let url = format!("{}/cas/blobs/{}", self.endpoint, digest.hash);
        let response = self.client.get(&url).send().await?;

        if !response.status().is_success() {
            anyhow::bail!("Failed to download blob: {}", response.status());
        }

        let data = response.bytes().await?.to_vec();

        // Verify digest
        let actual_digest = Digest::from_bytes(&data);
        if actual_digest.hash != digest.hash {
            anyhow::bail!(
                "Digest mismatch: expected {}, got {}",
                digest.hash,
                actual_digest.hash
            );
        }

        self.stats.downloads += 1;
        self.stats.bytes_downloaded += data.len() as u64;

        // Cache locally
        self.cache_blob(digest, &data)?;

        Ok(data)
    }

    /// Download blob to file.
    pub async fn download_to_file(&mut self, digest: &Digest, path: &Path) -> Result<()> {
        let data = self.download_blob(digest).await?;

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        fs::write(path, data)?;
        Ok(())
    }

    /// Upload multiple files and return their digests.
    pub async fn upload_files(&mut self, paths: &[PathBuf]) -> Result<Vec<Digest>> {
        let mut digests = Vec::new();

        for path in paths {
            let digest = self.upload_file(path).await?;
            digests.push(digest);
        }

        Ok(digests)
    }

    /// Download multiple blobs to files.
    pub async fn download_files(
        &mut self,
        digests: &[Digest],
        output_dir: &Path,
    ) -> Result<Vec<PathBuf>> {
        fs::create_dir_all(output_dir)?;

        let mut paths = Vec::new();

        for (i, digest) in digests.iter().enumerate() {
            let filename = format!("blob_{}", i);
            let path = output_dir.join(filename);
            self.download_to_file(digest, &path).await?;
            paths.push(path);
        }

        Ok(paths)
    }

    /// Check if a blob exists remotely.
    async fn has_blob(&self, digest: &Digest) -> Result<bool> {
        let url = format!("{}/cas/blobs/{}/exists", self.endpoint, digest.hash);
        let response = self.client.get(&url).send().await?;
        Ok(response.status().is_success())
    }

    /// Get blob from local cache.
    fn get_cached_blob(&self, digest: &Digest) -> Result<Option<Vec<u8>>> {
        let cache_path = self.cache_dir.join(&digest.hash);

        if !cache_path.exists() {
            return Ok(None);
        }

        let data = fs::read(cache_path)?;

        // Verify cached data
        let cached_digest = Digest::from_bytes(&data);
        if cached_digest.hash != digest.hash {
            // Cache corruption, remove
            let _ = fs::remove_file(self.cache_dir.join(&digest.hash));
            return Ok(None);
        }

        Ok(Some(data))
    }

    /// Cache blob locally.
    fn cache_blob(&self, digest: &Digest, data: &[u8]) -> Result<()> {
        let cache_path = self.cache_dir.join(&digest.hash);
        fs::write(cache_path, data)?;
        Ok(())
    }

    /// Get CAS statistics.
    pub fn stats(&self) -> &CasStats {
        &self.stats
    }

    /// Get cache directory.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Clear local cache.
    pub fn clear_cache(&mut self) -> Result<usize> {
        let mut cleared = 0;

        for entry in fs::read_dir(&self.cache_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                fs::remove_file(entry.path())?;
                cleared += 1;
            }
        }

        Ok(cleared)
    }

    /// Prune cache entries larger than size limit.
    pub fn prune_cache(&mut self, max_size_bytes: u64) -> Result<usize> {
        let mut entries: Vec<_> = fs::read_dir(&self.cache_dir)?
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let metadata = e.metadata().ok()?;
                let size = metadata.len();
                let modified = metadata.modified().ok()?;
                Some((e.path(), size, modified))
            })
            .collect();

        // Sort by modification time (oldest first)
        entries.sort_by_key(|(_, _, modified)| *modified);

        let mut total_size: u64 = entries.iter().map(|(_, size, _)| size).sum();
        let mut pruned = 0;

        for (path, size, _) in entries {
            if total_size <= max_size_bytes {
                break;
            }

            fs::remove_file(&path)?;
            total_size -= size;
            pruned += 1;
        }

        Ok(pruned)
    }
}

/// Directory manifest for CAS uploads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectoryManifest {
    /// Files in the directory with their digests.
    pub files: HashMap<String, Digest>,

    /// Subdirectories with their manifests.
    pub directories: HashMap<String, Box<DirectoryManifest>>,
}

impl DirectoryManifest {
    /// Create manifest from directory.
    pub fn from_directory<'a>(
        path: &'a Path,
        cas: &'a mut CasClient,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Self>> + Send + 'a>> {
        Box::pin(async move {
            let mut files = HashMap::new();
            let mut directories = HashMap::new();

            for entry in fs::read_dir(path)? {
                let entry = entry?;
                let entry_path = entry.path();
                let file_name = entry.file_name().to_string_lossy().to_string();

                if entry.file_type()?.is_dir() {
                    let subdir_manifest = Self::from_directory(&entry_path, cas).await?;
                    directories.insert(file_name, Box::new(subdir_manifest));
                } else if entry.file_type()?.is_file() {
                    let digest = cas.upload_file(&entry_path).await?;
                    files.insert(file_name, digest);
                }
            }

            Ok(DirectoryManifest { files, directories })
        })
    }

    /// Download directory from manifest.
    pub fn download_to<'a>(
        &'a self,
        cas: &'a mut CasClient,
        output_dir: &'a Path,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            fs::create_dir_all(output_dir)?;

            // Download files
            for (name, digest) in &self.files {
                let file_path = output_dir.join(name);
                cas.download_to_file(digest, &file_path).await?;
            }

            // Recursively download subdirectories
            for (name, subdir_manifest) in &self.directories {
                let subdir_path = output_dir.join(name);
                subdir_manifest.download_to(cas, &subdir_path).await?;
            }

            Ok(())
        })
    }

    /// Get total size of all files.
    pub fn total_size(&self) -> u64 {
        let files_size: u64 = self.files.values().map(|d| d.size_bytes).sum();
        let dirs_size: u64 = self.directories.values().map(|d| d.total_size()).sum();
        files_size + dirs_size
    }

    /// Get file count.
    pub fn file_count(&self) -> usize {
        let files = self.files.len();
        let subdirs: usize = self.directories.values().map(|d| d.file_count()).sum();
        files + subdirs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_digest_from_bytes() {
        let data = b"hello world";
        let digest = Digest::from_bytes(data);

        assert_eq!(digest.size_bytes, 11);
        assert!(!digest.hash.is_empty());
    }

    #[test]
    fn test_digest_consistency() {
        let data = b"test data";
        let digest1 = Digest::from_bytes(data);
        let digest2 = Digest::from_bytes(data);

        assert_eq!(digest1.hash, digest2.hash);
        assert_eq!(digest1.size_bytes, digest2.size_bytes);
    }

    #[test]
    fn test_local_cache() -> Result<()> {
        let temp_dir = TempDir::new()?;
        let cas = CasClient::new(
            "http://localhost:8080".to_string(),
            temp_dir.path().to_path_buf(),
        )?;

        let data = b"cached data";
        let digest = Digest::from_bytes(data);

        // Cache the blob
        cas.cache_blob(&digest, data)?;

        // Retrieve from cache
        let cached = cas.get_cached_blob(&digest)?;
        assert!(cached.is_some());
        assert_eq!(cached.unwrap(), data);

        Ok(())
    }
}
