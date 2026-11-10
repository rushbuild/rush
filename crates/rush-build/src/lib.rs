// Rush Build - Build orchestration
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Build orchestration layer.
//!
//! Coordinates loading, analysis, and execution phases.

#![allow(missing_docs)]

mod configured_flag_metadata;
mod native_options_header;
pub(crate) mod compiler_discovery;
pub(crate) mod configuration_table;
pub(crate) mod configured_target;

pub mod action_batch;
pub mod action_manager;
pub mod action_manifest;
pub mod action_timing;
mod generated_cc_sharding;
pub mod analysis_snapshot;
pub mod analyze_timing;
pub mod aspect;
pub mod cache_counters;

pub mod bazel_compat;
pub mod bazelrc;
pub mod benchmark;
pub mod bzlmod;
pub mod build_cleaner;
pub mod build_lock;
pub mod build_orchestrator_ext;
pub mod cache_gc;
pub mod cache_paths;
pub mod cold_noop_cache;
pub mod command_service;
pub mod candidate_blocker;
pub mod external_repo_manifest;
pub mod fetch_warmup_plan;
pub mod freshness_ledger;
pub mod phase_profile;
pub mod workspace_watcher;
pub mod legacy_deletion_dry_run;
pub mod legacy_scheduler_report;
pub mod ml_dtypes_detection;
pub mod verified_execution_shape;
pub mod virtual_materializer;
pub mod materialization_report;
pub mod cargo_parser;
pub mod compdb;
pub mod config;
pub mod coverage;
pub mod custom_rule_stub;
pub mod custom_rules;
#[cfg(unix)]
pub mod daemon;
pub mod dependency_graph;
pub mod depfile;
pub mod distributed;
pub mod events;
pub mod external_action_identity;
pub mod external_analysis_snapshot;
pub mod external_capture_adoption;
pub mod external_controller;
pub mod external_deps;
pub mod external_generation;
pub mod kbuild_receipt;
pub mod kbuild_snapshot;
pub mod external_repo_materialization;
pub mod external_replay;
pub mod graph_prune;
pub mod hotspot_summary;
pub mod parse_timing;
pub mod process_timing;
mod python_config;
pub mod incremental;
pub mod incremental_link;
pub mod lazy_eval;
pub mod lockfree_queue;
pub mod lsp;
pub mod memory_guard;
pub mod metrics;
pub mod mmap_io;
pub mod optimization;
pub mod output_tree;
pub(crate) mod native_execroot;
pub mod persistent_workers;
pub mod profile;
pub mod profiling;
pub mod progress;
pub mod proto_expander;
pub mod query;
pub mod remote_cache;
pub mod remote_exec;
pub mod reporter;
pub mod repository_manager;
pub mod reproducible;
pub mod rule_execution;
pub mod rules_cpp;
pub mod rules_genrule;
pub mod rules_go;
pub mod rules_java;
pub mod workspace_loader;

use crate::workspace_loader::WorkspaceLoader;
pub mod rules_proto;
pub mod rules_python;
pub mod rules_rust;
pub mod rules_shell;
pub mod sandbox;
pub mod scheduler;
pub mod stamping;
pub mod test_cache;
pub mod test_runner;
pub(crate) mod thin_archive;
pub mod toolchain;
pub mod transitions;
pub mod visualization;
pub mod watch;
pub mod work_stealing;
// Disabled: workspace_rules depends on removed ExternalDepManager types
// pub mod workspace_rules;

use action_manager::RushActionManager;
use anyhow::{Context, Result};
use aspect::AspectManager;
use incremental::ChangeDetector;
use incremental_link::IncrementalLinker;
use metrics::BuildMetrics;
use once_cell::sync::Lazy;
use owo_colors::OwoColorize;
use progress::BuildProgress;
use rush_action::{Action, ActionId, Artifact, Command, ExecutionInfo};
use rush_cache::ActionCache;
use rush_exec::{ExecutionStrategy, LocalStrategy, WorkerPool};
use rush_graph::EvaluationGraph;
use rush_parse::{parse_build_file_with_bzl_loader, Value};
use rush_rules::registry::RuleRegistry;
use std::collections::{HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use test_cache::{TestCache, TestResultParams};
use tracing::info;
// Re-export resource types from rush-resource crate
use repository_manager::RepositoryManager;
use resource::{ResourceManager, ResourceRequest};
pub use rush_resource as resource;
use tokio::task::JoinSet;

// Import external dependency mapper
use crate::external_deps::SystemPackageMapper;

// Re-export remote types for convenience
pub use rush_remote::{RemoteExecutor, RemoteExecutorStats, RemoteStrategy};

// Debug logging helper - only outputs if RUSH_DEBUG=1 is set
macro_rules! debug_log {
    ($($arg:tt)*) => {
        if std::env::var("RUSH_DEBUG").unwrap_or_default() == "1" {
            eprintln!($($arg)*);
        }
    };
}

pub use bazelrc::{BazelRc, BazelRcNamedConfig, CompilerFlags};
pub use benchmark::{BenchmarkResults, BenchmarkTimer};
pub use build_lock::BuildLock;
pub use compdb::{CompilationDatabase, CompilationEntry};
pub use depfile::{extract_headers, parse_depfile, DepFileEntry};
pub use events::{BuildEvent, BuildEventStream};
pub use incremental::ChangeDetectorStats;
pub use output_tree::{BuildConfig, OutputTree};
pub use profile::BuildProfiler;
pub use rush_cache::CacheConfig;
/// The one authority for where rush's caches live (see
/// `rush_cache::roots`), re-exported so the CLI and benchmark surfaces
/// resolve paths through the same code the cache itself uses.
pub use rush_cache::roots as cache_roots;

/// Type alias for dependency cache entry (outputs, include_dirs)
type DependencyCacheEntry = Arc<tokio::sync::Mutex<Option<(Vec<PathBuf>, Vec<PathBuf>)>>>;

/// A parsed BUILD file admitted against one command's tracked analysis inputs.
struct CachedBuildFile {
    build_file: Arc<rush_parse::BuildFile>,
    manifest: rush_incremental::DependencyManifest,
    admitted_io: std::sync::Weak<rush_incremental::RealTrackedIo>,
}

/// BUILD cache entry, locked while one task parses or validates the package.
///
/// The manifest contains the BUILD source, transitive `.bzl` reads, and glob or
/// existence probes that produced `build_file`. A later command validates it
/// before serving the value, then replays it into that command's manifest.
/// `None` leaves the lock holder responsible for parsing.
type BuildFileCacheEntry = Arc<tokio::sync::Mutex<Option<CachedBuildFile>>>;

/// Find workspace root by searching for WORKSPACE file.
pub fn find_workspace_root() -> Result<PathBuf> {
    let mut dir = std::env::current_dir()?;

    loop {
        // Check for WORKSPACE or WORKSPACE.bazel or MODULE.bazel
        if dir.join("WORKSPACE").exists()
            || dir.join("WORKSPACE.bazel").exists()
            || dir.join("MODULE.bazel").exists()
        {
            return Ok(dir);
        }

        // Move up one directory
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => anyhow::bail!("Could not find WORKSPACE file"),
        }
    }
}

/// Parse a target label into (package, name) components.
///
/// Examples:
/// - "//examples/hello_cpp:hello" → ("examples/hello_cpp", "hello")
/// - ":hello" → ("", "hello")
/// - "//pkg:target" → ("pkg", "target")
/// - "@repo//pkg:target" → ("pkg", "target") [repo prefix stripped]
fn parse_target_label(label: &str) -> Result<(String, String)> {
    // Handle external repository labels: @repo//package:name
    let working_label = if label.starts_with('@') {
        // Find the '//' that separates repo from package
        if let Some(slash_pos) = label.find("//") {
            // Strip @repo, keep //package:name
            &label[slash_pos..]
        } else {
            anyhow::bail!(
                "Invalid external repository label (missing '//'): {}",
                label
            )
        }
    } else {
        label
    };

    if working_label.starts_with("//") {
        // Absolute label: //package:name or //package (default target)
        let without_slashes = working_label.strip_prefix("//").unwrap();
        if let Some(colon_pos) = without_slashes.find(':') {
            let package = without_slashes[..colon_pos].to_string();
            let name = without_slashes[colon_pos + 1..].to_string();
            Ok((package, name))
        } else {
            // Package-only label: //package means //package:package
            // (the default target, typically named after the last component)
            let package = without_slashes.to_string();
            let name = package.rsplit('/').next().unwrap_or(&package).to_string();
            Ok((package, name))
        }
    } else if working_label.starts_with(':') {
        // Relative label: :name (current package)
        let name = working_label.strip_prefix(':').unwrap().to_string();
        Ok((String::new(), name))
    } else {
        anyhow::bail!(
            "Invalid target label (must start with '//', ':', or '@'): {}",
            label
        )
    }
}

/// FIX #188: Find BUILD file in a directory.
/// Bazel supports both "BUILD" and "BUILD.bazel" filenames.
/// `BUILD.bazel` takes precedence when both exist.
fn build_file_candidates(package_dir: &Path) -> [PathBuf; 2] {
    [package_dir.join("BUILD.bazel"), package_dir.join("BUILD")]
}

fn find_build_file(package_dir: &Path) -> Option<PathBuf> {
    build_file_candidates(package_dir)
        .into_iter()
        .find(|path| path.exists())
}

/// Build request specifying targets to build.
#[derive(Debug, Clone)]
pub struct BuildRequest {
    /// Target labels to build (e.g., "//pkg:target").
    pub targets: Vec<String>,
}

/// Test output verbosity mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestOutputMode {
    /// Only show summary (default).
    Summary,
    /// Show output from failed tests.
    Errors,
    /// Show output from all tests.
    All,
}

impl TestOutputMode {
    /// Parse from string.
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "summary" => Ok(TestOutputMode::Summary),
            "errors" => Ok(TestOutputMode::Errors),
            "all" => Ok(TestOutputMode::All),
            _ => anyhow::bail!(
                "Invalid test output mode: {}. Expected: summary, errors, or all",
                s
            ),
        }
    }
}

/// Result from running a single test.
#[derive(Debug, Clone)]
pub struct TestResult {
    /// Test label (e.g., "//pkg:test").
    pub label: String,
    /// Whether the test passed.
    pub passed: bool,
    /// Test execution duration.
    pub duration: std::time::Duration,
    /// Exit code from test binary.
    pub exit_code: i32,
    /// Standard output from test.
    pub stdout: String,
    /// Standard error from test.
    pub stderr: String,
}

/// Collection of test results.
#[derive(Debug, Clone)]
pub struct TestResults {
    /// Individual test results.
    pub results: Vec<TestResult>,
}

impl TestResults {
    /// Create a new empty test results collection.
    pub fn new() -> Self {
        TestResults {
            results: Vec::new(),
        }
    }

    /// Add a test result.
    pub fn add(&mut self, result: TestResult) {
        self.results.push(result);
    }

    /// Get total number of tests.
    pub fn total(&self) -> usize {
        self.results.len()
    }

    /// Get number of passed tests.
    pub fn passed(&self) -> usize {
        self.results.iter().filter(|r| r.passed).count()
    }

    /// Get number of failed tests.
    pub fn failed(&self) -> usize {
        self.results.iter().filter(|r| !r.passed).count()
    }

    /// Check if all tests passed.
    pub fn all_passed(&self) -> bool {
        self.results.iter().all(|r| r.passed)
    }

    /// Print summary of test results.
    pub fn print_summary(&self) {
        eprintln!("\n{}", "📊 Test Summary:".bold().cyan());

        let total_text = format!("Total: {}", self.total());
        eprintln!("   {}", total_text.bright_white());

        let passed = self.passed();
        let passed_text = format!("Passed: {} ✅", passed);
        if passed > 0 {
            eprintln!("   {}", passed_text.green());
        } else {
            eprintln!("   {}", passed_text.bright_white());
        }

        let failed = self.failed();
        let failed_text = format!("Failed: {} ❌", failed);
        if failed > 0 {
            eprintln!("   {}", failed_text.red().bold());
        } else {
            eprintln!("   {}", failed_text.bright_white());
        }

        if self.all_passed() {
            eprintln!("\n{}", "🎉 All tests passed!".green().bold());
        } else {
            eprintln!("\n{}", "❌ Some tests failed".red().bold());
            eprintln!("\n{}:", "Failed tests".yellow());
            for result in &self.results {
                if !result.passed {
                    eprintln!(
                        "   {} {} {}",
                        "•".red(),
                        result.label.bright_white(),
                        format!("(exit code: {})", result.exit_code).red()
                    );
                }
            }
        }
    }
}

impl Default for TestResults {
    fn default() -> Self {
        Self::new()
    }
}

/// Global singleton .bzl cache shared across all BuildOrchestrator instances.
///
/// This ensures only ONE 2GB cache exists for the entire build process,
/// instead of creating multiple 2GB caches (one per BuildOrchestrator).
///
/// Root cause fix for OOM issue: Previously each constructor created its own
/// GlobalBzlCache::new(), leading to 10+ instances × 2GB = 20+ GB memory usage.
static GLOBAL_BZL_CACHE: Lazy<rush_starlark::GlobalBzlCache> = Lazy::new(|| {
    // Phase 5.C.4 commit 97: env-var-tunable cache caps.
    //
    // RUSH_BZL_CACHE_MAX_ENTRIES   — override the entry
    //                                count limit. Default
    //                                is GlobalBzlCache::DEFAULT_MAX_ENTRIES.
    // RUSH_BZL_CACHE_MAX_BYTES     — override the byte
    //                                limit. Accepts:
    //                                  plain integer (bytes),
    //                                  Nk / NM / NG / NT (KiB/MiB/GiB/TiB).
    //                                Default is
    //                                GlobalBzlCache::DEFAULT_MAX_MEMORY_BYTES.
    //
    // Defaults are UNCHANGED in this commit. Operators
    // must set the env vars explicitly to opt into a
    // different cap. A future commit (commit 100) may
    // change defaults based on sweep data — see
    // docs/scheduler-preflight-census.md.
    let entries = std::env::var("RUSH_BZL_CACHE_MAX_ENTRIES")
        .ok()
        .and_then(|s| parse_cache_entries(&s))
        .unwrap_or(rush_starlark::GlobalBzlCache::DEFAULT_MAX_ENTRIES);
    let bytes = std::env::var("RUSH_BZL_CACHE_MAX_BYTES")
        .ok()
        .and_then(|s| parse_cache_bytes(&s))
        .unwrap_or(rush_starlark::GlobalBzlCache::DEFAULT_MAX_MEMORY_BYTES);
    debug_log!(
        "[CACHE] Initializing singleton GlobalBzlCache ({} bytes limit, {} entries)",
        bytes,
        entries
    );
    let cache = rush_starlark::GlobalBzlCache::with_limits(entries, bytes);
    // Phase 5.C.4 commit 133 (Batch Q): register the
    // process-singleton cache with bzl_loader so static
    // fetch paths (fetch_repo_via_bazel) can record
    // bazel-fetch subprocess wait time. The OnceLock
    // accepts the first set; subsequent calls are no-ops,
    // so registering here is safe and idempotent.
    rush_starlark::bzl_loader::register_cache_for_fetch_timing(cache.clone());

    // Phase 5.C.4 commit 148 (Batch S): register the
    // external-repo manifest writer so rush-starlark
    // can call through it after every successful
    // fetch (per-repo or combined warmup).
    rush_starlark::fetch_strategy::register_manifest_writer(
        std::sync::Arc::new(
            |workspace_root, repo_name, external_path, cmd| {
                let outcome =
                    crate::external_repo_manifest::write_manifest_for_repo(
                        workspace_root,
                        repo_name,
                        external_path,
                        cmd,
                    );
                match outcome {
                    crate::external_repo_manifest::ManifestWriteOutcome::Written(_) => {
                        "written".to_string()
                    }
                    // Phase 5.C.4 commit 175 (Batch W):
                    // unchanged on-disk manifest -> skip.
                    // Operator-visible bucket distinct
                    // from Written so counter wiring
                    // can show "manifest is current"
                    // vs "manifest was rewritten this run."
                    crate::external_repo_manifest::ManifestWriteOutcome::SkippedUnchanged(_) => {
                        "skipped_unchanged".to_string()
                    }
                    crate::external_repo_manifest::ManifestWriteOutcome::SkippedNoBuild => {
                        "skipped_no_build".to_string()
                    }
                    crate::external_repo_manifest::ManifestWriteOutcome::SkippedNoDir => {
                        "skipped_no_dir".to_string()
                    }
                    crate::external_repo_manifest::ManifestWriteOutcome::Failed(reason) => {
                        eprintln!(
                            "[manifest] write failed for @{}: {}",
                            repo_name, reason
                        );
                        format!("failed: {}", reason)
                    }
                }
            },
        ),
    );

    // Phase 5.C.4 commit 149 (Batch S): register
    // the manifest reader so the materialized-skip
    // path can distinguish manifest-validated skips
    // from trust-mode skips.
    rush_starlark::fetch_strategy::register_manifest_reader(
        std::sync::Arc::new(|workspace_root, repo_name| {
            crate::external_repo_manifest::read_manifest_status(
                workspace_root,
                repo_name,
            )
            .as_str()
            .to_string()
        }),
    );

    cache
});

/// BUILD packages already scanned for literal configuration settings during
/// the current command. The set is cleared at each command boundary; source
/// reads are tracked and each package snapshot replaces its prior definitions.
static PRESCANNED_CONFIG_PACKAGES: Lazy<Mutex<HashMap<String, HashSet<String>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

thread_local! {
    /// Same-thread recursion guard for configuration-package evaluation.
    /// Concurrent callers intentionally do not share it: they converge on the
    /// per-BUILD cache mutex and wait for the first evaluation to complete.
    static CONFIG_PACKAGE_EVALUATION_STACK: std::cell::RefCell<HashSet<String>> =
        std::cell::RefCell::new(HashSet::new());
}

struct ConfigPackageEvaluationGuard(String);

impl ConfigPackageEvaluationGuard {
    fn enter(key: String) -> Option<Self> {
        CONFIG_PACKAGE_EVALUATION_STACK
            .with(|stack| stack.borrow_mut().insert(key.clone()))
            .then_some(Self(key))
    }
}

impl Drop for ConfigPackageEvaluationGuard {
    fn drop(&mut self) {
        CONFIG_PACKAGE_EVALUATION_STACK.with(|stack| {
            stack.borrow_mut().remove(&self.0);
        });
    }
}

/// Phase 5.C.4 commit 97: parse an entry-count override
/// from an env var. Rejects zero (would disable cache);
/// rejects non-integers; returns `None` on bad input so
/// the default is used.
fn parse_cache_entries(s: &str) -> Option<usize> {
    let n: usize = s.trim().parse().ok()?;
    if n == 0 {
        eprintln!(
            "warning: RUSH_BZL_CACHE_MAX_ENTRIES={} rejected \
             (must be > 0); using default",
            s
        );
        return None;
    }
    Some(n)
}

/// Phase 5.C.4 commit 97: parse a byte-count override
/// from an env var. Accepts plain integers (bytes) plus
/// the suffixes `k`/`K`/`m`/`M`/`g`/`G`/`t`/`T`
/// (case-insensitive; binary multipliers).
///
/// Rejects zero (would disable cache); rejects bad
/// input; returns `None` so the default is used.
fn parse_cache_bytes(s: &str) -> Option<usize> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (digits, mult): (&str, usize) = match trimmed.chars().last()? {
        'k' | 'K' => (&trimmed[..trimmed.len() - 1], 1024),
        'm' | 'M' => (&trimmed[..trimmed.len() - 1], 1024 * 1024),
        'g' | 'G' => (&trimmed[..trimmed.len() - 1], 1024 * 1024 * 1024),
        't' | 'T' => (&trimmed[..trimmed.len() - 1], 1024 * 1024 * 1024 * 1024),
        c if c.is_ascii_digit() => (trimmed, 1),
        _ => return None,
    };
    let n: usize = digits.trim().parse().ok()?;
    if n == 0 {
        eprintln!(
            "warning: RUSH_BZL_CACHE_MAX_BYTES={} rejected \
             (must be > 0); using default",
            s
        );
        return None;
    }
    n.checked_mul(mult)
}

#[derive(Debug, Default)]
pub(crate) struct PackageDeclarationCatalog {
    producers: HashMap<String, String>,
}

impl PackageDeclarationCatalog {
    pub(crate) fn producer_for(&self, label_name: &str) -> Option<&str> {
        self.producers.get(label_name).map(String::as_str)
    }
}

/// Orchestrates the build process.
pub struct BuildOrchestrator {
    /// Workspace root directory.
    workspace_root: PathBuf,

    /// Incremental evaluation graph.
    graph: Arc<EvaluationGraph>,

    /// Action result cache.
    cache: Arc<ActionCache>,

    /// Output tree for organizing build artifacts.
    output_tree: Arc<OutputTree>,

    /// Build progress tracker.
    progress: Arc<BuildProgress>,

    /// Build profiler (optional).
    profiler: Arc<BuildProfiler>,

    /// Build event stream (optional).
    events: Arc<BuildEventStream>,

    /// Build metrics collector.
    metrics: Arc<BuildMetrics>,

    /// Change detector for incremental builds.
    change_detector: Arc<tokio::sync::Mutex<ChangeDetector>>,

    /// Compilation database for IDE integration.
    compdb: Arc<tokio::sync::Mutex<CompilationDatabase>>,

    /// Enable real-time output streaming.
    streaming_enabled: Arc<std::sync::atomic::AtomicBool>,

    /// Persistent worker pool for fast execution.
    worker_pool: Arc<WorkerPool>,

    /// Incremental linker for fast iteration.
    incremental_linker: Arc<Mutex<IncrementalLinker>>,

    /// Remote executor for distributed builds (optional).
    remote_executor: Option<Arc<RemoteExecutor>>,

    /// Test result cache for fast test execution.
    test_cache: Arc<Mutex<TestCache>>,

    /// Aspect manager for cross-cutting concerns.
    aspects: Arc<Mutex<AspectManager>>,

    /// Cache of parsed BUILD files to avoid re-parsing.
    /// Uses LRU eviction to prevent unbounded memory growth on large projects.
    /// Default capacity: 500 BUILD files (configurable via RUSH_BUILD_CACHE_SIZE).
    /// Entries are wrapped in tokio::sync::Mutex to prevent concurrent parsing of the same file.
    build_file_cache: Arc<Mutex<lru::LruCache<String, BuildFileCacheEntry>>>,

    /// Rule registry for extensible build rules.
    rule_registry: Arc<RuleRegistry>,

    /// Dynamic resource manager for memory and CPU allocation.
    /// Tracks actual resource usage to prevent OOM during builds.
    resource_manager: Arc<ResourceManager>,

    /// Global shared .bzl cache across all BUILD files.
    /// This cache is shared by all BzlLoader instances to prevent re-parsing
    /// the same .bzl files multiple times when building different packages.
    global_bzl_cache: rush_starlark::GlobalBzlCache,

    /// External dependency manager for WORKSPACE dependencies.
    /// NOTE: Replaced by SystemPackageMapper (static methods)
    // external_dep_manager: Arc<Mutex<external_deps::ExternalDepManager>>,

    /// Map from external repository name to path.
    pub external_repo_paths: Arc<tokio::sync::Mutex<HashMap<String, PathBuf>>>,

    /// Repository manager for handling external repositories.
    pub repository_manager: Arc<RepositoryManager>,

    /// .bzl file loader for Starlark macros and helper functions.
    bzl_loader: Arc<Mutex<rush_starlark::BzlLoader>>,

    /// Cache of in-progress and completed dependency builds.
    /// Maps dependency label to (outputs, include_dirs) to avoid redundant builds.
    /// Uses LRU eviction to prevent unbounded memory growth on large projects.
    /// Default capacity: 1000 dependencies (configurable via RUSH_DEP_CACHE_SIZE).
    dependency_cache: Arc<Mutex<lru::LruCache<String, DependencyCacheEntry>>>,

    /// Memory guard for preventing OOM and detecting memory bugs.
    memory_guard: Arc<memory_guard::MemoryGuard>,

    /// Action manager for executing build actions.
    action_manager: Arc<RushActionManager>,

    /// The current command's tracked analysis IO (persistent-daemon brick 1b).
    ///
    /// Installed once at the [`build_with_dependencies`](Self::build_with_dependencies)
    /// entry from a [`CommandSemantics::from_process`] snapshot, wrapped in an
    /// `Arc<RealTrackedIo>`, and used to install `tracked::scope` around the
    /// synchronous analysis regions and to `tracked::bind` across the BUILD-parse
    /// `spawn_blocking` / `.bzl`-eval `thread::spawn` boundaries. A single shared
    /// `Arc` per command means every migrated read — on any of those threads —
    /// records into the same manifest. `None` until the entry installs it; held
    /// behind a `Mutex` so the `&self`/`Clone` orchestrator can set it once.
    tracked_io: Arc<std::sync::Mutex<Option<Arc<rush_incremental::RealTrackedIo>>>>,
    feature_flag_command: Arc<std::sync::Mutex<Option<configured_flag_metadata::FeatureFlagCommandState>>>,

    /// Retained analysis snapshots (persistent-daemon Run-wiring brick).
    ///
    /// One live `BuildOrchestrator` per (workspace, output, config) owns the
    /// shared process state; the per-target reusable thing is an
    /// [`AnalysisSnapshot`](crate::analysis_snapshot::AnalysisSnapshot). On an
    /// identical command whose dependency manifest still validates `Fresh`, the
    /// fully analyzed graph is reused (skipping `.bzl` eval / `BUILD` parse /
    /// PASS-A) and only execution runs — the no-op collapse. Shared via `Arc`
    /// so a cloned orchestrator sees the same cache.
    snapshot_cache: Arc<crate::analysis_snapshot::SnapshotCache>,

    /// Per-process reuse / analysis-work counters proving the invariant (a
    /// reuse performs ZERO `.bzl`/package/target analysis). Shared via `Arc`.
    reuse_counters: Arc<crate::analysis_snapshot::ReuseCounters>,

    /// Phase-2 daemon dirty-set filesystem watcher (the crush-Bazel plan).
    ///
    /// UNARMED by default — single-shot / in-process builds behave exactly as
    /// before (always full-validate). The daemon (`rush daemon`) arms it once
    /// over the workspace + external + output roots; a healthy watcher lets a
    /// warm no-op skip the ~20k-dep manifest stat-walk when nothing it depends on
    /// has changed (validate 0 paths). The watcher is a PERF HINT — on any doubt
    /// (overflow/broken/no-continuity/dirty intersection) the reuse path falls
    /// back to the full Phase-1 manifest validate. Shared via `Arc` so a cloned
    /// orchestrator sees the same armed watcher.
    watcher: Arc<crate::workspace_watcher::WorkspaceWatcher>,
}

#[derive(Default)]
struct ConfigurationMetadataPreload {
    visited: std::collections::HashSet<String>,
    alias_nodes: std::collections::BTreeSet<String>,
    edges: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    flag_conditions: std::collections::BTreeSet<String>,
    deferred_alias_errors: Vec<anyhow::Error>,
}

impl ConfigurationMetadataPreload {
    fn classify_flag_proofs(
        &self, scope: &str, roots: &[String], inherited: &std::collections::BTreeSet<String>,
    ) -> (std::collections::BTreeSet<String>, std::collections::BTreeMap<String, std::collections::BTreeSet<String>>) {
        let mut queue = std::collections::VecDeque::new();
        for label in roots {
            if inherited.is_empty() { queue.push_back((label.clone(),None)); }
            else { queue.extend(inherited.iter().cloned().map(|origin| (label.clone(),Some(origin)))); }
        }
        let mut visited = std::collections::HashSet::new();
        let mut required = std::collections::BTreeSet::new();
        let mut provenance: std::collections::BTreeMap<String, std::collections::BTreeSet<String>> = std::collections::BTreeMap::new();
        while let Some((label, mut origin)) = queue.pop_front() {
            if origin.is_none() && self.alias_nodes.contains(&label)
                && rush_starlark::config_defines::pycross_environment_alias_profile_in_scope(scope, &label).is_some() {
                origin = Some(label.clone());
            }
            if !visited.insert((label.clone(), origin.clone())) { continue; }
            if let Some(origin) = &origin { provenance.entry(label.clone()).or_default().insert(origin.clone()); }
            if self.flag_conditions.contains(&label) && origin.is_none() { required.insert(label.clone()); }
            if let Some(edges) = self.edges.get(&label) {
                queue.extend(edges.iter().cloned().map(|label| (label, origin.clone())));
            }
        }
        (required, provenance)
    }
}

/// Configuration state is keyed by canonical workspace path, so every public
/// command for that path must be single-flight even when callers independently
/// construct orchestrators instead of cloning one instance.
static WORKSPACE_COMMAND_LOCKS: once_cell::sync::Lazy<
    std::sync::Mutex<
        HashMap<PathBuf, std::sync::Weak<tokio::sync::Mutex<()>>>,
    >,
> = once_cell::sync::Lazy::new(|| std::sync::Mutex::new(HashMap::new()));

fn workspace_command_lock(workspace_root: &Path) -> Arc<tokio::sync::Mutex<()>> {
    let key = workspace_root.canonicalize().unwrap_or_else(|_| {
        if workspace_root.is_absolute() {
            workspace_root.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_default()
                .join(workspace_root)
        }
    });
    let mut locks = WORKSPACE_COMMAND_LOCKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(lock) = locks.get(&key).and_then(std::sync::Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

struct CudaConfig {
    include_dir: PathBuf,
    lib_dir: PathBuf,
}

fn legacy_cc_binary_shared_mode(rule: &rush_parse::Rule) -> Result<bool> {
    let explicit = match rule.attributes.get("linkshared") {
        None => None,
        Some(Value::Bool(value)) => Some(*value),
        Some(Value::Int(0)) => Some(false),
        Some(Value::Int(1)) => Some(true),
        Some(value) => anyhow::bail!(
            "cc_binary attribute 'linkshared' must be a bool or 0/1 int, got {:?}",
            value,
        ),
    };
    Ok(explicit.unwrap_or_else(|| rule.name.ends_with(".so"))
        || matches!(
            rule.rule_type.as_str(),
            "tf_custom_op_library"
                | "tf_cc_shared_object"
                | "cc_shared_library"
                | "tf_cc_shared_library"
        ))
}

fn legacy_cc_binary_linkopts(rule: &rush_parse::Rule) -> Result<Vec<String>> {
    match rule.attributes.get("linkopts") {
        None => Ok(Vec::new()),
        Some(Value::List(values)) => values
            .iter()
            .map(|value| match value {
                Value::String(linkopt) => Ok(linkopt.clone()),
                other => anyhow::bail!(
                    "cc_binary attribute 'linkopts' must contain strings, got {:?}",
                    other,
                ),
            })
            .collect(),
        Some(value) => anyhow::bail!(
            "cc_binary attribute 'linkopts' must be a list, got {:?}",
            value,
        ),
    }
}

fn legacy_expand_cc_binary_linkopts(
    rule: &rush_parse::Rule,
    package: &str,
    dep_outputs_by_label: &HashMap<String, Vec<PathBuf>>,
) -> Result<(Vec<String>, Vec<PathBuf>)> {
    let raw_linkopts = legacy_cc_binary_linkopts(rule)?;
    rush_rules::rules_cc::expand_cc_binary_linkopts(&raw_linkopts, |label| {
        let mut matched = None;
        for (declared_label, outputs) in dep_outputs_by_label {
            if !rush_rules::providers::dep_label_matches_repository_exact(
                declared_label,
                label,
                package,
                None,
            ) {
                continue;
            }
            if matched.is_some_and(|existing| existing != outputs) {
                anyhow::bail!(
                    "cc_binary linkopts label {} ambiguously matches multiple exact declared dependencies",
                    label,
                );
            }
            matched = Some(outputs);
        }
        let outputs = matched.ok_or_else(|| {
            anyhow::anyhow!(
                "cc_binary linkopts label {} is not an exact declared dependency",
                label,
            )
        })?;
        Ok(outputs
            .iter()
            .map(|path| (path.clone(), path.clone()))
            .collect())
    })
}

fn legacy_cc_binary_positional_link_input(path: &Path) -> bool {
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("");
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some("o" | "obj" | "a" | "lib" | "lo" | "so" | "dylib")
    ) || name.contains(".so.")
}

impl Clone for BuildOrchestrator {
    fn clone(&self) -> Self {
        BuildOrchestrator {
            workspace_root: self.workspace_root.clone(),
            graph: Arc::clone(&self.graph),
            cache: Arc::clone(&self.cache),
            output_tree: Arc::clone(&self.output_tree),
            progress: Arc::clone(&self.progress),
            profiler: Arc::clone(&self.profiler),
            events: Arc::clone(&self.events),
            metrics: Arc::clone(&self.metrics),
            change_detector: Arc::clone(&self.change_detector),
            compdb: Arc::clone(&self.compdb),
            streaming_enabled: Arc::clone(&self.streaming_enabled),
            worker_pool: Arc::clone(&self.worker_pool),
            incremental_linker: Arc::clone(&self.incremental_linker),
            remote_executor: self.remote_executor.clone(),
            test_cache: Arc::clone(&self.test_cache),
            aspects: Arc::clone(&self.aspects),
            build_file_cache: Arc::clone(&self.build_file_cache),
            rule_registry: Arc::clone(&self.rule_registry),
            resource_manager: Arc::clone(&self.resource_manager),
            repository_manager: Arc::clone(&self.repository_manager),
            global_bzl_cache: self.global_bzl_cache.clone(),
            // external_dep_manager: Arc::clone(&self.external_dep_manager),
            external_repo_paths: Arc::clone(&self.external_repo_paths),
            bzl_loader: Arc::clone(&self.bzl_loader),
            dependency_cache: Arc::clone(&self.dependency_cache),

            memory_guard: Arc::clone(&self.memory_guard),
            action_manager: Arc::clone(&self.action_manager),
            tracked_io: Arc::clone(&self.tracked_io),
            feature_flag_command: Arc::clone(&self.feature_flag_command),
            snapshot_cache: Arc::clone(&self.snapshot_cache),
            reuse_counters: Arc::clone(&self.reuse_counters),
            watcher: Arc::clone(&self.watcher),
        }
    }
}

/// Create the map of built-in external repositories that are always available.
///
/// These are "magic" repositories like @bazel_tools that don't need to be
/// declared in WORKSPACE but are available to all .bzl files.
///
/// This function also materializes the placeholder files for these repositories.
fn create_builtin_repos(workspace_root: &Path) -> anyhow::Result<HashMap<String, PathBuf>> {
    let mut repos = HashMap::new();
    let bzlmod_only = workspace_root.join("MODULE.bazel").is_file()
        && !workspace_root.join("WORKSPACE").exists()
        && !workspace_root.join("WORKSPACE.bazel").exists();

    // @bazel_tools - Bazel's built-in tooling. Bzlmod projects must use the
    // real embedded-tools repository from the shared Bazel output base; the
    // legacy minimal repository omits modern APIs such as action_names.bzl.
    if bzlmod_only {
        let cache_root = std::env::var("RUSH_WORKSPACE_CACHE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| workspace_root.join(".cache").join("rush"));
        let bazel_tools_path = cache_root
            .join("bazel_output_base")
            .join("external")
            .join("bazel_tools");
        if bazel_tools_path.exists() {
            repos.insert("bazel_tools".to_string(), bazel_tools_path);
        }
    } else {
        let bazel_tools_path =
            cache_paths::workspace_external_dir(workspace_root).join("bazel_tools");
        BuildOrchestrator::create_bazel_tools_minimal(&bazel_tools_path)?;
        repos.insert("bazel_tools".to_string(), bazel_tools_path);
    }

    // SystemPackageMapper repositories are compatibility shims for legacy
    // WORKSPACE builds. In a Bzlmod-only workspace they shadow canonical repos
    // materialized by Bazel (for example com_google_protobuf -> protobuf+).
    // Leave those projects to BzlLoader's repository mapping + fetch path.
    if !bzlmod_only {
        let known_repos = SystemPackageMapper::get_known_repository_paths(workspace_root)?;
        for (name, path) in known_repos {
            repos.insert(name, path);
        }
    }

    Ok(repos)
}

/// Parameters for source file compilation.
struct CompileSourceParams<'a> {
    workspace_root: &'a Path,
    cache: &'a ActionCache,
    output_tree: &'a OutputTree,
    progress: &'a BuildProgress,
    compdb: &'a Arc<tokio::sync::Mutex<CompilationDatabase>>,
    remote_executor: Option<Arc<RemoteExecutor>>,
    streaming_enabled: bool,
    src: &'a str,
    package: &'a str,
    include_dirs: &'a [PathBuf],
    is_shared_library: bool,
}

impl BuildOrchestrator {
    /// The shared Phase-2 dirty-set watcher (unarmed by default).
    pub fn watcher(&self) -> Arc<crate::workspace_watcher::WorkspaceWatcher> {
        Arc::clone(&self.watcher)
    }

    /// Arm the daemon's filesystem watcher over the relevant roots so a healthy
    /// warm no-op can validate ZERO paths. Called once by `rush daemon` after
    /// the orchestrator is built (before serving). Idempotent (re-arming resets
    /// the dirty state). On any failure to establish coverage the watcher reports
    /// Broken and every no-op falls back to the full Phase-1 manifest validate —
    /// so arming can never cause a stale build, only a slower one.
    ///
    /// Watch roots (per the crush-Bazel plan):
    /// * the workspace root (source / BUILD / .bzl edits, glob dir-listings),
    /// * external-repo roots: the global clone cache `external/` dir
    ///   (`RUSH_GLOBAL_CLONE_CACHE`) plus the in-workspace `external/` dir,
    /// We deliberately do NOT recursively watch `rush-out` (build outputs): the
    /// analysis manifest never lists an output path, and output freshness is
    /// covered by the freshness ledger's own stat of the recorded outputs. Adding
    /// a multi-GB output tree would blow the inotify watch-count limit for no
    /// correctness gain (a changed output still re-stats via the ledger).
    pub fn arm_watcher(&self) {
        let mut roots: Vec<PathBuf> = Vec::new();
        // Workspace root, canonicalized (so the watched paths match the
        // canonicalized paths recorded in the manifest).
        let ws = std::fs::canonicalize(&self.workspace_root)
            .unwrap_or_else(|_| self.workspace_root.clone());
        roots.push(ws.clone());
        // The in-workspace external/ tree (materialized external repos).
        let ws_external = ws.join("external");
        if ws_external.is_dir() {
            roots.push(ws_external);
        }
        // The global clone cache's external roots, if configured.
        if let Ok(cache) = std::env::var("RUSH_GLOBAL_CLONE_CACHE") {
            if !cache.is_empty() {
                let c = PathBuf::from(&cache);
                let ext = c.join("external");
                if ext.is_dir() {
                    roots.push(std::fs::canonicalize(&ext).unwrap_or(ext));
                } else if c.is_dir() {
                    roots.push(std::fs::canonicalize(&c).unwrap_or(c));
                }
            }
        }
        roots.sort();
        roots.dedup();
        self.watcher.arm(&roots);
    }

    /// Create LRU cache for BUILD files with configurable capacity.
    ///
    /// Default: 500 BUILD files (~50-150MB for large projects like TensorFlow)
    /// Override with: RUSH_BUILD_CACHE_SIZE=1000
    fn create_build_file_cache() -> lru::LruCache<String, BuildFileCacheEntry> {
        let capacity = std::env::var("RUSH_BUILD_CACHE_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(500);

        use std::num::NonZeroUsize;
        let capacity = NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::new(500).unwrap());

        lru::LruCache::new(capacity)
    }

    /// Create LRU cache for dependency builds with configurable capacity.
    ///
    /// Prevents unbounded memory growth from caching dependency build results
    /// (output paths + include directories) for every target dependency.
    ///
    /// Default: 1000 dependencies (~100-500MB for large dependency graphs)
    /// Override with: RUSH_DEP_CACHE_SIZE=2000
    ///
    /// On projects like TensorFlow with thousands of interdependent targets,
    /// unbounded caching can consume 5-15GB+. LRU eviction prevents OOM.
    fn create_dependency_cache() -> lru::LruCache<String, DependencyCacheEntry> {
        let capacity = std::env::var("RUSH_DEP_CACHE_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1000);

        use std::num::NonZeroUsize;
        let capacity = NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::new(1000).unwrap());

        lru::LruCache::new(capacity)
    }

    /// Create a new build orchestrator.
    ///
    /// Finds workspace root by searching for WORKSPACE file.
    pub fn new() -> Result<Self> {
        let workspace_root = find_workspace_root()?;
        let output_tree = OutputTree::new(workspace_root.clone(), BuildConfig::Fastbuild);
        let change_detector = ChangeDetector::new(&workspace_root);
        let compdb = CompilationDatabase::new(workspace_root.clone());
        let test_cache_dir = workspace_root.join(".rush-cache").join("test-results");
        let test_cache = TestCache::new(test_cache_dir);

        // Create rule registry and register TensorFlow rules
        let mut rule_registry = RuleRegistry::new();
        tensorflow_rules::register_tensorflow_rules(&mut rule_registry);

        // Register standard C++ build rules
        rule_registry.register(Box::new(rush_rules::rules_cc::CcLibraryRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::CcBinaryRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::CcTestRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::FilegroupRule));
        // Phase 5.C.4 commit 8: rush_rules::rules_cc::TfCcLibraryRule
        // is an alias that delegates to CcLibraryRule. The real
        // TF-shape impl with TF-specific argv (and the
        // build_legacy_command_specs the verifier needs) lives in
        // tensorflow-rules and was already registered above via
        // register_tensorflow_rules(). Registering the alias here
        // would overwrite the real one in the HashMap (last wins),
        // so we skip it. Same reasoning for TfCcTestRule.
        rule_registry.register(Box::new(rush_rules::rules_cc::TfCudaLibraryRule));

        // Register protocol buffer rules
        rule_registry.register(Box::new(rush_rules::rules_cc::ProtoLibraryRule));
        rule_registry.register(Box::new(rush_rules::rules_proto_toolchain::ProtoLangToolchainRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::CcProtoLibraryRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::CcSharedLibraryRule));

        // Register Python rules (graph-only tracking; no .py compilation)
        rule_registry.register(Box::new(rush_rules::rules_python::PyLibraryRule));
        rule_registry.register(Box::new(rush_rules::rules_python::PyBinaryRule));
        rule_registry.register(Box::new(rush_rules::rules_python::PyTestRule));
        rule_registry.register(Box::new(rush_rules::rules_sh::ShBinaryRule));
        rule_registry.register(Box::new(rush_rules::rules_sh::ShTestRule));

        // Register test utility rules
        rule_registry.register(Box::new(rush_rules::rules_cc::TestSuiteRule));

        // Register Bazel utility rules
        rule_registry.register(Box::new(rush_rules::rules_cc::PackageGroupRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::ExportsFilesRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::SourceFileRule));
        rule_registry.register(Box::new(rush_rules::rules_config::ConfigSettingRule));
        for rule_type in [
            "platform",
            "toolchain",
            "toolchain_type",
            "constraint_setting",
            "constraint_value",
        ] {
            rule_registry.register(Box::new(
                rush_rules::rules_config::ConfigNounRule { rule_type },
            ));
        }
        rule_registry.register(Box::new(rush_rules::rules_config::LabelFlagRule));
        rule_registry.register(Box::new(rush_rules::rules_system::SystemCcLibraryRule));

        // Register placeholder for Starlark-defined custom rules (rule() — see UNIMPLEMENTED.md)
        rule_registry.register(Box::new(crate::custom_rule_stub::CustomRuleStub));

        // Retain Java declarations as fail-loud configured targets.
        for java_rule in crate::rules_java::UnsupportedJavaRule::all() {
            rule_registry.register(Box::new(java_rule));
        }

        // Create external dependency manager
        // NOTE: Replaced by SystemPackageMapper (static methods, no instance needed)
        // let external_cache_dir = workspace_root.join(".cache").join("rush").join("external");
        // let external_dep_manager = external_deps::ExternalDepManager::new(external_cache_dir)?;

        // Create resource manager FIRST for system-wide memory monitoring
        // PSI monitoring starts immediately when ResourceManager is created
        let resource_manager =
            ResourceManager::new().expect("Failed to initialize resource manager");

        let system_mapper = Arc::new(SystemPackageMapper::new());
        let repository_manager = Arc::new(RepositoryManager::new(system_mapper));

        // Phase 2: Create global memory budget for .bzl loading
        // This provides atomic memory reservation to prevent race conditions
        let memory_budget = resource::GlobalMemoryBudget::with_defaults()
            .expect("Failed to initialize global memory budget");

        // Use singleton global .bzl cache (shared across ALL BuildOrchestrator instances)
        // This prevents creating multiple 2GB caches (OOM fix)
        let mut bzl_loader = rush_starlark::BzlLoader::new_with_all_and_budget(
            workspace_root.clone(),
            Some(resource_manager.clone()),
            Some(GLOBAL_BZL_CACHE.clone()),
            Some(memory_budget.clone()),
        );

        // Create memory guard with default configuration
        let memory_guard_config = memory_guard::MemoryGuardConfig::default();
        let memory_guard = memory_guard::MemoryGuard::new(memory_guard_config);

        // Create built-in repos before moving workspace_root
        let builtin_repos = create_builtin_repos(&workspace_root)
            .context("Failed to create built-in placeholder repositories")?;

        // Register builtin repos with bzl_loader so it can resolve external .bzl files
        // Convert HashMap to DashMap and wrap in Arc
        let builtin_repos_dashmap = Arc::new(dashmap::DashMap::new());
        for (name, path) in &builtin_repos {
            builtin_repos_dashmap.insert(name.clone(), path.clone());
        }
        bzl_loader.set_external_repos(builtin_repos_dashmap);
        for (name, path) in &builtin_repos {
            bzl_loader.add_implicit_system_adapter(name.clone(), path.clone());
        }

        // Phase 5.C.4 commit 7: hold a clone for RushActionManager
        // before resource_manager moves into the struct literal.
        let action_manager_resource_manager = Arc::clone(&resource_manager);
        let orch = BuildOrchestrator {
            workspace_root,
            graph: Arc::new(EvaluationGraph::new()),
            cache: Arc::new(ActionCache::new()),
            output_tree: Arc::new(output_tree),
            progress: Arc::new(BuildProgress::new()),
            profiler: Arc::new(BuildProfiler::new(Self::build_trace_path().is_some())),
            events: Arc::new(BuildEventStream::new(false)),
            metrics: Arc::new(BuildMetrics::new()),
            change_detector: Arc::new(tokio::sync::Mutex::new(change_detector)),
            compdb: Arc::new(tokio::sync::Mutex::new(compdb)),
            streaming_enabled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            worker_pool: Arc::new(WorkerPool::new(4)),
            incremental_linker: Arc::new(Mutex::new(IncrementalLinker::new())),
            remote_executor: None,
            test_cache: Arc::new(Mutex::new(test_cache)),
            aspects: Arc::new(Mutex::new(AspectManager::new())),
            build_file_cache: Arc::new(Mutex::new(Self::create_build_file_cache())),
            rule_registry: Arc::new(rule_registry),
            resource_manager,
            repository_manager,
            global_bzl_cache: GLOBAL_BZL_CACHE.clone(),
            // external_dep_manager: Arc::new(Mutex::new(external_dep_manager)),
            external_repo_paths: Arc::new(tokio::sync::Mutex::new(builtin_repos)),
            bzl_loader: Arc::new(Mutex::new(bzl_loader)),
            dependency_cache: Arc::new(Mutex::new(Self::create_dependency_cache())),

            memory_guard: Arc::new(memory_guard),
            action_manager: Arc::new(RushActionManager::new(
                Arc::new(ActionCache::new()),
                Arc::new(LocalStrategy),
                action_manager_resource_manager,
            )),
            tracked_io: Arc::new(std::sync::Mutex::new(None)),
            feature_flag_command: Arc::new(std::sync::Mutex::new(None)),
            snapshot_cache: Arc::new(crate::analysis_snapshot::SnapshotCache::default()),
            reuse_counters: Arc::new(crate::analysis_snapshot::ReuseCounters::new()),
            watcher: Arc::new(crate::workspace_watcher::WorkspaceWatcher::unarmed()),
        };

        // Phase 3: Start adaptive memory pressure response task
        // Subscribe to memory budget pressure changes and trigger cache eviction
        let mut pressure_rx = memory_budget.subscribe_pressure();
        let bzl_loader_clone = orch.bzl_loader.clone();
        let global_cache_clone = GLOBAL_BZL_CACHE.clone();

        tokio::spawn(async move {
            while pressure_rx.changed().await.is_ok() {
                let pressure = *pressure_rx.borrow();

                match pressure {
                    resource::MemoryPressure::High => {
                        // Evict 25% of symbol cache to free memory
                        if let Ok(mut loader) = bzl_loader_clone.lock() {
                            loader.evict_lru_symbols(0.25);
                        }
                        // Also evict from global cache
                        global_cache_clone.evict_symbols(0.25);
                    }
                    resource::MemoryPressure::Critical => {
                        // Aggressively clear 50% of program cache and 50% of symbols
                        if let Ok(mut loader) = bzl_loader_clone.lock() {
                            loader.clear_program_cache(0.50);
                            loader.evict_lru_symbols(0.50);
                        }
                        // Also evict from global cache
                        global_cache_clone.evict_symbols(0.50);
                    }
                    _ => {
                        // Low/Medium pressure - no action needed
                    }
                }
            }
        });

        Ok(orch)
    }

    /// Create orchestrator with explicit workspace root and build config.
    ///
    /// Panics if setup fails. Prefer [`try_with_config`](Self::try_with_config)
    /// on any path that reaches a user: setup failures here are actionable
    /// configuration problems (a missing declared repository, a protobuf
    /// version skew), and a panic buries the diagnosis under a backtrace note.
    pub fn with_config(workspace_root: PathBuf, config: BuildConfig) -> Self {
        Self::try_with_config(workspace_root, config)
            .expect("Failed to construct BuildOrchestrator")
    }

    /// Fallible form of [`with_config`](Self::with_config).
    pub fn try_with_config(workspace_root: PathBuf, config: BuildConfig) -> Result<Self> {
        let output_tree = OutputTree::new(workspace_root.clone(), config);
        let change_detector = ChangeDetector::new(&workspace_root);
        let compdb = CompilationDatabase::new(workspace_root.clone());
        let test_cache_dir = workspace_root.join(".rush-cache").join("test-results");
        let test_cache = TestCache::new(test_cache_dir);

        // Create rule registry and register TensorFlow rules
        let mut rule_registry = RuleRegistry::new();
        tensorflow_rules::register_tensorflow_rules(&mut rule_registry);

        // Register standard C++ build rules
        rule_registry.register(Box::new(rush_rules::rules_cc::CcLibraryRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::CcBinaryRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::CcTestRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::FilegroupRule));
        // Phase 5.C.4 commit 8: rush_rules::rules_cc::TfCcLibraryRule
        // is an alias that delegates to CcLibraryRule. The real
        // TF-shape impl with TF-specific argv (and the
        // build_legacy_command_specs the verifier needs) lives in
        // tensorflow-rules and was already registered above via
        // register_tensorflow_rules(). Registering the alias here
        // would overwrite the real one in the HashMap (last wins),
        // so we skip it. Same reasoning for TfCcTestRule.
        rule_registry.register(Box::new(rush_rules::rules_cc::TfCudaLibraryRule));

        // Register protocol buffer rules
        rule_registry.register(Box::new(rush_rules::rules_cc::ProtoLibraryRule));
        rule_registry.register(Box::new(rush_rules::rules_proto_toolchain::ProtoLangToolchainRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::CcProtoLibraryRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::CcSharedLibraryRule));

        // Register Python rules (graph-only tracking; no .py compilation)
        rule_registry.register(Box::new(rush_rules::rules_python::PyLibraryRule));
        rule_registry.register(Box::new(rush_rules::rules_python::PyBinaryRule));
        rule_registry.register(Box::new(rush_rules::rules_python::PyTestRule));
        rule_registry.register(Box::new(rush_rules::rules_sh::ShBinaryRule));
        rule_registry.register(Box::new(rush_rules::rules_sh::ShTestRule));

        // Register test utility rules
        rule_registry.register(Box::new(rush_rules::rules_cc::TestSuiteRule));

        // Register Bazel utility rules
        rule_registry.register(Box::new(rush_rules::rules_cc::PackageGroupRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::ExportsFilesRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::SourceFileRule));
        rule_registry.register(Box::new(rush_rules::rules_config::ConfigSettingRule));
        for rule_type in [
            "platform",
            "toolchain",
            "toolchain_type",
            "constraint_setting",
            "constraint_value",
        ] {
            rule_registry.register(Box::new(
                rush_rules::rules_config::ConfigNounRule { rule_type },
            ));
        }
        rule_registry.register(Box::new(rush_rules::rules_config::LabelFlagRule));
        rule_registry.register(Box::new(rush_rules::rules_system::SystemCcLibraryRule));

        // Register placeholder for Starlark-defined custom rules (rule() — see UNIMPLEMENTED.md)
        rule_registry.register(Box::new(crate::custom_rule_stub::CustomRuleStub));

        // Retain Java declarations as fail-loud configured targets.
        for java_rule in crate::rules_java::UnsupportedJavaRule::all() {
            rule_registry.register(Box::new(java_rule));
        }

        // Create external dependency manager
        // NOTE: Replaced by SystemPackageMapper (static methods, no instance needed)
        // let external_cache_dir = workspace_root.join(".cache").join("rush").join("external");
        // let external_dep_manager = external_deps::ExternalDepManager::new(external_cache_dir)
        //     .unwrap_or_else(|_| panic!("Failed to create external dependency manager"));

        // Create resource manager FIRST for system-wide memory monitoring
        let resource_manager =
            ResourceManager::new().expect("Failed to initialize resource manager");

        let system_mapper = Arc::new(SystemPackageMapper::new());
        let repository_manager = Arc::new(RepositoryManager::new(system_mapper));

        // Phase 2: Create global memory budget for .bzl loading
        let memory_budget = resource::GlobalMemoryBudget::with_defaults()
            .expect("Failed to initialize global memory budget");

        // Use singleton global .bzl cache (shared across ALL BuildOrchestrator instances)
        // This prevents creating multiple 2GB caches (OOM fix)
        let mut bzl_loader = rush_starlark::BzlLoader::new_with_all_and_budget(
            workspace_root.clone(),
            Some(resource_manager.clone()),
            Some(GLOBAL_BZL_CACHE.clone()),
            Some(memory_budget.clone()),
        );

        // Create memory guard with default configuration
        let memory_guard_config = memory_guard::MemoryGuardConfig::default();
        let memory_guard = memory_guard::MemoryGuard::new(memory_guard_config);

        // Create built-in repos before moving workspace_root
        let builtin_repos = create_builtin_repos(&workspace_root)
            .context("Failed to create built-in placeholder repositories")?;

        // Register builtin repos with bzl_loader so external .bzl loads resolve
        // even when WORKSPACE evaluation fails early. Without this, every entry
        // that SystemPackageMapper::get_known_repository_paths writes under
        // .cache/rush/external/<repo>/ (grpc, local_config_cuda, pybind11, …)
        // is invisible to the loader's external_repos DashMap. The parallel
        // registration inside load_workspace_internal_impl only runs if the
        // complete WORKSPACE evaluation succeeds. TensorFlow can still stop
        // that evaluation at an unsupported repository macro, before reaching
        // the registration bridge.
        let builtin_repos_dashmap = Arc::new(dashmap::DashMap::new());
        for (name, path) in &builtin_repos {
            builtin_repos_dashmap.insert(name.clone(), path.clone());
        }
        bzl_loader.set_external_repos(builtin_repos_dashmap);
        for (name, path) in &builtin_repos {
            bzl_loader.add_implicit_system_adapter(name.clone(), path.clone());
        }

        let cache = Arc::new(ActionCache::new());

        // Phase 5.C.4 commit 7: clone resource_manager for the
        // ActionManager before the struct literal moves it.
        let action_manager_resource_manager = Arc::clone(&resource_manager);
        Ok(BuildOrchestrator {
            workspace_root,
            graph: Arc::new(EvaluationGraph::new()),
            cache: cache.clone(),
            output_tree: Arc::new(output_tree),
            progress: Arc::new(BuildProgress::new()),
            profiler: Arc::new(BuildProfiler::new(Self::build_trace_path().is_some())),
            events: Arc::new(BuildEventStream::new(false)),
            metrics: Arc::new(BuildMetrics::new()),
            change_detector: Arc::new(tokio::sync::Mutex::new(change_detector)),
            compdb: Arc::new(tokio::sync::Mutex::new(compdb)),
            streaming_enabled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            worker_pool: Arc::new(WorkerPool::new(4)),
            incremental_linker: Arc::new(Mutex::new(IncrementalLinker::new())),
            remote_executor: None,
            test_cache: Arc::new(Mutex::new(test_cache)),
            aspects: Arc::new(Mutex::new(AspectManager::new())),
            build_file_cache: Arc::new(Mutex::new(Self::create_build_file_cache())),
            rule_registry: Arc::new(rule_registry),
            resource_manager,
            repository_manager,
            global_bzl_cache: GLOBAL_BZL_CACHE.clone(),
            // external_dep_manager: Arc::new(Mutex::new(external_dep_manager)),
            external_repo_paths: Arc::new(tokio::sync::Mutex::new(builtin_repos)),
            bzl_loader: Arc::new(Mutex::new(bzl_loader)),
            dependency_cache: Arc::new(Mutex::new(Self::create_dependency_cache())),
            memory_guard: Arc::new(memory_guard),
            action_manager: Arc::new(RushActionManager::new(
                cache,
                Arc::new(LocalStrategy),
                action_manager_resource_manager,
            )),
            tracked_io: Arc::new(std::sync::Mutex::new(None)),
            feature_flag_command: Arc::new(std::sync::Mutex::new(None)),
            snapshot_cache: Arc::new(crate::analysis_snapshot::SnapshotCache::default()),
            reuse_counters: Arc::new(crate::analysis_snapshot::ReuseCounters::new()),
            watcher: Arc::new(crate::workspace_watcher::WorkspaceWatcher::unarmed()),
        })
    }

    /// Create orchestrator with explicit workspace root (uses fastbuild by default).
    pub fn with_workspace(workspace_root: PathBuf) -> Self {
        Self::with_config(workspace_root, BuildConfig::Fastbuild)
    }

    /// Create orchestrator with disk-backed cache and build config.
    pub fn with_disk_cache_and_config(
        workspace_root: PathBuf,
        cache_config: CacheConfig,
        build_config: BuildConfig,
    ) -> Result<Self> {
        let cache = Arc::new(ActionCache::with_disk(cache_config)?);
        let output_tree = OutputTree::new(workspace_root.clone(), build_config);
        let change_detector = ChangeDetector::new(&workspace_root);
        let compdb = CompilationDatabase::new(workspace_root.clone());
        let test_cache_dir = workspace_root.join(".rush-cache").join("test-results");
        let test_cache = TestCache::new(test_cache_dir);

        // Create rule registry and register TensorFlow rules
        let mut rule_registry = RuleRegistry::new();
        tensorflow_rules::register_tensorflow_rules(&mut rule_registry);

        // Register standard C++ build rules
        rule_registry.register(Box::new(rush_rules::rules_cc::CcLibraryRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::CcBinaryRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::CcTestRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::FilegroupRule));
        // Phase 5.C.4 commit 8: rush_rules::rules_cc::TfCcLibraryRule
        // is an alias that delegates to CcLibraryRule. The real
        // TF-shape impl with TF-specific argv (and the
        // build_legacy_command_specs the verifier needs) lives in
        // tensorflow-rules and was already registered above via
        // register_tensorflow_rules(). Registering the alias here
        // would overwrite the real one in the HashMap (last wins),
        // so we skip it. Same reasoning for TfCcTestRule.
        rule_registry.register(Box::new(rush_rules::rules_cc::TfCudaLibraryRule));

        // Register protocol buffer rules
        rule_registry.register(Box::new(rush_rules::rules_cc::ProtoLibraryRule));
        rule_registry.register(Box::new(rush_rules::rules_proto_toolchain::ProtoLangToolchainRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::CcProtoLibraryRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::CcSharedLibraryRule));

        // Register Python rules (graph-only tracking; no .py compilation)
        rule_registry.register(Box::new(rush_rules::rules_python::PyLibraryRule));
        rule_registry.register(Box::new(rush_rules::rules_python::PyBinaryRule));
        rule_registry.register(Box::new(rush_rules::rules_python::PyTestRule));
        rule_registry.register(Box::new(rush_rules::rules_sh::ShBinaryRule));
        rule_registry.register(Box::new(rush_rules::rules_sh::ShTestRule));

        // Register test utility rules
        rule_registry.register(Box::new(rush_rules::rules_cc::TestSuiteRule));

        // Register Bazel utility rules
        rule_registry.register(Box::new(rush_rules::rules_cc::PackageGroupRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::ExportsFilesRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::SourceFileRule));
        rule_registry.register(Box::new(rush_rules::rules_config::ConfigSettingRule));
        for rule_type in [
            "platform",
            "toolchain",
            "toolchain_type",
            "constraint_setting",
            "constraint_value",
        ] {
            rule_registry.register(Box::new(
                rush_rules::rules_config::ConfigNounRule { rule_type },
            ));
        }
        rule_registry.register(Box::new(rush_rules::rules_config::LabelFlagRule));
        rule_registry.register(Box::new(rush_rules::rules_system::SystemCcLibraryRule));

        // Register placeholder for Starlark-defined custom rules (rule() — see UNIMPLEMENTED.md)
        rule_registry.register(Box::new(crate::custom_rule_stub::CustomRuleStub));

        // Retain Java declarations as fail-loud configured targets.
        for java_rule in crate::rules_java::UnsupportedJavaRule::all() {
            rule_registry.register(Box::new(java_rule));
        }

        // Create external dependency manager
        // NOTE: Replaced by SystemPackageMapper (static methods, no instance needed)
        // let external_cache_dir = workspace_root.join(".cache").join("rush").join("external");
        // let external_dep_manager = external_deps::ExternalDepManager::new(external_cache_dir)?;

        // Create resource manager FIRST for system-wide memory monitoring
        let resource_manager =
            ResourceManager::new().expect("Failed to initialize resource manager");

        let system_mapper = Arc::new(SystemPackageMapper::new());
        let repository_manager = Arc::new(RepositoryManager::new(system_mapper));

        // Phase 2: Create global memory budget for .bzl loading
        let memory_budget = resource::GlobalMemoryBudget::with_defaults()
            .expect("Failed to initialize global memory budget");

        // Use singleton global .bzl cache (shared across ALL BuildOrchestrator instances)
        // This prevents creating multiple 2GB caches (OOM fix)
        let mut bzl_loader = rush_starlark::BzlLoader::new_with_all_and_budget(
            workspace_root.clone(),
            Some(resource_manager.clone()),
            Some(GLOBAL_BZL_CACHE.clone()),
            Some(memory_budget.clone()),
        );

        // Create memory guard with default configuration
        let memory_guard_config = memory_guard::MemoryGuardConfig::default();
        let memory_guard = memory_guard::MemoryGuard::new(memory_guard_config);

        // Create built-in repos before moving workspace_root
        let builtin_repos = create_builtin_repos(&workspace_root)
            .context("Failed to create built-in placeholder repositories")?;

        // Register builtin repos with bzl_loader so external .bzl loads resolve
        // even when WORKSPACE evaluation fails early. Without this, every entry
        // that SystemPackageMapper::get_known_repository_paths writes under
        // .cache/rush/external/<repo>/ (grpc, local_config_cuda, pybind11, …)
        // is invisible to the loader's external_repos DashMap. The parallel
        // registration inside load_workspace_internal_impl only runs if the
        // complete WORKSPACE evaluation succeeds. TensorFlow can still stop
        // that evaluation at an unsupported repository macro, before reaching
        // the registration bridge.
        let builtin_repos_dashmap = Arc::new(dashmap::DashMap::new());
        for (name, path) in &builtin_repos {
            builtin_repos_dashmap.insert(name.clone(), path.clone());
        }
        bzl_loader.set_external_repos(builtin_repos_dashmap);
        for (name, path) in &builtin_repos {
            bzl_loader.add_implicit_system_adapter(name.clone(), path.clone());
        }

        // Phase 5.C.4 commit 7: clone resource_manager for ActionManager.
        let action_manager_resource_manager = Arc::clone(&resource_manager);
        Ok(BuildOrchestrator {
            workspace_root,
            graph: Arc::new(EvaluationGraph::new()),
            cache: cache.clone(),
            output_tree: Arc::new(output_tree),
            progress: Arc::new(BuildProgress::new()),
            profiler: Arc::new(BuildProfiler::new(Self::build_trace_path().is_some())),
            events: Arc::new(BuildEventStream::new(false)),
            metrics: Arc::new(BuildMetrics::new()),
            change_detector: Arc::new(tokio::sync::Mutex::new(change_detector)),
            compdb: Arc::new(tokio::sync::Mutex::new(compdb)),
            streaming_enabled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            worker_pool: Arc::new(WorkerPool::new(4)),
            incremental_linker: Arc::new(Mutex::new(IncrementalLinker::new())),
            remote_executor: None,
            test_cache: Arc::new(Mutex::new(test_cache)),
            aspects: Arc::new(Mutex::new(AspectManager::new())),
            build_file_cache: Arc::new(Mutex::new(Self::create_build_file_cache())),
            rule_registry: Arc::new(rule_registry),
            resource_manager,
            repository_manager,
            global_bzl_cache: GLOBAL_BZL_CACHE.clone(),
            // external_dep_manager: Arc::new(Mutex::new(external_dep_manager)),
            external_repo_paths: Arc::new(tokio::sync::Mutex::new(builtin_repos)),
            bzl_loader: Arc::new(Mutex::new(bzl_loader)),
            dependency_cache: Arc::new(Mutex::new(Self::create_dependency_cache())),
            memory_guard: Arc::new(memory_guard),
            action_manager: Arc::new(RushActionManager::new(
                cache,
                Arc::new(LocalStrategy),
                action_manager_resource_manager,
            )),
            tracked_io: Arc::new(std::sync::Mutex::new(None)),
            feature_flag_command: Arc::new(std::sync::Mutex::new(None)),
            snapshot_cache: Arc::new(crate::analysis_snapshot::SnapshotCache::default()),
            reuse_counters: Arc::new(crate::analysis_snapshot::ReuseCounters::new()),
            watcher: Arc::new(crate::workspace_watcher::WorkspaceWatcher::unarmed()),
        })
    }

    /// Create orchestrator with disk-backed cache (uses fastbuild by default).
    pub fn with_disk_cache(workspace_root: PathBuf, cache_config: CacheConfig) -> Result<Self> {
        Self::with_disk_cache_and_config(workspace_root, cache_config, BuildConfig::Fastbuild)
    }

    /// Create orchestrator with disk and remote HTTP cache.
    pub fn with_disk_and_remote_cache(
        workspace_root: PathBuf,
        cache_config: CacheConfig,
        remote_url: String,
    ) -> Result<Self> {
        let cache = Arc::new(ActionCache::with_disk_and_remote(cache_config, remote_url)?);
        let output_tree = OutputTree::new(workspace_root.clone(), BuildConfig::Fastbuild);
        let change_detector = ChangeDetector::new(&workspace_root);
        let compdb = CompilationDatabase::new(workspace_root.clone());
        let test_cache_dir = workspace_root.join(".rush-cache").join("test-results");
        let test_cache = TestCache::new(test_cache_dir);

        // Create rule registry and register TensorFlow rules
        let mut rule_registry = RuleRegistry::new();
        tensorflow_rules::register_tensorflow_rules(&mut rule_registry);

        // Register standard C++ build rules
        rule_registry.register(Box::new(rush_rules::rules_cc::CcLibraryRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::CcBinaryRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::CcTestRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::FilegroupRule));
        // Phase 5.C.4 commit 8: rush_rules::rules_cc::TfCcLibraryRule
        // is an alias that delegates to CcLibraryRule. The real
        // TF-shape impl with TF-specific argv (and the
        // build_legacy_command_specs the verifier needs) lives in
        // tensorflow-rules and was already registered above via
        // register_tensorflow_rules(). Registering the alias here
        // would overwrite the real one in the HashMap (last wins),
        // so we skip it. Same reasoning for TfCcTestRule.
        rule_registry.register(Box::new(rush_rules::rules_cc::TfCudaLibraryRule));

        // Register protocol buffer rules
        rule_registry.register(Box::new(rush_rules::rules_cc::ProtoLibraryRule));
        rule_registry.register(Box::new(rush_rules::rules_proto_toolchain::ProtoLangToolchainRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::CcProtoLibraryRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::CcSharedLibraryRule));

        // Register Python rules (graph-only tracking; no .py compilation)
        rule_registry.register(Box::new(rush_rules::rules_python::PyLibraryRule));
        rule_registry.register(Box::new(rush_rules::rules_python::PyBinaryRule));
        rule_registry.register(Box::new(rush_rules::rules_python::PyTestRule));
        rule_registry.register(Box::new(rush_rules::rules_sh::ShBinaryRule));
        rule_registry.register(Box::new(rush_rules::rules_sh::ShTestRule));

        // Register test utility rules
        rule_registry.register(Box::new(rush_rules::rules_cc::TestSuiteRule));

        // Register Bazel utility rules
        rule_registry.register(Box::new(rush_rules::rules_cc::PackageGroupRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::ExportsFilesRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::SourceFileRule));
        rule_registry.register(Box::new(rush_rules::rules_config::ConfigSettingRule));
        for rule_type in [
            "platform",
            "toolchain",
            "toolchain_type",
            "constraint_setting",
            "constraint_value",
        ] {
            rule_registry.register(Box::new(
                rush_rules::rules_config::ConfigNounRule { rule_type },
            ));
        }
        rule_registry.register(Box::new(rush_rules::rules_config::LabelFlagRule));
        rule_registry.register(Box::new(rush_rules::rules_system::SystemCcLibraryRule));

        // Register placeholder for Starlark-defined custom rules (rule() — see UNIMPLEMENTED.md)
        rule_registry.register(Box::new(crate::custom_rule_stub::CustomRuleStub));

        // Retain Java declarations as fail-loud configured targets.
        for java_rule in crate::rules_java::UnsupportedJavaRule::all() {
            rule_registry.register(Box::new(java_rule));
        }

        // Create external dependency manager
        // NOTE: Replaced by SystemPackageMapper (static methods, no instance needed)
        // let external_cache_dir = workspace_root.join(".cache").join("rush").join("external");
        // let external_dep_manager = external_deps::ExternalDepManager::new(external_cache_dir)?;

        // Create resource manager FIRST for system-wide memory monitoring
        let resource_manager =
            ResourceManager::new().expect("Failed to initialize resource manager");

        let system_mapper = Arc::new(SystemPackageMapper::new());
        let repository_manager = Arc::new(RepositoryManager::new(system_mapper));

        // Phase 2: Create global memory budget for .bzl loading
        let memory_budget = resource::GlobalMemoryBudget::with_defaults()
            .expect("Failed to initialize global memory budget");

        // Use singleton global .bzl cache (shared across ALL BuildOrchestrator instances)
        // This prevents creating multiple 2GB caches (OOM fix)
        let mut bzl_loader = rush_starlark::BzlLoader::new_with_all_and_budget(
            workspace_root.clone(),
            Some(resource_manager.clone()),
            Some(GLOBAL_BZL_CACHE.clone()),
            Some(memory_budget.clone()),
        );

        // Create memory guard with default configuration
        let memory_guard_config = memory_guard::MemoryGuardConfig::default();
        let memory_guard = memory_guard::MemoryGuard::new(memory_guard_config);

        // Create built-in repos before moving workspace_root
        let builtin_repos = create_builtin_repos(&workspace_root)
            .context("Failed to create built-in placeholder repositories")?;

        // Register builtin repos with bzl_loader so external .bzl loads resolve
        // even when WORKSPACE evaluation fails early. Without this, every entry
        // that SystemPackageMapper::get_known_repository_paths writes under
        // .cache/rush/external/<repo>/ (grpc, local_config_cuda, pybind11, …)
        // is invisible to the loader's external_repos DashMap. The parallel
        // registration inside load_workspace_internal_impl only runs if the
        // complete WORKSPACE evaluation succeeds. TensorFlow can still stop
        // that evaluation at an unsupported repository macro, before reaching
        // the registration bridge.
        let builtin_repos_dashmap = Arc::new(dashmap::DashMap::new());
        for (name, path) in &builtin_repos {
            builtin_repos_dashmap.insert(name.clone(), path.clone());
        }
        bzl_loader.set_external_repos(builtin_repos_dashmap);
        for (name, path) in &builtin_repos {
            bzl_loader.add_implicit_system_adapter(name.clone(), path.clone());
        }

        // Phase 5.C.4 commit 7: clone resource_manager for ActionManager.
        let action_manager_resource_manager = Arc::clone(&resource_manager);
        Ok(BuildOrchestrator {
            workspace_root,
            graph: Arc::new(EvaluationGraph::new()),
            cache: cache.clone(),
            output_tree: Arc::new(output_tree),
            progress: Arc::new(BuildProgress::new()),
            profiler: Arc::new(BuildProfiler::new(Self::build_trace_path().is_some())),
            events: Arc::new(BuildEventStream::new(false)),
            metrics: Arc::new(BuildMetrics::new()),
            change_detector: Arc::new(tokio::sync::Mutex::new(change_detector)),
            compdb: Arc::new(tokio::sync::Mutex::new(compdb)),
            streaming_enabled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            worker_pool: Arc::new(WorkerPool::new(4)),
            incremental_linker: Arc::new(Mutex::new(IncrementalLinker::new())),
            remote_executor: None,
            test_cache: Arc::new(Mutex::new(test_cache)),
            aspects: Arc::new(Mutex::new(AspectManager::new())),
            build_file_cache: Arc::new(Mutex::new(Self::create_build_file_cache())),
            rule_registry: Arc::new(rule_registry),
            resource_manager,
            repository_manager,
            global_bzl_cache: GLOBAL_BZL_CACHE.clone(),
            // external_dep_manager: Arc::new(Mutex::new(external_dep_manager)),
            external_repo_paths: Arc::new(tokio::sync::Mutex::new(builtin_repos)),
            bzl_loader: Arc::new(Mutex::new(bzl_loader)),
            dependency_cache: Arc::new(Mutex::new(Self::create_dependency_cache())),
            memory_guard: Arc::new(memory_guard),
            action_manager: Arc::new(RushActionManager::new(
                cache,
                Arc::new(LocalStrategy),
                action_manager_resource_manager,
            )),
            tracked_io: Arc::new(std::sync::Mutex::new(None)),
            feature_flag_command: Arc::new(std::sync::Mutex::new(None)),
            snapshot_cache: Arc::new(crate::analysis_snapshot::SnapshotCache::default()),
            reuse_counters: Arc::new(crate::analysis_snapshot::ReuseCounters::new()),
            watcher: Arc::new(crate::workspace_watcher::WorkspaceWatcher::unarmed()),
        })
    }

    /// Create orchestrator with remote HTTP cache only (no disk).
    pub fn with_remote_cache(workspace_root: PathBuf, remote_url: String) -> Result<Self> {
        let cache = Arc::new(ActionCache::with_remote(remote_url)?);
        let output_tree = OutputTree::new(workspace_root.clone(), BuildConfig::Fastbuild);
        let change_detector = ChangeDetector::new(&workspace_root);
        let compdb = CompilationDatabase::new(workspace_root.clone());
        let test_cache_dir = workspace_root.join(".rush-cache").join("test-results");
        let test_cache = TestCache::new(test_cache_dir);

        // Create rule registry and register TensorFlow rules
        let mut rule_registry = RuleRegistry::new();
        tensorflow_rules::register_tensorflow_rules(&mut rule_registry);

        // Register standard C++ build rules
        rule_registry.register(Box::new(rush_rules::rules_cc::CcLibraryRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::CcBinaryRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::CcTestRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::FilegroupRule));
        // Phase 5.C.4 commit 8: rush_rules::rules_cc::TfCcLibraryRule
        // is an alias that delegates to CcLibraryRule. The real
        // TF-shape impl with TF-specific argv (and the
        // build_legacy_command_specs the verifier needs) lives in
        // tensorflow-rules and was already registered above via
        // register_tensorflow_rules(). Registering the alias here
        // would overwrite the real one in the HashMap (last wins),
        // so we skip it. Same reasoning for TfCcTestRule.
        rule_registry.register(Box::new(rush_rules::rules_cc::TfCudaLibraryRule));

        // Register protocol buffer rules
        rule_registry.register(Box::new(rush_rules::rules_cc::ProtoLibraryRule));
        rule_registry.register(Box::new(rush_rules::rules_proto_toolchain::ProtoLangToolchainRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::CcProtoLibraryRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::CcSharedLibraryRule));

        // Register Python rules (graph-only tracking; no .py compilation)
        rule_registry.register(Box::new(rush_rules::rules_python::PyLibraryRule));
        rule_registry.register(Box::new(rush_rules::rules_python::PyBinaryRule));
        rule_registry.register(Box::new(rush_rules::rules_python::PyTestRule));
        rule_registry.register(Box::new(rush_rules::rules_sh::ShBinaryRule));
        rule_registry.register(Box::new(rush_rules::rules_sh::ShTestRule));

        // Register test utility rules
        rule_registry.register(Box::new(rush_rules::rules_cc::TestSuiteRule));

        // Register Bazel utility rules
        rule_registry.register(Box::new(rush_rules::rules_cc::PackageGroupRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::ExportsFilesRule));
        rule_registry.register(Box::new(rush_rules::rules_cc::SourceFileRule));
        rule_registry.register(Box::new(rush_rules::rules_config::ConfigSettingRule));
        for rule_type in [
            "platform",
            "toolchain",
            "toolchain_type",
            "constraint_setting",
            "constraint_value",
        ] {
            rule_registry.register(Box::new(
                rush_rules::rules_config::ConfigNounRule { rule_type },
            ));
        }
        rule_registry.register(Box::new(rush_rules::rules_config::LabelFlagRule));
        rule_registry.register(Box::new(rush_rules::rules_system::SystemCcLibraryRule));

        // Register placeholder for Starlark-defined custom rules (rule() — see UNIMPLEMENTED.md)
        rule_registry.register(Box::new(crate::custom_rule_stub::CustomRuleStub));

        // Retain Java declarations as fail-loud configured targets.
        for java_rule in crate::rules_java::UnsupportedJavaRule::all() {
            rule_registry.register(Box::new(java_rule));
        }

        // Create external dependency manager
        // NOTE: Replaced by SystemPackageMapper (static methods, no instance needed)
        // let external_cache_dir = workspace_root.join(".cache").join("rush").join("external");
        // let external_dep_manager = external_deps::ExternalDepManager::new(external_cache_dir)?;

        // Create resource manager FIRST for system-wide memory monitoring
        let resource_manager =
            ResourceManager::new().expect("Failed to initialize resource manager");

        let system_mapper = Arc::new(SystemPackageMapper::new());
        let repository_manager = Arc::new(RepositoryManager::new(system_mapper));

        // Phase 2: Create global memory budget for .bzl loading
        let memory_budget = resource::GlobalMemoryBudget::with_defaults()
            .expect("Failed to initialize global memory budget");

        // Use singleton global .bzl cache (shared across ALL BuildOrchestrator instances)
        // This prevents creating multiple 2GB caches (OOM fix)
        let mut bzl_loader = rush_starlark::BzlLoader::new_with_all_and_budget(
            workspace_root.clone(),
            Some(resource_manager.clone()),
            Some(GLOBAL_BZL_CACHE.clone()),
            Some(memory_budget.clone()),
        );

        // Create memory guard with default configuration
        let memory_guard_config = memory_guard::MemoryGuardConfig::default();
        let memory_guard = memory_guard::MemoryGuard::new(memory_guard_config);

        // Create built-in repos before moving workspace_root
        let builtin_repos = create_builtin_repos(&workspace_root)
            .context("Failed to create built-in placeholder repositories")?;

        // Register builtin repos with bzl_loader so external .bzl loads resolve
        // even when WORKSPACE evaluation fails early. Without this, every entry
        // that SystemPackageMapper::get_known_repository_paths writes under
        // .cache/rush/external/<repo>/ (grpc, local_config_cuda, pybind11, …)
        // is invisible to the loader's external_repos DashMap. The parallel
        // registration inside load_workspace_internal_impl only runs if the
        // complete WORKSPACE evaluation succeeds. TensorFlow can still stop
        // that evaluation at an unsupported repository macro, before reaching
        // the registration bridge.
        let builtin_repos_dashmap = Arc::new(dashmap::DashMap::new());
        for (name, path) in &builtin_repos {
            builtin_repos_dashmap.insert(name.clone(), path.clone());
        }
        bzl_loader.set_external_repos(builtin_repos_dashmap);
        for (name, path) in &builtin_repos {
            bzl_loader.add_implicit_system_adapter(name.clone(), path.clone());
        }

        // Phase 5.C.4 commit 7: clone resource_manager for ActionManager.
        let action_manager_resource_manager = Arc::clone(&resource_manager);
        Ok(BuildOrchestrator {
            workspace_root,
            graph: Arc::new(EvaluationGraph::new()),
            cache: cache.clone(),
            output_tree: Arc::new(output_tree),
            progress: Arc::new(BuildProgress::new()),
            profiler: Arc::new(BuildProfiler::new(Self::build_trace_path().is_some())),
            events: Arc::new(BuildEventStream::new(false)),
            metrics: Arc::new(BuildMetrics::new()),
            change_detector: Arc::new(tokio::sync::Mutex::new(change_detector)),
            compdb: Arc::new(tokio::sync::Mutex::new(compdb)),
            streaming_enabled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            worker_pool: Arc::new(WorkerPool::new(4)),
            incremental_linker: Arc::new(Mutex::new(IncrementalLinker::new())),
            remote_executor: None,
            test_cache: Arc::new(Mutex::new(test_cache)),
            aspects: Arc::new(Mutex::new(AspectManager::new())),
            build_file_cache: Arc::new(Mutex::new(Self::create_build_file_cache())),
            rule_registry: Arc::new(rule_registry),
            resource_manager,
            repository_manager,
            global_bzl_cache: GLOBAL_BZL_CACHE.clone(),
            // external_dep_manager: Arc::new(Mutex::new(external_dep_manager)),
            external_repo_paths: Arc::new(tokio::sync::Mutex::new(builtin_repos)),
            bzl_loader: Arc::new(Mutex::new(bzl_loader)),
            dependency_cache: Arc::new(Mutex::new(Self::create_dependency_cache())),
            memory_guard: Arc::new(memory_guard),
            action_manager: Arc::new(RushActionManager::new(
                cache,
                Arc::new(LocalStrategy),
                action_manager_resource_manager,
            )),
            tracked_io: Arc::new(std::sync::Mutex::new(None)),
            feature_flag_command: Arc::new(std::sync::Mutex::new(None)),
            snapshot_cache: Arc::new(crate::analysis_snapshot::SnapshotCache::default()),
            reuse_counters: Arc::new(crate::analysis_snapshot::ReuseCounters::new()),
            watcher: Arc::new(crate::workspace_watcher::WorkspaceWatcher::unarmed()),
        })
    }

    /// Get workspace root directory.
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// Enable progress bar for visual build feedback.
    pub fn enable_progress_bar(&self) {
        self.progress.enable_progress_bar();
    }

    /// Enable real-time output streaming during compilation.
    pub fn enable_streaming(&self) {
        self.streaming_enabled
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Get worker pool statistics.
    pub async fn worker_pool_stats(&self) -> std::collections::HashMap<String, usize> {
        self.worker_pool.stats().await
    }

    /// Shutdown all workers in the pool.
    pub async fn shutdown_worker_pool(&self) -> Result<()> {
        self.worker_pool.shutdown_all().await
    }

    /// Load WORKSPACE file and fetch external dependencies.
    ///
    /// This parses the WORKSPACE file at the workspace root, registers all
    /// external repositories, and optionally fetches them.
    ///
    /// # Clippy Suppression
    ///
    /// `await_holding_lock`: Holds std::sync::Mutex guard on external_dep_manager across
    /// await points during dependency fetching. Proper fix requires migrating to
    /// tokio::sync::Mutex, which is an architectural change affecting the dependency
    /// management subsystem. Lock is held intentionally short and dropped explicitly.
    #[allow(clippy::await_holding_lock)]
    pub async fn load_workspace(&self, fetch_dependencies: bool) -> Result<()> {
        let workspace_file = self.workspace_root.join("WORKSPACE");

        // Also check for WORKSPACE.bazel
        let workspace_file = if !workspace_file.exists() {
            let alt = self.workspace_root.join("WORKSPACE.bazel");
            if alt.exists() {
                alt
            } else {
                // No WORKSPACE file - skip external dependencies
                return Ok(());
            }
        } else {
            workspace_file
        };

        // Workspace loading performs synchronous tracked configuration probes
        // (notably local_config_python) between await points. Alternate callers
        // may enter here without build_with_dependencies having installed the
        // command IO, so establish the same process-semantics fallback used by
        // target expansion. A daemon command's existing client semantics win.
        self.ensure_tracked_io();

        // Configure Python before evaluating WORKSPACE. TensorFlow's WORKSPACE
        // evaluation is intentionally best-effort and may return early on a
        // macro Rush cannot yet evaluate; Python header materialization and
        // repository registration must not disappear with that unrelated
        // failure.
        let external_dir = cache_paths::workspace_external_dir(&self.workspace_root);
        std::fs::create_dir_all(&external_dir).with_context(|| {
            format!(
                "failed to create external repository directory {}",
                external_dir.display()
            )
        })?;
        let registered_python = self.external_repo_paths.lock().await.get("local_config_python").cloned();
        if !self.has_registered_source_repository_for_path("local_config_python", registered_python.as_deref()) {
            let python_repo_path = external_dir.join("local_config_python");
            self.with_tracked_scope(|| {
                python_config::materialize_local_config_python(
                    &self.workspace_root,
                    &python_repo_path,
                )
            })
            .context("failed to configure @local_config_python")?;
            self.external_repo_paths.lock().await
                .insert("local_config_python".to_string(), python_repo_path.clone());
            if let Ok(mut loader) = self.bzl_loader.lock() {
                // This is an automatic native configuration, not an explicit
                // WORKSPACE source. A real declaration must still own the name.
                loader.add_implicit_system_adapter("local_config_python".to_string(), python_repo_path);
            }
        }

        eprintln!("📋 Loading WORKSPACE from: {}", workspace_file.display());

        // Track visited WORKSPACE files to prevent infinite recursion
        let mut visited_workspaces = std::collections::HashSet::new();
        visited_workspaces.insert(workspace_file.clone());

        // Start recursive loading with depth tracking
        self.load_workspace_internal(
            &workspace_file,
            fetch_dependencies,
            0,
            &mut visited_workspaces,
        )
        .await
    }

    /// Internal recursive WORKSPACE loading with safeguards.
    ///
    /// # Arguments
    /// * `workspace_path` - Path to WORKSPACE file to load
    /// * `fetch_dependencies` - Whether to fetch http_archive dependencies
    /// * `depth` - Current recursion depth (for limiting nested WORKSPACE files)
    /// * `visited` - Set of already-processed WORKSPACE files (circular detection)
    fn load_workspace_internal<'a>(
        &'a self,
        workspace_path: &'a std::path::Path,
        fetch_dependencies: bool,
        depth: usize,
        visited: &'a mut std::collections::HashSet<std::path::PathBuf>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + 'a>> {
        Box::pin(async move {
            self.load_workspace_internal_impl(workspace_path, fetch_dependencies, depth, visited)
                .await
        })
    }

    /// Implementation of recursive WORKSPACE loading (boxed for async recursion).
    ///
    /// # Clippy Suppression
    ///
    /// `await_holding_lock`: This function holds std::sync::Mutex guards across await points
    /// when fetching external repositories during WORKSPACE file evaluation. The proper fix is
    /// to migrate to tokio::sync::Mutex, but that requires architectural changes to the
    /// external_dep_manager and starlark evaluator integration. The locks are explicitly
    /// dropped before awaits where possible, and critical sections are kept short.
    #[allow(clippy::await_holding_lock)]
    async fn load_workspace_internal_impl(
        &self,
        workspace_path: &std::path::Path,
        fetch_dependencies: bool,
        depth: usize,
        visited: &mut std::collections::HashSet<std::path::PathBuf>,
    ) -> Result<()> {
        const MAX_WORKSPACE_DEPTH: usize = 10;

        // Safeguard 1: Depth limit to prevent runaway recursion
        if depth > MAX_WORKSPACE_DEPTH {
            anyhow::bail!(
                "WORKSPACE nesting too deep (depth {}). Possible infinite recursion?\nFile: {}",
                depth,
                workspace_path.display()
            );
        }

        // Safeguard 2: Timeout on file metadata access (NFS stale handles)
        let metadata_result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::fs::metadata(workspace_path),
        )
        .await;

        match metadata_result {
            Ok(Ok(_)) => {} // File exists and is accessible
            Ok(Err(e)) => {
                debug_log!(
                    "Warning: Cannot access WORKSPACE file {}: {}",
                    workspace_path.display(),
                    e
                );
                return Ok(()); // Don't fail entire build
            }
            Err(_) => {
                debug_log!(
                    "Warning: Timeout accessing WORKSPACE file {} (NFS stale handle?)",
                    workspace_path.display()
                );
                return Ok(()); // Don't fail entire build
            }
        }

        if depth > 0 {
            eprintln!(
                "   ↪ Nested WORKSPACE (depth {}): {}",
                depth,
                workspace_path.display()
            );
        }

        // Determine workspace root for this WORKSPACE file (for resolving relative paths)
        let current_workspace_root = workspace_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("WORKSPACE file has no parent directory"))?;

        // Use WorkspaceLoader to evaluate WORKSPACE file via Starlark
        let loader = WorkspaceLoader::new(
            current_workspace_root.to_path_buf(),
            self.repository_manager.clone(),
            self.global_bzl_cache.clone(),
        );
        let discovered = match loader.load(workspace_path, fetch_dependencies).await {
            Ok(d) => d,
            Err(e) if fetch_dependencies => return Err(e).with_context(|| format!("load WORKSPACE repository declarations from {}", workspace_path.display())),
            Err(e) => {
                debug_log!(
                    "Warning: Failed to load WORKSPACE file {}: {}",
                    workspace_path.display(),
                    e
                );
                debug_log!("Continuing build without this WORKSPACE's dependencies");
                return Ok(());
            }
        };

        if depth == 0 {
            if let Ok(mut loader) = self.bzl_loader.lock() {
                loader.set_main_workspace_name(discovered.workspace_name.clone());
            }
        }
        if let Some(ref name) = discovered.workspace_name {
            eprintln!("   Workspace: {}", name);
        }

        // Register http_archive dependencies
        // NOTE: ExternalDepManager replaced by SystemPackageMapper for known repos
        // Unknown repos will be handled at build time via on-demand fetch
        if !discovered.http_archives.is_empty() {
            eprintln!(
                "   Found {} http_archive declarations",
                discovered.http_archives.len()
            );
            for archive in &discovered.http_archives {
                self.repository_manager
                    .register_http_archive(archive.clone());

                // Source materialization is demand-driven by actual load() or
                // selected graph labels, preserving the requested closure.
            }
        }

        // Register git_repository dependencies
        if !discovered.git_repositories.is_empty() {
            eprintln!(
                "   Found {} git_repository declarations",
                discovered.git_repositories.len()
            );

            for repo in &discovered.git_repositories {
                // For now, just create a placeholder directory so builds don't immediately fail on missing paths
                let external_dir = cache_paths::workspace_external_dir(&self.workspace_root);
                let repo_path = external_dir.join(&repo.name);
                std::fs::create_dir_all(&repo_path).ok();

                // Create a minimal BUILD file if one doesn't exist
                let build_file = repo_path.join("BUILD");
                if find_build_file(&repo_path).is_none() {
                    std::fs::write(
                        &build_file,
                        "# Auto-generated placeholder for git_repository",
                    )
                    .ok();
                }

                // Register
                let mut repo_paths = self.external_repo_paths.lock().await;
                repo_paths.insert(repo.name.clone(), repo_path.clone());
                drop(repo_paths);

                if let Ok(mut loader) = self.bzl_loader.lock() {
                    loader.add_implicit_system_adapter(repo.name.clone(), repo_path.clone());
                }

                eprintln!(
                    "      ✓ @{} (git stub) registered at {}",
                    repo.name,
                    repo_path.display()
                );
            }
        }

        // Register local_repository declarations
        if !discovered.local_repositories.is_empty() {
            eprintln!(
                "   Found {} local_repository declarations",
                discovered.local_repositories.len()
            );

            for repo in &discovered.local_repositories {
                let local_path = current_workspace_root.join(&repo.path);

                // Register in external_repo_paths
                let mut repo_paths = self.external_repo_paths.lock().await;
                repo_paths.insert(repo.name.clone(), local_path.clone());
                eprintln!("      ✓ {}: {} (local)", repo.name, local_path.display());
                drop(repo_paths); // Release lock

                // Register in BzlLoader
                if let Ok(mut loader) = self.bzl_loader.lock() {
                    loader.add_external_repo(repo.name.clone(), local_path.clone());
                    if !repo.repo_mapping.is_empty() {
                        loader.set_command_repo_mapping_for(
                            repo.name.clone(),
                            repo.repo_mapping.clone(),
                        );
                    }
                }

                // Recursively load external repo's WORKSPACE if it exists
                let external_workspace = local_path.join("WORKSPACE");
                let external_workspace = if external_workspace.exists() {
                    Some(external_workspace)
                } else {
                    let alt = local_path.join("WORKSPACE.bazel");
                    if alt.exists() {
                        Some(alt)
                    } else {
                        None
                    }
                };

                if let Some(ext_ws) = external_workspace {
                    if !visited.contains(&ext_ws) {
                        visited.insert(ext_ws.clone());
                        eprintln!("      → Loading nested WORKSPACE from: {}", repo.name);

                        // Recursive call with increased depth
                        if let Err(e) = self
                            .load_workspace_internal(
                                &ext_ws,
                                fetch_dependencies,
                                depth + 1,
                                visited,
                            )
                            .await
                        {
                            debug_log!(
                                "      ⚠  Warning: Failed to load nested WORKSPACE for {}: {}",
                                repo.name,
                                e
                            );
                            debug_log!("      Continuing with partial registration");
                        }
                    } else {
                        debug_log!(
                            "      ⚠  Circular WORKSPACE reference detected: {}",
                            repo.name
                        );
                    }
                }
            }
        }

        // Load statements are handled by WorkspaceLoader via Starlark evaluation.
        // The Evaluator recursively loads and executes .bzl files, and any rule calls
        // (like http_archive) made within those macros are captured in discovered.http_archives etc.

        // Create and register placeholder config repositories
        // These are auto-generated placeholders for configuration rules (cuda_configure, etc.)
        {
            let external_dir = cache_paths::workspace_external_dir(&self.workspace_root);
            std::fs::create_dir_all(&external_dir).ok();

            let minimal_repos = vec![
                ("local_config_cuda", "cuda", vec!["build_defs.bzl"]),
                ("local_config_rocm", "rocm", vec!["build_defs.bzl"]),
                ("local_config_tensorrt", "", vec!["build_defs.bzl"]),
                (
                    "local_config_remote_execution",
                    "",
                    vec!["remote_execution.bzl"],
                ),
                ("local_config_syslibs", "", vec!["build_defs.bzl"]),
                ("local_config_android", "", vec!["android.bzl"]),
                ("bazel_tools", "", vec![]),
                ("local_tsl", "", vec![]),
                ("local_xla", "", vec![]),
            ];

            let mut repo_paths = self.external_repo_paths.lock().await;
            let preserve_declared_source = |name: &str, paths: &mut HashMap<String, PathBuf>| -> Result<bool> {
                let mut loader = self.bzl_loader.lock()
                    .map_err(|_| anyhow::anyhow!("BzlLoader lock poisoned while preserving repository source"))?;
                if let Some(path) = loader.preferred_registered_repository_path(name, paths.get(name).map(PathBuf::as_path))
                    .filter(|path| name == "bazel_tools" || !loader.is_implicit_system_adapter(name, path))
                {
                    paths.insert(name.to_string(), path.clone());
                    if loader.is_managed_workspace_source(name, &path) {
                        loader.add_managed_workspace_source(name.to_string(), path);
                    } else if loader.is_implicit_system_adapter(name, &path) {
                        loader.add_implicit_system_adapter(name.to_string(), path);
                    } else {
                        loader.add_external_repo(name.to_string(), path);
                    }
                    return Ok(true);
                }
                Ok(false)
            };

            for (repo_name, subdir, bzl_files) in minimal_repos {
                // Compatibility setup fills missing/implicit registrations; a
                // discovered local or genuine source is already authoritative.
                if preserve_declared_source(repo_name, &mut repo_paths)? { continue; }
                let repo_path = external_dir.join(repo_name);

                // Create placeholder repo if it doesn't exist
                if !repo_path.exists() {
                    if let Err(e) = Self::create_minimal_repo(&repo_path, subdir, &bzl_files) {
                        debug_log!(
                            "[WARN] Failed to create placeholder repo {}: {}",
                            repo_name,
                            e
                        );
                        continue;
                    }
                    debug_log!("[DEBUG] Created placeholder repo: {}", repo_name);
                }

                repo_paths.insert(repo_name.to_string(), repo_path.clone());
                if let Ok(mut loader) = self.bzl_loader.lock() {
                    loader.add_implicit_system_adapter(repo_name.to_string(), repo_path.clone());
                }
            }

            // TEMPORARY HACK: Manually register org_tensorflow external repository
            // TODO: Parse local_repository() from WORKSPACE properly
            // Use TF_PATH env var, or default to ../tensorflow relative to workspace root
            let tf_path = std::env::var("TF_PATH")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| self.workspace_root.join("../tensorflow"));

            if tf_path.exists() && !preserve_declared_source("org_tensorflow", &mut repo_paths)? {
                debug_log!(
                    "[DEBUG] Registered external repo: org_tensorflow -> {}",
                    tf_path.display()
                );
                repo_paths.insert("org_tensorflow".to_string(), tf_path.clone());
                if let Ok(mut loader) = self.bzl_loader.lock() {
                    loader.add_implicit_system_adapter("org_tensorflow".to_string(), tf_path.clone());
                }
            }

            // Create symlinks for tf_vendored repositories (like local_xla, local_tsl)
            // These point to vendored dependencies within the workspace
            // NOTE: com_google_googleapis is NOT included here because third_party/googleapis
            // is a TensorFlow helper that loads from @com_google_googleapis, not the actual repo.
            // The actual @com_google_googleapis stub is created in external_deps.rs.
            let vendored_repos = vec![
                ("local_xla", "third_party/xla"),
                ("local_tsl", "third_party/xla/third_party/tsl"),
                // Use the checked-in abseil-py source instead of the C++ absl shim folder.
                // The path is relative to the TensorFlow workspace, so we hop up to Rush's
                // `external/absl_py` vendored copy.
                ("absl_py", "../external/absl_py"),
            ];

            for (repo_name, relpath) in vendored_repos {
                if preserve_declared_source(repo_name, &mut repo_paths)? { continue; }
                let repo_path = external_dir.join(repo_name);
                let target_path = tf_path.join(relpath);

                if target_path.exists() {
                    // Remove existing symlink/directory if present
                    if repo_path.exists() || repo_path.is_symlink() {
                        let _ = std::fs::remove_dir_all(&repo_path);
                    }

                    // Create symlink
                    #[cfg(unix)]
                    {
                        if let Err(e) = std::os::unix::fs::symlink(&target_path, &repo_path) {
                            debug_log!("[WARN] Failed to create symlink for {}: {}", repo_name, e);
                            continue;
                        }
                    }
                    #[cfg(not(unix))]
                    {
                        debug_log!(
                            "[WARN] Symlink creation not supported on this platform for {}",
                            repo_name
                        );
                        continue;
                    }

                    debug_log!(
                        "[DEBUG] Created vendored repo symlink: {} -> {}",
                        repo_name,
                        target_path.display()
                    );
                    repo_paths.insert(repo_name.to_string(), repo_path.clone());
                    if let Ok(mut loader) = self.bzl_loader.lock() {
                        loader.add_implicit_system_adapter(repo_name.to_string(), repo_path.clone());
                    }
                } else {
                    debug_log!(
                        "[WARN] Vendored path does not exist: {}",
                        target_path.display()
                    );
                }
            }
        }

        // Connect external repository paths to .bzl loader
        {
            let mut repo_paths = self.external_repo_paths.lock().await;
            let mut bzl_loader = self
                .bzl_loader
                .lock()
                .map_err(|e| anyhow::anyhow!("Failed to lock .bzl loader: {}", e))?;
            // A synchronous module load can update any repository before the
            // async graph sees it. Reconcile all names before bulk mirroring;
            // compatibility fallback lists are not a source-authority boundary.
            let mut names = repo_paths.keys().cloned().collect::<std::collections::HashSet<_>>();
            names.extend(bzl_loader.external_repos.iter().map(|entry| entry.key().clone()));
            let repo_paths_dashmap = Arc::new(dashmap::DashMap::new());
            for name in names {
                if let Some(path) = bzl_loader.preferred_registered_repository_path(&name, repo_paths.get(&name).map(PathBuf::as_path)) {
                    repo_paths.insert(name.clone(), path.clone());
                    repo_paths_dashmap.insert(name, path);
                }
            }
            bzl_loader.set_external_repos(repo_paths_dashmap);
            drop(repo_paths);

            // Configure repository alias mappings (env/file-driven with sensible defaults)
            let aliases_dashmap = Arc::new(dashmap::DashMap::new());

            // 1) Env var: RUSH_REPO_ALIASES="tsl=local_tsl,xla=local_xla"
            if let Ok(val) = std::env::var("RUSH_REPO_ALIASES") {
                for pair in val.split(',') {
                    if let Some((k, v)) = pair.split_once('=') {
                        if !k.trim().is_empty() && !v.trim().is_empty() {
                            aliases_dashmap.insert(k.trim().to_string(), v.trim().to_string());
                        }
                    }
                }
            }

            // 2) Config file: .rush/repo_aliases.json (object of {"tsl":"local_tsl"})
            let cfg_path = self.workspace_root.join(".rush").join("repo_aliases.json");
            if cfg_path.exists() {
                if let Ok(text) = std::fs::read_to_string(&cfg_path) {
                    if let Ok(map) = serde_json::from_str::<serde_json::Value>(&text) {
                        if let Some(obj) = map.as_object() {
                            for (k, v) in obj.iter() {
                                if let Some(s) = v.as_str() {
                                    aliases_dashmap.insert(k.to_string(), s.to_string());
                                }
                            }
                        }
                    }
                }
            }

            // 3) Defaults (only if not overridden)
            if !aliases_dashmap.contains_key("tsl") {
                aliases_dashmap.insert("tsl".to_string(), "local_tsl".to_string());
            }
            if !aliases_dashmap.contains_key("xla") {
                aliases_dashmap.insert("xla".to_string(), "local_xla".to_string());
            }

            bzl_loader.set_repo_aliases(aliases_dashmap);

            // Parse repo_mapping entries from WORKSPACE (Bazel semantics)
            if let Ok(ws_full) = rush_repo::workspace::parse_workspace(workspace_path) {
                for repo in ws_full.repos {
                    if let Some(rush_repo::workspace::Value::Dict(d)) =
                        repo.attributes.get("repo_mapping")
                    {
                        let mut mapping = std::collections::HashMap::new();
                        for (apparent, val) in d.iter() {
                            if let rush_repo::workspace::Value::String(canonical) = val {
                                mapping.insert(
                                    apparent.clone(),
                                    canonical.trim_start_matches('@').to_string(),
                                );
                            }
                        }
                        bzl_loader.set_command_repo_mapping_for(repo.name, mapping);
                    }
                }
            }
        }

        // Optional: run configure.py to generate local_config_* repositories when enabled
        {
            let need_local_configs = [
                "local_config_cuda",
                "local_config_rocm",
                "local_config_tensorrt",
            ];
            let missing_any = {
                let repo_paths = self.external_repo_paths.lock().await;
                need_local_configs
                    .iter()
                    .any(|r| !repo_paths.contains_key(&r.to_string()))
            };
            if missing_any && std::env::var("RUSH_RUN_CONFIGURE").ok().as_deref() == Some("1") {
                let cfg_py = self.workspace_root.join("configure.py");
                if cfg_py.exists() {
                    debug_log!("[INFO] Running configure.py to generate local_config_* repos...");
                    let mut cmd = std::process::Command::new("python3");
                    cmd.arg(&cfg_py).current_dir(&self.workspace_root);

                    // Optional non-interactive preset
                    if std::env::var("RUSH_CONFIGURE_PRESET").ok().as_deref() == Some("minimal") {
                        debug_log!("[INFO] Using minimal non-interactive configure preset");
                        cmd.env("PYTHON_BIN_PATH", "python3")
                            .env("TF_NEED_CUDA", "0")
                            .env("TF_NEED_ROCM", "0")
                            .env("TF_ENABLE_XLA", "0")
                            .env("TF_DOWNLOAD_CLANG", "0")
                            .env("TF_SET_ANDROID_WORKSPACE", "0")
                            .env("CC_OPT_FLAGS", "-O2")
                            .env("TF_NEED_TENSORRT", "0")
                            .env("TF_NEED_OPENCL_SYCL", "0")
                            .env("TF_NEED_MPI", "0")
                            .env("TF_CUDA_COMPUTE_CAPABILITIES", "")
                            .env("TF_NEED_S3", "0")
                            .env("TF_NEED_GCP", "0")
                            .env("TF_NEED_HDFS", "0");
                    }

                    match cmd.status() {
                        Ok(status) if status.success() => {
                            debug_log!("[INFO] configure.py completed successfully");
                        }
                        Ok(status) => {
                            debug_log!(
                                "[WARN] configure.py exited with status: {:?}",
                                status.code()
                            );
                        }
                        Err(e) => {
                            debug_log!("[WARN] Failed to execute configure.py: {}", e);
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Fetch a repository on-demand by searching for its definition in .bzl files.
    ///
    /// This allows lazy loading of external repositories only when they're actually needed,
    /// keeping CPUs busy with builds instead of parsing hundreds of .bzl files upfront.
    async fn fetch_repository_on_demand(&self, repo_name: &str) -> Result<()> {
        if self.requires_declared_workspace_source(repo_name) {
            self.fetch_workspace_repository_with_bazel(repo_name, &HashMap::new(), None).await?;
            return Ok(());
        }
        if repo_name == "com_google_absl" {
            eprintln!(
                "         Using system-provided stub repository for @{}",
                repo_name
            );
            return Ok(());
        }

        if !SystemPackageMapper::is_system_repo(repo_name) {
            eprintln!(
                "         Using Bazel repo fetcher for external repository @{}",
                repo_name
            );
            self.fetch_repo_with_bazel(repo_name).await?;
            return Ok(());
        }

        eprintln!(
            "         Searching for repository definition: @{}",
            repo_name
        );

        // Common search paths for TensorFlow-style projects
        let search_paths = vec![
            // TensorFlow XLA third-party (e.g., com_google_absl)
            format!(
                "third_party/xla/third_party/{}/workspace.bzl",
                repo_name.trim_start_matches("com_google_")
            ),
            // TensorFlow third-party
            format!("third_party/{}/workspace.bzl", repo_name),
            // Workspace macro files
            "tensorflow/workspace3.bzl".to_string(),
            "tensorflow/workspace2.bzl".to_string(),
            "tensorflow/workspace1.bzl".to_string(),
            "tensorflow/workspace0.bzl".to_string(),
        ];

        // Build list of root directories to search: main workspace + all external repos
        let mut search_roots = vec![self.workspace_root.clone()];
        {
            let repo_paths = self.external_repo_paths.lock().await;
            for (_, path) in repo_paths.iter() {
                if !search_roots.contains(path) {
                    search_roots.push(path.clone());
                }
            }
        }

        // Try each combination of root and search path
        for root in &search_roots {
            for search_path in &search_paths {
                let bzl_path = root.join(search_path);

                if !bzl_path.exists() {
                    continue;
                }

                eprintln!("         Checking: {}", search_path);

                // Read and parse the .bzl file
                let content = match std::fs::read_to_string(&bzl_path) {
                    Ok(c) => c,
                    Err(_) => continue,
                };

                // Quick check: does this file mention our repository?
                if !content.contains(&format!("name = \"{}\"", repo_name))
                    && !content.contains(&format!("name=\"{}\"", repo_name))
                {
                    continue;
                }

                eprintln!("         Found reference in {}, parsing...", search_path);

                // Parse the .bzl file
                let program = match rush_starlark::parse(&content) {
                    Ok(p) => p,
                    Err(e) => {
                        debug_log!("         Warning: Failed to parse {}: {}", search_path, e);
                        continue;
                    }
                };

                // Create minimal evaluator to capture http_archive calls
                let mut http_archives = Vec::new();

                // Evaluate the .bzl file with a special evaluator that captures http_archive calls
                // For now, use a simplified approach: extract http_archive calls from the AST directly
                if let Err(e) = self
                    .extract_http_archives_from_bzl(&program, repo_name, &mut http_archives)
                    .await
                {
                    debug_log!(
                        "         Warning: Failed to extract repositories from {}: {}",
                        search_path,
                        e
                    );
                    continue;
                }

                // If we found the repository, fetch it
                if !http_archives.is_empty() {
                    eprintln!(
                        "         Found {} repository definition(s)",
                        http_archives.len()
                    );

                    for archive in &http_archives {
                        if archive.name == repo_name {
                            eprintln!("         Downloading @{}...", repo_name);

                            // Use existing HttpArchiveManager to fetch
                            let manager = rush_repo::HttpArchiveManager::new(&self.workspace_root)?;
                            let repo_path = manager.fetch(archive).await?;

                            // Register in external_repo_paths
                            let mut repo_paths = self.external_repo_paths.lock().await;
                            repo_paths.insert(repo_name.to_string(), repo_path.clone());
                            if let Ok(mut loader) = self.bzl_loader.lock() {
                                loader.add_implicit_system_adapter(repo_name.to_string(), repo_path.clone());
                            }

                            eprintln!(
                                "         ✓ @{} fetched to {}",
                                repo_name,
                                repo_path.display()
                            );
                            return Ok(());
                        }
                    }
                }
            }
        }

        anyhow::bail!(
            "Repository @{} not found in any known .bzl files",
            repo_name
        )
    }

    /// Extract http_archive definitions from a .bzl AST
    async fn extract_http_archives_from_bzl(
        &self,
        program: &rush_starlark::Program,
        target_repo: &str,
        archives: &mut Vec<rush_repo::http_archive::HttpArchive>,
    ) -> Result<()> {
        // First pass: extract top-level variable assignments for format substitution
        let mut variables = std::collections::HashMap::new();
        for stmt in &program.statements {
            Self::extract_variables_from_stmt(stmt, &mut variables);
        }

        // Second pass: extract http_archive calls with variable context
        for stmt in &program.statements {
            self.extract_archives_from_stmt(stmt, target_repo, &variables, archives)?;
        }

        Ok(())
    }

    /// Extract top-level variable assignments from statements
    fn extract_variables_from_stmt(
        stmt: &rush_starlark::Stmt,
        variables: &mut std::collections::HashMap<String, String>,
    ) {
        use rush_starlark::{Expr, Stmt};

        match stmt {
            Stmt::Assign { target, value } => {
                // Only handle simple string assignments: VAR = "value"
                if let rush_starlark::BindingPattern::Identifier(name) = target {
                    if let Expr::String(s) = &**value {
                        variables.insert(name.clone(), s.clone());
                    }
                }
            }
            Stmt::FunctionDef { body, .. } => {
                // Recursively extract from function bodies
                for stmt in body {
                    Self::extract_variables_from_stmt(stmt, variables);
                }
            }
            _ => {}
        }
    }

    fn extract_archives_from_stmt(
        &self,
        stmt: &rush_starlark::Stmt,
        target_repo: &str,
        variables: &std::collections::HashMap<String, String>,
        archives: &mut Vec<rush_repo::http_archive::HttpArchive>,
    ) -> Result<()> {
        use rush_starlark::Stmt;

        match stmt {
            Stmt::Expr(expr) => {
                self.extract_archives_from_expr(expr, target_repo, variables, archives)?;
            }
            Stmt::FunctionDef { body, .. } => {
                for stmt in body {
                    self.extract_archives_from_stmt(stmt, target_repo, variables, archives)?;
                }
            }
            _ => {}
        }

        Ok(())
    }

    fn extract_archives_from_expr(
        &self,
        expr: &rush_starlark::Expr,
        target_repo: &str,
        variables: &std::collections::HashMap<String, String>,
        archives: &mut Vec<rush_repo::http_archive::HttpArchive>,
    ) -> Result<()> {
        use rush_starlark::Expr;

        if let Expr::Call(call) = expr {
            // Check if this is an http_archive or tf_http_archive call
            let func_name = match call.function.as_ref() {
                Expr::Ident(name) => Some(name.as_str()),
                _ => None,
            };

            if matches!(func_name, Some("http_archive") | Some("tf_http_archive")) {
                // Extract parameters
                let mut name = None;
                let mut url = None;
                let mut urls = None;
                let mut sha256 = None;
                let mut strip_prefix = None;
                let mut build_file = None;

                for (key, value) in &call.kwargs {
                    match key.as_str() {
                        "name" => {
                            if let Expr::String(s) = value {
                                name = Some(s.clone());
                            }
                        }
                        "url" => {
                            if let Expr::String(s) = value {
                                url = Some(s.clone());
                            }
                        }
                        "urls" => {
                            match value {
                                // Handle literal list: urls = ["url1", "url2"]
                                Expr::List(items) => {
                                    if let Some(Expr::String(s)) = items.first() {
                                        urls = Some(s.clone());
                                    }
                                }
                                // Handle function call (including method calls): urls = tf_mirror_urls("url".format(...))
                                Expr::Call { .. } => {
                                    // Extract and evaluate format strings with variable substitution
                                    urls = Self::eval_string_expr_with_format(value, variables);
                                }
                                _ => {}
                            }
                        }
                        "sha256" => {
                            if let Expr::String(s) = value {
                                sha256 = Some(s.clone());
                            }
                        }
                        "strip_prefix" => {
                            match value {
                                Expr::String(s) => {
                                    strip_prefix = Some(s.clone());
                                }
                                // Handle format calls: strip_prefix = "string-{var}".format(var=VAR)
                                Expr::Call { .. } => {
                                    strip_prefix =
                                        Self::eval_string_expr_with_format(value, variables);
                                }
                                _ => {}
                            }
                        }
                        "build_file" => {
                            if let Expr::String(s) = value {
                                build_file = Some(s.clone());
                            }
                        }
                        _ => {}
                    }
                }

                // Only add if this is the target repository
                if let Some(repo_name) = &name {
                    if repo_name == target_repo {
                        let final_url = urls.or(url).ok_or_else(|| {
                            anyhow::anyhow!("http_archive for @{} missing url/urls", repo_name)
                        })?;

                        // Convert build_file label to PathBuf
                        // Format: "//package:file" -> workspace_root/package/file
                        let build_file_path = build_file.as_ref().and_then(|label| {
                            if let Some(stripped) = label.strip_prefix("//") {
                                let parts: Vec<&str> = stripped.splitn(2, ':').collect();
                                if parts.len() == 2 {
                                    Some(self.workspace_root.join(parts[0]).join(parts[1]))
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        });

                        archives.push(rush_repo::http_archive::HttpArchive {
                            name: repo_name.clone(),
                            url: final_url,
                            sha256,
                            strip_prefix,
                            type_hint: None,
                            build_file_content: None,
                            build_file: build_file_path,
                        });
                    }
                }
            }
        }

        Ok(())
    }

    /// Evaluate a string expression with Python .format() substitution
    ///
    /// Handles expressions like:
    /// - "simple string" -> "simple string"
    /// - "template-{var}".format(var=VALUE) -> "template-<substituted>"
    /// - tf_mirror_urls("url-{commit}".format(commit=COMMIT)) -> "url-<substituted>"
    fn eval_string_expr_with_format(
        expr: &rush_starlark::Expr,
        variables: &std::collections::HashMap<String, String>,
    ) -> Option<String> {
        use rush_starlark::Expr;

        match expr {
            // Direct string literal
            Expr::String(s) => Some(s.clone()),

            // Function call - could be format() or wrapping function like tf_mirror_urls()
            Expr::Call(call) => {
                // Check if this is a .format() call on a string
                if let Expr::Attribute { object, attr } = call.function.as_ref() {
                    if attr == "format" {
                        // This is a format call: "template".format(...)
                        if let Some(template) =
                            Self::eval_string_expr_with_format(object, variables)
                        {
                            return Some(Self::apply_python_format(
                                &template,
                                &call.kwargs,
                                variables,
                            ));
                        }
                    }
                }

                // Otherwise, recursively search for strings in the call
                // (e.g., tf_mirror_urls(...) wrapping a format call)

                // Check function first (for method calls on strings)
                if let Some(s) = Self::eval_string_expr_with_format(&call.function, variables) {
                    return Some(s);
                }

                // Then check positional args
                for arg in &call.args {
                    if let Some(s) = Self::eval_string_expr_with_format(arg, variables) {
                        return Some(s);
                    }
                }

                None
            }

            // Attribute access: for method calls like "string".format()
            Expr::Attribute { object, .. } => Self::eval_string_expr_with_format(object, variables),

            // List: check first element
            Expr::List(items) => {
                for item in items {
                    if let Some(s) = Self::eval_string_expr_with_format(item, variables) {
                        return Some(s);
                    }
                }
                None
            }

            _ => None,
        }
    }

    /// Apply Python format string substitution
    ///
    /// Handles keyword arguments like format(commit="abc123")
    /// Supports both positional {0} and named {commit} placeholders
    fn apply_python_format(
        template: &str,
        kwargs: &[(String, rush_starlark::Expr)],
        variables: &std::collections::HashMap<String, String>,
    ) -> String {
        use rush_starlark::Expr;

        let mut result = template.to_string();

        // Process each keyword argument
        for (key, value_expr) in kwargs {
            // Resolve the value
            let value = match value_expr {
                Expr::String(s) => s.clone(),
                Expr::Ident(var_name) => {
                    // Look up variable in the context
                    variables
                        .get(var_name)
                        .cloned()
                        .unwrap_or_else(|| var_name.clone())
                }
                _ => continue, // Skip complex expressions for now
            };

            // Replace {key} with value
            result = result.replace(&format!("{{{}}}", key), &value);
        }

        result
    }

    /// Recursively find the first string literal in an expression tree
    #[allow(dead_code)]
    fn find_first_string_in_expr(expr: &rush_starlark::Expr) -> Option<String> {
        use rush_starlark::Expr;

        match expr {
            Expr::String(s) => Some(s.clone()),
            Expr::List(items) => {
                for item in items {
                    if let Some(s) = Self::find_first_string_in_expr(item) {
                        return Some(s);
                    }
                }
                None
            }
            Expr::Call(call) => {
                // Check function first (for method calls: "string".format() -> function is Attribute)
                if let Some(s) = Self::find_first_string_in_expr(&call.function) {
                    return Some(s);
                }
                // Then check args
                for arg in &call.args {
                    if let Some(s) = Self::find_first_string_in_expr(arg) {
                        return Some(s);
                    }
                }
                None
            }
            Expr::Attribute { object, .. } => {
                // For obj.attr, recursively search the object
                Self::find_first_string_in_expr(object)
            }
            _ => None,
        }
    }

    /// Create a minimal placeholder repository with minimal .bzl files
    ///
    /// Creates placeholder configuration repositories like @local_config_cuda that provide
    /// minimal implementations of configuration functions. This allows builds to proceed
    /// without actual hardware detection or configuration.
    ///
    /// # Arguments
    /// * `repo_path` - Path where the placeholder repo should be created
    /// * `subdir` - Subdirectory within the repo (e.g., "cuda", "rocm", or empty string)
    /// * `bzl_files` - List of .bzl files to create with minimal implementations
    fn create_minimal_repo(
        repo_path: &Path,
        subdir: &str,
        bzl_files: &[&str],
    ) -> anyhow::Result<()> {
        use std::fs;

        // Special handling for bazel_tools repository
        let repo_name = repo_path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        if repo_name == "bazel_tools" {
            return Self::create_bazel_tools_minimal(repo_path);
        }

        // Create repo directory structure
        let base_dir = if subdir.is_empty() {
            repo_path.to_path_buf()
        } else {
            let dir = repo_path.join(subdir);
            fs::create_dir_all(&dir)?;
            dir
        };

        fs::create_dir_all(&base_dir)?;

        // Create BUILD file (required for all repos)
        let build_file = repo_path.join("BUILD");
        if find_build_file(repo_path).is_none() {
            let build_content = Self::generate_minimal_build_content(repo_name);
            fs::write(&build_file, build_content)?;
        }

        // Create BUILD file for sub-packages when needed
        if !subdir.is_empty() {
            let sub_build = base_dir.join("BUILD");
            if find_build_file(&base_dir).is_none() {
                if let Some(content) =
                    Self::generate_minimal_subdir_build_content(repo_name, subdir)
                {
                    fs::write(sub_build, content)?;
                }
            }
        }

        // Create placeholder .bzl files with minimal implementations
        for bzl_file in bzl_files {
            let bzl_path = base_dir.join(bzl_file);
            if !bzl_path.exists() {
                let minimal_content = Self::generate_minimal_bzl_content(bzl_file);
                fs::write(&bzl_path, minimal_content)?;
            }
        }

        Ok(())
    }

    /// Create a placeholder bazel_tools repository with common tool .bzl files
    fn create_bazel_tools_minimal(repo_path: &Path) -> anyhow::Result<()> {
        use std::fs;

        // Create directory structure for bazel_tools
        fs::create_dir_all(repo_path)?;
        fs::create_dir_all(repo_path.join("tools/build_defs/repo"))?;
        fs::create_dir_all(repo_path.join("tools/cpp"))?;

        // Create BUILD files
        fs::write(
            repo_path.join("BUILD"),
            "# Auto-generated minimal placeholder for bazel_tools\n",
        )?;
        fs::write(
            repo_path.join("tools/BUILD"),
            "# Auto-generated minimal placeholder for bazel_tools/tools\n",
        )?;
        fs::write(
            repo_path.join("tools/build_defs/BUILD"),
            "# Auto-generated minimal placeholder for bazel_tools/tools/build_defs\n",
        )?;
        fs::write(
            repo_path.join("tools/build_defs/repo/BUILD"),
            "# Auto-generated minimal placeholder for bazel_tools/tools/build_defs/repo\n",
        )?;

        // Create placeholder http.bzl
        fs::write(
            repo_path.join("tools/build_defs/repo/http.bzl"),
            r#"# Auto-generated minimal placeholder for bazel_tools http.bzl

# Alias repository rules to native implementations so WORKSPACE evaluation can capture rule calls
http_archive = native.http_archive
http_file = native.http_file
http_jar = native.http_jar
"#,
        )?;

        // Create placeholder git.bzl
        fs::write(
            repo_path.join("tools/build_defs/repo/git.bzl"),
            r#"# Auto-generated minimal placeholder for bazel_tools git.bzl

# Alias git repository rule to native implementation
git_repository = native.git_repository
new_git_repository = native.git_repository
"#,
        )?;

        // Create placeholder local.bzl
        fs::write(
            repo_path.join("tools/build_defs/repo/local.bzl"),
            r#"# Auto-generated minimal placeholder for bazel_tools local.bzl

# Alias local repository rules to native implementation
local_repository = native.local_repository
new_local_repository = native.new_local_repository
"#,
        )?;

        // Create placeholder utils.bzl commonly loaded by repo macros
        fs::write(
            repo_path.join("tools/build_defs/repo/utils.bzl"),
            r#"# Auto-generated minimal placeholder for bazel_tools utils.bzl

def maybe(repo_rule, **kwargs):
    # Invoke the rule if needed; minimal no-op placeholder
    repo_rule(**kwargs)

def workspace_and_buildfile(ctx, **kwargs):
    # Minimal placeholder for functions that write workspace/BUILD files
    return None

def read_netrc(ctx, filename):
    """rush doesn't fetch with .netrc auth; return empty."""
    return {}

def read_user_netrc(ctx):
    """rush doesn't fetch with .netrc auth; return empty."""
    return {}

def use_netrc(netrc, urls, patterns):
    """rush doesn't fetch with .netrc auth; return empty."""
    return {}

def update_attrs(orig, keys, override):
    """Attribute-dict merge (real implementation, not empty)."""
    result = dict(orig)
    for key in keys:
        if key in override:
            result[key] = override[key]
    return result

def get_auth(ctx, urls):
    """rush doesn't fetch authenticated URLs; return empty."""
    return {}
"#,
        )?;

        // Create placeholder java.bzl
        fs::write(
            repo_path.join("tools/build_defs/repo/java.bzl"),
            r#"# Auto-generated minimal placeholder for bazel_tools java.bzl

def java_import_external(**kwargs):
    """Placeholder for java_import_external - returns default value"""
    pass

# Alias old-style maven jar rule to native http_file for simple handling
maven_jar = native.http_file
"#,
        )?;

        // Create placeholder lib_cc_configure.bzl for C++ toolchain configuration
        fs::write(
            repo_path.join("tools/cpp/lib_cc_configure.bzl"),
            r#"# Auto-generated minimal placeholder for bazel_tools lib_cc_configure.bzl

def escape_string(s):
    """Placeholder for escaping strings in C++ configuration"""
    return s

def get_env_var(repository_ctx, name, default = None):
    """Placeholder for getting environment variable"""
    return default if default != None else ""

def auto_configure_fail(msg):
    """Placeholder for auto-configuration failure"""
    fail(msg)

def cc_configure():
    """Placeholder for C++ toolchain configuration"""
    pass

def cc_autoconf():
    """Placeholder for C++ auto-configuration"""
    pass

def cc_download():
    """Placeholder for C++ toolchain download"""
    pass
"#,
        )?;

        // Create placeholder windows_cc_configure.bzl for Windows C++ toolchain
        fs::write(
            repo_path.join("tools/cpp/windows_cc_configure.bzl"),
            r#"# Auto-generated minimal placeholder for bazel_tools windows_cc_configure.bzl

def find_vc_path(repository_ctx):
    """Placeholder for finding Visual Studio path"""
    return ""

def find_msvc_tool(repository_ctx, vc_path, tool_name):
    """Placeholder for finding MSVC tool"""
    return ""

def setup_vc_env_vars(repository_ctx, vc_path):
    """Placeholder for setting up Visual Studio environment variables"""
    return {
        "PATH": "",
        "INCLUDE": "",
        "LIB": "",
    }

def configure_windows_toolchain():
    """Placeholder for Windows C++ toolchain configuration"""
    pass

def find_vs_path():
    """Placeholder for finding Visual Studio path"""
    pass
"#,
        )?;

        // Create placeholder cc_configure.bzl for C++ auto-configuration
        fs::write(
            repo_path.join("tools/cpp/cc_configure.bzl"),
            r#"# Auto-generated minimal placeholder for bazel_tools cc_configure.bzl

def cc_autoconf_impl(repository_ctx):
    """Placeholder for C++ toolchain auto-configuration implementation"""
    pass

def cc_autoconf(repository_ctx):
    """Placeholder for C++ toolchain auto-configuration"""
    pass
"#,
        )?;

        // Create placeholder toolchain_utils.bzl for Bazel's C++ helpers
        fs::write(
            repo_path.join("tools/cpp/toolchain_utils.bzl"),
            r#"# Auto-generated placeholder for bazel_tools/tools/cpp:toolchain_utils.bzl

def find_cpp_toolchain(_ctx = None):
    """Rush doesn't configure custom C++ toolchains yet."""
    return None

def use_cpp_toolchain(_impl = None):
    """rules_cc hook — rush uses a fixed toolchain, so ignore the impl."""
    pass
"#,
        )?;

        // #39 Phase R test-corpus: @bazel_tools//tools/cpp/runfiles. TF's tsl
        // (tsl/platform/default) depends on @bazel_tools//tools/cpp/runfiles for
        // the C++ runfiles library, so tf_cc_test sources that touch the env/
        // resource_loader path `#include "tools/cpp/runfiles/runfiles.h"`.
        // bazel_tools is system-classified, so that include previously had no
        // -I and no compiled object → "fatal error: ... runfiles.h: No such
        // file". Partially materialize ONLY this subpackage inside the existing
        // bazel_tools repo (Codex 019eb3ae): the embedded_tools forwarder header
        // (typedef bazel::tools::cpp::runfiles::Runfiles ->
        // rules_cc::cc::runfiles::Runfiles) plus the REAL rules_cc runfiles
        // {h,cc} (Apache-2.0, vendored under src/vendor/bazel_runfiles). Two
        // cc_library packages with repo-root `includes` so BOTH spellings
        // resolve: `tools/cpp/runfiles/runfiles.h` (what TF includes) and
        // `rules_cc/cc/runfiles/runfiles.h` (what the forwarder + runfiles.cc
        // include). `srcs` are package-relative, so runfiles.cc lives in its own
        // rules_cc/cc/runfiles package. NO stub — real source, compiled for real.
        let rc_runfiles = repo_path.join("rules_cc/cc/runfiles");
        let tcr_runfiles = repo_path.join("tools/cpp/runfiles");
        fs::create_dir_all(&rc_runfiles)?;
        fs::create_dir_all(&tcr_runfiles)?;
        fs::write(
            rc_runfiles.join("runfiles.h"),
            include_str!("vendor/bazel_runfiles/rules_cc_runfiles.h"),
        )?;
        fs::write(
            rc_runfiles.join("runfiles.cc"),
            include_str!("vendor/bazel_runfiles/rules_cc_runfiles.cc"),
        )?;
        fs::write(
            tcr_runfiles.join("runfiles.h"),
            include_str!("vendor/bazel_runfiles/tools_cpp_runfiles.h"),
        )?;
        let bt_root = repo_path.display().to_string();
        fs::write(
            rc_runfiles.join("BUILD"),
            format!(
                "# Real rules_cc C++ runfiles library (Apache-2.0), vendored by rush.\n\
                 package(default_visibility = [\"//visibility:public\"])\n\n\
                 cc_library(\n\
                 \x20   name = \"runfiles\",\n\
                 \x20   srcs = [\"runfiles.cc\"],\n\
                 \x20   hdrs = [\"runfiles.h\"],\n\
                 \x20   includes = [\"{root}\"],\n\
                 \x20   visibility = [\"//visibility:public\"],\n\
                 )\n",
                root = bt_root,
            ),
        )?;
        fs::write(
            tcr_runfiles.join("BUILD"),
            format!(
                "# Deprecated forwarder to @rules_cc//cc/runfiles (what TF depends on).\n\
                 package(default_visibility = [\"//visibility:public\"])\n\n\
                 cc_library(\n\
                 \x20   name = \"runfiles\",\n\
                 \x20   srcs = [],\n\
                 \x20   hdrs = [\"runfiles.h\"],\n\
                 \x20   deps = [\"//rules_cc/cc/runfiles:runfiles\"],\n\
                 \x20   includes = [\"{root}\"],\n\
                 \x20   visibility = [\"//visibility:public\"],\n\
                 )\n",
                root = bt_root,
            ),
        )?;

        // Python tools use the corresponding Bazel runfiles module through
        // `from bazel_tools.tools.python.runfiles import runfiles`. Materialize
        // a real directory/manifest resolver instead of stubbing this target:
        // generated py_binary wrappers expose their external-repository roots
        // through RUSH_RUNFILES_ROOTS.
        let py_runfiles = repo_path.join("tools/python/runfiles");
        fs::create_dir_all(&py_runfiles)?;
        fs::write(py_runfiles.join("__init__.py"), "")?;
        fs::write(
            py_runfiles.join("runfiles.py"),
            include_str!("vendor/bazel_runfiles/python_runfiles.py"),
        )?;
        fs::write(
            py_runfiles.join("BUILD"),
            "# Bazel-compatible Python runfiles library materialized by rush.\n\
             package(default_visibility = [\"//visibility:public\"])\n\n\
             py_library(\n\
             \x20   name = \"runfiles\",\n\
             \x20   srcs = [\"__init__.py\", \"runfiles.py\"],\n\
             )\n",
        )?;

        Ok(())
    }

    /// Generate placeholder BUILD file content based on repository name
    fn generate_minimal_build_content(repo_name: &str) -> String {
        match repo_name {
            _ => {
                // Default BUILD file for other repositories
                "# Auto-generated minimal placeholder repository\n".to_string()
            }
        }
    }

    /// Generate placeholder .bzl file content with minimal function implementations
    fn generate_minimal_bzl_content(filename: &str) -> String {
        match filename {
            "build_defs.bzl" => {
                // Common placeholder functions for build_defs.bzl files
                r#"# Auto-generated minimal placeholder for configuration repository

def if_cuda(if_true, if_false = []):
    """Returns if_false (CUDA not configured)"""
    return if_false

def if_rocm(if_true, if_false = []):
    """Returns if_false (ROCm not configured)"""
    return if_false

def if_rocm_is_configured(if_true, if_false = []):
    """Returns if_false (ROCm not configured)"""
    return if_false

def if_rocm_hipblaslt(if_true, if_false = []):
    """Returns if_false (HIP BLAS LT not configured)"""
    return if_false

def if_cuda_is_configured(if_true, if_false = []):
    """Returns if_false (CUDA auto-config not available in Rush)"""
    return if_false

def if_cuda_toolkit_path(if_true, if_false = []):
    """Returns if_false (CUDA toolkit path detection not implemented)"""
    return if_false

def if_tensorrt(if_true, if_false = []):
    """Returns if_false (TensorRT not configured)"""
    return if_false

def if_cuda_exec(if_true, if_false = []):
    """Returns if_false (CUDA exec not configured)"""
    return if_false

def if_tensorrt_exec(if_true, if_false = []):
    """Returns if_false (TensorRT exec not configured)"""
    return if_false

def cuda_library(**kwargs):
    """Treat cuda_library as a cc_library for dependency analysis."""
    native.cc_library(**kwargs)

def rocm_copts():
    """Returns empty copts for ROCm"""
    return []

def cuda_gpu_architectures():
    """Return empty list since CUDA is disabled"""
    return []

def rocm_gpu_architectures():
    """Return empty list since ROCm is disabled"""
    return []

# System libraries configuration (for local_config_syslibs)
SYSTEM_LIBS_ENABLED = False
SYSTEM_LIBS_LIST = []

def if_any_system_libs(a, b=[]):
    """Conditional which evaluates to 'a' if any system libraries are configured."""
    if SYSTEM_LIBS_ENABLED:
        return a
    else:
        return b

def if_system_lib(lib, a, b=[]):
    """Conditional which evaluates to 'a' if we're using the system version of lib"""
    if SYSTEM_LIBS_ENABLED and lib in SYSTEM_LIBS_LIST:
        return a
    else:
        return b

def if_not_system_lib(lib, a, b=[]):
    """Conditional which evaluates to 'a' if we're NOT using the system version of lib"""
    return if_system_lib(lib, b, a)
"#
                .to_string()
            }
            "android.bzl" => {
                // Placeholder for Android configuration
                r#"# Auto-generated minimal placeholder for Android configuration repository

"""Set up configurable Android SDK and NDK dependencies."""

def android_workspace():
    """Minimal placeholder for android_workspace - Android not configured."""
    pass
"#
                .to_string()
            }
            "remote_execution.bzl" => {
                // Placeholder for remote execution configuration
                r#"# Auto-generated minimal placeholder for remote execution

def if_remote_execution(if_true, if_false = []):
    """Returns if_false (remote execution not configured)"""
    return if_false

def gpu_test_tags():
    """Returns empty list (GPU testing not configured)"""
    return []
"#
                .to_string()
            }
            _ => {
                // Generic placeholder for unknown .bzl files
                format!("# Auto-generated minimal placeholder for {}\n", filename)
            }
        }
    }

    /// Generate BUILD file content for placeholder sub-packages.
    fn generate_minimal_subdir_build_content(repo_name: &str, subdir: &str) -> Option<String> {
        match (repo_name, subdir) {
            ("local_config_cuda", "cuda") => {
                let cuda_cfg = Self::detect_cuda_config().unwrap_or_else(|| CudaConfig {
                    include_dir: PathBuf::from("/usr/local/cuda/include"),
                    lib_dir: PathBuf::from("/usr/local/cuda/lib64"),
                });

                let include_flag = format!("-I{}", cuda_cfg.include_dir.display());
                let lib_flag = format!("-L{}", cuda_cfg.lib_dir.display());
                let rpath_flag = format!("-Wl,-rpath,{}", cuda_cfg.lib_dir.display());

                Some(format!(
                    r#"# Auto-generated CUDA package pointing at the system toolkit
cc_library(
    name = "cudart",
    hdrs = [],
    copts = ["{include}"],
    linkopts = ["{lib}", "{rpath}", "-lcudart"],
    visibility = ["//visibility:public"],
)
cc_library(
    name = "cuda_headers",
    hdrs = [],
    copts = ["{include}"],
    visibility = ["//visibility:public"],
)
"#,
                    include = include_flag,
                    lib = lib_flag,
                    rpath = rpath_flag
                ))
            }
            _ => None,
        }
    }

    fn detect_cuda_config() -> Option<CudaConfig> {
        let mut include_candidates = Vec::new();
        let mut lib_candidates = Vec::new();

        for var in ["CUDA_HOME", "CUDA_PATH"] {
            if let Ok(home) = env::var(var) {
                let home_path = PathBuf::from(home);
                include_candidates.push(home_path.join("include"));
                lib_candidates.push(home_path.join("lib64"));
                lib_candidates.push(home_path.join("lib"));
                lib_candidates.push(home_path.join("targets/x86_64-linux/lib"));
            }
        }

        include_candidates.push(PathBuf::from("/usr/local/cuda/include"));
        include_candidates.push(PathBuf::from("/usr/include"));

        lib_candidates.push(PathBuf::from("/usr/local/cuda/lib64"));
        lib_candidates.push(PathBuf::from("/usr/local/cuda/lib"));
        lib_candidates.push(PathBuf::from("/usr/lib/x86_64-linux-gnu"));
        lib_candidates.push(PathBuf::from("/usr/lib64"));
        lib_candidates.push(PathBuf::from("/usr/lib"));

        let include_dir = include_candidates
            .into_iter()
            .find(|dir| dir.join("cuda.h").exists());
        let lib_dir = lib_candidates.into_iter().find(|dir| {
            dir.join("libcudart.so").exists() || dir.join("libcudart_static.a").exists()
        });

        match (include_dir, lib_dir) {
            (Some(include_dir), Some(lib_dir)) => Some(CudaConfig {
                include_dir,
                lib_dir,
            }),
            _ => None,
        }
    }

    /// Set remote executor endpoint for distributed builds.
    pub fn set_remote_executor(&mut self, endpoint: String) {
        self.remote_executor = Some(Arc::new(RemoteExecutor::new(endpoint)));
    }

    /// Select strict standard-protocol execution for native file actions.
    pub fn set_reapi_executor(&mut self, endpoint: String, instance: String,
        platform: std::collections::BTreeMap<String, String>) -> Result<()> {
        self.remote_executor = Some(Arc::new(RemoteExecutor::with_reapi(
            endpoint, self.workspace_root.clone(), instance, platform)?));
        self.action_manager = Arc::new(RushActionManager::new(
            Arc::new(ActionCache::new()),
            Arc::new(rush_remote::executor::ReapiStrategy(Arc::clone(self.remote_executor.as_ref().unwrap()))),
            Arc::clone(&self.resource_manager),
        ));
        Ok(())
    }

    /// Set maximum number of parallel jobs for compilation.
    ///
    /// Controls the global concurrency limit across all compilation actions.
    /// Lower values reduce memory consumption but increase build time.
    /// Higher values speed up builds but may cause OOM on systems with limited RAM.
    ///
    /// # Arguments
    /// * `jobs` - Maximum number of concurrent compilation jobs (minimum: 1)
    ///
    /// Set maximum parallel jobs (creates new resource manager with explicit limits).
    ///
    /// # Examples
    /// ```ignore
    /// orchestrator.set_max_jobs(8);  // Limit to 8 parallel compilations
    /// ```
    pub fn set_max_jobs(&mut self, jobs: u32) {
        let jobs = jobs.max(1) as usize;
        // Preserve dynamic memory monitoring; only adjust CPU slot limit.
        // This avoids freezing available memory at a one-time snapshot.
        self.resource_manager.set_max_cpu_slots_sync(jobs);
    }

    /// Get remote executor statistics if remote execution is enabled.
    pub async fn remote_executor_stats(&self) -> Option<RemoteExecutorStats> {
        if let Some(ref executor) = self.remote_executor {
            Some(executor.stats().await)
        } else {
            None
        }
    }

    /// Determine resource requirements for an action based on its mnemonic and args.
    fn resource_request_for_action(&self, action: &Action) -> ResourceRequest {
        let args = &action.command.args;
        match action.mnemonic.as_str() {
            // Heuristics: bump memory for debug builds and LTO
            "CppCompile" | "CCompile" => {
                let mut req = ResourceRequest::cpp_compile();
                let has_lto = args.iter().any(|a| a == "-flto" || a.contains("thinlto"));
                let has_debug = args.iter().any(|a| a == "-g");
                if has_lto {
                    // LTO compiles tend to need more memory
                    req.memory_mb = req.memory_mb.max(4096);
                }
                if has_debug {
                    // Debug info can inflate memory use substantially on large TU
                    req.memory_mb += 512;
                }
                req
            }
            "CppLink" | "Link" => {
                // Base link cost, scale with number of objects and LTO
                let mut req = ResourceRequest::cpp_link();
                let obj_count = args
                    .iter()
                    .filter(|a| a.ends_with(".o") || a.ends_with(".obj"))
                    .count();
                if obj_count > 0 {
                    // Every ~400 objects, add ~2GB. Cap at 16GB by default.
                    let extra_chunks = obj_count / 400;
                    let extra_mb = extra_chunks.saturating_mul(2048);
                    req.memory_mb = (req.memory_mb + extra_mb).min(16 * 1024);
                }
                let has_lto = args
                    .iter()
                    .any(|a| a == "-flto" || a.contains("thinlto") || a.contains("-Wl,--thinlto"));
                if has_lto {
                    req.memory_mb = req.memory_mb.max(8 * 1024);
                }
                req
            }
            "RustCompile" => {
                let mut req = ResourceRequest::rust_compile();
                if args.iter().any(|a| a == "-C" && a.contains("lto")) {
                    req.memory_mb = req.memory_mb.max(5 * 1024);
                }
                req
            }
            "RustLink" => ResourceRequest::rust_link(),
            _ => ResourceRequest::minimal(),
        }
    }

    fn action_outputs_exist(action: &Action) -> bool {
        action.outputs.iter().all(|out| match out {
            Artifact::Source(path) => path.exists(),
            Artifact::Derived { output_path, .. } => output_path.exists(),
        })
    }

    fn source_value_artifact(path: &Path) -> Artifact {
        // Inputs are identified by their current value, never by a producer's
        // ActionId. The Source hash path reads the filesystem or an explicitly
        // verified virtual record.
        Artifact::Source(path.to_path_buf())
    }

    fn link_input_has_archive_or_library_flag(flag: &str) -> bool {
        if flag.contains("--threads") || flag.contains("--no-threads") {
            return true;
        }
        if flag.contains(".a") || flag.contains(".so.") {
            return true;
        }
        matches!(
            flag,
            "-Wl,--whole-archive"
                | "-Wl,--no-whole-archive"
                | "-Wl,--start-group"
                | "-Wl,--end-group"
                | "-l"
        ) || flag.starts_with("-l")
    }

    fn legacy_link_argv_approx_len(object_paths: &[PathBuf], link_flags: &[String]) -> usize {
        object_paths.iter().map(|p| p.as_os_str().len() + 1).sum::<usize>()
            + link_flags.iter().map(|s| s.len() + 1).sum::<usize>()
            + 8
    }

    fn legacy_mold_small_link_threads_flag(
        linker_flag: Option<&str>,
        object_paths: &[PathBuf],
        link_flags: &[String],
    ) -> Option<String> {
        if linker_flag != Some("-fuse-ld=mold") {
            return None;
        }

        match std::env::var("RUSH_MOLD_SMALL_LINK_THREADS").ok() {
            None => {}
            Some(raw) => {
                let value = raw.trim();
                if value.is_empty() || value == "0" || value.eq_ignore_ascii_case("off") {
                    return None;
                }
                if value.eq_ignore_ascii_case("no-threads") {
                    return Some("-Wl,--no-threads".to_string());
                }
                return Some(format!("-Wl,--threads={value}"));
            }
        }

        if object_paths.len() < 128 {
            return None;
        }
        let approx_len = Self::legacy_link_argv_approx_len(object_paths, link_flags);
        if approx_len < 16 * 1024 || approx_len > 64 * 1024 {
            return None;
        }
        if link_flags
            .iter()
            .any(|flag| Self::link_input_has_archive_or_library_flag(flag))
        {
            return None;
        }

        Some("-Wl,--threads=2".to_string())
    }

    /// Execute an action using the appropriate strategy (remote or local).
    async fn execute_action(&self, action: &Action) -> Result<rush_exec::ActionResult> {
        let action_t0 = std::time::Instant::now();
        if matches!(action.mnemonic.as_str(), "CppArchive" | "CcArchive")
            && Self::action_outputs_exist(action)
            && crate::thin_archive::can_reuse_cc_archive_command(
                &action.command.executable,
                &action.command.args,
            )
        {
            let result = rush_exec::ActionResult {
                action_id: action.id.clone(),
                exit_code: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
                outputs: action.outputs.clone(),
            output_records: Vec::new(),
                duration: action_t0.elapsed(),
                cached: true,
            };
            if crate::action_timing::enabled() {
                crate::action_timing::record(action, None, action_t0.elapsed(), Some(0), true);
            }
            return Ok(result);
        }

        // Acquire resources before execution (OOM prevention)
        let resource_request = self.resource_request_for_action(action);
        let resource_handle = self.resource_manager.acquire(resource_request).await?;

        // Execute action with appropriate strategy
        let execution_timer = crate::action_timing::ExecutionTimer::start_if_enabled();
        let result = if let Some(ref remote_executor) = self.remote_executor {
            // Use remote execution with local fallback
            let local_fallback = async {
                let strategy = LocalStrategy;
                strategy.execute(action).await
            };
            remote_executor
                .execute_with_fallback(action, local_fallback)
                .await
        } else {
            // Local execution only
            let strategy = LocalStrategy;
            strategy.execute(action).await
        };
        let execution = execution_timer.map(|timer| timer.finish());

        // Release resources after execution
        resource_handle.release_async().await;

        if crate::action_timing::enabled() {
            let (exit_code, cached) = match &result {
                Ok(r) => (Some(r.exit_code), r.cached),
                Err(_) => (None, false),
            };
            crate::action_timing::record_with_execution(
                action,
                None,
                action_t0.elapsed(),
                execution,
                exit_code,
                cached,
            );
        }

        result
    }

    /// Execute an action with streaming output using the appropriate strategy.
    async fn _execute_action_streaming<F>(
        &self,
        action: &Action,
        on_output: F,
    ) -> Result<rush_exec::ActionResult>
    where
        F: Fn(&str),
    {
        // Acquire resources before execution (OOM prevention)
        let resource_request = self.resource_request_for_action(action);
        let resource_handle = self.resource_manager.acquire(resource_request).await?;

        // Remote execution doesn't support streaming yet, so use local
        let strategy = LocalStrategy;
        let result = strategy.execute_streaming(action, on_output).await;

        // Release resources after execution
        resource_handle.release_async().await;

        result
    }

    /// Clear incremental build state (force clean build).
    pub async fn clear_incremental_state(&self) -> Result<()> {
        let mut detector = self.change_detector.lock().await;
        detector.clear();
        detector.save_state()?;
        eprintln!("✨ Incremental build state cleared");
        Ok(())
    }

    /// Get incremental build statistics.
    pub async fn incremental_stats(&self) -> Option<incremental::ChangeDetectorStats> {
        Some(self.change_detector.lock().await.stats())
    }

    /// Get build metrics.
    pub fn metrics(&self) -> &Arc<BuildMetrics> {
        &self.metrics
    }

    /// Get resource manager statistics.
    pub fn resource_stats(&self) -> resource::ResourceStats {
        self.resource_manager.stats_sync()
    }

    /// Write compilation database to compile_commands.json.
    pub async fn write_compilation_database(&self) -> Result<()> {
        let db = self.compdb.lock().await;

        if db.is_empty() {
            return Ok(());
        }

        let output_path = self.workspace_root.join("compile_commands.json");
        db.write_to_file(&output_path)?;

        let count = db.len();
        eprintln!(
            "📝 {}",
            format!(
                "Wrote {} compilation commands to compile_commands.json",
                count
            )
            .cyan()
        );

        Ok(())
    }

    /// Export build metrics to JSON file.
    pub fn export_metrics_json(&self, path: &std::path::Path) -> Result<()> {
        use std::fs::File;
        use std::io::Write;

        let json = self.metrics.to_json();
        let json_str = serde_json::to_string_pretty(&json)?;

        let mut file = File::create(path)?;
        file.write_all(json_str.as_bytes())?;
        file.write_all(b"\n")?;

        eprintln!(
            "📊 {}",
            format!("Exported metrics to {}", path.display()).cyan()
        );

        Ok(())
    }

    /// Export build metrics to CSV file.
    pub fn export_metrics_csv(&self, path: &std::path::Path) -> Result<()> {
        use std::fs::File;
        use std::io::Write;

        let csv = self.metrics.to_csv();

        let mut file = File::create(path)?;
        file.write_all(csv.as_bytes())?;

        eprintln!(
            "📊 {}",
            format!("Exported metrics to {}", path.display()).cyan()
        );

        Ok(())
    }

    /// Keep the disk cache within its configured size.
    ///
    /// Runs before one-shot builds and after daemon builds. Errors are reported
    /// and swallowed: failing a successful build because eviction had trouble
    /// would be worse than the disk growth it prevents.
    pub async fn gc_disk_cache_if_needed(&self) {
        if !self.cache.uses_local_disk() {
            return;
        }
        if let Err(e) = self.cache.gc_disk().await {
            eprintln!("⚠️  disk cache gc failed: {}", e);
        }
        // The CAS needs its own eviction: ActionResult gc bounds the
        // .cache files, but blob bytes dominate - a TF certification
        // sweep wrote 415 GB of blobs and filled the host. Budget from
        // RUSH_CAS_MAX_BYTES, default 50 GiB. The directory walk touches
        // one inode per blob, so run it off the async thread.
        let max_bytes = std::env::var("RUSH_CAS_MAX_BYTES")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(50 * 1024 * 1024 * 1024);
        let done = tokio::task::spawn_blocking(move || {
            let cas = rush_cache::cas::CasStore::new(&rush_cache::roots::cas_store_root())?;
            cas.gc(max_bytes)
        })
        .await;
        match done {
            Ok(Ok(stats)) if stats.evicted > 0 || stats.stale_tmp > 0 => {
                eprintln!(
                    "🧹 CAS gc: {:.1} GB -> {:.1} GB ({} blobs evicted, {} stale tmp)",
                    stats.bytes_before as f64 / 1e9,
                    stats.bytes_after as f64 / 1e9,
                    stats.evicted,
                    stats.stale_tmp
                );
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => eprintln!("⚠️  CAS gc failed: {}", e),
            Err(e) => eprintln!("⚠️  CAS gc join error: {}", e),
        }
    }

    /// Path for a Chrome-trace of this build, from `RUSH_BUILD_TRACE`.
    ///
    /// The trace profiler and its nine phase scopes - workspace_load,
    /// target_expansion, topological_sort, analyze_pass_a, snapshot_commit,
    /// execute_pass_b and friends - were already wired, and every
    /// constructor passed `BuildProfiler::new(Self::build_trace_path().is_some())`, so nothing could
    /// ever turn them on. Answering "where do the seconds go in a
    /// single-file incremental build?" needed exactly this data.
    pub fn build_trace_path() -> Option<std::path::PathBuf> {
        std::env::var("RUSH_BUILD_TRACE")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from)
    }

    /// Write the Chrome trace if `RUSH_BUILD_TRACE` asked for one.
    pub fn write_build_trace_if_enabled(&self) {
        let Some(path) = Self::build_trace_path() else {
            return;
        };
        match self.profiler.write_trace(&path) {
            Ok(()) => eprintln!("📊 build trace written to {}", path.display()),
            Err(e) => eprintln!("⚠️  could not write build trace: {}", e),
        }
    }

    /// Export profiler trace to file (Chrome Trace Format).
    pub fn export_profiler_trace(&self, path: &std::path::Path) -> Result<()> {
        self.profiler.write_trace(path)
    }

    /// Generate DOT graph for visualization.
    ///
    /// Creates a Graphviz DOT representation of the build dependency graph.
    /// If target is None, generates the full workspace graph.
    /// If target is Some, generates graph for just that target and its deps.
    pub fn generate_dot_graph(&self, target: Option<&str>) -> Result<String> {
        use std::collections::HashSet;

        let mut dot = String::new();
        dot.push_str("digraph BuildGraph {\n");
        dot.push_str("  rankdir=LR;\n");
        dot.push_str("  node [shape=box];\n\n");

        // Collect all targets to include
        let targets = if let Some(target_label) = target {
            vec![target_label.to_string()]
        } else {
            // Find all targets in workspace
            self.find_all_targets()?
        };

        let mut visited = HashSet::new();
        let mut edges = Vec::new();

        for target_label in &targets {
            self.collect_target_graph(target_label, &mut visited, &mut edges)?;
        }

        // Generate nodes with colors based on rule type
        for node in &visited {
            let (package, name) = parse_target_label(node)?;
            let build_path = find_build_file(&self.workspace_root.join(&package));

            if let Some(build_path) = build_path {
                let Ok(build_file) = self.parse_build_file_cached(&build_path, None) else {
                    continue;
                };
                if let Some(rule) = build_file.rules.iter().find(|r| r.name == name) {
                    let color = match rule.rule_type.as_str() {
                        "cc_library" => "lightblue",
                        "cc_binary" => "lightgreen",
                        "cc_test" => "lightyellow",
                        "rust_library" => "orange",
                        "rust_binary" => "darkorange",
                        "rust_test" => "gold",
                        "py_library" | "py_strict_library" => "lightcyan",
                        "py_binary" => "cyan",
                        "py_test" => "turquoise",
                        _ => "white",
                    };
                    dot.push_str(&format!(
                        "  \"{}\" [label=\"{}\\n({})\", fillcolor=\"{}\", style=filled];\n",
                        node, name, rule.rule_type, color
                    ));
                }
            }
        }

        dot.push('\n');

        // Generate edges
        for (from, to) in &edges {
            dot.push_str(&format!("  \"{}\" -> \"{}\";\n", from, to));
        }

        dot.push_str("}\n");

        Ok(dot)
    }

    /// Recursively collect target and its dependencies for graph generation.
    fn collect_target_graph(
        &self,
        target: &str,
        visited: &mut HashSet<String>,
        edges: &mut Vec<(String, String)>,
    ) -> Result<()> {
        if visited.contains(target) {
            return Ok(());
        }

        visited.insert(target.to_string());

        let (package, name) = parse_target_label(target)?;
        let Some(build_path) = find_build_file(&self.workspace_root.join(&package)) else {
            return Ok(());
        };

        let build_file = self.parse_build_file_cached(&build_path, None)?;
        let rule = match build_file.rules.iter().find(|r| r.name == name) {
            Some(r) => r,
            None => return Ok(()),
        };

        // Process dependencies
        if let Some(Value::List(deps)) = rule.attributes.get("deps") {
            for dep in deps {
                if let Value::String(dep_label_str) = dep {
                    // Resolve relative labels
                    let dep_label = if dep_label_str.starts_with(':') {
                        format!("//{}:{}", package, dep_label_str.strip_prefix(':').unwrap())
                    } else {
                        dep_label_str.clone()
                    };

                    edges.push((target.to_string(), dep_label.clone()));

                    // Recurse into dependency
                    self.collect_target_graph(&dep_label, visited, edges)?;
                }
            }
        }

        Ok(())
    }

    /// Enable build profiling.
    pub fn with_profiler(mut self, enabled: bool) -> Self {
        self.profiler = Arc::new(BuildProfiler::new(enabled));
        self
    }

    /// Enable build event stream.
    pub fn with_events(mut self, enabled: bool) -> Self {
        self.events = Arc::new(BuildEventStream::new(enabled));
        self
    }

    /// Set build event stream to write to a file.
    pub fn with_event_file(mut self, path: &Path) -> Result<Self> {
        self.events = Arc::new(BuildEventStream::to_file(path)?);
        Ok(self)
    }

    fn preferred_build_file(&self, package_dir: &Path) -> Option<PathBuf> {
        let exists = |path: &Path| {
            if self.tracked_io_shared().is_some() {
                self.with_tracked_scope(|| rush_incremental::tracked::exists(path))
            } else {
                path.exists()
            }
        };
        build_file_candidates(package_dir)
            .into_iter()
            .find(|path| exists(path))
    }

    fn repository_name_for_root(&self, repo_root: &Path) -> Option<String> {
        if repo_root == self.workspace_root.as_path() {
            return None;
        }
        let canonicalize = |path: &Path| {
            if self.tracked_io_shared().is_some() {
                self.with_tracked_scope(|| {
                    rush_incremental::tracked::canonicalize(path)
                })
                .unwrap_or_else(|_| path.to_path_buf())
            } else {
                std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
            }
        };
        let canonical_root = canonicalize(repo_root);
        let loader = self.bzl_loader.lock().ok()?;
        let repository = loader.external_repos.iter().find_map(|entry| {
            let candidate = canonicalize(entry.value());
            (candidate == canonical_root).then(|| entry.key().clone())
        });
        repository
    }

    fn config_registry_scope(&self) -> String {
        self.workspace_root.to_string_lossy().into_owned()
    }

    /// Pre-register the config_settings of a BUILD file's package AND of any
    /// package its `select()`s reference, BEFORE the file is parsed/evaluated.
    ///
    /// This removes the parse-order dependence of define-driven `select()`
    /// resolution (see the call site in [`Self::parse_build_file_cached`]). It
    /// first does a cheap AST scan, then fully evaluates referenced condition
    /// packages so macro-generated declarations are registered before the
    /// consumer is instantiated. It reads the BUILD source, parses it with
    /// `rush_starlark::parse`, registers
    /// every literal `config_setting` with supported define, native-value, or
    /// canonical `@platforms` constraints that it declares,
    /// then resolves each cross-package `//pkg:name` select condition to that
    /// package's `BUILD`/`BUILD.bazel` and registers ITS literal config_settings
    /// too. Each package snapshot replaces prior configuration declarations,
    /// so edited/deleted declarations cannot stay active.
    ///
    /// Guarded by a per-command visited set so concurrent consumers wait until
    /// registration is complete, while later commands re-read tracked source
    /// and replace stale package definitions. Fully evaluating a newly scanned
    /// condition package runs this pass for that package in turn, so chains of
    /// macro-generated conditions are loaded transitively; the visited set
    /// terminates cycles and keeps the pass O(referenced packages).
    fn configuration_package_visit_key(
        repo_root: &Path,
        repository: Option<&str>,
        package: &str,
    ) -> String {
        format!(
            "{}::{}::{}",
            repo_root.display(),
            repository.unwrap_or("<main>"),
            package
        )
    }

    fn fully_evaluate_configuration_package(
        &self,
        build_path: &Path,
        repo_root: &Path,
        repository: Option<&str>,
        package: &str,
    ) -> Result<()> {
        let key = Self::configuration_package_visit_key(
            repo_root,
            repository,
            package,
        );
        let Some(_guard) = ConfigPackageEvaluationGuard::enter(key) else {
            return Ok(());
        };

        let repo_root = repo_root.to_path_buf();
        self.parse_build_file_cached_for_repository(
            build_path,
            Some(&repo_root),
            repository,
        )
        .map(|_| ())
    }

    fn pre_register_config_settings(
        &self,
        build_path: &Path,
        actual_repo_root: &Path,
        package: &str,
        explicit_repository: Option<&str>,
    ) -> Result<()> {
        let repository = explicit_repository
            .map(str::to_owned)
            .or_else(|| self.repository_name_for_root(actual_repo_root));
        if actual_repo_root != self.workspace_root.as_path()
            && repository.is_none()
        {
            // Never collapse an unidentified external repository into main
            // configuration identity. Its normal evaluator pass can register
            // conditions once the loader supplies the repository name.
            return Ok(());
        }

        // Scan `this_package`'s BUILD at `path`: register its config settings
        // and return referenced select labels. Hold the visited-set lock through
        // registration so a parallel consumer cannot observe an in-progress
        // marker. Read failures are not memoized; the normal evaluator later
        // reports the authoritative error.
        let scan = |path: &Path,
                    repo_root: &Path,
                    repository: Option<&str>,
                    this_package: &str|
         -> Result<Option<std::collections::HashSet<String>>> {
            let key = Self::configuration_package_visit_key(
                repo_root,
                repository,
                this_package,
            );
            let scope = self.config_registry_scope();
            let mut all_seen = PRESCANNED_CONFIG_PACKAGES.lock().unwrap();
            let seen = all_seen.entry(scope).or_default();
            if seen.contains(&key) {
                return Ok(None);
            }
            let source = if self.tracked_io_shared().is_some() {
                self.with_tracked_scope(|| {
                    rush_incremental::tracked::read_to_string(path)
                })
                .with_context(|| format!("read configuration package {}", path.display()))?
            } else {
                std::fs::read_to_string(path)
                    .with_context(|| format!("read configuration package {}", path.display()))?
            };
            let program = rush_starlark::parse(&source)
                .with_context(|| format!("parse configuration package {}", path.display()))?;
            rush_starlark::config_defines::register_config_settings_from_program(
                repository,
                this_package,
                &program,
            );
            seen.insert(key);
            Ok(Some(rush_starlark::config_defines::collect_select_condition_labels(
                &program,
            )))
        };

        let _ = scan(
            build_path,
            actual_repo_root,
            repository.as_deref(),
            package,
        )?;

        // Recompute the active set now that this package's config_settings are
        // registered (cheap; only meaningful when defines are active).
        rush_starlark::config_defines::recompute_active_config_settings_public();

        rush_starlark::config_defines::recompute_active_config_settings_public();
        Ok(())
    }

    fn resolve_condition_package(
        &self,
        condition: &str,
        current_repo_root: &Path,
        current_repository: Option<&str>,
    ) -> Result<Option<(PathBuf, Option<String>, String)>> {
        if condition == "//conditions:default" {
            return Ok(None);
        }
        if let Some(rest) = condition.strip_prefix("//") {
            return Ok(Some((
                current_repo_root.to_path_buf(),
                current_repository.map(str::to_owned),
                rest.to_string(),
            )));
        }

        let (canonical_label, explicit) = if let Some(explicit) = condition.strip_prefix("@@") {
            (true, explicit)
        } else if let Some(explicit) = condition.strip_prefix('@') {
            (false, explicit)
        } else {
            return Ok(None);
        };
        let Some((repository, rest)) = explicit.split_once("//") else {
            return Ok(None);
        };
        if repository.is_empty() {
            return Ok(Some((self.workspace_root.clone(), None, rest.to_string())));
        }
        let is_current_canonical_repository = current_repository == Some(repository)
            && self
                .bzl_loader
                .lock()
                .ok()
                .and_then(|loader| loader.external_repo_path(repository))
                .is_some_and(|root| root == current_repo_root);
        if (canonical_label || is_current_canonical_repository)
            && current_repository == Some(repository)
        {
            return Ok(Some((
                current_repo_root.to_path_buf(),
                Some(repository.to_string()),
                rest.to_string(),
            )));
        }
        let resolved = self
            .bzl_loader
            .lock()
            .map_err(|error| anyhow::anyhow!("Failed to lock .bzl loader: {error}"))?
            .resolve_repository_for_owner_with_target(
                repository,
                canonical_label,
                current_repository,
                current_repo_root,
                Some(rest),
            )
            .with_context(|| {
                format!(
                    "resolve configuration repository '@{}' from {}",
                    repository,
                    current_repository.unwrap_or("the main repository")
                )
            })?;
        let Some((canonical_repository, repo_root)) = resolved else {
            return Ok(None);
        };
        if canonical_repository.is_empty() {
            if rest == "conditions:default" {
                return Ok(None);
            }
            return Ok(Some((self.workspace_root.clone(), None, rest.to_string())));
        }
        Ok(Some((
            repo_root,
            Some(canonical_repository),
            rest.to_string(),
        )))
    }

    fn register_evaluated_configuration_rules(
        &self,
        build_file: &rush_parse::BuildFile,
        repo_root: &Path,
        repository: Option<&str>,
        package: &str,
    ) -> Result<()> {
        let context = rule_execution::InstantiationContext {
            repo_name: repository.map(str::to_owned),
            package: package.to_string(),
            main_repository_name: self
                .bzl_loader
                .lock()
                .ok()
                .and_then(|loader| loader.main_workspace_name()),
            repository_mapping: self.repository_mapping_for(repository, repo_root),
            ..rule_execution::InstantiationContext::default()
        };
        let canonical_reference = |label: &str| {
            rule_execution::canonicalize_configuration_reference(label, &context)
        };
        let string_dict = |rule: &rush_parse::Rule,
                           attribute: &str|
         -> Result<HashMap<String, String>> {
            let Some(value) = rule.attributes.get(attribute) else {
                return Ok(HashMap::new());
            };
            let Value::Dict(values) = value else {
                anyhow::bail!("{}.{} must be a string dictionary", rule.name, attribute);
            };
            values
                .iter()
                .map(|(key, value)| match value {
                    Value::String(value) => Ok((key.clone(), value.clone())),
                    _ => anyhow::bail!(
                        "{}.{} value for '{}' must be a string",
                        rule.name,
                        attribute,
                        key
                    ),
                })
                .collect()
        };

        for rule in &build_file.rules {
            match rule.rule_type.as_str() {
                "config_setting" => {
                    let define_values = string_dict(rule, "define_values")?;
                    let native_values = string_dict(rule, "values")?;
                    let flag_values = string_dict(rule, "flag_values")?
                        .into_iter()
                        .map(|(label, value)| (canonical_reference(&label), value))
                        .collect();
                    let constraint_values = match rule.attributes.get("constraint_values") {
                        None => Vec::new(),
                        Some(Value::List(values)) => values
                            .iter()
                            .map(|value| match value {
                                Value::String(value) => Ok(canonical_reference(value)),
                                _ => anyhow::bail!(
                                    "{}.constraint_values entries must be labels",
                                    rule.name
                                ),
                            })
                            .collect::<Result<Vec<_>>>()?,
                        Some(_) => anyhow::bail!(
                            "{}.constraint_values must be a sequence",
                            rule.name
                        ),
                    };
                    rush_starlark::config_defines::consider_config_setting_predicates(
                        repository,
                        package,
                        &rule.name,
                        &define_values,
                        &native_values,
                        &constraint_values,
                        &flag_values,
                    );
                }
                "alias" => {
                    if let Some(Value::String(actual)) = rule.attributes.get("actual") {
                        rush_starlark::config_defines::consider_config_alias(
                            repository,
                            package,
                            &rule.name,
                            &canonical_reference(actual),
                        );
                    }
                }
                "constraint_value" => {
                    rush_starlark::config_defines::consider_constraint_value(
                        repository,
                        package,
                        &rule.name,
                    );
                }
                _ => {}
            }
            let is_declared_build_setting = rule.rule_type == "label_flag"
                || matches!(
                    rule.attributes.get("__rush_build_setting"),
                    Some(Value::Bool(true))
                );
            if is_declared_build_setting {
                let Some(default) = rule.attributes.get("build_setting_default") else {
                    continue;
                };
                let value = match default {
                    Value::String(value) => Some(
                        rush_starlark::config_defines::BuildSettingValue::String(value.clone()),
                    ),
                    Value::Bool(value) => Some(
                        rush_starlark::config_defines::BuildSettingValue::Bool(*value),
                    ),
                    Value::Int(value) => Some(
                        rush_starlark::config_defines::BuildSettingValue::Int(*value),
                    ),
                    // A custom list/dict build setting is valid Starlark. Rush
                    // does not consume it in this TODO's configuration model;
                    // keep the package loadable and let an exact future
                    // consumer report that unsupported value type.
                    _ => None,
                };
                if let Some(value) = value {
                    rush_starlark::config_defines::consider_typed_build_setting_default(
                        repository,
                        package,
                        &rule.name,
                        value,
                    );
                }
            }
        }
        rush_starlark::config_defines::refresh_composed_config_settings();
        Ok(())
    }

    fn pre_register_evaluated_select_conditions(
        &self,
        build_file: &rush_parse::BuildFile,
        actual_repo_root: &Path,
        repository: Option<&str>,
        package: &str,
        target_names: Option<&std::collections::HashSet<String>>,
    ) -> Result<std::collections::HashSet<String>> {
        // This entry is intentionally self-scoping. Several production callers
        // reach it after a cached parse has restored its thread-local scope;
        // relying on caller scope would register recursively loaded conditions
        // in the empty compatibility registry and make results order-dependent.
        let scope = self.config_registry_scope();
        rush_starlark::config_defines::with_configuration_scope(&scope, || {
            self.pre_register_evaluated_select_conditions_scoped(
                build_file,
                actual_repo_root,
                repository,
                package,
                target_names,
            )
        })
    }

    fn symbolic_declared_outputs(rule: &rush_parse::Rule) -> Result<Vec<String>> {
        fn string_list(value: Option<&Value>, attribute: &str) -> Result<Vec<String>> {
            let Some(value) = value else {
                return Ok(Vec::new());
            };
            let Value::List(values) = value else {
                anyhow::bail!("{} must be a list of strings", attribute);
            };
            values
                .iter()
                .map(|value| match value {
                    Value::String(value) => Ok(value.clone()),
                    _ => anyhow::bail!("{} must be a list of strings", attribute),
                })
                .collect()
        }

        match rule.rule_type.as_str() {
            "genrule" => string_list(rule.attributes.get("outs"), "genrule.outs"),
            "expand_template" => match rule.attributes.get("out") {
                Some(Value::String(output)) => Ok(vec![output.clone()]),
                Some(_) => anyhow::bail!("expand_template.out must be a string"),
                None => Ok(Vec::new()),
            },
            "tf_gen_options_header" => match rule.attributes.get("output_header") {
                Some(Value::String(output)) => Ok(vec![output.clone()]),
                Some(_) => anyhow::bail!("tf_gen_options_header.output_header must be a string"),
                None => Ok(Vec::new()),
            },
            "gentbl_cc_library" => {
                let Some(value) = rule.attributes.get("tbl_outs") else {
                    return Ok(Vec::new());
                };
                let Value::List(entries) = value else {
                    anyhow::bail!("gentbl_cc_library.tbl_outs must be a list");
                };
                entries
                    .iter()
                    .map(|entry| match entry {
                        Value::List(pair) if pair.len() == 2 => match &pair[1] {
                            Value::String(output) => Ok(output.clone()),
                            _ => anyhow::bail!(
                                "gentbl_cc_library.tbl_outs output must be a string"
                            ),
                        },
                        _ => anyhow::bail!(
                            "gentbl_cc_library.tbl_outs entries must be two-element tuples"
                        ),
                    })
                    .collect()
            }
            _ => Ok(Vec::new()),
        }
    }

    fn implicit_proto_alias_names(
        target_name: &str,
        rule_type: &str,
        repository: Option<&str>,
    ) -> Vec<String> {
        if matches!(
            rule_type,
            "proto_alias" | "alias" | "shared_library_output_alias"
        ) {
            return Vec::new();
        }

        let mut aliases = Vec::new();
        let is_derived_alias = target_name.ends_with("_genproto")
            || target_name.ends_with("_proto_srcs")
            || target_name.ends_with("_cc_impl")
            || target_name.ends_with("_headers_only");
        let is_proto_target = !is_derived_alias
            && (matches!(rule_type, "proto_library" | "py_proto_library")
                || target_name.ends_with("_proto")
                || target_name.ends_with("_proto_cc"));
        if is_proto_target && repository != Some("com_google_protobuf") {
            if !target_name.ends_with("_cc") {
                aliases.push(format!("{target_name}_cc"));
            }
            aliases.push(format!("{target_name}_cc_impl"));
            if !target_name.ends_with("_cc") {
                aliases.push(format!("{target_name}_genproto"));
                aliases.push(format!("{target_name}_cc_genproto"));
            }
        }

        if target_name.ends_with("_proto_cc") && target_name.len() > 9 {
            aliases.push(target_name[..target_name.len() - 3].to_string());
        } else if target_name.ends_with("_cc") {
            let base = target_name.trim_end_matches("_cc");
            if !base.is_empty() {
                aliases.push(base.to_string());
                aliases.push(format!("{base}_cc_impl"));
                aliases.push(format!("{base}_genproto"));
            }
        }
        aliases.sort();
        aliases.dedup();
        aliases
    }

    pub(crate) fn package_declaration_catalog(
        build_file: &rush_parse::BuildFile,
        repository: Option<&str>,
        package: &str,
    ) -> Result<PackageDeclarationCatalog> {
        let mut catalog = PackageDeclarationCatalog::default();
        let mut rules = build_file.rules.iter().collect::<Vec<_>>();
        rules.sort_by(|left, right| left.name.cmp(&right.name));
        let source_file_targets = rules
            .iter()
            .filter(|rule| rule.rule_type == "source_file")
            .map(|rule| rule.name.as_str())
            .collect::<std::collections::HashSet<_>>();

        for rule in &rules {
            catalog
                .producers
                .insert(rule.name.clone(), rule.name.clone());
        }

        let mut output_owners = HashMap::<PathBuf, String>::new();
        for rule in &rules {
            let owner = crate::dependency_graph::Label {
                workspace: repository.map(str::to_owned),
                package: package.to_string(),
                target: rule.name.clone(),
            };
            let mut outputs = Self::symbolic_declared_outputs(rule)?;
            outputs.sort();
            for output in outputs {
                let normalized = crate::dependency_graph::DependencyGraph::normalize_declared_output(
                    &owner,
                    &output,
                )?;
                let output_name = normalized.to_string_lossy().into_owned();
                let output_label = crate::dependency_graph::Label {
                    workspace: repository.map(str::to_owned),
                    package: package.to_string(),
                    target: output_name.clone(),
                };
                if let Some(previous) = output_owners.insert(normalized, rule.name.clone()) {
                    let previous = crate::dependency_graph::Label {
                        workspace: repository.map(str::to_owned),
                        package: package.to_string(),
                        target: previous,
                    };
                    if previous == owner {
                        anyhow::bail!(
                            "Target {} declares output {} more than once",
                            owner,
                            output_label
                        );
                    }
                    anyhow::bail!(
                        "Declared output {} has multiple owners: {} and {}",
                        output_label,
                        previous,
                        owner
                    );
                }
                if let Some(explicit) = catalog.producers.get(&output_name) {
                    if explicit != &rule.name
                        && !source_file_targets.contains(output_name.as_str())
                    {
                        anyhow::bail!(
                            "Declared output {} conflicts with target {}//{}:{}",
                            output_label,
                            repository.map(|name| format!("@{name}")).unwrap_or_default(),
                            package,
                            explicit
                        );
                    }
                }
                catalog.producers.insert(output_name, rule.name.clone());
            }
        }

        for rule in rules {
            for alias in
                Self::implicit_proto_alias_names(&rule.name, &rule.rule_type, repository)
            {
                catalog.producers.entry(alias).or_insert_with(|| rule.name.clone());
            }
        }
        Ok(catalog)
    }

    fn pre_register_evaluated_select_conditions_scoped(
        &self,
        build_file: &rush_parse::BuildFile,
        actual_repo_root: &Path,
        repository: Option<&str>,
        package: &str,
        target_names: Option<&std::collections::HashSet<String>>,
    ) -> Result<std::collections::HashSet<String>> {
        let mut context = rule_execution::InstantiationContext {
            configuration_scope: self.config_registry_scope(),
            repo_name: repository.map(str::to_owned),
            package: package.to_string(),
            main_repository_name: self
                .bzl_loader
                .lock()
                .ok()
                .and_then(|loader| loader.main_workspace_name()),
            repository_mapping: self.repository_mapping_for(repository, actual_repo_root),
            ..rule_execution::InstantiationContext::default()
        };

        let declarations =
            Self::package_declaration_catalog(build_file, repository, package)?;
        let all_rule_names = build_file
            .rules
            .iter()
            .map(|rule| rule.name.clone())
            .collect::<std::collections::HashSet<_>>();
        let mut pending = target_names
            .map(|names| {
                names
                    .iter()
                    .filter_map(|name| declarations.producer_for(name).map(str::to_owned))
                    .collect::<std::collections::BTreeSet<_>>()
            })
            .unwrap_or_else(|| all_rule_names.iter().cloned().collect());
        let mut configured = std::collections::HashSet::new();

        // Keep condition loading scoped to the configured closure, including
        // same-package children that are already present in the package graph
        // and therefore never appear in the later "missing label" loader.
        // Each rule is resolved only after its own select keys are preloaded;
        // the resolved local dependencies extend the fixed point.
        while let Some(name) = pending.pop_first() {
            if !configured.insert(name.clone()) {
                continue;
            }
            let selected_build_file = rush_parse::BuildFile {
                path: build_file.path.clone(),
                rules: build_file
                    .rules
                    .iter()
                    .filter(|rule| rule.name == name)
                    .cloned()
                    .collect(),
            };
            let mut roots = Vec::new();
            if target_names.is_some() && selected_build_file.rules.iter().any(|rule|
                rule.rule_type == "config_setting"
                    && matches!(rule.attributes.get("flag_values"), Some(Value::Dict(values)) if !values.is_empty())) {
                roots.push(format!("//{package}:{name}"));
            }
            for condition in rule_execution::build_file_raw_configuration_conditions(&selected_build_file, &context) {
                roots.push(if condition.starts_with(':') { format!("//{package}{condition}") }
                    else if condition.starts_with("//") || condition.starts_with('@') { condition }
                    else { format!("//{package}:{condition}") });
            }
            let inherited = if target_names.is_some() && selected_build_file.rules.iter()
                .all(|rule| matches!(rule.rule_type.as_str(), "alias" | "config_setting")) {
                let current = crate::dependency_graph::Label {
                    workspace: repository.map(str::to_owned), package: package.to_owned(), target: name.clone(),
                };
                rush_starlark::config_defines::inherited_pycross_metadata_origins(&context.configuration_scope, &current.to_string())
            } else { std::collections::BTreeSet::new() };
            if !inherited.is_empty() && roots.is_empty() { roots.push(format!("//{package}:{name}")); }
            self.preload_configuration_metadata_roots(&roots, actual_repo_root, repository,
                target_names.is_some(), &inherited)?;
            rush_starlark::config_defines::recompute_active_config_settings_public();
            context.repository_mapping =
                self.repository_mapping_for(repository, actual_repo_root);

            let instances = rule_execution::build_file_to_instances_with_context(
                &selected_build_file,
                &self.rule_registry,
                &context,
            )?;
            for dependency in instances
                .iter()
                .flat_map(|instance| instance.dependencies())
            {
                let Ok(label) = crate::dependency_graph::Label::parse(
                    &dependency,
                    repository,
                    Some(package),
                ) else {
                    continue;
                };
                let same_repository = match (repository, label.workspace.as_deref()) {
                    (Some(current), Some(candidate)) => current == candidate,
                    (None, None) => true,
                    (None, Some(candidate)) => {
                        context.main_repository_name.as_deref() == Some(candidate)
                    }
                    _ => false,
                };
                if same_repository && label.package == package {
                    if let Some(producer) = declarations.producer_for(&label.target) {
                        if !configured.contains(producer) {
                            pending.insert(producer.to_string());
                        }
                    }
                }
            }
        }
        rush_starlark::config_defines::recompute_active_config_settings_public();
        Ok(configured)
    }

    /// Load the exact configuration-metadata closure rooted at one select key.
    ///
    /// A condition target may be an alias to a config_setting in another
    /// package, and a config_setting may in turn name aliased/custom
    /// constraint_values.  Loading only the first package makes select results
    /// depend on which package happened to be parsed first.  Chase only these
    /// metadata references (never unrelated sibling rules), with a target-level
    /// cycle guard, before resolving the consumer's select.
    fn validate_feature_flag_condition(&self, canonical_label: &str) -> Result<()> {
        self.validate_feature_flag_condition_inner(canonical_label).with_context(|| format!(
            "condition {canonical_label}: Rush does not yet support non-empty config_setting.flag_values without verified configured metadata"))
    }

    fn validate_feature_flag_condition_inner(&self, canonical_label: &str) -> Result<()> {
        let scope = self.config_registry_scope();
        if rush_starlark::config_defines::feature_flag_condition_is_verified(&scope, canonical_label) {
            return Ok(());
        }
        let command = self.feature_flag_command.lock()
            .map_err(|_| anyhow::anyhow!("feature-flag command lock poisoned"))?
            .clone().context("configured flag analysis has no immutable command request")?;
        let analysis = if let Some(analysis) = command.analysis { analysis } else {
            let binary = rush_starlark::BzlLoader::resolve_configured_analysis_binary(&self.workspace_root)?;
            let cache_root = command.semantics.getenv(b"RUSH_WORKSPACE_CACHE_ROOT")
                .map(|value| {
                    #[cfg(unix)] { use std::os::unix::ffi::OsStrExt;
                        PathBuf::from(std::ffi::OsStr::from_bytes(value)) }
                    #[cfg(not(unix))] { PathBuf::from(String::from_utf8_lossy(value).into_owned()) }
                }).unwrap_or_else(|| self.workspace_root.join(".cache/rush"));
            let analysis = configured_flag_metadata::ConfiguredFeatureFlagAnalysis::new_with_toolchain(
                &self.workspace_root, &binary, &cache_root.join("bazel_output_base"),
                &command.semantics, rush_rules::context::Toolchain::default())?;
            let mut slot = self.feature_flag_command.lock()
                .map_err(|_| anyhow::anyhow!("feature-flag command lock poisoned"))?;
            if let Some(current) = slot.as_mut() {
                if current.semantics.config_fingerprint() != command.semantics.config_fingerprint() {
                    anyhow::bail!("feature-flag command changed while constructing analysis context");
                }
                current.analysis = Some(analysis.clone());
            }
            analysis
        };
        analysis.validate_registered_condition(&scope, canonical_label)
    }

    /// Resolve only a consumed options-header declaration through the same
    /// immutable configured-analysis capsule used for flag predicates. No
    /// provider is attached and no inline writer is enabled by this method.
    pub(crate) fn resolve_options_header_inputs(
        &self,
        request: &rush_rules::providers::OptionsHeaderRequest,
    ) -> Result<std::sync::Arc<rush_rules::providers::VerifiedOptionsHeaderInputs>> {
        let scope = self.config_registry_scope();
        let generation = rush_starlark::config_defines::feature_flag_generation(&scope);
        let command = self
            .feature_flag_command
            .lock()
            .map_err(|_| anyhow::anyhow!("configured command lock poisoned"))?
            .clone()
            .context("options header has no immutable native command request")?;
        let analysis = if let Some(analysis) = command.analysis.clone() {
            analysis
        } else {
            let binary =
                rush_starlark::BzlLoader::resolve_configured_analysis_binary(&self.workspace_root)?;
            let cache_root = command
                .semantics
                .getenv(b"RUSH_WORKSPACE_CACHE_ROOT")
                .map(|value| {
                    #[cfg(unix)]
                    {
                        use std::os::unix::ffi::OsStrExt;
                        PathBuf::from(std::ffi::OsStr::from_bytes(value))
                    }
                    #[cfg(not(unix))]
                    {
                        PathBuf::from(String::from_utf8_lossy(value).into_owned())
                    }
                })
                .unwrap_or_else(|| self.workspace_root.join(".cache/rush"));
            let analysis =
                configured_flag_metadata::ConfiguredFeatureFlagAnalysis::new_with_toolchain(
                    &self.workspace_root,
                    &binary,
                    &cache_root.join("bazel_output_base"),
                    &command.semantics,
                    rush_rules::context::Toolchain::default(),
                )?;
            let mut slot = self
                .feature_flag_command
                .lock()
                .map_err(|_| anyhow::anyhow!("configured command lock poisoned"))?;
            let current = slot
                .as_mut()
                .context("native command ended during options metadata preparation")?;
            anyhow::ensure!(
                current.semantics.config_fingerprint() == command.semantics.config_fingerprint(),
                "native command changed during options metadata preparation"
            );
            current.analysis = Some(analysis.clone());
            analysis
        };
        let inputs = analysis.resolve_options_header_inputs(request)?;
        let current = self
            .feature_flag_command
            .lock()
            .map_err(|_| anyhow::anyhow!("configured command lock poisoned"))?
            .clone()
            .context("native command ended during options metadata analysis")?;
        anyhow::ensure!(
            current.semantics.config_fingerprint() == command.semantics.config_fingerprint()
                && inputs.command_fingerprint == *command.semantics.config_fingerprint()
                && generation == rush_starlark::config_defines::feature_flag_generation(&scope),
            "native command or configured metadata generation changed during options analysis"
        );
        Ok(inputs)
    }

    fn preload_configuration_metadata_roots(
        &self, raw_roots: &[String], repo_root: &Path, repository: Option<&str>, demand_proof: bool,
        inherited: &std::collections::BTreeSet<String>,
    ) -> Result<()> {
        let scope = self.config_registry_scope();
        for _ in 0..64 {
            let mut metadata = ConfigurationMetadataPreload::default();
            let mut roots = Vec::new();
            for root in raw_roots {
                if let Some(label) = self.preload_configuration_metadata_target(root, repo_root, repository, &mut metadata)? {
                    roots.push(label);
                }
            }
            if !demand_proof {
                return metadata.deferred_alias_errors.into_iter().next().map_or(Ok(()), Err);
            }
            let mut changed = false;
            let (required, provenance) = metadata.classify_flag_proofs(&scope, &roots, inherited);
            for condition in required {
                if !rush_starlark::config_defines::feature_flag_condition_is_verified(&scope, &condition) {
                    self.validate_feature_flag_condition(&condition)?;
                    changed = true;
                }
            }
            if !changed {
                if let Some(error) = metadata.deferred_alias_errors.into_iter().next() { return Err(error); }
                rush_starlark::config_defines::record_pycross_metadata_origins(&scope, &provenance);
                return Ok(());
            }
            // A newly proved predicate can change a configurable alias actual.
            // Reload only these roots so its newly selected metadata is loaded
            // before any consumer's select or pending RuleInstance is resolved.
        }
        anyhow::bail!("configured metadata proof closure did not converge")
    }

    fn preload_configuration_metadata_target(
        &self,
        raw_label: &str,
        current_repo_root: &Path,
        current_repository: Option<&str>,
        metadata: &mut ConfigurationMetadataPreload,
    ) -> Result<Option<String>> {
        let Some((repo_root, repository, rest)) = self.resolve_condition_package(
            raw_label,
            current_repo_root,
            current_repository,
        )? else {
            // `//conditions:default` and intrinsic @platforms constraints have
            // no BUILD package that Rush needs to materialize here.
            return Ok(None);
        };
        let Some((package, target)) = rest.split_once(':') else {
            return Ok(None);
        };
        let canonical_label = crate::dependency_graph::Label {
            workspace: repository.clone(), package: package.to_owned(), target: target.to_owned(),
        }.to_string();
        let visit_key = format!(
            "{}::{}::{}:{}",
            repo_root.display(),
            repository.as_deref().unwrap_or("<main>"),
            package,
            target,
        );
        if !metadata.visited.insert(visit_key) {
            return Ok(Some(canonical_label));
        }

        let Some(build_path) = self.preferred_build_file(&repo_root.join(package)) else {
            let scope = self.config_registry_scope();
            rush_starlark::config_defines::with_configuration_scope(&scope, || {
                rush_starlark::config_defines::clear_configuration_package(
                    repository.as_deref(),
                    package,
                );
            });
            return Ok(Some(canonical_label));
        };
        self.pre_register_config_settings(
            &build_path,
            &repo_root,
            package,
            repository.as_deref(),
        )?;
        self.fully_evaluate_configuration_package(
            &build_path,
            &repo_root,
            repository.as_deref(),
            package,
        )?;
        let build_file = self.parse_build_file_cached_for_repository(
            &build_path,
            Some(&repo_root),
            repository.as_deref(),
        )?;
        let Some(rule) = build_file.rules.iter().find(|rule| rule.name == target) else {
            return Ok(Some(canonical_label));
        };

        if rule.rule_type == "config_setting"
            && matches!(rule.attributes.get("flag_values"), Some(Value::Dict(values)) if !values.is_empty()) {
            metadata.flag_conditions.insert(canonical_label.clone());
        }
        if rule.rule_type == "alias" { metadata.alias_nodes.insert(canonical_label.clone()); }
        let mut alias_edges = std::collections::BTreeSet::new();

        let relative = |label: &str| {
            if label.starts_with(':') {
                format!("//{package}{label}")
            } else if label.starts_with("//") || label.starts_with('@') {
                label.to_string()
            } else {
                format!("//{package}:{label}")
            }
        };
        let mut references = Vec::new();
        let mut resolved_configurable_alias_actual = None;
        match rule.rule_type.as_str() {
            "alias" => {
                match rule.attributes.get("actual") {
                    Some(Value::String(actual)) => references.push(relative(actual)),
                    Some(Value::Select(_) | Value::Configurable { .. }) => {
                        // Skylib implements config_setting_group as a chain of
                        // native aliases whose `actual` is a select. Load only
                        // this alias attribute's condition keys, resolve it in
                        // the owning repository/package, then publish the
                        // selected alias edge before following the chain.
                        let alias_build_file = rush_parse::BuildFile {
                            path: build_file.path.clone(),
                            rules: vec![rule.clone()],
                        };
                        let mut context = rule_execution::InstantiationContext {
                            configuration_scope: self.config_registry_scope(),
                            repo_name: repository.clone(),
                            package: package.to_string(),
                            main_repository_name: self
                                .bzl_loader
                                .lock()
                                .ok()
                                .and_then(|loader| loader.main_workspace_name()),
                            repository_mapping: self.repository_mapping_for(
                                repository.as_deref(),
                                &repo_root,
                            ),
                            ..rule_execution::InstantiationContext::default()
                        };
                        let mut conditions =
                            rule_execution::build_file_raw_configuration_conditions(
                                &alias_build_file,
                                &context,
                            );
                        conditions.sort();
                        conditions.dedup();
                        for condition in conditions {
                            if let Some(label) = self.preload_configuration_metadata_target(
                                &relative(&condition), &repo_root, repository.as_deref(), metadata,
                            )? { alias_edges.insert(label); }
                        }
                        context.repository_mapping = self.repository_mapping_for(
                            repository.as_deref(),
                            &repo_root,
                        );
                        match rule_execution::resolve_parse_value_as_label(
                            rule.attributes.get("actual").expect("matched actual"), &context,
                        ) {
                            Ok(actual) => {
                                resolved_configurable_alias_actual = Some(actual.clone());
                                references.push(relative(&actual));
                            }
                            Err(error) => {
                                // Discovery still records every condition edge.
                                // Required flag proofs run before selecting an
                                // actual; on the next pass a matching no-default
                                // select can resolve. If no proof changes, return
                                // this original error rather than suppressing it.
                                metadata.deferred_alias_errors.push(error.context(
                                    format!("resolve configurable metadata alias {canonical_label}")));
                            }
                        }
                    }
                    _ => {}
                }
            }
            "config_setting" => {
                if let Some(Value::List(values)) = rule.attributes.get("constraint_values") {
                    references.extend(values.iter().filter_map(|value| match value {
                        Value::String(value) => Some(relative(value)),
                        _ => None,
                    }));
                }
                // Genuine configured Bazel analysis validates flag producers,
                // aliases, type and allowed domain. Do not execute those rule
                // implementations through Rush's ordinary dependency loader.

            }
            "constraint_value" => {
                if let Some(Value::String(setting)) = rule.attributes.get("constraint_setting") {
                    references.push(relative(setting));
                }
            }
            _ => {}
        }
        references.sort();
        references.dedup();
        for reference in references {
            if let Some(label) = self.preload_configuration_metadata_target(
                &reference, &repo_root, repository.as_deref(), metadata,
            )? {
                if rule.rule_type == "alias" { alias_edges.insert(label); }
            }
        }
        if let Some(actual) = resolved_configurable_alias_actual {
            let context = rule_execution::InstantiationContext {
                repo_name: repository.clone(),
                package: package.to_string(),
                main_repository_name: self
                    .bzl_loader
                    .lock()
                    .ok()
                    .and_then(|loader| loader.main_workspace_name()),
                repository_mapping: self.repository_mapping_for(
                    repository.as_deref(),
                    &repo_root,
                ),
                ..rule_execution::InstantiationContext::default()
            };
            let actual = rule_execution::canonicalize_configuration_reference(
                &actual,
                &context,
            );
            rush_starlark::config_defines::consider_config_alias(
                repository.as_deref(),
                package,
                target,
                &actual,
            );
        }
        // Loading the exact metadata closure may have populated an owner-scoped
        // apparent-to-canonical repository mapping. Replay this package's
        // declarations now so registry predicates and alias edges use the same
        // canonical identities the graph instances will receive.
        self.register_evaluated_configuration_rules(
            &build_file,
            &repo_root,
            repository.as_deref(),
            package,
        )?;
        metadata.edges.entry(canonical_label.clone()).or_default().extend(alias_edges);
        rush_starlark::config_defines::recompute_active_config_settings_public();
        Ok(Some(canonical_label))
    }

    /// Start a fresh pre-registration pass for the next command.
    fn reset_config_pre_registration(&self) {
        let scope = self.config_registry_scope();
        PRESCANNED_CONFIG_PACKAGES.lock().unwrap().remove(&scope);
        if let Ok(mut loader) = self.bzl_loader.lock() {
            loader.clear_command_repo_mappings();
        }
    }

    /// Return an external repository known to either orchestrator registry.
    ///
    /// Exact Bzlmod condition loading can materialize a canonical repository
    /// synchronously through `BzlLoader` before the async dependency walk sees
    /// it. Mirror that path into the orchestrator registry so the configured
    /// graph consumes the already-resolved identity instead of trying a second,
    /// context-free fetch.
    fn has_legacy_workspace(&self) -> bool {
        self.workspace_root.join("WORKSPACE").exists() || self.workspace_root.join("WORKSPACE.bazel").exists()
    }

    fn requires_declared_workspace_source(&self, name: &str) -> bool {
        self.requires_declared_workspace_source_for_path(name, None)
    }

    fn requires_declared_workspace_source_for_path(&self, name: &str, registered: Option<&Path>) -> bool {
        if name == "bazel_tools" { return false; }
        let Ok(loader) = self.bzl_loader.lock() else { return true; };
        let candidate = loader.preferred_registered_repository_path(name, registered);
        // A previously admitted WORKSPACE source keeps its proof obligation
        // even after its defining file disappears or the workspace switches
        // modes. Absence cannot turn managed bytes into an explicit root.
        if candidate.as_ref().is_some_and(|path| loader.is_managed_workspace_source(name, path)) {
            return true;
        }
        if !self.has_legacy_workspace() { return false; }
        match candidate {
            Some(path) => loader.is_implicit_system_adapter(name, &path)
                || self.repository_manager.get_http_archive(name).is_some(),
            None => !loader.is_main_workspace_repository(name),
        }
    }

    /// Registered user/local and genuine source roots own their target
    /// namespace. Compatibility synthesis must not hide a missing real target.
    fn has_registered_source_repository(&self, name: &str) -> bool {
        // Do not block a synchronous stub API on an async registry lock. A
        // busy registry supplies no authority to invent a compatibility node.
        let registered = match self.external_repo_paths.try_lock() {
            Ok(paths) => paths.get(name).cloned(),
            Err(_) => return true,
        };
        self.has_registered_source_repository_for_path(name, registered.as_deref())
    }

    fn has_registered_source_repository_for_path(&self, name: &str, registered: Option<&Path>) -> bool {
        let Ok(loader) = self.bzl_loader.lock() else { return true; };
        registered.is_some_and(|path| !loader.is_implicit_system_adapter(name, path))
            || loader.external_repo_path(name)
                .is_some_and(|path| !loader.is_implicit_system_adapter(name, &path))
    }

    async fn registered_external_repository_path(&self, name: &str, requested: &crate::dependency_graph::Label) -> Result<Option<PathBuf>> {
        let registered = self.external_repo_paths.lock().await.get(name).cloned();
        let candidate = self.bzl_loader.lock().map_err(|_| anyhow::anyhow!("BzlLoader lock poisoned"))?
            .preferred_registered_repository_path(name, registered.as_deref());
        if name == "bazel_tools" {
            let admitted = self.with_tracked_scope(|| {
                self.bzl_loader.lock().map_err(|_| anyhow::anyhow!("BzlLoader lock poisoned"))?
                    .admit_builtin_repository(name, candidate.as_deref())
            })?;
            if let Some(path) = &admitted {
                self.external_repo_paths.lock().await.insert(name.to_string(), path.clone());
            }
            return Ok(admitted);
        }
        let declared = self.repository_manager.get_http_archive(name);
        let implicit_or_missing = self.requires_declared_workspace_source_for_path(name, candidate.as_deref());
        if candidate.is_none() && self.has_legacy_workspace() && !implicit_or_missing && name != "bazel_tools" {
            let main = self.bzl_loader.lock().map_err(|_| anyhow::anyhow!("BzlLoader lock poisoned"))?
                .is_main_workspace_repository(name);
            if main { return Ok(Some(self.workspace_root.clone())); }
        }
        if declared.is_some() || implicit_or_missing {
            let mapping = declared.map(|archive| archive.repo_mapping).unwrap_or_default();
            return self.fetch_workspace_repository_with_bazel(name, &mapping, Some(requested)).await.map(Some);
        }
        if let Some(path) = candidate {
            self.external_repo_paths.lock().await.insert(name.to_string(), path.clone());
            return Ok(Some(path));
        }
        Ok(None)
    }

    /// Expand wildcard target patterns into concrete target labels.
    ///
    /// Supports:
    /// Parse a BUILD file with caching to avoid re-parsing.
    fn parse_build_file_cached(
        &self,
        build_path: &Path,
        repo_root: Option<&PathBuf>,
    ) -> Result<Arc<rush_parse::BuildFile>> {
        self.parse_build_file_cached_for_repository(build_path, repo_root, None)
    }

    fn parse_build_file_cached_for_repository(
        &self,
        build_path: &Path,
        repo_root: Option<&PathBuf>,
        repo_name: Option<&str>,
    ) -> Result<Arc<rush_parse::BuildFile>> {
        let scope = self.config_registry_scope();
        self.with_tracked_scope(|| {
            rush_starlark::config_defines::with_configuration_scope(&scope, || {
                self.parse_build_file_cached_scoped(build_path, repo_root, repo_name)
            })
        })
    }

    fn parse_build_file_cached_scoped(
        &self,
        build_path: &Path,
        repo_root: Option<&PathBuf>,
        repo_name: Option<&str>,
    ) -> Result<Arc<rush_parse::BuildFile>> {
        // Determine the actual repository root for this BUILD file
        let actual_repo_root = repo_root.unwrap_or(&self.workspace_root);
        let repository = repo_name
            .map(str::to_owned)
            .or_else(|| self.repository_name_for_root(actual_repo_root));

        let package = build_path
            .parent()
            .and_then(|p| p.strip_prefix(actual_repo_root).ok())
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| String::from(""));
        let _current_config_package = ConfigPackageEvaluationGuard::enter(
            Self::configuration_package_visit_key(
                actual_repo_root,
                repository.as_deref(),
                &package,
            ),
        );

        // Order-independent config_setting pre-registration. Rush preserves
        // select() symbolically while loading, then resolves it during rule
        // instantiation against the scoped active config_settings. Those
        // settings are otherwise populated only as a side effect of evaluating
        // each declaring rule. A select keyed on a
        // config_setting in ANOTHER package (e.g. TF's
        // `//tensorflow/lite:tflite_with_xnnpack_explicit_false`, referenced
        // from `tensorflow/lite/kernels/BUILD` but declared in
        // `tensorflow/lite/BUILD`) therefore resolved to the wrong branch
        // whenever the referencing package was evaluated FIRST — pulling
        // @XNNPACK under `--define tflite_with_xnnpack=false`. Before this BUILD
        // is parsed/evaluated, register its own config_settings AND those of any
        // package its select()s reference, so activation is a deterministic
        // function of the defines and the config_setting definitions rather than
        // of parse order (Bazel parity: config_settings resolve as part of
        // analysis, order-independently).
        self.pre_register_config_settings(
            build_path,
            actual_repo_root,
            &package,
            repository.as_deref(),
        )?;

        // Conservatively partition BUILD cache entries by active defines.
        // `select()` is now retained symbolically in the parsed BuildFile, but
        // older macro bridges can still materialize configuration-dependent
        // attributes while loading. Retain the active-defines fingerprint until
        // every compatibility bridge is configuration-independent, so a cache
        // entry can never cross command configurations. An empty fingerprint
        // keeps the ordinary default cache key unchanged.
        let defines_fp = rush_starlark::config_defines::defines_fingerprint();
        let repository_key = repository.as_deref().unwrap_or("<main>");
        let build_file_key = build_path
            .strip_prefix(actual_repo_root)
            .unwrap_or(build_path)
            .display();
        let cache_key = if defines_fp.is_empty() {
            format!(
                "{}::{}::{}",
                actual_repo_root.display(),
                repository_key,
                build_file_key
            )
        } else {
            format!(
                "{}::{}::{}::{}",
                actual_repo_root.display(),
                repository_key,
                build_file_key,
                defines_fp
            )
        };

        // Get or create cache entry (with placeholder for work-in-progress)
        // This prevents multiple concurrent tasks from parsing the same BUILD file
        let result_lock = {
            let mut cache = self.build_file_cache.lock().unwrap();
            cache
                .get_or_insert(cache_key.clone(), || {
                    Arc::new(tokio::sync::Mutex::new(None))
                })
                .clone()
        };

        // Lock the result and check if it's already populated
        let mut result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async { result_lock.lock().await })
        });

        if let Some(cached) = result.as_mut() {
            // A parsed BUILD is a value derived from its tracked inputs, not just
            // a bag of dependencies to replay. A retained snapshot can correctly
            // detect that a glob directory changed and request re-analysis, while
            // this lower cache still holds the pre-change expansion. Validate the
            // entry once when a new command's tracker first reaches it; replay is
            // safe only after that admission succeeds.
            let current_io = self.real_tracked_io_shared();
            let stale_reason = match current_io {
                Some(_) if cached.manifest.is_empty() => {
                    Some("cached BUILD has no tracked dependency manifest".to_string())
                }
                Some(ref io)
                    if cached
                        .admitted_io
                        .upgrade()
                        .is_some_and(|admitted| Arc::ptr_eq(&admitted, io)) =>
                {
                    None
                }
                Some(ref io) => {
                    let validated = io.validate_manifest(&cached.manifest);
                    match validated.staleness {
                        rush_incremental::Staleness::Fresh => {
                            cached.manifest = validated.refreshed;
                            cached.admitted_io = Arc::downgrade(io);
                            None
                        }
                        rush_incremental::Staleness::Stale { reason, .. } => Some(reason),
                    }
                }
                // Calls outside a command scope have no client semantics to
                // validate against. Daemon and normal builds install `tracked_io`.
                None => None,
            };

            if let Some(reason) = stale_reason {
                debug_log!("[BUILD CACHE STALE] {} - {}", cache_key, reason);
                *result = None;
            } else {
                debug_log!("[BUILD CACHE HIT] {}", cache_key);
                // Replay only an admitted manifest. Replaying a stale observation
                // would poison the new command's manifest and retain the wrong
                // parsed value for glob and package-boundary decisions.
                let dep_set = cached.manifest.deps();
                self.with_tracked_scope(|| rush_incremental::tracked::replay_deps(dep_set));
                self.register_evaluated_configuration_rules(
                    &cached.build_file,
                    actual_repo_root,
                    repository.as_deref(),
                    &package,
                )?;
                return Ok(Arc::clone(&cached.build_file));
            }
        }

        // We're the first task to request this BUILD file - parse it
        debug_log!("[BUILD CACHE MISS] {} - parsing now", cache_key);
        // Mark the current command's tracked manifest before parsing so we can
        // capture this BUILD file's full dependency set on the (miss) parse path.
        // `.bzl` loads nested under the parse contribute their own dep_sets (a
        // nested `.bzl` cache hit replays into the same manifest, a miss records),
        // so the mark-diff is the BUILD file's complete transitive set. `None`
        // outside a command scope (capture then a no-op store). Installed via
        // `with_tracked_scope` so direct callers (no outer scope) still see the IO.
        let dep_mark = self.with_tracked_scope(rush_incremental::tracked::mark);

        // Pressure-gated resource acquisition. Off-CPU profiling with
        // perf on `//tensorflow:tensorflow_cc` traced ~35% of main-
        // thread wall-clock to the original unconditional
        // `block_in_place(block_on(acquire(..)))` pair even at 0% PSI
        // memory pressure — tokio's block_in_place parks the worker,
        // block_on runs the async acquire, unparks. Every BUILD parse
        // paid that round-trip twice (acquire + release).
        //
        // Fast path: query the PSI watcher channel (a cheap
        // `watch::Receiver::borrow`, no syscall, no tokio park). If
        // the monitor hasn't flagged pressure, skip the acquire
        // entirely — BUILD parsing is parser-bound I/O, each .bzl's
        // AST gets bounded allocation via the cache's LRU eviction,
        // and the GlobalMemoryBudget in the starlark evaluator
        // already caps coarse memory separately.
        //
        // Slow path: under pressure, fall through to the full
        // `resource_manager.acquire(ResourceRequest::minimal())` so
        // the 500 ms / 200 ms backoff loops still throttle parsing
        // when the system is genuinely thrashing.
        let _resource_handle = if self.resource_manager.is_under_pressure() {
            debug_log!(
                "[DEBUG] PSI pressure flagged — acquiring resources for BUILD parse: {}",
                build_path.display()
            );
            Some(tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    self.resource_manager
                        .acquire(rush_resource::ResourceRequest::minimal())
                        .await
                })
            })?)
        } else {
            None
        };

        // Create a NEW BzlLoader instance for this BUILD file (no lock contention!)
        // Each instance has its own mutable state but shares the GlobalBzlCache
        let mut bzl_loader = {
            let loader_guard = self.bzl_loader.lock().unwrap();
            loader_guard.clone_for_parallel_parsing()
        };

        // Set the current_repo_root before parsing
        // This ensures that // labels in .bzl files resolve relative to the correct repository
        bzl_loader.set_current_repo_root(actual_repo_root.clone());
        bzl_loader.set_current_repo_name(repository.clone());

        // Use full Starlark parser with .bzl loader support (lock-free!)
        debug_log!(
            "[DEBUG] Calling parse_build_file_with_loader for {} (lock-free)",
            build_path.display()
        );
        // Persistent-daemon brick 1b (scope-coverage fix): install the per-command
        // tracked analysis IO for THIS synchronous parse region, here at the parse
        // boundary itself rather than at each call site. `parse_build_file_cached`
        // is reached from ~10 synchronous main-thread regions (initial loading, the
        // missing-dependency / stub-synthesis loop in `build_with_dependencies`, and
        // several resolve/register helpers), only one of which was wrapped — the
        // others reached `parse_build_file_with_loader`'s fail-loud
        // `tracked::read_to_string` outside any scope and panicked at TF scale
        // (e.g. while chasing @com_google_absl//absl/strings). Installing the scope
        // intrinsically to the parse covers every caller uniformly. `tracked::scope`
        // nests (an outer `with_tracked_scope` is restored on exit), and the
        // `spawn_blocking` boundary (which calls `parse_build_file_with_loader`
        // directly via `tracked::bind`/`scope`) is unaffected because it does not go
        // through this function.
        let build_file = self
            .with_tracked_scope(|| rush_parse::parse_build_file_with_loader(build_path, bzl_loader))?;
        self.register_evaluated_configuration_rules(
            &build_file,
            actual_repo_root,
            repository.as_deref(),
            &package,
        )?;
        debug_log!("[DEBUG] parse_build_file_with_loader completed");

        // Slow-path cleanup: if we acquired a ResourceHandle under
        // pressure, release it explicitly. Handle has no `Drop` impl;
        // letting it fall out of scope would leak the `allocated_memory_mb`
        // bookkeeping in ResourceManager (the OwnedSemaphorePermit's
        // own Drop still frees the CPU slot, but the state.allocated
        // stays inflated until process exit).
        if let Some(handle) = _resource_handle {
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current()
                    .block_on(async { handle.release_async().await })
            });
        }

        // Persistent-daemon Run-wiring (blocker #1): capture this BUILD file's
        // full dependency set (everything recorded since `dep_mark`) and store it
        // in the cache entry so a cross-command HIT can replay it. Empty when no
        // command scope is active. `with_tracked_scope` installs the IO so
        // `captured_since` resolves the same shared manifest the parse recorded into.
        let dep_set = match dep_mark {
            Some(mark) => {
                self.with_tracked_scope(|| rush_incremental::tracked::captured_since(mark))
            }
            None => Vec::new(),
        };
        let manifest = rush_incremental::DependencyManifest::from_deps(dep_set);
        let admitted_io = self
            .real_tracked_io_shared()
            .map(|io| Arc::downgrade(&io))
            .unwrap_or_default();

        // Store the parsed result in the cache entry (this will unblock other waiting tasks)
        *result = Some(CachedBuildFile {
            build_file: Arc::clone(&build_file),
            manifest,
            admitted_io,
        });
        debug_log!("[BUILD CACHE STORE] {}", cache_key);

        Ok(build_file)
    }

    /// Parse a BUILD file and instantiate all rules as RuleInstances.
    ///
    /// This method combines parsing with rule instantiation, creating executable
    /// RuleInstance objects that can generate build actions.
    /// Persistent-daemon brick 1b: clone the per-command tracked analysis IO as a
    /// `SharedTrackedIo` (the `dyn TrackedIo` trait object the `tracked::scope`
    /// API installs), or `None` if no command has installed one (e.g. unit tests
    /// or analysis entered through a path other than `build_with_dependencies`).
    fn real_tracked_io_shared(&self) -> Option<Arc<rush_incremental::RealTrackedIo>> {
        self.tracked_io
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub(crate) fn tracked_io_shared(&self) -> Option<rush_incremental::tracked::SharedTrackedIo> {
        self.real_tracked_io_shared()
            .map(|arc| arc as rush_incremental::tracked::SharedTrackedIo)
    }

    /// Run `f` with the per-command tracked analysis IO installed as the current
    /// thread's `tracked::scope`, so every `tracked::*` read inside `f` records
    /// into the one command manifest. If no IO is installed, runs `f` unscoped —
    /// any `tracked::*` read inside then fail-loud panics (catching an analysis
    /// path that runs outside a command, by design).
    ///
    /// This is the synchronous-region installer: `build_with_dependencies` is
    /// async (the thread-local cannot survive `.await`), so the scope is installed
    /// right around each synchronous parse region rather than across the whole
    /// async body.
    pub(crate) fn with_tracked_scope<R>(&self, f: impl FnOnce() -> R) -> R {
        match self.tracked_io_shared() {
            Some(io) => rush_incremental::tracked::scope(io, f),
            None => f(),
        }
    }

    /// Run one synchronous analysis region in this orchestrator's
    /// configuration registry. Unlike a thread-local inherited by accident,
    /// this helper can be re-entered explicitly on blocking worker threads.
    fn with_config_registry_scope<R>(&self, f: impl FnOnce() -> R) -> R {
        let scope = self.config_registry_scope();
        rush_starlark::config_defines::with_configuration_scope(&scope, f)
    }

    fn repository_mapping_for(
        &self,
        repository: Option<&str>,
        repo_root: &Path,
    ) -> HashMap<String, String> {
        self.bzl_loader
            .lock()
            .map(|loader| loader.repository_mapping_for_owner(repository, repo_root))
            .unwrap_or_default()
    }

    /// Install a per-command tracked analysis IO if none has been installed yet.
    ///
    /// `build_with_dependencies` installs one explicitly; this is the equivalent
    /// for the alternate analysis entry points (`build`, `test`) so the migrated
    /// "what-exists" discovery reads (target-pattern expansion's package walk /
    /// BUILD existence probes) have an IO to record into instead of fail-loud
    /// panicking. Idempotent: it never clobbers an already-installed IO.
    pub(crate) fn ensure_tracked_io(&self) {
        let mut slot = self
            .tracked_io
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if slot.is_none() {
            let semantics = rush_command_core::CommandSemantics::from_process();
            *slot = Some(std::sync::Arc::new(rush_incremental::RealTrackedIo::new(
                semantics,
                &self.workspace_root,
            )));
        }
    }

    /// Test-only: install a FRESH per-command tracked analysis IO, replacing any
    /// existing one, and return the shared `Arc` so the test can snapshot its
    /// manifest after analysis. Used by the cross-command cache-replay correctness
    /// proof to simulate two distinct commands hitting the same persistent caches.
    #[doc(hidden)]
    pub fn install_fresh_tracked_io_for_test(
        &self,
    ) -> std::sync::Arc<rush_incremental::RealTrackedIo> {
        let semantics = rush_command_core::CommandSemantics::from_process();
        let io = std::sync::Arc::new(rush_incremental::RealTrackedIo::new(
            semantics,
            &self.workspace_root,
        ));
        let mut slot = self
            .tracked_io
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        *slot = Some(std::sync::Arc::clone(&io));
        io
    }

    pub fn instantiate_build_file(
        &self,
        build_path: &Path,
        repo_root: Option<&PathBuf>,
    ) -> Result<Vec<Box<dyn rush_rules::RuleInstance>>> {
        self.instantiate_build_file_for_repository(build_path, repo_root, None)
    }

    pub(crate) fn instantiate_build_file_for_repository(
        &self,
        build_path: &Path,
        repo_root: Option<&PathBuf>,
        repo_name: Option<&str>,
    ) -> Result<Vec<Box<dyn rush_rules::RuleInstance>>> {
        let (instances, _) = self
            .instantiate_build_file_with_subphases_for_repository(
                build_path,
                repo_root,
                repo_name,
            )?;
        Ok(instances)
    }

    pub(crate) fn instantiate_build_file_for_repository_targets(
        &self,
        build_path: &Path,
        repo_root: Option<&PathBuf>,
        repo_name: Option<&str>,
        target_names: &std::collections::HashSet<String>,
    ) -> Result<Vec<Box<dyn rush_rules::RuleInstance>>> {
        let (instances, _) = self
            .instantiate_build_file_with_subphases_for_repository_targets(
                build_path,
                repo_root,
                repo_name,
                Some(target_names),
            )?;
        Ok(instances)
    }

    /// Phase 5.C.4 commit 90: variant of
    /// [`instantiate_build_file`] that reports subphase
    /// timing alongside the result. Returns
    /// `(instances, (starlark_eval_duration, instantiate_duration))`.
    ///
    /// Pro's commit-90 directive: split `build_file_parse`
    /// into smaller phase timers. Today's coarsest useful
    /// split is `starlark_eval` (parse_build_file_cached)
    /// vs `instantiate` (build_file_to_instances). Finer
    /// breakdown requires rush-starlark-side
    /// instrumentation.
    pub fn instantiate_build_file_with_subphases(
        &self,
        build_path: &Path,
        repo_root: Option<&PathBuf>,
    ) -> Result<(
        Vec<Box<dyn rush_rules::RuleInstance>>,
        (std::time::Duration, std::time::Duration),
    )> {
        self.instantiate_build_file_with_subphases_for_repository(
            build_path,
            repo_root,
            None,
        )
    }

    pub(crate) fn instantiate_build_file_with_subphases_for_repository(
        &self,
        build_path: &Path,
        repo_root: Option<&PathBuf>,
        repo_name: Option<&str>,
    ) -> Result<(
        Vec<Box<dyn rush_rules::RuleInstance>>,
        (std::time::Duration, std::time::Duration),
    )> {
        self.instantiate_build_file_with_subphases_for_repository_targets(
            build_path,
            repo_root,
            repo_name,
            None,
        )
    }

    pub(crate) fn instantiate_build_file_with_subphases_for_repository_targets(
        &self,
        build_path: &Path,
        repo_root: Option<&PathBuf>,
        repo_name: Option<&str>,
        target_names: Option<&std::collections::HashSet<String>>,
    ) -> Result<(
        Vec<Box<dyn rush_rules::RuleInstance>>,
        (std::time::Duration, std::time::Duration),
    )> {
        let actual_repo_root = repo_root.unwrap_or(&self.workspace_root);
        let effective_repository = repo_name
            .map(str::to_owned)
            .or_else(|| self.repository_name_for_root(actual_repo_root));
        let package = build_path
            .parent()
            .and_then(|parent| parent.strip_prefix(actual_repo_root).ok())
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default();
        let starlark_start = std::time::Instant::now();
        // Parse the BUILD file using Starlark evaluation.
        // Persistent-daemon brick 1b: install the per-command tracked analysis IO
        // for this synchronous parse region so the BUILD content read (rush-parse)
        // and the `.bzl` source reads it triggers record into the command
        // manifest. (`build_with_dependencies` is async; the scope must wrap the
        // synchronous region, not span an `.await`.)
        let build_file = self.with_tracked_scope(|| {
            self.parse_build_file_cached_for_repository(
                build_path,
                repo_root,
                effective_repository.as_deref(),
            )
        })?;
        let configured_targets = self.with_tracked_scope(|| {
            self.pre_register_evaluated_select_conditions(
                &build_file,
                actual_repo_root,
                effective_repository.as_deref(),
                &package,
                target_names,
            )
        })?;
        let starlark_dur = starlark_start.elapsed();
        let main_repository_name = self
            .bzl_loader
            .lock()
            .ok()
            .and_then(|loader| loader.main_workspace_name());

        let inst_start = std::time::Instant::now();
        // Convert parsed rules to RuleInstance objects using the registry.
        // Thread an InstantiationContext so the Bzlmod @protobuf path can re-route
        // its `CustomRule` proto_library/cc_proto_library (the repo's .bzl shadows
        // the native names) to the real native rules while attrs (srcs) are live.
        //
        // Persistent-daemon TrackedIo migration (env slice): instantiation reads
        // analysis-affecting env (`RUSH_ENABLE_BZLMOD` here, `RUSH_ENABLE_ROCM` in
        // `parse_rule_to_instance_with_context`'s gpu_srcs fold) through the
        // fail-loud `tracked::getenv`. The PARSE region above is scoped, but the
        // instantiate region is reached from callers that are NOT all under a
        // scope (e.g. `instantiate_build_file`), so wrap it intrinsically here —
        // mirroring the parse-boundary install above. `with_tracked_scope` nests
        // and reuses an already-installed IO.
        let scope = self.config_registry_scope();
        let mut instances = self.with_tracked_scope(|| {
            rush_starlark::config_defines::with_configuration_scope(&scope, || {
                let bzlmod =
                    rush_incremental::tracked::getenv_str("RUSH_ENABLE_BZLMOD").as_deref()
                        == Some("1");
                let inst_ctx = rule_execution::InstantiationContext {
                    bzlmod,
                    configuration_scope: scope.clone(),
                    // Native cc_proto_library currently has no configured
                    // ProtoLangToolchainInfo or exec-transition consumer. Once
                    // an analyzer is selected, refuse that lane explicitly
                    // rather than substituting global/bootstrap protoc.
                    native_proto_toolchain_error: if build_file.rules.iter().any(|rule| rule.rule_type == "cc_proto_library"
                        || (bzlmod && rule.rule_type == "CustomRule" && rule.name.ends_with("_cc_proto"))) {
                        match rush_starlark::BzlLoader::selected_repository_analysis_binary(&self.workspace_root) {
                            Ok(Some(_)) => Some(rush_rules::rules_proto_toolchain::UNSUPPORTED_PROTO_TOOLCHAIN.to_string()),
                            Ok(None) => None,
                            Err(error) => Some(format!("native cc_proto_library analyzer selection failed: {error:#}")),
                        }
                    } else { None },
                    repo_name: effective_repository.clone(),
                    package: package.clone(),
                    main_repository_name: main_repository_name.clone(),
                    repository_mapping: self.repository_mapping_for(
                        effective_repository.as_deref(),
                        actual_repo_root,
                    ),
                };
                rule_execution::build_file_to_instances_with_context(
                    &build_file,
                    &self.rule_registry,
                    &inst_ctx,
                )
            })
        })?;
        if target_names.is_some() {
            instances.retain(|instance| configured_targets.contains(instance.target_name()));
        }
        let inst_dur = inst_start.elapsed();

        Ok((instances, (starlark_dur, inst_dur)))
    }

    /// - `//...` - all targets in all packages
    /// - `//pkg:all` - all targets in a specific package
    /// - `//pkg/sub/...` - all targets in pkg/sub and subdirectories
    pub fn expand_targets(&self, patterns: &[String]) -> Result<Vec<String>> {
        // Persistent-daemon TrackedIo migration: package discovery here walks the
        // filesystem (`find_*_targets`/`walk_for_build_files`) through `tracked::*`,
        // which is fail-loud outside a scope. The command path installs the IO in
        // `build_with_dependencies`; for the alternate analysis/query entry points
        // (`build`/`test`/CLI query) install one here so the discovery reads are
        // recorded rather than panicking. `ensure_tracked_io` is idempotent and
        // `with_tracked_scope` reuses an already-installed IO.
        self.ensure_tracked_io();
        self.with_tracked_scope(|| self.expand_targets_inner(patterns))
    }

    fn expand_targets_inner(&self, patterns: &[String]) -> Result<Vec<String>> {
        let mut expanded = Vec::new();

        for pattern in patterns {
            // Handle external repository targets: @repo//...
            if pattern.starts_with('@') {
                expanded.extend(self.expand_external_repo_targets(pattern)?);
                continue;
            }

            if pattern == "//..." {
                // Find all BUILD files recursively
                expanded.extend(self.find_all_targets()?);
            } else if pattern.ends_with(":all") {
                // Find all targets in a specific package
                let package = pattern
                    .strip_suffix(":all")
                    .and_then(|p| p.strip_prefix("//"))
                    .ok_or_else(|| anyhow::anyhow!("Invalid :all pattern: {}", pattern))?;
                expanded.extend(self.find_package_targets(package)?);
            } else if pattern.ends_with("/...") {
                // Find all targets in package and subdirectories
                let package_prefix = pattern
                    .strip_suffix("/...")
                    .and_then(|p| p.strip_prefix("//"))
                    .ok_or_else(|| anyhow::anyhow!("Invalid /... pattern: {}", pattern))?;
                expanded.extend(self.find_recursive_targets(package_prefix)?);
            } else {
                // Regular target label
                expanded.push(pattern.clone());
            }
        }

        Ok(expanded)
    }

    /// Expand external repository target patterns.
    ///
    /// Examples:
    /// - `@repo//...` - all targets in external repository
    /// - `@repo//package:all` - all targets in a package
    /// - `@repo//package:target` - specific target
    fn expand_external_repo_targets(&self, pattern: &str) -> Result<Vec<String>> {
        // Parse @repo//rest
        if !pattern.starts_with('@') {
            anyhow::bail!("External repo pattern must start with '@': {}", pattern);
        }

        let without_at = &pattern[1..];
        let slash_pos = without_at
            .find("//")
            .ok_or_else(|| anyhow::anyhow!("External repo pattern missing '//': {}", pattern))?;

        let repo_name = &without_at[..slash_pos];
        let rest = &without_at[slash_pos + 2..]; // Skip the //

        // Look up repository path
        let repo_paths = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.external_repo_paths.lock())
        });

        let repo_path = repo_paths.get(repo_name).ok_or_else(|| {
            anyhow::anyhow!(
                "External repository '{}' not found. Available repos: {:?}",
                repo_name,
                repo_paths.keys().collect::<Vec<_>>()
            )
        })?;

        // Now handle the pattern within the external repository
        let mut targets = Vec::new();

        if rest == "..." {
            // @repo//... - all targets in the repo
            targets.extend(self.find_external_repo_all_targets(repo_name, repo_path)?);
        } else if rest.ends_with(":all") {
            // @repo//package:all - all targets in package
            let package = rest
                .strip_suffix(":all")
                .ok_or_else(|| anyhow::anyhow!("Invalid :all pattern: {}", pattern))?;
            targets.extend(self.find_external_repo_package_targets(repo_name, repo_path, package)?);
        } else if rest.ends_with("/...") {
            // @repo//package/... - recursive targets
            let package_prefix = rest
                .strip_suffix("/...")
                .ok_or_else(|| anyhow::anyhow!("Invalid /... pattern: {}", pattern))?;
            targets.extend(self.find_external_repo_recursive_targets(
                repo_name,
                repo_path,
                package_prefix,
            )?);
        } else {
            // @repo//package:target - specific target
            targets.push(pattern.to_string());
        }

        Ok(targets)
    }

    /// Find all targets in an external repository.
    fn find_external_repo_all_targets(
        &self,
        repo_name: &str,
        repo_path: &Path,
    ) -> Result<Vec<String>> {
        self.find_external_repo_recursive_targets(repo_name, repo_path, "")
    }

    /// Find all targets in a specific package in an external repository.
    fn find_external_repo_package_targets(
        &self,
        repo_name: &str,
        repo_path: &Path,
        package: &str,
    ) -> Result<Vec<String>> {
        let Some(build_path) = find_build_file(&repo_path.join(package)) else {
            return Ok(Vec::default());
        };

        // Use macro-aware parsing to properly evaluate TensorFlow macros
        let build_file = self.with_config_registry_scope(|| {
            parse_build_file_with_bzl_loader(
                &build_path,
                Some(Arc::clone(&self.bzl_loader)),
            )
        })?;
        let targets = build_file
            .rules
            .iter()
            .map(|rule| format!("@{}//{}:{}", repo_name, package, rule.name))
            .collect();

        Ok(targets)
    }

    /// Find all targets recursively in an external repository.
    fn find_external_repo_recursive_targets(
        &self,
        repo_name: &str,
        repo_path: &Path,
        package_prefix: &str,
    ) -> Result<Vec<String>> {
        let mut targets = Vec::new();
        let search_root = if package_prefix.is_empty() {
            repo_path.to_path_buf()
        } else {
            repo_path.join(package_prefix)
        };

        self.walk_external_repo_for_build_files(
            repo_name,
            repo_path,
            &search_root,
            package_prefix,
            &mut targets,
        )?;

        Ok(targets)
    }

    /// Recursively walk external repository directory tree finding BUILD files.
    ///
    /// # Clippy Suppression
    ///
    /// `only_used_in_recursion`: The `package_prefix` parameter is only used to pass through
    /// to recursive calls. This is intentional as it maintains context across the recursion tree.
    /// The parameter could be refactored out, but it would complicate the function signature.
    #[allow(clippy::only_used_in_recursion)]
    fn walk_external_repo_for_build_files(
        &self,
        repo_name: &str,
        repo_root: &Path,
        dir: &Path,
        package_prefix: &str,
        targets: &mut Vec<String>,
    ) -> Result<()> {
        use std::fs;

        if !dir.is_dir() {
            return Ok(());
        }

        if let Some(build_path) = find_build_file(dir) {
            // Calculate package path relative to repo root
            let package = if package_prefix.is_empty() {
                dir.strip_prefix(repo_root)
                    .unwrap_or(dir)
                    .to_string_lossy()
                    .to_string()
            } else {
                let rel = dir.strip_prefix(repo_root).unwrap_or(dir);
                rel.to_string_lossy().to_string()
            };

            // Parse BUILD file with macro support and add all targets with @repo prefix
            if let Ok(build_file) = self.with_config_registry_scope(|| {
                parse_build_file_with_bzl_loader(
                    &build_path,
                    Some(Arc::clone(&self.bzl_loader)),
                )
            }) {
                for rule in &build_file.rules {
                    targets.push(format!("@{}//{}:{}", repo_name, package, rule.name));
                }
            }
        }

        // Recurse into subdirectories (but skip hidden dirs and output dirs)
        for entry_name in rush_incremental::tracked::read_dir_names(dir)? {
            let path = dir.join(&entry_name);
            if path.is_dir() {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

                // Skip hidden directories and build output directories
                if !name.starts_with('.') && name != "rush-out" && name != "bazel-out" {
                    self.walk_external_repo_for_build_files(
                        repo_name,
                        repo_root,
                        &path,
                        package_prefix,
                        targets,
                    )?;
                }
            }
        }

        Ok(())
    }

    /// Find all targets in the workspace.
    fn find_all_targets(&self) -> Result<Vec<String>> {
        self.find_recursive_targets("")
    }

    /// Find all targets in a specific package.
    fn find_package_targets(&self, package: &str) -> Result<Vec<String>> {
        // Check for both BUILD.bazel and BUILD (BUILD.bazel takes precedence in Bazel).
        // Persistent-daemon TrackedIo migration: these `:all`-expansion existence
        // probes pick which package file is parsed (or whether the package is
        // empty), so they are recorded `FileExistence` deps — a later-created
        // BUILD file must invalidate. Caller runs this inside `with_tracked_scope`.
        let Some(build_path) = self.preferred_build_file(&self.workspace_root.join(package)) else {
            return Ok(Vec::default());
        };

        // Use Starlark evaluator to properly handle list comprehensions and macros
        let build_file = self.parse_build_file_cached(&build_path, None)?;
        let targets = build_file
            .rules
            .iter()
            .map(|rule| format!("//{}:{}", package, rule.name))
            .collect();

        Ok(targets)
    }

    /// Find all targets recursively from a package prefix.
    fn find_recursive_targets(&self, package_prefix: &str) -> Result<Vec<String>> {
        let mut targets = Vec::new();
        let search_root = if package_prefix.is_empty() {
            self.workspace_root.clone()
        } else {
            self.workspace_root.join(package_prefix)
        };

        // Walk directory tree looking for BUILD files
        self.walk_for_build_files(&search_root, package_prefix, &mut targets)?;

        Ok(targets)
    }

    /// Recursively walk directory tree finding BUILD files.
    fn walk_for_build_files(
        &self,
        dir: &Path,
        package_prefix: &str,
        targets: &mut Vec<String>,
    ) -> Result<()> {
        // Persistent-daemon TrackedIo migration: `//...` / recursive target
        // DISCOVERY is a "what-exists" analysis read — a newly-created BUILD file
        // or directory must change the discovered target set and invalidate a
        // reused graph. Every directory listing is therefore recorded as a
        // `DirListing` dependency (via `tracked::read_dir_names`) and the BUILD /
        // BUILD.bazel and is-dir probes as `FileExistence` deps. Callers run this
        // inside `with_tracked_scope`.
        if !matches!(
            rush_incremental::tracked::metadata_kind(dir),
            Some(rush_incremental::FileKind::Dir)
        ) {
            return Ok(());
        }

        if let Some(build_path) = self.preferred_build_file(dir) {
            // Calculate package path
            let package = if package_prefix.is_empty() {
                dir.strip_prefix(&self.workspace_root)
                    .unwrap_or(dir)
                    .to_string_lossy()
                    .to_string()
            } else {
                let rel = dir.strip_prefix(&self.workspace_root).unwrap_or(dir);
                rel.to_string_lossy().to_string()
            };

            // Lightweight parse: extract target info without loading .bzl files
            // This is much faster for query operations on large codebases
            if let Ok(target_info) = Self::extract_target_info_lightweight(&build_path) {
                for (name, _rule_type) in target_info {
                    targets.push(format!("//{}:{}", package, name));
                }
            }

            // Check memory after each BUILD file to prevent OOM on large projects
            self.memory_guard.check_or_abort()?;
        }

        // Recurse into subdirectories (but skip hidden dirs and output dirs).
        // The listing is recorded as a `DirListing` dep so an added/removed
        // subdirectory invalidates.
        for entry_name in rush_incremental::tracked::read_dir_names(dir)? {
            let name = match entry_name.to_str() {
                Some(n) => n,
                None => continue,
            };
            // Skip hidden directories and build output directories before probing
            // type (avoids descending into the output tree).
            if name.starts_with('.') || name == "rush-out" || name == "bazel-out" {
                continue;
            }
            let path = dir.join(name);
            if matches!(
                rush_incremental::tracked::metadata_kind(&path),
                Some(rush_incremental::FileKind::Dir)
            ) {
                self.walk_for_build_files(&path, package_prefix, targets)?;
            }
        }

        Ok(())
    }

    /// Lightweight extraction of target info from BUILD file without loading .bzl files.
    ///
    /// Returns (name, rule_type) tuples extracted from the BUILD file AST.
    /// This is used by query operations to quickly scan BUILD files without the overhead
    /// of loading and evaluating all .bzl dependencies. Much faster on large codebases.
    pub fn extract_target_info_lightweight(build_path: &Path) -> Result<Vec<(String, String)>> {
        use rush_starlark::ast::{Expr, Stmt};

        let content = std::fs::read_to_string(build_path)?;
        let program = rush_starlark::parse(&content)?;

        let mut targets = Vec::new();

        for stmt in &program.statements {
            // Look for function calls (rule declarations)
            if let Stmt::Expr(expr) = stmt {
                if let Expr::Call(call) = expr.as_ref() {
                    // Get the rule type (function name)
                    let rule_type = if let Expr::Ident(func_name) = call.function.as_ref() {
                        func_name.clone()
                    } else {
                        continue;
                    };

                    // Extract name attribute from kwargs
                    for (key, value) in &call.kwargs {
                        if key == "name" {
                            if let Expr::String(name) = value {
                                targets.push((name.clone(), rule_type.clone()));
                                break;
                            }
                        }
                    }
                }
            }
        }

        Ok(targets)
    }

    /// Execute a build request.
    pub async fn build(&self, request: BuildRequest) -> Result<()> {
        self.build_with_dependencies(request.targets).await
    }

    // Retained temporarily for source-level comparison while callers migrate;
    // it is deliberately private so every production BuildRequest follows the
    // scoped, target-closure-aware dependency-graph path above.
    #[allow(dead_code)]
    async fn build_legacy(&self, request: BuildRequest) -> Result<()> {
        use std::time::Instant;
        let build_start = Instant::now();

        // Start metrics collection
        self.metrics.start_build();

        // Set resource manager for parallelization metrics
        self.memory_guard
            .set_resource_manager(Arc::clone(&self.resource_manager));

        // Start memory guard monitoring in the background
        let _memory_monitor_handle = self.memory_guard.start_monitoring();

        // Load WORKSPACE and fetch external dependencies
        self.load_workspace(true).await?;

        eprintln!("🔨 Rush Build Starting");
        eprintln!("Workspace: {}", self.workspace_root.display());
        debug_log!("[DEBUG] About to print targets");
        eprintln!("Targets: {:?}", request.targets);
        debug_log!("[DEBUG] Targets printed");

        if request.targets.is_empty() {
            anyhow::bail!("No targets specified");
        }

        // Emit build started event
        debug_log!("[DEBUG] About to emit build started event");
        let build_id = format!("build_{}", BuildEventStream::timestamp());
        self.events
            .build_started(build_id.clone(), request.targets.clone());
        debug_log!("[DEBUG] Build started event emitted");

        // Expand wildcard patterns
        debug_log!("[DEBUG] About to expand targets");
        let expand_start = Instant::now();
        let _expand_scope = self.profiler.scope("expand_targets", "analysis");
        // (expand_targets self-installs the tracked analysis IO and records its
        // package-discovery reads — see its definition.)
        let expanded_targets = self.expand_targets(&request.targets)?;
        debug_log!(
            "[DEBUG] Targets expanded: {} targets",
            expanded_targets.len()
        );
        drop(_expand_scope);
        self.metrics
            .record_phase("analysis", expand_start.elapsed());

        if expanded_targets.is_empty() {
            anyhow::bail!("No targets found matching patterns: {:?}", request.targets);
        }

        eprintln!("\n📋 Building {} targets", expanded_targets.len());

        // Phase 1: Parse all BUILD files and collect target metadata
        struct TargetBuildInfo {
            target_label: String,
            build_file: Arc<rush_parse::BuildFile>,
            rule: rush_parse::Rule,
            package: String,
            workspace_root: PathBuf,
            start_time: Instant,
        }

        // PARALLEL BUILD FILE PARSING - Use all CPU cores!
        // Instead of sequential loop, parse all BUILD files concurrently
        let total_targets = expanded_targets.len();
        eprintln!("\n⚡ Parsing {} BUILD files in parallel...", total_targets);

        use futures::stream::{self, FuturesUnordered, StreamExt};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let parsed_count = Arc::new(AtomicUsize::new(0));

        let parse_results = stream::iter(expanded_targets.iter().cloned())
            .map(|target| {
                let workspace_root = self.workspace_root.clone();
                let external_repo_paths = Arc::clone(&self.external_repo_paths);
                let memory_guard = self.memory_guard.clone();
                let events = self.events.clone();
                let counter = Arc::clone(&parsed_count);
                let total = total_targets;

                async move {
                    let (package, name) = parse_target_label(&target)?;

                    // Resolve workspace root based on whether this is an external repository
                    let (workspace_root, repository) = if target.starts_with('@') {
                        if let Some(slash_pos) = target.find("//") {
                            let repo_name = &target[1..slash_pos];
                            let repo_paths = external_repo_paths.lock().await;
                            (
                                repo_paths
                                    .get(repo_name)
                                    .cloned()
                                    .unwrap_or(workspace_root),
                                Some(repo_name.to_string()),
                            )
                        } else {
                            (workspace_root, None)
                        }
                    } else {
                        (workspace_root, None)
                    };

                    // FIX #188: Find and parse BUILD file (supports both BUILD and BUILD.bazel)
                    let package_dir = workspace_root.join(&package);
                    let build_path = match find_build_file(&package_dir) {
                        Some(path) => path,
                        None => {
                            let build = package_dir.join("BUILD");
                            let build_bazel = package_dir.join("BUILD.bazel");
                            debug_log!(
                                "⚠️  BUILD file not found for dependency: {} (checked {} and {})",
                                target,
                                build.display(),
                                build_bazel.display()
                            );
                            return Err(anyhow::anyhow!(
                                "BUILD file not found for dependency: {} (checked {} and {})",
                                target,
                                build.display(),
                                build_bazel.display()
                            ));
                        }
                    };

                    // Check memory pressure before parsing
                    memory_guard.check_or_abort()?;

                    // Use the same scoped, condition-aware cached analysis path
                    // as dependency builds. The old direct evaluator bypassed
                    // condition-package discovery and could choose a default
                    // branch merely because `rush run` parsed on a worker.
                    let build_file = self.parse_build_file_cached_for_repository(
                        &build_path,
                        Some(&workspace_root),
                        repository.as_deref(),
                    )?;

                    // Filter to only the requested target
                    let filtered_rules: Vec<_> = build_file
                        .rules
                        .iter()
                        .filter(|r| r.name == name)
                        .cloned()
                        .collect();

                    if filtered_rules.is_empty() {
                        return Err(anyhow::anyhow!(
                            "Target '{}' not found in BUILD file",
                            target
                        ));
                    }

                    let build_file = Arc::new(rush_parse::BuildFile {
                        path: build_file.path.clone(),
                        rules: filtered_rules,
                    });

                    let rule = build_file.rules[0].clone();

                    // Emit target started event
                    events.target_started(target.clone(), rule.rule_type.clone());

                    // Update progress counter
                    let count = counter.fetch_add(1, Ordering::Relaxed) + 1;
                    if count % 100 == 0 || count == total {
                        eprintln!("  📋 Parsed {}/{} BUILD files...", count, total);
                    }

                    Ok::<_, anyhow::Error>(TargetBuildInfo {
                        target_label: target,
                        build_file,
                        rule,
                        package,
                        workspace_root,
                        start_time: Instant::now(),
                    })
                }
            })
            .buffer_unordered(num_cpus::get() * 8); // Pipeline: 8x cores (128 on 16-core) - optimal for simple C file compilation

        // STREAMING PIPELINE: Parse and build concurrently to eliminate waterfall
        // As soon as a parse completes, start building - don't wait for all parses to finish!
        let mut parse_stream = parse_results;
        let mut build_pool: FuturesUnordered<_> = FuturesUnordered::new();
        let max_concurrent_builds = num_cpus::get() * 4; // 64 on 16-core - sweet spot balancing throughput and contention
        let mut failed = Vec::new();
        let mut build_count = 0;
        let mut parse_done = false;

        loop {
            // Process parses and builds concurrently using select!
            tokio::select! {
                // Parse stream has a result ready
                Some(parse_result) = parse_stream.next(), if !parse_done => {
                    match parse_result {
                        Ok(info) => {
                            eprintln!("🎯 Building target: {} (type: {})", info.rule.name, info.rule.rule_type);

                            let TargetBuildInfo {
                                target_label,
                                build_file,
                                rule,
                                package,
                                workspace_root,
                                start_time,
                            } = info;

                            // Create build future immediately
                            let repo_root = if workspace_root != self.workspace_root {
                                Some(workspace_root)
                            } else {
                                None
                            };

                            let future = async move {
                                let build_file_ref: &rush_parse::BuildFile = &build_file;
                                let rule_ref: &rush_parse::Rule = &rule;
                                let package_ref: &str = &package;
                                let repo_root_ref = repo_root.as_ref();
                                let result = self
                                    .build_target(build_file_ref, rule_ref, package_ref, repo_root_ref)
                                    .await;
                                (target_label, result, start_time, rule.rule_type)
                            };

                            build_pool.push(future);

                            // Throttle if build pool is full
                            while build_pool.len() >= max_concurrent_builds {
                                if let Some((target, result, start_time, _rule_type)) = build_pool.next().await {
                                    match result {
                                        Ok(_) => {
                                            build_count += 1;
                                            let duration_ms = start_time.elapsed().as_millis() as u64;
                                            self.events.target_completed(target.clone(), true, duration_ms);
                                            eprintln!("✅ {} built successfully", target);
                                        }
                                        Err(e) => {
                                            let duration_ms = start_time.elapsed().as_millis() as u64;
                                            self.events.target_completed(target.clone(), false, duration_ms);
                                            debug_log!("❌ Failed to build {}: {}", target, e);
                                            failed.push(target);
                                        }
                                    }
                                    self.memory_guard.check_or_abort()?;
                                }
                            }
                        }
                        Err(e) => {
                            debug_log!("⚠️  Failed to parse target: {}", e);
                            // We don't have the original target label here in streaming mode
                        }
                    }
                },

                // Build result ready
                Some((target, result, start_time, _rule_type)) = build_pool.next(), if !build_pool.is_empty() => {
                    match result {
                        Ok(_) => {
                            build_count += 1;
                            let duration_ms = start_time.elapsed().as_millis() as u64;
                            self.events.target_completed(target.clone(), true, duration_ms);
                            eprintln!("✅ {} built successfully", target);
                        }
                        Err(e) => {
                            let duration_ms = start_time.elapsed().as_millis() as u64;
                            self.events.target_completed(target.clone(), false, duration_ms);
                            debug_log!("❌ Failed to build {}: {}", target, e);
                            failed.push(target);
                        }
                    }
                    self.memory_guard.check_or_abort()?;
                },

                // All parses done, no more work to queue
                else => {
                    parse_done = true;
                    if build_pool.is_empty() {
                        break;
                    }
                }
            }
        }

        // Drain any remaining builds
        while let Some((target, result, start_time, _rule_type)) = build_pool.next().await {
            match result {
                Ok(_) => {
                    build_count += 1;
                    let duration_ms = start_time.elapsed().as_millis() as u64;
                    self.events
                        .target_completed(target.clone(), true, duration_ms);
                    eprintln!("✅ {} built successfully", target);
                }
                Err(e) => {
                    let duration_ms = start_time.elapsed().as_millis() as u64;
                    self.events
                        .target_completed(target.clone(), false, duration_ms);
                    debug_log!("❌ Failed to build {}: {}", target, e);
                    failed.push(target);
                }
            }
            self.memory_guard.check_or_abort()?;
        }

        // Create convenience symlinks
        if let Err(e) = self.output_tree.create_convenience_symlinks() {
            debug_log!("Warning: Failed to create convenience symlinks: {}", e);
        }

        // Print build summary
        self.progress.print_summary();

        // Print metrics
        self.metrics.print_summary();

        // Print incremental build stats and save state
        {
            let detector = self.change_detector.lock().await;
            let stats = detector.stats();
            eprintln!("\n📁 Incremental Build State:");
            eprintln!("   Tracked files: {}", stats.tracked_files);
            eprintln!("   Tracked targets: {}", stats.tracked_targets);

            // Save state
            if let Err(e) = detector.save_state() {
                debug_log!("⚠️  Warning: Failed to save build state: {}", e);
            }
        }

        // Emit build finished event
        let build_duration_ms = build_start.elapsed().as_millis() as u64;
        let success = failed.is_empty();
        self.events
            .build_finished(success, build_duration_ms, build_count, failed.len());
        self.events.flush();

        // Finish progress bar
        self.progress.finish_progress_bar();

        // Stop memory guard and get final stats
        self.memory_guard.stop();
        let peak_memory_gb = self.memory_guard.peak_memory_gb();

        // Dump diagnostics if threshold was exceeded
        if self.memory_guard.threshold_exceeded() {
            let dump_dir = self.workspace_root.join(".rush-cache").join("diagnostics");
            std::fs::create_dir_all(&dump_dir)?;
            if let Err(e) = self.memory_guard.dump_diagnostics(&dump_dir) {
                debug_log!("⚠️  Failed to dump memory diagnostics: {}", e);
            }
        }

        // Update cache memory in ResourceManager before getting stats
        let cache_memory_mb = self.global_bzl_cache.estimated_memory_mb();
        self.resource_manager
            .update_cache_memory(cache_memory_mb)
            .await;

        // Log resource statistics
        let stats = self.resource_manager.stats().await;
        info!("Resource usage: {}", stats.format());

        // Emit GlobalBzlCache statistics for operator visibility
        self.global_bzl_cache.print_stats();

        if failed.is_empty() {
            eprintln!("\n🎉 Build completed successfully!");
            // A success that silently dropped an unparseable package is
            // exactly the state that must not pass unremarked.
            crate::build_orchestrator_ext::report_poisoned_stubs_if_any();
            self.write_build_trace_if_enabled();
            eprintln!("   {} targets built", build_count);
            eprintln!("   Output: {}", self.output_tree.config_dir().display());
            eprintln!(
                "   Peak memory: {} MB ({}% of {} MB available)",
                stats.peak_memory_mb,
                (stats.peak_memory_mb * 100) / stats.available_memory_mb.max(1),
                stats.available_memory_mb
            );
            eprintln!(
                "   Cache memory: {} MB (peak: {} MB)",
                stats.cache_memory_mb, stats.peak_cache_mb
            );
            eprintln!("   Memory guard peak: {:.2} GB", peak_memory_gb);
            Ok(())
        } else {
            eprintln!("\n❌ Build completed with failures");
            // A failed build's phase breakdown is worth as much as a
            // successful one's when the question is where time goes.
            self.write_build_trace_if_enabled();
            eprintln!("   {} succeeded, {} failed", build_count, failed.len());
            eprintln!("   Peak memory: {} MB", stats.peak_memory_mb);
            eprintln!(
                "   Cache memory: {} MB (peak: {} MB)",
                stats.cache_memory_mb, stats.peak_cache_mb
            );
            eprintln!("   Memory guard peak: {:.2} GB", peak_memory_gb);
            eprintln!("\nFailed targets:");
            for target in &failed {
                eprintln!("   - {}", target);
            }
            anyhow::bail!("{} target(s) failed to build", failed.len())
        }
    }

    /// Execute a test request (build and run tests).
    pub async fn test(
        &self,
        request: BuildRequest,
        output_mode: TestOutputMode,
        filter: Option<&str>,
    ) -> Result<TestResults> {
        let owner = self.clone();
        let filter = filter.map(str::to_owned);
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || handle.block_on(async move {
            owner.test_owned(request,output_mode,filter.as_deref()).await
        })).await.map_err(|e|anyhow::anyhow!("owned native test failed: {e}"))?
    }

    async fn test_owned(
        &self,
        request: BuildRequest,
        output_mode: TestOutputMode,
        filter: Option<&str>,
    ) -> Result<TestResults> {
        let command_lock = workspace_command_lock(&self.workspace_root);
        let _command_guard = command_lock.lock().await;
        let test_semantics = rush_command_core::CommandSemantics::from_process();
        anyhow::ensure!(crate::compiler_discovery::requested_compile_mode(&test_semantics)?
            == rush_rules::context::NativeCompileMode::Literal,
            "diagnostic compiler discovery is a build-only profile");
        let _native_lease = crate::compiler_discovery::WorkspaceLease::acquire(&self.workspace_root,
            rush_rules::context::NativeCompileMode::Literal)?;
        _native_lease.token(false)?;
        use std::time::Instant;
        let test_start = Instant::now();

        eprintln!("🧪 Rush Test Starting");
        eprintln!("Workspace: {}", self.workspace_root.display());
        eprintln!("Targets: {:?}", request.targets);
        if let Some(f) = filter {
            eprintln!("Filter: {}", f);
        }

        if request.targets.is_empty() {
            anyhow::bail!("No test targets specified");
        }

        // Emit build started event (tests require building first)
        let build_id = format!("test_{}", BuildEventStream::timestamp());
        self.events
            .build_started(build_id.clone(), request.targets.clone());

        // Expand wildcard patterns (expand_targets self-installs the tracked IO).
        let expanded_targets = self.expand_targets(&request.targets)?;

        if expanded_targets.is_empty() {
            anyhow::bail!("No targets found matching patterns: {:?}", request.targets);
        }

        // Filter to only test targets
        let mut test_targets = Vec::new();
        for target in &expanded_targets {
            let (package, name) = parse_target_label(target)?;
            let Some(build_path) = find_build_file(&self.workspace_root.join(&package)) else {
                continue;
            };
            if let Ok(build_file) = self.parse_build_file_cached(&build_path, None) {
                if let Some(rule) = build_file.rules.iter().find(|r| r.name == name) {
                    if matches!(rule.rule_type.as_str(), "cc_test" | "rust_test" | "py_test") {
                        test_targets.push(target.clone());
                    }
                }
            }
        }

        if test_targets.is_empty() {
            eprintln!("\n⚠️  No test targets found");
            return Ok(TestResults::new());
        }

        eprintln!("\n📋 Found {} test targets", test_targets.len());

        // Phase 1: Build all test targets
        eprintln!("\n🔨 Building tests...");
        let mut test_binaries = Vec::new();

        for target in &test_targets {
            let (package, name) = parse_target_label(target)?;

            // Apply name filter if specified
            if let Some(pattern) = filter {
                if !name.contains(pattern) {
                    eprintln!(
                        "⏭️  Skipping test: {} (doesn't match filter '{}')",
                        name, pattern
                    );
                    continue;
                }
            }

            eprintln!("🎯 Building test: {}", name);
            // Route the build phase through the configured dependency graph.
            // The legacy raw-rule builder ignored symbolic select() values in
            // deps/srcs and therefore could run a differently configured test.
            self.build_with_dependencies_with_semantics_locked(
                vec![target.clone()],
                rush_command_core::CommandSemantics::from_process(),
            )
            .await?;
            let executable = self.output_tree.bin_path(&package, &name);
            if !executable.exists() {
                anyhow::bail!("Test build produced no executable: {}", executable.display());
            }
            test_binaries.push((target.clone(), executable));
        }

        if test_binaries.is_empty() {
            eprintln!("\n⚠️  No tests to run after filtering");
            return Ok(TestResults::new());
        }

        // Create convenience symlinks
        if let Err(e) = self.output_tree.create_convenience_symlinks() {
            debug_log!("Warning: Failed to create convenience symlinks: {}", e);
        }

        // Print build summary
        self.progress.print_summary();

        // Phase 2: Run all tests in parallel (with caching)
        eprintln!("\n🚀 Running {} tests in parallel...", test_binaries.len());

        let mut join_set = JoinSet::new();
        let events = Arc::clone(&self.events);
        let test_cache = Arc::clone(&self.test_cache);

        for (label, test_binary) in test_binaries {
            let events_clone = Arc::clone(&events);
            let test_cache_clone = Arc::clone(&test_cache);

            // Check test cache first
            let cached_result = {
                let mut cache = test_cache_clone.lock().unwrap();
                cache.get(&label, &test_binary)
            };

            if let Some(cached) = cached_result {
                // Use cached result
                eprintln!(
                    "✓ {} {} {}",
                    label.bright_white(),
                    "(cached)".green(),
                    format!("{}ms", cached.duration_ms).bright_black()
                );

                // Create TestResult from cached data
                let test_result = TestResult {
                    label: label.clone(),
                    passed: cached.passed,
                    duration: std::time::Duration::from_millis(cached.duration_ms),
                    exit_code: cached.exit_code,
                    stdout: cached.stdout,
                    stderr: cached.stderr,
                };

                // Emit test completed event
                events_clone.emit(BuildEvent::TestCompleted {
                    timestamp: BuildEventStream::timestamp(),
                    test: label.clone(),
                    passed: cached.passed,
                    duration_ms: cached.duration_ms,
                    exit_code: cached.exit_code,
                });

                // Store cached result to add later (can't modify results in loop)
                join_set.spawn(async move { Ok(test_result) });
                continue;
            }

            // Emit test started event
            events_clone.emit(BuildEvent::TestStarted {
                timestamp: BuildEventStream::timestamp(),
                test: label.clone(),
            });

            let test_cache_for_task = Arc::clone(&test_cache);
            join_set.spawn(async move {
                let result = Self::run_test_static(&test_binary, &label, output_mode).await?;

                // Cache the test result
                {
                    let mut cache = test_cache_for_task.lock().unwrap();
                    let _ = cache.put(TestResultParams {
                        label: label.clone(),
                        binary_path: test_binary.clone(),
                        passed: result.passed,
                        exit_code: result.exit_code,
                        duration_ms: result.duration.as_millis() as u64,
                        stdout: result.stdout.clone(),
                        stderr: result.stderr.clone(),
                    });
                    let _ = cache.save();
                }

                Ok(result)
            });
        }

        // Keep the shared workspace lease until every started test finishes,
        // including blocking children whose async waiter cannot cancel them.
        let results = Self::collect_started_test_results(&mut join_set, &events).await?;

        // Emit build finished event
        let test_duration_ms = test_start.elapsed().as_millis() as u64;
        let success = results.all_passed();
        events.build_finished(
            success,
            test_duration_ms,
            results.passed(),
            results.failed(),
        );
        events.flush();

        // Print test cache statistics
        {
            let cache = self.test_cache.lock().unwrap();
            let stats = cache.stats();
            if stats.lookups > 0 {
                eprintln!();
                eprintln!("{}", "📦 Test Cache Statistics:".bold().cyan());
                eprintln!(
                    "   {} {}",
                    "Cache hit rate:".bright_black(),
                    format!("{:.1}%", cache.hit_rate()).green()
                );
                eprintln!(
                    "   {} {}",
                    "Tests skipped:".bright_black(),
                    stats.tests_skipped.to_string().cyan()
                );
                eprintln!(
                    "   {} {}",
                    "Time saved:".bright_black(),
                    format!("{}ms", stats.time_saved_ms).cyan()
                );
            }
        }

        Ok(results)
    }

    async fn collect_started_test_results(
        join_set: &mut JoinSet<Result<TestResult>>,
        events: &BuildEventStream,
    ) -> Result<TestResults> {
        let mut results = TestResults::new();
        let mut first_error = None;
        while let Some(result) = join_set.join_next().await {
            match result {
                Ok(Ok(test_result)) => {
                    // Emit test completed event
                    events.emit(BuildEvent::TestCompleted {
                        timestamp: BuildEventStream::timestamp(),
                        test: test_result.label.clone(),
                        passed: test_result.passed,
                        duration_ms: test_result.duration.as_millis() as u64,
                        exit_code: test_result.exit_code,
                    });
                    results.add(test_result);
                }
                Ok(Err(error)) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(anyhow::anyhow!("Test task panicked: {}", error));
                    }
                }
            }
        }

        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(results)
    }
    /// Run a single test binary and capture results.
    async fn _run_test(
        &self,
        test_binary: &Path,
        label: &str,
        output_mode: TestOutputMode,
    ) -> Result<TestResult> {
        Self::run_test_static(test_binary, label, output_mode).await
    }

    /// Run a single test binary (static implementation for parallel execution).
    async fn run_test_static(
        test_binary: &Path,
        label: &str,
        output_mode: TestOutputMode,
    ) -> Result<TestResult> {
        use std::process::Command;
        use std::time::Instant;

        let start = Instant::now();

        // Execute in blocking thread to avoid blocking async runtime
        let test_binary_clone = test_binary.to_path_buf();
        let output = tokio::task::spawn_blocking(move || Command::new(&test_binary_clone).output())
            .await
            .map_err(|e| anyhow::anyhow!("Failed to spawn test task: {}", e))?
            .map_err(|e| anyhow::anyhow!("Failed to execute test: {}", e))?;

        let duration = start.elapsed();

        let passed = output.status.success();
        let exit_code = output.status.code().unwrap_or(-1);

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        // Print test output based on output mode
        let should_show_output = match output_mode {
            TestOutputMode::Summary => false,
            TestOutputMode::Errors => !passed,
            TestOutputMode::All => true,
        };

        if should_show_output {
            if !stdout.is_empty() {
                eprintln!("\n--- Test Output: {} ---", label);
                print!("{}", stdout);
            }
            if !stderr.is_empty() {
                debug_log!("\n--- Test Errors: {} ---", label);
                eprint!("{}", stderr);
            }
        }

        if passed {
            eprintln!(
                "{} {} {} {}",
                "✅".green(),
                label.bright_white(),
                "PASSED".green().bold(),
                format!("in {:.2}s", duration.as_secs_f64()).bright_black()
            );
        } else {
            eprintln!(
                "{} {} {} {}",
                "❌".red(),
                label.bright_white(),
                format!("FAILED (exit code: {})", exit_code).red().bold(),
                format!("in {:.2}s", duration.as_secs_f64()).bright_black()
            );
        }

        Ok(TestResult {
            label: label.to_string(),
            passed,
            duration,
            exit_code,
            stdout,
            stderr,
        })
    }

    /// Build a single target and its dependencies.
    fn build_target<'a>(
        &'a self,
        _build_file: &'a rush_parse::BuildFile,
        rule: &'a rush_parse::Rule,
        package: &'a str,
        repo_root: Option<&'a PathBuf>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<PathBuf>>> + Send + 'a>>
    {
        Box::pin(async move {
            let target_label = format!("//{}:{}", package, rule.name);

            // First, build all dependencies in parallel and collect their outputs and include paths
            let mut dep_outputs = Vec::new();
            let mut dep_outputs_by_label: HashMap<String, Vec<PathBuf>> = HashMap::new();
            let mut dep_include_dirs = Vec::new();
            let mut dep_labels = Vec::new();

            if let Some(Value::List(deps)) = rule.attributes.get("deps") {
                // Build dependencies in parallel using a shared cache to avoid redundant builds.
                // The dependency_cache ensures that each unique dependency is built exactly once
                // even when multiple targets depend on it.
                let mut dep_tasks = JoinSet::new();

                for dep in deps {
                    if let Value::String(dep_label) = dep {
                        // Normalize dependency label to full form
                        let normalized_label = if dep_label.starts_with("//") {
                            dep_label.clone()
                        } else if dep_label.starts_with(':') {
                            format!("//{}:{}", package, dep_label.strip_prefix(':').unwrap())
                        } else if !dep_label.contains('/')
                            && !dep_label.contains(':')
                            && !dep_label.starts_with('@')
                        {
                            format!("//{}:{}", package, dep_label)
                        } else {
                            dep_label.clone()
                        };

                        dep_labels.push(normalized_label.clone());

                        // Spawn parallel task to build dependency
                        let dep_label_clone = dep_label.clone();
                        let normalized_label_clone = normalized_label.clone();
                        let package_clone = package.to_string();
                        let target_label_clone = target_label.clone();
                        let self_clone = self.clone();
                        let repo_root_clone = repo_root.cloned();

                        dep_tasks.spawn(async move {
                            debug_log!(
                                "[TRACE] {} about to build dependency: {}",
                                target_label_clone,
                                normalized_label_clone
                            );

                            // Check if dependency is already built or being built
                            // Use LRU cache with bounded capacity to prevent OOM
                            let result_lock = {
                                let mut cache = self_clone.dependency_cache.lock().unwrap();
                                cache
                                    .get_or_insert(normalized_label_clone.clone(), || {
                                        Arc::new(tokio::sync::Mutex::new(None))
                                    })
                                    .clone()
                            };

                            let mut result = result_lock.lock().await;

                            if let Some(cached) = &*result {
                                // Dependency already built, use cached result
                                debug_log!(
                                    "[TRACE] {} using cached dependency: {}",
                                    target_label_clone,
                                    normalized_label_clone
                                );
                                Ok::<_, anyhow::Error>((
                                    normalized_label_clone,
                                    cached.clone(),
                                ))
                            } else {
                                // Build dependency and cache result
                                let dep_result = self_clone
                                    .resolve_and_build_dep(
                                        &dep_label_clone,
                                        &package_clone,
                                        repo_root_clone.as_ref(),
                                    )
                                    .await?;
                                *result = Some(dep_result.clone());
                                debug_log!(
                                    "[TRACE] {} completed dependency: {}",
                                    target_label_clone,
                                    normalized_label_clone
                                );
                                Ok((
                                    normalized_label_clone,
                                    dep_result,
                                ))
                            }
                        });
                    }
                }

                // Collect all dependency results
                while let Some(result) = dep_tasks.join_next().await {
                    match result {
                        Ok(Ok((normalized_label, (outputs, includes)))) => {
                            dep_outputs_by_label
                                .insert(normalized_label, outputs.clone());
                            dep_outputs.extend(outputs);
                            dep_include_dirs.extend(includes);
                        }
                        Ok(Err(e)) => return Err(e),
                        Err(e) => {
                            return Err(anyhow::anyhow!("Dependency build task failed: {}", e))
                        }
                    }
                }
            }

            // Check if this target needs rebuilding
            debug_log!("[TRACE] About to check needs_rebuild for {}", target_label);
            let needs_rebuild = {
                let srcs = self.extract_srcs(rule)?;
                let input_paths: Vec<PathBuf> = srcs
                    .iter()
                    .map(|src| self.workspace_root.join(package).join(src))
                    .collect();

                debug_log!(
                    "[TRACE] Acquiring change_detector lock for {}",
                    target_label
                );
                let mut detector = self.change_detector.lock().await;
                debug_log!("[TRACE] Lock acquired for {}", target_label);

                let result = detector.needs_rebuild(&target_label, &input_paths, &dep_labels)?;
                debug_log!("[TRACE] needs_rebuild={} for {}", result, target_label);
                result
            };
            debug_log!("[TRACE] Released change_detector lock for {}", target_label);

            if !needs_rebuild {
                // Target is up to date - return expected outputs
                eprintln!("   ⚡ {} is up to date (skipping)", rule.name);
                self.progress.record_target_skip();

                let outputs: Vec<PathBuf> = match rule.rule_type.as_str() {
                    "cc_library"
                    | "tf_cuda_library"
                    | "tf_cc_library"
                    | "cuda_library"
                    | "tf_library"
                    | "_tf_library"
                    | "tf_kernel_library"
                    | "tf_gpu_kernel_library"
                    | "gpu_kernel_library"
                    | "tf_mkl_kernel_library"
                    | "tf_ops_fuzz_target_lib"
                    | "cc_header_only_library" => {
                        let srcs = self.extract_srcs(rule)?;
                        let mut lib_outputs: Vec<PathBuf> = srcs
                            .iter()
                            .map(|src| {
                                let obj_filename =
                                    format!("{}.o", src.trim_end_matches(".cc").replace('/', "_"));
                                self.output_tree.obj_path(package, &obj_filename)
                            })
                            .collect();
                        // Include transitive dependency outputs for cc_library
                        lib_outputs.extend_from_slice(&dep_outputs);
                        lib_outputs
                    }
                    "cc_binary"
                    | "cc_test"
                    | "tf_cc_binary"
                    | "tf_cc_test"
                    | "tf_cuda_cc_test"
                    | "tf_cc_test_mkl"
                    | "tf_cc_shared_test"
                    | "tf_custom_op_library"
                    | "tf_cc_shared_object"
                    | "tf_cc_shared_library"
                    | "cc_shared_library" => {
                        vec![self.output_tree.bin_path(package, &rule.name)]
                    }
                    "alias"
                    | "macro_expanded"
                    | "genrule"
                    | "filegroup"
                    | "tf_gen_op_wrappers_cc"
                    | "pywrap_tensorflow_macro"
                    | "config_setting" => {
                        // No outputs for these
                        Vec::new()
                    }
                    "rust_library" => {
                        let output_dir = self.output_tree.bin_path(package, ".libs");
                        vec![output_dir.join(format!("lib{}.rlib", rule.name))]
                    }
                    "rust_binary" | "rust_test" => {
                        vec![self.output_tree.bin_path(package, &rule.name)]
                    }
                    "py_library" | "py_strict_library" | "pytype_strict_library" => {
                        // Python libraries: return source file paths for dependency tracking
                        // py_strict_library is TensorFlow's stricter variant, but handled identically
                        use crate::rules_python::PyLibrary;
                        if let Ok(py_lib) = PyLibrary::from_rule(rule) {
                            py_lib.get_sources(&self.workspace_root, package)
                        } else {
                            Vec::new()
                        }
                    }
                    "py_binary" | "py_test" | "pytype_strict_binary" | "py_strict_test" => {
                        vec![self.output_tree.bin_path(package, &rule.name)]
                    }
                    "proto_library" | "tf_proto_library" => {
                        // Proto library returns compiled object files (.pb.o) from generated code
                        use crate::rules_proto::ProtoLibrary;
                        let proto_lib = ProtoLibrary::from_rule(rule)?;

                        // Return paths to compiled .pb.o files (not .proto sources!)
                        let genfiles_dir = self.output_tree.config_dir().join("genfiles");
                        let sources = proto_lib.get_sources(&self.workspace_root, package);

                        sources
                            .iter()
                            .map(|proto_file| {
                                let proto_rel_path = proto_file
                                    .strip_prefix(&self.workspace_root)
                                    .unwrap_or(proto_file);
                                let proto_base = proto_rel_path.with_extension("");
                                let proto_name = proto_base.to_string_lossy();
                                // Generate path to .pb.cc, then replace extension with .o
                                let pb_cc_path = genfiles_dir.join(format!("{}.pb.cc", proto_name));
                                pb_cc_path.with_extension("o")
                            })
                            .collect()
                    }
                    "cc_proto_library" => {
                        // Return library archive path (libname.a)
                        // cc_proto_library now creates a static library from proto-generated code
                        let lib_name = format!("lib{}.a", rule.name);
                        vec![self.output_tree.lib_path(package, &lib_name)]
                    }
                    // Metadata-only rules that legitimately have no outputs
                    "test_suite"
                    | "package_group"
                    | "exports_files"
                    | "package"
                    | "licenses"
                    | "glob_lit_tests"
                    | "tf_cc_tests"
                    | "gentbl_cc_library"
                    | "gen_saved_model"
                    | "tf_pyclif_proto_library" => Vec::new(),
                    "sh_library" => {
                        // Shell library: return source file paths for dependency tracking
                        use crate::rules_shell::ShLibrary;
                        if let Ok(sh_lib) = ShLibrary::from_rule(rule) {
                            sh_lib.get_sources(&self.workspace_root, package)
                        } else {
                            Vec::new()
                        }
                    }
                    "sh_binary" | "sh_test" => {
                        // Shell binary/test: return executable path
                        vec![self.output_tree.bin_path(package, &rule.name)]
                    }
                    "tf_gen_op_libs" => {
                        // TensorFlow op libs placeholder - no real outputs
                        Vec::new()
                    }
                    _ => {
                        anyhow::bail!(
                            "Unsupported rule type '{}' for target //{}:{}\n\
                             \n\
                             This rule type is not implemented in Rush. If this is a legitimate Bazel rule,\n\
                             please file an issue at https://github.com/rush-rs/rush/issues",
                            rule.rule_type,
                            package,
                            rule.name
                        );
                    }
                };

                return Ok(outputs);
            }

            // Target needs rebuilding
            let outputs = match rule.rule_type.as_str() {
                "cc_library"
                | "tf_cuda_library"
                | "tf_cc_library"
                | "cuda_library"
                | "tf_library"
                | "_tf_library"
                | "tf_kernel_library"
                | "tf_gpu_kernel_library"
                | "gpu_kernel_library"
                | "tf_mkl_kernel_library"
                | "tf_ops_fuzz_target_lib"
                | "cc_header_only_library" => {
                    self.build_cc_library(rule, package, &dep_outputs, &dep_include_dirs)
                        .await?
                }
                "cc_binary"
                | "tf_cc_binary"
                | "tf_cc_shared_object"
                | "tf_custom_op_library"
                | "tf_cc_shared_library"
                | "cc_shared_library" => {
                    self.build_cc_binary(
                        rule,
                        package,
                        &dep_outputs,
                        &dep_outputs_by_label,
                        &dep_include_dirs,
                    )
                        .await?
                }
                "cc_test" | "tf_cc_test" | "tf_cuda_cc_test" | "tf_cc_test_mkl"
                | "tf_cc_shared_test" => {
                    self.build_cc_test(rule, package, &dep_outputs, &dep_include_dirs)
                        .await?
                }
                "rust_library" => self.build_rust_library(rule, package).await?,
                "rust_binary" => self.build_rust_binary(rule, package).await?,
                "rust_test" => self.build_rust_test(rule, package).await?,
                "py_library" | "py_strict_library" | "pytype_strict_library" => {
                    self.build_py_library(rule, package, repo_root).await?
                }
                "py_binary" | "pytype_strict_binary" => {
                    self.build_py_binary(rule, package, repo_root).await?
                }
                "py_test" | "py_strict_test" => {
                    self.build_py_test(rule, package, repo_root).await?
                }
                "sh_library" => self.build_sh_library(rule, package).await?,
                "sh_binary" => self.build_sh_binary(rule, package).await?,
                "sh_test" => self.build_sh_test(rule, package).await?,
                "proto_library" | "tf_proto_library" => {
                    self.build_proto_library(rule, package).await?
                }
                "cc_proto_library" => {
                    self.build_cc_proto_library(rule, package, &dep_outputs)
                        .await?
                }
                "tf_gen_op_wrappers_cc" => self.build_tf_gen_op_wrappers_cc(rule, package).await?,
                "alias" => {
                    // Alias just points to another target - build that target instead
                    if let Some(Value::String(actual)) = rule.attributes.get("actual") {
                        let (actual_package, actual_name) = if actual.starts_with("//") {
                            parse_target_label(actual)?
                        } else if actual.starts_with(':') {
                            (
                                package.to_string(),
                                actual.strip_prefix(':').unwrap().to_string(),
                            )
                        } else {
                            anyhow::bail!("Invalid alias actual: {}", actual);
                        };

                        // Build the actual target (pass repo_root to maintain context)
                        self.build_cross_package_dep(&actual_package, &actual_name, repo_root)
                            .await?
                    } else {
                        anyhow::bail!(
                            "alias rule '{}' in package '{}' is missing required 'actual' attribute",
                            rule.name,
                            package
                        );
                    }
                }
                // Metadata-only rules that are legitimately metadata-only
                "test_suite"
                | "package_group"
                | "filegroup"
                | "exports_files"
                | "package"
                | "licenses"
                | "glob_lit_tests"
                | "tf_cc_tests"
                | "gentbl_cc_library"
                | "gen_saved_model"
                | "tf_pyclif_proto_library" => {
                    // test_suite: Collection of test targets (metadata)
                    // package_group: Visibility specification (metadata)
                    // filegroup: Collection of files (for dependencies, no build action)
                    // exports_files: Mark files as visible (metadata)
                    // package: Package-level metadata
                    // licenses: License metadata
                    // glob_lit_tests: LLVM lit test generator (TensorFlow testing)
                    // tf_cc_tests: TensorFlow multi-test generator (metadata wrapper)
                    // gentbl_cc_library: MLIR TableGen code generation (generated code checked in)
                    // gen_saved_model: SavedModel generator (Python/TF Serving, not C++ build)
                    // tf_pyclif_proto_library: Python CLIF proto bindings (Python only)
                    Vec::new()
                }

                // Rules that need implementation but aren't critical yet
                "macro_expanded" => {
                    // Macro expansion result - treat as metadata-only for now
                    // This is generated by macro expansion and may need special handling
                    Vec::new()
                }

                // Generic build rule
                "genrule" => self.build_genrule(rule, package).await?,

                // config_setting is a metadata rule that defines configuration conditions
                // Currently treated as metadata-only since Rush doesn't yet support
                // full conditional compilation. select() statements are resolved during
                // attribute parsing by choosing the "//conditions:default" case.
                "config_setting" => Vec::new(),

                // pywrap_tensorflow_macro generates Python bindings —
                // out of rush's C++-only scope. No artifacts produced.
                // Any C++ target reached by this macro's expansion is
                // built through its own cc_library / cc_binary rules.
                // See UNIMPLEMENTED.md "Python binding generation".
                "pywrap_tensorflow_macro" => Vec::new(),

                "tf_gen_op_libs" => self.build_tf_gen_op_libs(rule, package).await?,
                "glob" => {
                    anyhow::bail!(
                        "glob used as rule '{}' in package '{}' - this is likely a parser error\n\
                         \n\
                         glob should be a function call in BUILD files, not a rule definition.\n\
                         This likely indicates a parser issue with function call vs rule distinction.",
                        rule.name,
                        package
                    );
                }
                _ => anyhow::bail!("Unsupported rule type: {}", rule.rule_type),
            };

            // Mark target as built (state will be saved at end of build)
            debug_log!("[TRACE] About to mark {} as built", target_label);
            {
                debug_log!(
                    "[TRACE] Acquiring change_detector lock to mark {} built",
                    target_label
                );
                let mut detector = self.change_detector.lock().await;
                debug_log!("[TRACE] Lock acquired, marking {} as built", target_label);
                detector.mark_target_built(&target_label);
                debug_log!("[TRACE] Marked {} as built, releasing lock", target_label);
                // Note: We don't save_state() here because it does blocking file I/O
                // while holding a mutex in an async context, which can
                // deadlock the tokio runtime. State is saved once at the end of build.
            }
            debug_log!(
                "[TRACE] Lock released for {}, returning outputs",
                target_label
            );

            debug_log!("[TRACE] Returning from build_target for {}", target_label);
            Ok(outputs)
        })
    }

    /// Resolve and build a dependency target.
    /// Returns (object_files, include_directories).
    /// Stage Bazel's `include_prefix` virtual-include farm for a cc_library
    /// rule, returning the virtual-include root to put on the consumer's (and
    /// the producer's own) `-I` search path.
    ///
    /// Bazel's `include_prefix` makes every header in `hdrs` includable at
    /// `<include_prefix>/<rel>`, where `<rel>` is the header's
    /// package-relative path AFTER `strip_include_prefix` is removed (the two
    /// attributes compose: strip first, then prefix). Bazel materializes this
    /// in a per-target `_virtual_includes/<target>/` directory under the bin
    /// dir and adds that root to the quote-include search path. rush mirrors
    /// that exactly: it symlinks each real header into
    /// `OutputTree::virtual_includes_dir(package, target)` at its remapped
    /// path and returns the root so callers can prepend `-I<root>`.
    ///
    /// `source_pkg_dir` is the on-disk directory the rule's `hdrs` are
    /// relative to (the package dir in the workspace or external repo).
    /// Returns `Ok(None)` when the rule has no `include_prefix` (nothing to
    /// stage). This is the faithful replacement for the farmhash_gpu
    /// file-copy workaround — no special-casing, any `include_prefix` works.
    fn stage_include_prefix(
        &self,
        rule: &rush_parse::Rule,
        source_pkg_dir: &Path,
        package: &str,
    ) -> Result<Option<PathBuf>> {
        let include_prefix = match rule.attributes.get("include_prefix") {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            _ => return Ok(None),
        };
        // `strip_include_prefix` is removed from the header's package-relative
        // path BEFORE the include_prefix is prepended. A leading "/" form is
        // repo-root-relative in Bazel, but for the prefix-composition the
        // package-relative header path is what matters; normalize by trimming
        // a leading slash so both forms behave the same when stripping.
        let strip = match rule.attributes.get("strip_include_prefix") {
            Some(Value::String(s)) => Some(s.trim_start_matches('/').to_string()),
            _ => None,
        };

        // Collect plain (non-label) header paths from `hdrs`.
        let hdrs: Vec<String> = match rule.attributes.get("hdrs") {
            Some(Value::List(list)) => list
                .iter()
                .filter_map(|v| match v {
                    Value::String(s)
                        if !(s.starts_with(':')
                            || s.starts_with("//")
                            || s.starts_with('@')) =>
                    {
                        Some(s.clone())
                    }
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        if hdrs.is_empty() {
            return Ok(None);
        }

        let target = rule.name.as_str();
        let vroot = self.output_tree.virtual_includes_dir(package, target);
        let mut staged_any = false;

        for hdr in &hdrs {
            // The header's path relative to the package.
            let rel = Path::new(hdr.as_str());
            // Apply strip_include_prefix: drop the leading <strip>/ component
            // from the package-relative path (Bazel errors if a header is not
            // under strip_include_prefix; rush is lenient and leaves it as-is,
            // which keeps an over-broad strip from dropping headers).
            let stripped: PathBuf = match &strip {
                Some(p) if !p.is_empty() && p != "." => {
                    rel.strip_prefix(p).map(|r| r.to_path_buf()).unwrap_or_else(|_| rel.to_path_buf())
                }
                _ => rel.to_path_buf(),
            };
            // Prepend include_prefix → the path the header is includable at.
            let virtual_rel = Path::new(&include_prefix).join(&stripped);
            let link_path = vroot.join(&virtual_rel);
            let real_path = source_pkg_dir.join(hdr);
            if !real_path.exists() {
                // Header may itself be generated/staged elsewhere; skip
                // silently rather than fail the whole library — the compile
                // will surface a precise missing-include error if it's truly
                // needed.
                continue;
            }
            if let Some(parent) = link_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // Re-point idempotently so reruns pick up moved sources.
            if link_path.symlink_metadata().is_ok() {
                let _ = std::fs::remove_file(&link_path);
            }
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(&real_path, &link_path).map_err(|e| {
                    anyhow::anyhow!(
                        "include_prefix: failed to stage virtual header {} -> {}: {}",
                        link_path.display(),
                        real_path.display(),
                        e
                    )
                })?;
            }
            #[cfg(not(unix))]
            {
                std::fs::copy(&real_path, &link_path)?;
            }
            staged_any = true;
        }

        if staged_any {
            Ok(Some(vroot))
        } else {
            Ok(None)
        }
    }

    async fn resolve_and_build_dep(
        &self,
        dep_label: &str,
        current_package: &str,
        repo_root: Option<&PathBuf>,
    ) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
        // Handle external dependencies (@repo//package:target)
        if dep_label.starts_with('@') {
            return self.resolve_external_dep(dep_label).await;
        }

        // Parse dependency label and normalize to full form
        let (dep_package, dep_name) = if dep_label.starts_with("//") {
            parse_target_label(dep_label)?
        } else if let Some(stripped) = dep_label.strip_prefix(':') {
            // Relative to current package
            (current_package.to_string(), stripped.to_string())
        } else {
            anyhow::bail!("Invalid dependency label: {}", dep_label);
        };

        let full_dep_label = format!("//{}:{}", dep_package, dep_name);
        eprintln!("   Building dependency: {}", full_dep_label);

        // Build the dependency (always use cross-package path to properly load BUILD file)
        let outputs = self
            .build_cross_package_dep(&dep_package, &dep_name, repo_root)
            .await?;

        // Include directory is the package directory (use repo_root if in external repo)
        let workspace_root = repo_root.unwrap_or(&self.workspace_root);
        let include_dir = workspace_root.join(&dep_package);

        // Also include genfiles directory for proto-generated headers
        let genfiles_dir = self.output_tree.config_dir().join("genfiles");

        // Honor Bazel `include_prefix` for in-workspace cc_library deps too:
        // stage the virtual-include farm and expose its root FIRST so the
        // prefixed `#include` resolves. Look the rule up in the dep package's
        // BUILD file (cached parse); a missing BUILD/rule is non-fatal here
        // (the build already produced `outputs`).
        let mut include_dirs = Vec::new();
        let build_path = self.preferred_build_file(&workspace_root.join(&dep_package));
        if let Some(bp) = build_path {
            if let Ok(build_file) = self.parse_build_file_cached(&bp, repo_root) {
                if let Some(rule) = build_file.rules.iter().find(|r| r.name == dep_name) {
                    if let Some(vroot) =
                        self.stage_include_prefix(rule, &include_dir, &dep_package)?
                    {
                        include_dirs.push(vroot);
                    }
                }
            }
        }
        include_dirs.push(include_dir);
        include_dirs.push(genfiles_dir);

        Ok((outputs, include_dirs))
    }

    /// Resolve an external dependency label (@repo//package:target or @repo).
    ///
    /// Parses the external label, looks up the repository path, and builds the target.
    async fn resolve_external_dep(&self, dep_label: &str) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
        // Parse @repo//package:target or just @repo
        // Examples:
        //   @bazel_skylib//lib:paths
        //   @sobol_data (repository-only reference, treat as data dependency)
        let rest = &dep_label[1..]; // Remove '@'
        let parts: Vec<&str> = rest.splitn(2, "//").collect();

        // For repository-only format, create a String with ":repo" that lives for the function
        let repo_only_target_path;
        let (repo_name, target_path) = if parts.len() == 2 {
            // Standard format: @repo//package:target
            (parts[0], parts[1])
        } else {
            // Repository-only format: @repo
            // Treat as @repo//:repo (empty package, target = repo name)
            // Use ":repo" format so package parsing below extracts empty package
            repo_only_target_path = format!(":{}", parts[0]);
            (parts[0], repo_only_target_path.as_str())
        };

        eprintln!(
            "   Resolving external dependency: @{} ({})",
            repo_name, target_path
        );

        // Extract target name from target_path (package:target or just package)
        let target = if let Some(colon_pos) = target_path.find(':') {
            &target_path[colon_pos + 1..]
        } else {
            // No :target - default to last segment of package path
            target_path.split('/').next_back().unwrap_or(target_path)
        };

        let registered = self.external_repo_paths.lock().await.get(repo_name).cloned();
        let required_source = self.requires_declared_workspace_source_for_path(repo_name, registered.as_deref());
        let explicit_source = self.bzl_loader.lock().ok().is_some_and(|loader|
            loader.preferred_registered_repository_path(repo_name, registered.as_deref()).is_some_and(|path|
                !loader.is_implicit_system_adapter(repo_name, &path)));
        // Compatibility include-only mapping cannot replace a required
        // declaration or an explicitly registered/local source provider.
        if !required_source && !explicit_source {
        if let Ok(include_dirs) =
            SystemPackageMapper::resolve_external_target(repo_name, target, &self.workspace_root)
        {
            // System package mapping succeeded - return with no outputs but include paths
            eprintln!(
                "      ✓ Mapped to system packages: {} include dirs",
                include_dirs.len()
            );
            return Ok((Vec::new(), include_dirs));
        }
        }

        // System mapping failed - fall back to building from repository

        // Look up repository path - try on-demand fetch if not found
        let requested = dependency_graph::Label::parse(dep_label, None, None)?;
        let admitted = self.registered_external_repository_path(repo_name, &requested).await?;
        let repo_root = if let Some(path) = admitted { path } else {
            let repo_paths = self.external_repo_paths.lock().await;

            if let Some(path) = repo_paths.get(repo_name) {
                path.clone()
            } else {
                // Repository not loaded yet - try on-demand fetch
                drop(repo_paths); // Release lock before expensive operation

                eprintln!(
                    "      ⚡ Repository @{} not found, attempting on-demand fetch...",
                    repo_name
                );

                // Try to fetch the repository on-demand
                if let Err(e) = self.fetch_repository_on_demand(repo_name).await {
                    anyhow::bail!(
                        "External repository '@{}' not found and on-demand fetch failed: {}",
                        repo_name,
                        e
                    );
                }

                // Re-acquire lock and try again
                let repo_paths = self.external_repo_paths.lock().await;
                repo_paths
                    .get(repo_name)
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                        "External repository '@{}' fetch succeeded but repository not registered",
                        repo_name
                    )
                    })?
                    .clone()
            }
        };

        // Parse package:target
        // If no :target specified, use Bazel convention: target name = last segment of package path
        let (package, target) = if let Some(colon_pos) = target_path.find(':') {
            (&target_path[..colon_pos], &target_path[colon_pos + 1..])
        } else {
            // No :target - default to last segment of package path
            // e.g., @local_tsl//tsl/platform -> package="tsl/platform", target="platform"
            let last_segment = target_path.split('/').next_back().unwrap_or(target_path);
            (target_path, last_segment)
        };

        eprintln!("      Repository root: {}", repo_root.display());
        eprintln!("      Package: {}, Target: {}", package, target);

        let build_path = self
            .preferred_build_file(&repo_root.join(package))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "BUILD file not found for external package @{}//{}: tried BUILD.bazel and BUILD",
                    repo_name,
                    package
                )
            })?;

        // Parse BUILD file
        let build_file = self.parse_build_file_cached(&build_path, Some(&repo_root))?;

        // Find target rule
        let rule = build_file
            .rules
            .iter()
            .find(|r| r.name == target)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Target '{}' not found in external repository @{}//{}",
                    target,
                    repo_name,
                    package
                )
            })?;

        // Build the target using the external repo root as workspace context
        let outputs = self
            .build_target(&build_file, rule, package, Some(&repo_root))
            .await?;

        // Include directory is the package directory within the external repo
        let include_dir = repo_root.join(package);

        // Honor Bazel `include_prefix`: stage a virtual-include farm so this
        // dep's headers are reachable at their prefixed path and expose that
        // root to consumers FIRST (so the prefixed include wins). e.g.
        // @farmhash_gpu_archive sets include_prefix="third_party/farmhash_gpu"
        // → consumers `#include "third_party/farmhash_gpu/src/farmhash_gpu.h"`.
        let mut include_dirs = Vec::new();
        if let Some(vroot) =
            self.stage_include_prefix(rule, &repo_root.join(package), package)?
        {
            include_dirs.push(vroot);
        }
        include_dirs.push(include_dir);

        Ok((outputs, include_dirs))
    }

    /// Build a dependency from a different package.
    async fn build_cross_package_dep(
        &self,
        dep_package: &str,
        dep_name: &str,
        repo_root: Option<&PathBuf>,
    ) -> Result<Vec<PathBuf>> {
        eprintln!(
            "   Loading cross-package dependency: //{}:{}",
            dep_package, dep_name
        );

        // Handle external dependencies (packages starting with "external/")
        // These are Bazel's way of referencing external repositories
        if dep_package.starts_with("external/") {
            // External dependency - treat as metadata-only since we don't have the repos
            return Ok(vec![]);
        }

        // Find and parse BUILD file for the dependency package
        // Use repo_root if we're in an external repository context
        let workspace_root = repo_root.unwrap_or(&self.workspace_root);
        let build_path = self
            .preferred_build_file(&workspace_root.join(dep_package))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "BUILD file not found for package '{}': checked BUILD.bazel and BUILD",
                    dep_package
                )
            })?;

        // Use cached BUILD file parser to avoid re-parsing the same file for multiple dependencies
        // This dramatically reduces analysis time for packages with many interdependent targets
        debug_log!(
            "[MACRO DEP] Using macro-aware parsing for dependency: {}:{}",
            dep_package,
            dep_name
        );
        let build_file = match self.parse_build_file_cached(&build_path, repo_root) {
            Ok(bf) => {
                // Filter to only the requested target to avoid loading unnecessary dependencies
                let filtered_rules: Vec<_> = bf
                    .rules
                    .iter()
                    .filter(|r| r.name == dep_name)
                    .cloned()
                    .collect();
                debug_log!("[MACRO DEP] ✓ Parsed with macro support (cached), {} rules found (filtered to 1 target)", bf.rules.len());
                Arc::new(rush_parse::BuildFile {
                    path: bf.path.clone(),
                    rules: filtered_rules,
                })
            }
            Err(e) => {
                debug_log!("[MACRO DEP] ✗ Macro-aware parsing failed: {}", e);

                // Try smart fallbacks before full parsing (which triggers .bzl explosion)

                // Pattern 1: *_proto_cc targets are generated from tf_proto_library(*_proto)
                if dep_name.ends_with("_proto_cc") {
                    let base_name = dep_name.strip_suffix("_cc").unwrap();
                    debug_log!("[MACRO DEP] Trying base proto target: {}", base_name);

                    // Parse the base proto target with macro support (use cache)
                    match self.parse_build_file_cached(&build_path, repo_root) {
                        Ok(bf) => {
                            debug_log!("[MACRO DEP] ✓ Found base proto target: {}", base_name);
                            debug_log!(
                                "[MACRO DEP]   Note: Using proto_library instead of _cc variant"
                            );

                            // Find and build the proto rule
                            let proto_rule = bf
                                .rules
                                .iter()
                                .find(|r| r.name == base_name)
                                .ok_or_else(|| {
                                    anyhow::anyhow!(
                                        "Proto target '{}' not found in BUILD file",
                                        base_name
                                    )
                                })?;

                            // Build the proto_library target to generate .pb.h files
                            return self
                                .build_target(&bf, proto_rule, dep_package, repo_root)
                                .await;
                        }
                        Err(_) => {
                            // Proto target not found, continue to other fallbacks
                        }
                    }
                }

                // Pattern 2: For other missing targets, return empty for now
                // This prevents .bzl explosion from full parsing
                // Header-only libraries can tolerate missing dependencies
                debug_log!(
                    "[LAZY DEP] ⚠️  Dependency not found, returning empty (header-only compatible)"
                );
                debug_log!(
                    "[LAZY DEP]   This prevents .bzl loading explosion from full BUILD parsing"
                );
                return Ok(vec![]);

                // DISABLED: Full parsing triggers .bzl explosion
                // self.parse_build_file_cached(&build_path, repo_root)?
            }
        };

        // Find the target rule
        let dep_rule = build_file
            .rules
            .iter()
            .find(|r| r.name == dep_name)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Target '{}' not found in package '{}'",
                    dep_name,
                    dep_package
                )
            })?;

        // Build the target (passing repo_root to maintain external repository context)
        self.build_target(&build_file, dep_rule, dep_package, repo_root)
            .await
    }

    /// Build a cc_library target (compile all sources to .o files).
    /// Returns the object files from this library plus all transitive dependencies.
    async fn build_cc_library(
        &self,
        rule: &rush_parse::Rule,
        package: &str,
        dep_outputs: &[PathBuf],
        include_dirs: &[PathBuf],
    ) -> Result<Vec<PathBuf>> {
        let mut srcs = self.extract_srcs(rule)?;

        // For *_impl targets, resolve filegroup references to get actual sources
        if rule.name.ends_with("_impl") {
            debug_log!(
                "[IMPL] Detected _impl target: {}, resolving filegroups...",
                rule.name
            );
            let filegroup_refs = self.extract_filegroup_refs(rule).await?;
            debug_log!("[IMPL] Found {} filegroup references", filegroup_refs.len());

            for fg_ref in filegroup_refs {
                match self.resolve_filegroup_to_sources(&fg_ref, package).await {
                    Ok(mut fg_srcs) => {
                        debug_log!(
                            "[IMPL] Resolved filegroup '{}' to {} sources",
                            fg_ref,
                            fg_srcs.len()
                        );
                        srcs.append(&mut fg_srcs);
                    }
                    Err(e) => {
                        debug_log!(
                            "[IMPL] Warning: Failed to resolve filegroup '{}': {}",
                            fg_ref,
                            e
                        );
                        // Continue anyway - some filegroups might be headers only
                    }
                }
            }

            debug_log!(
                "[IMPL] Total sources after filegroup resolution: {}",
                srcs.len()
            );
        }

        let num_srcs = srcs.len(); // Capture before srcs is moved

        eprintln!(
            "  {} {} source files",
            "📦".bold(),
            num_srcs.to_string().cyan()
        );

        // Track total actions
        self.progress.add_total(num_srcs);

        // Ensure output directories exist
        self.output_tree.ensure_dirs(package)?;

        // Use resource manager for dynamic memory-aware parallelism
        let resource_mgr = Arc::clone(&self.resource_manager);
        let mut join_set = JoinSet::new();

        // Ensure output directories exist
        self.output_tree.ensure_dirs(package)?;

        // Add workspace root to include directories so workspace-relative includes work
        let mut include_dirs_vec = vec![self.workspace_root.clone()];
        include_dirs_vec.extend_from_slice(include_dirs);

        // Honor Bazel `include_prefix` for this library's OWN compile: Bazel
        // adds the per-target virtual-includes root to the producer's own
        // quote-include path, so a source in this same library can
        // `#include "<include_prefix>/..."`. Stage the farm (idempotent) and
        // put the root first. The source dir is the in-workspace package dir;
        // external-repo producers stage their farm in resolve_external_dep
        // against the repo-relative package dir.
        let own_src_dir = self.workspace_root.join(package);
        if own_src_dir.is_dir() {
            if let Some(vroot) = self.stage_include_prefix(rule, &own_src_dir, package)? {
                if !include_dirs_vec.contains(&vroot) {
                    include_dirs_vec.insert(0, vroot);
                }
            }
        }

        // Honor strip_include_prefix (Bazel): headers under <repo>/<prefix>/… are
        // exposed with <prefix> stripped, so the compile needs -I<repo>/<prefix>.
        // protobuf's cc_library targets use strip_include_prefix="/src" → their
        // google/protobuf/*.h (and absl headers) resolve only with -I<repo>/src;
        // rush previously handled only the "." form, so protobuf .cc failed
        // (port.cc: std::variant/absl::string_view unfound). All protobuf targets
        // share one repo+src root, so adding this per-target self-suffices. Gated
        // under RUSH_ENABLE_BZLMOD to keep the certified path byte-identical.
        if rush_incremental::tracked::getenv_str("RUSH_ENABLE_BZLMOD").as_deref() == Some("1") {
            if let Some(Value::String(prefix)) = rule.attributes.get("strip_include_prefix") {
                let repo_root = if self.workspace_root.join(package).is_dir() {
                    self.workspace_root.clone()
                } else {
                    let external_dir = cache_paths::workspace_external_dir(&self.workspace_root);
                    rush_incremental::tracked::read_dir_names(&external_dir)
                        .ok()
                        .and_then(|names| {
                            names
                                .into_iter()
                                .map(|name| external_dir.join(name))
                                .find(|p| p.join(package).is_dir())
                        })
                        .unwrap_or_else(|| self.workspace_root.clone())
                };
                let inc = if let Some(rel) = prefix.strip_prefix('/') {
                    repo_root.join(rel) // repo-relative ("/src" → <repo>/src)
                } else if prefix == "." {
                    repo_root.join(package) // the package dir itself
                } else {
                    repo_root.join(package).join(prefix.as_str()) // package-relative
                };
                if inc.is_dir() && !include_dirs_vec.contains(&inc) {
                    include_dirs_vec.push(inc);
                }
            }
        }

        // Add genfiles directory for proto-generated headers (.pb.h files)
        let genfiles_dir = self.output_tree.config_dir().join("genfiles");
        include_dirs_vec.push(genfiles_dir);

        // Add /usr/local/include to prefer non-Debian packaged dependencies
        // Debian packages absl with versioned namespaces (absl::debian7::) which breaks
        // compatibility with protobuf. Use /usr/local absl if available (built from source).
        let usr_local_include = PathBuf::from("/usr/local/include");
        if usr_local_include.exists() {
            include_dirs_vec.push(usr_local_include);
        }

        // Add TensorFlow TSL (TensorFlow Shared Libraries) include path if it exists
        // TensorFlow headers include "tsl/..." paths that need this
        let tsl_path = self.workspace_root.join("third_party/xla/third_party/tsl");
        if tsl_path.exists() {
            include_dirs_vec.push(tsl_path);

            // TensorFlow TSL needs ml_dtypes headers (float8, bfloat16, int4)
            // ml_dtypes bundles its own Eigen version with required extensions
            let ml_dtypes_eigen = self.workspace_root.join("external/ml_dtypes_eigen");
            let ml_dtypes_path = self.workspace_root.join("external");

            if ml_dtypes_eigen.exists()
                && ml_dtypes_path.join("ml_dtypes/include/float8.h").exists()
            {
                // Use ml_dtypes' vendored Eigen (has get_integer_by_size template)
                include_dirs_vec.push(ml_dtypes_eigen);
                // Add ml_dtypes headers (external/ so that ml_dtypes/include/... works)
                include_dirs_vec.push(ml_dtypes_path);
            } else {
                // Fallback to system Eigen3 if ml_dtypes not available
                let eigen3_path = PathBuf::from("/usr/include/eigen3");
                if eigen3_path.exists() {
                    include_dirs_vec.push(eigen3_path);
                }
            }
        }

        // cc_library creates static archives, not shared libraries
        let is_shared_library = false;

        // AZF-2 (Pro 2026-05-21): filter srcs to compile-
        // eligible sources only. Headers / label-form refs in
        // `srcs` must NOT be compiled as translation units.
        // Three-way shared predicate with build_legacy_command_specs
        // and the scheduler analyze() so all three pipelines
        // agree on which srcs become CppCompile actions.
        let srcs: Vec<String> = srcs
            .into_iter()
            .filter(|s| rush_rules::cc_command_spec::is_compilable_source(s))
            .collect();

        // Spawn compilation tasks
        for src in srcs {
            let resource_handle = resource_mgr.acquire(ResourceRequest::cpp_compile()).await?;
            let workspace = self.workspace_root.clone();
            let cache = Arc::clone(&self.cache);
            let output_tree = Arc::clone(&self.output_tree);
            let progress = Arc::clone(&self.progress);
            let compdb = Arc::clone(&self.compdb);
            let streaming = self
                .streaming_enabled
                .load(std::sync::atomic::Ordering::Acquire);
            let package_str = package.to_string();
            let includes = include_dirs_vec.clone();

            join_set.spawn(async move {
                debug_log!("[TRACE] Task starting for {}", src);
                let result = Self::compile_source_impl(CompileSourceParams {
                    workspace_root: &workspace,
                    cache: &cache,
                    output_tree: &output_tree,
                    progress: &progress,
                    compdb: &compdb,
                    remote_executor: None,
                    streaming_enabled: streaming,
                    src: &src,
                    package: &package_str,
                    include_dirs: &includes,
                    is_shared_library,
                })
                .await;
                debug_log!("[TRACE] compile_source_impl returned for {}", src);
                // Explicitly release resource asynchronously
                debug_log!(
                    "[TRACE] About to call resource_handle.release_async() for {}",
                    src
                );
                resource_handle.release_async().await;
                debug_log!("[TRACE] Resource released for {}", src);
                result
            });
        }

        // Collect results with fail-fast error handling
        debug_log!(
            "[TRACE] build_cc_library collecting results for {} sources in {}",
            num_srcs,
            package
        );
        let mut outputs = Vec::new();
        let mut completed = 0;
        while let Some(result) = join_set.join_next().await {
            completed += 1;
            debug_log!(
                "[TRACE] build_cc_library received result {}/{} for {}",
                completed,
                num_srcs,
                package
            );
            match result {
                Ok(Ok(obj_file)) => {
                    debug_log!(
                        "[TRACE] build_cc_library compilation OK: {}",
                        obj_file.display()
                    );
                    outputs.push(obj_file);
                }
                Ok(Err(e)) => {
                    join_set.abort_all(); // Cancel remaining compilations
                    return Err(e);
                }
                Err(e) => {
                    join_set.abort_all();
                    return Err(anyhow::anyhow!("Compilation task panicked: {}", e));
                }
            }
        }
        debug_log!(
            "[TRACE] build_cc_library all {} results collected for {}",
            completed,
            package
        );

        // Don't include transitive dependencies in our archive - only our own objects
        // Transitive deps will be linked when a binary uses this library

        // Create static library archive from object files
        if !outputs.is_empty() {
            let lib_name = format!("lib{}.a", rule.name);
            let lib_path = self.output_tree.lib_path(package, &lib_name);

            eprintln!(
                "  {} Archiving {} object files into {}",
                "📚".bold(),
                outputs.len().to_string().cyan(),
                lib_name.cyan()
            );

            // Create archive directory if needed
            if let Some(parent) = lib_path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            // Create archive action
            let archive_action = self.create_archive_action(&outputs, &lib_path)?;

            // Execute archive action
            self.progress.add_total(1);
            if self
                .streaming_enabled
                .load(std::sync::atomic::Ordering::Acquire)
            {
                self.progress.print_action("CppArchive", &lib_name);
            }

            self.execute_action(&archive_action).await?;
            self.progress.complete_action();

            // Return the library archive path (and include transitive deps for linking)
            let mut result = vec![lib_path];
            result.extend_from_slice(dep_outputs);
            Ok(result)
        } else {
            // No sources to compile, just pass through dependencies
            Ok(dep_outputs.to_vec())
        }
    }

    /// Build a cc_binary target (link object files into executable).
    async fn build_cc_binary(
        &self,
        rule: &rush_parse::Rule,
        package: &str,
        dep_objects: &[PathBuf],
        dep_outputs_by_label: &HashMap<String, Vec<PathBuf>>,
        include_dirs: &[PathBuf],
    ) -> Result<Vec<PathBuf>> {
        // Resolve explicit Bazel linkshared mode while retaining the legacy
        // no-attribute macro/name defaults. Shared outputs require PIC.
        let is_shared_library = legacy_cc_binary_shared_mode(rule)?;
        let (expanded_linkopts, linkopt_inputs) =
            legacy_expand_cc_binary_linkopts(rule, package, dep_outputs_by_label)?;

        // Compile our own sources first
        let srcs = self.extract_srcs(rule)?;
        let mut all_objects = Vec::new();

        // Track total actions
        self.progress.add_total(srcs.len() + 1); // +1 for link

        // Ensure output directories exist
        self.output_tree.ensure_dirs(package)?;

        eprintln!(
            "  {} {} source files",
            "📦".bold(),
            srcs.len().to_string().cyan()
        );

        // Add workspace root to include directories so workspace-relative includes work
        // (e.g., #include "tensorflow/core/framework/op.h")
        let mut include_dirs_vec = vec![self.workspace_root.clone()];
        include_dirs_vec.extend_from_slice(include_dirs);

        // Add /usr/local/include to prefer non-Debian packaged dependencies
        // Debian packages absl with versioned namespaces (absl::debian7::) which breaks
        // compatibility with protobuf. Use /usr/local absl if available (built from source).
        let usr_local_include = PathBuf::from("/usr/local/include");
        if usr_local_include.exists() {
            include_dirs_vec.push(usr_local_include);
        }

        // Add TensorFlow TSL (TensorFlow Shared Libraries) include path if it exists
        // TensorFlow headers include "tsl/..." paths that need this
        let tsl_path = self.workspace_root.join("third_party/xla/third_party/tsl");
        if tsl_path.exists() {
            include_dirs_vec.push(tsl_path);

            // TensorFlow TSL needs ml_dtypes headers (float8, bfloat16, int4)
            // ml_dtypes bundles its own Eigen version with required extensions
            let ml_dtypes_eigen = self.workspace_root.join("external/ml_dtypes_eigen");
            let ml_dtypes_path = self.workspace_root.join("external");

            if ml_dtypes_eigen.exists()
                && ml_dtypes_path.join("ml_dtypes/include/float8.h").exists()
            {
                // Use ml_dtypes' vendored Eigen (has get_integer_by_size template)
                include_dirs_vec.push(ml_dtypes_eigen);
                // Add ml_dtypes headers (external/ so that ml_dtypes/include/... works)
                include_dirs_vec.push(ml_dtypes_path);
            } else {
                // Fallback to system Eigen3 if ml_dtypes not available
                let eigen3_path = PathBuf::from("/usr/include/eigen3");
                if eigen3_path.exists() {
                    include_dirs_vec.push(eigen3_path);
                }
            }
        }

        // Parallel compilation for multiple sources (use resource manager)
        if srcs.len() > 1 {
            let resource_mgr = Arc::clone(&self.resource_manager);
            let mut join_set = JoinSet::new();
            for src in srcs {
                let resource_handle = resource_mgr.acquire(ResourceRequest::cpp_compile()).await?;
                let workspace = self.workspace_root.clone();
                let cache = Arc::clone(&self.cache);
                let output_tree = Arc::clone(&self.output_tree);
                let progress = Arc::clone(&self.progress);
                let compdb = Arc::clone(&self.compdb);
                let streaming = self
                    .streaming_enabled
                    .load(std::sync::atomic::Ordering::Acquire);
                let package_str = package.to_string();
                let includes = include_dirs_vec.clone();

                join_set.spawn(async move {
                    let _resource_guard = resource_handle;
                    Self::compile_source_impl(CompileSourceParams {
                        workspace_root: &workspace,
                        cache: &cache,
                        output_tree: &output_tree,
                        progress: &progress,
                        compdb: &compdb,
                        remote_executor: None,
                        streaming_enabled: streaming,
                        src: &src,
                        package: &package_str,
                        include_dirs: &includes,
                        is_shared_library,
                    })
                    .await
                });
            }

            // Collect results with fail-fast error handling
            while let Some(result) = join_set.join_next().await {
                match result {
                    Ok(Ok(obj_file)) => all_objects.push(obj_file),
                    Ok(Err(e)) => {
                        join_set.abort_all();
                        return Err(e);
                    }
                    Err(e) => {
                        join_set.abort_all();
                        return Err(anyhow::anyhow!("Compilation task panicked: {}", e));
                    }
                }
            }
        } else {
            // Single source - compile sequentially
            for src in &srcs {
                let obj_file = self
                    .compile_source(src, package, &include_dirs_vec, is_shared_library)
                    .await?;
                all_objects.push(obj_file);
            }
        }

        // Add dependency objects
        all_objects.extend_from_slice(dep_objects);

        // A location-referenced file is an action input, not a positional
        // object. TensorFlow places generated version scripts in `deps` so
        // they are built before the binary; remove those files from the
        // ordinary object/archive stream after resolving the linkopt.
        let linkopt_input_set: HashSet<_> = linkopt_inputs.iter().cloned().collect();
        all_objects.retain(|path| {
            !linkopt_input_set.contains(path)
                || legacy_cc_binary_positional_link_input(path)
        });

        // Deduplicate object files (same transitive dep may appear through multiple paths)
        // CRITICAL: Maintain link order - .o files must come BEFORE .a files
        // Unix linkers process files left-to-right and resolve symbols lazily.
        // Objects using symbols must appear before libraries defining them.
        all_objects.sort_by(|a, b| {
            let a_is_archive = a.extension().and_then(|s| s.to_str()) == Some("a");
            let b_is_archive = b.extension().and_then(|s| s.to_str()) == Some("a");

            match (a_is_archive, b_is_archive) {
                (false, true) => std::cmp::Ordering::Less, // .o before .a
                (true, false) => std::cmp::Ordering::Greater, // .a after .o
                _ => a.cmp(b),                             // Same type: alphabetical
            }
        });
        all_objects.dedup();

        // Handle edge case: no sources and no dependencies
        // Some TensorFlow targets (like tf_cc_shared_object wrappers) have no sources
        // and only declare dependencies. These are pure dependency aggregators.
        if all_objects.is_empty() {
            eprintln!(
                "  {} {} {}",
                "ℹ️".bold(),
                "No sources or dependencies to link".yellow(),
                "(dependency aggregator)".bright_black()
            );
            // Return empty - this target produces no artifacts, only dependencies
            // HACK: dependency-only aggregator produces nothing
            return Ok(Vec::default());
        }

        // Link into executable
        eprintln!(
            "  {} Linking {} object files",
            "🔗".bold(),
            all_objects.len().to_string().cyan()
        );

        let executable = self.output_tree.bin_path(package, &rule.name);
        let link_action = Self::create_link_action(
            &all_objects,
            &executable,
            is_shared_library,
            &expanded_linkopts,
            &linkopt_inputs,
        )?;

        // Execute link. `execute_action()` owns the link cache lookup and the
        // output-presence check, so the binary path does not need to repeat
        // that work here.
        let result = self.execute_action(&link_action).await?;

        if result.cached {
            self.progress.print_action(
                "✓",
                &format!("{} {}", &rule.name.bright_black(), "(cached)".green()),
            );
            return Ok(vec![executable]);
        }

        if result.exit_code != 0 {
            debug_log!("\n{}", "❌ Link failed".red().bold());
            debug_log!("{} {}", "Target:".yellow(), &rule.name);
            if !result.stderr.is_empty() {
                debug_log!("\n{}", "Error output:".yellow());
                debug_log!("{}", String::from_utf8_lossy(&result.stderr));
            }
            anyhow::bail!("Link failed with exit code {}", result.exit_code);
        }
        self.progress.complete_action();

        Ok(vec![executable])
    }

    /// Build a cc_test target (compile and link into test executable).
    async fn build_cc_test(
        &self,
        rule: &rush_parse::Rule,
        package: &str,
        dep_objects: &[PathBuf],
        include_dirs: &[PathBuf],
    ) -> Result<Vec<PathBuf>> {
        // cc_test is identical to cc_binary in terms of compilation and linking
        // The difference is in execution (tests are run, binaries are just built)

        // Compile our own sources first
        let srcs = self.extract_srcs(rule)?;
        let mut all_objects = Vec::new();

        // Track total actions
        self.progress.add_total(srcs.len() + 1); // +1 for link

        // Ensure output directories exist
        self.output_tree.ensure_dirs(package)?;

        eprintln!(
            "  {} {} test source files",
            "🧪".bold(),
            srcs.len().to_string().cyan()
        );

        // Add workspace root to include directories so workspace-relative includes work
        let mut include_dirs_vec = vec![self.workspace_root.clone()];
        include_dirs_vec.extend_from_slice(include_dirs);

        // Add TensorFlow TSL (TensorFlow Shared Libraries) include path if it exists
        // TensorFlow headers include "tsl/..." paths that need this
        let tsl_path = self.workspace_root.join("third_party/xla/third_party/tsl");
        if tsl_path.exists() {
            include_dirs_vec.push(tsl_path);

            // TensorFlow TSL needs ml_dtypes headers (float8, bfloat16, int4)
            // ml_dtypes bundles its own Eigen version with required extensions
            let ml_dtypes_eigen = self.workspace_root.join("external/ml_dtypes_eigen");
            let ml_dtypes_path = self.workspace_root.join("external");

            if ml_dtypes_eigen.exists()
                && ml_dtypes_path.join("ml_dtypes/include/float8.h").exists()
            {
                // Use ml_dtypes' vendored Eigen (has get_integer_by_size template)
                include_dirs_vec.push(ml_dtypes_eigen);
                // Add ml_dtypes headers (external/ so that ml_dtypes/include/... works)
                include_dirs_vec.push(ml_dtypes_path);
            } else {
                // Fallback to system Eigen3 if ml_dtypes not available
                let eigen3_path = PathBuf::from("/usr/include/eigen3");
                if eigen3_path.exists() {
                    include_dirs_vec.push(eigen3_path);
                }
            }
        }

        // Tests are not shared libraries
        let is_shared_library = false;

        // Parallel compilation for multiple sources (use resource manager)
        if srcs.len() > 1 {
            let resource_mgr = Arc::clone(&self.resource_manager);
            let mut join_set = JoinSet::new();
            for src in srcs {
                let resource_handle = resource_mgr.acquire(ResourceRequest::cpp_compile()).await?;
                let workspace = self.workspace_root.clone();
                let cache = Arc::clone(&self.cache);
                let output_tree = Arc::clone(&self.output_tree);
                let progress = Arc::clone(&self.progress);
                let compdb = Arc::clone(&self.compdb);
                let streaming = self
                    .streaming_enabled
                    .load(std::sync::atomic::Ordering::Acquire);
                let package_str = package.to_string();
                let includes = include_dirs_vec.clone();

                join_set.spawn(async move {
                    let _resource_guard = resource_handle;
                    Self::compile_source_impl(CompileSourceParams {
                        workspace_root: &workspace,
                        cache: &cache,
                        output_tree: &output_tree,
                        progress: &progress,
                        compdb: &compdb,
                        remote_executor: None,
                        streaming_enabled: streaming,
                        src: &src,
                        package: &package_str,
                        include_dirs: &includes,
                        is_shared_library,
                    })
                    .await
                });
            }

            // Collect results with fail-fast error handling
            while let Some(result) = join_set.join_next().await {
                match result {
                    Ok(Ok(obj_file)) => all_objects.push(obj_file),
                    Ok(Err(e)) => {
                        join_set.abort_all();
                        return Err(e);
                    }
                    Err(e) => {
                        join_set.abort_all();
                        return Err(anyhow::anyhow!("Compilation task panicked: {}", e));
                    }
                }
            }
        } else {
            // Single source - compile sequentially
            for src in &srcs {
                let obj_file = self
                    .compile_source(src, package, &include_dirs_vec, is_shared_library)
                    .await?;
                all_objects.push(obj_file);
            }
        }

        // Add dependency objects
        all_objects.extend_from_slice(dep_objects);

        // Deduplicate object files (same transitive dep may appear through multiple paths)
        all_objects.sort();
        all_objects.dedup();

        // Link into test executable
        eprintln!(
            "  {} Linking test {} object files",
            "🔗".bold(),
            all_objects.len().to_string().cyan()
        );

        let test_executable = self.output_tree.bin_path(package, &rule.name);
        let test_is_shared = test_executable
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.ends_with(".so"))
            .unwrap_or(false);
        let link_action = Self::create_link_action(
            &all_objects,
            &test_executable,
            test_is_shared,
            &[],
            &[],
        )?;

        // Execute link. `execute_action()` owns the link cache lookup and the
        // output-presence check, so the test path does not need to repeat that
        // work here.
        let result = self.execute_action(&link_action).await?;

        if result.cached {
            self.progress.print_action(
                "✓",
                &format!("{} {}", &rule.name.bright_black(), "(cached)".green()),
            );
            return Ok(vec![test_executable]);
        }

        if result.exit_code != 0 {
            debug_log!("\n{}", "❌ Test link failed".red().bold());
            debug_log!("{} {}", "Target:".yellow(), &rule.name);
            if !result.stderr.is_empty() {
                debug_log!("\n{}", "Error output:".yellow());
                debug_log!("{}", String::from_utf8_lossy(&result.stderr));
            }
            anyhow::bail!("Test link failed with exit code {}", result.exit_code);
        }
        self.progress.complete_action();

        Ok(vec![test_executable])
    }

    /// Compile a single source file to an object file (sequential version).
    async fn compile_source(
        &self,
        src: &str,
        package: &str,
        include_dirs: &[PathBuf],
        is_shared_library: bool,
    ) -> Result<PathBuf> {
        // Acquire resources before compilation (OOM prevention)
        let resource_handle = self
            .resource_manager
            .acquire(ResourceRequest::cpp_compile())
            .await?;

        let streaming = self
            .streaming_enabled
            .load(std::sync::atomic::Ordering::Acquire);
        let result = Self::compile_source_impl(CompileSourceParams {
            workspace_root: &self.workspace_root,
            cache: &self.cache,
            output_tree: &self.output_tree,
            progress: &self.progress,
            compdb: &self.compdb,
            remote_executor: self.remote_executor.clone(),
            streaming_enabled: streaming,
            src,
            package,
            include_dirs,
            is_shared_library,
        })
        .await;

        // Release resources after compilation
        resource_handle.release_async().await;

        result
    }

    /// Compile a single source file (static implementation for parallel execution).
    async fn compile_source_impl(params: CompileSourceParams<'_>) -> Result<PathBuf> {
        // Default: workspace-relative path
        let mut src_path = params.workspace_root.join(params.package).join(params.src);

        // Check if source doesn't exist at workspace path - might be in external repo
        if !src_path.exists() {
            // Try finding in external repositories
            let external_dir = cache_paths::workspace_external_dir(&params.workspace_root);
            if let Ok(entries) = std::fs::read_dir(&external_dir) {
                for entry in entries.flatten() {
                    let candidate = entry.path().join(params.package).join(params.src);
                    if candidate.exists() {
                        src_path = candidate;
                        break;
                    }
                }
            }
        }
        let obj_filename = format!("{}.o", params.src.trim_end_matches(".cc").replace('/', "_"));
        let obj_file = params.output_tree.obj_path(params.package, &obj_filename);

        // Get build configuration flags
        let mut config_flags = params.output_tree.config().compiler_flags();

        // Add -fPIC for shared libraries (position-independent code required for .so files)
        if params.is_shared_library {
            config_flags.push("-fPIC".to_string());
        }

        if std::env::var("RUSH_DUMP_CC_ARGV").is_ok() {
            eprintln!(
                "RUSH_CC_ARGV(legacy) src={} include_dirs={:?}",
                src_path.display(),
                params.include_dirs
            );
        }

        // Create compile action with include directories and config flags
        let action = Self::create_compile_action_static(
            &src_path,
            &obj_file,
            params.include_dirs,
            &config_flags,
        )?;

        // Record compilation in compilation database
        {
            let mut args = vec!["-c".to_string(), src_path.display().to_string()];
            args.extend_from_slice(&config_flags);
            for dir in params.include_dirs {
                args.push("-I".to_string());
                args.push(dir.display().to_string());
            }
            args.push("-o".to_string());
            args.push(obj_file.display().to_string());

            let mut db = params.compdb.lock().await;
            db.add_cpp_compile(
                src_path.clone(),
                "g++",
                args[1..].to_vec(), // Skip the "-c" since add_cpp_compile adds the compiler
                Some(obj_file.clone()),
            );
        }

        // Check cache - but also verify output file exists
        let action_t0 = std::time::Instant::now();
        if let Some(_cached) = params.cache.get(&action.id) {
            // Verify the output file actually exists on disk
            if obj_file.exists() {
                params.progress.record_cache_hit();
                params.progress.print_action(
                    "✓",
                    &format!("{} {}", params.src.bright_black(), "(cached)".green()),
                );
                if crate::action_timing::enabled() {
                    let owner = format!("{}/{}", params.package, params.src);
                    crate::action_timing::record(
                        &action,
                        Some(&owner),
                        action_t0.elapsed(),
                        Some(0),
                        true,
                    );
                }
                return Ok(obj_file);
            }
            // Cache hit but output missing - need to recompile
            debug_log!(
                "[WARN] Cache hit for {} but output file missing, recompiling",
                params.src
            );
        }

        params
            .progress
            .print_action("⚡", &format!("Compiling {}", params.src.cyan()));

        // Execute with remote or local strategy
        // Note: Resource management is handled by the caller (either parallel or sequential path)
        let action_exec_t0 = std::time::Instant::now();
        let result = if let Some(ref executor) = params.remote_executor {
            // Remote execution with local fallback
            if params.streaming_enabled {
                // Streaming not supported for remote, use local
                let strategy = LocalStrategy;
                strategy
                    .execute_streaming(&action, |line| {
                        params
                            .progress
                            .print_progress(&format!("  {}", line.bright_black()));
                    })
                    .await?
            } else {
                let local_fallback = async {
                    let strategy = LocalStrategy;
                    strategy.execute(&action).await
                };
                executor
                    .execute_with_fallback(&action, local_fallback)
                    .await?
            }
        } else {
            // Local execution only
            let strategy = LocalStrategy;
            if params.streaming_enabled {
                strategy
                    .execute_streaming(&action, |line| {
                        params
                            .progress
                            .print_progress(&format!("  {}", line.bright_black()));
                    })
                    .await?
            } else {
                strategy.execute(&action).await?
            }
        };
        if crate::action_timing::enabled() {
            let owner = format!("{}/{}", params.package, params.src);
            crate::action_timing::record(
                &action,
                Some(&owner),
                action_exec_t0.elapsed(),
                Some(result.exit_code),
                result.cached,
            );
        }

        if result.exit_code != 0 {
            debug_log!("\n{}", "❌ Compilation failed".red().bold());
            debug_log!("{} {}", "File:".yellow(), params.src);
            if !result.stderr.is_empty() {
                debug_log!("\n{}", "Error output:".yellow());
                debug_log!("{}", String::from_utf8_lossy(&result.stderr));
            }
            anyhow::bail!("Compilation failed: {}", params.src);
        }

        params.cache.put(action.id.clone(), result);
        params.progress.complete_action();
        Ok(obj_file)
    }

    /// Extract source files from a rule.
    fn extract_srcs(&self, rule: &rush_parse::Rule) -> Result<Vec<String>> {
        match rule.attributes.get("srcs") {
            Some(Value::List(list)) => {
                let mut sources = Vec::new();

                for v in list {
                    if let Value::String(s) = v {
                        // Check if this is a target label (filegroup reference)
                        if s.starts_with(':') || s.starts_with("//") || s.starts_with('@') {
                            // Skip target labels for now - they need async resolution
                            // We'll handle filegroup expansion in a separate pass
                            continue;
                        } else {
                            // Regular source file
                            sources.push(s.clone());
                        }
                    }
                }

                Ok(sources)
            }
            _ => Ok(vec![]),
        }
    }

    /// Extract filegroup references from srcs attribute (for resolution).
    async fn extract_filegroup_refs(&self, rule: &rush_parse::Rule) -> Result<Vec<String>> {
        match rule.attributes.get("srcs") {
            Some(Value::List(list)) => Ok(list
                .iter()
                .filter_map(|v| match v {
                    Value::String(s) if s.starts_with(':') || s.starts_with("//") => {
                        Some(s.clone())
                    }
                    _ => None,
                })
                .collect()),
            _ => Ok(vec![]),
        }
    }

    /// Resolve a filegroup reference to actual source file paths.
    ///
    /// Handles labels like:
    /// - `:filegroup_name` (same package)
    /// - `//package:filegroup_name` (cross-package)
    async fn resolve_filegroup_to_sources(
        &self,
        filegroup_ref: &str,
        current_package: &str,
    ) -> Result<Vec<String>> {
        // Parse the filegroup label
        let (fg_package, fg_name) = if let Some(rest) = filegroup_ref.strip_prefix("//") {
            // Cross-package reference: //package:name
            let parts: Vec<&str> = rest.splitn(2, ':').collect();
            if parts.len() != 2 {
                anyhow::bail!("Invalid filegroup reference: {}", filegroup_ref);
            }
            (parts[0].to_string(), parts[1].to_string())
        } else if let Some(name) = filegroup_ref.strip_prefix(':') {
            // Same package reference: :name
            (current_package.to_string(), name.to_string())
        } else {
            anyhow::bail!("Invalid filegroup reference: {}", filegroup_ref);
        };

        debug_log!(
            "[FILEGROUP] Resolving filegroup: package='{}', name='{}'",
            fg_package,
            fg_name
        );

        // Load the BUILD file containing the filegroup
        let build_path = find_build_file(&self.workspace_root.join(&fg_package))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "BUILD file not found for package '{}': checked BUILD.bazel and BUILD",
                    fg_package
                )
            })?;

        // Parse the BUILD file with macro support for the filegroup target
        let build_file = self.with_config_registry_scope(|| {
            rush_parse::parse_build_file_with_bzl_loader(
                &build_path,
                Some(Arc::clone(&self.bzl_loader)),
            )
        })?;

        // Find the filegroup rule
        let filegroup_rule = build_file
            .rules
            .iter()
            .find(|r| r.name == fg_name)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Filegroup '{}' not found in {}",
                    fg_name,
                    build_path.display()
                )
            })?;

        debug_log!(
            "[FILEGROUP] Found filegroup rule type: {}",
            filegroup_rule.rule_type
        );

        // Extract sources from the filegroup
        let srcs = match filegroup_rule.attributes.get("srcs") {
            Some(Value::List(list)) => {
                list.iter()
                    .filter_map(|v| match v {
                        Value::String(s) => {
                            // Skip nested filegroup references (would need recursion)
                            if s.starts_with(':') || s.starts_with("//") || s.starts_with('@') {
                                debug_log!("[FILEGROUP] Skipping nested filegroup: {}", s);
                                None
                            } else {
                                // Regular source file - if from different package, adjust path
                                // If fg_package == current_package, just use the source as-is
                                // If fg_package != current_package, need relative path from current to fg_package
                                let source_path = if fg_package == current_package {
                                    // Same package, source is already relative
                                    s.clone()
                                } else {
                                    // Different package - need to compute relative path
                                    // For now, use absolute path from current package
                                    // Current package: tensorflow/core
                                    // FG package: tensorflow/core/util
                                    // Source: file.cc
                                    // Result should be: util/file.cc (relative to current package)

                                    // Strip common prefix to get relative path
                                    if fg_package.starts_with(&format!("{}/", current_package)) {
                                        // Subpackage of current package
                                        let suffix = &fg_package[current_package.len() + 1..];
                                        format!("{}/{}", suffix, s)
                                    } else if current_package
                                        .starts_with(&format!("{}/", fg_package))
                                    {
                                        // Current package is subpackage of fg_package - go up
                                        // This case is rare, just use the source as-is
                                        s.clone()
                                    } else {
                                        // Completely different packages - use relative path from workspace
                                        format!("{}/{}", fg_package, s)
                                            .trim_start_matches(&format!("{}/", current_package))
                                            .to_string()
                                    }
                                };
                                Some(source_path)
                            }
                        }
                        _ => None,
                    })
                    .collect()
            }
            _ => Vec::new(),
        };

        debug_log!(
            "[FILEGROUP] Extracted {} sources from filegroup '{}'",
            srcs.len(),
            fg_name
        );

        Ok(srcs)
    }

    /// Create a compile action for a source file (static for parallel use).
    fn create_compile_action_static(
        src_path: &Path,
        obj_file: &Path,
        include_dirs: &[PathBuf],
        config_flags: &[String],
    ) -> Result<Action> {
        let inputs = vec![Artifact::Source(src_path.to_path_buf())];

        let mut args = vec!["-c".to_string(), src_path.display().to_string()];

        // Add build configuration flags (optimization, debug, etc.)
        args.extend_from_slice(config_flags);

        // Add include directories
        for dir in include_dirs {
            args.push("-I".to_string());
            args.push(dir.display().to_string());
        }

        args.push("-o".to_string());
        args.push(obj_file.display().to_string());

        let command = Command {
            executable: PathBuf::from("/usr/bin/g++"),
            args,
            working_dir: None,
            execution_info: ExecutionInfo::default(),
        };

        let env = HashMap::new();
        let id = Action::compute_id(&inputs, &command, &env)?;

        let outputs = vec![Artifact::Derived {
            action: id.clone(),
            output_path: obj_file.to_path_buf(),
        }];

        Ok(Action {
            id,
            inputs,
            outputs,
            command,
            env,
            mnemonic: "CppCompile".to_string(),
        })
    }

    /// Create a link action for object files.
    fn create_link_action(
        obj_files: &[PathBuf],
        executable: &Path,
        is_shared_library: bool,
        linkopts: &[String],
        linkopt_inputs: &[PathBuf],
    ) -> Result<Action> {
        let mut inputs: Vec<_> = obj_files
            .iter()
            .map(|path| Self::source_value_artifact(path))
            .collect();
        let mut input_paths: HashSet<_> = obj_files.iter().cloned().collect();
        for path in linkopt_inputs {
            if input_paths.insert(path.clone()) {
                inputs.push(Self::source_value_artifact(path));
            }
        }

        let mut args: Vec<String> = Vec::new();

        // Add -shared flag for shared libraries
        if is_shared_library {
            args.push("-shared".to_string());
        }

        let mut link_flags = Vec::new();
        // Check if we're linking proto-generated code (need protobuf runtime)
        let has_proto_objects = obj_files.iter().any(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.ends_with(".pb.o"))
                .unwrap_or(false)
        });

        // Add protobuf library if needed. #32: prefer TF's pinned protobuf
        // 3.21.9 static archive when materialized (system protobuf 4.25 breaks
        // op-gen TextFormat parsing of PrintV2's newline-default attr); else
        // fall back to system -lprotobuf.
        if has_proto_objects {
            link_flags.extend(rush_rules::rules_system::protobuf_runtime_link_flags());
        }

        // Add absl library for absl::Status and other absl symbols
        if is_shared_library {
            link_flags.push("-labsl_status".to_string());
            link_flags.push("-labsl_base".to_string());
            link_flags.push("-labsl_strings".to_string());
        }

        if let Some(flag) = rush_rules::rules_cc::fast_linker_driver_flag() {
            link_flags.push(flag);
        }
        if let Some(flag) = Self::legacy_mold_small_link_threads_flag(
            link_flags
                .iter()
                .find(|flag| flag.as_str() == "-fuse-ld=mold")
                .map(|s| s.as_str()),
            obj_files,
            &link_flags,
        ) {
            link_flags.push(flag);
        }

        args.extend(link_flags);

        // Add object files and static libraries
        // Note: .a archives can be passed directly to g++, it will extract and link them
        for obj_file in obj_files {
            args.push(obj_file.display().to_string());
        }

        args.extend(linkopts.iter().cloned());

        args.push("-o".to_string());
        args.push(executable.display().to_string());

        let command = Command {
            executable: PathBuf::from("/usr/bin/g++"),
            args,
            working_dir: None,
            execution_info: ExecutionInfo::default(),
        };

        let env = HashMap::new();
        let id = Action::compute_id(&inputs, &command, &env)?;

        let outputs = vec![Artifact::Derived {
            action: id.clone(),
            output_path: executable.to_path_buf(),
        }];

        Ok(Action {
            id,
            inputs,
            outputs,
            command,
            env,
            mnemonic: "CppLink".to_string(),
        })
    }

    /// Create an archive action (ar rcs) for creating static libraries
    fn create_archive_action(&self, obj_files: &[PathBuf], archive: &Path) -> Result<Action> {
        let inputs: Vec<_> = obj_files
            .iter()
            .map(|path| Self::source_value_artifact(path))
            .collect();

        // #39 link fix: `ar rcs` only inserts/replaces archive members by
        // name — it never removes members absent from the current obj list.
        // After a source's object name changed (e.g. the cross-package-label
        // fix renamed tracing.o → tsl_platform_tracing.o) the OLD member
        // lingers in libtracing.a, yielding a stale duplicate symbol at link
        // (multiple definition of tsl::tracing::*). The scheduler CcArchive
        // path already removes the archive first (build_orchestrator_ext.rs);
        // mirror that here in the legacy path by building the archive fresh:
        // `rm -f <archive> && ar rcs <archive> <objs...>`. Shell-quote every
        // path so spaces/odd chars in rush-out can't break the command.
        let q = |p: &Path| -> String { format!("'{}'", p.display().to_string().replace('\'', "'\\''")) };
        let mut ar_line = format!("rm -f {} && /usr/bin/ar rcs {}", q(archive), q(archive));
        for obj in obj_files {
            ar_line.push(' ');
            ar_line.push_str(&q(obj));
        }
        let args = vec!["-c".to_string(), ar_line];

        let command = Command {
            executable: PathBuf::from("/bin/sh"),
            args,
            working_dir: None,
            execution_info: ExecutionInfo::default(),
        };

        let env = HashMap::new();
        let id = Action::compute_id(&inputs, &command, &env)?;

        let outputs = vec![Artifact::Derived {
            action: id.clone(),
            output_path: archive.to_path_buf(),
        }];

        Ok(Action {
            id,
            inputs,
            outputs,
            command,
            env,
            mnemonic: "CppArchive".to_string(),
        })
    }

    /// Build a rust_library target.
    async fn build_rust_library(
        &self,
        rule: &rush_parse::Rule,
        package: &str,
    ) -> Result<Vec<PathBuf>> {
        use crate::rules_rust::RustLibrary;

        let rust_lib = RustLibrary::from_rule(rule)?;
        self.progress.add_total(1);

        eprintln!("  {} Rust library", "📦".bold());

        let output_dir = self.output_tree.bin_path(package, ".libs");
        std::fs::create_dir_all(&output_dir)?;

        let action = rust_lib.create_action(&self.workspace_root, package, &output_dir)?;

        // Check cache
        if let Some(_cached) = self.cache.get(&action.id) {
            self.progress.record_cache_hit();
            self.progress.print_action(
                "✓",
                &format!("{} {}", rule.name.bright_black(), "(cached)".green()),
            );
            return Ok(vec![output_dir.join(format!("lib{}.rlib", rule.name))]);
        }

        self.progress.print_action(
            "🦀",
            &format!("Compiling Rust library {}", rule.name.cyan()),
        );

        let result = self.execute_action(&action).await?;

        if result.exit_code != 0 {
            debug_log!("\n{}", "❌ Rust compilation failed".red().bold());
            if !result.stderr.is_empty() {
                debug_log!("{}", String::from_utf8_lossy(&result.stderr));
            }
            anyhow::bail!("Rust compilation failed");
        }

        self.cache.put(action.id, result);
        self.progress.complete_action();

        Ok(vec![output_dir.join(format!("lib{}.rlib", rule.name))])
    }

    /// Build a rust_binary target.
    async fn build_rust_binary(
        &self,
        rule: &rush_parse::Rule,
        package: &str,
    ) -> Result<Vec<PathBuf>> {
        use crate::rules_rust::RustBinary;

        let rust_bin = RustBinary::from_rule(rule)?;
        self.progress.add_total(1);

        eprintln!("  {} Rust binary", "⚙️".bold());

        let output_dir = self.output_tree.bin_path(package, ".");
        std::fs::create_dir_all(&output_dir)?;

        let action = rust_bin.create_action(&self.workspace_root, package, &output_dir)?;
        let output_file = output_dir.join(&rule.name);

        // Check cache
        if let Some(_cached) = self.cache.get(&action.id) {
            self.progress.record_cache_hit();
            self.progress.print_action(
                "✓",
                &format!("{} {}", rule.name.bright_black(), "(cached)".green()),
            );
            return Ok(vec![output_file]);
        }

        self.progress
            .print_action("🦀", &format!("Compiling Rust binary {}", rule.name.cyan()));

        let result = self.execute_action(&action).await?;

        if result.exit_code != 0 {
            debug_log!("\n{}", "❌ Rust compilation failed".red().bold());
            if !result.stderr.is_empty() {
                debug_log!("{}", String::from_utf8_lossy(&result.stderr));
            }
            anyhow::bail!("Rust compilation failed");
        }

        self.cache.put(action.id, result);
        self.progress.complete_action();

        Ok(vec![output_file])
    }

    /// Build a rust_test target.
    async fn build_rust_test(
        &self,
        rule: &rush_parse::Rule,
        package: &str,
    ) -> Result<Vec<PathBuf>> {
        use crate::rules_rust::RustTest;

        let rust_test = RustTest::from_rule(rule)?;
        self.progress.add_total(1);

        eprintln!("  {} Rust test", "🧪".bold());

        let output_dir = self.output_tree.bin_path(package, ".");
        std::fs::create_dir_all(&output_dir)?;

        let action = rust_test.create_action(&self.workspace_root, package, &output_dir)?;
        let output_file = output_dir.join(&rule.name);

        // Check cache
        if let Some(_cached) = self.cache.get(&action.id) {
            self.progress.record_cache_hit();
            self.progress.print_action(
                "✓",
                &format!("{} {}", rule.name.bright_black(), "(cached)".green()),
            );
            return Ok(vec![output_file]);
        }

        self.progress
            .print_action("🦀", &format!("Compiling Rust test {}", rule.name.cyan()));

        let result = self.execute_action(&action).await?;

        if result.exit_code != 0 {
            debug_log!("\n{}", "❌ Rust test compilation failed".red().bold());
            if !result.stderr.is_empty() {
                debug_log!("{}", String::from_utf8_lossy(&result.stderr));
            }
            anyhow::bail!("Rust test compilation failed");
        }

        self.cache.put(action.id, result);
        self.progress.complete_action();

        Ok(vec![output_file])
    }

    /// Build a py_library target.
    async fn build_py_library(
        &self,
        rule: &rush_parse::Rule,
        package: &str,
        repo_root: Option<&PathBuf>,
    ) -> Result<Vec<PathBuf>> {
        use crate::rules_python::PyLibrary;

        let py_lib = PyLibrary::from_rule(rule)?;

        // Use repo_root for external repositories
        let workspace_root = repo_root.unwrap_or(&self.workspace_root);

        // Get source paths
        let sources = py_lib.get_sources(workspace_root, package);

        // Validate that all source files exist
        for src_path in &sources {
            if !src_path.exists() {
                anyhow::bail!(
                    "Python source file not found: {}\n\
                     Required by py_library '{}' in package '{}'",
                    src_path.display(),
                    rule.name,
                    package
                );
            }
            if !src_path.is_file() {
                anyhow::bail!(
                    "Python source is not a file: {}\n\
                     Required by py_library '{}' in package '{}'",
                    src_path.display(),
                    rule.name,
                    package
                );
            }
        }

        eprintln!(
            "  {} Python library with {} sources",
            "📚".bold(),
            sources.len().to_string().cyan()
        );

        // Python libraries don't need compilation, just track source paths
        Ok(sources)
    }

    /// Build a py_binary target.
    async fn build_py_binary(
        &self,
        rule: &rush_parse::Rule,
        package: &str,
        repo_root: Option<&PathBuf>,
    ) -> Result<Vec<PathBuf>> {
        use crate::rules_python::PyBinary;

        let py_bin = PyBinary::from_rule(rule)?;
        self.progress.add_total(1);

        eprintln!("  {} Python binary", "🐍".bold());

        let output_dir = self.output_tree.bin_path(package, ".");
        std::fs::create_dir_all(&output_dir)?;

        // Use repo_root for external repositories
        let workspace_root = repo_root.unwrap_or(&self.workspace_root);

        let action = py_bin.create_action(workspace_root, package, &output_dir)?;
        let output_file = output_dir.join(&rule.name);

        // Check cache
        if let Some(_cached) = self.cache.get(&action.id) {
            self.progress.record_cache_hit();
            self.progress.print_action(
                "✓",
                &format!("{} {}", rule.name.bright_black(), "(cached)".green()),
            );
            return Ok(vec![output_file]);
        }

        self.progress.print_action(
            "🐍",
            &format!("Creating Python binary {}", rule.name.cyan()),
        );

        let result = self.execute_action(&action).await?;

        if result.exit_code != 0 {
            debug_log!("\n{}", "❌ Python binary creation failed".red().bold());
            if !result.stderr.is_empty() {
                debug_log!("{}", String::from_utf8_lossy(&result.stderr));
            }
            anyhow::bail!("Python binary creation failed");
        }

        self.cache.put(action.id, result);
        self.progress.complete_action();

        Ok(vec![output_file])
    }

    /// Build a py_test target.
    async fn build_py_test(
        &self,
        rule: &rush_parse::Rule,
        package: &str,
        repo_root: Option<&PathBuf>,
    ) -> Result<Vec<PathBuf>> {
        use crate::rules_python::PyTest;

        let py_test = PyTest::from_rule(rule)?;
        self.progress.add_total(1);

        eprintln!("  {} Python test", "🧪".bold());

        let output_dir = self.output_tree.bin_path(package, ".");
        std::fs::create_dir_all(&output_dir)?;

        // Use repo_root for external repositories
        let workspace_root = repo_root.unwrap_or(&self.workspace_root);

        let action = py_test.create_action(workspace_root, package, &output_dir)?;
        let output_file = output_dir.join(format!("{}_test", rule.name));

        // Check cache
        if let Some(_cached) = self.cache.get(&action.id) {
            self.progress.record_cache_hit();
            self.progress.print_action(
                "✓",
                &format!("{} {}", rule.name.bright_black(), "(cached)".green()),
            );
            return Ok(vec![output_file]);
        }

        self.progress
            .print_action("🐍", &format!("Creating Python test {}", rule.name.cyan()));

        let result = self.execute_action(&action).await?;

        if result.exit_code != 0 {
            debug_log!("\n{}", "❌ Python test creation failed".red().bold());
            if !result.stderr.is_empty() {
                debug_log!("{}", String::from_utf8_lossy(&result.stderr));
            }
            anyhow::bail!("Python test creation failed");
        }

        self.cache.put(action.id, result);
        self.progress.complete_action();

        Ok(vec![output_file])
    }

    /// Build a sh_library target.
    async fn build_sh_library(
        &self,
        rule: &rush_parse::Rule,
        package: &str,
    ) -> Result<Vec<PathBuf>> {
        use crate::rules_shell::ShLibrary;

        let sh_lib = ShLibrary::from_rule(rule)?;

        eprintln!("  {} Shell library", "📜".bold());

        // For sh_library, we just return the source files as outputs
        // (similar to filegroup - no actual build action needed)
        let sources = sh_lib.get_sources(&self.workspace_root, package);

        Ok(sources)
    }

    /// Build a sh_binary target.
    async fn build_sh_binary(
        &self,
        rule: &rush_parse::Rule,
        package: &str,
    ) -> Result<Vec<PathBuf>> {
        use crate::rules_shell::ShBinary;

        let sh_bin = ShBinary::from_rule(rule)?;
        self.progress.add_total(1);

        eprintln!("  {} Shell binary", "📜".bold());

        // Use bin_dir() instead of bin_path(., ".") to avoid path ending with "/.  "
        let output_dir = self.output_tree.bin_dir(package);
        std::fs::create_dir_all(&output_dir)?;

        let action = sh_bin.create_action(&self.workspace_root, package, &output_dir)?;
        let output_file = output_dir.join(&rule.name);

        // Check cache
        if let Some(_cached) = self.cache.get(&action.id) {
            self.progress.record_cache_hit();
            self.progress.print_action(
                "✓",
                &format!("{} {}", rule.name.bright_black(), "(cached)".green()),
            );
            return Ok(vec![output_file]);
        }

        self.progress
            .print_action("📜", &format!("Creating shell binary {}", rule.name.cyan()));

        let result = self.execute_action(&action).await?;

        if result.exit_code != 0 {
            debug_log!("\n{}", "❌ Shell binary creation failed".red().bold());
            if !result.stderr.is_empty() {
                debug_log!("{}", String::from_utf8_lossy(&result.stderr));
            }
            anyhow::bail!("Shell binary creation failed");
        }

        self.cache.put(action.id, result);
        self.progress.complete_action();

        Ok(vec![output_file])
    }

    /// Build a sh_test target.
    async fn build_sh_test(&self, rule: &rush_parse::Rule, package: &str) -> Result<Vec<PathBuf>> {
        use crate::rules_shell::ShTest;

        let sh_test = ShTest::from_rule(rule)?;
        self.progress.add_total(1);

        eprintln!("  {} Shell test", "🧪".bold());

        // Use bin_dir() instead of bin_path(., ".") to avoid path ending with "/."
        let output_dir = self.output_tree.bin_dir(package);
        std::fs::create_dir_all(&output_dir)?;

        let action = sh_test.create_action(&self.workspace_root, package, &output_dir)?;
        let output_file = output_dir.join(format!("{}_test", rule.name));

        // Check cache
        if let Some(_cached) = self.cache.get(&action.id) {
            self.progress.record_cache_hit();
            self.progress.print_action(
                "✓",
                &format!("{} {}", rule.name.bright_black(), "(cached)".green()),
            );
            return Ok(vec![output_file]);
        }

        self.progress
            .print_action("📜", &format!("Creating shell test {}", rule.name.cyan()));

        let result = self.execute_action(&action).await?;

        if result.exit_code != 0 {
            debug_log!("\n{}", "❌ Shell test creation failed".red().bold());
            if !result.stderr.is_empty() {
                debug_log!("{}", String::from_utf8_lossy(&result.stderr));
            }
            anyhow::bail!("Shell test creation failed");
        }

        self.cache.put(action.id, result);
        self.progress.complete_action();

        Ok(vec![output_file])
    }

    /// Build a genrule target.
    async fn build_genrule(&self, rule: &rush_parse::Rule, package: &str) -> Result<Vec<PathBuf>> {
        use crate::rules_genrule::GenRule;

        let genrule = GenRule::from_rule(rule)?;
        self.progress.add_total(1);

        eprintln!("  {} Genrule", "⚙️".bold());

        // Genrule outputs go to genfiles directory
        let output_dir = self.output_tree.genfiles_dir(package);
        std::fs::create_dir_all(&output_dir)?;

        let action = genrule.create_action(&self.workspace_root, package, &output_dir)?;

        // Collect all output files
        let output_files: Vec<PathBuf> = genrule
            .outs
            .iter()
            .map(|out| output_dir.join(out))
            .collect();

        // Check cache
        if let Some(_cached) = self.cache.get(&action.id) {
            self.progress.record_cache_hit();
            self.progress.print_action(
                "✓",
                &format!("{} {}", rule.name.bright_black(), "(cached)".green()),
            );
            return Ok(output_files);
        }

        self.progress
            .print_action("⚙️", &format!("Running genrule {}", rule.name.cyan()));

        let result = self.execute_action(&action).await?;

        if result.exit_code != 0 {
            debug_log!("\n{}", "❌ Genrule execution failed".red().bold());
            if !result.stderr.is_empty() {
                debug_log!("{}", String::from_utf8_lossy(&result.stderr));
            }
            if !result.stdout.is_empty() {
                debug_log!("{}", String::from_utf8_lossy(&result.stdout));
            }
            anyhow::bail!("Genrule '{}' failed", rule.name);
        }

        self.cache.put(action.id, result);
        self.progress.complete_action();

        Ok(output_files)
    }

    /// Build a tf_gen_op_wrappers_cc target (TensorFlow op code generation).
    ///
    /// NOT IMPLEMENTED: This requires TensorFlow operation code generation.
    async fn build_tf_gen_op_wrappers_cc(
        &self,
        rule: &rush_parse::Rule,
        package: &str,
    ) -> Result<Vec<PathBuf>> {
        anyhow::bail!(
            "tf_gen_op_wrappers_cc rule '{}' in package '{}' is not yet implemented.\n\
             \n\
             This rule generates C++ wrapper code for TensorFlow operations.\n\
             \n\
             To implement this properly, Rush needs:\n\
             1. TensorFlow OpDef protobuf parser\n\
             2. C++ code generation from operation definitions\n\
             3. Template system for TensorFlow C++ API wrappers\n\
             4. Integration with tf_gen_op_libs for operation registration\n\
             \n\
             This is a complex TensorFlow-specific code generation feature.\n\
             See UNIMPLEMENTED.md for tracking.\n\
             \n\
             Expected outputs that cannot be generated: {:?}",
            rule.name,
            package,
            rule.attributes.get("out_srcs")
        )
    }

    /// Build a tf_gen_op_libs target (TensorFlow op library generation).
    ///
    /// NOT IMPLEMENTED: This requires TensorFlow operation library code generation.
    async fn build_tf_gen_op_libs(
        &self,
        rule: &rush_parse::Rule,
        package: &str,
    ) -> Result<Vec<PathBuf>> {
        anyhow::bail!(
            "tf_gen_op_libs rule '{}' in package '{}' is not yet implemented.\n\
             \n\
             This rule generates C++ libraries for TensorFlow operation registration.\n\
             \n\
             To implement this properly, Rush needs:\n\
             1. TensorFlow OpDef protobuf parser and validator\n\
             2. Operation registration code generation\n\
             3. C++ compilation for generated operation libraries\n\
             4. Linking with TensorFlow runtime libraries\n\
             5. OpList proto generation for operation discovery\n\
             \n\
             This is a complex TensorFlow-specific build rule.\n\
             See UNIMPLEMENTED.md for tracking.\n\
             \n\
             Target: {}",
            rule.name,
            package,
            rule.name
        )
    }

    /// Build a proto_library target.
    async fn build_proto_library(
        &self,
        rule: &rush_parse::Rule,
        package: &str,
    ) -> Result<Vec<PathBuf>> {
        use crate::rules_proto::ProtoLibrary;

        let proto_lib = ProtoLibrary::from_rule(rule)?;

        eprintln!("  {} Proto library", "📄".bold());

        // Get proto source files
        let sources = proto_lib.get_sources(&self.workspace_root, package);

        if sources.is_empty() {
            // No sources - this is a collection of proto dependencies
            // HACK: proto-only deps have no artifacts
            return Ok(Vec::default());
        }

        // Generate C++ code from proto files
        let output_dir = self.output_tree.config_dir();
        let genfiles_dir = output_dir.join("genfiles");
        std::fs::create_dir_all(&genfiles_dir)?;

        eprintln!(
            "  {} Generating C++ from {} proto files",
            "🔄".bold(),
            sources.len()
        );

        for proto_file in &sources {
            // Run protoc to generate C++ code
            let output = std::process::Command::new(
                rush_rules::rules_system::protobuf_protoc_path(),
            )
                // proto3 optional needs this on protoc < 3.15; harmless on newer (Bazel always passes it).
                .arg("--experimental_allow_proto3_optional")
                .arg(format!("--cpp_out={}", genfiles_dir.display()))
                .arg(format!("--proto_path={}", self.workspace_root.display()))
                .arg(proto_file)
                .output()?;

            if !output.status.success() {
                debug_log!("\n{}", "❌ protoc failed".red().bold());
                debug_log!("{}", String::from_utf8_lossy(&output.stderr));
                anyhow::bail!("protoc failed for {}", proto_file.display());
            }

            // Verify generated files exist
            let proto_rel_path = proto_file
                .strip_prefix(&self.workspace_root)
                .unwrap_or(proto_file);
            let proto_base = proto_rel_path.with_extension("");
            let proto_name = proto_base.to_string_lossy();

            let generated_h = genfiles_dir.join(format!("{}.pb.h", proto_name));
            if !generated_h.exists() {
                anyhow::bail!(
                    "protoc did not generate expected file: {}",
                    generated_h.display()
                );
            }

            eprintln!(
                "     ✓ Generated {} from {}",
                generated_h.file_name().unwrap().to_string_lossy().cyan(),
                proto_file.file_name().unwrap().to_string_lossy()
            );
        }

        // Compile generated .pb.cc files
        let mut pb_cc_files = Vec::new();
        for proto_file in &sources {
            let proto_rel_path = proto_file
                .strip_prefix(&self.workspace_root)
                .unwrap_or(proto_file);
            let proto_base = proto_rel_path.with_extension("");
            let proto_name = proto_base.to_string_lossy();
            let pb_cc_file = genfiles_dir.join(format!("{}.pb.cc", proto_name));
            pb_cc_files.push(pb_cc_file);
        }

        // Compile the generated C++ files
        let mut object_files = Vec::new();
        for pb_cc_file in &pb_cc_files {
            // Replace .pb.cc with .pb.o (use .with_extension("o") not .with_extension("pb.o")!)
            let obj_file = pb_cc_file.with_extension("o");
            let obj_file_name = obj_file.file_name().unwrap().to_string_lossy();

            // Compile .pb.cc to .pb.o
            let mut compile_cmd = std::process::Command::new("g++");
            compile_cmd
                .arg("-c")
                .arg(pb_cc_file)
                .arg("-o")
                .arg(&obj_file)
                .arg("-I")
                .arg(&genfiles_dir)
                .arg("-I")
                .arg(&self.workspace_root)
                .arg("-I/usr/include")
                .arg("-std=c++17")
                .arg("-fPIC");

            eprintln!("     {} Compiling {}", "🔨".bold(), obj_file_name.cyan());

            let output = compile_cmd.output()?;
            if !output.status.success() {
                debug_log!("\n{}", "❌ Compilation failed".red().bold());
                debug_log!("{}", String::from_utf8_lossy(&output.stderr));
                anyhow::bail!(
                    "Failed to compile generated proto C++ file: {}",
                    pb_cc_file.display()
                );
            }

            object_files.push(obj_file);
        }

        // Return compiled object files (for linking)
        Ok(object_files)
    }

    /// Build a cc_proto_library target.
    async fn build_cc_proto_library(
        &self,
        rule: &rush_parse::Rule,
        package: &str,
        _dep_outputs: &[PathBuf],
    ) -> Result<Vec<PathBuf>> {
        use crate::rules_proto::CcProtoLibrary;

        let cc_proto = CcProtoLibrary::from_rule(rule)?;
        self.progress.add_total(1);

        eprintln!("  {} C++ proto library", "🔄".bold());

        // Get proto files from dependencies (proto_library targets)
        let mut proto_files = Vec::new();
        for dep_label in &cc_proto.deps {
            let (dep_package, dep_name) = if dep_label.starts_with("//") {
                parse_target_label(dep_label)?
            } else if dep_label.starts_with(':') {
                (
                    package.to_string(),
                    dep_label.strip_prefix(':').unwrap().to_string(),
                )
            } else {
                anyhow::bail!("Invalid proto dependency: {}", dep_label);
            };

            // Build the proto_library dependency to get proto file paths
            // TODO: Pass repo_root if we need to support external repo protos
            let proto_outputs = self
                .build_cross_package_dep(&dep_package, &dep_name, None)
                .await?;
            proto_files.extend(proto_outputs);
        }

        // Set up output directories
        let output_dir = self.output_tree.config_dir();
        let genfiles_dir = output_dir.join("genfiles");
        let obj_dir = self.output_tree.obj_dir(package);
        std::fs::create_dir_all(&genfiles_dir)?;
        std::fs::create_dir_all(&obj_dir)?;

        // Generate C++ code from proto files
        self.progress.print_action(
            "🔄",
            &format!("Generating C++ from proto files for {}", rule.name.cyan()),
        );

        let (generated_sources, _generated_headers) =
            cc_proto.generate_cpp_code(&proto_files, &self.workspace_root, &output_dir)?;

        // Create compile actions for generated .pb.cc files
        let actions =
            cc_proto.create_compile_actions(&generated_sources, &genfiles_dir, &obj_dir)?;

        // Execute all compile actions IN PARALLEL for max CPU utilization
        eprintln!(
            "  ⚡ Compiling {} proto-generated C++ files in parallel...",
            actions.len()
        );

        use rush_exec::LocalStrategy;
        use tokio::task::JoinSet;
        let mut join_set = JoinSet::new();
        let mut all_outputs = Vec::new();

        // Track total actions for progress
        self.progress.add_total(actions.len());

        for action in actions {
            // Check cache - but also verify output file exists
            if let Some(_cached) = self.cache.get(&action.id) {
                // Verify all output files exist
                let outputs_exist = action.outputs.iter().all(|a| match a {
                    Artifact::Source(p) => p.exists(),
                    _ => true,
                });

                if outputs_exist {
                    self.progress.record_cache_hit();
                    self.progress.print_action(
                        "✓",
                        &format!("{} {}", rule.name.bright_black(), "(cached)".green()),
                    );
                    all_outputs.extend(action.outputs.iter().filter_map(|a| match a {
                        Artifact::Source(p) => Some(p.clone()),
                        _ => None,
                    }));
                    self.progress.complete_action();
                    continue;
                } else {
                    // Cache hit but output missing - need to recompile
                    debug_log!(
                        "[WARN] Cache hit for {} but output file missing, recompiling",
                        rule.name
                    );
                }
            }

            // Spawn parallel compilation task
            let resource_manager = Arc::clone(&self.resource_manager);
            let action_id = action.id.clone();
            let action_outputs = action.outputs.clone();
            let resource_request = self.resource_request_for_action(&action);

            join_set.spawn(async move {
                // Acquire resources before execution
                let resource_handle = resource_manager.acquire(resource_request).await?;

                // Execute action using LocalStrategy
                let strategy = LocalStrategy;
                let result = strategy.execute(&action).await;

                // Release resources after execution
                resource_handle.release_async().await;

                Ok::<_, anyhow::Error>((action_id, action_outputs, result?))
            });
        }

        // Collect results from parallel executions
        while let Some(result) = join_set.join_next().await {
            match result {
                Ok(Ok((action_id, outputs, exec_result))) => {
                    if exec_result.exit_code != 0 {
                        join_set.abort_all();
                        debug_log!("\n{}", "❌ Proto compilation failed".red().bold());
                        if !exec_result.stderr.is_empty() {
                            debug_log!("{}", String::from_utf8_lossy(&exec_result.stderr));
                        }
                        anyhow::bail!("Proto compilation failed");
                    }

                    self.cache.put(action_id, exec_result);
                    self.progress.complete_action();

                    all_outputs.extend(outputs.iter().filter_map(|a| match a {
                        Artifact::Source(p) => Some(p.clone()),
                        _ => None,
                    }));
                }
                Ok(Err(e)) => {
                    join_set.abort_all();
                    return Err(e);
                }
                Err(e) => {
                    join_set.abort_all();
                    return Err(anyhow::anyhow!("Proto compilation task panicked: {}", e));
                }
            }
        }

        // Create static library archive from compiled proto objects
        // (similar to cc_library archiving)
        if !all_outputs.is_empty() {
            let lib_name = format!("lib{}.a", rule.name);
            let lib_path = self.output_tree.lib_path(package, &lib_name);

            eprintln!(
                "  {} Archiving {} proto object files into {}",
                "📚".bold(),
                all_outputs.len().to_string().cyan(),
                lib_name.cyan()
            );

            // Create archive directory if needed
            if let Some(parent) = lib_path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            let archive_action = self.create_archive_action(&all_outputs, &lib_path)?;
            self.execute_action(&archive_action).await?;
            self.progress.complete_action();

            // Return library archive path
            Ok(vec![lib_path])
        } else {
            // No proto sources to compile
            // HACK: proto rule re-exports dependencies only
            return Ok(Vec::default());
        }
    }
}

impl Default for BuildOrchestrator {
    fn default() -> Self {
        Self::new().expect("Failed to find workspace")
    }
}

#[cfg(test)]
mod cc_binary_linkshared_compat_tests {
    use super::*;

    #[test]
    fn explicit_linkshared_drives_recursive_orchestrator_link_mode() {
        let temp = tempfile::tempdir().unwrap();
        let object = temp.path().join("extension.o");
        let output = temp.path().join("binding_without_suffix");
        std::fs::write(&object, b"object fixture").unwrap();
        let rule = rush_parse::Rule {
            name: "binding_without_suffix".to_string(),
            rule_type: "cc_binary".to_string(),
            attributes: HashMap::from([("linkshared".to_string(), Value::Int(1))]),
        };

        let shared = legacy_cc_binary_shared_mode(&rule).unwrap();
        let action = BuildOrchestrator::create_link_action(
            &[object],
            &output,
            shared,
            &[],
            &[],
        )
        .unwrap();

        assert!(shared);
        assert_eq!(action.command.args.first().map(String::as_str), Some("-shared"));
        assert_eq!(
            action.command.args.last().map(String::as_str),
            output.to_str(),
        );

        let mut compatibility_rule = rush_parse::Rule {
            name: "legacy.so".to_string(),
            rule_type: "cc_binary".to_string(),
            attributes: HashMap::new(),
        };
        assert!(legacy_cc_binary_shared_mode(&compatibility_rule).unwrap());

        for value in [Value::Bool(false), Value::Int(0)] {
            compatibility_rule
                .attributes
                .insert("linkshared".to_string(), value);
            assert!(!legacy_cc_binary_shared_mode(&compatibility_rule).unwrap());
        }
        for value in [
            Value::Int(-1),
            Value::Int(2),
            Value::String("yes".to_string()),
        ] {
            compatibility_rule
                .attributes
                .insert("linkshared".to_string(), value);
            assert!(legacy_cc_binary_shared_mode(&compatibility_rule).is_err());
        }
    }

    #[test]
    fn recursive_linkshared_expands_location_and_wires_the_exact_input() {
        let temp = tempfile::tempdir().unwrap();
        let object = temp.path().join("extension.o");
        let version = temp.path().join("binding-version.lds");
        let output = temp.path().join("binding.so");
        std::fs::write(&object, b"object fixture").unwrap();
        std::fs::write(&version, b"{ global: rush_extension_value; local: *; };\n").unwrap();
        let rule = rush_parse::Rule {
            name: "binding.so".to_string(),
            rule_type: "cc_binary".to_string(),
            attributes: HashMap::from([
                ("linkshared".to_string(), Value::Int(1)),
                (
                    "linkopts".to_string(),
                    Value::List(vec![
                        Value::String("-Wl,--version-script".to_string()),
                        Value::String("$(location binding-version.lds)".to_string()),
                    ]),
                ),
            ]),
        };
        let outputs = HashMap::from([(
            "//pkg:binding-version.lds".to_string(),
            vec![version.clone()],
        )]);

        let (linkopts, linkopt_inputs) =
            legacy_expand_cc_binary_linkopts(&rule, "pkg", &outputs).unwrap();
        assert_eq!(
            linkopts,
            vec![
                "-Wl,--version-script".to_string(),
                version.display().to_string(),
            ],
        );
        let action = BuildOrchestrator::create_link_action(
            &[object],
            &output,
            legacy_cc_binary_shared_mode(&rule).unwrap(),
            &linkopts,
            &linkopt_inputs,
        )
        .unwrap();
        assert!(action.command.args.iter().any(|arg| arg == "-shared"));
        assert!(action.command.args.iter().any(|arg| arg == version.to_str().unwrap()));
        assert!(!action.command.args.iter().any(|arg| arg.contains("$(location")));
        assert!(action.inputs.iter().any(|input| match input {
            Artifact::Source(path) => path == &version,
            Artifact::Derived { output_path, .. } => output_path == &version,
        }));

        let unknown = legacy_expand_cc_binary_linkopts(&rule, "pkg", &HashMap::new())
            .expect_err("recursive path must reject an undeclared label");
        assert!(format!("{unknown:#}").contains("not an exact declared dependency"));
        let many = HashMap::from([(
            "//pkg:binding-version.lds".to_string(),
            vec![version, temp.path().join("other.lds")],
        )]);
        let multi = legacy_expand_cc_binary_linkopts(&rule, "pkg", &many)
            .expect_err("recursive path must reject a multi-output singular reference");
        assert!(format!("{multi:#}").contains("resolved to 2 direct files"));

        let ambiguous = HashMap::from([
            (":binding-version.lds".to_string(), vec![PathBuf::from("one")]),
            (
                "//pkg:binding-version.lds".to_string(),
                vec![PathBuf::from("two")],
            ),
        ]);
        let error = legacy_expand_cc_binary_linkopts(&rule, "pkg", &ambiguous)
            .expect_err("different outputs for one canonical label must be ambiguous");
        assert!(format!("{error:#}").contains("ambiguously matches"));

        assert!(!legacy_cc_binary_positional_link_input(Path::new("exports.lds")));
        for linkable in ["extension.o", "libdep.a", "libdep.so.2", "libdep.dylib"] {
            assert!(legacy_cc_binary_positional_link_input(Path::new(linkable)));
        }
    }
}

#[cfg(test)]
mod package_declaration_catalog_tests {
    use super::*;

    // TensorFlow v2.17.0 tensorflow/core/framework/registration/BUILD uses
    // these generator and consumer attributes. Loads and unrelated tests
    // are omitted so this exercises Rush's native macro and parser directly.
    // Source SHA256: 3ef1fc225fd8d20ac05a80723d7f54e4fa24ce85d4234bc0283273467d1aae14.
    const REGISTRATION_OPTIONS_BUILD: &str = r#"
tf_gen_options_header(
    name = "gen_registration_options",
    build_settings = {
        "//tensorflow:enable_registration_v2": "REGISTRATION_V2",
    },
    output_header = "options.h",
    template = "options.h.tpl",
)

cc_library(
    name = "registration",
    hdrs = ["options.h", "registration.h"],
)
"#;

    fn parse_registration_options_build(
        extra_rule: &str,
    ) -> (tempfile::TempDir, Arc<rush_parse::BuildFile>) {
        let root = tempfile::tempdir().unwrap();
        let package = root.path().join("tensorflow/core/framework/registration");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(root.path().join("WORKSPACE"), "").unwrap();
        let build_path = package.join("BUILD");
        std::fs::write(
            &build_path,
            format!("{REGISTRATION_OPTIONS_BUILD}\n{extra_rule}\n"),
        ).unwrap();
        let parsed = rush_incremental::tracked::test_scope(root.path(), || {
            rush_parse::parse_build_file_with_loader(
                &build_path,
                rush_starlark::BzlLoader::new(root.path().to_path_buf()),
            )
            .unwrap()
        });
        (root, parsed)
    }

    #[test]
    fn registration_macro_output_resolves_to_generator_without_an_explicit_alias() {
        let (_root, build_file) = parse_registration_options_build("");
        assert_eq!(build_file.rules.len(), 2);
        assert!(!build_file.rules.iter().any(|rule| rule.name == "options.h"));
        let generator = build_file.rules.iter()
            .find(|rule| rule.name == "gen_registration_options")
            .unwrap();
        assert_eq!(generator.rule_type, "tf_gen_options_header");
        assert_eq!(
            BuildOrchestrator::symbolic_declared_outputs(generator).unwrap(),
            vec!["options.h"]
        );
        let catalog = BuildOrchestrator::package_declaration_catalog(
            &build_file,
            None,
            "tensorflow/core/framework/registration",
        ).unwrap();
        assert_eq!(
            catalog.producer_for("options.h"),
            Some("gen_registration_options")
        );
        assert_eq!(catalog.producer_for("registration"), Some("registration"));
    }

    #[test]
    fn registration_macro_output_still_rejects_an_explicit_target_alias() {
        let (_root, build_file) = parse_registration_options_build(
            "alias(name = \"options.h\", actual = \":registration\")",
        );
        let error = BuildOrchestrator::package_declaration_catalog(
            &build_file,
            None,
            "tensorflow/core/framework/registration",
        ).expect_err("a declared output must not replace an explicit rule target");
        assert!(error.to_string().contains("conflicts with target"), "{error}");
    }

    fn rule(name: &str, rule_type: &str, attributes: HashMap<String, Value>) -> rush_parse::Rule {
        rush_parse::Rule {
            name: name.to_string(),
            rule_type: rule_type.to_string(),
            attributes,
        }
    }

    fn generated_header_fixture(competing_rule_type: &str) -> rush_parse::BuildFile {
        rush_parse::BuildFile {
            path: PathBuf::from("BUILD"),
            rules: vec![
                rule(
                    "options.h",
                    competing_rule_type,
                    HashMap::from([(
                        "path".to_string(),
                        Value::String("options.h".to_string()),
                    )]),
                ),
                rule(
                    "gen_registration_options",
                    "genrule",
                    HashMap::from([(
                        "outs".to_string(),
                        Value::List(vec![Value::String("options.h".to_string())]),
                    )]),
                ),
            ],
        }
    }

    #[test]
    fn generated_output_supersedes_source_file_target() {
        let build_file = generated_header_fixture("source_file");
        let catalog = BuildOrchestrator::package_declaration_catalog(
            &build_file,
            None,
            "tensorflow/core/framework/registration",
        )
        .unwrap();

        assert_eq!(
            catalog.producer_for("options.h"),
            Some("gen_registration_options")
        );
    }

    #[test]
    fn generated_output_still_rejects_real_target_collision() {
        let build_file = generated_header_fixture("cc_library");
        let error = BuildOrchestrator::package_declaration_catalog(
            &build_file,
            None,
            "tensorflow/core/framework/registration",
        )
        .expect_err("a real target must retain ownership of its label");

        assert!(error.to_string().contains("conflicts with target"));
    }
}

#[cfg(test)]
mod commit_97_tunable_parse_tests {
    use super::{parse_cache_bytes, parse_cache_entries};

    /// Phase 5.C.4 commit 97: entry-count parser accepts
    /// plain integers, rejects zero, rejects bad input.
    #[test]
    fn parse_entries_accepts_valid_values() {
        assert_eq!(parse_cache_entries("50"), Some(50));
        assert_eq!(parse_cache_entries("  500  "), Some(500));
        assert_eq!(parse_cache_entries("1"), Some(1));
    }

    #[test]
    fn parse_entries_rejects_zero() {
        assert_eq!(parse_cache_entries("0"), None);
    }

    #[test]
    fn parse_entries_rejects_bad_input() {
        assert_eq!(parse_cache_entries("abc"), None);
        assert_eq!(parse_cache_entries("100abc"), None);
        assert_eq!(parse_cache_entries(""), None);
    }

    /// Phase 5.C.4 commit 97: byte parser accepts plain
    /// integers + k/K/m/M/g/G/t/T suffixes (binary
    /// multipliers).
    #[test]
    fn parse_bytes_plain_integer() {
        assert_eq!(parse_cache_bytes("1024"), Some(1024));
        assert_eq!(parse_cache_bytes("12345"), Some(12345));
    }

    #[test]
    fn parse_bytes_kilobyte_suffix() {
        assert_eq!(parse_cache_bytes("1k"), Some(1024));
        assert_eq!(parse_cache_bytes("1K"), Some(1024));
        assert_eq!(parse_cache_bytes("500K"), Some(500 * 1024));
    }

    #[test]
    fn parse_bytes_megabyte_suffix() {
        assert_eq!(parse_cache_bytes("1M"), Some(1024 * 1024));
        assert_eq!(parse_cache_bytes("512m"), Some(512 * 1024 * 1024));
    }

    #[test]
    fn parse_bytes_gigabyte_suffix() {
        assert_eq!(parse_cache_bytes("1G"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_cache_bytes("2g"), Some(2 * 1024 * 1024 * 1024));
    }

    #[test]
    fn parse_bytes_terabyte_suffix() {
        assert_eq!(
            parse_cache_bytes("1T"),
            Some(1024usize * 1024 * 1024 * 1024)
        );
    }

    #[test]
    fn parse_bytes_rejects_zero() {
        assert_eq!(parse_cache_bytes("0"), None);
        assert_eq!(parse_cache_bytes("0G"), None);
    }

    #[test]
    fn parse_bytes_rejects_bad_suffix() {
        assert_eq!(parse_cache_bytes("100x"), None);
        assert_eq!(parse_cache_bytes("abc"), None);
        assert_eq!(parse_cache_bytes(""), None);
    }
}
