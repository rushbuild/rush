// Rush Incremental Build Support
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Incremental build support with file change detection.
//!
//! Tracks file modification times and content hashes to detect when
//! targets need to be rebuilt.

use crate::cache_paths;
use anyhow::Result;
use blake3::Hasher;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// File change tracker for incremental builds.
#[derive(Debug, Clone)]
pub struct ChangeDetector {
    /// Path to state file.
    state_path: PathBuf,

    /// Current build state.
    state: BuildState,
}

/// Build state tracking file hashes and mtimes.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BuildState {
    /// File hashes: path -> hash
    file_hashes: HashMap<String, String>,

    /// File modification times: path -> mtime
    file_mtimes: HashMap<String, u64>,

    /// File sizes: path -> size in bytes.
    ///
    /// The mtime fast path is only sound together with this. Filesystem
    /// timestamps are coarse (milliseconds here, and some filesystems are
    /// coarser still), so two writes inside one tick share an mtime; trusting
    /// mtime alone reported a genuinely edited file as unchanged and served a
    /// stale build with no error.
    #[serde(default)]
    file_sizes: HashMap<String, u64>,

    /// Target build timestamps: target -> timestamp
    target_builds: HashMap<String, u64>,
}

impl BuildState {
    fn new() -> Self {
        BuildState {
            file_hashes: HashMap::new(),
            file_mtimes: HashMap::new(),
            file_sizes: HashMap::new(),
            target_builds: HashMap::new(),
        }
    }
}

impl ChangeDetector {
    /// Create a new change detector.
    pub fn new(workspace_root: &Path) -> Self {
        let state_path =
            cache_paths::workspace_output_root(workspace_root).join(".build-state.json");

        // Try to load existing state
        let state = if state_path.exists() {
            Self::load_state(&state_path).unwrap_or_else(|_| BuildState::new())
        } else {
            BuildState::new()
        };

        ChangeDetector { state_path, state }
    }

    /// Load build state from disk.
    fn load_state(path: &Path) -> Result<BuildState> {
        let contents = fs::read_to_string(path)?;
        let state = serde_json::from_str(&contents)?;
        Ok(state)
    }

    /// Save build state to disk.
    pub fn save_state(&self) -> Result<()> {
        // Ensure directory exists
        if let Some(parent) = self.state_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let json = serde_json::to_string_pretty(&self.state)?;
        fs::write(&self.state_path, json)?;
        Ok(())
    }

    /// Check if a file has changed since last build.
    pub fn has_file_changed(&mut self, path: &Path) -> Result<bool> {
        let path_str = path.to_string_lossy().to_string();

        // Get current mtime
        let metadata = fs::metadata(path)?;
        let mtime = metadata
            .modified()?
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_millis() as u64;

        let size = metadata.len();

        // Quick check: skip hashing only when BOTH mtime and size are
        // unchanged. mtime alone is not evidence — see `file_sizes`. A
        // same-size edit inside one filesystem tick still needs the hash,
        // which is why this is a fast path and not the decision itself.
        if let (Some(&old_mtime), Some(&old_size)) = (
            self.state.file_mtimes.get(&path_str),
            self.state.file_sizes.get(&path_str),
        ) {
            if mtime == old_mtime && size == old_size {
                return Ok(false);
            }
        }

        // Mtime changed or new file - compute content hash
        let hash = self.compute_file_hash(path)?;

        // Compare with stored hash
        let changed = match self.state.file_hashes.get(&path_str) {
            Some(old_hash) => &hash != old_hash,
            None => true, // New file
        };

        // Update state
        self.state.file_hashes.insert(path_str.clone(), hash);
        self.state.file_mtimes.insert(path_str.clone(), mtime);
        self.state.file_sizes.insert(path_str, size);

        Ok(changed)
    }

