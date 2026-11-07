// Rush Repo - Repository cache
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Repository cache for downloaded dependencies.
//!
//! Caches downloaded and extracted repositories to avoid re-downloading on
//! subsequent builds. The cache is content-addressable, keyed by repository
//! rule parameters to ensure correctness.

use anyhow::{Context, Result};
use dashmap::DashMap;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{debug, info};
use walkdir::WalkDir;

/// Cache for repository rules.
///
/// Stores downloaded and extracted repositories in a content-addressable cache.
/// The cache key is derived from the repository rule parameters (URL, commit, etc.)
/// to ensure that changes to dependencies trigger re-downloads.
///
/// # Cache Directory Structure
///
/// ```text
/// cache_dir/rush-repo-cache/
/// ├── archives/
/// │   └── <sha256>/          # Extracted http_archive
/// ├── repos/
/// │   └── <name>/            # Git repositories
/// └── metadata/
///     └── <key>.json         # Cache metadata
/// ```
///
/// # Thread Safety
///
/// This cache is thread-safe and can be shared across multiple threads.
/// It uses `DashMap` for concurrent in-memory tracking and atomic filesystem
/// operations (temp + rename) to prevent partial cache entries.
pub struct RepositoryCache {
    /// Root directory for the cache
    cache_dir: PathBuf,

    /// In-memory tracking of cache entries for fast lookups
    entries: Arc<DashMap<String, CacheEntry>>,
}

/// Metadata about a cached repository entry.
#[derive(Debug, Clone)]
struct CacheEntry {
    /// Cache key for this entry
    #[allow(dead_code)]
    key: String,

    /// Path to the cached repository
    path: PathBuf,

    /// When this was cached
    #[allow(dead_code)]
    timestamp: std::time::SystemTime,

    /// Size in bytes (if computed)
    #[allow(dead_code)]
    size_bytes: Option<u64>,
}

impl RepositoryCache {
    /// Create a new repository cache.
    ///
    /// Initializes the cache directory structure if it doesn't exist.
    ///
    /// # Arguments
    ///
    /// * `cache_dir` - Root directory for cached repositories
    ///
    /// # Errors
    ///
    /// Returns an error if cache directories cannot be created.
    pub fn new<P: Into<PathBuf>>(cache_dir: P) -> Result<Self> {
        let cache_dir = cache_dir.into();
        let base_dir = cache_dir.join("rush-repo-cache");

        // Create cache directory structure
        fs::create_dir_all(&base_dir).context("Failed to create cache base directory")?;
        fs::create_dir_all(base_dir.join("archives"))
            .context("Failed to create archives directory")?;
        fs::create_dir_all(base_dir.join("repos")).context("Failed to create repos directory")?;
        fs::create_dir_all(base_dir.join("metadata"))
            .context("Failed to create metadata directory")?;

        debug!(cache_dir = %base_dir.display(), "Initialized repository cache");

        Ok(Self {
            cache_dir: base_dir,
            entries: Arc::new(DashMap::new()),
        })
    }

    /// Get the path for a cached repository entry.
    ///
    /// # Arguments
    ///
    /// * `key` - Cache key computed from repository parameters
    ///
    /// # Returns
    ///
    /// Returns `Some(PathBuf)` if the entry exists in cache, `None` otherwise.
    pub fn get(&self, key: &str) -> Option<PathBuf> {
        // Check in-memory cache first
        if let Some(entry) = self.entries.get(key) {
            let path = &entry.path;
            if path.exists() && path.is_dir() {
                debug!(key = %key, path = %path.display(), "Cache hit");
                return Some(path.clone());
            } else {
                // Path no longer exists, remove from in-memory cache
                drop(entry);
                self.entries.remove(key);
            }
        }

        // Check filesystem for archive cache
        let archive_path = self.cache_dir.join("archives").join(key);
        if archive_path.exists() && archive_path.is_dir() {
            debug!(key = %key, path = %archive_path.display(), "Cache hit (archives)");

            // Add to in-memory cache
            self.entries.insert(
                key.to_string(),
                CacheEntry {
                    key: key.to_string(),
                    path: archive_path.clone(),
                    timestamp: std::time::SystemTime::now(),
                    size_bytes: None,
                },
            );

            return Some(archive_path);
        }

        debug!(key = %key, "Cache miss");
        None
    }

