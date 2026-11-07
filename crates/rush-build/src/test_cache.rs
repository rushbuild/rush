// Rush Build - Test Result Cache
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Test result caching for fast test execution.
//!
//! Caches test results based on test binary hash and dependencies,
//! avoiding expensive test re-execution when nothing has changed.

use anyhow::Result;
use blake3::Hasher;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Parameters for storing a test result.
#[derive(Debug, Clone)]
pub struct TestResultParams {
    /// Test label (e.g., "//pkg:test").
    pub label: String,
    /// Path to test binary.
    pub binary_path: PathBuf,
    /// Whether the test passed.
    pub passed: bool,
    /// Test exit code.
    pub exit_code: i32,
    /// Test duration in milliseconds.
    pub duration_ms: u64,
    /// Test stdout.
    pub stdout: String,
    /// Test stderr.
    pub stderr: String,
}

/// Cached test result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedTestResult {
    /// Test label (e.g., "//pkg:test").
    pub label: String,

    /// Whether the test passed.
    pub passed: bool,

    /// Test exit code.
    pub exit_code: i32,

    /// Test duration in milliseconds.
    pub duration_ms: u64,

    /// Test stdout.
    pub stdout: String,

    /// Test stderr.
    pub stderr: String,

    /// Hash of test binary.
    pub binary_hash: String,

    /// Timestamp when result was cached.
    pub cached_at: SystemTime,

    /// Number of times this cached result was used.
    pub hit_count: usize,
}

/// Test result cache for avoiding redundant test execution.
///
/// Stores test results keyed by test label and binary hash.
pub struct TestCache {
    /// Cache directory.
    cache_dir: PathBuf,

    /// In-memory cache of test results.
    cache: HashMap<String, CachedTestResult>,

    /// Cache statistics.
    stats: TestCacheStats,
}

/// Test cache statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TestCacheStats {
    /// Total test lookups.
    pub lookups: usize,

    /// Cache hits.
    pub hits: usize,

    /// Cache misses.
    pub misses: usize,

    /// Tests skipped due to cache.
    pub tests_skipped: usize,

    /// Time saved by cache (ms).
    pub time_saved_ms: u64,
}

impl TestCache {
    /// Create a new test cache.
    pub fn new(cache_dir: PathBuf) -> Self {
        let cache = Self::load_cache(&cache_dir).unwrap_or_default();

        TestCache {
            cache_dir,
            cache,
            stats: TestCacheStats::default(),
        }
    }

    /// Check if a test result is cached.
    ///
    /// Returns cached result if test binary hasn't changed.
    pub fn get(&mut self, label: &str, binary_path: &Path) -> Option<CachedTestResult> {
        self.stats.lookups += 1;

        // Compute hash of test binary
        let binary_hash = match self.hash_file(binary_path) {
            Ok(hash) => hash,
            Err(_) => return None,
        };

        // Check if we have a cached result
        let cache_key = format!("{}:{}", label, binary_hash);
        if let Some(mut cached) = self.cache.get(&cache_key).cloned() {
            // Verify cached result is still valid
            if cached.binary_hash == binary_hash {
                self.stats.hits += 1;
                self.stats.tests_skipped += 1;
                self.stats.time_saved_ms += cached.duration_ms;

                // Update hit count
                cached.hit_count += 1;
                self.cache.insert(cache_key, cached.clone());

                return Some(cached);
            }
        }

        self.stats.misses += 1;
        None
    }

    /// Store a test result in the cache.
    pub fn put(&mut self, params: TestResultParams) -> Result<()> {
        let binary_hash = self.hash_file(&params.binary_path)?;

        let result = CachedTestResult {
            label: params.label.clone(),
            passed: params.passed,
            exit_code: params.exit_code,
            duration_ms: params.duration_ms,
            stdout: params.stdout,
            stderr: params.stderr,
            binary_hash: binary_hash.clone(),
            cached_at: SystemTime::now(),
            hit_count: 0,
        };

        let cache_key = format!("{}:{}", params.label, binary_hash);
        self.cache.insert(cache_key, result);

        Ok(())
    }

    /// Save cache to disk.
    pub fn save(&self) -> Result<()> {
        fs::create_dir_all(&self.cache_dir)?;

        let cache_file = self.cache_dir.join("test_results.json");
        let json = serde_json::to_string_pretty(&self.cache)?;
        fs::write(cache_file, json)?;

        Ok(())
    }

    /// Load cache from disk.
    fn load_cache(cache_dir: &Path) -> Result<HashMap<String, CachedTestResult>> {
        let cache_file = cache_dir.join("test_results.json");

        if !cache_file.exists() {
            return Ok(HashMap::new());
        }

        let json = fs::read_to_string(cache_file)?;
        let cache: HashMap<String, CachedTestResult> = serde_json::from_str(&json)?;

        Ok(cache)
    }