    /// Check if any input files for a target have changed.
    pub fn have_inputs_changed(&mut self, inputs: &[PathBuf]) -> Result<bool> {
        for input in inputs {
            if self.has_file_changed(input)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Compute hash of a file's contents.
    fn compute_file_hash(&self, path: &Path) -> Result<String> {
        let mut file = File::open(path)?;
        let mut hasher = Hasher::new();
        let mut buffer = [0u8; 8192];

        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }

        let hash = hasher.finalize();
        Ok(hash.to_hex().to_string())
    }

    /// Mark a target as built at current time.
    pub fn mark_target_built(&mut self, target: &str) {
        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        self.state
            .target_builds
            .insert(target.to_string(), timestamp);
    }

    /// Check if a target needs rebuilding.
    ///
    /// Returns true if:
    /// - Target has never been built
    /// - Any input files have changed
    /// - Any dependencies have been rebuilt
    pub fn needs_rebuild(
        &mut self,
        target: &str,
        inputs: &[PathBuf],
        dependencies: &[String],
    ) -> Result<bool> {
        // Never built before?
        let target_timestamp = match self.state.target_builds.get(target) {
            Some(&ts) => ts,
            None => return Ok(true), // Never built
        };

        // Check if any inputs changed
        if self.have_inputs_changed(inputs)? {
            return Ok(true);
        }

        // Check if any dependencies were rebuilt more recently
        for dep in dependencies {
            if let Some(&dep_timestamp) = self.state.target_builds.get(dep) {
                if dep_timestamp > target_timestamp {
                    return Ok(true); // Dependency rebuilt
                }
            } else {
                // Dependency never built
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Clear all state (for clean builds).
    pub fn clear(&mut self) {
        self.state = BuildState::new();
    }

    /// Get statistics about the build state.
    pub fn stats(&self) -> ChangeDetectorStats {
        ChangeDetectorStats {
            tracked_files: self.state.file_hashes.len(),
            tracked_targets: self.state.target_builds.len(),
        }
    }
}

/// Statistics about change detector state.
#[derive(Debug, Clone)]
pub struct ChangeDetectorStats {
    /// Number of tracked files.
    pub tracked_files: usize,

    /// Number of tracked targets.
    pub tracked_targets: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_change_detection() -> Result<()> {
        let temp = TempDir::new()?;
        let mut detector = ChangeDetector::new(temp.path());

        // Create a test file
        let test_file = temp.path().join("test.txt");
        fs::write(&test_file, "hello")?;

        // First check - file is new
        assert!(detector.has_file_changed(&test_file)?);

        // Second check - file unchanged
        assert!(!detector.has_file_changed(&test_file)?);

        // Modify file
        fs::write(&test_file, "hello world")?;
        assert!(detector.has_file_changed(&test_file)?);

        // Check again - should be unchanged
        assert!(!detector.has_file_changed(&test_file)?);

        Ok(())
    }

    #[test]
    fn test_target_rebuild() -> Result<()> {
        let temp = TempDir::new()?;
        let mut detector = ChangeDetector::new(temp.path());

        let input = temp.path().join("input.cc");
        fs::write(&input, "int main() {}")?;

        // Never built - needs rebuild
        assert!(detector.needs_rebuild("//pkg:target", std::slice::from_ref(&input), &[])?);

        // Mark as built
        detector.has_file_changed(&input)?; // Update file state
        detector.mark_target_built("//pkg:target");

        // Should not need rebuild
        assert!(!detector.needs_rebuild("//pkg:target", std::slice::from_ref(&input), &[])?);

        // Modify input
        fs::write(&input, "int main() { return 1; }")?;

        // Should need rebuild
        assert!(detector.needs_rebuild("//pkg:target", &[input], &[])?);

        Ok(())
    }

    #[test]
    fn test_state_persistence() -> Result<()> {
        let temp = TempDir::new()?;
        let test_file = temp.path().join("test.txt");
        fs::write(&test_file, "content")?;

        {
            let mut detector = ChangeDetector::new(temp.path());
            detector.has_file_changed(&test_file)?;
            detector.save_state()?;
        }

        // Create new detector - should load previous state
        let mut detector = ChangeDetector::new(temp.path());
        assert!(!detector.has_file_changed(&test_file)?);

        Ok(())
    }
}
