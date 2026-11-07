// Rush Build - Incremental Linking
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Incremental linking for faster iteration.
//!
//! Instead of linking all object files every time, track which objects changed
//! and use partial linking or thin archives to speed up the process.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

/// Tracks object file modifications for incremental linking.
pub struct IncrementalLinker {
    /// Object file modification times.
    obj_mtimes: HashMap<PathBuf, SystemTime>,

    /// Last link time for each target.
    link_times: HashMap<String, SystemTime>,

    /// Whether to use thin archives.
    _use_thin_archives: bool,
}

impl IncrementalLinker {
    /// Create a new incremental linker.
    pub fn new() -> Self {
        IncrementalLinker {
            obj_mtimes: HashMap::new(),
            link_times: HashMap::new(),
            _use_thin_archives: true,
        }
    }

    /// Check if a target needs re-linking.
    pub fn needs_relink(&mut self, target: &str, obj_files: &[PathBuf]) -> Result<bool> {
        // Get last link time
        let last_link = self.link_times.get(target);

        if last_link.is_none() {
            // Never linked before
            return Ok(true);
        }

        let last_link_time = *last_link.unwrap();

        // Check if any object file is newer than last link
        for obj_file in obj_files {
            let metadata = std::fs::metadata(obj_file)?;
            let modified = metadata.modified()?;

            if modified > last_link_time {
                // Object file changed since last link
                return Ok(true);
            }

            // Update cache
            self.obj_mtimes.insert(obj_file.clone(), modified);
        }

        Ok(false)
    }

    /// Mark a target as linked.
    pub fn mark_linked(&mut self, target: &str) {
        self.link_times
            .insert(target.to_string(), SystemTime::now());
    }

    /// Get changed object files since last link.
    pub fn get_changed_objects(&self, target: &str, obj_files: &[PathBuf]) -> Result<Vec<PathBuf>> {
        let last_link = match self.link_times.get(target) {
            Some(t) => *t,
            None => return Ok(obj_files.to_vec()), // All changed if never linked
        };

        let mut changed = Vec::new();

        for obj_file in obj_files {
            if let Ok(metadata) = std::fs::metadata(obj_file) {
                if let Ok(modified) = metadata.modified() {
                    if modified > last_link {
                        changed.push(obj_file.clone());
                    }
                }
            }
        }

        Ok(changed)
    }

    /// Create a thin archive for faster linking.
    ///
    /// Thin archives store references to object files instead of embedding them,
    /// making archive creation nearly instant.
    pub fn create_thin_archive(
        &self,
        obj_files: &[PathBuf],
        archive_path: &Path,
    ) -> Result<Vec<String>> {
        // ar crsT archive.a obj1.o obj2.o ...
        // T flag creates a thin archive
        let mut args = vec!["crsT".to_string(), archive_path.display().to_string()];

        for obj in obj_files {
            args.push(obj.display().to_string());
        }

        Ok(args)
    }

    /// Get linker flags for incremental linking.
    pub fn get_linker_flags(&self, use_lld: bool) -> Vec<String> {
        let mut flags = Vec::new();

        if use_lld {
            // Use LLD (LLVM linker) for faster linking
            flags.push("-fuse-ld=lld".to_string());

            // Enable incremental linking
            flags.push("-Wl,--incremental".to_string());
        }

        // Use parallel linking if available
        flags.push("-Wl,--threads".to_string());

        flags
    }

    /// Split large binaries into shared libraries for faster iteration.
    ///
    /// This suggests splitting a monolithic binary into:
    /// - Core shared library (rarely changes)
    /// - Feature shared libraries (changes sometimes)
    /// - Small main binary (changes often)
    pub fn suggest_library_split(
        &self,
        obj_files: &[PathBuf],
        min_size_kb: u64,
    ) -> Result<Vec<Vec<PathBuf>>> {
        // Group object files by directory (heuristic for modules)
        let mut groups: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();

        for obj_file in obj_files {
            if let Some(parent) = obj_file.parent() {
                groups
                    .entry(parent.to_path_buf())
                    .or_default()
                    .push(obj_file.clone());
            }
        }

        // Filter groups by total size
        let mut result = Vec::new();
        for (_, group_files) in groups {
            let total_size: u64 = group_files
                .iter()
                .filter_map(|f| std::fs::metadata(f).ok())
                .map(|m| m.len())
                .sum();

            if total_size > min_size_kb * 1024 {
                result.push(group_files);
            }
        }

        Ok(result)
    }
}

