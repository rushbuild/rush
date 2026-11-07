// Disk-backed cache implementation
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Disk-backed action cache using simple file-based storage.
//!
//! Each action result is stored as a separate file named by its ActionId hash.
//! This is simpler than RocksDB and avoids GCC 15 compatibility issues.

use anyhow::{Context, Result};
use dashmap::DashMap;
use rush_action::ActionId;
use rush_exec::ActionResult;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use walkdir;

/// Configuration for disk cache.
#[derive(Debug, Clone)]
pub struct CacheConfig {
    /// Root directory for cache storage.
    pub cache_root: PathBuf,

    /// Maximum cache size in bytes (0 = unlimited).
    pub max_size: u64,

    /// Time-to-live for cache entries (seconds, 0 = forever).
    pub ttl_seconds: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        // One authority — see crate::roots. Do not re-derive this.
        let cache_root = crate::roots::action_cache_root();

        CacheConfig {
            cache_root,
            max_size: 10 * 1024 * 1024 * 1024, // 10 GB
            ttl_seconds: 30 * 24 * 3600,       // 30 days
        }
    }
}

/// Disk-backed action cache using file-based storage.
pub struct DiskCache {
    config: CacheConfig,
    memory_cache: Arc<DashMap<ActionId, ActionResult>>,
}

impl DiskCache {
    /// Create new disk cache with given configuration.
    pub fn new(config: CacheConfig) -> Result<Self> {
        // Create cache directory if it doesn't exist
        std::fs::create_dir_all(&config.cache_root).context("Failed to create cache directory")?;

        Ok(DiskCache {
            config,
            memory_cache: Arc::new(DashMap::new()),
        })
    }

    /// Get file path for an action ID.
    fn cache_file_path(&self, action_id: &ActionId) -> PathBuf {
        self.config.cache_root.join(format!("{}.cache", action_id))
    }

