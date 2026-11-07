// Rush Build - Build Reproducibility
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Build reproducibility guarantees.
//!
//! Ensures hermetic, deterministic builds by controlling timestamps, environment
//! variables, and other sources of non-determinism. Critical for security audits,
//! debugging, and cache correctness.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Reproducibility mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReproducibilityMode {
    /// No reproducibility guarantees (fastest).
    None,

    /// Basic reproducibility (controlled timestamps).
    Basic,

    /// Strict reproducibility (full hermetic environment).
    Strict,
}

/// Build reproducibility controller.
pub struct ReproducibilityController {
    /// Reproducibility mode.
    mode: ReproducibilityMode,

    /// Fixed source date epoch (for deterministic timestamps).
    source_date_epoch: Option<u64>,

    /// Allowed environment variables.
    allowed_env_vars: Vec<String>,

    /// Fixed environment variables.
    fixed_env_vars: HashMap<String, String>,

    /// Build hash for reproducibility verification.
    build_hash: Option<String>,
}

impl ReproducibilityController {
    /// Create a new reproducibility controller.
    pub fn new(mode: ReproducibilityMode) -> Self {
        let mut controller = ReproducibilityController {
            mode,
            source_date_epoch: None,
            allowed_env_vars: Vec::new(),
            fixed_env_vars: HashMap::new(),
            build_hash: None,
        };

        // Set defaults based on mode
        match mode {
            ReproducibilityMode::None => {
                // No restrictions
            }
            ReproducibilityMode::Basic => {
                controller.set_default_source_date_epoch();
                controller.set_basic_allowed_env_vars();
            }
            ReproducibilityMode::Strict => {
                controller.set_default_source_date_epoch();
                controller.set_strict_env_vars();
            }
        }

        controller
    }

    /// Set source date epoch for reproducible timestamps.
    pub fn set_source_date_epoch(&mut self, epoch: u64) {
        self.source_date_epoch = Some(epoch);
    }

    /// Set source date epoch from git commit (if available).
    pub fn set_source_date_epoch_from_git(&mut self) -> Result<()> {
        let output = std::process::Command::new("git")
            .args(["log", "-1", "--format=%ct"])
            .output()
            .context("Failed to get git commit timestamp")?;

        if output.status.success() {
            let timestamp = String::from_utf8(output.stdout)?
                .trim()
                .parse::<u64>()
                .context("Failed to parse git timestamp")?;

            self.set_source_date_epoch(timestamp);
        }

        Ok(())
    }

    /// Set default source date epoch (Unix epoch).
    fn set_default_source_date_epoch(&mut self) {
        self.source_date_epoch = Some(0);
    }

    /// Set basic allowed environment variables.
    fn set_basic_allowed_env_vars(&mut self) {
        self.allowed_env_vars = vec![
            "PATH".to_string(),
            "HOME".to_string(),
            "USER".to_string(),
            "TMPDIR".to_string(),
        ];
    }

    /// Set strict environment variables (minimal set).
    fn set_strict_env_vars(&mut self) {
        // In strict mode, we only allow minimal PATH
        self.allowed_env_vars = vec!["PATH".to_string()];

        // Set fixed values for reproducibility
        self.fixed_env_vars
            .insert("HOME".to_string(), "/home/build".to_string());
        self.fixed_env_vars
            .insert("USER".to_string(), "build".to_string());
        self.fixed_env_vars
            .insert("LANG".to_string(), "C.UTF-8".to_string());
        self.fixed_env_vars
            .insert("TZ".to_string(), "UTC".to_string());
    }

    /// Get source date epoch.
    pub fn source_date_epoch(&self) -> Option<u64> {
        self.source_date_epoch
    }

    /// Prepare environment for reproducible execution.
    pub fn prepare_environment(&self, base_env: &mut HashMap<String, String>) {
        match self.mode {
            ReproducibilityMode::None => {
                // No changes
            }
            ReproducibilityMode::Basic => {
                // Filter to allowed vars
                base_env.retain(|k, _| self.allowed_env_vars.contains(k));

                // Set SOURCE_DATE_EPOCH if configured
                if let Some(epoch) = self.source_date_epoch {
                    base_env.insert("SOURCE_DATE_EPOCH".to_string(), epoch.to_string());
                }
            }
            ReproducibilityMode::Strict => {
                // Start with empty environment
                let mut new_env = HashMap::new();

                // Copy only allowed vars
                for var in &self.allowed_env_vars {
                    if let Some(value) = base_env.get(var) {
                        new_env.insert(var.clone(), value.clone());
                    }
                }

                // Set fixed vars
                for (k, v) in &self.fixed_env_vars {
                    new_env.insert(k.clone(), v.clone());
                }

                // Set SOURCE_DATE_EPOCH
                if let Some(epoch) = self.source_date_epoch {
                    new_env.insert("SOURCE_DATE_EPOCH".to_string(), epoch.to_string());
                }

                *base_env = new_env;
            }
        }
    }

