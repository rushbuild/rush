// Rush Build - Memory-Mapped I/O
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Memory-mapped I/O for faster file operations.
//!
//! Provides zero-copy file access using memory mapping, dramatically
//! improving performance for large file operations during builds.

use anyhow::{Context, Result};
use memmap2::{Mmap, MmapMut, MmapOptions};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Memory-mapped file reader.
///
/// Provides zero-copy read access to files using memory mapping.
pub struct MmapReader {
    /// The memory map.
    mmap: Mmap,

    /// File path.
    path: PathBuf,

    /// File size.
    size: usize,
}

impl MmapReader {
    /// Open a file for memory-mapped reading.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path = path.as_ref();
        let file = File::open(path).context("Failed to open file for reading")?;

        let metadata = file.metadata().context("Failed to get file metadata")?;
        let size = metadata.len() as usize;

        // Create memory map
        let mmap = unsafe {
            MmapOptions::new()
                .map(&file)
                .context("Failed to create memory map")?
        };

        Ok(MmapReader {
            mmap,
            path: path.to_path_buf(),
            size,
        })
    }

    /// Get the entire file contents as a byte slice.
    pub fn as_slice(&self) -> &[u8] {
        &self.mmap[..]
    }

    /// Get a portion of the file.
    pub fn slice(&self, start: usize, len: usize) -> Result<&[u8]> {
        if start + len > self.size {
            anyhow::bail!(
                "Slice out of bounds: start={}, len={}, size={}",
                start,
                len,
                self.size
            );
        }
        Ok(&self.mmap[start..start + len])
    }

    /// Get file size.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Get file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read entire file as string.
    pub fn as_str(&self) -> Result<&str> {
        std::str::from_utf8(&self.mmap[..]).context("File is not valid UTF-8")
    }

    /// Search for a pattern in the file.
    pub fn find(&self, pattern: &[u8]) -> Option<usize> {
        self.mmap
            .windows(pattern.len())
            .position(|window| window == pattern)
    }

    /// Count occurrences of a byte.
    pub fn count_byte(&self, byte: u8) -> usize {
        self.mmap.iter().filter(|&&b| b == byte).count()
    }
}

/// Memory-mapped file writer.
///
/// Provides zero-copy write access to files using memory mapping.
pub struct MmapWriter {
    /// The mutable memory map.
    mmap: MmapMut,

    /// File path.
    path: PathBuf,

    /// Current write position.
    position: usize,

    /// File size.
    size: usize,
}

impl MmapWriter {
    /// Create a new file for memory-mapped writing.
    pub fn create<P: AsRef<Path>>(path: P, size: usize) -> Result<Self> {
        let path = path.as_ref();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .context("Failed to create file")?;

        // Set file size
        file.set_len(size as u64)
            .context("Failed to set file size")?;

        // Create memory map
        let mmap = unsafe {
            MmapOptions::new()
                .map_mut(&file)
                .context("Failed to create mutable memory map")?
        };

        Ok(MmapWriter {
            mmap,
            path: path.to_path_buf(),
            position: 0,
            size,
        })
    }

    /// Write data at current position.
    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        if self.position + data.len() > self.size {
            anyhow::bail!("Write would exceed file size");
        }

        self.mmap[self.position..self.position + data.len()].copy_from_slice(data);
        self.position += data.len();

        Ok(())
    }

    /// Write data at specific offset.
    pub fn write_at(&mut self, offset: usize, data: &[u8]) -> Result<()> {
        if offset + data.len() > self.size {
            anyhow::bail!("Write would exceed file size");
        }

        self.mmap[offset..offset + data.len()].copy_from_slice(data);

        Ok(())
    }

    /// Get current position.
    pub fn position(&self) -> usize {
        self.position
    }

    /// Seek to position.
    pub fn seek(&mut self, position: usize) -> Result<()> {
        if position > self.size {
            anyhow::bail!("Seek position beyond file size");
        }
        self.position = position;
        Ok(())
    }

    /// Flush changes to disk.
    pub fn flush(&self) -> Result<()> {
        self.mmap.flush().context("Failed to flush memory map")
    }

    /// Flush changes asynchronously.
    pub fn flush_async(&self) -> Result<()> {
        self.mmap
            .flush_async()
            .context("Failed to flush memory map asynchronously")
    }

    /// Get file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Get file size.
    pub fn size(&self) -> usize {
        self.size
    }
}

/// Memory-mapped build artifact cache.
///
/// Caches build artifacts using memory-mapped I/O for fast access.
pub struct MmapArtifactCache {
    /// Cache directory.
    cache_dir: PathBuf,

