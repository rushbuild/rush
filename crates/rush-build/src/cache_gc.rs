use anyhow::{Context, Result};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::interval;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvictionPolicy {
    /// Least Recently Used - evict oldest accessed entries
    LRU,
    /// Least Frequently Used - evict least accessed entries
    LFU,
    /// First In First Out - evict oldest created entries
    FIFO,
    /// Time To Live - evict by age threshold
    TTL,
    /// Hybrid - combines LRU with size and age constraints
    Hybrid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GCConfig {
    /// Maximum cache size in bytes (0 = unlimited)
    pub max_size_bytes: u64,
    /// Maximum number of entries (0 = unlimited)
    pub max_entries: usize,
    /// Maximum age in seconds before eviction (0 = unlimited)
    pub max_age_secs: u64,
    /// Target size after GC (as percentage of max_size_bytes)
    pub target_size_percent: u8,
    /// Background GC run interval in seconds
    pub gc_interval_secs: u64,
    /// Eviction policy to use
    pub eviction_policy: EvictionPolicy,
    /// Enable incremental GC (process in batches)
    pub incremental: bool,
    /// Max entries to process per incremental batch
    pub incremental_batch_size: usize,
    /// Enable background GC thread
    pub background_gc: bool,
}

impl Default for GCConfig {
    fn default() -> Self {
        Self {
            max_size_bytes: 10 * 1024 * 1024 * 1024, // 10 GB
            max_entries: 100_000,
            max_age_secs: 30 * 24 * 60 * 60, // 30 days
            target_size_percent: 80,
            gc_interval_secs: 3600, // 1 hour
            eviction_policy: EvictionPolicy::LRU,
            incremental: true,
            incremental_batch_size: 1000,
            background_gc: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    /// Unique identifier for this entry
    pub key: String,
    /// File path or identifier
    pub path: PathBuf,
    /// Size in bytes
    pub size: u64,
    /// Creation timestamp (seconds since epoch)
    pub created_at: u64,
    /// Last access timestamp (seconds since epoch)
    pub last_accessed: u64,
    /// Access count for LFU
    pub access_count: u64,
    /// Whether this entry is protected from eviction
    pub protected: bool,
    /// Cache level (0 = local, 1 = remote, etc.)
    pub level: u8,
}

impl CacheEntry {
    fn age_secs(&self, now: u64) -> u64 {
        now.saturating_sub(self.created_at)
    }

    fn _idle_secs(&self, now: u64) -> u64 {
        now.saturating_sub(self.last_accessed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EvictionCandidate {
    key: String,
    score: u64,
    size: u64,
}

impl Ord for EvictionCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        other.score.cmp(&self.score)
    }
}

impl PartialOrd for EvictionCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheStats {
    /// Total size in bytes
    pub total_size: u64,
    /// Number of entries
    pub entry_count: usize,
    /// Number of hits
    pub hits: u64,
    /// Number of misses
    pub misses: u64,
    /// Number of evictions
    pub evictions: u64,
    /// Total bytes evicted
    pub bytes_evicted: u64,
    /// Last GC run timestamp
    pub last_gc_run: u64,
    /// Last GC duration in milliseconds
    pub last_gc_duration_ms: u64,
    /// Number of protected entries
    pub protected_count: usize,
}

impl CacheStats {
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            0.0
        } else {
            self.hits as f64 / total as f64
        }
    }

    pub fn avg_entry_size(&self) -> u64 {
        if self.entry_count == 0 {
            0
        } else {
            self.total_size / self.entry_count as u64
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EvictionResult {
    /// Number of entries evicted
    pub entries_evicted: usize,
    /// Total bytes freed
    pub bytes_freed: u64,
    /// Duration of GC run in milliseconds
    pub duration_ms: u64,
    /// Keys that were evicted
    pub evicted_keys: Vec<String>,
    /// Whether GC was incremental
    pub incremental: bool,
    /// Whether more work remains
    pub has_more: bool,
}

enum GCCommand {
    RunGC {
        force: bool,
        reply: oneshot::Sender<Result<EvictionResult>>,
    },
    UpdateEntry {
        key: String,
        entry: CacheEntry,
    },
    RemoveEntry {
        key: String,
    },
    RecordHit {
        key: String,
    },
    RecordMiss,
    GetStats {
        reply: oneshot::Sender<CacheStats>,
    },
    Shutdown,
}

struct CacheGCInner {
    config: GCConfig,
    entries: HashMap<String, CacheEntry>,
    stats: CacheStats,
    protected_keys: HashSet<String>,
    _last_incremental_pos: usize,
}

impl CacheGCInner {
    fn new(config: GCConfig) -> Self {
        Self {
            config,
            entries: HashMap::new(),
            stats: CacheStats::default(),
            protected_keys: HashSet::new(),
            _last_incremental_pos: 0,
        }
    }

    fn needs_gc(&self) -> bool {
        if self.config.max_size_bytes > 0 && self.stats.total_size > self.config.max_size_bytes {
            return true;
        }
        if self.config.max_entries > 0 && self.stats.entry_count > self.config.max_entries {
            return true;
        }
        false
    }

    fn calculate_target_size(&self) -> u64 {
        if self.config.max_size_bytes == 0 {
            return u64::MAX;
        }
        (self.config.max_size_bytes * self.config.target_size_percent as u64) / 100
    }

    fn calculate_target_entries(&self) -> usize {
        if self.config.max_entries == 0 {
            return usize::MAX;
        }
        (self.config.max_entries * self.config.target_size_percent as usize) / 100
    }

    fn build_eviction_candidates(&self, now: u64) -> BinaryHeap<EvictionCandidate> {
        let mut heap = BinaryHeap::new();

        for (key, entry) in &self.entries {
            if entry.protected {
                continue;
            }

            let score = self.calculate_eviction_score(entry, now);
            heap.push(EvictionCandidate {
                key: key.clone(),
                score,
                size: entry.size,
            });
        }

        heap
    }

    fn calculate_eviction_score(&self, entry: &CacheEntry, now: u64) -> u64 {
        match self.config.eviction_policy {
            EvictionPolicy::LRU => {
                // Lower score = evict first (oldest access)
                entry.last_accessed
            }
            EvictionPolicy::LFU => {
                // Lower score = evict first (least frequently used)
                entry.access_count
            }
            EvictionPolicy::FIFO => {
                // Lower score = evict first (oldest creation)
                entry.created_at
            }
            EvictionPolicy::TTL => {
                // Higher score = evict first (oldest)
                now.saturating_sub(entry.created_at)
            }
            EvictionPolicy::Hybrid => {
                // Combine recency and frequency
                // Score = last_accessed + (access_count * weight)
                let frequency_weight = 3600; // 1 hour per access
                entry
                    .last_accessed
                    .saturating_add(entry.access_count.saturating_mul(frequency_weight))
            }
        }
    }

    fn run_gc(&mut self, force: bool) -> Result<EvictionResult> {
        let start = SystemTime::now();
        let now = start.duration_since(UNIX_EPOCH)?.as_secs();

        if !force && !self.needs_gc() {
            return Ok(EvictionResult::default());
        }

        let target_size = self.calculate_target_size();
        let target_entries = self.calculate_target_entries();

        let mut result = EvictionResult {
            incremental: self.config.incremental,
            ..Default::default()
        };

        // Age-based eviction first
        if self.config.max_age_secs > 0 {
            self.evict_by_age(now, &mut result)?;
        }

        // Size/count-based eviction
        if self.stats.total_size > target_size || self.stats.entry_count > target_entries {
            if self.config.incremental {
                self.incremental_evict(target_size, target_entries, &mut result)?;
            } else {
                self.full_evict(target_size, target_entries, &mut result)?;
            }
        }

        let duration = SystemTime::now().duration_since(start)?;
        result.duration_ms = duration.as_millis() as u64;

        self.stats.last_gc_run = now;
        self.stats.last_gc_duration_ms = result.duration_ms;
        self.stats.evictions += result.entries_evicted as u64;
        self.stats.bytes_evicted += result.bytes_freed;

        Ok(result)
    }

    fn evict_by_age(&mut self, now: u64, result: &mut EvictionResult) -> Result<()> {
        let max_age = self.config.max_age_secs;
        let keys_to_evict: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, entry)| !entry.protected && entry.age_secs(now) > max_age)
            .map(|(key, _)| key.clone())
            .collect();

        for key in keys_to_evict {
            if let Some(entry) = self.entries.remove(&key) {
                self.evict_entry(&key, &entry, result)?;
            }
        }

        Ok(())
    }

    fn full_evict(
        &mut self,
        target_size: u64,
        target_entries: usize,
        result: &mut EvictionResult,
    ) -> Result<()> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let mut candidates = self.build_eviction_candidates(now);

        while (self.stats.total_size > target_size || self.stats.entry_count > target_entries)
            && !candidates.is_empty()
        {
            if let Some(candidate) = candidates.pop() {
                if let Some(entry) = self.entries.remove(&candidate.key) {
                    self.evict_entry(&candidate.key, &entry, result)?;
                }
            }
        }

        Ok(())
    }

    fn incremental_evict(
        &mut self,
        target_size: u64,
        target_entries: usize,
        result: &mut EvictionResult,
    ) -> Result<()> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let mut candidates = self.build_eviction_candidates(now);

        let batch_size = self.config.incremental_batch_size;
        let mut processed = 0;

        while (self.stats.total_size > target_size || self.stats.entry_count > target_entries)
            && processed < batch_size
            && !candidates.is_empty()
        {
            if let Some(candidate) = candidates.pop() {
                if let Some(entry) = self.entries.remove(&candidate.key) {
                    self.evict_entry(&candidate.key, &entry, result)?;
                    processed += 1;
                }
            }
        }

        result.has_more = !candidates.is_empty()
            && (self.stats.total_size > target_size || self.stats.entry_count > target_entries);

        Ok(())
    }

    fn evict_entry(
        &mut self,
        key: &str,
        entry: &CacheEntry,
        result: &mut EvictionResult,
    ) -> Result<()> {
        // Delete the physical file if it exists
        if entry.path.exists() {
            std::fs::remove_file(&entry.path)
                .with_context(|| format!("Failed to remove cache file: {:?}", entry.path))?;
        }

        self.stats.total_size = self.stats.total_size.saturating_sub(entry.size);
        self.stats.entry_count = self.stats.entry_count.saturating_sub(1);

        result.entries_evicted += 1;
        result.bytes_freed += entry.size;
        result.evicted_keys.push(key.to_string());

        Ok(())
    }

    fn update_entry(&mut self, key: String, mut entry: CacheEntry) {
        if let Some(old_entry) = self.entries.get(&key) {
            self.stats.total_size = self.stats.total_size.saturating_sub(old_entry.size);
        } else {
            self.stats.entry_count += 1;
        }

        if entry.protected {
            self.protected_keys.insert(key.clone());
            self.stats.protected_count = self.protected_keys.len();
        }

        self.stats.total_size += entry.size;
        entry.last_accessed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        self.entries.insert(key, entry);
    }

    fn remove_entry(&mut self, key: &str) {
        if let Some(entry) = self.entries.remove(key) {
            self.stats.total_size = self.stats.total_size.saturating_sub(entry.size);
            self.stats.entry_count = self.stats.entry_count.saturating_sub(1);
            self.protected_keys.remove(key);
            self.stats.protected_count = self.protected_keys.len();
        }
    }

    fn record_hit(&mut self, key: &str) {
        self.stats.hits += 1;
        if let Some(entry) = self.entries.get_mut(key) {
            entry.last_accessed = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            entry.access_count += 1;
        }
    }

    fn record_miss(&mut self) {
        self.stats.misses += 1;
    }
}

pub struct CacheGC {
    tx: mpsc::UnboundedSender<GCCommand>,
    handle: Option<JoinHandle<()>>,
}

impl CacheGC {
    pub fn new(config: GCConfig) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(Self::run_gc_loop(config, rx));

        Self {
            tx,
            handle: Some(handle),
        }
    }

    async fn run_gc_loop(config: GCConfig, mut rx: mpsc::UnboundedReceiver<GCCommand>) {
        let inner = Arc::new(RwLock::new(CacheGCInner::new(config.clone())));

        let mut gc_interval = if config.background_gc && config.gc_interval_secs > 0 {
            Some(interval(Duration::from_secs(config.gc_interval_secs)))
        } else {
            None
        };

        loop {
            tokio::select! {
                Some(cmd) = rx.recv() => {
                    match cmd {
                        GCCommand::RunGC { force, reply } => {
                            let result = inner.write().run_gc(force);
                            let _ = reply.send(result);
                        }
                        GCCommand::UpdateEntry { key, entry } => {
                            inner.write().update_entry(key, entry);
                        }
                        GCCommand::RemoveEntry { key } => {
                            inner.write().remove_entry(&key);
                        }
                        GCCommand::RecordHit { key } => {
                            inner.write().record_hit(&key);
                        }
                        GCCommand::RecordMiss => {
                            inner.write().record_miss();
                        }
                        GCCommand::GetStats { reply } => {
                            let stats = inner.read().stats.clone();
                            let _ = reply.send(stats);
                        }
                        GCCommand::Shutdown => {
                            break;
                        }
                    }
                }
                Some(_) = async {
                    match &mut gc_interval {
                        Some(interval) => Some(interval.tick().await),
                        None => None,
                    }
                } => {
                    // Background GC tick
                    let _ = inner.write().run_gc(false);
                }
                else => break,
            }
        }
    }

    pub async fn run_gc(&self, force: bool) -> Result<EvictionResult> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(GCCommand::RunGC { force, reply: tx })
            .context("Failed to send GC command")?;
        rx.await.context("Failed to receive GC result")?
    }

    pub fn update_entry(&self, key: String, entry: CacheEntry) -> Result<()> {
        self.tx
            .send(GCCommand::UpdateEntry { key, entry })
            .context("Failed to send update command")
    }

    pub fn remove_entry(&self, key: String) -> Result<()> {
        self.tx
            .send(GCCommand::RemoveEntry { key })
            .context("Failed to send remove command")
    }

    pub fn record_hit(&self, key: String) -> Result<()> {
        self.tx
            .send(GCCommand::RecordHit { key })
            .context("Failed to send hit record")
    }

    pub fn record_miss(&self) -> Result<()> {
        self.tx
            .send(GCCommand::RecordMiss)
            .context("Failed to send miss record")
    }

    pub async fn get_stats(&self) -> Result<CacheStats> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(GCCommand::GetStats { reply: tx })
            .context("Failed to send stats request")?;
        rx.await.context("Failed to receive stats")
    }

    pub async fn shutdown(mut self) {
        let _ = self.tx.send(GCCommand::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
    }
}