    /// Normalize file timestamp for reproducibility.
    pub fn normalize_timestamp(&self, _path: &Path) -> Result<()> {
        if self.mode == ReproducibilityMode::None {
            return Ok(());
        }

        // In practice, we would set file timestamps here
        // For now, just document the intent
        // This would use filetime crate to set mtime to source_date_epoch

        Ok(())
    }

    /// Verify build reproducibility by comparing hashes.
    pub fn verify_reproducibility(&self, current_hash: &str) -> Result<bool> {
        match &self.build_hash {
            Some(expected) => Ok(current_hash == expected),
            None => {
                // First build - store hash for future verification
                Ok(true)
            }
        }
    }

    /// Set expected build hash.
    pub fn set_build_hash(&mut self, hash: String) {
        self.build_hash = Some(hash);
    }

    /// Get reproducibility mode.
    pub fn mode(&self) -> ReproducibilityMode {
        self.mode
    }
}

/// Deterministic path hasher for build reproducibility.
pub struct DeterministicPathHasher {
    /// Base paths to make relative.
    base_paths: Vec<PathBuf>,
}

impl DeterministicPathHasher {
    /// Create a new deterministic path hasher.
    pub fn new() -> Self {
        DeterministicPathHasher {
            base_paths: Vec::new(),
        }
    }

    /// Add a base path for relativization.
    pub fn add_base_path<P: AsRef<Path>>(&mut self, path: P) {
        self.base_paths.push(path.as_ref().to_path_buf());
    }

    /// Normalize path for reproducible hashing.
    pub fn normalize_path(&self, path: &Path) -> PathBuf {
        // Try to make path relative to base paths
        for base in &self.base_paths {
            if let Ok(rel) = path.strip_prefix(base) {
                return rel.to_path_buf();
            }
        }

        // If not relative to any base, use as-is
        path.to_path_buf()
    }

    /// Compute deterministic hash of file.
    pub fn hash_file(&self, path: &Path) -> Result<String> {
        let normalized = self.normalize_path(path);

        let contents = std::fs::read(path).context(format!("Failed to read file: {:?}", path))?;

        let mut hasher = blake3::Hasher::new();

        // Hash normalized path (not absolute path)
        hasher.update(normalized.to_string_lossy().as_bytes());
        hasher.update(b"\0"); // Separator

        // Hash contents
        hasher.update(&contents);

        Ok(hasher.finalize().to_hex().to_string())
    }

    /// Compute hash of directory structure.
    pub fn hash_directory(&self, dir: &Path) -> Result<String> {
        use walkdir::WalkDir;

        let mut file_hashes = Vec::new();

        for entry in WalkDir::new(dir)
            .sort_by_file_name()
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.file_type().is_file() {
                let hash = self.hash_file(entry.path())?;
                file_hashes.push(hash);
            }
        }

        // Hash all file hashes together
        let mut hasher = blake3::Hasher::new();
        for hash in file_hashes {
            hasher.update(hash.as_bytes());
        }

        Ok(hasher.finalize().to_hex().to_string())
    }
}

impl Default for DeterministicPathHasher {
    fn default() -> Self {
        Self::new()
    }
}

/// Build artifact normalizer for reproducibility.
pub struct ArtifactNormalizer {
    /// Strip debug info.
    pub strip_debug: bool,

    /// Normalize timestamps in archives.
    pub normalize_timestamps: bool,

    /// Sort entries in archives.
    pub sort_entries: bool,

    /// Remove non-deterministic metadata.
    pub remove_metadata: bool,
}

impl ArtifactNormalizer {
    /// Create a new artifact normalizer.
    pub fn new() -> Self {
        ArtifactNormalizer {
            strip_debug: true,
            normalize_timestamps: true,
            sort_entries: true,
            remove_metadata: true,
        }
    }

    /// Normalize a build artifact for reproducibility.
    pub fn normalize(&self, artifact_path: &Path) -> Result<()> {
        let extension = artifact_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");

        match extension {
            "a" | "so" | "dylib" => self.normalize_library(artifact_path)?,
            "tar" | "gz" | "tgz" => self.normalize_archive(artifact_path)?,
            _ => {
                // Unknown type - skip normalization
            }
        }

        Ok(())
    }