    /// Statistics.
    stats: Arc<MmapCacheStats>,
}

/// Memory-mapped cache statistics.
#[derive(Debug, Default)]
pub struct MmapCacheStats {
    /// Total reads.
    pub reads: AtomicU64,

    /// Total writes.
    pub writes: AtomicU64,

    /// Bytes read.
    pub bytes_read: AtomicU64,

    /// Bytes written.
    pub bytes_written: AtomicU64,

    /// Cache hits.
    pub hits: AtomicU64,

    /// Cache misses.
    pub misses: AtomicU64,
}

impl MmapArtifactCache {
    /// Create a new memory-mapped artifact cache.
    pub fn new<P: AsRef<Path>>(cache_dir: P) -> Result<Self> {
        let cache_dir = cache_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&cache_dir).context("Failed to create cache directory")?;

        Ok(MmapArtifactCache {
            cache_dir,
            stats: Arc::new(MmapCacheStats::default()),
        })
    }

    /// Read an artifact from cache.
    pub fn read(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let path = self.cache_dir.join(key);

        if !path.exists() {
            self.stats.misses.fetch_add(1, Ordering::Relaxed);
            return Ok(None);
        }

        let reader = MmapReader::open(&path)?;
        let data = reader.as_slice().to_vec();

        self.stats.hits.fetch_add(1, Ordering::Relaxed);
        self.stats.reads.fetch_add(1, Ordering::Relaxed);
        self.stats
            .bytes_read
            .fetch_add(data.len() as u64, Ordering::Relaxed);

        Ok(Some(data))
    }

    /// Write an artifact to cache.
    pub fn write(&self, key: &str, data: &[u8]) -> Result<()> {
        let path = self.cache_dir.join(key);

        let mut writer = MmapWriter::create(&path, data.len())?;
        writer.write(data)?;
        writer.flush()?;

        self.stats.writes.fetch_add(1, Ordering::Relaxed);
        self.stats
            .bytes_written
            .fetch_add(data.len() as u64, Ordering::Relaxed);

        Ok(())
    }

    /// Check if artifact exists in cache.
    pub fn contains(&self, key: &str) -> bool {
        self.cache_dir.join(key).exists()
    }

    /// Remove an artifact from cache.
    pub fn remove(&self, key: &str) -> Result<()> {
        let path = self.cache_dir.join(key);
        if path.exists() {
            std::fs::remove_file(&path).context("Failed to remove cached artifact")?;
        }
        Ok(())
    }

    /// Get statistics.
    pub fn stats(&self) -> MmapCacheStatsSnapshot {
        MmapCacheStatsSnapshot {
            reads: self.stats.reads.load(Ordering::Relaxed),
            writes: self.stats.writes.load(Ordering::Relaxed),
            bytes_read: self.stats.bytes_read.load(Ordering::Relaxed),
            bytes_written: self.stats.bytes_written.load(Ordering::Relaxed),
            hits: self.stats.hits.load(Ordering::Relaxed),
            misses: self.stats.misses.load(Ordering::Relaxed),
        }
    }

    /// Clear the cache.
    pub fn clear(&self) -> Result<()> {
        std::fs::remove_dir_all(&self.cache_dir).context("Failed to clear cache")?;
        std::fs::create_dir_all(&self.cache_dir).context("Failed to recreate cache directory")?;
        Ok(())
    }
}

/// Snapshot of memory-mapped cache statistics.
#[derive(Debug, Clone)]
pub struct MmapCacheStatsSnapshot {
    /// Total reads.
    pub reads: u64,

    /// Total writes.
    pub writes: u64,

    /// Bytes read.
    pub bytes_read: u64,

    /// Bytes written.
    pub bytes_written: u64,

    /// Cache hits.
    pub hits: u64,

    /// Cache misses.
    pub misses: u64,
}

impl MmapCacheStatsSnapshot {
    /// Get cache hit rate.
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            return 0.0;
        }
        (self.hits as f64 / total as f64) * 100.0
    }
}

/// Memory-mapped file pool for reusing mappings.
pub struct MmapPool {
    /// Pool of memory-mapped files.
    pool: dashmap::DashMap<PathBuf, Arc<Mmap>>,

    /// Maximum pool size.
    max_size: usize,

    /// Statistics.
    stats: Arc<MmapPoolStats>,
}

/// Memory-mapped pool statistics.
#[derive(Debug, Default)]
pub struct MmapPoolStats {
    /// Pool hits (reused mapping).
    pub pool_hits: AtomicU64,