    /// Store a repository in the cache.
    ///
    /// Uses atomic operations (temp directory + rename) to ensure cache
    /// consistency even under concurrent access.
    ///
    /// # Arguments
    ///
    /// * `key` - Cache key computed from repository parameters
    /// * `source_path` - Path to the repository to cache
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Source path doesn't exist
    /// - Cache directory operations fail
    /// - Atomic rename fails
    pub fn put(&self, key: &str, source_path: &Path) -> Result<PathBuf> {
        if !source_path.exists() {
            anyhow::bail!("Source path does not exist: {}", source_path.display());
        }

        let cache_path = self.cache_dir.join("archives").join(key);

        // If already cached, verify and return
        if cache_path.exists() && cache_path.is_dir() {
            info!(key = %key, path = %cache_path.display(), "Repository already cached");
            return Ok(cache_path);
        }

        info!(
            key = %key,
            source = %source_path.display(),
            dest = %cache_path.display(),
            "Caching repository"
        );

        // Create temporary directory for atomic operation
        let temp_dir = tempfile::tempdir_in(self.cache_dir.join("archives"))
            .context("Failed to create temporary directory for cache storage")?;

        // Copy repository to temp directory
        if source_path.is_dir() {
            copy_dir_all(source_path, temp_dir.path())
                .context("Failed to copy repository to cache")?;
        } else {
            anyhow::bail!("Source path must be a directory: {}", source_path.display());
        }

        // Atomic rename to final location
        fs::rename(temp_dir.path(), &cache_path).with_context(|| {
            format!(
                "Failed to move temporary cache to final location: {}",
                cache_path.display()
            )
        })?;

        // Add to in-memory cache
        self.entries.insert(
            key.to_string(),
            CacheEntry {
                key: key.to_string(),
                path: cache_path.clone(),
                timestamp: std::time::SystemTime::now(),
                size_bytes: None,
            },
        );

        info!(key = %key, path = %cache_path.display(), "Repository cached successfully");
        Ok(cache_path)
    }

    /// Compute a content-addressable cache key from repository rule parameters.
    ///
    /// The cache key is a SHA256 hash of all parameters that affect the
    /// repository contents. This ensures that any change to the repository
    /// definition will result in a new cache entry.
    ///
    /// # Arguments
    ///
    /// * `params` - Slice of (key, value) parameter pairs to hash
    ///
    /// # Returns
    ///
    /// Hex-encoded SHA256 hash of the parameters.
    ///
    /// # Example
    ///
    /// ```
    /// use rush_repo::RepositoryCache;
    ///
    /// # fn example() -> anyhow::Result<()> {
    /// let cache = RepositoryCache::new("/tmp/cache")?;
    /// let key = cache.compute_key(&[
    ///     ("name", "zlib"),
    ///     ("url", "https://zlib.net/zlib-1.3.tar.gz"),
    ///     ("sha256", "ff0ba4c292013dbc27530b3a81e1f9a813cd39de01ca5e0f8bf355702efa593e"),
    ///     ("strip_prefix", "zlib-1.3"),
    /// ]);
    /// # Ok(())
    /// # }
    /// ```
    pub fn compute_key(&self, params: &[(&str, &str)]) -> String {
        let mut hasher = Sha256::new();

        // Sort parameters for deterministic hashing
        let mut sorted_params: Vec<_> = params.to_vec();
        sorted_params.sort_by_key(|(k, _)| *k);

        // Hash each parameter
        for (key, value) in sorted_params {
            hasher.update(key.as_bytes());
            hasher.update(b"=");
            hasher.update(value.as_bytes());
            hasher.update(b"\n");
        }

        let hash = hasher.finalize();
        hex::encode(hash)
    }

    /// Clear all cached repositories.
    ///
    /// Removes the entire cache directory and recreates an empty cache.
    /// This operation is atomic per repository but may race with concurrent
    /// cache writes.
    ///
    /// # Errors
    ///
    /// Returns an error if cache directories cannot be removed or recreated.
    pub fn clear(&self) -> Result<()> {
        info!(cache_dir = %self.cache_dir.display(), "Clearing repository cache");

        // Clear in-memory cache
        self.entries.clear();

        // Remove and recreate cache directories
        let archives_dir = self.cache_dir.join("archives");
        let repos_dir = self.cache_dir.join("repos");
        let metadata_dir = self.cache_dir.join("metadata");

        if archives_dir.exists() {
            fs::remove_dir_all(&archives_dir).context("Failed to remove archives directory")?;
        }
        if repos_dir.exists() {
            fs::remove_dir_all(&repos_dir).context("Failed to remove repos directory")?;
        }
        if metadata_dir.exists() {
            fs::remove_dir_all(&metadata_dir).context("Failed to remove metadata directory")?;
        }

        fs::create_dir_all(&archives_dir).context("Failed to recreate archives directory")?;
        fs::create_dir_all(&repos_dir).context("Failed to recreate repos directory")?;
        fs::create_dir_all(&metadata_dir).context("Failed to recreate metadata directory")?;

        info!("Repository cache cleared successfully");
        Ok(())
    }