impl Default for IncrementalLinker {
    fn default() -> Self {
        Self::new()
    }
}

/// Advanced incremental linking with LTO and split DWARF.
pub struct AdvancedLinker {
    /// Base incremental linker.
    _base: IncrementalLinker,

    /// LTO cache directory.
    lto_cache_dir: PathBuf,

    /// Split DWARF directory.
    dwarf_dir: PathBuf,

    /// Symbol table cache.
    symbol_cache: HashMap<PathBuf, Vec<String>>,

    /// Link configuration.
    config: LinkConfig,
}

/// Link configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkConfig {
    /// Use ThinLTO.
    pub thin_lto: bool,

    /// Use split DWARF.
    pub split_dwarf: bool,

    /// Number of parallel link jobs.
    pub link_jobs: usize,

    /// Use identical code folding (ICF).
    pub icf: bool,

    /// Strip debug symbols.
    pub strip_debug: bool,

    /// Linker to use.
    pub linker: LinkerType,

    /// Enable link-time optimization cache.
    pub lto_cache: bool,

    /// ICF level (safe, all, none).
    pub icf_level: IcfLevel,
}

/// Linker type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum LinkerType {
    /// GNU ld (traditional).
    Ld,

    /// GNU gold (faster).
    Gold,

    /// LLVM lld (fastest).
    Lld,

    /// Apple ld64.
    Ld64,

    /// MSVC link.exe.
    Link,

    /// Mold (very fast).
    Mold,
}

/// ICF level.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IcfLevel {
    /// No ICF.
    None,

    /// Safe ICF (only identical code).
    Safe,

    /// Aggressive ICF (all identical).
    All,
}

impl Default for LinkConfig {
    fn default() -> Self {
        LinkConfig {
            thin_lto: true,
            split_dwarf: false,
            link_jobs: num_cpus::get(),
            icf: true,
            strip_debug: false,
            linker: LinkerType::Lld,
            lto_cache: true,
            icf_level: IcfLevel::Safe,
        }
    }
}

impl AdvancedLinker {
    /// Create new advanced linker.
    pub fn new(cache_dir: PathBuf, config: LinkConfig) -> Result<Self> {
        let lto_cache_dir = cache_dir.join("lto-cache");
        let dwarf_dir = cache_dir.join("dwarf");

        std::fs::create_dir_all(&lto_cache_dir)?;
        std::fs::create_dir_all(&dwarf_dir)?;

        Ok(AdvancedLinker {
            _base: IncrementalLinker::new(),
            lto_cache_dir,
            dwarf_dir,
            symbol_cache: HashMap::new(),
            config,
        })
    }

    /// Generate linker command with all optimizations.
    pub fn generate_link_command(
        &self,
        output: &Path,
        obj_files: &[PathBuf],
        libs: &[String],
    ) -> Result<Vec<String>> {
        let mut args = Vec::new();

        // Select linker
        match &self.config.linker {
            LinkerType::Lld => args.push("-fuse-ld=lld".to_string()),
            LinkerType::Gold => args.push("-fuse-ld=gold".to_string()),
            LinkerType::Mold => args.push("-fuse-ld=mold".to_string()),
            _ => {}
        }

        // Output file
        args.push("-o".to_string());
        args.push(output.display().to_string());

        // ThinLTO
        if self.config.thin_lto {
            args.push("-flto=thin".to_string());

            if self.config.lto_cache {
                args.push(format!(
                    "-Wl,--thinlto-cache-dir={}",
                    self.lto_cache_dir.display()
                ));
            }

            // ThinLTO jobs
            args.push(format!("-Wl,--thinlto-jobs={}", self.config.link_jobs));
        }

        // Split DWARF
        if self.config.split_dwarf {
            args.push("-gsplit-dwarf".to_string());
            args.push(format!(
                "-Wl,--split-dwarf-output={}",
                self.dwarf_dir.display()
            ));
        }

        // Identical code folding
        if self.config.icf {
            let icf_arg = match self.config.icf_level {
                IcfLevel::Safe => "--icf=safe",
                IcfLevel::All => "--icf=all",
                IcfLevel::None => "--icf=none",
            };
            args.push(format!("-Wl,{}", icf_arg));
        }

        // Parallel linking
        args.push(format!("-Wl,--threads={}", self.config.link_jobs));

        // Strip debug if requested
        if self.config.strip_debug {
            args.push("-Wl,--strip-debug".to_string());
        }

        // Object files
        for obj in obj_files {
            args.push(obj.display().to_string());
        }

        // Libraries
        for lib in libs {
            args.push(format!("-l{}", lib));
        }

        Ok(args)
    }

