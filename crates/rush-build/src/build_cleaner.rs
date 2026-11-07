// Rush Build - Build Artifact Cleaner
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Smart build artifact cleaning.
//!
//! Analyzes build outputs and intelligently removes stale artifacts that are
//! no longer needed. Unlike simple `rm -rf`, this cleaner understands the
//! build graph and preserves artifacts that may be reused.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use walkdir::WalkDir;

/// Build artifact cleaner.
pub struct BuildCleaner {
    /// Output directory to clean.
    output_dir: PathBuf,

    /// Cleaner configuration.
    config: CleanerConfig,

    /// Statistics from last clean.
    stats: CleanerStats,
}

/// Cleaner configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanerConfig {
    /// Remove artifacts older than this age.
    pub max_age: Option<Duration>,

    /// Keep at least this many versions of each target.
    pub keep_versions: usize,

    /// Dry run mode (don't actually delete).
    pub dry_run: bool,

    /// Exclude patterns (glob patterns to never delete).
    pub exclude_patterns: Vec<String>,

    /// Clean test outputs.
    pub clean_tests: bool,

    /// Clean cache entries.
    pub clean_cache: bool,

    /// Clean only unreferenced artifacts.
    pub clean_unreferenced: bool,
}

impl Default for CleanerConfig {
    fn default() -> Self {
        CleanerConfig {
            max_age: Some(Duration::from_secs(30 * 24 * 3600)), // 30 days
            keep_versions: 3,
            dry_run: false,
            exclude_patterns: vec![],
            clean_tests: true,
            clean_cache: false,
            clean_unreferenced: true,
        }
    }
}

/// Cleaner statistics.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CleanerStats {
    /// Files scanned.
    pub files_scanned: usize,

    /// Files deleted.
    pub files_deleted: usize,

    /// Bytes freed.
    pub bytes_freed: u64,

    /// Errors encountered.
    pub errors: usize,

    /// Time taken.
    pub duration: Duration,
}

impl CleanerStats {
    /// Get freed space in MB.
    pub fn freed_mb(&self) -> f64 {
        self.bytes_freed as f64 / 1_048_576.0
    }

    /// Get freed space in GB.
    pub fn freed_gb(&self) -> f64 {
        self.bytes_freed as f64 / 1_073_741_824.0
    }
}

/// Artifact metadata.
#[derive(Debug, Clone)]
struct ArtifactInfo {
    /// File path.
    path: PathBuf,

    /// File size in bytes.
    size: u64,

    /// Last modified time.
    modified: SystemTime,

    /// Target that created this artifact.
    target: Option<String>,

    /// Whether this artifact is referenced by current build.
    _referenced: bool,
}

impl BuildCleaner {
    /// Create new build cleaner.
    pub fn new<P: AsRef<Path>>(output_dir: P, config: CleanerConfig) -> Self {
        BuildCleaner {
            output_dir: output_dir.as_ref().to_path_buf(),
            config,
            stats: CleanerStats::default(),
        }
    }

    /// Clean build artifacts.
    pub fn clean(&mut self) -> Result<CleanerStats> {
        let start = std::time::Instant::now();

        // Reset stats
        self.stats = CleanerStats::default();

        // Scan output directory
        let artifacts = self.scan_artifacts()?;

        // Determine what to delete
        let to_delete = self.select_artifacts_to_delete(&artifacts)?;

        // Delete selected artifacts
        for artifact in &to_delete {
            if self.config.dry_run {
                eprintln!("Would delete: {}", artifact.path.display());
                self.stats.bytes_freed += artifact.size;
                self.stats.files_deleted += 1;
            } else {
                match std::fs::remove_file(&artifact.path) {
                    Ok(_) => {
                        self.stats.bytes_freed += artifact.size;
                        self.stats.files_deleted += 1;
                    }
                    Err(e) => {
                        eprintln!("Failed to delete {}: {}", artifact.path.display(), e);
                        self.stats.errors += 1;
                    }
                }
            }
        }

        // Clean empty directories
        if !self.config.dry_run {
            self.clean_empty_dirs()?;
        }

        self.stats.duration = start.elapsed();

        Ok(self.stats.clone())
    }