    /// Get the number of cached repository entries.
    ///
    /// Counts both in-memory entries and filesystem entries.
    pub fn size(&self) -> usize {
        // Count filesystem entries for accurate count
        let mut count = 0;

        let archives_dir = self.cache_dir.join("archives");
        if let Ok(entries) = fs::read_dir(&archives_dir) {
            count += entries.filter_map(|e| e.ok()).count();
        }

        let repos_dir = self.cache_dir.join("repos");
        if let Ok(entries) = fs::read_dir(&repos_dir) {
            count += entries.filter_map(|e| e.ok()).count();
        }

        count
    }

    /// Calculate total disk usage of cached repositories.
    ///
    /// Recursively walks all cached directories to compute total size.
    /// This operation may be expensive for large caches.
    pub fn total_size_bytes(&self) -> u64 {
        let mut total_size = 0u64;

        // Walk archives directory
        let archives_dir = self.cache_dir.join("archives");
        if archives_dir.exists() {
            for entry in WalkDir::new(&archives_dir)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                if entry.file_type().is_file() {
                    if let Ok(metadata) = entry.metadata() {
                        total_size += metadata.len();
                    }
                }
            }
        }

        // Walk repos directory
        let repos_dir = self.cache_dir.join("repos");
        if repos_dir.exists() {
            for entry in WalkDir::new(&repos_dir).into_iter().filter_map(|e| e.ok()) {
                if entry.file_type().is_file() {
                    if let Ok(metadata) = entry.metadata() {
                        total_size += metadata.len();
                    }
                }
            }
        }

        total_size
    }

    /// Get cache statistics.
    ///
    /// Returns information about the cache including number of entries
    /// and total disk usage.
    pub fn stats(&self) -> CacheStats {
        CacheStats {
            total_repositories: self.size(),
            total_size_bytes: self.total_size_bytes(),
        }
    }
}

/// Recursively copy a directory and all its contents.
fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)
        .with_context(|| format!("Failed to create directory: {}", dst.display()))?;

    for entry in
        fs::read_dir(src).with_context(|| format!("Failed to read directory: {}", src.display()))?
    {
        let entry = entry?;
        let ty = entry.file_type()?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if ty.is_dir() {
            copy_dir_all(&src_path, &dst_path)?;
        } else if ty.is_file() {
            fs::copy(&src_path, &dst_path).with_context(|| {
                format!(
                    "Failed to copy file from {} to {}",
                    src_path.display(),
                    dst_path.display()
                )
            })?;
        } else if ty.is_symlink() {
            // Preserve symlinks
            let link_target = fs::read_link(&src_path)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&link_target, &dst_path).with_context(|| {
                format!(
                    "Failed to create symlink from {} to {}",
                    link_target.display(),
                    dst_path.display()
                )
            })?;
            #[cfg(not(unix))]
            {
                warn!(
                    "Symlink copying not supported on this platform: {}",
                    src_path.display()
                );
            }
        }
    }

    Ok(())
}

/// Statistics about the repository cache.
#[derive(Debug, Clone)]
pub struct CacheStats {
    /// Total number of cached repositories
    pub total_repositories: usize,

    /// Total size of cache in bytes
    pub total_size_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;

    #[test]
    fn test_cache_initialization() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let _cache = RepositoryCache::new(temp_dir.path())?;

        // Verify directory structure
        let base = temp_dir.path().join("rush-repo-cache");
        assert!(base.join("archives").exists());
        assert!(base.join("repos").exists());
        assert!(base.join("metadata").exists());

