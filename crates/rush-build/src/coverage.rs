// Rush Build - Code coverage implementation
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Comprehensive code coverage implementation supporting multiple languages and formats.
//!
//! Provides instrumentation, collection, and reporting for Rust, C++, and Go codebases.
//! Supports multiple output formats including LCOV, HTML, JSON, and Cobertura for
//! integration with various CI/CD systems and coverage tools.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use tokio::process::Command as TokioCommand;

/// Coverage output format
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CoverageFormat {
    /// LCOV tracefile format (standard for many tools)
    Lcov,
    /// HTML report with line-by-line coverage
    Html,
    /// JSON format for programmatic consumption
    Json,
    /// Cobertura XML format (Jenkins, GitLab CI)
    Cobertura,
}

/// How to instrument code for coverage
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstrumentationMode {
    /// Source-based instrumentation (compile-time)
    Source,
    /// LLVM-based coverage (requires LLVM toolchain)
    Llvm,
    /// GCC gcov instrumentation
    Gcov,
    /// Tarpaulin for Rust
    Tarpaulin,
    /// Go native coverage
    GoCoverage,
}

/// Language-specific coverage configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LanguageConfig {
    /// Rust coverage configuration
    Rust {
        /// Use tarpaulin or LLVM
        mode: RustCoverageMode,
        /// Extra tarpaulin flags
        tarpaulin_flags: Vec<String>,
        /// Extra rustflags
        rustflags: Vec<String>,
    },
    /// C++ coverage configuration
    Cpp {
        /// Use gcov or LLVM
        mode: CppCoverageMode,
        /// Compiler flags for instrumentation
        compile_flags: Vec<String>,
        /// Linker flags for instrumentation
        link_flags: Vec<String>,
    },
    /// Go coverage configuration
    Go {
        /// Coverage mode (set, count, atomic)
        mode: GoCoverageMode,
        /// Extra flags for go test
        test_flags: Vec<String>,
    },
}

/// Rust coverage modes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RustCoverageMode {
    /// Tarpaulin (easiest, Rust-specific)
    Tarpaulin,
    /// LLVM source-based coverage (most accurate)
    LlvmCov,
}

/// C++ coverage modes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CppCoverageMode {
    /// GCC gcov
    Gcov,
    /// LLVM coverage
    Llvm,
}

/// Go coverage modes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GoCoverageMode {
    /// Set mode (whether line was executed)
    Set,
    /// Count mode (how many times)
    Count,
    /// Atomic mode (thread-safe counting)
    Atomic,
}

/// Line coverage information
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LineCoverage {
    /// Line number (1-indexed)
    pub line: usize,
    /// Number of times this line was hit
    pub hits: u64,
    /// Whether this line has a branch
    pub has_branch: bool,
    /// Branch taken count (if has_branch)
    pub branch_taken: Option<u64>,
    /// Branch not taken count (if has_branch)
    pub branch_not_taken: Option<u64>,
}

/// Branch coverage information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchCoverage {
    /// Line number
    pub line: usize,
    /// Branch number
    pub branch: usize,
    /// Times taken
    pub taken: u64,
    /// Times not taken
    pub not_taken: u64,
}

/// Coverage data for a single source file
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceCoverage {
    /// Path to source file (relative to workspace root)
    pub path: PathBuf,
    /// Line coverage data
    pub lines: Vec<LineCoverage>,
    /// Branch coverage data
    pub branches: Vec<BranchCoverage>,
    /// Total lines
    pub total_lines: usize,
    /// Covered lines
    pub covered_lines: usize,
    /// Total branches
    pub total_branches: usize,
    /// Covered branches
    pub covered_branches: usize,
}

impl SourceCoverage {
    /// Calculate line coverage percentage
    pub fn line_coverage_percent(&self) -> f64 {
        if self.total_lines == 0 {
            return 0.0;
        }
        (self.covered_lines as f64 / self.total_lines as f64) * 100.0
    }

    /// Calculate branch coverage percentage
    pub fn branch_coverage_percent(&self) -> f64 {
        if self.total_branches == 0 {
            return 100.0; // No branches is 100% covered
        }
        (self.covered_branches as f64 / self.total_branches as f64) * 100.0
    }

    /// Check if source file meets threshold
    pub fn meets_threshold(&self, threshold: f64) -> bool {
        self.line_coverage_percent() >= threshold
    }
}

/// Merged coverage report across all files
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverageReport {
    /// Coverage data per source file
    pub files: HashMap<PathBuf, SourceCoverage>,
    /// Total lines across all files
    pub total_lines: usize,
    /// Total covered lines
    pub covered_lines: usize,
    /// Total branches
    pub total_branches: usize,
    /// Total covered branches
    pub covered_branches: usize,
    /// Timestamp of report generation
    pub timestamp: String,
}

impl CoverageReport {
    /// Create new empty coverage report
    pub fn new() -> Self {
        Self {
            files: HashMap::new(),
            total_lines: 0,
            covered_lines: 0,
            total_branches: 0,
            covered_branches: 0,
            timestamp: chrono::Utc::now().to_rfc3339(),
        }
    }

    /// Add source coverage to report
    pub fn add_file(&mut self, coverage: SourceCoverage) {
        self.total_lines += coverage.total_lines;
        self.covered_lines += coverage.covered_lines;
        self.total_branches += coverage.total_branches;
        self.covered_branches += coverage.covered_branches;
        self.files.insert(coverage.path.clone(), coverage);
    }