    /// Clear all cached test results.
    pub fn clear(&mut self) -> Result<()> {
        self.cache.clear();
        self.stats = TestCacheStats::default();

        let cache_file = self.cache_dir.join("test_results.json");
        if cache_file.exists() {
            fs::remove_file(cache_file)?;
        }

        Ok(())
    }

    /// Get cache statistics.
    pub fn stats(&self) -> &TestCacheStats {
        &self.stats
    }

    /// Get cache hit rate.
    pub fn hit_rate(&self) -> f64 {
        if self.stats.lookups == 0 {
            return 0.0;
        }
        (self.stats.hits as f64 / self.stats.lookups as f64) * 100.0
    }

    /// Compute BLAKE3 hash of a file.
    fn hash_file(&self, path: &Path) -> Result<String> {
        let contents = fs::read(path)?;
        let mut hasher = Hasher::new();
        hasher.update(&contents);
        Ok(hasher.finalize().to_hex().to_string())
    }

    /// Get number of cached results.
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Check if cache is empty.
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    /// Prune old cache entries (older than specified days).
    pub fn prune(&mut self, max_age_days: u64) -> Result<usize> {
        let now = SystemTime::now();
        let max_age = std::time::Duration::from_secs(max_age_days * 24 * 60 * 60);

        let mut pruned = 0;
        self.cache.retain(|_, result| {
            if let Ok(age) = now.duration_since(result.cached_at) {
                if age > max_age {
                    pruned += 1;
                    return false;
                }
            }
            true
        });

        Ok(pruned)
    }

    /// Get all cached results sorted by hit count (most used first).
    pub fn top_cached_tests(&self, limit: usize) -> Vec<CachedTestResult> {
        let mut results: Vec<_> = self.cache.values().cloned().collect();
        results.sort_by(|a, b| b.hit_count.cmp(&a.hit_count));
        results.into_iter().take(limit).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_cache_miss() {
        let temp_dir = TempDir::new().unwrap();
        let mut cache = TestCache::new(temp_dir.path().to_path_buf());

        let test_binary = temp_dir.path().join("test");
        fs::write(&test_binary, b"test binary").unwrap();

        let result = cache.get("//pkg:test", &test_binary);
        assert!(result.is_none());
        assert_eq!(cache.stats().misses, 1);
        assert_eq!(cache.stats().hits, 0);
    }

    #[test]
    fn test_cache_hit() {
        let temp_dir = TempDir::new().unwrap();
        let mut cache = TestCache::new(temp_dir.path().to_path_buf());

        let test_binary = temp_dir.path().join("test");
        fs::write(&test_binary, b"test binary").unwrap();

        // Store result
        cache
            .put(TestResultParams {
                label: "//pkg:test".to_string(),
                binary_path: test_binary.clone(),
                passed: true,
                exit_code: 0,
                duration_ms: 100,
                stdout: "output".to_string(),
                stderr: "".to_string(),
            })
            .unwrap();

        // Retrieve result
        let result = cache.get("//pkg:test", &test_binary);
        assert!(result.is_some());

        let cached = result.unwrap();
        assert!(cached.passed);
        assert_eq!(cached.duration_ms, 100);
        assert_eq!(cache.stats().hits, 1);
    }

    #[test]
    fn test_cache_invalidation() {
        let temp_dir = TempDir::new().unwrap();
        let mut cache = TestCache::new(temp_dir.path().to_path_buf());

        let test_binary = temp_dir.path().join("test");
        fs::write(&test_binary, b"test binary v1").unwrap();

        // Store result for v1
        cache
            .put(TestResultParams {
                label: "//pkg:test".to_string(),
                binary_path: test_binary.clone(),
                passed: true,
                exit_code: 0,
                duration_ms: 100,
                stdout: "v1 output".to_string(),
                stderr: "".to_string(),
            })
            .unwrap();

        // Modify binary (v2)
        fs::write(&test_binary, b"test binary v2").unwrap();

        // Should be cache miss since binary changed
        let result = cache.get("//pkg:test", &test_binary);
        assert!(result.is_none());
        assert_eq!(cache.stats().misses, 1);
    }

    #[test]
    fn test_persistence() {
        let temp_dir = TempDir::new().unwrap();
        let cache_dir = temp_dir.path().to_path_buf();

        let test_binary = temp_dir.path().join("test");
        fs::write(&test_binary, b"test binary").unwrap();

        // Create cache and store result
        {
            let mut cache = TestCache::new(cache_dir.clone());
            cache
                .put(TestResultParams {
                    label: "//pkg:test".to_string(),
                    binary_path: test_binary.clone(),
                    passed: true,
                    exit_code: 0,
                    duration_ms: 100,
                    stdout: "output".to_string(),
                    stderr: "".to_string(),
                })
                .unwrap();
            cache.save().unwrap();
        }

        // Load cache and verify result persisted
        {
            let mut cache = TestCache::new(cache_dir);
            let result = cache.get("//pkg:test", &test_binary);
            assert!(result.is_some());
        }
    }
}