    /// Scan output directory for artifacts.
    fn scan_artifacts(&mut self) -> Result<Vec<ArtifactInfo>> {
        let mut artifacts = Vec::new();

        for entry in WalkDir::new(&self.output_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }

            self.stats.files_scanned += 1;

            let path = entry.path().to_path_buf();

            // Check exclusions
            if self.is_excluded(&path) {
                continue;
            }

            let metadata = entry.metadata()?;
            let size = metadata.len();
            let modified = metadata.modified()?;

            // Try to determine target from path
            let target = self.extract_target_from_path(&path);

            artifacts.push(ArtifactInfo {
                path,
                size,
                modified,
                target,
                _referenced: false, // Will be determined later
            });
        }

        Ok(artifacts)
    }

    /// Select artifacts to delete based on policy.
    fn select_artifacts_to_delete(&self, artifacts: &[ArtifactInfo]) -> Result<Vec<ArtifactInfo>> {
        let mut to_delete = Vec::new();

        // Group artifacts by target
        let mut by_target: HashMap<String, Vec<&ArtifactInfo>> = HashMap::new();
        for artifact in artifacts {
            if let Some(ref target) = artifact.target {
                by_target.entry(target.clone()).or_default().push(artifact);
            }
        }

        // For each target, keep only recent versions
        for (_target, mut target_artifacts) in by_target {
            // Sort by modified time (newest first)
            target_artifacts.sort_by(|a, b| b.modified.cmp(&a.modified));

            // Keep first N versions
            for artifact in target_artifacts.into_iter().skip(self.config.keep_versions) {
                // Check age if configured
                if let Some(max_age) = self.config.max_age {
                    if let Ok(age) = SystemTime::now().duration_since(artifact.modified) {
                        if age > max_age {
                            to_delete.push(artifact.clone());
                        }
                    }
                } else {
                    to_delete.push(artifact.clone());
                }
            }
        }

        // Clean unreferenced artifacts if configured
        if self.config.clean_unreferenced {
            for artifact in artifacts {
                if artifact.target.is_none() && !self.is_excluded(&artifact.path) {
                    // Check age
                    if let Some(max_age) = self.config.max_age {
                        if let Ok(age) = SystemTime::now().duration_since(artifact.modified) {
                            if age > max_age {
                                to_delete.push(artifact.clone());
                            }
                        }
                    }
                }
            }
        }

        Ok(to_delete)
    }

    /// Check if path matches exclusion patterns.
    fn is_excluded(&self, path: &Path) -> bool {
        let path_str = path.to_str().unwrap_or("");

        for pattern in &self.config.exclude_patterns {
            if glob_match(pattern, path_str) {
                return true;
            }
        }

        false
    }

    /// Extract target name from artifact path.
    fn extract_target_from_path(&self, path: &Path) -> Option<String> {
        // Try to extract target from path structure
        // e.g., output/pkg/name/target_name.o -> //pkg:target_name

        let relative = path.strip_prefix(&self.output_dir).ok()?;
        let components: Vec<_> = relative.components().collect();

        if components.len() >= 2 {
            // Simple heuristic: package/target
            let package = components[0].as_os_str().to_str()?;
            let file_stem = path.file_stem()?.to_str()?;

            Some(format!("//{}:{}", package, file_stem))
        } else {
            None
        }
    }

    /// Clean empty directories.
    fn clean_empty_dirs(&self) -> Result<()> {
        let mut dirs_to_check = Vec::new();

        // Collect all directories
        for entry in WalkDir::new(&self.output_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.file_type().is_dir() {
                dirs_to_check.push(entry.path().to_path_buf());
            }
        }

        // Sort by depth (deepest first) to clean from bottom up
        dirs_to_check.sort_by_key(|p| std::cmp::Reverse(p.components().count()));

        for dir in dirs_to_check {
            if dir == self.output_dir {
                continue; // Don't delete output root
            }

            if let Ok(is_empty) = Self::is_dir_empty(&dir) {
                if is_empty {
                    let _ = std::fs::remove_dir(&dir);
                }
            }
        }

        Ok(())
    }

    /// Check if directory is empty.
    fn is_dir_empty(dir: &Path) -> Result<bool> {
        Ok(std::fs::read_dir(dir)?.next().is_none())
    }

    /// Get last cleaner stats.
    pub fn stats(&self) -> &CleanerStats {
        &self.stats
    }

    /// Print cleaner stats.
    pub fn print_stats(&self) {
        eprintln!("\nBuild Cleaner Results:");
        eprintln!("  Files scanned:  {}", self.stats.files_scanned);
        eprintln!("  Files deleted:  {}", self.stats.files_deleted);
        eprintln!(
            "  Space freed:    {:.2} MB ({:.3} GB)",
            self.stats.freed_mb(),
            self.stats.freed_gb()
        );

        if self.stats.errors > 0 {
            eprintln!("  Errors:         {}", self.stats.errors);
        }

        eprintln!(
            "  Duration:       {:.2}s",
            self.stats.duration.as_secs_f64()
        );
    }
}

