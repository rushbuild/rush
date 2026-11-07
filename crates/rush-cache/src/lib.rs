// Rush Cache - Action cache
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Content-addressable action cache.
//!
//! Stores action results keyed by action ID for incremental builds.
//!
//! Supports both in-memory and disk-backed storage:
//! - Memory: Fast, ephemeral (lost on exit)
//! - Disk: Persistent, survives restarts (RocksDB + CAS)

#![warn(missing_docs)]

pub mod cas;
pub mod disk;
pub mod roots;
pub mod staging;

use anyhow::Result;
use dashmap::DashMap;
use rush_action::ActionId;
use rush_exec::ActionResult;
use std::sync::Arc;

pub use disk::{CacheConfig, DiskCache};

#[cfg(feature = "remote")]
use rush_remote::HttpCache;

/// Cache backend selection
pub enum CacheBackend {
    /// In-memory only (fast, ephemeral)
    Memory,

    /// Disk-backed (persistent, survives restarts)
    Disk(Arc<DiskCache>),

    /// Disk + Remote HTTP cache (distributed builds)
    #[cfg(feature = "remote")]
    DiskAndRemote {
        /// Local disk cache
        disk: Arc<DiskCache>,
        /// Remote HTTP cache
        remote: Arc<HttpCache>,
    },

    /// Remote HTTP cache only (no local disk)
    #[cfg(feature = "remote")]
    Remote(Arc<HttpCache>),
}

/// Content-addressable storage for action results.
///
/// Maps ActionId → ActionResult with pluggable backend.
pub struct ActionCache {
    backend: CacheBackend,

    /// In-memory cache (always present for hot entries)
    memory: Arc<DashMap<ActionId, CachedResult>>,
}

/// Cached action result with metadata.
#[derive(Clone, Debug)]
struct CachedResult {
    /// The action result.
    result: ActionResult,

    /// When this was cached (for LRU eviction).
    _timestamp: std::time::SystemTime,
}

impl ActionCache {
    fn insert_memory(&self, action_id: ActionId, result: ActionResult) {
        let cached = CachedResult {
            result,
            _timestamp: std::time::SystemTime::now(),
        };
        self.memory.insert(action_id, cached);
    }

    /// Create a new in-memory action cache.
    pub fn new() -> Self {
        ActionCache {
            backend: CacheBackend::Memory,
            memory: Arc::new(DashMap::new()),
        }
    }

    /// Create a disk-backed action cache.
    pub fn with_disk(config: CacheConfig) -> Result<Self> {
        let disk = Arc::new(DiskCache::new(config)?);
        Ok(ActionCache {
            backend: CacheBackend::Disk(disk),
            memory: Arc::new(DashMap::new()),
        })
    }

    /// Create a cache with disk and remote HTTP cache.
    #[cfg(feature = "remote")]
    pub fn with_disk_and_remote(config: CacheConfig, remote_url: String) -> Result<Self> {
        let disk = Arc::new(DiskCache::new(config)?);
        let remote = Arc::new(HttpCache::new(remote_url)?);
        Ok(ActionCache {
            backend: CacheBackend::DiskAndRemote { disk, remote },
            memory: Arc::new(DashMap::new()),
        })
    }

    /// Create a cache with remote HTTP cache only (no disk).
    #[cfg(feature = "remote")]
    pub fn with_remote(remote_url: String) -> Result<Self> {
        let remote = Arc::new(HttpCache::new(remote_url)?);
        Ok(ActionCache {
            backend: CacheBackend::Remote(remote),
            memory: Arc::new(DashMap::new()),
        })
    }

    /// Return whether this cache has a local persistent backend.
    ///
    /// Callers use this to keep `--disk-cache=false` honest: a memory-only
    /// action cache must not open, populate, or garbage-collect the local CAS.
    pub fn uses_local_disk(&self) -> bool {
        match &self.backend {
            CacheBackend::Disk(_) => true,
            #[cfg(feature = "remote")]
            CacheBackend::DiskAndRemote { .. } => true,
            _ => false,
        }
    }