        Ok(())
    }

    #[test]
    fn test_compute_key_deterministic() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let cache = RepositoryCache::new(temp_dir.path())?;

        let params = &[
            ("name", "zlib"),
            ("url", "https://example.com/zlib.tar.gz"),
            ("sha256", "abc123"),
        ];

        let key1 = cache.compute_key(params);
        let key2 = cache.compute_key(params);

        assert_eq!(key1, key2, "Cache keys should be deterministic");
        assert_eq!(key1.len(), 64, "SHA256 hash should be 64 hex chars");

        Ok(())
    }

    #[test]
    fn test_compute_key_parameter_order_independent() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let cache = RepositoryCache::new(temp_dir.path())?;

        let params1 = &[
            ("name", "zlib"),
            ("url", "https://example.com/zlib.tar.gz"),
            ("sha256", "abc123"),
        ];

        let params2 = &[
            ("sha256", "abc123"),
            ("name", "zlib"),
            ("url", "https://example.com/zlib.tar.gz"),
        ];

        let key1 = cache.compute_key(params1);
        let key2 = cache.compute_key(params2);

        assert_eq!(key1, key2, "Cache keys should be order-independent");

        Ok(())
    }

    #[test]
    fn test_compute_key_different_params() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let cache = RepositoryCache::new(temp_dir.path())?;

        let params1 = &[
            ("name", "zlib"),
            ("url", "https://example.com/zlib-1.2.tar.gz"),
        ];

        let params2 = &[
            ("name", "zlib"),
            ("url", "https://example.com/zlib-1.3.tar.gz"),
        ];

        let key1 = cache.compute_key(params1);
        let key2 = cache.compute_key(params2);

        assert_ne!(key1, key2, "Different params should produce different keys");

        Ok(())
    }

    #[test]
    fn test_get_nonexistent() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let cache = RepositoryCache::new(temp_dir.path())?;

        let result = cache.get("nonexistent-key");
        assert!(result.is_none(), "Should return None for nonexistent key");

        Ok(())
    }

    #[test]
    fn test_put_and_get() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let cache = RepositoryCache::new(temp_dir.path())?;

        // Create a test directory to cache
        let source_dir = temp_dir.path().join("source");
        fs::create_dir(&source_dir)?;

        let test_file = source_dir.join("test.txt");
        let mut file = File::create(&test_file)?;
        file.write_all(b"test content")?;

        // Put in cache
        let key = "test-key";
        let cached_path = cache.put(key, &source_dir)?;

        assert!(cached_path.exists());
        assert!(cached_path.join("test.txt").exists());

        // Get from cache
        let retrieved = cache.get(key);
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap(), cached_path);

        Ok(())
    }

    #[test]
    fn test_put_idempotent() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let cache = RepositoryCache::new(temp_dir.path())?;

        let source_dir = temp_dir.path().join("source");
        fs::create_dir(&source_dir)?;

        let key = "test-key";
        let path1 = cache.put(key, &source_dir)?;
        let path2 = cache.put(key, &source_dir)?;

        assert_eq!(
            path1, path2,
            "Putting same content twice should be idempotent"
        );

        Ok(())
    }

    #[test]
    fn test_size() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let cache = RepositoryCache::new(temp_dir.path())?;

        assert_eq!(cache.size(), 0);

        // Add some cache entries
        let source_dir = temp_dir.path().join("source");
        fs::create_dir(&source_dir)?;

        cache.put("key1", &source_dir)?;
        assert_eq!(cache.size(), 1);

        cache.put("key2", &source_dir)?;
        assert_eq!(cache.size(), 2);

        Ok(())
    }

    #[test]
    fn test_clear() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let cache = RepositoryCache::new(temp_dir.path())?;

        let source_dir = temp_dir.path().join("source");
        fs::create_dir(&source_dir)?;

        // Add cache entries
        cache.put("key1", &source_dir)?;
        cache.put("key2", &source_dir)?;
        assert_eq!(cache.size(), 2);

        // Clear cache
        cache.clear()?;
        assert_eq!(cache.size(), 0);
        assert!(cache.get("key1").is_none());
        assert!(cache.get("key2").is_none());

        Ok(())
    }

    #[test]
    fn test_stats() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let cache = RepositoryCache::new(temp_dir.path())?;

        let stats = cache.stats();
        assert_eq!(stats.total_repositories, 0);
        assert_eq!(stats.total_size_bytes, 0);

        // Add a cache entry with some content
        let source_dir = temp_dir.path().join("source");
        fs::create_dir(&source_dir)?;

        let test_file = source_dir.join("test.txt");
        let content = b"test content with some size";
        let mut file = File::create(&test_file)?;
        file.write_all(content)?;

        cache.put("test-key", &source_dir)?;

        let stats = cache.stats();
        assert_eq!(stats.total_repositories, 1);
        assert!(stats.total_size_bytes >= content.len() as u64);

        Ok(())
    }

    #[test]
    fn test_copy_dir_all() -> Result<()> {
        let temp_dir = tempfile::tempdir()?;

        // Create source directory structure
        let src = temp_dir.path().join("src");
        fs::create_dir(&src)?;
        fs::create_dir(src.join("subdir"))?;

        let mut file1 = File::create(src.join("file1.txt"))?;
        file1.write_all(b"content1")?;

        let mut file2 = File::create(src.join("subdir").join("file2.txt"))?;
        file2.write_all(b"content2")?;

        // Copy to destination
        let dst = temp_dir.path().join("dst");
        copy_dir_all(&src, &dst)?;

        // Verify structure
        assert!(dst.exists());
        assert!(dst.join("file1.txt").exists());
        assert!(dst.join("subdir").exists());
        assert!(dst.join("subdir").join("file2.txt").exists());

        // Verify contents
        let content1 = fs::read_to_string(dst.join("file1.txt"))?;
        assert_eq!(content1, "content1");

        let content2 = fs::read_to_string(dst.join("subdir").join("file2.txt"))?;
        assert_eq!(content2, "content2");

        Ok(())
    }
}