/// Simple glob matching.
fn glob_match(pattern: &str, text: &str) -> bool {
    // Simple implementation - just check if pattern is contained
    // Real implementation would use proper glob matching
    if pattern.contains('*') {
        let parts: Vec<&str> = pattern.split('*').collect();
        if parts.len() == 2 {
            text.starts_with(parts[0]) && text.ends_with(parts[1])
        } else {
            false
        }
    } else {
        text.contains(pattern)
    }
}

/// Clean command builder.
pub struct CleanBuilder {
    config: CleanerConfig,
}

impl CleanBuilder {
    /// Create new clean builder.
    pub fn new() -> Self {
        CleanBuilder {
            config: CleanerConfig::default(),
        }
    }

    /// Set maximum age.
    pub fn max_age(mut self, age: Duration) -> Self {
        self.config.max_age = Some(age);
        self
    }

    /// Set number of versions to keep.
    pub fn keep_versions(mut self, versions: usize) -> Self {
        self.config.keep_versions = versions;
        self
    }

    /// Enable dry run mode.
    pub fn dry_run(mut self, dry_run: bool) -> Self {
        self.config.dry_run = dry_run;
        self
    }

    /// Add exclusion pattern.
    pub fn exclude(mut self, pattern: String) -> Self {
        self.config.exclude_patterns.push(pattern);
        self
    }

    /// Clean test outputs.
    pub fn clean_tests(mut self, clean: bool) -> Self {
        self.config.clean_tests = clean;
        self
    }

    /// Build cleaner config.
    pub fn build(self) -> CleanerConfig {
        self.config
    }
}

impl Default for CleanBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_cleaner_config_default() {
        let config = CleanerConfig::default();
        assert_eq!(config.keep_versions, 3);
        assert!(!config.dry_run);
        assert!(config.clean_tests);
    }

    #[test]
    fn test_clean_builder() {
        let config = CleanBuilder::new()
            .max_age(Duration::from_secs(3600))
            .keep_versions(5)
            .dry_run(true)
            .exclude("*.tmp".to_string())
            .build();

        assert_eq!(config.max_age, Some(Duration::from_secs(3600)));
        assert_eq!(config.keep_versions, 5);
        assert!(config.dry_run);
        assert_eq!(config.exclude_patterns.len(), 1);
    }

    #[test]
    fn test_glob_match() {
        assert!(glob_match("*.tmp", "file.tmp"));
        assert!(glob_match("test_*", "test_file"));
        assert!(!glob_match("*.tmp", "file.txt"));
    }

    #[test]
    fn test_stats_freed_mb() {
        let stats = CleanerStats {
            bytes_freed: 10_485_760, // 10 MB
            ..Default::default()
        };

        assert!((stats.freed_mb() - 10.0).abs() < 0.01);
    }

    #[test]
    fn test_cleaner_creation() {
        let temp_dir = tempfile::tempdir().unwrap();
        let cleaner = BuildCleaner::new(temp_dir.path(), CleanerConfig::default());

        assert_eq!(cleaner.stats.files_scanned, 0);
        assert_eq!(cleaner.stats.files_deleted, 0);
    }

    #[test]
    fn test_clean_dry_run() {
        let temp_dir = tempfile::tempdir().unwrap();

        // Create some test files
        fs::write(temp_dir.path().join("test1.o"), b"data").unwrap();
        fs::write(temp_dir.path().join("test2.o"), b"data").unwrap();

        let config = CleanBuilder::new().dry_run(true).build();
        let mut cleaner = BuildCleaner::new(temp_dir.path(), config);

        let stats = cleaner.clean().unwrap();

        // Files should still exist in dry run
        assert!(temp_dir.path().join("test1.o").exists());
        assert!(temp_dir.path().join("test2.o").exists());
        assert!(stats.files_scanned > 0);
    }
}