#[derive(Clone)]
pub struct MultiLevelCache {
    local_gc: Arc<CacheGC>,
    remote_gc: Option<Arc<CacheGC>>,
}

impl MultiLevelCache {
    pub fn new(local_config: GCConfig, remote_config: Option<GCConfig>) -> Self {
        let local_gc = Arc::new(CacheGC::new(local_config));
        let remote_gc = remote_config.map(|cfg| Arc::new(CacheGC::new(cfg)));

        Self {
            local_gc,
            remote_gc,
        }
    }

    pub async fn run_gc(&self, level: u8, force: bool) -> Result<EvictionResult> {
        match level {
            0 => self.local_gc.run_gc(force).await,
            1 => {
                if let Some(remote) = &self.remote_gc {
                    remote.run_gc(force).await
                } else {
                    anyhow::bail!("Remote cache not configured")
                }
            }
            _ => anyhow::bail!("Invalid cache level: {}", level),
        }
    }

    pub async fn run_all_gc(&self, force: bool) -> Result<Vec<EvictionResult>> {
        let mut results = Vec::new();

        results.push(self.local_gc.run_gc(force).await?);

        if let Some(remote) = &self.remote_gc {
            results.push(remote.run_gc(force).await?);
        }

        Ok(results)
    }

    pub fn update_entry(&self, entry: CacheEntry) -> Result<()> {
        match entry.level {
            0 => self.local_gc.update_entry(entry.key.clone(), entry),
            1 => {
                if let Some(remote) = &self.remote_gc {
                    remote.update_entry(entry.key.clone(), entry)
                } else {
                    anyhow::bail!("Remote cache not configured")
                }
            }
            _ => anyhow::bail!("Invalid cache level: {}", entry.level),
        }
    }