    /// Return whether durable publication has a persistent backend.
    pub fn supports_durable_publication(&self) -> bool {
        match &self.backend {
            CacheBackend::Memory => false,
            CacheBackend::Disk(_) => true,
            #[cfg(feature = "remote")]
            CacheBackend::DiskAndRemote { .. } | CacheBackend::Remote(_) => true,
        }
    }

    /// Look up a cached result for an action.
    ///
    /// Returns None if not cached.
    /// Checks memory first, then disk, then remote (if enabled).
    pub fn get(&self, action_id: &ActionId) -> Option<ActionResult> {
        // Try memory first
        if let Some(entry) = self.memory.get(action_id) {
            return Some(entry.result.clone());
        }

        // Try disk and/or remote based on backend
        match &self.backend {
            CacheBackend::Memory => None,

            CacheBackend::Disk(disk) => {
                // Block on async disk lookup
                let result = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(disk.get(action_id))
                });

                match result {
                    Ok(Some(result)) => {
                        // Populate memory cache for next time
                        let cached = CachedResult {
                            result: result.clone(),
                            _timestamp: std::time::SystemTime::now(),
                        };
                        self.memory.insert(action_id.clone(), cached);
                        Some(result)
                    }
                    _ => None,
                }
            }

            #[cfg(feature = "remote")]
            CacheBackend::DiskAndRemote { disk, remote } => {
                // Try disk first
                let disk_result = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(disk.get(action_id))
                });

                if let Ok(Some(result)) = disk_result {
                    let cached = CachedResult {
                        result: result.clone(),
                        _timestamp: std::time::SystemTime::now(),
                    };
                    self.memory.insert(action_id.clone(), cached);
                    return Some(result);
                }

                // Disk miss - try remote
                let remote_result = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(remote.get(action_id))
                });

                match remote_result {
                    Ok(Some(result)) => {
                        println!("   🌐 Remote cache hit");
                        // Populate both memory and disk for next time
                        let cached = CachedResult {
                            result: result.clone(),
                            _timestamp: std::time::SystemTime::now(),
                        };
                        self.memory.insert(action_id.clone(), cached);

                        // Write to disk asynchronously
                        let disk_clone = disk.clone();
                        let id = action_id.clone();
                        let res = result.clone();
                        tokio::spawn(async move {
                            let _ = disk_clone.put(id, res).await;
                        });

                        Some(result)
                    }
                    _ => None,
                }
            }

            #[cfg(feature = "remote")]
            CacheBackend::Remote(remote) => {
                // Remote-only - no disk fallback
                let result = tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(remote.get(action_id))
                });

                match result {
                    Ok(Some(result)) => {
                        println!("   🌐 Remote cache hit");
                        let cached = CachedResult {
                            result: result.clone(),
                            _timestamp: std::time::SystemTime::now(),
                        };
                        self.memory.insert(action_id.clone(), cached);
                        Some(result)
                    }
                    _ => None,
                }
            }
        }
    }

    /// Store an action result in the cache.
    pub fn put(&self, action_id: ActionId, result: ActionResult) {
        // Always update memory cache
        self.insert_memory(action_id.clone(), result.clone());

        // Also write to disk and/or remote based on backend
        match &self.backend {
            CacheBackend::Memory => {}

            CacheBackend::Disk(disk) => {
                // Fire and forget disk write (don't block on it)
                let disk = disk.clone();
                let id = action_id.clone();
                let res = result.clone();
                tokio::spawn(async move {
                    if let Err(e) = disk.put(id, res).await {
                        eprintln!("Warning: Failed to write to disk cache: {}", e);
                    }
                });
            }

            #[cfg(feature = "remote")]
            CacheBackend::DiskAndRemote { disk, remote } => {
                // Write to both disk and remote asynchronously
                let disk_clone = disk.clone();
                let remote_clone = Arc::clone(remote);
                let id = action_id.clone();
                let res = result.clone();

                tokio::spawn(async move {
                    // Write to disk
                    if let Err(e) = disk_clone.put(id.clone(), res.clone()).await {
                        eprintln!("Warning: Failed to write to disk cache: {}", e);
                    }

                    // Write to remote
                    if let Err(e) = remote_clone.put(&id, &res).await {
                        eprintln!("Warning: Failed to write to remote cache: {}", e);
                    }
                });
            }

            #[cfg(feature = "remote")]
            CacheBackend::Remote(remote) => {
                // Write to remote only
                let remote_clone = Arc::clone(remote);
                let id = action_id.clone();
                let res = result.clone();

                tokio::spawn(async move {
                    if let Err(e) = remote_clone.put(&id, &res).await {
                        eprintln!("Warning: Failed to write to remote cache: {}", e);
                    }
                });
            }
        }
    }

    /// Store an action result and wait for every configured backend.
    ///
    /// Short-lived replay processes use this path so successful completion is
    /// also a durability boundary. The ordinary build path may retain the
    /// fire-and-forget behavior of [`Self::put`].
    pub async fn put_durable(&self, action_id: ActionId, result: ActionResult) -> Result<()> {
        match &self.backend {
            CacheBackend::Memory => {}
            CacheBackend::Disk(disk) => {
                disk.put_durable(action_id.clone(), result.clone()).await?;
            }
            #[cfg(feature = "remote")]
            CacheBackend::DiskAndRemote { disk, remote } => {
                let disk_write = disk.put_durable(action_id.clone(), result.clone());
                let remote_write = remote.put(&action_id, &result);
                tokio::try_join!(disk_write, remote_write)?;
            }
            #[cfg(feature = "remote")]
            CacheBackend::Remote(remote) => {
                remote.put(&action_id, &result).await?;
            }
        }
        self.insert_memory(action_id, result);
        Ok(())
    }

    /// Evict least-recently-used entries until the disk cache is under its
    /// configured `max_size`.
    ///
    /// `DiskCache::gc()` has always existed and nothing ever called it, so
    /// the configured limit was decorative: the cache on one corpus host
    /// reached 433 GB against a 10 GB limit, filled the disk, and aborted a
    /// running sweep. Memory-only and remote-only backends have nothing to
    /// collect.
    pub async fn gc_disk(&self) -> anyhow::Result<()> {
        match &self.backend {
            CacheBackend::Disk(disk) => disk.gc().await,
            #[cfg(feature = "remote")]
            CacheBackend::DiskAndRemote { disk, .. } => disk.gc().await,
            _ => Ok(()),
        }
    }

    /// Check if an action is cached.
    pub fn contains(&self, action_id: &ActionId) -> bool {
        self.memory.contains_key(action_id)
    }

    /// Get the number of cached actions.
    pub fn size(&self) -> usize {
        self.memory.len()
    }

    /// Clear all cached entries.
    pub fn clear(&self) {
        self.memory.clear();
    }
}

impl Default for ActionCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod backend_tests {
    use super::*;

    #[test]
    fn memory_cache_does_not_claim_local_disk() {
        assert!(!ActionCache::new().uses_local_disk());
    }

    #[test]
    fn disk_cache_claims_local_disk() {
        let root = tempfile::tempdir().unwrap();
        let cache = ActionCache::with_disk(CacheConfig {
            cache_root: root.path().to_path_buf(),
            ..CacheConfig::default()
        })
        .unwrap();

        assert!(cache.uses_local_disk());
    }

    #[cfg(feature = "remote")]
    #[test]
    fn remote_cache_does_not_claim_local_disk() {
        let cache = ActionCache::with_remote("http://127.0.0.1:1".to_string()).unwrap();

        assert!(!cache.uses_local_disk());
    }

    #[cfg(feature = "remote")]
    #[test]
    fn combined_cache_claims_local_disk() {
        let root = tempfile::tempdir().unwrap();
        let cache = ActionCache::with_disk_and_remote(
            CacheConfig {
                cache_root: root.path().to_path_buf(),
                ..CacheConfig::default()
            },
            "http://127.0.0.1:1".to_string(),
        )
        .unwrap();

        assert!(cache.uses_local_disk());
    }
}