    /// Calculate overall line coverage percentage
    pub fn line_coverage_percent(&self) -> f64 {
        if self.total_lines == 0 {
            return 0.0;
        }
        (self.covered_lines as f64 / self.total_lines as f64) * 100.0
    }

    /// Calculate overall branch coverage percentage
    pub fn branch_coverage_percent(&self) -> f64 {
        if self.total_branches == 0 {
            return 100.0;
        }
        (self.covered_branches as f64 / self.total_branches as f64) * 100.0
    }

    /// Check if report meets minimum threshold
    pub fn meets_threshold(&self, threshold: f64) -> bool {
        self.line_coverage_percent() >= threshold
    }

    /// Merge another coverage report into this one
    pub fn merge(&mut self, other: &CoverageReport) {
        for (path, other_cov) in &other.files {
            if let Some(existing) = self.files.get_mut(path) {
                // Merge line coverage
                for other_line in &other_cov.lines {
                    if let Some(existing_line) = existing
                        .lines
                        .iter_mut()
                        .find(|l| l.line == other_line.line)
                    {
                        existing_line.hits += other_line.hits;
                        if let (Some(bt), Some(other_bt)) =
                            (&mut existing_line.branch_taken, &other_line.branch_taken)
                        {
                            *bt += other_bt;
                        }
                        if let (Some(bnt), Some(other_bnt)) = (
                            &mut existing_line.branch_not_taken,
                            &other_line.branch_not_taken,
                        ) {
                            *bnt += other_bnt;
                        }
                    } else {
                        existing.lines.push(other_line.clone());
                    }
                }
                existing.lines.sort_by_key(|l| l.line);

                // Recalculate totals
                existing.covered_lines = existing.lines.iter().filter(|l| l.hits > 0).count();
                existing.total_lines = existing.lines.len();
            } else {
                self.files.insert(path.clone(), other_cov.clone());
            }
        }

        // Recalculate overall totals
        self.total_lines = 0;
        self.covered_lines = 0;
        self.total_branches = 0;
        self.covered_branches = 0;

        for coverage in self.files.values() {
            self.total_lines += coverage.total_lines;
            self.covered_lines += coverage.covered_lines;
            self.total_branches += coverage.total_branches;
            self.covered_branches += coverage.covered_branches;
        }
    }

    /// Filter coverage to only include changed files
    pub fn filter_changed(&self, changed_files: &HashSet<PathBuf>) -> Self {
        let mut filtered = Self::new();
        for (path, coverage) in &self.files {
            if changed_files.contains(path) {
                filtered.add_file(coverage.clone());
            }
        }
        filtered.timestamp = self.timestamp.clone();
        filtered
    }
}

impl Default for CoverageReport {
    fn default() -> Self {
        Self::new()
    }
}

/// Coverage threshold configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverageThreshold {
    /// Minimum line coverage percentage
    pub line_coverage: f64,
    /// Minimum branch coverage percentage
    pub branch_coverage: f64,
    /// Whether to fail build on threshold violation
    pub fail_on_violation: bool,
}

impl Default for CoverageThreshold {
    fn default() -> Self {
        Self {
            line_coverage: 80.0,
            branch_coverage: 75.0,
            fail_on_violation: false,
        }
    }
}

/// Main coverage collector and orchestrator
pub struct CoverageCollector {
    /// Workspace root directory
    workspace_root: PathBuf,
    /// Output directory for coverage data
    output_dir: PathBuf,
    /// Language configurations
    language_configs: HashMap<String, LanguageConfig>,
    /// Coverage thresholds
    thresholds: CoverageThreshold,
    /// Whether to collect incremental coverage only
    incremental_only: bool,
    /// Changed files for incremental mode
    changed_files: HashSet<PathBuf>,
}

impl CoverageCollector {
    /// Create new coverage collector
    pub fn new(workspace_root: PathBuf, output_dir: PathBuf) -> Self {
        Self {
            workspace_root,
            output_dir,
            language_configs: HashMap::new(),
            thresholds: CoverageThreshold::default(),
            incremental_only: false,
            changed_files: HashSet::new(),
        }
    }

    /// Set coverage thresholds
    pub fn with_thresholds(mut self, thresholds: CoverageThreshold) -> Self {
        self.thresholds = thresholds;
        self
    }

    /// Enable incremental coverage mode
    pub fn with_incremental(mut self, changed_files: HashSet<PathBuf>) -> Self {
        self.incremental_only = true;
        self.changed_files = changed_files;
        self
    }

    /// Add language configuration
    pub fn add_language_config(&mut self, lang: &str, config: LanguageConfig) {
        self.language_configs.insert(lang.to_string(), config);
    }

    /// Run coverage collection for all configured languages
    pub async fn collect(&self) -> Result<CoverageReport> {
        fs::create_dir_all(&self.output_dir)
            .context("Failed to create coverage output directory")?;

        let mut report = CoverageReport::new();

        for config in self.language_configs.values() {
            let lang_report = match config {
                LanguageConfig::Rust { mode, .. } => self.collect_rust_coverage(*mode).await?,
                LanguageConfig::Cpp { mode, .. } => self.collect_cpp_coverage(*mode).await?,
                LanguageConfig::Go { mode, .. } => self.collect_go_coverage(*mode).await?,
            };
            report.merge(&lang_report);
        }

        // Filter to changed files if incremental mode
        if self.incremental_only {
            report = report.filter_changed(&self.changed_files);
        }

        Ok(report)
    }