    /// Pool misses (created new mapping).
    pub pool_misses: AtomicU64,

    /// Total mappings created.
    pub mappings_created: AtomicU64,

    /// Total mappings removed.
    pub mappings_removed: AtomicU64,
}

impl MmapPool {
    /// Create a new memory-mapped file pool.
    pub fn new(max_size: usize) -> Self {
        MmapPool {
            pool: dashmap::DashMap::new(),
            max_size,
            stats: Arc::new(MmapPoolStats::default()),
        }
    }

    /// Get or create a memory-mapped file.
    pub fn get<P: AsRef<Path>>(&self, path: P) -> Result<Arc<Mmap>> {
        let path = path.as_ref().to_path_buf();

        // Check if already in pool
        if let Some(mmap) = self.pool.get(&path) {
            self.stats.pool_hits.fetch_add(1, Ordering::Relaxed);
            return Ok(Arc::clone(&mmap));
        }

        // Not in pool - create new mapping
        let file = File::open(&path).context("Failed to open file")?;
        let mmap = unsafe {
            MmapOptions::new()
                .map(&file)
                .context("Failed to create memory map")?
        };

        let mmap = Arc::new(mmap);

        // Add to pool if not full
        if self.pool.len() < self.max_size {
            self.pool.insert(path.clone(), Arc::clone(&mmap));
        }

        self.stats.pool_misses.fetch_add(1, Ordering::Relaxed);
        self.stats.mappings_created.fetch_add(1, Ordering::Relaxed);

        Ok(mmap)
    }

    /// Remove a mapping from the pool.
    pub fn remove<P: AsRef<Path>>(&self, path: P) {
        if self.pool.remove(path.as_ref()).is_some() {
            self.stats.mappings_removed.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Clear the pool.
    pub fn clear(&self) {
        let removed = self.pool.len();
        self.pool.clear();
        self.stats
            .mappings_removed
            .fetch_add(removed as u64, Ordering::Relaxed);
    }

    /// Get pool size.
    pub fn size(&self) -> usize {
        self.pool.len()
    }

    /// Get statistics.
    pub fn stats(&self) -> MmapPoolStatsSnapshot {
        MmapPoolStatsSnapshot {
            pool_hits: self.stats.pool_hits.load(Ordering::Relaxed),
            pool_misses: self.stats.pool_misses.load(Ordering::Relaxed),
            mappings_created: self.stats.mappings_created.load(Ordering::Relaxed),
            mappings_removed: self.stats.mappings_removed.load(Ordering::Relaxed),
            current_size: self.pool.len(),
        }
    }
}

/// Snapshot of memory-mapped pool statistics.
#[derive(Debug, Clone)]
pub struct MmapPoolStatsSnapshot {
    /// Pool hits (reused mapping).
    pub pool_hits: u64,

    /// Pool misses (created new mapping).
    pub pool_misses: u64,

    /// Total mappings created.
    pub mappings_created: u64,

    /// Total mappings removed.
    pub mappings_removed: u64,

    /// Current pool size.
    pub current_size: usize,
}

impl MmapPoolStatsSnapshot {
    /// Get pool hit rate.
    pub fn hit_rate(&self) -> f64 {
        let total = self.pool_hits + self.pool_misses;
        if total == 0 {
            return 0.0;
        }
        (self.pool_hits as f64 / total as f64) * 100.0
    }
}

/// Zero-copy file concatenation using memory maps.
pub struct MmapConcatenator {
    /// Output file.
    output: PathBuf,

    /// Input files.
    inputs: Vec<PathBuf>,
}

impl MmapConcatenator {
    /// Create a new concatenator.
    pub fn new<P: AsRef<Path>>(output: P) -> Self {
        MmapConcatenator {
            output: output.as_ref().to_path_buf(),
            inputs: Vec::new(),
        }
    }

    /// Add an input file.
    pub fn add_input<P: AsRef<Path>>(&mut self, input: P) {
        self.inputs.push(input.as_ref().to_path_buf());
    }

    /// Concatenate all input files to output.
    pub fn concatenate(&self) -> Result<()> {
        // Calculate total size
        let mut total_size = 0u64;
        for input in &self.inputs {
            let metadata = std::fs::metadata(input)
                .context(format!("Failed to get metadata for {:?}", input))?;
            total_size += metadata.len();
        }

        // Create output file with total size
        let mut writer = MmapWriter::create(&self.output, total_size as usize)?;

        // Copy each input file
        let mut offset = 0;
        for input in &self.inputs {
            let reader = MmapReader::open(input)?;
            let data = reader.as_slice();

            writer.write_at(offset, data)?;
            offset += data.len();
        }

        writer.flush()?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mmap_reader() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");

        // Write test data
        let test_data = b"Hello, memory-mapped I/O!";
        std::fs::write(&file_path, test_data).unwrap();

        // Read with mmap
        let reader = MmapReader::open(&file_path).unwrap();

        assert_eq!(reader.size(), test_data.len());
        assert_eq!(reader.as_slice(), test_data);
        assert_eq!(reader.as_str().unwrap(), "Hello, memory-mapped I/O!");

        // Test slicing
        let slice = reader.slice(0, 5).unwrap();
        assert_eq!(slice, b"Hello");

        // Test searching
        assert_eq!(reader.find(b"memory"), Some(7));
        // "Hello, memory-mapped I/O!" contains three 'e': Hello, memory,
        // mapped. The implementation was right; this expectation was not.
        assert_eq!(reader.count_byte(b'e'), 3);
    }

    #[test]
    fn test_mmap_writer() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let file_path = temp_dir.path().join("output.txt");

        let test_data = b"Test data for memory-mapped writing";

        // Write with mmap
        let mut writer = MmapWriter::create(&file_path, test_data.len()).unwrap();
        writer.write(test_data).unwrap();
        writer.flush().unwrap();

        // Read back with standard I/O
        let read_data = std::fs::read(&file_path).unwrap();
        assert_eq!(read_data, test_data);
    }

    #[test]
    fn test_mmap_writer_at_offset() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let file_path = temp_dir.path().join("offset.txt");

        let size = 100;
        let mut writer = MmapWriter::create(&file_path, size).unwrap();

        writer.write_at(0, b"START").unwrap();
        writer.write_at(50, b"MIDDLE").unwrap();
        writer.write_at(95, b"END").unwrap();
        writer.flush().unwrap();

        let reader = MmapReader::open(&file_path).unwrap();
        assert_eq!(reader.slice(0, 5).unwrap(), b"START");
        assert_eq!(reader.slice(50, 6).unwrap(), b"MIDDLE");
        assert_eq!(reader.slice(95, 3).unwrap(), b"END");
    }