    /// Extract symbols from object file for caching.
    pub fn extract_symbols(&mut self, obj_file: &Path) -> Result<Vec<String>> {
        // Check cache first
        if let Some(symbols) = self.symbol_cache.get(obj_file) {
            return Ok(symbols.clone());
        }

        // Run nm to extract symbols
        let output = Command::new("nm")
            .arg("--defined-only")
            .arg(obj_file)
            .output()
            .context("Failed to run nm")?;

        let symbols: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 3 {
                    Some(parts[2].to_string())
                } else {
                    None
                }
            })
            .collect();

        // Cache symbols
        self.symbol_cache
            .insert(obj_file.to_path_buf(), symbols.clone());

        Ok(symbols)
    }

    /// Check if partial linking is beneficial.
    pub fn should_partial_link(&self, obj_files: &[PathBuf]) -> Result<bool> {
        // Partial linking is beneficial when:
        // 1. Many object files (> 50)
        // 2. Files grouped by directory
        // 3. Some files change more frequently than others

        if obj_files.len() < 50 {
            return Ok(false);
        }

        // Check if files are grouped by directory
        let mut dirs = std::collections::HashSet::new();
        for obj in obj_files {
            if let Some(parent) = obj.parent() {
                dirs.insert(parent);
            }
        }

        // If files span multiple directories, partial linking helps
        Ok(dirs.len() > 3)
    }

    /// Perform partial linking to combine objects into intermediate archives.
    pub fn partial_link(
        &self,
        obj_groups: Vec<Vec<PathBuf>>,
        output_dir: &Path,
    ) -> Result<Vec<PathBuf>> {
        let mut partial_archives = Vec::new();

        for (i, group) in obj_groups.iter().enumerate() {
            let archive_path = output_dir.join(format!("partial_{}.a", i));

            // Create archive
            let mut cmd = Command::new("ar");
            cmd.arg("crs").arg(&archive_path);

            for obj in group {
                cmd.arg(obj);
            }

            cmd.status().context("Failed to create partial archive")?;

            partial_archives.push(archive_path);
        }

        Ok(partial_archives)
    }

    /// Optimize link with ICF and deduplication.
    pub fn optimize_link(&self, obj_files: &[PathBuf]) -> Result<LinkOptimization> {
        let mut stats = LinkOptimization::default();

        // Extract symbols from all objects
        let mut all_symbols: HashMap<String, Vec<PathBuf>> = HashMap::new();

        for obj in obj_files {
            let output = Command::new("nm").arg("--defined-only").arg(obj).output()?;

            let symbols: Vec<String> = String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() >= 3 {
                        Some(parts[2].to_string())
                    } else {
                        None
                    }
                })
                .collect();

            for symbol in symbols {
                all_symbols.entry(symbol).or_default().push(obj.clone());
            }
        }

        // Find duplicate symbols
        for (symbol, files) in &all_symbols {
            if files.len() > 1 {
                stats
                    .duplicate_symbols
                    .insert(symbol.clone(), files.clone());
            }
        }

        stats.total_symbols = all_symbols.len();
        stats.unique_symbols = all_symbols.iter().filter(|(_, v)| v.len() == 1).count();

        Ok(stats)
    }

    /// Clean LTO cache.
    pub fn clean_lto_cache(&self) -> Result<()> {
        if self.lto_cache_dir.exists() {
            std::fs::remove_dir_all(&self.lto_cache_dir)?;
            std::fs::create_dir_all(&self.lto_cache_dir)?;
        }
        Ok(())
    }

    /// Get LTO cache statistics.
    pub fn lto_cache_stats(&self) -> Result<LtoCacheStats> {
        let mut stats = LtoCacheStats::default();

        if self.lto_cache_dir.exists() {
            for entry in std::fs::read_dir(&self.lto_cache_dir)? {
                let entry = entry?;
                let metadata = entry.metadata()?;
                stats.total_files += 1;
                stats.total_size_bytes += metadata.len();
            }
        }

        Ok(stats)
    }
}