    pub fn record_hit(&self, level: u8, key: String) -> Result<()> {
        match level {
            0 => self.local_gc.record_hit(key),
            1 => {
                if let Some(remote) = &self.remote_gc {
                    remote.record_hit(key)
                } else {
                    anyhow::bail!("Remote cache not configured")
                }
            }
            _ => anyhow::bail!("Invalid cache level: {}", level),
        }
    }

    pub fn record_miss(&self, level: u8) -> Result<()> {
        match level {
            0 => self.local_gc.record_miss(),
            1 => {
                if let Some(remote) = &self.remote_gc {
                    remote.record_miss()
                } else {
                    anyhow::bail!("Remote cache not configured")
                }
            }
            _ => anyhow::bail!("Invalid cache level: {}", level),
        }
    }

    pub async fn get_stats(&self, level: u8) -> Result<CacheStats> {
        match level {
            0 => self.local_gc.get_stats().await,
            1 => {
                if let Some(remote) = &self.remote_gc {
                    remote.get_stats().await
                } else {
                    anyhow::bail!("Remote cache not configured")
                }
            }
            _ => anyhow::bail!("Invalid cache level: {}", level),
        }
    }

    pub async fn get_all_stats(&self) -> Result<Vec<CacheStats>> {
        let mut stats = Vec::new();

        stats.push(self.local_gc.get_stats().await?);

        if let Some(remote) = &self.remote_gc {
            stats.push(remote.get_stats().await?);
        }

        Ok(stats)
    }
}