    /// Collect Rust coverage using tarpaulin or LLVM
    async fn collect_rust_coverage(&self, mode: RustCoverageMode) -> Result<CoverageReport> {
        match mode {
            RustCoverageMode::Tarpaulin => self.collect_rust_tarpaulin().await,
            RustCoverageMode::LlvmCov => self.collect_rust_llvm_cov().await,
        }
    }

    /// Collect Rust coverage using tarpaulin
    async fn collect_rust_tarpaulin(&self) -> Result<CoverageReport> {
        let output_file = self.output_dir.join("tarpaulin.json");

        let mut cmd = TokioCommand::new("cargo");
        cmd.arg("tarpaulin")
            .arg("--out")
            .arg("Json")
            .arg("--output-dir")
            .arg(&self.output_dir)
            .current_dir(&self.workspace_root);

        if let Some(LanguageConfig::Rust {
            tarpaulin_flags, ..
        }) = self.language_configs.get("rust")
        {
            for flag in tarpaulin_flags {
                cmd.arg(flag);
            }
        }

        let output = cmd
            .output()
            .await
            .context("Failed to run cargo tarpaulin")?;

        if !output.status.success() {
            return Err(anyhow!(
                "Tarpaulin failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        self.parse_tarpaulin_json(&output_file).await
    }

    /// Collect Rust coverage using LLVM
    async fn collect_rust_llvm_cov(&self) -> Result<CoverageReport> {
        let profdata = self.output_dir.join("coverage.profdata");
        let json_output = self.output_dir.join("llvm-cov.json");

        // Set environment variables for instrumentation
        std::env::set_var("RUSTFLAGS", "-C instrument-coverage");
        std::env::set_var(
            "LLVM_PROFILE_FILE",
            self.output_dir.join("coverage-%p-%m.profraw"),
        );

        // Run tests
        let test_output = TokioCommand::new("cargo")
            .arg("test")
            .current_dir(&self.workspace_root)
            .output()
            .await
            .context("Failed to run cargo test")?;

        if !test_output.status.success() {
            return Err(anyhow!(
                "Tests failed: {}",
                String::from_utf8_lossy(&test_output.stderr)
            ));
        }

        // Merge profraw files
        let profraw_files: Vec<PathBuf> = fs::read_dir(&self.output_dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("profraw"))
            .collect();

        let mut merge_cmd = TokioCommand::new("llvm-profdata");
        merge_cmd.arg("merge").arg("-sparse");

        for profraw in &profraw_files {
            merge_cmd.arg(profraw);
        }

        merge_cmd.arg("-o").arg(&profdata);

        let merge_output = merge_cmd
            .output()
            .await
            .context("Failed to merge profraw files")?;

        if !merge_output.status.success() {
            return Err(anyhow!(
                "llvm-profdata merge failed: {}",
                String::from_utf8_lossy(&merge_output.stderr)
            ));
        }

        // Generate JSON report
        let cov_output = TokioCommand::new("llvm-cov")
            .arg("export")
            .arg("--format=text")
            .arg("--instr-profile")
            .arg(&profdata)
            .arg("--object")
            .arg(self.workspace_root.join("target/debug/deps"))
            .output()
            .await
            .context("Failed to generate coverage report")?;

        if !cov_output.status.success() {
            return Err(anyhow!(
                "llvm-cov export failed: {}",
                String::from_utf8_lossy(&cov_output.stderr)
            ));
        }

        fs::write(&json_output, &cov_output.stdout).context("Failed to write coverage JSON")?;

        self.parse_llvm_json(&json_output).await
    }

    /// Collect C++ coverage using gcov or LLVM
    async fn collect_cpp_coverage(&self, mode: CppCoverageMode) -> Result<CoverageReport> {
        match mode {
            CppCoverageMode::Gcov => self.collect_cpp_gcov().await,
            CppCoverageMode::Llvm => self.collect_cpp_llvm().await,
        }
    }

    /// Collect C++ coverage using gcov
    async fn collect_cpp_gcov(&self) -> Result<CoverageReport> {
        // Run gcov on all .gcda files
        let gcda_files: Vec<PathBuf> = walkdir::WalkDir::new(&self.workspace_root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("gcda"))
            .map(|e| e.path().to_path_buf())
            .collect();

        for gcda_file in &gcda_files {
            let output = TokioCommand::new("gcov")
                .arg(gcda_file)
                .output()
                .await
                .context("Failed to run gcov")?;

            if !output.status.success() {
                eprintln!("gcov warning: {}", String::from_utf8_lossy(&output.stderr));
            }
        }

        // Parse gcov output files
        self.parse_gcov_files().await
    }

    /// Collect C++ coverage using LLVM
    async fn collect_cpp_llvm(&self) -> Result<CoverageReport> {
        // Similar to Rust LLVM coverage but for C++
        let profdata = self.output_dir.join("cpp-coverage.profdata");
        let json_output = self.output_dir.join("cpp-llvm-cov.json");

        // Find profraw files
        let profraw_files: Vec<PathBuf> = walkdir::WalkDir::new(&self.workspace_root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("profraw"))
            .map(|e| e.path().to_path_buf())
            .collect();

        if profraw_files.is_empty() {
            return Err(anyhow!(
                "No profraw files found. Did you compile with -fprofile-instr-generate?"
            ));
        }

        // Merge profraw files
        let mut merge_cmd = TokioCommand::new("llvm-profdata");
        merge_cmd.arg("merge").arg("-sparse");
        for profraw in &profraw_files {
            merge_cmd.arg(profraw);
        }
        merge_cmd.arg("-o").arg(&profdata);

        let merge_output = merge_cmd
            .output()
            .await
            .context("Failed to merge profraw files")?;

        if !merge_output.status.success() {
            return Err(anyhow!(
                "llvm-profdata merge failed: {}",
                String::from_utf8_lossy(&merge_output.stderr)
            ));
        }

        // Export coverage data
        let cov_output = TokioCommand::new("llvm-cov")
            .arg("export")
            .arg("--format=text")
            .arg("--instr-profile")
            .arg(&profdata)
            .output()
            .await
            .context("Failed to export coverage")?;

        fs::write(&json_output, &cov_output.stdout).context("Failed to write JSON")?;

        self.parse_llvm_json(&json_output).await
    }

    /// Collect Go coverage
    async fn collect_go_coverage(&self, mode: GoCoverageMode) -> Result<CoverageReport> {
        let coverprofile = self.output_dir.join("go-coverage.out");

        let mode_str = match mode {
            GoCoverageMode::Set => "set",
            GoCoverageMode::Count => "count",
            GoCoverageMode::Atomic => "atomic",
        };

        let output = TokioCommand::new("go")
            .arg("test")
            .arg("-covermode")
            .arg(mode_str)
            .arg("-coverprofile")
            .arg(&coverprofile)
            .arg("./...")
            .current_dir(&self.workspace_root)
            .output()
            .await
            .context("Failed to run go test")?;

        if !output.status.success() {
            return Err(anyhow!(
                "Go tests failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }

        self.parse_go_coverprofile(&coverprofile).await
    }

    /// Parse tarpaulin JSON output
    async fn parse_tarpaulin_json(&self, path: &Path) -> Result<CoverageReport> {
        let content = tokio::fs::read_to_string(path)
            .await
            .context("Failed to read tarpaulin JSON")?;

        #[derive(Deserialize)]
        struct TarpaulinFile {
            path: String,
            lines: BTreeMap<usize, u64>,
        }

        #[derive(Deserialize)]
        struct TarpaulinReport {
            files: HashMap<String, TarpaulinFile>,
        }

        let tarp_report: TarpaulinReport =
            serde_json::from_str(&content).context("Failed to parse tarpaulin JSON")?;

        let mut report = CoverageReport::new();

        for (_, file_data) in tarp_report.files {
            let path = PathBuf::from(file_data.path);
            let mut lines = Vec::new();
            let mut covered = 0;

            for (line_num, hits) in file_data.lines {
                if hits > 0 {
                    covered += 1;
                }
                lines.push(LineCoverage {
                    line: line_num,
                    hits,
                    has_branch: false,
                    branch_taken: None,
                    branch_not_taken: None,
                });
            }

            let total = lines.len();
            report.add_file(SourceCoverage {
                path,
                lines,
                branches: Vec::new(),
                total_lines: total,
                covered_lines: covered,
                total_branches: 0,
                covered_branches: 0,
            });
        }

        Ok(report)
    }

    /// Parse LLVM coverage JSON
    async fn parse_llvm_json(&self, path: &Path) -> Result<CoverageReport> {
        let content = tokio::fs::read_to_string(path)
            .await
            .context("Failed to read LLVM coverage JSON")?;

        #[derive(Deserialize)]
        struct LlvmSegment {
            line: usize,
            _col: usize,
            count: u64,
            #[serde(default)]
            has_count: bool,
        }

        #[derive(Deserialize)]
        struct _LlvmRegion {
            segments: Vec<LlvmSegment>,
        }

        #[derive(Deserialize)]
        struct LlvmFile {
            filename: String,
            segments: Vec<LlvmSegment>,
        }

        #[derive(Deserialize)]
        struct LlvmData {
            files: Vec<LlvmFile>,
        }

        #[derive(Deserialize)]
        struct LlvmExport {
            data: Vec<LlvmData>,
        }

        let llvm_export: LlvmExport =
            serde_json::from_str(&content).context("Failed to parse LLVM JSON")?;

        let mut report = CoverageReport::new();

        for data in llvm_export.data {
            for file in data.files {
                let path = PathBuf::from(file.filename);
                let mut line_hits: HashMap<usize, u64> = HashMap::new();

                for seg in file.segments {
                    if seg.has_count {
                        *line_hits.entry(seg.line).or_insert(0) += seg.count;
                    }
                }

                let mut lines = Vec::new();
                let mut covered = 0;

                for (line_num, hits) in line_hits {
                    if hits > 0 {
                        covered += 1;
                    }
                    lines.push(LineCoverage {
                        line: line_num,
                        hits,
                        has_branch: false,
                        branch_taken: None,
                        branch_not_taken: None,
                    });
                }

                lines.sort_by_key(|l| l.line);
                let total = lines.len();

                report.add_file(SourceCoverage {
                    path,
                    lines,
                    branches: Vec::new(),
                    total_lines: total,
                    covered_lines: covered,
                    total_branches: 0,
                    covered_branches: 0,
                });
            }
        }

        Ok(report)
    }

    /// Parse gcov output files
    async fn parse_gcov_files(&self) -> Result<CoverageReport> {
        let gcov_files: Vec<PathBuf> = walkdir::WalkDir::new(&self.workspace_root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("gcov"))
            .map(|e| e.path().to_path_buf())
            .collect();

        let mut report = CoverageReport::new();

        for gcov_file in gcov_files {
            let content = tokio::fs::read_to_string(&gcov_file).await?;
            let source_file = self.extract_source_file_from_gcov(&content)?;
            let coverage = self.parse_gcov_content(&content, &source_file)?;
            report.add_file(coverage);
        }

        Ok(report)
    }

    /// Extract source filename from gcov content
    fn extract_source_file_from_gcov(&self, content: &str) -> Result<PathBuf> {
        for line in content.lines() {
            if line.contains(":Source:") {
                let parts: Vec<&str> = line.split(':').collect();
                if parts.len() >= 3 {
                    return Ok(PathBuf::from(parts[2].trim()));
                }
            }
        }
        Err(anyhow!("Could not find source file in gcov output"))
    }

    /// Parse gcov file content
    fn parse_gcov_content(&self, content: &str, source_file: &Path) -> Result<SourceCoverage> {
        let mut lines = Vec::new();
        let mut covered = 0;

        for line in content.lines() {
            if line.trim().is_empty() || line.contains(":Source:") || line.contains(":Graph:") {
                continue;
            }

            let parts: Vec<&str> = line.split(':').collect();
            if parts.len() >= 3 {
                let exec_count = parts[0].trim();
                let line_num = parts[1].trim().parse::<usize>().unwrap_or(0);

                if line_num == 0 {
                    continue;
                }

                let hits = if exec_count == "-" || exec_count == "#####" {
                    0
                } else {
                    exec_count.parse::<u64>().unwrap_or(0)
                };

                if hits > 0 {
                    covered += 1;
                }

                lines.push(LineCoverage {
                    line: line_num,
                    hits,
                    has_branch: false,
                    branch_taken: None,
                    branch_not_taken: None,
                });
            }
        }

        let total = lines.len();

        Ok(SourceCoverage {
            path: source_file.to_path_buf(),
            lines,
            branches: Vec::new(),
            total_lines: total,
            covered_lines: covered,
            total_branches: 0,
            covered_branches: 0,
        })
    }

    /// Parse Go coverprofile
    async fn parse_go_coverprofile(&self, path: &Path) -> Result<CoverageReport> {
        let content = tokio::fs::read_to_string(path)
            .await
            .context("Failed to read Go coverprofile")?;

        let mut report = CoverageReport::new();
        let mut file_coverage: HashMap<PathBuf, HashMap<usize, u64>> = HashMap::new();

        for line in content.lines().skip(1) {
            // Format: file.go:startline.startcol,endline.endcol numstmt count
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 3 {
                continue;
            }

            let file_and_range = parts[0];
            let count: u64 = parts[2].parse().unwrap_or(0);

            let file_parts: Vec<&str> = file_and_range.split(':').collect();
            if file_parts.len() < 2 {
                continue;
            }

            let filename = PathBuf::from(file_parts[0]);
            let range_parts: Vec<&str> = file_parts[1].split(',').collect();
            if range_parts.len() < 2 {
                continue;
            }

            let start_line: usize = range_parts[0]
                .split('.')
                .next()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0);
            let end_line: usize = range_parts[1]
                .split('.')
                .next()
                .unwrap_or("0")
                .parse()
                .unwrap_or(0);

            let file_map = file_coverage.entry(filename.clone()).or_default();
            for line_num in start_line..=end_line {
                *file_map.entry(line_num).or_insert(0) += count;
            }
        }

        for (path, line_map) in file_coverage {
            let mut lines = Vec::new();
            let mut covered = 0;

            for (line_num, hits) in line_map {
                if hits > 0 {
                    covered += 1;
                }
                lines.push(LineCoverage {
                    line: line_num,
                    hits,
                    has_branch: false,
                    branch_taken: None,
                    branch_not_taken: None,
                });
            }

            lines.sort_by_key(|l| l.line);
            let total = lines.len();

            report.add_file(SourceCoverage {
                path,
                lines,
                branches: Vec::new(),
                total_lines: total,
                covered_lines: covered,
                total_branches: 0,
                covered_branches: 0,
            });
        }

        Ok(report)
    }

    /// Generate coverage report in specified format
    pub async fn generate_report(
        &self,
        report: &CoverageReport,
        format: CoverageFormat,
    ) -> Result<PathBuf> {
        match format {
            CoverageFormat::Lcov => self.generate_lcov(report).await,
            CoverageFormat::Html => self.generate_html(report).await,
            CoverageFormat::Json => self.generate_json(report).await,
            CoverageFormat::Cobertura => self.generate_cobertura(report).await,
        }
    }

    /// Generate LCOV format report
    async fn generate_lcov(&self, report: &CoverageReport) -> Result<PathBuf> {
        let output_path = self.output_dir.join("lcov.info");
        let mut file = tokio::fs::File::create(&output_path)
            .await
            .context("Failed to create LCOV file")?;

        for (path, coverage) in &report.files {
            file.write_all("TN:\n".to_string().as_bytes()).await?;
            file.write_all(format!("SF:{}\n", path.display()).as_bytes())
                .await?;

            for line in &coverage.lines {
                file.write_all(format!("DA:{},{}\n", line.line, line.hits).as_bytes())
                    .await?;
            }

            file.write_all(format!("LF:{}\n", coverage.total_lines).as_bytes())
                .await?;
            file.write_all(format!("LH:{}\n", coverage.covered_lines).as_bytes())
                .await?;
            file.write_all(b"end_of_record\n").await?;
        }

        Ok(output_path)
    }

    /// Generate HTML coverage report
    async fn generate_html(&self, report: &CoverageReport) -> Result<PathBuf> {
        let html_dir = self.output_dir.join("html");
        tokio::fs::create_dir_all(&html_dir)
            .await
            .context("Failed to create HTML directory")?;

        // Generate index.html
        let index_path = html_dir.join("index.html");
        let mut index = tokio::fs::File::create(&index_path).await?;

        index
            .write_all(b"<!DOCTYPE html>\n<html>\n<head>\n")
            .await?;
        index.write_all(b"<title>Coverage Report</title>\n").await?;
        index.write_all(b"<style>\n").await?;
        index
            .write_all(b"body { font-family: sans-serif; margin: 20px; }\n")
            .await?;
        index
            .write_all(b"table { border-collapse: collapse; width: 100%; }\n")
            .await?;
        index
            .write_all(b"th, td { border: 1px solid #ddd; padding: 8px; text-align: left; }\n")
            .await?;
        index
            .write_all(b"th { background-color: #4CAF50; color: white; }\n")
            .await?;
        index
            .write_all(b".covered { background-color: #dff0d8; }\n")
            .await?;
        index
            .write_all(b".uncovered { background-color: #f2dede; }\n")
            .await?;
        index
            .write_all(b".partial { background-color: #fcf8e3; }\n")
            .await?;
        index.write_all(b"</style>\n</head>\n<body>\n").await?;
        index
            .write_all("<h1>Coverage Report</h1>\n".to_string().as_bytes())
            .await?;
        index
            .write_all(
                format!(
                    "<p>Overall Coverage: {:.2}% ({}/{} lines)</p>\n",
                    report.line_coverage_percent(),
                    report.covered_lines,
                    report.total_lines
                )
                .as_bytes(),
            )
            .await?;

        index
            .write_all(b"<table>\n<tr><th>File</th><th>Line Coverage</th><th>Lines</th></tr>\n")
            .await?;

        for (path, coverage) in &report.files {
            let filename = path.file_name().unwrap_or_default().to_string_lossy();
            let html_file = format!("{}.html", filename);

            index
                .write_all(
                    format!(
                        "<tr><td><a href=\"{}\">{}</a></td><td>{:.2}%</td><td>{}/{}</td></tr>\n",
                        html_file,
                        path.display(),
                        coverage.line_coverage_percent(),
                        coverage.covered_lines,
                        coverage.total_lines
                    )
                    .as_bytes(),
                )
                .await?;

            // Generate per-file HTML
            self.generate_file_html(&html_dir.join(&html_file), path, coverage)
                .await?;
        }

        index.write_all(b"</table>\n</body>\n</html>\n").await?;

        Ok(index_path)
    }

    /// Generate HTML for individual source file
    async fn generate_file_html(
        &self,
        output: &Path,
        source_path: &Path,
        coverage: &SourceCoverage,
    ) -> Result<()> {
        let mut file = tokio::fs::File::create(output).await?;

        file.write_all(b"<!DOCTYPE html>\n<html>\n<head>\n").await?;
        file.write_all(format!("<title>{}</title>\n", source_path.display()).as_bytes())
            .await?;
        file.write_all(b"<style>\n").await?;
        file.write_all(b"body { font-family: monospace; margin: 0; padding: 20px; }\n")
            .await?;
        file.write_all(b".line { padding: 2px 5px; }\n").await?;
        file.write_all(b".covered { background-color: #dff0d8; }\n")
            .await?;
        file.write_all(b".uncovered { background-color: #f2dede; }\n")
            .await?;
        file.write_all(b".line-number { display: inline-block; width: 50px; text-align: right; margin-right: 10px; color: #999; }\n").await?;
        file.write_all(b".hits { display: inline-block; width: 50px; text-align: right; margin-right: 10px; color: #666; }\n").await?;
        file.write_all(b"</style>\n</head>\n<body>\n").await?;
        file.write_all(format!("<h1>{}</h1>\n", source_path.display()).as_bytes())
            .await?;
        file.write_all(
            format!(
                "<p>Coverage: {:.2}% ({}/{} lines)</p>\n",
                coverage.line_coverage_percent(),
                coverage.covered_lines,
                coverage.total_lines
            )
            .as_bytes(),
        )
        .await?;

        // Read source file
        if let Ok(source_content) = tokio::fs::read_to_string(source_path).await {
            let mut line_coverage_map: HashMap<usize, &LineCoverage> = HashMap::new();
            for line_cov in &coverage.lines {
                line_coverage_map.insert(line_cov.line, line_cov);
            }

            for (line_num, line_text) in source_content.lines().enumerate() {
                let line_num = line_num + 1;
                let class = if let Some(cov) = line_coverage_map.get(&line_num) {
                    if cov.hits > 0 {
                        "covered"
                    } else {
                        "uncovered"
                    }
                } else {
                    ""
                };

                let hits = line_coverage_map
                    .get(&line_num)
                    .map(|c| c.hits)
                    .unwrap_or(0);
                let hits_str = if hits > 0 {
                    format!("{}", hits)
                } else {
                    "-".to_string()
                };

                file.write_all(format!("<div class=\"line {}\"><span class=\"line-number\">{}</span><span class=\"hits\">{}</span>{}</div>\n",
                    class, line_num, hits_str, html_escape(line_text)).as_bytes()).await?;
            }
        }

        file.write_all(b"</body>\n</html>\n").await?;
        Ok(())
    }

    /// Generate JSON format report
    async fn generate_json(&self, report: &CoverageReport) -> Result<PathBuf> {
        let output_path = self.output_dir.join("coverage.json");
        let json =
            serde_json::to_string_pretty(report).context("Failed to serialize coverage report")?;

        tokio::fs::write(&output_path, json)
            .await
            .context("Failed to write JSON report")?;

        Ok(output_path)
    }

    /// Generate Cobertura XML format report
    async fn generate_cobertura(&self, report: &CoverageReport) -> Result<PathBuf> {
        let output_path = self.output_dir.join("cobertura.xml");
        let mut file = tokio::fs::File::create(&output_path).await?;

        let line_rate = report.line_coverage_percent() / 100.0;
        let branch_rate = report.branch_coverage_percent() / 100.0;

        file.write_all(b"<?xml version=\"1.0\" ?>\n").await?;
        file.write_all(
            format!(
                "<coverage line-rate=\"{:.4}\" branch-rate=\"{:.4}\" timestamp=\"{}\">\n",
                line_rate,
                branch_rate,
                chrono::Utc::now().timestamp()
            )
            .as_bytes(),
        )
        .await?;
        file.write_all(b"<packages>\n").await?;

        // Group files by directory (package)
        let mut packages: HashMap<PathBuf, Vec<(&PathBuf, &SourceCoverage)>> = HashMap::new();
        for (path, coverage) in &report.files {
            let package = path.parent().unwrap_or(Path::new(".")).to_path_buf();
            packages.entry(package).or_default().push((path, coverage));
        }

        for (package_path, files) in packages {
            let package_name = package_path.to_string_lossy();
            file.write_all(format!("<package name=\"{}\">\n", package_name).as_bytes())
                .await?;
            file.write_all(b"<classes>\n").await?;

            for (path, coverage) in files {
                let filename = path.file_name().unwrap_or_default().to_string_lossy();
                let class_line_rate = coverage.line_coverage_percent() / 100.0;

                file.write_all(
                    format!(
                        "<class name=\"{}\" filename=\"{}\" line-rate=\"{:.4}\">\n",
                        filename,
                        path.display(),
                        class_line_rate
                    )
                    .as_bytes(),
                )
                .await?;
                file.write_all(b"<lines>\n").await?;

                for line in &coverage.lines {
                    let hits = if line.hits > 0 { 1 } else { 0 };
                    file.write_all(
                        format!("<line number=\"{}\" hits=\"{}\"/>\n", line.line, hits).as_bytes(),
                    )
                    .await?;
                }

                file.write_all(b"</lines>\n").await?;
                file.write_all(b"</class>\n").await?;
            }

            file.write_all(b"</classes>\n").await?;
            file.write_all(b"</package>\n").await?;
        }

        file.write_all(b"</packages>\n").await?;
        file.write_all(b"</coverage>\n").await?;

        Ok(output_path)
    }

    /// Check if coverage meets thresholds
    pub fn check_thresholds(&self, report: &CoverageReport) -> Result<()> {
        let line_coverage = report.line_coverage_percent();
        let branch_coverage = report.branch_coverage_percent();

        let mut violations = Vec::new();

        if line_coverage < self.thresholds.line_coverage {
            violations.push(format!(
                "Line coverage {:.2}% is below threshold {:.2}%",
                line_coverage, self.thresholds.line_coverage
            ));
        }

        if branch_coverage < self.thresholds.branch_coverage {
            violations.push(format!(
                "Branch coverage {:.2}% is below threshold {:.2}%",
                branch_coverage, self.thresholds.branch_coverage
            ));
        }

        if !violations.is_empty() && self.thresholds.fail_on_violation {
            return Err(anyhow!(
                "Coverage threshold violations:\n{}",
                violations.join("\n")
            ));
        }

        Ok(())
    }

    /// Generate coverage badge SVG
    pub async fn generate_badge(&self, report: &CoverageReport) -> Result<PathBuf> {
        let output_path = self.output_dir.join("coverage-badge.svg");
        let coverage = report.line_coverage_percent();

        let color = if coverage >= 90.0 {
            "brightgreen"
        } else if coverage >= 75.0 {
            "green"
        } else if coverage >= 60.0 {
            "yellow"
        } else if coverage >= 40.0 {
            "orange"
        } else {
            "red"
        };

        let svg = format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="120" height="20">
  <linearGradient id="b" x2="0" y2="100%">
    <stop offset="0" stop-color="#bbb" stop-opacity=".1"/>
    <stop offset="1" stop-opacity=".1"/>
  </linearGradient>
  <mask id="a">
    <rect width="120" height="20" rx="3" fill="#fff"/>
  </mask>
  <g mask="url(#a)">
    <path fill="#555" d="M0 0h61v20H0z"/>
    <path fill="{}" d="M61 0h59v20H61z"/>
    <path fill="url(#b)" d="M0 0h120v20H0z"/>
  </g>
  <g fill="#fff" text-anchor="middle" font-family="DejaVu Sans,Verdana,Geneva,sans-serif" font-size="11">
    <text x="30.5" y="15" fill="#010101" fill-opacity=".3">coverage</text>
    <text x="30.5" y="14">coverage</text>
    <text x="89.5" y="15" fill="#010101" fill-opacity=".3">{:.1}%</text>
    <text x="89.5" y="14">{:.1}%</text>
  </g>
</svg>"##,
            color, coverage, coverage
        );

        tokio::fs::write(&output_path, svg)
            .await
            .context("Failed to write badge SVG")?;

        Ok(output_path)
    }
}

/// Escape HTML special characters
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_coverage_report_creation() {
        let report = CoverageReport::new();
        assert_eq!(report.total_lines, 0);
        assert_eq!(report.covered_lines, 0);
        assert_eq!(report.line_coverage_percent(), 0.0);
    }

    #[test]
    fn test_source_coverage_percentage() {
        let coverage = SourceCoverage {
            path: PathBuf::from("test.rs"),
            lines: vec![
                LineCoverage {
                    line: 1,
                    hits: 1,
                    has_branch: false,
                    branch_taken: None,
                    branch_not_taken: None,
                },
                LineCoverage {
                    line: 2,
                    hits: 0,
                    has_branch: false,
                    branch_taken: None,
                    branch_not_taken: None,
                },
                LineCoverage {
                    line: 3,
                    hits: 1,
                    has_branch: false,
                    branch_taken: None,
                    branch_not_taken: None,
                },
                LineCoverage {
                    line: 4,
                    hits: 1,
                    has_branch: false,
                    branch_taken: None,
                    branch_not_taken: None,
                },
            ],
            branches: vec![],
            total_lines: 4,
            covered_lines: 3,
            total_branches: 0,
            covered_branches: 0,
        };

        assert_eq!(coverage.line_coverage_percent(), 75.0);
    }

    #[test]
    fn test_coverage_merge() {
        let mut report1 = CoverageReport::new();
        let coverage1 = SourceCoverage {
            path: PathBuf::from("test.rs"),
            lines: vec![
                LineCoverage {
                    line: 1,
                    hits: 1,
                    has_branch: false,
                    branch_taken: None,
                    branch_not_taken: None,
                },
                LineCoverage {
                    line: 2,
                    hits: 0,
                    has_branch: false,
                    branch_taken: None,
                    branch_not_taken: None,
                },
            ],
            branches: vec![],
            total_lines: 2,
            covered_lines: 1,
            total_branches: 0,
            covered_branches: 0,
        };
        report1.add_file(coverage1);

        let mut report2 = CoverageReport::new();
        let coverage2 = SourceCoverage {
            path: PathBuf::from("test.rs"),
            lines: vec![
                LineCoverage {
                    line: 1,
                    hits: 2,
                    has_branch: false,
                    branch_taken: None,
                    branch_not_taken: None,
                },
                LineCoverage {
                    line: 2,
                    hits: 1,
                    has_branch: false,
                    branch_taken: None,
                    branch_not_taken: None,
                },
            ],
            branches: vec![],
            total_lines: 2,
            covered_lines: 2,
            total_branches: 0,
            covered_branches: 0,
        };
        report2.add_file(coverage2);

        report1.merge(&report2);

        let merged = report1.files.get(&PathBuf::from("test.rs")).unwrap();
        assert_eq!(merged.lines[0].hits, 3); // 1 + 2
        assert_eq!(merged.lines[1].hits, 1); // 0 + 1
    }

    #[test]
    fn test_threshold_check() {
        let mut report = CoverageReport::new();
        let coverage = SourceCoverage {
            path: PathBuf::from("test.rs"),
            lines: vec![
                LineCoverage {
                    line: 1,
                    hits: 1,
                    has_branch: false,
                    branch_taken: None,
                    branch_not_taken: None,
                },
                LineCoverage {
                    line: 2,
                    hits: 1,
                    has_branch: false,
                    branch_taken: None,
                    branch_not_taken: None,
                },
            ],
            branches: vec![],
            total_lines: 2,
            covered_lines: 2,
            total_branches: 0,
            covered_branches: 0,
        };
        report.add_file(coverage);

        assert!(report.meets_threshold(100.0));
        assert!(report.meets_threshold(80.0));
        assert!(!report.meets_threshold(100.1));
    }

    #[tokio::test]
    async fn test_collector_creation() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path().to_path_buf();
        let output = temp_dir.path().join("coverage");

        let collector = CoverageCollector::new(workspace, output.clone());
        assert_eq!(collector.workspace_root, temp_dir.path());
        assert_eq!(collector.output_dir, output);
    }

    #[test]
    fn test_filter_changed_files() {
        let mut report = CoverageReport::new();

        let cov1 = SourceCoverage {
            path: PathBuf::from("file1.rs"),
            lines: vec![LineCoverage {
                line: 1,
                hits: 1,
                has_branch: false,
                branch_taken: None,
                branch_not_taken: None,
            }],
            branches: vec![],
            total_lines: 1,
            covered_lines: 1,
            total_branches: 0,
            covered_branches: 0,
        };

        let cov2 = SourceCoverage {
            path: PathBuf::from("file2.rs"),
            lines: vec![LineCoverage {
                line: 1,
                hits: 1,
                has_branch: false,
                branch_taken: None,
                branch_not_taken: None,
            }],
            branches: vec![],
            total_lines: 1,
            covered_lines: 1,
            total_branches: 0,
            covered_branches: 0,
        };

        report.add_file(cov1);
        report.add_file(cov2);

        let mut changed = HashSet::new();
        changed.insert(PathBuf::from("file1.rs"));

        let filtered = report.filter_changed(&changed);
        assert_eq!(filtered.files.len(), 1);
        assert!(filtered.files.contains_key(&PathBuf::from("file1.rs")));
    }

    #[test]
    fn test_html_escape() {
        assert_eq!(
            html_escape("<script>alert('xss')</script>"),
            "&lt;script&gt;alert(&#39;xss&#39;)&lt;/script&gt;"
        );
        assert_eq!(html_escape("normal text"), "normal text");
        assert_eq!(html_escape("a & b"), "a &amp; b");
    }
}