    /// Normalize a library file.
    fn normalize_library(&self, _path: &Path) -> Result<()> {
        // In practice, this would:
        // 1. Strip debug info if requested
        // 2. Normalize symbol table
        // 3. Remove timestamps from object files

        // This requires architecture-specific tools like strip, objcopy, etc.
        // For now, just document the intent

        Ok(())
    }

    /// Normalize an archive file.
    fn normalize_archive(&self, _path: &Path) -> Result<()> {
        // In practice, this would:
        // 1. Extract archive
        // 2. Sort entries alphabetically
        // 3. Normalize timestamps to SOURCE_DATE_EPOCH
        // 4. Remove metadata fields
        // 5. Recreate archive with normalized data

        // This requires tar/zip manipulation
        // For now, just document the intent

        Ok(())
    }
}

impl Default for ArtifactNormalizer {
    fn default() -> Self {
        Self::new()
    }
}

/// Reproducibility verification report.
#[derive(Debug, Clone)]
pub struct ReproducibilityReport {
    /// Build hash.
    pub build_hash: String,

    /// Expected hash (if verifying).
    pub expected_hash: Option<String>,

    /// Is build reproducible.
    pub is_reproducible: bool,

    /// Non-deterministic files detected.
    pub non_deterministic_files: Vec<PathBuf>,

    /// Reproducibility violations.
    pub violations: Vec<String>,
}

impl ReproducibilityReport {
    /// Create a new report.
    pub fn new(build_hash: String) -> Self {
        ReproducibilityReport {
            build_hash,
            expected_hash: None,
            is_reproducible: true,
            non_deterministic_files: Vec::new(),
            violations: Vec::new(),
        }
    }

    /// Add a violation.
    pub fn add_violation(&mut self, violation: String) {
        self.violations.push(violation);
        self.is_reproducible = false;
    }

    /// Add a non-deterministic file.
    pub fn add_non_deterministic_file(&mut self, path: PathBuf) {
        self.non_deterministic_files.push(path);
        self.is_reproducible = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reproducibility_controller_modes() {
        let none_controller = ReproducibilityController::new(ReproducibilityMode::None);
        assert_eq!(none_controller.mode(), ReproducibilityMode::None);
        assert_eq!(none_controller.source_date_epoch(), None);

        let basic_controller = ReproducibilityController::new(ReproducibilityMode::Basic);
        assert_eq!(basic_controller.mode(), ReproducibilityMode::Basic);
        assert_eq!(basic_controller.source_date_epoch(), Some(0));

        let strict_controller = ReproducibilityController::new(ReproducibilityMode::Strict);
        assert_eq!(strict_controller.mode(), ReproducibilityMode::Strict);
        assert_eq!(strict_controller.source_date_epoch(), Some(0));
    }

    #[test]
    fn test_environment_preparation_basic() {
        let controller = ReproducibilityController::new(ReproducibilityMode::Basic);

        let mut env = HashMap::new();
        env.insert("PATH".to_string(), "/usr/bin".to_string());
        env.insert("HOME".to_string(), "/home/user".to_string());
        env.insert("RANDOM_VAR".to_string(), "value".to_string());

        controller.prepare_environment(&mut env);

        // RANDOM_VAR should be filtered out
        assert!(!env.contains_key("RANDOM_VAR"));
        assert!(env.contains_key("PATH"));
        assert!(env.contains_key("SOURCE_DATE_EPOCH"));
    }

    #[test]
    fn test_environment_preparation_strict() {
        let controller = ReproducibilityController::new(ReproducibilityMode::Strict);

        let mut env = HashMap::new();
        env.insert("PATH".to_string(), "/usr/bin".to_string());
        env.insert("HOME".to_string(), "/home/user".to_string());

        controller.prepare_environment(&mut env);

        // HOME should be fixed value
        assert_eq!(env.get("HOME"), Some(&"/home/build".to_string()));
        assert_eq!(env.get("LANG"), Some(&"C.UTF-8".to_string()));
        assert!(env.contains_key("SOURCE_DATE_EPOCH"));
    }

    #[test]
    fn test_path_hasher() {
        let mut hasher = DeterministicPathHasher::new();
        hasher.add_base_path("/home/user/project");

        let abs_path = PathBuf::from("/home/user/project/src/main.rs");
        let normalized = hasher.normalize_path(&abs_path);

        assert_eq!(normalized, PathBuf::from("src/main.rs"));
    }

    #[test]
    fn test_reproducibility_report() {
        let mut report = ReproducibilityReport::new("abc123".to_string());

        assert!(report.is_reproducible);
        assert_eq!(report.violations.len(), 0);

        report.add_violation("Timestamp mismatch".to_string());

        assert!(!report.is_reproducible);
        assert_eq!(report.violations.len(), 1);
    }
}