pub fn load_cache_metadata<P: AsRef<Path>>(path: P) -> Result<HashMap<String, CacheEntry>> {
    let content = std::fs::read_to_string(path)?;
    let entries: HashMap<String, CacheEntry> = serde_json::from_str(&content)?;
    Ok(entries)
}

pub fn save_cache_metadata<P: AsRef<Path>>(
    path: P,
    entries: &HashMap<String, CacheEntry>,
) -> Result<()> {
    let content = serde_json::to_string_pretty(entries)?;
    std::fs::write(path, content)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_entry(key: &str, size: u64, protected: bool) -> CacheEntry {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        CacheEntry {
            key: key.to_string(),
            path: PathBuf::from(format!("/tmp/cache/{}", key)),
            size,
            created_at: now,
            last_accessed: now,
            access_count: 0,
            protected,
            level: 0,
        }
    }

    #[tokio::test]
    async fn test_cache_gc_basic() {
        let config = GCConfig {
            max_size_bytes: 1000,
            max_entries: 5,
            background_gc: false,
            ..Default::default()
        };

        let gc = CacheGC::new(config);

        // Add some entries
        for i in 0..3 {
            let entry = create_test_entry(&format!("key{}", i), 200, false);
            gc.update_entry(entry.key.clone(), entry).unwrap();
        }

        let stats = gc.get_stats().await.unwrap();
        assert_eq!(stats.entry_count, 3);
        assert_eq!(stats.total_size, 600);

        gc.shutdown().await;
    }

    #[tokio::test]
    async fn test_lru_eviction() {
        let temp_dir = TempDir::new().unwrap();

        let config = GCConfig {
            max_size_bytes: 500,
            max_entries: 10,
            eviction_policy: EvictionPolicy::LRU,
            background_gc: false,
            incremental: false,
            ..Default::default()
        };

        let gc = CacheGC::new(config);

        // Create actual files
        for i in 0..5 {
            let path = temp_dir.path().join(format!("file{}", i));
            std::fs::write(&path, vec![0u8; 200]).unwrap();

            let mut entry = create_test_entry(&format!("key{}", i), 200, false);
            entry.path = path;
            entry.last_accessed -= i as u64 * 10;

            gc.update_entry(entry.key.clone(), entry).unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let result = gc.run_gc(true).await.unwrap();

        assert!(result.entries_evicted >= 3);
        assert!(result.bytes_freed >= 600);

        let stats = gc.get_stats().await.unwrap();
        assert!(stats.total_size <= 500);

        gc.shutdown().await;
    }

    #[tokio::test]
    async fn test_protected_entries() {
        let temp_dir = TempDir::new().unwrap();

        let config = GCConfig {
            max_size_bytes: 300,
            background_gc: false,
            incremental: false,
            ..Default::default()
        };

        let gc = CacheGC::new(config);

        // Protected entry
        let protected_path = temp_dir.path().join("protected");
        std::fs::write(&protected_path, vec![0u8; 200]).unwrap();
        let mut protected = create_test_entry("protected", 200, true);
        protected.path = protected_path;
        gc.update_entry(protected.key.clone(), protected).unwrap();

        // Regular entries
        for i in 0..3 {
            let path = temp_dir.path().join(format!("file{}", i));
            std::fs::write(&path, vec![0u8; 100]).unwrap();

            let mut entry = create_test_entry(&format!("key{}", i), 100, false);
            entry.path = path;
            gc.update_entry(entry.key.clone(), entry).unwrap();
        }

        let result = gc.run_gc(true).await.unwrap();

        assert!(!result.evicted_keys.contains(&"protected".to_string()));
        assert!(result.entries_evicted > 0);

        let stats = gc.get_stats().await.unwrap();
        assert_eq!(stats.protected_count, 1);

        gc.shutdown().await;
    }

    #[tokio::test]
    async fn test_age_based_eviction() {
        let temp_dir = TempDir::new().unwrap();

        let config = GCConfig {
            max_age_secs: 5,
            background_gc: false,
            incremental: false,
            ..Default::default()
        };

        let gc = CacheGC::new(config);

        // Old entry
        let old_path = temp_dir.path().join("old");
        std::fs::write(&old_path, vec![0u8; 100]).unwrap();
        let mut old_entry = create_test_entry("old", 100, false);
        old_entry.path = old_path;
        old_entry.created_at -= 10; // 10 seconds old
        gc.update_entry(old_entry.key.clone(), old_entry).unwrap();

        // New entry
        let new_path = temp_dir.path().join("new");
        std::fs::write(&new_path, vec![0u8; 100]).unwrap();
        let mut new_entry = create_test_entry("new", 100, false);
        new_entry.path = new_path;
        gc.update_entry(new_entry.key.clone(), new_entry).unwrap();

        let result = gc.run_gc(true).await.unwrap();

        assert!(result.evicted_keys.contains(&"old".to_string()));
        assert!(!result.evicted_keys.contains(&"new".to_string()));

        gc.shutdown().await;
    }

    #[tokio::test]
    async fn test_incremental_gc() {
        let temp_dir = TempDir::new().unwrap();

        let config = GCConfig {
            max_size_bytes: 500,
            incremental: true,
            incremental_batch_size: 2,
            background_gc: false,
            ..Default::default()
        };

        let gc = CacheGC::new(config);

        for i in 0..10 {
            let path = temp_dir.path().join(format!("file{}", i));
            std::fs::write(&path, vec![0u8; 200]).unwrap();

            let mut entry = create_test_entry(&format!("key{}", i), 200, false);
            entry.path = path;
            gc.update_entry(entry.key.clone(), entry).unwrap();
        }

        let result = gc.run_gc(true).await.unwrap();

        assert!(result.incremental);
        assert!(result.entries_evicted <= 2 || result.has_more);

        gc.shutdown().await;
    }

    #[tokio::test]
    async fn test_hit_miss_tracking() {
        let config = GCConfig {
            background_gc: false,
            ..Default::default()
        };

        let gc = CacheGC::new(config);

        gc.record_hit("key1".to_string()).unwrap();
        gc.record_hit("key2".to_string()).unwrap();
        gc.record_miss().unwrap();

        let stats = gc.get_stats().await.unwrap();

        assert_eq!(stats.hits, 2);
        assert_eq!(stats.misses, 1);
        assert!((stats.hit_rate() - 0.666).abs() < 0.01);

        gc.shutdown().await;
    }

    #[tokio::test]
    async fn test_multi_level_cache() {
        let local_config = GCConfig {
            max_size_bytes: 1000,
            background_gc: false,
            ..Default::default()
        };

        let remote_config = GCConfig {
            max_size_bytes: 5000,
            background_gc: false,
            ..Default::default()
        };

        let cache = MultiLevelCache::new(local_config, Some(remote_config));

        let local_entry = create_test_entry("local1", 200, false);
        cache.update_entry(local_entry).unwrap();

        let mut remote_entry = create_test_entry("remote1", 500, false);
        remote_entry.level = 1;
        cache.update_entry(remote_entry).unwrap();

        cache.record_hit(0, "local1".to_string()).unwrap();
        cache.record_hit(1, "remote1".to_string()).unwrap();

        let all_stats = cache.get_all_stats().await.unwrap();
        assert_eq!(all_stats.len(), 2);
        assert_eq!(all_stats[0].entry_count, 1);
        assert_eq!(all_stats[1].entry_count, 1);
    }

    #[tokio::test]
    async fn test_lfu_eviction() {
        let temp_dir = TempDir::new().unwrap();

        let config = GCConfig {
            max_size_bytes: 500,
            eviction_policy: EvictionPolicy::LFU,
            background_gc: false,
            incremental: false,
            ..Default::default()
        };

        let gc = CacheGC::new(config);

        for i in 0..5 {
            let path = temp_dir.path().join(format!("file{}", i));
            std::fs::write(&path, vec![0u8; 200]).unwrap();

            let mut entry = create_test_entry(&format!("key{}", i), 200, false);
            entry.path = path;
            entry.access_count = i as u64 * 10;
            gc.update_entry(entry.key.clone(), entry).unwrap();
        }

        let result = gc.run_gc(true).await.unwrap();

        assert!(result.entries_evicted >= 3);

        gc.shutdown().await;
    }

    #[tokio::test]
    async fn test_metadata_persistence() {
        let temp_dir = TempDir::new().unwrap();
        let metadata_path = temp_dir.path().join("cache_metadata.json");

        let mut entries = HashMap::new();
        entries.insert("key1".to_string(), create_test_entry("key1", 100, false));
        entries.insert("key2".to_string(), create_test_entry("key2", 200, true));

        save_cache_metadata(&metadata_path, &entries).unwrap();

        let loaded = load_cache_metadata(&metadata_path).unwrap();

        assert_eq!(loaded.len(), 2);
        assert!(loaded.contains_key("key1"));
        assert!(loaded.contains_key("key2"));
        assert!(loaded.get("key2").unwrap().protected);
    }

    #[tokio::test]
    async fn test_stats_calculations() {
        let config = GCConfig {
            background_gc: false,
            ..Default::default()
        };

        let gc = CacheGC::new(config);

        for i in 0..5 {
            let entry = create_test_entry(&format!("key{}", i), 100, false);
            gc.update_entry(entry.key.clone(), entry).unwrap();
        }

        gc.record_hit("key1".to_string()).unwrap();
        gc.record_hit("key2".to_string()).unwrap();
        gc.record_hit("key3".to_string()).unwrap();
        gc.record_miss().unwrap();

        let stats = gc.get_stats().await.unwrap();

        assert_eq!(stats.entry_count, 5);
        assert_eq!(stats.total_size, 500);
        assert_eq!(stats.avg_entry_size(), 100);
        assert_eq!(stats.hits, 3);
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.hit_rate(), 0.75);

        gc.shutdown().await;
    }
}