    /// Get cached result for action.
    pub async fn get(&self, action_id: &ActionId) -> Result<Option<ActionResult>> {
        // Check memory cache first
        if let Some(result) = self.memory_cache.get(action_id) {
            return Ok(Some(result.clone()));
        }

        // Try loading from disk
        let cache_path = self.cache_file_path(action_id);
        if !cache_path.exists() {
            return Ok(None);
        }

        let mut file = fs::File::open(&cache_path).context("Failed to open cache file")?;
        let mut contents = Vec::new();
        file.read_to_end(&mut contents)
            .context("Failed to read cache file")?;
        // Refresh mtime so a bounded cache's gc sees this entry as used. An
        // unlimited cache never evicts, so touching every first disk hit per
        // process would be a pure filesystem mutation. Best-effort: a stale
        // mtime can only make an entry a better eviction candidate.
        if self.config.max_size != 0 {
            let _ = file
                .set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()));
        }

        let result: ActionResult =
            bincode::deserialize(&contents).context("Failed to deserialize cached result")?;

        // Populate memory cache for future reads
        self.memory_cache.insert(action_id.clone(), result.clone());

        Ok(Some(result))
    }

    /// Store action result in cache.
    pub async fn put(&self, action_id: ActionId, result: ActionResult) -> Result<()> {
        self.put_inner(action_id, result, false).await
    }

    /// Store an action result and fsync both its file and containing directory
    /// before returning.
    pub async fn put_durable(&self, action_id: ActionId, result: ActionResult) -> Result<()> {
        self.put_inner(action_id, result, true).await
    }

    async fn put_inner(
        &self,
        action_id: ActionId,
        result: ActionResult,
        durable: bool,
    ) -> Result<()> {
        // Persist to disk atomically: a crash mid-write must never leave a
        // torn entry (torn entries deserialize-fail into misses today, but
        // atomic publication removes even the dead files, and CAS blobs
        // are published BEFORE the ActionResult that references them).
        let cache_path = self.cache_file_path(&action_id);
        let serialized = bincode::serialize(&result).context("Failed to serialize result")?;

        let tmp_path = self.config.cache_root.join(format!(
            ".tmp-{}-{}-{}",
            std::process::id(),
            action_id,
            unique_suffix()
        ));
        let publication = (|| -> Result<()> {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options
                .open(&tmp_path)
                .context("Failed to create cache temp")?;
            file.write_all(&serialized)
                .context("Failed to write cache temp")?;
            if durable {
                file.sync_all().context("Failed to sync cache temp")?;
            }
            drop(file);
            fs::rename(&tmp_path, &cache_path).context("Failed to publish cache entry")?;
            if durable {
                fs::File::open(&self.config.cache_root)
                    .context("Failed to open cache directory")?
                    .sync_all()
                    .context("Failed to sync cache directory")?;
            }
            Ok(())
        })();
        if publication.is_err() {
            let _ = fs::remove_file(&tmp_path);
        }
        publication?;

        // Admit the fast in-process copy only after durable publication. A
        // failed write must remain a miss even if this DiskCache survives.
        self.memory_cache.insert(action_id, result);

        Ok(())
    }

    /// Check if action result is cached.
    pub async fn contains(&self, action_id: &ActionId) -> Result<bool> {
        Ok(self.memory_cache.contains_key(action_id))
    }

    /// Clear all cached entries.
    pub async fn clear(&self) -> Result<()> {
        self.memory_cache.clear();
        Ok(())
    }

    /// Get current cache size by scanning cache directory.
    pub async fn size(&self) -> Result<u64> {
        let cache_dir = self.config.cache_root.clone();
        tokio::task::spawn_blocking(move || {
            // Depth 1, .cache files only: the ActionResult store's own
            // entries. The CAS lives under cas/ in the same root with its
            // own, much larger budget; counting its blobs here made this
            // store's 10 GB limit apply to both.
            walkdir::WalkDir::new(&cache_dir)
                .max_depth(1)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.path().extension().is_some_and(|x| x == "cache"))
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .sum()
        })
        .await
        .context("Failed to calculate cache size")
    }

    /// Run garbage collection to enforce cache size limits.
    ///
    /// Removes least-recently-used entries until cache is under max_size.
    pub async fn gc(&self) -> Result<()> {
        // `CacheConfig` defines zero as unlimited. In particular, do not
        // interpret that sentinel as a zero-byte budget and evict everything.
        if self.config.max_size == 0 {
            return Ok(());
        }
        let current_size = self.size().await?;
        let max_size = self.config.max_size;

        if current_size <= max_size {
            return Ok(()); // Under limit
        }

        let cache_dir = self.config.cache_root.clone();
        let target_free = current_size - max_size;

        tokio::task::spawn_blocking(move || {
            // Get all cache entries with access times
            // Same scope as size(): depth 1, .cache files only. The
            // recursive walk deleted CAS blobs under this store's 10 GB
            // budget - a fresh 415 GB CAS was shredded to 6 GB by the
            // ActionResult gc while the CAS's own 50 GiB gc found nothing
            // left to do. Each store enforces its own budget over its own
            // files. LRU by mtime, not atime: relatime mounts leave atime
            // stale, and both stores refresh mtime on use.
            let mut entries: Vec<_> = walkdir::WalkDir::new(&cache_dir)
                .max_depth(1)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
                .filter(|e| e.path().extension().is_some_and(|x| x == "cache"))
                .filter_map(|e| {
                    let metadata = e.metadata().ok()?;
                    let used = metadata.modified().ok()?;
                    Some((e.path().to_path_buf(), metadata.len(), used))
                })
                .collect();

            // Sort by last use (oldest first)
            entries.sort_by_key(|(_, _, used)| *used);

            // Delete oldest until under limit
            let mut freed = 0u64;
            for (path, size, _) in entries {
                if freed >= target_free {
                    break;
                }
                if fs::remove_file(&path).is_ok() {
                    freed += size;
                }
            }

            Ok(())
        })
        .await
        .context("GC task failed")?
    }
}