/// Link optimization statistics.
#[derive(Debug, Default)]
pub struct LinkOptimization {
    /// Total symbols found.
    pub total_symbols: usize,

    /// Unique symbols.
    pub unique_symbols: usize,

    /// Duplicate symbols (symbol -> files).
    pub duplicate_symbols: HashMap<String, Vec<PathBuf>>,
}

/// LTO cache statistics.
#[derive(Debug, Default)]
pub struct LtoCacheStats {
    /// Total cached files.
    pub total_files: usize,

    /// Total cache size in bytes.
    pub total_size_bytes: u64,
}

impl LtoCacheStats {
    /// Get cache size in MB.
    pub fn size_mb(&self) -> f64 {
        self.total_size_bytes as f64 / 1_048_576.0
    }
}

/// Linker wrapper that selects best linker for platform.
pub struct LinkerSelector;

impl LinkerSelector {
    /// Detect available linkers on system.
    pub fn detect_available() -> Vec<LinkerType> {
        let mut available = Vec::new();

        if Command::new("ld.lld").arg("--version").status().is_ok() {
            available.push(LinkerType::Lld);
        }

        if Command::new("ld.gold").arg("--version").status().is_ok() {
            available.push(LinkerType::Gold);
        }

        if Command::new("mold").arg("--version").status().is_ok() {
            available.push(LinkerType::Mold);
        }

        if Command::new("ld").arg("--version").status().is_ok() {
            available.push(LinkerType::Ld);
        }

        available
    }

    /// Select best linker for platform.
    pub fn select_best() -> LinkerType {
        let available = Self::detect_available();

        // Preference order: mold > lld > gold > ld
        if available.contains(&LinkerType::Mold) {
            LinkerType::Mold
        } else if available.contains(&LinkerType::Lld) {
            LinkerType::Lld
        } else if available.contains(&LinkerType::Gold) {
            LinkerType::Gold
        } else {
            LinkerType::Ld
        }
    }

    /// Get linker executable name.
    pub fn linker_executable(linker: &LinkerType) -> &'static str {
        match linker {
            LinkerType::Lld => "ld.lld",
            LinkerType::Gold => "ld.gold",
            LinkerType::Mold => "mold",
            LinkerType::Ld => "ld",
            LinkerType::Ld64 => "ld64",
            LinkerType::Link => "link.exe",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::TempDir;

    #[test]
    fn test_needs_relink() -> Result<()> {
        let temp = TempDir::new()?;
        let obj_file = temp.path().join("test.o");

        // Create object file
        File::create(&obj_file)?.write_all(b"fake object")?;

        let mut linker = IncrementalLinker::new();

        // First link - should need relink
        assert!(linker.needs_relink("test", std::slice::from_ref(&obj_file))?);

        // Mark as linked
        linker.mark_linked("test");

        // Should not need relink immediately
        assert!(!linker.needs_relink("test", std::slice::from_ref(&obj_file))?);

        // Touch the object file
        std::thread::sleep(std::time::Duration::from_millis(10));
        File::create(&obj_file)?.write_all(b"modified")?;

        // Should need relink after modification
        assert!(linker.needs_relink("test", std::slice::from_ref(&obj_file))?);

        Ok(())
    }

    #[test]
    fn test_thin_archive_args() {
        let linker = IncrementalLinker::new();
        let obj_files = vec![PathBuf::from("a.o"), PathBuf::from("b.o")];

        let args = linker
            .create_thin_archive(&obj_files, Path::new("libtest.a"))
            .unwrap();

        assert_eq!(args[0], "crsT");
        assert_eq!(args[1], "libtest.a");
        assert!(args.contains(&"a.o".to_string()));
        assert!(args.contains(&"b.o".to_string()));
    }
}