    #[test]
    fn test_mmap_artifact_cache() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let cache = MmapArtifactCache::new(temp_dir.path()).unwrap();

        // Write artifact
        let key = "test_artifact";
        let data = b"Cached build artifact";
        cache.write(key, data).unwrap();

        // Read artifact
        let read_data = cache.read(key).unwrap();
        assert_eq!(read_data, Some(data.to_vec()));

        // Check statistics
        let stats = cache.stats();
        assert_eq!(stats.writes, 1);
        assert_eq!(stats.reads, 1);
        assert_eq!(stats.hits, 1);

        // Test cache miss
        assert!(cache.read("nonexistent").unwrap().is_none());

        let stats = cache.stats();
        assert_eq!(stats.misses, 1);
    }

    #[test]
    fn test_mmap_pool() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let file_path = temp_dir.path().join("pooled.txt");

        std::fs::write(&file_path, b"Pooled file").unwrap();

        let pool = MmapPool::new(10);

        // First access - should create mapping
        let mmap1 = pool.get(&file_path).unwrap();
        let stats = pool.stats();
        assert_eq!(stats.pool_misses, 1);
        assert_eq!(stats.mappings_created, 1);

        // Second access - should reuse mapping
        let mmap2 = pool.get(&file_path).unwrap();
        let stats = pool.stats();
        assert_eq!(stats.pool_hits, 1);

        // Verify both point to same mapping
        assert_eq!(mmap1.as_ptr(), mmap2.as_ptr());
    }

    #[test]
    fn test_mmap_concatenator() {
        let temp_dir = tempfile::TempDir::new().unwrap();

        // Create input files
        let input1 = temp_dir.path().join("input1.txt");
        let input2 = temp_dir.path().join("input2.txt");
        let input3 = temp_dir.path().join("input3.txt");

        std::fs::write(&input1, b"First ").unwrap();
        std::fs::write(&input2, b"Second ").unwrap();
        std::fs::write(&input3, b"Third").unwrap();

        // Concatenate
        let output = temp_dir.path().join("output.txt");
        let mut concat = MmapConcatenator::new(&output);
        concat.add_input(&input1);
        concat.add_input(&input2);
        concat.add_input(&input3);

        concat.concatenate().unwrap();

        // Verify result
        let result = std::fs::read_to_string(&output).unwrap();
        assert_eq!(result, "First Second Third");
    }
}