fn unique_suffix() -> String {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{time}-{sequence}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(action_id: ActionId) -> ActionResult {
        ActionResult {
            action_id,
            exit_code: 0,
            stdout: vec![],
            stderr: vec![],
            outputs: vec![],
            output_records: Vec::new(),
            duration: std::time::Duration::ZERO,
            cached: false,
        }
    }

    /// The ActionResult gc owns only the top-level .cache files. It once
    /// walked the whole cache root recursively, so its 10 GB budget was
    /// applied to the CAS too and shredded a fresh 415 GB blob store to
    /// 6 GB. A CAS blob must survive this gc no matter how far over
    /// budget the .cache files are; older .cache entries must go first.
    #[tokio::test]
    async fn gc_scopes_to_cache_files_and_spares_the_cas() {
        let temp_dir = tempfile::tempdir().unwrap();
        let root = temp_dir.path().to_path_buf();
        let config = CacheConfig {
            cache_root: root.clone(),
            max_size: 4096,
            ttl_seconds: 3600,
        };
        let cache = DiskCache::new(config).unwrap();

        // A CAS blob under the same root, far larger than the budget.
        let cas_shard = root.join("cas").join("ab");
        std::fs::create_dir_all(&cas_shard).unwrap();
        let blob = cas_shard.join("ab".repeat(32));
        std::fs::write(&blob, vec![b'x'; 64 * 1024]).unwrap();

        // Two .cache entries over budget, with distinct ages.
        let old_entry = root.join("old.cache");
        let new_entry = root.join("new.cache");
        std::fs::write(&old_entry, vec![b'o'; 4096]).unwrap();
        std::fs::write(&new_entry, vec![b'n'; 4096]).unwrap();
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(600);
        let f = std::fs::File::options().write(true).open(&old_entry).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(past)).unwrap();
        drop(f);

        cache.gc().await.unwrap();

        assert!(blob.exists(), "the gc must never touch CAS blobs");
        assert!(!old_entry.exists(), "the older .cache entry is evicted first");
        assert!(new_entry.exists(), "eviction stops once under budget");
    }

    #[tokio::test]
    async fn test_disk_cache_basic() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config = CacheConfig {
            cache_root: temp_dir.path().to_path_buf(),
            max_size: 1024 * 1024,
            ttl_seconds: 3600,
        };

        let cache = DiskCache::new(config).unwrap();

        let hash = blake3::hash(b"test_action");
        let action_id = ActionId::new(hash);
        let result = result(action_id.clone());

        // Should not be cached initially
        assert!(!cache.contains(&action_id).await.unwrap());

        // Store result
        cache.put(action_id.clone(), result.clone()).await.unwrap();

        // Should be cached now
        assert!(cache.contains(&action_id).await.unwrap());

        // Should retrieve same result
        let cached = cache.get(&action_id).await.unwrap();
        assert!(cached.is_some());
        assert_eq!(cached.unwrap().exit_code, 0);
    }

    #[tokio::test]
    async fn unlimited_disk_hits_do_not_refresh_mtime() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config = CacheConfig {
            cache_root: temp_dir.path().to_path_buf(),
            max_size: 0,
            ttl_seconds: 0,
        };
        let id = ActionId::new(blake3::hash(b"unlimited-hit"));
        DiskCache::new(config.clone())
            .unwrap()
            .put(id.clone(), result(id.clone()))
            .await
            .unwrap();

        let path = temp_dir.path().join(format!("{id}.cache"));
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(600);
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(past))
            .unwrap();
        drop(file);
        let before = fs::metadata(&path).unwrap().modified().unwrap();

        assert!(DiskCache::new(config)
            .unwrap()
            .get(&id)
            .await
            .unwrap()
            .is_some());
        assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), before);
    }

    #[tokio::test]
    async fn bounded_disk_hits_refresh_mtime_for_lru() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config = CacheConfig {
            cache_root: temp_dir.path().to_path_buf(),
            max_size: 1024 * 1024,
            ttl_seconds: 0,
        };
        let id = ActionId::new(blake3::hash(b"bounded-hit"));
        DiskCache::new(config.clone())
            .unwrap()
            .put(id.clone(), result(id.clone()))
            .await
            .unwrap();

        let path = temp_dir.path().join(format!("{id}.cache"));
        let past = std::time::SystemTime::now() - std::time::Duration::from_secs(600);
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(past))
            .unwrap();
        drop(file);
        let before = fs::metadata(&path).unwrap().modified().unwrap();

        assert!(DiskCache::new(config)
            .unwrap()
            .get(&id)
            .await
            .unwrap()
            .is_some());
        assert!(fs::metadata(path).unwrap().modified().unwrap() > before);
    }

    #[tokio::test]
    async fn unlimited_gc_preserves_entries() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config = CacheConfig {
            cache_root: temp_dir.path().to_path_buf(),
            max_size: 0,
            ttl_seconds: 0,
        };
        let id = ActionId::new(blake3::hash(b"unlimited-gc"));
        let cache = DiskCache::new(config).unwrap();
        cache.put(id.clone(), result(id.clone())).await.unwrap();

        cache.gc().await.unwrap();

        assert!(temp_dir.path().join(format!("{id}.cache")).exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn durable_put_is_private_and_survives_reopen() {
        use std::os::unix::fs::PermissionsExt;

        let temp_dir = tempfile::tempdir().unwrap();
        let config = CacheConfig {
            cache_root: temp_dir.path().to_path_buf(),
            max_size: 0,
            ttl_seconds: 0,
        };
        let id = ActionId::new(blake3::hash(b"durable"));
        DiskCache::new(config.clone())
            .unwrap()
            .put_durable(id.clone(), result(id.clone()))
            .await
            .unwrap();

        let path = temp_dir.path().join(format!("{id}.cache"));
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(DiskCache::new(config)
            .unwrap()
            .get(&id)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_durable_puts_do_not_share_a_temporary_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let config = CacheConfig {
            cache_root: temp_dir.path().to_path_buf(),
            max_size: 0,
            ttl_seconds: 0,
        };
        let cache = Arc::new(DiskCache::new(config).unwrap());
        let id = ActionId::new(blake3::hash(b"concurrent-durable"));
        let mut writers = Vec::new();
        for _ in 0..32 {
            let cache = Arc::clone(&cache);
            let id = id.clone();
            writers.push(tokio::spawn(async move {
                cache.put_durable(id.clone(), result(id)).await.unwrap();
            }));
        }
        for writer in writers {
            writer.await.unwrap();
        }

        assert!(cache.get(&id).await.unwrap().is_some());
        assert!(fs::read_dir(temp_dir.path()).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".tmp-")));
    }
}
