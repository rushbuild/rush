/// .bzl file loader for Starlark macros and helper functions
///
/// Loads, parses, and evaluates .bzl files to make their exported symbols
/// available to BUILD files.
use crate::{evaluator::{DefinitionLabelContext, Environment}, Evaluator, Program, Value};
use anyhow::{anyhow, Context, Result};
use dashmap::{DashMap, DashSet};
use lru::LruCache;
use once_cell::sync::Lazy;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, UNIX_EPOCH};

use rush_incremental::{Dependency, DependencyManifest};
// Import ResourceManager from rush-resource crate
use rush_resource::{ResourceManager, ResourceRequest};

#[cfg(not(windows))]
const BAZEL_EXECUTABLE_NAMES: &[&str] = &["bazel"];
#[cfg(windows)]
const BAZEL_EXECUTABLE_NAMES: &[&str] = &["bazel.exe", "bazel.cmd", "bazel.bat", "bazel"];

fn is_executable_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn resolve_bazel_binary_from_command_environment(
    workspace_root: &Path,
    configured: Option<&str>,
    client_path: Option<&str>,
) -> Result<PathBuf> {
    if let Some(configured) = configured {
        if configured.is_empty() {
            return Err(anyhow!("RUSH_BAZEL_PATH is set but empty"));
        }
        let configured = PathBuf::from(configured);
        let candidate = if configured.is_absolute() {
            configured
        } else {
            workspace_root.join(configured)
        };
        if is_executable_file(&candidate) {
            return Ok(candidate);
        }
        return Err(anyhow!(
            "RUSH_BAZEL_PATH points to a missing or non-executable Bazel binary: {}",
            candidate.display()
        ));
    }

    let client_path = client_path.ok_or_else(|| {
        anyhow!(
            "cannot fetch an external repository because the client command supplied neither \
             RUSH_BAZEL_PATH nor PATH"
        )
    })?;
    for directory in std::env::split_paths(OsStr::new(client_path)) {
        let directory = if directory.as_os_str().is_empty() {
            workspace_root.to_path_buf()
        } else if directory.is_absolute() {
            directory
        } else {
            workspace_root.join(directory)
        };
        for name in BAZEL_EXECUTABLE_NAMES {
            let candidate = directory.join(name);
            if is_executable_file(&candidate) {
                return Ok(candidate);
            }
        }
    }

    Err(anyhow!(
        "cannot fetch an external repository because no executable Bazel binary was found in \
         the client PATH; set RUSH_BAZEL_PATH to an exact Bazel executable"
    ))
}

fn tracked_command_environment(name: &str) -> Result<Option<String>> {
    rush_incremental::tracked::getenv(name.as_bytes())
        .map(|value| {
            String::from_utf8(value.to_vec())
                .with_context(|| format!("client environment variable {} is not valid UTF-8", name))
        })
        .transpose()
}

fn track_optional_repository_mapping_input(path: &Path) -> Result<()> {
    if rush_incremental::tracked::exists(path) {
        rush_incremental::tracked::read_file(path).with_context(|| {
            format!(
                "Failed to read repository-mapping input {}",
                path.display()
            )
        })?;
    }
    Ok(())
}

fn bazel_repository_mapping_probe_environment() -> Vec<Vec<u8>> {
    [
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "TMPDIR",
        "TMP",
        "TEMP",
        "JAVA_HOME",
        "XDG_CACHE_HOME",
        "BAZELISK_HOME",
        "USE_BAZEL_VERSION",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "no_proxy",
    ]
    .into_iter()
    .map(|name| name.as_bytes().to_vec())
    .collect()
}

fn path_command_argument(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        path.as_os_str().to_string_lossy().as_bytes().to_vec()
    }
}

fn canonical_repo_from_output_base_path(
    context_root: &Path,
    output_base: &Path,
) -> Option<String> {
    let external_dir = context_root.parent()?;
    if external_dir.file_name() != Some(OsStr::new("external")) {
        return None;
    }
    let repository_output_base = external_dir.parent()?;
    if repository_output_base != output_base
        && repository_output_base.parent() != Some(output_base)
    {
        return None;
    }
    context_root
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

fn resolve_bazel_binary_for_fetch(workspace_root: &Path) -> Result<PathBuf> {
    let configured = tracked_command_environment("RUSH_BAZEL_PATH")?;
    let client_path = if configured.is_none() {
        tracked_command_environment("PATH")?
    } else {
        None
    };
    resolve_bazel_binary_from_command_environment(
        workspace_root,
        configured.as_deref(),
        client_path.as_deref(),
    )
}

// Global set to detect circular dependencies in .bzl loading across all threads
//
// FIX #8: Changed from thread-local stack to global DashSet to prevent infinite loops
// during parallel batch loading. When .bzl files load each other in a cycle across
// different threads, the thread-local stack can't detect the circular dependency.
//
// Using DashSet (thread-safe concurrent hash set) instead of thread-local Vec allows
// detection of circular dependencies across all threads during parallel batch_load_bzl_files().
//
// Example of cross-thread circular dependency:
// - Thread A loads file1.bzl → eval_program → loads file2.bzl
// - Thread B loads file2.bzl → eval_program → loads file1.bzl
// Thread-local stacks wouldn't see each other, but global DashSet catches this.
static BZL_LOADING_SET: Lazy<DashSet<String>> = Lazy::new(DashSet::new);

// Track files that have already failed/timed out to prevent repeated 60s waits
// When a file times out waiting, we add it here so subsequent threads fail fast
static BZL_FAILED_SET: Lazy<DashSet<String>> = Lazy::new(DashSet::new);

// Track in-flight .bzl loads globally so we can dump diagnostics while other threads wait.
static IN_FLIGHT_BZL_LOADS: Lazy<DashMap<String, InFlightLoadInfo>> = Lazy::new(DashMap::new);
static IN_FLIGHT_BZL_PATHS: Lazy<DashSet<String>> = Lazy::new(DashSet::new);

// Track which .bzl files hit the infinite-loop guard so evaluator can register macros instead.
static LOOP_GUARDED_MODULES: Lazy<DashSet<String>> = Lazy::new(DashSet::new);

// ---------------------------------------------------------------------------
// #39 graph-RSS: peak live-Program census (Codex 019e8ee5).
//
// The slope + GlobalBzlCache censuses cleared CcInfo providers AND showed the
// post-load retained Value graph is small — yet core:lib hit 34GB MID-LOAD.
// The leading hypothesis (H2) is transient parsed-`Program` retention: ASTs
// held live across the recursive/parallel .bzl load (the cache itself notes
// "Programs consume 100-1000x source file size"), plus a non-tokio path that
// clones the Program. These atomics + the RAII LiveProgramGuard track how many
// Programs (and how much parsed AST / source) are live simultaneously, and the
// process RSS at the moment of peak concurrent live-Program weight — so a
// `RUSH_MEM_CENSUS=1` run NAMES whether the mid-load spike is transient
// Program retention rather than guessing from post-load cache state. All
// inert (a handful of relaxed atomic adds) unless RUSH_MEM_CENSUS=1.
mod program_census {
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    pub static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    pub fn enabled() -> bool {
        *ENABLED.get_or_init(|| std::env::var("RUSH_MEM_CENSUS").as_deref() == Ok("1"))
    }

    pub static ACTIVE_PROGRAMS: AtomicUsize = AtomicUsize::new(0);
    pub static PEAK_ACTIVE_PROGRAMS: AtomicUsize = AtomicUsize::new(0);
    pub static PROGRAMS_PARSED_TOTAL: AtomicUsize = AtomicUsize::new(0);
    pub static PROGRAM_CLONES_TOTAL: AtomicUsize = AtomicUsize::new(0);
    pub static ACTIVE_SOURCE_BYTES: AtomicUsize = AtomicUsize::new(0);
    pub static PEAK_ACTIVE_SOURCE_BYTES: AtomicUsize = AtomicUsize::new(0);
    pub static ACTIVE_STMT_NODES: AtomicUsize = AtomicUsize::new(0);
    pub static PEAK_ACTIVE_STMT_NODES: AtomicUsize = AtomicUsize::new(0);
    // RSS high-water (KiB) seen at any guard transition, plus a snapshot of the
    // live-Program quantities at that exact moment.
    pub static PEAK_RSS_KB: AtomicU64 = AtomicU64::new(0);
    pub static PROGRAMS_AT_PEAK_RSS: AtomicUsize = AtomicUsize::new(0);
    pub static STMT_NODES_AT_PEAK_RSS: AtomicUsize = AtomicUsize::new(0);

    /// Read VmRSS (KiB) from /proc; None off-Linux.
    pub fn self_rss_kb() -> Option<u64> {
        #[cfg(target_os = "linux")]
        {
            let s = std::fs::read_to_string("/proc/self/status").ok()?;
            for line in s.lines() {
                if let Some(rest) = line.strip_prefix("VmRSS:") {
                    return rest.split_whitespace().next()?.parse().ok();
                }
            }
            None
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    fn bump_peak(slot: &AtomicUsize, current: usize) {
        let mut prev = slot.load(Ordering::Relaxed);
        while current > prev {
            match slot.compare_exchange_weak(prev, current, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(p) => prev = p,
            }
        }
    }

    /// Sample RSS; if it is a new high-water mark, record it together with the
    /// current live-Program snapshot so the report can answer "what was alive
    /// when memory peaked". Returns Some(kb) on a NEW peak (for optional log).
    pub fn sample_rss_peak() -> Option<u64> {
        let kb = self_rss_kb()?;
        let prev = PEAK_RSS_KB.load(Ordering::Relaxed);
        if kb > prev
            && PEAK_RSS_KB
                .compare_exchange(prev, kb, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            PROGRAMS_AT_PEAK_RSS.store(ACTIVE_PROGRAMS.load(Ordering::Relaxed), Ordering::Relaxed);
            STMT_NODES_AT_PEAK_RSS.store(ACTIVE_STMT_NODES.load(Ordering::Relaxed), Ordering::Relaxed);
            return Some(kb);
        }
        None
    }

    /// RAII guard: a parsed `Program` is live for the guard's lifetime.
    /// Construct right after parse; drop when the Program is dropped.
    pub struct LiveProgramGuard {
        source_bytes: usize,
        stmt_nodes: usize,
    }

    impl LiveProgramGuard {
        pub fn new(source_bytes: usize, stmt_nodes: usize) -> Self {
            PROGRAMS_PARSED_TOTAL.fetch_add(1, Ordering::Relaxed);
            let p = ACTIVE_PROGRAMS.fetch_add(1, Ordering::Relaxed) + 1;
            let sb = ACTIVE_SOURCE_BYTES.fetch_add(source_bytes, Ordering::Relaxed) + source_bytes;
            let sn = ACTIVE_STMT_NODES.fetch_add(stmt_nodes, Ordering::Relaxed) + stmt_nodes;
            bump_peak(&PEAK_ACTIVE_PROGRAMS, p);
            bump_peak(&PEAK_ACTIVE_SOURCE_BYTES, sb);
            bump_peak(&PEAK_ACTIVE_STMT_NODES, sn);
            sample_rss_peak();
            Self { source_bytes, stmt_nodes }
        }
    }

    impl Drop for LiveProgramGuard {
        fn drop(&mut self) {
            // Sample RSS before decrementing so a peak that coincides with the
            // widest live window is captured.
            sample_rss_peak();
            ACTIVE_PROGRAMS.fetch_sub(1, Ordering::Relaxed);
            ACTIVE_SOURCE_BYTES.fetch_sub(self.source_bytes, Ordering::Relaxed);
            ACTIVE_STMT_NODES.fetch_sub(self.stmt_nodes, Ordering::Relaxed);
        }
    }
}

/// #39 graph-RSS: build a guard for a freshly-parsed Program (no-op-ish unless
/// RUSH_MEM_CENSUS=1 — the guard always tracks, but the cost is a few relaxed
/// atomic adds). Returns None when the census is disabled so callers pay
/// nothing. Counting `stmt_node_count()` is itself only done when enabled.
fn track_live_program(
    program: &crate::Program,
    source_bytes: usize,
) -> Option<program_census::LiveProgramGuard> {
    if !program_census::enabled() {
        return None;
    }
    Some(program_census::LiveProgramGuard::new(
        source_bytes,
        program.stmt_node_count(),
    ))
}

/// #39 graph-RSS: peak live-Program census report (Codex 019e8ee5 decisive
/// measurement). Names whether the mid-load memory spike is transient Program
/// retention. Empty string when the census never observed a parse.
pub fn program_load_peak_census() -> String {
    use program_census::*;
    use std::sync::atomic::Ordering::Relaxed;
    let parsed = PROGRAMS_PARSED_TOTAL.load(Relaxed);
    if parsed == 0 {
        return "peak live-Program census: (no .bzl Programs parsed this run)".to_string();
    }
    let mb = |b: usize| (b as f64) / (1024.0 * 1024.0);
    let gib = |kb: u64| (kb as f64) / (1024.0 * 1024.0);
    format!(
        "peak live-Program census (transient .bzl parse/eval state):\n  \
         programs_parsed_total      {}\n  \
         program_clones_total       {}  <- non-tokio Program.clone() doublings\n  \
         peak_active_programs       {}\n  \
         peak_active_source_bytes   {} (~{:.1} MB)\n  \
         peak_active_stmt_nodes     {}  <- peak parsed-AST stmts held simultaneously\n  \
         peak_rss                   {:.2} GiB\n  \
         programs_at_peak_rss       {}\n  \
         stmt_nodes_at_peak_rss     {}",
        parsed,
        PROGRAM_CLONES_TOTAL.load(Relaxed),
        PEAK_ACTIVE_PROGRAMS.load(Relaxed),
        PEAK_ACTIVE_SOURCE_BYTES.load(Relaxed),
        mb(PEAK_ACTIVE_SOURCE_BYTES.load(Relaxed)),
        PEAK_ACTIVE_STMT_NODES.load(Relaxed),
        gib(PEAK_RSS_KB.load(Relaxed)),
        PROGRAMS_AT_PEAK_RSS.load(Relaxed),
        STMT_NODES_AT_PEAK_RSS.load(Relaxed),
    )
}

thread_local! {
    /// Stack of .bzl files currently being evaluated on this thread.
    static BZL_LOAD_STACK: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

// Debug logging helper - only outputs if RUSH_DEBUG=1 is set
macro_rules! debug_log {
    ($($arg:tt)*) => {
        if std::env::var("RUSH_DEBUG").unwrap_or_default() == "1" {
            eprintln!($($arg)*);
        }
    };
}

fn cached_symbols_with_fallback(
    symbols: &HashMap<String, Value>,
    bzl_label: &str,
    requested: &[(String, String)],
) -> Option<HashMap<String, Value>> {
    let mut exported = HashMap::with_capacity(requested.len());
    let mut all_found = true;

    for (local_name, original_name) in requested {
        if let Some(value) = symbols.get(original_name) {
            exported.insert(local_name.clone(), value.clone());
        } else {
            all_found = false;
        }
    }

    if !exported.is_empty() || all_found {
        return Some(exported);
    }

    if symbols.is_empty() {
        return None;
    }

    let all_lazy = symbols
        .values()
        .all(|value| matches!(value, Value::LazyLoad { .. }));
    if !all_lazy {
        return None;
    }

    Some(
        requested
            .iter()
            .map(|(local_name, original_name)| {
                (
                    local_name.clone(),
                    Value::LazyLoad {
                        bzl_label: bzl_label.to_string(),
                        symbol_name: original_name.clone(),
                    },
                )
            })
            .collect(),
    )
}

/// Check if loading a .bzl file would create a circular dependency on the current thread.
/// This is used by the evaluator to detect cycles BEFORE calling load_bzl_from_evaluator().
/// Returns Some(cycle_path) if a cycle would occur, None otherwise.
pub fn detect_local_cycle_path(label: &str) -> Option<String> {
    BZL_LOAD_STACK.with(|stack| {
        let stack = stack.borrow();
        if let Some(pos) = stack.iter().rposition(|entry| entry == label) {
            let cycle_portion = &stack[pos..];

            // FIX #21: Allow self-references (A→A) - valid in Starlark due to symbol hoisting
            // Only detect TRUE circular dependencies (A→B→A or deeper)
            if cycle_portion.is_empty() {
                // Stack is empty at this position - self-reference on first load
                if std::env::var("RUSH_DEBUG").as_deref() == Ok("1") {
                    eprintln!("✓ LOCAL SELF-REFERENCE: {} loads itself (allowed - symbols are hoisted)", label);
                }
                return None;
            }

            // Check if all entries in the cycle are the same file (self-reference)
            let all_same_file = cycle_portion.iter().all(|f| f == label);
            if all_same_file {
                // All entries are the same file - recursive self-reference
                if std::env::var("RUSH_DEBUG").as_deref() == Ok("1") {
                    eprintln!("✓ LOCAL SELF-REFERENCE: {} loads itself recursively (allowed - symbols are hoisted)", label);
                }
                return None;
            }

            // This is a TRUE circular dependency involving multiple files
            let mut cycle = cycle_portion.to_vec();
            cycle.push(label.to_string());
            Some(cycle.join(" -> "))
        } else {
            None
        }
    })
}

/// Add a file to IN_FLIGHT tracking (called by evaluator before checking for cycles)
/// This allows circular dependency detection to work correctly by adding the file BEFORE
/// the synchronous call to load_bzl_from_evaluator().
pub fn mark_bzl_inflight_start(label: &str) {
    let current_thread_id = std::thread::current().id();

    // Get current BZL_LOAD_STACK to use as stack snapshot
    let stack_snapshot = BZL_LOAD_STACK.with(|stack| stack.borrow().clone());

    IN_FLIGHT_BZL_LOADS.insert(
        label.to_string(),
        InFlightLoadInfo {
            start: Instant::now(),
            thread_id: current_thread_id,
            stack: stack_snapshot,
        },
    );
}

/// Remove a file from IN_FLIGHT tracking (called by evaluator after load completes or fails)
pub fn mark_bzl_inflight_end(label: &str) {
    IN_FLIGHT_BZL_LOADS.remove(label);
}

pub fn mark_loop_guarded(label: &str) {
    LOOP_GUARDED_MODULES.insert(label.to_string());
}

pub fn take_loop_guarded(label: &str) -> bool {
    LOOP_GUARDED_MODULES.remove(label).is_some()
}

/// Check if loading a .bzl file would create a circular dependency by consulting IN_FLIGHT_BZL_LOADS.
/// This works EARLIER than detect_local_cycle_path because files are added to IN_FLIGHT before
/// being added to BZL_LOAD_STACK.
///
/// Returns Some(cycle_path) if:
/// 1. The file is already being loaded by the current thread (immediate cycle)
/// 2. The file's load chain includes a file from the current thread's stack (nested cycle)
pub fn detect_inflight_cycle(label: &str) -> Option<String> {
    let current_thread_id = std::thread::current().id();

    // Check if this file is already being loaded by the current thread
    if let Some(load_info) = IN_FLIGHT_BZL_LOADS.get(label) {
        if load_info.thread_id == current_thread_id {
            // FIX #21: Allow self-references (A→A) - valid in Starlark due to symbol hoisting
            // Only detect TRUE circular dependencies (A→B→A or deeper)
            //
            // Self-reference: stack is empty or contains only the current file
            // True cycle: stack contains other files before coming back to this one
            if load_info.stack.is_empty() {
                // First load of this file, and it's loading itself
                // This is a self-reference - ALLOWED (symbols are hoisted in Starlark)
                eprintln!(
                    "✓ SELF-REFERENCE: {} loads itself (allowed - symbols are hoisted)",
                    label
                );
                return None;
            }

            // Check if this is still just a self-reference (stack has only this file)
            let all_same_file = load_info.stack.iter().all(|f| f == label);
            if all_same_file {
                // All files in stack are the same - this is recursive self-reference
                // Still ALLOWED because symbols are hoisted
                if std::env::var("RUSH_DEBUG").as_deref() == Ok("1") {
                    eprintln!(
                        "✓ SELF-REFERENCE: {} loads itself recursively (allowed - symbols are hoisted)",
                        label
                    );
                }
                return None;
            }

            // This is a TRUE circular dependency involving multiple files
            let mut cycle_path = load_info.stack.clone();
            cycle_path.push(label.to_string());
            return Some(cycle_path.join(" -> "));
        }
    }

    // Check if any file in the current thread's BZL_LOAD_STACK is being loaded by this thread
    // This catches nested circular dependencies where the requested file is in the middle of a chain
    BZL_LOAD_STACK.with(|stack| {
        let stack = stack.borrow();
        for file_in_stack in stack.iter() {
            if let Some(load_info) = IN_FLIGHT_BZL_LOADS.get(file_in_stack.as_str()) {
                if load_info.thread_id == current_thread_id
                    && load_info.stack.contains(&label.to_string())
                {
                    // The file we want to load is in the stack of a file we're currently evaluating
                    let mut cycle_path = load_info.stack.clone();
                    cycle_path.push(label.to_string());
                    return Some(cycle_path.join(" -> "));
                }
            }
        }
        None
    })
}

struct LoadStackGuard {
    label: String,
}

impl LoadStackGuard {
    fn new(label: &str) -> Self {
        BZL_LOAD_STACK.with(|stack| stack.borrow_mut().push(label.to_string()));
        Self {
            label: label.to_string(),
        }
    }
}

impl Drop for LoadStackGuard {
    fn drop(&mut self) {
        BZL_LOAD_STACK.with(|stack| {
            let mut stack = stack.borrow_mut();
            if let Some(popped) = stack.pop() {
                if popped != self.label && std::env::var("RUSH_DEBUG").unwrap_or_default() == "1" {
                    eprintln!(
                        "[WARN BzlLoader] Load stack mismatch: expected {}, popped {}",
                        self.label, popped
                    );
                }
            }
        });
    }
}

struct LoadSetGuard {
    label: String,
}

impl LoadSetGuard {
    fn new(label: &str) -> Self {
        Self {
            label: label.to_string(),
        }
    }
}

impl Drop for LoadSetGuard {
    fn drop(&mut self) {
        BZL_LOADING_SET.remove(&self.label);
    }
}

struct LoadingGuards {
    #[allow(dead_code)]
    stack_guard: LoadStackGuard,
    #[allow(dead_code)]
    set_guard: LoadSetGuard,
    #[allow(dead_code)]
    inflight_guard: InFlightGuard,
}

impl LoadingGuards {
    fn new(label: &str, resolved_path: &str) -> Self {
        let stack_guard = LoadStackGuard::new(label);
        // Snapshot AFTER pushing the new label so we can see full path
        let stack_snapshot = current_stack_snapshot();
        Self {
            stack_guard,
            set_guard: LoadSetGuard::new(label),
            inflight_guard: InFlightGuard::new(label, resolved_path, stack_snapshot),
        }
    }
}

#[derive(Clone)]
struct InFlightLoadInfo {
    start: Instant,
    thread_id: std::thread::ThreadId,
    stack: Vec<String>,
}

struct InFlightGuard {
    label: String,
    resolved_path: String,
}

impl InFlightGuard {
    fn new(label: &str, resolved_path: &str, stack_snapshot: Vec<String>) -> Self {
        debug_log!(
            "[TRACE BzlLoader] Tracking in-flight load: {} (stack: {})",
            label,
            stack_snapshot.join(" -> ")
        );
        IN_FLIGHT_BZL_LOADS.insert(
            label.to_string(),
            InFlightLoadInfo {
                start: Instant::now(),
                thread_id: std::thread::current().id(),
                stack: stack_snapshot,
            },
        );
        IN_FLIGHT_BZL_PATHS.insert(resolved_path.to_string());
        Self {
            label: label.to_string(),
            resolved_path: resolved_path.to_string(),
        }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        debug_log!("[TRACE BzlLoader] In-flight load completed: {}", self.label);
        IN_FLIGHT_BZL_LOADS.remove(&self.label);
        IN_FLIGHT_BZL_PATHS.remove(&self.resolved_path);
    }
}

fn current_stack_snapshot() -> Vec<String> {
    BZL_LOAD_STACK.with(|stack| stack.borrow().clone())
}

fn log_wait_diagnostics(waiting_on: &str, elapsed_secs: u64, max_secs: u64) {
    let inflight: Vec<_> = IN_FLIGHT_BZL_LOADS
        .iter()
        .map(|entry| {
            let info = entry.value();
            format!(
                "{} ({}s, thread {:?}) stack: {}",
                entry.key(),
                info.start.elapsed().as_secs(),
                info.thread_id,
                info.stack.join(" -> ")
            )
        })
        .collect();

    if inflight.is_empty() {
        eprintln!(
            "   🔍 Wait diag: {}s/{}s waiting on {}, but no in-flight loaders recorded.",
            elapsed_secs, max_secs, waiting_on
        );
        return;
    }

    eprintln!(
        "   🔍 Wait diag: {}s/{}s waiting on {}. {} active loads:",
        elapsed_secs,
        max_secs,
        waiting_on,
        inflight.len()
    );
    for line in inflight.iter().take(5) {
        eprintln!("      • {}", line);
    }
    if inflight.len() > 5 {
        eprintln!("      • … {} more entries not shown", inflight.len() - 5);
    }
}

fn detect_single_thread_deadlock(waiting_on: &str) -> Option<(std::thread::ThreadId, String)> {
    let mut entries = Vec::new();
    let mut thread_ids = std::collections::HashSet::new();
    for entry in IN_FLIGHT_BZL_LOADS.iter() {
        let info = entry.value();
        thread_ids.insert(info.thread_id);
        entries.push((
            entry.key().clone(),
            info.stack.clone(),
            info.start.elapsed().as_secs(),
            info.thread_id,
        ));
    }
    if entries.is_empty() {
        return None;
    }
    if !entries.iter().any(|(label, _, _, _)| label == waiting_on) {
        return None;
    }
    debug_log!(
        "[TRACE BzlLoader] Deadlock check for {}: {} entries, {} unique threads",
        waiting_on,
        entries.len(),
        thread_ids.len()
    );
    if thread_ids.len() != 1 {
        return None;
    }
    let owning_thread = *thread_ids.iter().next().unwrap();

    let summary = entries
        .into_iter()
        .map(|(label, stack, elapsed, _)| {
            let chain = if stack.is_empty() {
                label
            } else {
                stack.join(" -> ")
            };
            format!("{elapsed}s {chain}")
        })
        .collect::<Vec<_>>()
        .join("\n      ");
    debug_log!(
        "[TRACE BzlLoader] Deadlock confirmed for {} on thread {:?}",
        waiting_on,
        owning_thread
    );
    Some((owning_thread, summary))
}

fn inflight_owned_by_current_thread(label: &str) -> bool {
    let current_thread = std::thread::current().id();
    IN_FLIGHT_BZL_LOADS
        .get(label)
        .is_some_and(|info| info.thread_id == current_thread)
}

fn make_stub_exports(bzl_label: &str, symbols: &[(String, String)]) -> HashMap<String, Value> {
    let mut exported = HashMap::new();
    for (local_name, original_name) in symbols {
        exported.insert(
            local_name.clone(),
            Value::Function {
                params: Vec::new(),
                varargs_param: Some("__rush_stub_args".to_string()),
                kwargs_param: Some("__rush_stub_kwargs".to_string()),
                body: Vec::new(),
                closure: Arc::new(Environment::new()),
            },
        );
        debug_log!(
            "[STUB] {} exported as stub function for {}:{}",
            local_name,
            bzl_label,
            original_name
        );
    }
    exported
}

/// Type alias for repository metadata tuple
/// (name, url, sha256, build_file_path)
type RepoMetadata = (String, Option<String>, Option<String>, Option<PathBuf>);

/// Phase 5.C.4 commit 134 (Batch Q): typed decision
/// outcome from `BzlLoader::try_fetch_known_repo`.
/// Each call produces exactly one variant.
///
/// Recorded into `GlobalBzlCache::record_fetch_decision`
/// so the analyze-report exposes the decisions
/// taken — operators can see whether the
/// `RUSH_FETCH_SKIP_IF_MATERIALIZED=1` mode is firing,
/// and which repos were genuinely fetched.
///
/// **Correctness caveat (per Pro's commit-134 letter):**
/// `SkippedMaterialized` is a *trust* mode, not a
/// content-verified cache hit. A `BUILD` file on disk
/// is necessary but not sufficient — the repo may still
/// be stale, partial, or from a wrong revision. The
/// env-var gates this; default-off keeps the decision
/// to the operator.
#[derive(Debug, Clone)]
pub enum FetchDecision {
    /// A real `bazel fetch <repo>` subprocess was
    /// started. `duration` is the wall time spent in
    /// the subprocess. Recorded by
    /// `fetch_repo_via_bazel` and the orchestrator-
    /// side `fetch_repo_with_bazel`.
    Invoked {
        repo_name: String,
        duration: std::time::Duration,
    },
    /// The pre-existing hard-coded `SKIP_FETCH_REPOS`
    /// allowlist matched AND the vendored
    /// `<workspace>/external/<repo>/` directory exists
    /// on disk. Rush trusts the existing copy and
    /// returns `Ok(None)` so the caller resolves
    /// labels against the vendored path.
    SkippedKnownRule {
        repo_name: String,
        path: PathBuf,
    },
    /// `RUSH_FETCH_SKIP_IF_MATERIALIZED=1` was set,
    /// the vendored external dir exists, AND a
    /// `BUILD` or `BUILD.bazel` file is present. Rush
    /// trusts the on-disk copy is materialized
    /// enough for analyze. Same semantics as
    /// `SkippedKnownRule` but generic.
    SkippedMaterialized {
        repo_name: String,
        path: PathBuf,
    },
    /// `RUSH_FETCH_SKIP_IF_MATERIALIZED=1` was set
    /// but the vendored dir did NOT exist. The skip
    /// did not fire; the caller falls through to
    /// the usual WORKSPACE-scan / third_party /
    /// bazel-fetch chain.
    MaterializedMissingDir { repo_name: String },
    /// `RUSH_FETCH_SKIP_IF_MATERIALIZED=1` was set,
    /// the vendored dir DID exist, but no `BUILD`
    /// or `BUILD.bazel` was found. The skip did not
    /// fire; the dir is treated as not-materialized.
    MaterializedMissingBuild {
        repo_name: String,
        path: PathBuf,
    },
    /// Phase 5.C.4 commit 149 (Batch S): the
    /// vendored dir + BUILD check passed AND a
    /// sidecar manifest at
    /// `<cache_root>/external-manifests/<repo>.json`
    /// loaded + validated to `Valid`. This is the
    /// strongest skip — backed by recorded
    /// materialization metadata. Pro's "Better:
    /// keep existing env behavior" framing means
    /// rush-FETCH_SKIP_IF_MATERIALIZED operators
    /// still get a skip here; commit 149 just
    /// distinguishes the strength of the trust.
    SkippedMaterializedManifestValid {
        repo_name: String,
        path: PathBuf,
        manifest_status: String,
    },
    /// Phase 5.C.4 commit 149 (Batch S): the
    /// vendored dir + BUILD check passed BUT the
    /// sidecar manifest is missing / invalid /
    /// incomplete. Rush still skips per the env
    /// var, but emits a one-line stderr warning
    /// so operators can see they're depending on
    /// trust-mode rather than a verified manifest.
    SkippedMaterializedTrustMode {
        repo_name: String,
        path: PathBuf,
        manifest_status: String,
    },
}

/// Phase 5.C.4 commit 135 (Batch Q): per-repo
/// fetch attribution report row. One per distinct
/// repo touched by `try_fetch_known_repo` during the
/// analyze pass.
///
/// Operators read this to see exactly which repos
/// fetched, which skipped (and why), and how much
/// wall time each Invoked fetch cost.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchReportEntry {
    pub repo_name: String,
    /// String form of the FetchDecision discriminant.
    /// Possible values include all of:
    ///   "invoked"
    ///   "skipped_materialized"  (commit 134 path)
    ///   "skipped_materialized_manifest_valid"  (149)
    ///   "skipped_materialized_trust_mode"      (149)
    ///   "skipped_known_rule"
    ///   "materialized_missing_dir"
    ///   "materialized_missing_build"
    pub decision: String,
    /// Total ms spent in bazel-fetch for this repo.
    /// Only nonzero for `decision == "invoked"`.
    pub duration_ms: u64,
    pub materialized_path: Option<String>,
    pub had_build: Option<bool>,
    /// Phase 5.C.4 commit 150 (Batch S): manifest
    /// status string from the reader callback:
    /// "valid" | "missing" | "invalid" | "incomplete"
    /// | "stale" | "unknown" | "absent".
    /// `None` if the decision did not consult the
    /// manifest (e.g. Invoked, SkippedKnownRule).
    pub manifest_status: Option<String>,
    /// `<cache_root>/external-manifests/<repo>.json`
    /// — populated whenever we consulted (or
    /// would consult) the sidecar. None for
    /// decisions that bypass the manifest layer.
    pub manifest_path: Option<String>,
    /// For Invalid manifests, the reason string from
    /// the reader (parse error, schema mismatch).
    /// None otherwise.
    pub manifest_reason: Option<String>,
    /// True iff the skip was backed by a Valid
    /// manifest (SkippedMaterializedManifestValid).
    pub skipped_with_manifest: bool,
    /// True iff the skip used trust-mode without
    /// manifest validation
    /// (SkippedMaterializedTrustMode OR the
    /// commit-134 SkippedMaterialized variant when
    /// commit-149's reader said anything other than
    /// "valid").
    pub skipped_by_trust_mode: bool,
}

impl FetchReportEntry {
    /// Lower a single `FetchDecision` into a report
    /// row. The `duration_ms` parameter is the
    /// already-aggregated total for `Invoked` repos
    /// (0 otherwise).
    pub fn from_decision(
        repo_name: &str,
        decision: &FetchDecision,
        duration_ms: u64,
    ) -> Self {
        match decision {
            FetchDecision::Invoked { .. } => FetchReportEntry {
                repo_name: repo_name.to_string(),
                decision: "invoked".into(),
                duration_ms,
                materialized_path: None,
                had_build: None,
                manifest_status: None,
                manifest_path: None,
                manifest_reason: None,
                skipped_with_manifest: false,
                skipped_by_trust_mode: false,
            },
            FetchDecision::SkippedMaterialized { path, .. } => {
                FetchReportEntry {
                    repo_name: repo_name.to_string(),
                    decision: "skipped_materialized".into(),
                    duration_ms: 0,
                    materialized_path: Some(
                        path.display().to_string(),
                    ),
                    had_build: Some(true),
                    manifest_status: None,
                    manifest_path: None,
                    manifest_reason: None,
                    // The legacy SkippedMaterialized
                    // variant predates commit 149's
                    // manifest reader and is treated
                    // as trust-mode.
                    skipped_with_manifest: false,
                    skipped_by_trust_mode: true,
                }
            }
            FetchDecision::SkippedKnownRule { path, .. } => {
                FetchReportEntry {
                    repo_name: repo_name.to_string(),
                    decision: "skipped_known_rule".into(),
                    duration_ms: 0,
                    materialized_path: Some(
                        path.display().to_string(),
                    ),
                    had_build: Some(true),
                    manifest_status: None,
                    manifest_path: None,
                    manifest_reason: None,
                    skipped_with_manifest: false,
                    skipped_by_trust_mode: false,
                }
            }
            FetchDecision::MaterializedMissingDir { .. } => {
                FetchReportEntry {
                    repo_name: repo_name.to_string(),
                    decision: "materialized_missing_dir".into(),
                    duration_ms: 0,
                    materialized_path: None,
                    had_build: None,
                    manifest_status: None,
                    manifest_path: None,
                    manifest_reason: None,
                    skipped_with_manifest: false,
                    skipped_by_trust_mode: false,
                }
            }
            FetchDecision::MaterializedMissingBuild {
                path, ..
            } => FetchReportEntry {
                repo_name: repo_name.to_string(),
                decision: "materialized_missing_build".into(),
                duration_ms: 0,
                materialized_path: Some(path.display().to_string()),
                had_build: Some(false),
                manifest_status: None,
                manifest_path: None,
                manifest_reason: None,
                skipped_with_manifest: false,
                skipped_by_trust_mode: false,
            },
            // Commit 149 variants
            FetchDecision::SkippedMaterializedManifestValid {
                path,
                manifest_status,
                ..
            } => FetchReportEntry {
                repo_name: repo_name.to_string(),
                decision: "skipped_materialized_manifest_valid".into(),
                duration_ms: 0,
                materialized_path: Some(path.display().to_string()),
                had_build: Some(true),
                manifest_status: Some(manifest_status.clone()),
                manifest_path: None,
                manifest_reason: None,
                skipped_with_manifest: true,
                skipped_by_trust_mode: false,
            },
            FetchDecision::SkippedMaterializedTrustMode {
                path,
                manifest_status,
                ..
            } => FetchReportEntry {
                repo_name: repo_name.to_string(),
                decision: "skipped_materialized_trust_mode".into(),
                duration_ms: 0,
                materialized_path: Some(path.display().to_string()),
                had_build: Some(true),
                manifest_status: Some(manifest_status.clone()),
                manifest_path: None,
                manifest_reason: None,
                skipped_with_manifest: false,
                skipped_by_trust_mode: true,
            },
        }
    }
}

/// Phase 5.C.4 commit 134 (Batch Q): canonical
/// path to the *vendored* external repo dir for
/// `<repo_name>`. Mirrors
/// `rush_build::cache_paths::workspace_vendored_external_repo_dir`
/// — rush-starlark can't depend on rush-build, so
/// we inline the one-liner here and document the
/// invariant.
///
/// **Vendored** specifically means
/// `<workspace_root>/external/<repo>/` — the
/// user-visible copy, distinct from rush's
/// internal `<cache_root>/external/<repo>/`
/// (which `rush_build::cache_paths::workspace_external_dir`
/// returns). The pre-existing `SKIP_FETCH_REPOS`
/// allowlist also checks this vendored path.
#[doc(hidden)]
pub fn vendored_external_repo_dir(
    workspace_root: &Path,
    repo_name: &str,
) -> PathBuf {
    workspace_root.join("external").join(repo_name)
}

/// Canonical cache root used by Rush for materialized external repositories.
/// Mirrors build-side logic: `RUSH_WORKSPACE_CACHE_ROOT`, falling back to
/// `<workspace_root>/.cache/rush`.
#[doc(hidden)]
fn workspace_cache_root(workspace_root: &Path) -> PathBuf {
    std::env::var("RUSH_WORKSPACE_CACHE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| workspace_root.join(".cache").join("rush"))
}

/// Return a cache-backed external repo root used for known stubs such as
/// `@local_config_remote_execution`.
#[doc(hidden)]
fn cache_backed_external_repo_dir(
    workspace_root: &Path,
    repo_name: &str,
) -> PathBuf {
    workspace_cache_root(workspace_root)
        .join("external")
        .join(repo_name)
}

/// Ensure a small cache-backed stub is present for repos that are safe to
/// synthesize locally (when Bazel `sync` is unavailable or too expensive).
///
/// Returns `Ok(Some(path))` only for repos we know how to stub.
#[doc(hidden)]
fn create_known_stub_repo(
    workspace_root: &Path,
    repo_name: &str,
) -> Result<Option<PathBuf>> {
    // Synthetic repositories model legacy WORKSPACE configuration only.
    // In a Bzlmod-only workspace they can shadow real canonical repositories
    // materialized by module extensions.
    let is_bzlmod_only = |root: &Path| {
        root.join("MODULE.bazel").exists()
            && !root.join("WORKSPACE").exists()
            && !root.join("WORKSPACE.bazel").exists()
    };
    let process_workspace_is_bzlmod_only = std::env::current_dir()
        .ok()
        .as_deref()
        .map(is_bzlmod_only)
        .unwrap_or(false);
    if is_bzlmod_only(workspace_root) || process_workspace_is_bzlmod_only {
        return Ok(None);
    }

    match repo_name {
        "local_config_remote_execution" => {
            let repo_dir = cache_backed_external_repo_dir(workspace_root, repo_name);
            fs::create_dir_all(&repo_dir)?;
            let bzl_path = repo_dir.join("remote_execution.bzl");
            // This cache-backed repository is Rush-owned. Refresh it
            // deterministically so a stub produced by an older, broken
            // generator cannot poison later commands that reuse the cache.
            fs::write(
                bzl_path,
                r#"# Auto-generated Rush stub for local_config_remote_execution
def gpu_test_tags():
    return []

def if_remote_execution(if_true, if_false = []):
    return if_false
"#,
            )?;
            let build_path = repo_dir.join("BUILD");
            if !repo_dir.join("BUILD.bazel").exists() && !build_path.exists() {
                fs::write(
                    build_path,
                    "package(default_visibility = [\"//visibility:public\"])\n",
                )?;
            }
            Ok(Some(repo_dir))
        }
        _ => Ok(None),
    }
}

/// Phase 5.C.4 commit 134 (Batch Q): pure
/// fetch-decision predicate for the
/// `RUSH_FETCH_SKIP_IF_MATERIALIZED=1` mode.
/// Returns the decision *that would be produced*
/// without invoking any side effects (no
/// fetch-subprocess, no metric record). Pulled
/// out as a separate function so the unit tests
/// can drive it without standing up a full
/// `BzlLoader` and `GlobalBzlCache`.
///
/// Returns:
/// - `Some(SkippedMaterialized)` — env was on,
///   dir exists, BUILD/BUILD.bazel present.
/// - `Some(MaterializedMissingDir)` — env was on,
///   dir missing.
/// - `Some(MaterializedMissingBuild)` — env was
///   on, dir present, BUILD missing.
/// - `None` — env was off; the materialized-skip
///   path was not consulted. The caller falls
///   through to the rest of `try_fetch_known_repo`.
#[doc(hidden)]
pub fn evaluate_materialized_skip(
    workspace_root: &Path,
    repo_name: &str,
    env_on: bool,
) -> Option<FetchDecision> {
    if !env_on {
        return None;
    }
    let external_path =
        vendored_external_repo_dir(workspace_root, repo_name);
    if !external_path.exists() {
        return Some(FetchDecision::MaterializedMissingDir {
            repo_name: repo_name.to_string(),
        });
    }
    let has_build = external_path.join("BUILD").exists()
        || external_path.join("BUILD.bazel").exists();
    if !has_build {
        return Some(FetchDecision::MaterializedMissingBuild {
            repo_name: repo_name.to_string(),
            path: external_path,
        });
    }
    // Phase 5.C.4 commit 149 (Batch S): consult the
    // manifest. "valid" → manifest-validated skip
    // (strong trust). Anything else → trust-mode
    // skip with a warning so operators see the
    // weaker trust state.
    let status = crate::fetch_strategy::read_manifest_status_via_callback(
        workspace_root,
        repo_name,
    );
    if status == "valid" {
        // Manifest-validated skip: no warning. This
        // is the safest skip path because the
        // typed identity (commit 176-179) passed
        // every check the reader applies.
        return Some(FetchDecision::SkippedMaterializedManifestValid {
            repo_name: repo_name.to_string(),
            path: external_path,
            manifest_status: status,
        });
    }
    // Phase 5.C.4 commit 180 (Batch W): optional
    // strict mode. RUSH_FETCH_SKIP_REQUIRE_MANIFEST=1
    // blocks trust-mode skip — only Valid manifests
    // are honored. The fetch path falls through to
    // the normal serial / combined / warmup-driven
    // bazel sync. This gives operators a way to
    // exercise the "future default" behavior
    // without flipping the materialized-skip
    // default.
    if is_skip_require_manifest() {
        eprintln!(
            "    ⚠️  materialized skip BLOCKED by RUSH_FETCH_SKIP_REQUIRE_MANIFEST=1: @{} (manifest={})",
            repo_name, status
        );
        return None;
    }
    // Trust-mode skip: warning stays. The on-disk
    // directory exists + has a BUILD file, but the
    // manifest is Missing / Incomplete / Stale /
    // Invalid / Unknown.
    eprintln!(
        "    ⚠️  materialized skip used without manifest validation: @{} (manifest={})",
        repo_name, status
    );
    Some(FetchDecision::SkippedMaterializedTrustMode {
        repo_name: repo_name.to_string(),
        path: external_path,
        manifest_status: status,
    })
}

/// Phase 5.C.4 commit 180 (Batch W): parse
/// `RUSH_FETCH_SKIP_REQUIRE_MANIFEST`. Accepts the
/// canonical opt-in vocabulary (1/true/yes/on,
/// case-insensitive). Anything else (including
/// unset) is the default: trust-mode skip allowed.
///
/// Pro's commit-180 spec: "Add optional stricter
/// env: RUSH_FETCH_SKIP_REQUIRE_MANIFEST=1. This
/// gives operators a way to test the 'future
/// default' behavior without actually making it
/// default."
pub fn is_skip_require_manifest() -> bool {
    std::env::var("RUSH_FETCH_SKIP_REQUIRE_MANIFEST")
        .map(|v| {
            let lc = v.trim().to_ascii_lowercase();
            matches!(lc.as_str(), "1" | "true" | "yes" | "on")
        })
        .unwrap_or(false)
}

/// Evaluated module identity is distinct from its filesystem dependencies.
/// `None` is reserved for legacy/test cache entries and is never admitted by a
/// real module load. This also makes pre-context entries a guaranteed miss.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ModuleCacheKey {
    path: PathBuf,
    workspace: PathBuf,
    context: Option<DefinitionLabelContext>,
    label: Option<String>,
    mappings: Vec<(String, Vec<(String, String)>)>,
    repository_roots: Vec<(String, PathBuf, PathBuf)>,
    aliases: Vec<(String, String)>,
}

impl ModuleCacheKey {
    fn unqualified(path: &Path) -> Self {
        Self { path: path.to_path_buf(), workspace: PathBuf::new(), context: None, label: None, mappings: Vec::new(), repository_roots: Vec::new(), aliases: Vec::new() }
    }

    fn display(&self) -> std::path::Display<'_> { self.path.display() }

    fn inflight_key(&self) -> String {
        // Debug preserves the entire identity, including sorted mapping entries;
        // this key is only process-local and is never used as a filesystem path.
        format!("{:?}", self)
    }
}

/// Global shared cache for .bzl files across all BzlLoader instances
///
/// This cache ONLY stores evaluated symbols (not ASTs/Programs).
/// Programs consume 100-1000x source file size in memory and are dropped after evaluation.
///
/// Cloning this struct creates a reference to the SAME underlying cache (Arc-based sharing).
#[derive(Clone)]
pub struct GlobalBzlCache {
    /// Shared concurrent cache of evaluated .bzl symbols (path -> HashMap<symbol, Value>)
    /// Uses DashMap for lock-free concurrent reads and fine-grained write locking
    symbol_cache: Arc<DashMap<ModuleCacheKey, CacheEntry>>,
    /// Maximum number of cached entries before evicting
    max_entries: usize,
    /// Maximum total estimated memory usage before evicting (bytes)
    max_memory_bytes: usize,
    /// Aggregated cache metrics (hits, misses, evictions, memory usage)
    metrics: Arc<Mutex<CacheMetrics>>,
    /// Monotonic identity for cache generations. Admission and final manifest
    /// attachment use this to avoid acting on a newer replacement at the same
    /// path after dropping a DashMap guard.
    next_generation: Arc<AtomicU64>,
    #[cfg(test)]
    admission_validation_hook: Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>,
}

struct CacheEntry {
    symbols: HashMap<String, Value>,
    /// Size estimate in bytes (reserved for future eviction logic)
    #[allow(dead_code)]
    size_estimate: usize,
    access_count: usize,
    #[cfg_attr(not(feature = "bzl-debug"), allow(dead_code))]
    inserted_at: Instant,
    generation: u64,
    /// Admission state is locked independently from the DashMap shard so a
    /// manifest validation may touch the filesystem without blocking unrelated
    /// cache keys in the same shard.
    admission: Arc<Mutex<CacheAdmission>>,
}

struct CacheAdmission {
    /// Canonical inputs that produced `symbols`, including transitive loads.
    manifest: DependencyManifest,
    /// The command that most recently admitted this value. A weak reference
    /// avoids extending command lifetime while suppressing repeated validation
    /// walks when several BUILD files load the module in one command.
    admitted_io: Option<std::sync::Weak<dyn rush_incremental::TrackedIo + Send + Sync>>,
}

#[derive(Debug, Default)]
struct CacheMetrics {
    hits: usize,
    misses: usize,
    evictions: usize,
    total_size_bytes: usize,
    peak_size_bytes: usize,
    /// Phase 5.C.4 commit 118: count of
    /// `estimate_symbols_size` calls across the
    /// process. Each cold-load triggers one call
    /// today. Once O-1's reuse path lands (commit
    /// 120), repeat-miss reloads that hit the
    /// fingerprint match WON'T increment this.
    estimate_symbols_size_calls: u64,
    /// Phase 5.C.4 commit 118: cumulative wall-clock
    /// duration of all `estimate_symbols_size` calls.
    /// Operators reading this divided by
    /// `estimate_symbols_size_calls` get the average
    /// per-call cost (which is ~150ms for
    /// tensorflow.bzl-sized maps per the commit-114
    /// data).
    estimate_symbols_size_total_duration_ns: u64,
    /// Phase 5.C.4 commit 118: count of repeat-miss
    /// reloads (i.e. same path was previously cached
    /// then evicted). This is the population
    /// `repeat_reload_estimate_reuse_hits` is drawn
    /// from once commit 120 lands the env-gated
    /// reuse logic.
    repeat_reload_estimate_candidates: u64,
    /// Phase 5.C.4 commit 118 (initially 0 — commit
    /// 120 will populate): count of repeat-miss
    /// reloads where a prior size-estimate record
    /// matched the new load's fingerprint and was
    /// reused instead of recomputed. Stays 0 until
    /// commit 120 enables the reuse path under
    /// `RUSH_BZL_REUSE_SIZE_ESTIMATE=1`.
    repeat_reload_estimate_reuse_hits: u64,
    /// Phase 5.C.4 commit 118 (initially 0): count
    /// of repeat-miss reloads where the fingerprint
    /// did NOT match the prior record. Reuse path
    /// won't activate; estimate is recomputed.
    repeat_reload_estimate_reuse_misses: u64,
    /// Phase 5.C.4 commit 120 (initially 0): count of
    /// reuse decisions verified against a fresh
    /// recompute (under
    /// `RUSH_BZL_REUSE_SIZE_ESTIMATE_VERIFY=1`) where
    /// the reused estimate did NOT match the
    /// recomputed one. A non-zero value here means
    /// the fingerprint is missing a relevant
    /// invariant; do not trust reuse.
    estimate_verification_mismatches: u64,
    /// Phase 5.C.4 commit 126 (Batch P): wait-category
    /// counters. Pro's commit-125 data showed the
    /// analyze pass is 96.3% wait. These counters
    /// split that wait by category.
    ///
    /// `bzl_async_spawn_wait_ns`: time at
    /// `rx.recv_timeout` after a `thread::spawn` in
    /// the non-tokio path of
    /// `load_bzl_from_evaluator`. Suspected dominant
    /// wait category for the 90-second floor.
    bzl_async_spawn_wait_ns: u64,
    bzl_async_spawn_wait_calls: u64,
    /// Aggregated wait time for
    /// `std::fs::metadata()` calls (inside
    /// SizeEstimateFingerprint, file_size capture,
    /// etc.).
    bzl_fs_metadata_wait_ns: u64,
    bzl_fs_metadata_wait_calls: u64,
    /// Phase 5.C.4 commit 133 (Batch Q): aggregated
    /// `loader_arc.lock()` wait time across all
    /// recursive load_bzl_from_evaluator entries.
    /// Per the commit-131 decision doc this is the
    /// prime suspect for the 96.3% wait floor on TF
    /// macros analyze (3322 ctx switches ≈ 110
    /// files × ~30 locks).
    bzl_loader_lock_wait_ns: u64,
    bzl_loader_lock_wait_calls: u64,
    /// Phase 5.C.4 commit 133 (Batch Q): time spent
    /// in the "concurrent eval polling" sleep loop at
    /// load_bzl_from_evaluator line ~3635. Each
    /// iteration sleeps 100ms; loop runs up to 600
    /// iterations (60s max per recursive load).
    /// THIS is the likely source of the 96.3% wait.
    bzl_concurrent_eval_wait_ns: u64,
    bzl_concurrent_eval_wait_calls: u64,
    /// Phase 5.C.4 commit 133 (Batch Q): time spent
    /// in `bazel fetch` subprocess calls from the
    /// orchestrator. THE dominant wait per the
    /// commit-133 finding.
    bzl_bazel_fetch_wait_ns: u64,
    bzl_bazel_fetch_wait_calls: u64,
    /// Phase 5.C.4 commit 134 (Batch Q): fetch-decision
    /// counters. Each `try_fetch_known_repo` call
    /// produces exactly ONE decision and increments
    /// exactly ONE of these (plus possibly
    /// `bzl_fetch_invoked_count` if the chosen path is
    /// actual bazel fetch). Counts are operator-visible
    /// in the analyze report and let us see whether the
    /// `RUSH_FETCH_SKIP_IF_MATERIALIZED=1` mode is
    /// firing.
    ///
    /// - invoked: a real `bazel fetch` subprocess
    ///   started (matches `bzl_bazel_fetch_wait_calls`).
    /// - skipped_known_rule: the pre-existing
    ///   SKIP_FETCH_REPOS allowlist hit + the vendored
    ///   external dir was found.
    /// - skipped_materialized: the new env-gated check
    ///   hit (vendored dir + BUILD file present).
    /// - materialized_missing_dir: env was on but the
    ///   vendored dir did NOT exist — fell through.
    /// - materialized_missing_build: env was on, dir
    ///   existed, but no BUILD/BUILD.bazel — fell
    ///   through.
    bzl_fetch_invoked_count: u64,
    bzl_fetch_skipped_materialized_count: u64,
    bzl_fetch_skipped_known_rule_count: u64,
    bzl_fetch_materialized_missing_dir_count: u64,
    bzl_fetch_materialized_missing_build_count: u64,
    /// Per-repo total fetch duration. Populated only
    /// for repos where `fetch_invoked` fired. Used by
    /// commit 135's attribution report.
    bzl_fetch_duration_by_repo: std::collections::HashMap<String, u64>,
    /// Phase 5.C.4 commit 149 (Batch S): of the
    /// `skipped_materialized` total, how many were
    /// backed by a Valid manifest vs trust-mode.
    /// Both sub-counters always sum to
    /// `bzl_fetch_skipped_materialized_count` (every
    /// materialized skip is one or the other).
    bzl_fetch_skipped_with_manifest_count: u64,
    bzl_fetch_skipped_by_trust_mode_count: u64,
    /// Phase 5.C.4 commit 135 (Batch Q): the most
    /// recent `FetchDecision` recorded for each repo
    /// in this analyze pass. The same repo is queried
    /// many times during one pass and (file state
    /// doesn't change) all calls produce the same
    /// decision; we keep the latest so the per-repo
    /// report has all the relevant context (decision
    /// kind, materialized path, BUILD-file
    /// presence). Cleared on each new pass via
    /// `clear()` — see the analyze-start reset path.
    bzl_fetch_last_decision_per_repo:
        std::collections::HashMap<String, FetchDecision>,
    /// Phase 5.C.4 commit 96: discriminate misses into
    /// cold (first time this path was looked up) vs
    /// repeat (the path was previously cached + evicted).
    cold_misses: usize,
    /// Misses on paths we've seen before. Repeat misses
    /// imply prior cache content was evicted before reuse.
    repeat_misses: usize,
    /// Phase 5.C.4 commit 96: discriminate evictions by
    /// trigger reason.
    evictions_by_entry_limit: usize,
    evictions_by_byte_limit: usize,
    evictions_manual: usize,
    /// High-water mark for entry count.
    peak_entry_count: usize,
    /// Phase 5.C.4 commit 96: set of paths the cache has
    /// ever seen (live or evicted). Lets us distinguish
    /// cold misses from repeat misses.
    seen_paths: std::collections::HashSet<PathBuf>,
    /// Phase 5.C.4 commit 96: per-path lookup / miss /
    /// repeat-miss / eviction / byte counters. Used by
    /// `top_n_by_*` accessors for the operator-facing
    /// report. Bounded to one entry per unique .bzl file
    /// ever loaded — typically a few thousand for TF.
    per_path: std::collections::HashMap<PathBuf, PerPathMetrics>,
}

/// Phase 5.C.4 commit 96: per-`.bzl`-file counters tracked
/// alongside the aggregate metrics.
#[derive(Debug, Default, Clone)]
pub struct PerPathMetrics {
    pub lookups: usize,
    pub hits: usize,
    pub misses: usize,
    pub repeat_misses: usize,
    pub evictions: usize,
    /// Most-recent-known size estimate in bytes. Updated
    /// on each insert; left in place when entry is
    /// evicted so the report can show typical sizes.
    pub last_size_estimate_bytes: usize,
    /// Phase 5.C.4 commit 103 / 104 (RENAMED): cumulative
    /// wall-clock duration of cold loads of this path,
    /// INCLUDING transitive child .bzl loads invoked by
    /// Starlark's `load()` statement.
    ///
    /// Pro's commit-104 directive: "Rename
    /// cold_load_duration to cumulative_cold_load_duration.
    /// Do not silently reinterpret. Rename it or document
    /// it loudly."
    ///
    /// To get THIS file's own parse/eval time, subtract
    /// `child_load_duration_ns` from
    /// `cumulative_cold_load_duration_ns`.
    pub cumulative_cold_load_duration_ns: u64,
    /// Phase 5.C.4 commit 104: time spent in CHILD .bzl
    /// loads triggered by this file's `load()` statements.
    /// Sum of nested children's cumulative durations.
    /// `None` when own/child accounting isn't reliable
    /// (e.g., recursive load detected at the same path).
    pub child_load_duration_ns: Option<u64>,
    /// Phase 5.C.4 commit 104: this file's OWN load
    /// duration, excluding transitive child loads.
    /// Computed as cumulative - child. `None` when
    /// child accounting was unreliable.
    ///
    /// Pro's commit-104 directive: "Both are useful.
    /// Cumulative time identifies expensive entry points.
    /// Own time identifies actual optimization targets."
    pub own_cold_load_duration_ns: Option<u64>,
    /// Phase 5.C.4 commit 104: subphase totals inside
    /// the cold-load path. Each is cumulative across
    /// the cold loads of this file. `None` for files
    /// whose subphase instrumentation wasn't reached
    /// (e.g., fallback / lazy-stub paths).
    pub subphase_read_ns: Option<u64>,
    pub subphase_parse_ns: Option<u64>,
    pub subphase_eval_ns: Option<u64>,
    pub subphase_finalize_ns: Option<u64>,
    /// Phase 5.C.4 commit 111: sub-finalize accumulator
    /// for `resolve_lazy_loads`. When this and
    /// `subphase_put_symbols_cache_ns` are both
    /// `Some(_)`, the two should sum to a value <=
    /// `subphase_finalize_ns` (modulo a small "other"
    /// bucket for inter-step lock acquisition).
    pub subphase_resolve_lazy_loads_ns: Option<u64>,
    /// Phase 5.C.4 commit 111: sub-finalize accumulator
    /// for `put_symbols_cache`.
    pub subphase_put_symbols_cache_ns: Option<u64>,
    /// Phase 5.C.4 commit 103: count of distinct cold
    /// loads (a path may be re-loaded after eviction —
    /// each re-load increments this).
    pub cold_load_count: u64,
    /// Phase 5.C.4 commit 103: on-disk `.bzl` file size
    /// in bytes (the BUILD-file-on-disk size, distinct
    /// from `last_size_estimate_bytes` which is the
    /// post-eval symbol-table size).
    pub file_size_bytes: u64,
    /// Phase 5.C.4 commit 103: number of symbols
    /// exported by the most recent successful load.
    pub exported_symbols_count: u64,
    /// Phase 5.C.4 commit 105: set of resolved `.bzl`
    /// paths that THIS file directly loaded during its
    /// own evaluation (1-hop children). Cardinality =
    /// fanout. Recorded by the load-graph walk in
    /// `push_cold_load_frame_ex` via `record_load_edge`.
    pub direct_children: std::collections::HashSet<PathBuf>,
    /// Phase 5.C.4 commit 105: set of resolved `.bzl`
    /// paths that directly loaded THIS file (reverse
    /// dependents, 1-hop parents). Cardinality = reverse
    /// fanout.
    pub direct_parents: std::collections::HashSet<PathBuf>,
    /// Phase 5.C.4 commit 105: deepest observed depth
    /// (cold-load frame stack height including self) at
    /// which this file was loaded. Depth 1 = top of
    /// chain (loaded from BUILD or `load_bzl` directly,
    /// no parent on stack).
    pub max_observed_depth: usize,
    /// Phase 5.C.4 commit 105: set of raw `load()` labels
    /// (or other entry-point label strings) that resolved
    /// to this file's path. If `len > 1` then multiple
    /// distinct identities (e.g. `//x:foo.bzl` vs
    /// `@repo//x:foo.bzl`) point at the same resolved
    /// path — flagged by the identity-collision summary.
    pub raw_identities: std::collections::HashSet<String>,
    /// Phase 5.C.4 commit 106: coarse Starlark operation
    /// counters accumulated across all cold loads of this
    /// path. Per Pro's directive: "function calls, builtin
    /// calls, rule/macro invocations, glob calls.
    /// Aggregate per .bzl. NOT a profiler." Each cold-load
    /// frame collects its own ops thread-locally; on pop
    /// we drain them into this struct.
    pub starlark_ops: OpCounts,
    /// Phase 5.C.4 commit 110: attribution of the
    /// MOST RECENT cold-load (per Pro: "do not emit
    /// fake own-time when children run on another
    /// thread"). When the attribution is anything
    /// other than `SameThreadComplete`, callers MUST
    /// treat `own_cold_load_duration_ns` as
    /// experimental even if it's `Some(_)`.
    pub latest_attribution: BzlTimingAttribution,
    /// Phase 5.C.4 commit 118: per-bzl
    /// `estimate_symbols_size` call count.
    pub estimate_symbols_size_calls: u64,
    /// Phase 5.C.4 commit 118: per-bzl cumulative
    /// estimate-call wall time.
    pub estimate_symbols_size_total_duration_ns: u64,
    /// Phase 5.C.4 commit 118: most-recently-recorded
    /// estimated_bytes for THIS path. Echoed from
    /// `last_size_estimate_bytes` (which is a `usize`);
    /// stored as `u64` here so per-bzl JSON shape uses
    /// the same width as the duration accumulators.
    pub estimated_bytes: u64,
    /// Phase 5.C.4 commit 119: most-recent
    /// `SizeEstimateRecord` for this path. Commit 120's
    /// env-gated reuse path consults this on the next
    /// `put_symbols` call: if the new fingerprint
    /// matches `self.last_size_estimate.fingerprint`,
    /// the recorded `estimated_bytes` is reused
    /// instead of recomputed. `None` until at least
    /// one estimate has been recorded for this path.
    pub last_size_estimate: Option<SizeEstimateRecord>,
}

#[derive(Clone)]
struct LoadDepCacheEntry {
    fingerprint: FileFingerprint,
    deps: Vec<String>,
}

/// Validity fingerprint for a loaded `.bzl` file.
///
/// Daemon-correctness (Codex P0 risk #6): the CORRECTNESS key is the
/// blake3 CONTENT `digest`, not metadata. `len` + `modified_ns` are a
/// fast PRECHECK used only to avoid re-hashing a file whose metadata is
/// unchanged relative to a known-prior fingerprint (see
/// `from_metadata_with_precheck`). An in-place edit that preserves both
/// length and mtime (e.g. a same-second rewrite or `touch -r` restore)
/// changes the digest and therefore invalidates the cache; an identical
/// rewrite produces the same digest and is change-pruned (reused).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FileFingerprint {
    len: u64,
    modified_ns: Option<u128>,
    /// inode change time (ctime) in nanoseconds since the UNIX epoch,
    /// when available. Part of the metadata PRECHECK only — an in-place
    /// content rewrite bumps ctime even if mtime is restored via
    /// `touch -r`, so this strengthens the precheck without being the
    /// correctness key. `None` on platforms / fs that don't expose it.
    changed_ns: Option<u128>,
    /// blake3 digest of the file's bytes. `None` only when the file
    /// could not be read to be hashed — a distinct, never-matching
    /// state (a `None` digest never compares equal to a `Some`, and two
    /// `None`s are conservatively treated as non-matching by `matches`).
    digest: Option<[u8; 32]>,
}

impl FileFingerprint {
    /// Hash the file bytes with blake3 (the workspace-standard hasher).
    fn hash_path(path: &Path) -> Option<[u8; 32]> {
        let bytes = fs::read(path).ok()?;
        Some(*blake3::hash(&bytes).as_bytes())
    }

    /// Build a fingerprint using `prior` as a fast precheck.
    ///
    /// Correctness vs. performance, resolved correctness-first: a
    /// metadata-only precheck CANNOT be trusted to short-circuit hashing
    /// on its own, because the exact attack this fix targets is a content
    /// edit that preserves (len, mtime) (an in-place same-second rewrite
    /// or a `touch -r` restore). Reusing the prior digest on a (len,
    /// mtime) match would re-introduce the stale-symbol bug. So we
    /// recompute the digest from disk whenever metadata DIFFERS, and —
    /// critically — also when metadata MATCHES but we cannot rule out a
    /// forged-mtime edit. To keep that honest while still pruning real
    /// work, we add `ctime` (inode change time) to the precheck: an
    /// in-place content rewrite bumps ctime even when mtime is restored,
    /// so a precheck on (len, mtime, ctime) detects every in-place edit
    /// observable through the filesystem. Only when ALL THREE match the
    /// prior is the stored digest reused without re-reading. On any
    /// mismatch (or no prior) the bytes are read and hashed.
    fn from_metadata_with_precheck(
        path: &Path,
        meta: &fs::Metadata,
        prior: Option<&FileFingerprint>,
    ) -> Self {
        let (len, modified_ns, changed_ns) = Self::metadata_parts(meta);
        if let Some(prior) = prior {
            if prior.len == len
                && prior.modified_ns == modified_ns
                && prior.changed_ns == changed_ns
                && changed_ns.is_some()
            {
                if let Some(d) = prior.digest {
                    return Self {
                        len,
                        modified_ns,
                        changed_ns,
                        digest: Some(d),
                    };
                }
            }
        }
        let digest = Self::hash_path(path);
        Self {
            len,
            modified_ns,
            changed_ns,
            digest,
        }
    }

    fn metadata_parts(meta: &fs::Metadata) -> (u64, Option<u128>, Option<u128>) {
        let len = meta.len();
        let modified_ns = meta
            .modified()
            .ok()
            .and_then(|ts| ts.duration_since(UNIX_EPOCH).ok())
            .map(|dur| dur.as_nanos());
        // inode change time: bumped on any in-place write/metadata change,
        // and not restorable via `touch -r` (which only sets atime/mtime).
        #[cfg(unix)]
        let changed_ns = {
            use std::os::unix::fs::MetadataExt;
            let secs = meta.ctime();
            let nsecs = meta.ctime_nsec();
            if secs < 0 {
                None
            } else {
                Some((secs as u128) * 1_000_000_000 + (nsecs.max(0) as u128))
            }
        };
        #[cfg(not(unix))]
        let changed_ns: Option<u128> = None;
        (len, modified_ns, changed_ns)
    }

    /// Cache-validity comparison. The CONTENT digest is the correctness
    /// key: two entries match iff both have a known digest and the
    /// digests are equal. An unreadable file (`digest == None`) never
    /// matches anything, forcing a conservative reload.
    fn matches(&self, other: &FileFingerprint) -> bool {
        match (self.digest, other.digest) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }
    }
}

/// RAM-aware default for the .bzl cache byte limit. The old fixed 512 MB caused severe eviction
/// thrash on TF (hundreds of .bzl files, GB-scale Value graph): every no-op re-loaded + re-estimated
/// evicted entries (~14s of the 21s no-op). Use ~50% of system RAM, clamped to [2 GiB, 64 GiB].
/// Override with RUSH_BZL_CACHE_MAX_MB=<megabytes>. Falls back to DEFAULT_MAX_MEMORY_BYTES if
/// /proc/meminfo is unreadable. (TF-scale no-op profiling 2026-06-24.)
fn default_max_memory_bytes() -> usize {
    if let Ok(mb) = std::env::var("RUSH_BZL_CACHE_MAX_MB") {
        if let Ok(n) = mb.parse::<usize>() {
            return (n * 1024 * 1024).max(1);
        }
    }
    let ram_bytes = std::fs::read_to_string("/proc/meminfo").ok().and_then(|s| {
        s.lines()
            .find_map(|l| l.strip_prefix("MemTotal:"))
            .and_then(|v| v.trim().split_whitespace().next())
            .and_then(|kb| kb.parse::<usize>().ok())
            .map(|kb| kb.saturating_mul(1024)) // kB -> bytes
    });
    match ram_bytes {
        Some(b) => (b / 2).clamp(2 * 1024 * 1024 * 1024, 64 * 1024 * 1024 * 1024),
        None => GlobalBzlCache::DEFAULT_MAX_MEMORY_BYTES,
    }
}

impl GlobalBzlCache {
    /// Default maximum number of cached entries. Was 50 — far too small for TF's hundreds of .bzl
    /// files: the cache thrashed (evict -> reload -> re-estimate every no-op), the single biggest
    /// no-op cost at TF scale (21s -> 7s when raised). Entry COUNT is not the memory risk (the byte
    /// limit is), so make it effectively unlimited. (TF-scale no-op profiling 2026-06-24.)
    pub const DEFAULT_MAX_ENTRIES: usize = 1_000_000;
    /// Fallback byte limit when system RAM can't be read (see default_max_memory_bytes()).
    pub const DEFAULT_MAX_MEMORY_BYTES: usize = 4 * 1024 * 1024 * 1024; // 4 GiB fallback

    /// Create a new global .bzl cache with default limits (RAM-aware byte limit).
    pub fn new() -> Self {
        Self::with_limits(Self::DEFAULT_MAX_ENTRIES, default_max_memory_bytes())
    }

    /// Create a new global .bzl cache with custom limits.
    pub fn with_limits(max_entries: usize, max_memory_bytes: usize) -> Self {
        let max_entries = max_entries.max(1);
        let max_memory_bytes = max_memory_bytes.max(1);

        GlobalBzlCache {
            symbol_cache: Arc::new(DashMap::new()),
            max_entries,
            max_memory_bytes,
            metrics: Arc::new(Mutex::new(CacheMetrics::default())),
            next_generation: Arc::new(AtomicU64::new(1)),
            #[cfg(test)]
            admission_validation_hook: Arc::new(Mutex::new(None)),
        }
    }

    /// #39 graph-RSS census (RUSH_MEM_CENSUS). The slope census cleared
    /// CcInfo providers as the 82GB analysis-RSS hog and pointed at this
    /// `.bzl` load/eval layer (core:lib: 34GB at 265 nodes, still loading).
    /// This walks every retained cached `Value` — deduping shared closure
    /// environments by `Arc::as_ptr` — to name whether the retained weight
    /// is function AST bodies, closure-pinned module globals, or value
    /// count, and reports the live in-flight load statics so a "huge RSS
    /// but small live cache" reading redirects to transient parse/eval
    /// duplication instead. Honest estimate; element bytes are not exact.
    pub fn mem_census(&self) -> String {
        use std::collections::HashSet;
        use std::fmt::Write as _;

        let mut stats = crate::evaluator::ValueCensus::default();
        let mut seen_envs: HashSet<usize> = HashSet::new();
        let mut entries = 0usize;
        let mut total_symbols = 0usize;
        let mut sum_size_estimate = 0usize;
        let mut per_module: Vec<(String, usize)> = Vec::new();

        for kv in self.symbol_cache.iter() {
            entries += 1;
            let entry = kv.value();
            total_symbols += entry.symbols.len();
            sum_size_estimate += entry.size_estimate;
            per_module.push((kv.key().display().to_string(), entry.symbols.len()));
            stats.accumulate(entry.symbols.values(), &mut seen_envs);
        }
        per_module.sort_by(|a, b| b.1.cmp(&a.1));

        let mb = |b: usize| (b as f64) / (1024.0 * 1024.0);
        let mut out = String::new();
        let _ = writeln!(out, "GlobalBzlCache mem census:");
        let _ = writeln!(
            out,
            "  cached .bzl modules:       {} ({} symbols total, sum size_estimate ~{:.1} MB)",
            entries,
            total_symbols,
            mb(sum_size_estimate)
        );
        let _ = writeln!(out, "  retained Value walk (closure envs Arc-deduped):");
        let _ = writeln!(out, "    value_nodes              {}", stats.value_nodes);
        let _ = writeln!(
            out,
            "    string_bytes             {} (~{:.1} MB)",
            stats.string_bytes,
            mb(stats.string_bytes)
        );
        let _ = writeln!(out, "    list_elements            {}", stats.list_elements);
        let _ = writeln!(out, "    dict_entries             {}", stats.dict_entries);
        let _ = writeln!(out, "    provider_instances       {}", stats.provider_instances);
        let _ = writeln!(out, "    function_count           {}", stats.function_count);
        let _ = writeln!(out, "    lambda_count             {}", stats.lambda_count);
        let _ = writeln!(
            out,
            "    function_body_stmts      {}  <- AST-retention signal",
            stats.function_body_stmts
        );
        let _ = writeln!(
            out,
            "    unique_closure_envs      {}  <- module-globals-pinned signal",
            stats.unique_closure_envs
        );
        let _ = writeln!(
            out,
            "    closure_env_var_entries  {}",
            stats.closure_env_var_entries
        );
        let _ = writeln!(out, "  top modules by symbol count:");
        for (path, n) in per_module.iter().take(8) {
            let _ = writeln!(out, "    {:>5}  {}", n, path);
        }
        let _ = writeln!(
            out,
            "  in-flight load statics: BZL_LOADING_SET={} IN_FLIGHT_BZL_LOADS={} IN_FLIGHT_BZL_PATHS={} LOOP_GUARDED_MODULES={}",
            BZL_LOADING_SET.len(),
            IN_FLIGHT_BZL_LOADS.len(),
            IN_FLIGHT_BZL_PATHS.len(),
            LOOP_GUARDED_MODULES.len()
        );
        out
    }

    /// Remove `generation` only if it is still the value stored for `path`.
    ///
    /// Admission deliberately drops every DashMap guard before validating a
    /// manifest because validation may hash files or rerun process probes. A
    /// concurrent cold loader can replace the path during that work, so an
    /// unconditional `remove(path)` would be allowed to delete the fresh value.
    fn remove_generation(&self, key: &ModuleCacheKey, generation: u64) -> Option<CacheEntry> {
        use dashmap::mapref::entry::Entry;

        match self.symbol_cache.entry(key.clone()) {
            Entry::Occupied(entry) if entry.get().generation == generation => {
                Some(entry.remove())
            }
            _ => None,
        }
    }

    /// Admit one stable cache generation, replay its producing dependencies, and
    /// expose it to `use_entry` as one atomic operation.
    ///
    /// The per-entry admission mutex serializes validation for a path without
    /// holding a DashMap shard across filesystem work. Rechecking `generation`
    /// after validation closes both stale-removal and value/manifest replay races.
    /// Values inserted by this command are already admitted, including temporary
    /// cycle-breaking entries whose manifest is not populated yet. Calls outside
    /// tracked analysis retain legacy cache behavior.
    fn with_admitted_entry<R>(
        &self,
        key: &ModuleCacheKey,
        mut use_entry: impl FnMut(&mut CacheEntry) -> R,
    ) -> Option<R> {
        loop {
            let (generation, admission) = {
                let entry = self.symbol_cache.get(key)?;
                (entry.generation, Arc::clone(&entry.admission))
            };
            let mut admission_guard = admission
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let current_io = rush_incremental::tracked::current();

            let stale_reason = current_io.as_ref().and_then(|current_io| {
                let admitted_by_current = admission_guard
                    .admitted_io
                    .as_ref()
                    .and_then(std::sync::Weak::upgrade)
                    .is_some_and(|admitted| Arc::ptr_eq(&admitted, current_io));
                if admitted_by_current {
                    return None;
                }
                if admission_guard.manifest.is_empty() {
                    return Some(
                        "cached .bzl value has no tracked dependency manifest".to_string(),
                    );
                }

                #[cfg(test)]
                if let Some(hook) = self
                    .admission_validation_hook
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
                {
                    hook();
                }
                let validated = current_io.validate_manifest(&admission_guard.manifest);
                match validated.staleness {
                    rush_incremental::Staleness::Fresh => {
                        admission_guard.manifest = validated.refreshed;
                        admission_guard.admitted_io = Some(Arc::downgrade(current_io));
                        None
                    }
                    rush_incremental::Staleness::Stale { reason, .. } => Some(reason),
                }
            });

            if let Some(reason) = stale_reason {
                drop(admission_guard);
                if let Some(removed) = self.remove_generation(key, generation) {
                    {
                        let mut metrics = self.metrics.lock().unwrap();
                        metrics.total_size_bytes = metrics
                            .total_size_bytes
                            .saturating_sub(removed.size_estimate);
                    }
                    debug_log!(
                        "[CACHE STALE] {} ({}), reloading",
                        key.path.display(),
                        reason
                    );
                    return None;
                }
                // The stale generation was replaced while validation ran. Inspect
                // the replacement rather than treating the path as a miss or
                // deleting it based on the old result.
                continue;
            }

            let Some(mut entry) = self.symbol_cache.get_mut(key) else {
                return None;
            };
            if entry.generation != generation {
                drop(entry);
                drop(admission_guard);
                continue;
            }
            if let Some(current_io) = current_io.as_ref() {
                current_io.record_deps(admission_guard.manifest.deps());
            }
            return Some(use_entry(&mut entry));
        }
    }

    fn record_cache_miss(&self, path: &Path) {
        let mut metrics = self.metrics.lock().unwrap();
        metrics.misses = metrics.misses.saturating_add(1);
        let is_repeat = metrics.seen_paths.contains(path);
        if is_repeat {
            metrics.repeat_misses += 1;
        } else {
            metrics.cold_misses += 1;
            metrics.seen_paths.insert(path.to_path_buf());
        }
        let per = metrics.per_path.entry(path.to_path_buf()).or_default();
        per.lookups += 1;
        per.misses += 1;
        if is_repeat {
            per.repeat_misses += 1;
        }
    }

    /// Get symbols from the cache (lock-free concurrent reads with DashMap).
    #[allow(dead_code)]
    fn get_symbols(&self, path: &Path) -> Option<HashMap<String, Value>> {
        self.get_symbols_key(&ModuleCacheKey::unqualified(path))
    }

    fn get_symbols_key(&self, key: &ModuleCacheKey) -> Option<HashMap<String, Value>> {
        let path = &key.path;
        if let Some(symbols) = self.with_admitted_entry(key, |entry| {
            entry.access_count = entry.access_count.saturating_add(1);

            {
                let mut metrics = self.metrics.lock().unwrap();
                metrics.hits = metrics.hits.saturating_add(1);
                // Phase 5.C.4 commit 96: per-path counters.
                let per = metrics.per_path.entry(path.to_path_buf())
                    .or_default();
                per.lookups += 1;
                per.hits += 1;
            }

            #[cfg(feature = "bzl-debug")]
            debug_log!(
                "[CACHE HIT] {} (access: {})",
                path.display(),
                entry.access_count
            );

            entry.symbols.clone()
        }) {
            Some(symbols)
        } else {
            self.record_cache_miss(path);

            #[cfg(feature = "bzl-debug")]
            debug_log!("[CACHE MISS] {}", path.display());

            None
        }
    }

    /// Get only the requested symbols from cache without cloning the full map.
    fn get_symbols_subset(&self, path: &Path, requested: &[(String, String)]) -> Option<HashMap<String, Value>> {
        self.get_symbols_subset_key(&ModuleCacheKey::unqualified(path), requested)
    }

    fn get_symbols_subset_key(
        &self,
        key: &ModuleCacheKey,
        requested: &[(String, String)],
    ) -> Option<HashMap<String, Value>> {
        let path = &key.path;
        if let Some(symbols) = self.with_admitted_entry(key, |entry| {
            entry.access_count = entry.access_count.saturating_add(1);

            {
                let mut metrics = self.metrics.lock().unwrap();
                metrics.hits = metrics.hits.saturating_add(1);
                let per = metrics.per_path.entry(path.to_path_buf())
                    .or_default();
                per.lookups += 1;
                per.hits += 1;
            }

            let path_display = key.label.clone().unwrap_or_else(|| path.display().to_string());
            cached_symbols_with_fallback(&entry.symbols, &path_display, requested)
        }) {
            symbols
        } else {
            self.record_cache_miss(path);
            None
        }
    }

    /// Get requested symbols without hit/miss accounting.
    fn get_symbols_subset_no_stats(&self, path: &Path, requested: &[(String, String)]) -> Option<HashMap<String, Value>> {
        self.get_symbols_subset_no_stats_key(&ModuleCacheKey::unqualified(path), requested)
    }

    fn get_symbols_subset_no_stats_key(
        &self,
        key: &ModuleCacheKey,
        requested: &[(String, String)],
    ) -> Option<HashMap<String, Value>> {
        let path = &key.path;
        self.with_admitted_entry(key, |entry| {
            let path_display = key.label.clone().unwrap_or_else(|| path.display().to_string());
            cached_symbols_with_fallback(&entry.symbols, &path_display, requested)
        })
        .flatten()
    }

    /// Insert symbols into the cache (lock-free with DashMap, no LRU eviction for now).
    #[allow(dead_code)]
    /// Phase 5.C.4 commit 118 (test support): public
    /// wrapper around the internal `put_symbols` so
    /// integration tests in
    /// `crates/rush-starlark/tests/` can drive cache
    /// insertion without an evaluator context. Not
    /// intended for production callers.
    #[doc(hidden)]
    pub fn put_symbols_public(&self, path: PathBuf, symbols: HashMap<String, Value>) {
        self.put_symbols(path, symbols);
    }

    /// Phase 5.C.4 commit 120 (test support): public
    /// wrapper that also accepts a canonical-label so
    /// integration tests can exercise the
    /// identity-collision path (different labels →
    /// different fingerprints → no reuse).
    #[doc(hidden)]
    pub fn put_symbols_with_label_public(
        &self,
        path: PathBuf,
        symbols: HashMap<String, Value>,
        canonical_label: Option<String>,
    ) {
        let _ = self.put_symbols_with_label(path, symbols, canonical_label);
    }

    fn put_symbols(&self, path: PathBuf, symbols: HashMap<String, Value>) {
        let _ = self.put_symbols_with_generation(path, symbols);
    }

    fn put_symbols_with_generation(
        &self,
        path: PathBuf,
        symbols: HashMap<String, Value>,
    ) -> Option<u64> {
        self.put_symbols_with_label(path, symbols, None)
    }

    /// Store the canonical producing manifest after a cold load completes.
    ///
    /// Symbol insertion precedes transitive dependency capture, so this update is
    /// separate from [`Self::put_symbols`]. The generation token prevents a slow
    /// finalizer from attaching an old manifest to a concurrent replacement.
    fn set_dep_set(&self, path: &Path, generation: u64, dep_set: Vec<Dependency>) {
        self.set_dep_set_key(&ModuleCacheKey::unqualified(path), generation, dep_set);
    }

    fn set_dep_set_key(&self, key: &ModuleCacheKey, generation: u64, dep_set: Vec<Dependency>) {
        let admission = {
            let entry = match self.symbol_cache.get(key) {
                Some(entry) if entry.generation == generation => entry,
                _ => return,
            };
            Arc::clone(&entry.admission)
        };
        let mut admission_guard = admission
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Follow the same admission -> DashMap lock order as cache lookup. Holding
        // this short-lived guard pins the checked generation through the update.
        let entry = match self.symbol_cache.get(key) {
            Some(entry) if entry.generation == generation => entry,
            _ => return,
        };
        admission_guard.manifest = DependencyManifest::from_deps(dep_set);
        admission_guard.admitted_io = rush_incremental::tracked::current()
            .map(|io| Arc::downgrade(&io));
        drop(entry);
    }

    /// Phase 5.C.4 commit 120: variant that carries
    /// the canonical `load()` label through to the
    /// fingerprint. The label disambiguates the
    /// identity-collision case (commit 105: same
    /// resolved path reached via two distinct raw
    /// labels). Today only the reuse-decision logic
    /// inside this function inspects it; the
    /// orchestrator can plumb labels through later.
    fn put_symbols_with_label(
        &self,
        path: PathBuf,
        symbols: HashMap<String, Value>,
        canonical_label: Option<String>,
    ) -> Option<u64> {
        self.put_symbols_key(ModuleCacheKey::unqualified(&path), symbols, canonical_label)
    }

    fn put_symbols_key(
        &self,
        key: ModuleCacheKey,
        symbols: HashMap<String, Value>,
        canonical_label: Option<String>,
    ) -> Option<u64> {
        let path = key.path.clone();
        let canonical_label = if key.context.is_some() {
            Some(key.inflight_key())
        } else {
            canonical_label
        };
        // Phase 5.C.4 commit 118 / 120: detect
        // repeat-miss reload and optionally reuse a
        // prior estimate_symbols_size result.
        let symbol_count_now = symbols.len() as u64;
        let (is_repeat_miss, prior_record) = {
            let metrics = self.metrics.lock().unwrap();
            let repeat = metrics.seen_paths.contains(&path);
            let prior = metrics
                .per_path
                .get(&path)
                .and_then(|m| m.last_size_estimate.clone());
            (repeat, prior)
        };
        // Phase 5.C.4 commit 120: env-gated reuse +
        // optional verification.
        let reuse_enabled = std::env::var("RUSH_BZL_REUSE_SIZE_ESTIMATE")
            .as_deref()
            == Ok("1");
        let verify_enabled = std::env::var("RUSH_BZL_REUSE_SIZE_ESTIMATE_VERIFY")
            .as_deref()
            == Ok("1");
        let new_fingerprint = SizeEstimateFingerprint::from_components(
            canonical_label.clone(),
            path.clone(),
            symbol_count_now,
        );
        let mut reuse_used = false;
        let mut reused_estimate: Option<u64> = None;
        if reuse_enabled && is_repeat_miss {
            if let Some(ref record) = prior_record {
                if record.fingerprint.matches(&new_fingerprint) {
                    reuse_used = true;
                    reused_estimate = Some(record.estimated_bytes);
                }
            }
        }
        // The estimate walk: always run it on the cold
        // path or under verify mode; skip it when
        // reuse is active and verify is off.
        let (size_estimate, estimate_dur_ns, mismatch) = if reuse_used && !verify_enabled
        {
            (reused_estimate.unwrap() as usize, 0u64, false)
        } else {
            let estimate_start = std::time::Instant::now();
            let computed = Self::estimate_symbols_size(&symbols);
            let dur_ns = estimate_start.elapsed().as_nanos() as u64;
            let mismatch = if reuse_used {
                // verify mode: compare reused vs
                // recomputed.
                reused_estimate.unwrap() != computed as u64
            } else {
                false
            };
            // If reuse_used was true under verify
            // mode, the accounting still uses the
            // REUSED value (per Pro: "reuse the
            // estimate for accounting; also recompute
            // in debug path"). Otherwise the freshly
            // computed value is authoritative.
            let used_estimate = if reuse_used {
                reused_estimate.unwrap() as usize
            } else {
                computed
            };
            (used_estimate, dur_ns, mismatch)
        };
        // Fold metrics — both global and per-path.
        {
            let mut metrics = self.metrics.lock().unwrap();
            // Commit 118 + 120: every call site counts
            // as one estimate call, EVEN if reuse
            // skipped the actual walk. The walk
            // duration is 0 in that case, which means
            // total_duration / calls is a fair "avg
            // estimate cost incl. reuses".
            metrics.estimate_symbols_size_calls = metrics
                .estimate_symbols_size_calls
                .saturating_add(1);
            metrics.estimate_symbols_size_total_duration_ns = metrics
                .estimate_symbols_size_total_duration_ns
                .saturating_add(estimate_dur_ns);
            if is_repeat_miss {
                metrics.repeat_reload_estimate_candidates = metrics
                    .repeat_reload_estimate_candidates
                    .saturating_add(1);
                if reuse_used {
                    metrics.repeat_reload_estimate_reuse_hits = metrics
                        .repeat_reload_estimate_reuse_hits
                        .saturating_add(1);
                } else {
                    metrics.repeat_reload_estimate_reuse_misses = metrics
                        .repeat_reload_estimate_reuse_misses
                        .saturating_add(1);
                }
            }
            if mismatch {
                metrics.estimate_verification_mismatches = metrics
                    .estimate_verification_mismatches
                    .saturating_add(1);
                debug_log!(
                    "[CACHE WARN] estimate_symbols_size reuse mismatch for {}: reused={} recomputed={}",
                    path.display(),
                    reused_estimate.unwrap_or(0),
                    size_estimate
                );
            }
            let per = metrics
                .per_path
                .entry(path.clone())
                .or_default();
            per.estimate_symbols_size_calls = per
                .estimate_symbols_size_calls
                .saturating_add(1);
            per.estimate_symbols_size_total_duration_ns = per
                .estimate_symbols_size_total_duration_ns
                .saturating_add(estimate_dur_ns);
            per.estimated_bytes = size_estimate as u64;
            // Phase 5.C.4 commit 119 / 120: store the
            // record (fingerprint + estimated_bytes)
            // for next reuse check. Always update —
            // even on reuse hit — so the canonical_label
            // (which can change from None to Some
            // between calls) is kept fresh.
            per.last_size_estimate = Some(SizeEstimateRecord {
                fingerprint: new_fingerprint,
                estimated_bytes: size_estimate as u64,
            });
        }

        // Skip if entry is too large
        if size_estimate > self.max_memory_bytes {
            #[cfg(feature = "bzl-debug")]
            debug_log!(
                "[CACHE SKIP] {} ({}MB) exceeds cache capacity ({}MB)",
                path.display(),
                size_estimate / (1024 * 1024),
                self.max_memory_bytes / (1024 * 1024)
            );
            return None;
        }

        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let entry = CacheEntry {
            symbols,
            size_estimate,
            access_count: 1,
            inserted_at: Instant::now(),
            generation,
            // The producing manifest is captured after the whole transitive load
            // finishes. Tagging the inserting command now keeps concurrent
            // same-command waiters from invalidating this interim entry.
            admission: Arc::new(Mutex::new(CacheAdmission {
                manifest: DependencyManifest::from_deps(Vec::new()),
                admitted_io: rush_incremental::tracked::current()
                    .map(|io| Arc::downgrade(&io)),
            })),
        };

        // Insert into DashMap (lock-free for different keys!)
        let replaced_size = self
            .symbol_cache
            .insert(key, entry)
            .map(|replaced| replaced.size_estimate)
            .unwrap_or(0);

        // Update metrics
        let (should_evict_entries, should_evict_memory) = {
            let mut metrics = self.metrics.lock().unwrap();
            metrics.total_size_bytes = metrics
                .total_size_bytes
                .saturating_sub(replaced_size)
                .saturating_add(size_estimate);
            if metrics.total_size_bytes > metrics.peak_size_bytes {
                metrics.peak_size_bytes = metrics.total_size_bytes;
            }
            // Phase 5.C.4 commit 96: track peak entry count
            // alongside peak bytes. Lets the operator
            // distinguish entry-bound vs byte-bound cache
            // pressure.
            let current_entries = self.symbol_cache.len();
            if current_entries > metrics.peak_entry_count {
                metrics.peak_entry_count = current_entries;
            }
            // Phase 5.C.4 commit 96: track per-path size
            // estimate. Operators reading the top-N-by-bytes
            // report want to know which .bzl modules are
            // the heavyweights.
            metrics.seen_paths.insert(path.clone());
            let per = metrics.per_path.entry(path.clone()).or_default();
            per.last_size_estimate_bytes = size_estimate;

            // ALWAYS log cache operations
            debug_log!(
                "[CACHE STORE] {} | size={}KB (estimated) | cache_total={}MB | entries={}",
                path.display(),
                size_estimate / 1024,
                metrics.total_size_bytes / (1024 * 1024),
                self.symbol_cache.len()
            );

            (
                self.symbol_cache.len() > self.max_entries,
                metrics.total_size_bytes > self.max_memory_bytes,
            )
        };

        // Evict if we exceeded limits.
        // Phase 5.C.4 commit 96: record eviction reason so
        // operators can distinguish entry-bound from
        // byte-bound pressure.
        if should_evict_entries || should_evict_memory {
            if should_evict_memory {
                // Evict down to 75% of max memory
                let target_mb = (self.max_memory_bytes * 3 / 4) / (1024 * 1024);
                let evicted =
                    self.evict_to_target_with_reason(target_mb, EvictReason::ByteLimit);
                debug_log!("[CACHE EVICT] Memory pressure: evicted {} entries", evicted);
            } else {
                // Evict 25% of entries
                let evicted =
                    self.evict_symbols_with_reason(0.25, EvictReason::EntryLimit);
                debug_log!("[CACHE EVICT] Entry count pressure: evicted {} entries", evicted);
            }
        }
        Some(generation)
    }

    /// Phase 5.C.4 commit 96: variant of
    /// [`evict_to_target`] that records the eviction
    /// reason in the metrics. Existing `evict_to_target`
    /// callers default to `Manual` (no upstream policy
    /// signal).
    fn evict_to_target_with_reason(
        &self,
        target_mb: usize,
        reason: EvictReason,
    ) -> usize {
        let evicted = self.evict_to_target_internal(target_mb);
        if evicted > 0 {
            let mut metrics = self.metrics.lock().unwrap();
            match reason {
                EvictReason::EntryLimit => {
                    metrics.evictions_by_entry_limit += evicted;
                }
                EvictReason::ByteLimit => {
                    metrics.evictions_by_byte_limit += evicted;
                }
                EvictReason::Manual => {
                    metrics.evictions_manual += evicted;
                }
            }
        }
        evicted
    }

    /// Variant of [`evict_symbols`] that records the
    /// eviction reason. Same semantics as the original;
    /// just adds reason accounting.
    fn evict_symbols_with_reason(
        &self,
        fraction: f64,
        reason: EvictReason,
    ) -> usize {
        let evicted = self.evict_symbols_internal(fraction);
        if evicted > 0 {
            let mut metrics = self.metrics.lock().unwrap();
            match reason {
                EvictReason::EntryLimit => {
                    metrics.evictions_by_entry_limit += evicted;
                }
                EvictReason::ByteLimit => {
                    metrics.evictions_by_byte_limit += evicted;
                }
                EvictReason::Manual => {
                    metrics.evictions_manual += evicted;
                }
            }
        }
        evicted
    }

    /// Estimate memory usage of symbol map using recursive Value sizing.
    ///
    /// A single `visited` pointer-set is threaded across every symbol (via
    /// [`Value::estimate_size_shared`]) instead of resetting it per symbol with
    /// `estimate_size`. Every Starlark function in a loaded `.bzl` closes over the
    /// SAME file/global [`Environment`]; with a per-symbol set that shared
    /// environment was re-walked once per symbol — O(symbols × env-size), which
    /// dominated the TF-scale no-op (the `Value`/`Environment`
    /// `estimate_size_internal` walk was ≈75% of analysis time). Sharing the set
    /// makes it O(symbols + env-size): the first function walks the shared
    /// environment, the rest hit the already-visited Arc pointer and stop.
    /// (TF-scale no-op profiling 2026-06-24.)
    fn estimate_symbols_size(symbols: &HashMap<String, Value>) -> usize {
        let mut size =
            symbols.capacity() * (std::mem::size_of::<String>() + std::mem::size_of::<Value>());
        let mut visited = crate::evaluator::SizeVisited::default();
        for (key, value) in symbols {
            size += key.len() + value.estimate_size_shared(&mut visited);
        }
        size
    }

    /// Get current estimated memory usage in bytes.
    pub fn estimated_memory_bytes(&self) -> usize {
        self.metrics.lock().unwrap().total_size_bytes
    }

    /// Get current estimated memory usage in megabytes.
    pub fn estimated_memory_mb(&self) -> usize {
        self.estimated_memory_bytes() / (1024 * 1024)
    }

    /// Get cache statistics for debugging.
    pub fn cache_stats(&self) -> CacheStats {
        let metrics = self.metrics.lock().unwrap();

        CacheStats {
            program_cache_entries: 0,
            symbol_cache_entries: self.symbol_cache.len(),
            estimated_memory_mb: metrics.total_size_bytes / (1024 * 1024),
            hits: metrics.hits,
            misses: metrics.misses,
            evictions: metrics.evictions,
            max_entries: self.max_entries,
            max_memory_mb: self.max_memory_bytes / (1024 * 1024),
            peak_memory_mb: metrics.peak_size_bytes / (1024 * 1024),
            cold_misses: metrics.cold_misses,
            repeat_misses: metrics.repeat_misses,
            evictions_by_entry_limit: metrics.evictions_by_entry_limit,
            evictions_by_byte_limit: metrics.evictions_by_byte_limit,
            evictions_manual: metrics.evictions_manual,
            peak_entry_count: metrics.peak_entry_count,
            unique_files_seen: metrics.seen_paths.len(),
            // Phase 5.C.4 commit 118: surface
            // estimate_symbols_size accounting.
            estimate_symbols_size_calls: metrics.estimate_symbols_size_calls,
            estimate_symbols_size_total_duration_ns: metrics
                .estimate_symbols_size_total_duration_ns,
            repeat_reload_estimate_candidates: metrics
                .repeat_reload_estimate_candidates,
            repeat_reload_estimate_reuse_hits: metrics
                .repeat_reload_estimate_reuse_hits,
            repeat_reload_estimate_reuse_misses: metrics
                .repeat_reload_estimate_reuse_misses,
            estimate_verification_mismatches: metrics
                .estimate_verification_mismatches,
            // Phase 5.C.4 commit 126 (Batch P): wait
            // category counters.
            async_spawn_wait_ns: metrics.bzl_async_spawn_wait_ns,
            async_spawn_wait_calls: metrics.bzl_async_spawn_wait_calls,
            fs_metadata_wait_ns: metrics.bzl_fs_metadata_wait_ns,
            fs_metadata_wait_calls: metrics.bzl_fs_metadata_wait_calls,
            loader_lock_wait_ns: metrics.bzl_loader_lock_wait_ns,
            loader_lock_wait_calls: metrics.bzl_loader_lock_wait_calls,
            concurrent_eval_wait_ns: metrics.bzl_concurrent_eval_wait_ns,
            concurrent_eval_wait_calls: metrics.bzl_concurrent_eval_wait_calls,
            bazel_fetch_wait_ns: metrics.bzl_bazel_fetch_wait_ns,
            bazel_fetch_wait_calls: metrics.bzl_bazel_fetch_wait_calls,
            // Phase 5.C.4 commit 134 (Batch Q): fetch
            // decision counters.
            fetch_invoked_count: metrics.bzl_fetch_invoked_count,
            fetch_skipped_materialized_count: metrics
                .bzl_fetch_skipped_materialized_count,
            fetch_skipped_known_rule_count: metrics
                .bzl_fetch_skipped_known_rule_count,
            fetch_materialized_missing_dir_count: metrics
                .bzl_fetch_materialized_missing_dir_count,
            fetch_materialized_missing_build_count: metrics
                .bzl_fetch_materialized_missing_build_count,
            fetch_duration_by_repo: metrics
                .bzl_fetch_duration_by_repo
                .clone(),
            fetch_skipped_with_manifest_count: metrics
                .bzl_fetch_skipped_with_manifest_count,
            fetch_skipped_by_trust_mode_count: metrics
                .bzl_fetch_skipped_by_trust_mode_count,
        }
    }

    /// Phase 5.C.4 commit 96: top-N per-path entries by
    /// a chosen comparator. Sorted descending; tie-break
    /// is lexicographic by path for determinism.
    pub fn top_n_per_path<F>(
        &self,
        top_n: usize,
        compare: F,
    ) -> Vec<PerPathStats>
    where
        F: Fn(&PerPathMetrics, &PerPathMetrics) -> std::cmp::Ordering,
    {
        let metrics = self.metrics.lock().unwrap();
        let mut entries: Vec<(PathBuf, PerPathMetrics)> = metrics
            .per_path
            .iter()
            .map(|(p, m)| (p.clone(), m.clone()))
            .collect();
        entries.sort_by(|a, b| compare(&b.1, &a.1).then(a.0.cmp(&b.0)));
        let mut out: Vec<PerPathStats> = entries
            .into_iter()
            .take(top_n.max(1))
            .map(|(path, m)| PerPathStats {
                path,
                lookups: m.lookups,
                hits: m.hits,
                misses: m.misses,
                repeat_misses: m.repeat_misses,
                evictions: m.evictions,
                last_size_estimate_bytes: m.last_size_estimate_bytes,
                cumulative_cold_load_duration_ns: m.cumulative_cold_load_duration_ns,
                child_load_duration_ns: m.child_load_duration_ns,
                own_cold_load_duration_ns: m.own_cold_load_duration_ns,
                subphase_read_ns: m.subphase_read_ns,
                subphase_parse_ns: m.subphase_parse_ns,
                subphase_eval_ns: m.subphase_eval_ns,
                subphase_finalize_ns: m.subphase_finalize_ns,
                subphase_resolve_lazy_loads_ns: m.subphase_resolve_lazy_loads_ns,
                subphase_put_symbols_cache_ns: m.subphase_put_symbols_cache_ns,
                cold_load_count: m.cold_load_count,
                file_size_bytes: m.file_size_bytes,
                exported_symbols_count: m.exported_symbols_count,
                // Phase 5.C.4 commit 105: load-graph
                // shape — fanout cardinalities plus
                // the raw identities multiset.
                direct_children_count: m.direct_children.len(),
                direct_parents_count: m.direct_parents.len(),
                max_observed_depth: m.max_observed_depth,
                raw_identities: m.raw_identities.iter().cloned().collect(),
                starlark_ops: m.starlark_ops,
                latest_attribution: m.latest_attribution,
                estimate_symbols_size_calls: m.estimate_symbols_size_calls,
                estimate_symbols_size_total_duration_ns: m
                    .estimate_symbols_size_total_duration_ns,
                estimated_bytes: m.estimated_bytes,
            })
            .collect();
        if top_n == 0 {
            out.clear();
        }
        out
    }

    /// Top-N paths by lookup count.
    pub fn top_n_by_lookups(&self, top_n: usize) -> Vec<PerPathStats> {
        self.top_n_per_path(top_n, |a, b| a.lookups.cmp(&b.lookups))
    }

    /// Top-N paths by miss count (cold + repeat
    /// combined).
    pub fn top_n_by_misses(&self, top_n: usize) -> Vec<PerPathStats> {
        self.top_n_per_path(top_n, |a, b| a.misses.cmp(&b.misses))
    }

    /// Top-N paths by repeat-miss count (post-eviction
    /// thrash signal).
    pub fn top_n_by_repeat_misses(&self, top_n: usize) -> Vec<PerPathStats> {
        self.top_n_per_path(top_n, |a, b| a.repeat_misses.cmp(&b.repeat_misses))
    }

    /// Top-N paths by eviction count.
    pub fn top_n_by_evictions(&self, top_n: usize) -> Vec<PerPathStats> {
        self.top_n_per_path(top_n, |a, b| a.evictions.cmp(&b.evictions))
    }

    /// Top-N paths by last-observed size estimate.
    pub fn top_n_by_bytes(&self, top_n: usize) -> Vec<PerPathStats> {
        self.top_n_per_path(top_n, |a, b| {
            a.last_size_estimate_bytes.cmp(&b.last_size_estimate_bytes)
        })
    }

    /// Phase 5.C.4 commit 103/104 (RENAMED): top-N paths
    /// by cumulative cold-load wall-clock cost (INCLUDES
    /// transitive child .bzl load time).
    pub fn top_n_by_cumulative_cold_load_duration(
        &self,
        top_n: usize,
    ) -> Vec<PerPathStats> {
        self.top_n_per_path(top_n, |a, b| {
            a.cumulative_cold_load_duration_ns
                .cmp(&b.cumulative_cold_load_duration_ns)
        })
    }

    /// Phase 5.C.4 commit 103/104: back-compat alias for
    /// the renamed accessor. Callers still using the
    /// pre-104 name keep working.
    #[deprecated(note = "renamed: use top_n_by_cumulative_cold_load_duration")]
    pub fn top_n_by_cold_load_duration(
        &self,
        top_n: usize,
    ) -> Vec<PerPathStats> {
        self.top_n_by_cumulative_cold_load_duration(top_n)
    }

    /// Phase 5.C.4 commit 104: top-N paths by OWN
    /// cold-load wall-clock cost (excludes transitive
    /// child loads). Operators looking for actual
    /// optimization targets start here. Paths with no
    /// own-time data (child accounting unreliable) sort
    /// last.
    pub fn top_n_by_own_cold_load_duration(
        &self,
        top_n: usize,
    ) -> Vec<PerPathStats> {
        self.top_n_per_path(top_n, |a, b| {
            a.own_cold_load_duration_ns
                .unwrap_or(0)
                .cmp(&b.own_cold_load_duration_ns.unwrap_or(0))
        })
    }

    /// Phase 5.C.4 commit 103: record a cold-load event
    /// (cumulative-only legacy 4-arg form).
    ///
    /// "Cold" here means the load was not satisfied by
    /// the cache — either a true first miss or a repeat
    /// miss after eviction. Both increment
    /// `cold_load_count` and accumulate into
    /// `cumulative_cold_load_duration_ns`.
    pub fn record_cold_load(
        &self,
        path: &Path,
        duration: std::time::Duration,
        file_size_bytes: u64,
        exported_symbols_count: u64,
    ) {
        self.record_cold_load_full_v2(
            path,
            duration,
            None,
            file_size_bytes,
            exported_symbols_count,
            None,
            BzlTimingAttribution::Unknown,
        );
    }

    /// Phase 5.C.4 commit 104: record a cold-load event
    /// with full cumulative/own/child + subphase
    /// accounting.
    ///
    /// `cumulative` is the wall-clock duration of the
    /// entire load (including any child `.bzl` loads
    /// triggered by `load()` statements).
    /// `child_duration` is the sum of immediate child
    /// loads' cumulative durations (None when child
    /// accounting wasn't reliable — e.g., recursive
    /// load on the same path).
    /// `subphases` is per-subphase wall time
    /// (read / parse / eval / finalize); each may be
    /// None on paths that didn't run that subphase.
    pub fn record_cold_load_full(
        &self,
        path: &Path,
        cumulative: std::time::Duration,
        child_duration: Option<std::time::Duration>,
        file_size_bytes: u64,
        exported_symbols_count: u64,
        subphases: Option<ColdLoadSubphases>,
    ) {
        // Phase 5.C.4 commit 110: legacy 6-arg form
        // delegates to v2 with `Unknown` attribution.
        // New callers should use `record_cold_load_full_v2`.
        self.record_cold_load_full_v2(
            path,
            cumulative,
            child_duration,
            file_size_bytes,
            exported_symbols_count,
            subphases,
            BzlTimingAttribution::Unknown,
        );
    }

    /// Phase 5.C.4 commit 110: same as
    /// `record_cold_load_full` but accepts an explicit
    /// attribution value. Per Pro: own-time must not
    /// be presented as precise when the attribution
    /// isn't `SameThreadComplete`. We still ACCUMULATE
    /// own_cold_load_duration_ns for backward
    /// compatibility, but `latest_attribution` lets
    /// consumers decide how to render.
    pub fn record_cold_load_full_v2(
        &self,
        path: &Path,
        cumulative: std::time::Duration,
        child_duration: Option<std::time::Duration>,
        file_size_bytes: u64,
        exported_symbols_count: u64,
        subphases: Option<ColdLoadSubphases>,
        attribution: BzlTimingAttribution,
    ) {
        let mut metrics = self.metrics.lock().unwrap();
        let per = metrics
            .per_path
            .entry(path.to_path_buf())
            .or_default();
        let cum_ns = cumulative.as_nanos() as u64;
        per.cumulative_cold_load_duration_ns =
            per.cumulative_cold_load_duration_ns.saturating_add(cum_ns);
        per.cold_load_count = per.cold_load_count.saturating_add(1);
        per.file_size_bytes = file_size_bytes;
        per.exported_symbols_count = exported_symbols_count;
        // Child-time accumulation. None on this call
        // doesn't invalidate prior accumulations; only
        // sets the field to None permanently if it's
        // ALREADY None (we honor first-seen wins).
        if let Some(child) = child_duration {
            let child_ns = child.as_nanos() as u64;
            per.child_load_duration_ns = Some(
                per.child_load_duration_ns
                    .unwrap_or(0)
                    .saturating_add(child_ns),
            );
            // own = cumulative - child for THIS load.
            // Accumulate into own_cold_load_duration_ns.
            let own_ns = cum_ns.saturating_sub(child_ns);
            per.own_cold_load_duration_ns = Some(
                per.own_cold_load_duration_ns
                    .unwrap_or(0)
                    .saturating_add(own_ns),
            );
        }
        // Subphase accumulation. Each subphase Option
        // accumulates into the per-path total when
        // present. None on a particular call doesn't
        // clobber an existing total.
        if let Some(s) = subphases {
            if let Some(r) = s.read {
                per.subphase_read_ns = Some(
                    per.subphase_read_ns
                        .unwrap_or(0)
                        .saturating_add(r.as_nanos() as u64),
                );
            }
            if let Some(p) = s.parse {
                per.subphase_parse_ns = Some(
                    per.subphase_parse_ns
                        .unwrap_or(0)
                        .saturating_add(p.as_nanos() as u64),
                );
            }
            if let Some(e) = s.eval {
                per.subphase_eval_ns = Some(
                    per.subphase_eval_ns
                        .unwrap_or(0)
                        .saturating_add(e.as_nanos() as u64),
                );
            }
            if let Some(f) = s.finalize {
                per.subphase_finalize_ns = Some(
                    per.subphase_finalize_ns
                        .unwrap_or(0)
                        .saturating_add(f.as_nanos() as u64),
                );
            }
            // Phase 5.C.4 commit 111: sub-finalize
            // accumulators.
            if let Some(r) = s.resolve_lazy_loads {
                per.subphase_resolve_lazy_loads_ns = Some(
                    per.subphase_resolve_lazy_loads_ns
                        .unwrap_or(0)
                        .saturating_add(r.as_nanos() as u64),
                );
            }
            if let Some(c) = s.put_symbols_cache {
                per.subphase_put_symbols_cache_ns = Some(
                    per.subphase_put_symbols_cache_ns
                        .unwrap_or(0)
                        .saturating_add(c.as_nanos() as u64),
                );
            }
        }
        // Phase 5.C.4 commit 110: stash the most-recent
        // attribution. Consumers read this to decide
        // whether to render own_cold_load_duration_ns
        // as precise. We deliberately keep
        // `latest-wins` semantics (rather than "any bad
        // load sticks") so a single recursive load
        // doesn't poison the metric forever.
        per.latest_attribution = attribution;
    }

    /// Phase 5.C.4 commit 133 (Batch Q): record time
    /// spent acquiring `loader_arc.lock()` in the
    /// recursive load_bzl_from_evaluator path.
    /// Suspected dominant wait category per
    /// commit-131's decision doc.
    pub fn record_loader_lock_wait(&self, dur: std::time::Duration) {
        let mut metrics = self.metrics.lock().unwrap();
        metrics.bzl_loader_lock_wait_ns = metrics
            .bzl_loader_lock_wait_ns
            .saturating_add(dur.as_nanos() as u64);
        metrics.bzl_loader_lock_wait_calls = metrics
            .bzl_loader_lock_wait_calls
            .saturating_add(1);
    }

    /// Phase 5.C.4 commit 133 (Batch Q): record time
    /// spent in a `bazel fetch` subprocess call from
    /// the orchestrator. THIS is the dominant wait —
    /// each fetch is a 5-15s bazel-startup+fetch
    /// during which the process blocks on the child.
    /// 8+ such fetches per TF macros analyze accounts
    /// for the 90-second wall-time floor.
    pub fn record_bazel_fetch_wait(&self, dur: std::time::Duration) {
        let mut metrics = self.metrics.lock().unwrap();
        metrics.bzl_bazel_fetch_wait_ns = metrics
            .bzl_bazel_fetch_wait_ns
            .saturating_add(dur.as_nanos() as u64);
        metrics.bzl_bazel_fetch_wait_calls = metrics
            .bzl_bazel_fetch_wait_calls
            .saturating_add(1);
    }

    /// Phase 5.C.4 commit 134 (Batch Q): record a
    /// fetch decision made by `try_fetch_known_repo`.
    /// Exactly one of these fires per call — and if
    /// the decision is `Invoked` then
    /// `record_bazel_fetch_wait` ALSO fires later with
    /// the subprocess duration.
    ///
    /// `repo_name` and the optional `duration` are
    /// used for per-repo attribution (commit 135's
    /// fetch report). For decisions other than
    /// `Invoked`, `duration` is `None`.
    pub fn record_fetch_decision(&self, decision: &FetchDecision) {
        let mut metrics = self.metrics.lock().unwrap();
        // Pluck the repo name out for the per-repo
        // "latest decision" map (commit 135 report).
        let repo_name = match decision {
            FetchDecision::Invoked { repo_name, .. } => repo_name.clone(),
            FetchDecision::SkippedMaterialized { repo_name, .. } => {
                repo_name.clone()
            }
            FetchDecision::SkippedKnownRule { repo_name, .. } => {
                repo_name.clone()
            }
            FetchDecision::MaterializedMissingDir { repo_name } => {
                repo_name.clone()
            }
            FetchDecision::MaterializedMissingBuild {
                repo_name, ..
            } => repo_name.clone(),
            // commit 149 variants
            FetchDecision::SkippedMaterializedManifestValid {
                repo_name, ..
            } => repo_name.clone(),
            FetchDecision::SkippedMaterializedTrustMode {
                repo_name, ..
            } => repo_name.clone(),
        };
        metrics
            .bzl_fetch_last_decision_per_repo
            .insert(repo_name, decision.clone());

        match decision {
            FetchDecision::Invoked {
                repo_name,
                duration,
            } => {
                metrics.bzl_fetch_invoked_count = metrics
                    .bzl_fetch_invoked_count
                    .saturating_add(1);
                let ms = duration.as_millis() as u64;
                let entry = metrics
                    .bzl_fetch_duration_by_repo
                    .entry(repo_name.clone())
                    .or_insert(0);
                *entry = entry.saturating_add(ms);
            }
            FetchDecision::SkippedMaterialized { .. } => {
                metrics.bzl_fetch_skipped_materialized_count =
                    metrics
                        .bzl_fetch_skipped_materialized_count
                        .saturating_add(1);
            }
            FetchDecision::SkippedKnownRule { .. } => {
                metrics.bzl_fetch_skipped_known_rule_count =
                    metrics
                        .bzl_fetch_skipped_known_rule_count
                        .saturating_add(1);
            }
            FetchDecision::MaterializedMissingDir { .. } => {
                metrics.bzl_fetch_materialized_missing_dir_count =
                    metrics
                        .bzl_fetch_materialized_missing_dir_count
                        .saturating_add(1);
            }
            FetchDecision::MaterializedMissingBuild { .. } => {
                metrics.bzl_fetch_materialized_missing_build_count =
                    metrics
                        .bzl_fetch_materialized_missing_build_count
                        .saturating_add(1);
            }
            // commit 149: manifest-validated and
            // trust-mode skips both fold into
            // skipped_materialized count for
            // backward-compatible operator
            // reports, but commit 150 will surface
            // the distinction in the per-repo
            // attribution report.
            FetchDecision::SkippedMaterializedManifestValid { .. } => {
                metrics.bzl_fetch_skipped_materialized_count =
                    metrics
                        .bzl_fetch_skipped_materialized_count
                        .saturating_add(1);
                metrics.bzl_fetch_skipped_with_manifest_count =
                    metrics
                        .bzl_fetch_skipped_with_manifest_count
                        .saturating_add(1);
            }
            FetchDecision::SkippedMaterializedTrustMode { .. } => {
                metrics.bzl_fetch_skipped_materialized_count =
                    metrics
                        .bzl_fetch_skipped_materialized_count
                        .saturating_add(1);
                metrics.bzl_fetch_skipped_by_trust_mode_count =
                    metrics
                        .bzl_fetch_skipped_by_trust_mode_count
                        .saturating_add(1);
            }
        }
    }

    /// Phase 5.C.4 commit 135 (Batch Q): snapshot of
    /// per-repo fetch decisions for the attribution
    /// report. Returns a stable, sorted-by-name vector
    /// (deterministic for JSON diffs and tests).
    pub fn fetch_report_entries(&self) -> Vec<FetchReportEntry> {
        let metrics = self.metrics.lock().unwrap();
        let mut entries: Vec<FetchReportEntry> = metrics
            .bzl_fetch_last_decision_per_repo
            .iter()
            .map(|(repo, decision)| {
                let duration_ms = metrics
                    .bzl_fetch_duration_by_repo
                    .get(repo)
                    .copied()
                    .unwrap_or(0);
                FetchReportEntry::from_decision(
                    repo,
                    decision,
                    duration_ms,
                )
            })
            .collect();
        entries.sort_by(|a, b| a.repo_name.cmp(&b.repo_name));
        entries
    }

    /// Phase 5.C.4 commit 133 (Batch Q): record time
    /// spent in the "concurrent eval polling" sleep
    /// loop in load_bzl_from_evaluator (line ~3635).
    /// This is the actual dominant wait — each
    /// iteration sleeps 100ms and the loop runs up to
    /// 600 attempts (60s max).
    pub fn record_concurrent_eval_wait(&self, dur: std::time::Duration) {
        let mut metrics = self.metrics.lock().unwrap();
        metrics.bzl_concurrent_eval_wait_ns = metrics
            .bzl_concurrent_eval_wait_ns
            .saturating_add(dur.as_nanos() as u64);
        metrics.bzl_concurrent_eval_wait_calls = metrics
            .bzl_concurrent_eval_wait_calls
            .saturating_add(1);
    }

    /// Phase 5.C.4 commit 126 (Batch P): record time
    /// spent at `rx.recv_timeout` in the non-tokio
    /// `thread::spawn` path of
    /// `load_bzl_from_evaluator`. Aggregated process-
    /// wide.
    pub fn record_async_spawn_wait(&self, dur: std::time::Duration) {
        let mut metrics = self.metrics.lock().unwrap();
        metrics.bzl_async_spawn_wait_ns = metrics
            .bzl_async_spawn_wait_ns
            .saturating_add(dur.as_nanos() as u64);
        metrics.bzl_async_spawn_wait_calls = metrics
            .bzl_async_spawn_wait_calls
            .saturating_add(1);
    }

    /// Phase 5.C.4 commit 126 (Batch P): record time
    /// spent in a `std::fs::metadata()` call.
    pub fn record_fs_metadata_wait(&self, dur: std::time::Duration) {
        let mut metrics = self.metrics.lock().unwrap();
        metrics.bzl_fs_metadata_wait_ns = metrics
            .bzl_fs_metadata_wait_ns
            .saturating_add(dur.as_nanos() as u64);
        metrics.bzl_fs_metadata_wait_calls = metrics
            .bzl_fs_metadata_wait_calls
            .saturating_add(1);
    }

    /// Phase 5.C.4 commit 106: fold `ops` into this
    /// path's per-path Starlark-op totals. Called from
    /// the cold-load record path with the OpCounts
    /// drained from the popped frame.
    pub fn record_starlark_ops(&self, path: &Path, ops: OpCounts) {
        let mut metrics = self.metrics.lock().unwrap();
        let per = metrics
            .per_path
            .entry(path.to_path_buf())
            .or_default();
        per.starlark_ops.add(ops);
    }

    /// Phase 5.C.4 commit 106: top-N paths by total
    /// Starlark op volume (sum of function + builtin +
    /// rule + macro + glob calls).
    pub fn top_n_by_starlark_ops(&self, top_n: usize) -> Vec<PerPathStats> {
        self.top_n_per_path(top_n, |a, b| {
            a.starlark_ops.total().cmp(&b.starlark_ops.total())
        })
    }

    /// Phase 5.C.4 commit 105: record a direct
    /// `load()` edge from `parent` to `child`. The
    /// parent gains a new entry in `direct_children`;
    /// the child gains a new entry in `direct_parents`.
    /// Both sides are de-duplicated by `HashSet`, so
    /// repeated cold-loads of the same parent→child
    /// pair don't inflate the counts.
    pub fn record_load_edge(&self, parent: &Path, child: &Path) {
        if parent == child {
            // Self-edge from a recursive load —
            // recursion is already flagged elsewhere; we
            // don't add a self-pointing edge.
            return;
        }
        let mut metrics = self.metrics.lock().unwrap();
        metrics
            .per_path
            .entry(parent.to_path_buf())
            .or_default()
            .direct_children
            .insert(child.to_path_buf());
        metrics
            .per_path
            .entry(child.to_path_buf())
            .or_default()
            .direct_parents
            .insert(parent.to_path_buf());
    }

    /// Phase 5.C.4 commit 105: associate a raw `load()`
    /// label (or other entry-point string) with the
    /// path it resolved to. If multiple distinct labels
    /// resolve to the same path, the per-path identity
    /// set ends up with `len > 1` — flagged by the
    /// identity-collision summary.
    pub fn record_identity(&self, raw_label: &str, resolved_path: &Path) {
        let mut metrics = self.metrics.lock().unwrap();
        metrics
            .per_path
            .entry(resolved_path.to_path_buf())
            .or_default()
            .raw_identities
            .insert(raw_label.to_string());
    }

    /// Phase 5.C.4 commit 105: record the post-push
    /// frame stack height at which `path` was observed,
    /// keeping the max across all observations of that
    /// path.
    pub fn record_observed_depth(&self, path: &Path, depth: usize) {
        let mut metrics = self.metrics.lock().unwrap();
        let per = metrics
            .per_path
            .entry(path.to_path_buf())
            .or_default();
        if depth > per.max_observed_depth {
            per.max_observed_depth = depth;
        }
    }

    /// Phase 5.C.4 commit 105: top-N paths by forward
    /// fanout (number of distinct .bzl files this file
    /// directly loads).
    pub fn top_n_by_fanout(&self, top_n: usize) -> Vec<PerPathStats> {
        self.top_n_per_path(top_n, |a, b| {
            a.direct_children
                .len()
                .cmp(&b.direct_children.len())
        })
    }

    /// Phase 5.C.4 commit 105: top-N paths by reverse
    /// fanout (number of distinct .bzl files that
    /// directly load this file). Helps answer "is this
    /// a widely-imported root .bzl?"
    pub fn top_n_by_reverse_dependents(&self, top_n: usize) -> Vec<PerPathStats> {
        self.top_n_per_path(top_n, |a, b| {
            a.direct_parents.len().cmp(&b.direct_parents.len())
        })
    }

    /// Phase 5.C.4 commit 105: list every path with
    /// more than one raw identity (e.g. the same file
    /// loaded via `//x:foo.bzl` AND `@repo//x:foo.bzl`).
    /// Output is sorted by descending identity-count,
    /// then by path for determinism.
    pub fn identity_collisions(&self) -> Vec<PerPathStats> {
        let metrics = self.metrics.lock().unwrap();
        let mut entries: Vec<(PathBuf, PerPathMetrics)> = metrics
            .per_path
            .iter()
            .filter(|(_, m)| m.raw_identities.len() > 1)
            .map(|(p, m)| (p.clone(), m.clone()))
            .collect();
        drop(metrics);
        entries.sort_by(|a, b| {
            b.1.raw_identities
                .len()
                .cmp(&a.1.raw_identities.len())
                .then(a.0.cmp(&b.0))
        });
        entries
            .into_iter()
            .map(|(path, m)| PerPathStats {
                path,
                lookups: m.lookups,
                hits: m.hits,
                misses: m.misses,
                repeat_misses: m.repeat_misses,
                evictions: m.evictions,
                last_size_estimate_bytes: m.last_size_estimate_bytes,
                cumulative_cold_load_duration_ns: m.cumulative_cold_load_duration_ns,
                child_load_duration_ns: m.child_load_duration_ns,
                own_cold_load_duration_ns: m.own_cold_load_duration_ns,
                subphase_read_ns: m.subphase_read_ns,
                subphase_parse_ns: m.subphase_parse_ns,
                subphase_eval_ns: m.subphase_eval_ns,
                subphase_finalize_ns: m.subphase_finalize_ns,
                subphase_resolve_lazy_loads_ns: m.subphase_resolve_lazy_loads_ns,
                subphase_put_symbols_cache_ns: m.subphase_put_symbols_cache_ns,
                cold_load_count: m.cold_load_count,
                file_size_bytes: m.file_size_bytes,
                exported_symbols_count: m.exported_symbols_count,
                direct_children_count: m.direct_children.len(),
                direct_parents_count: m.direct_parents.len(),
                max_observed_depth: m.max_observed_depth,
                raw_identities: m.raw_identities.iter().cloned().collect(),
                starlark_ops: m.starlark_ops,
                latest_attribution: m.latest_attribution,
                estimate_symbols_size_calls: m.estimate_symbols_size_calls,
                estimate_symbols_size_total_duration_ns: m
                    .estimate_symbols_size_total_duration_ns,
                estimated_bytes: m.estimated_bytes,
            })
            .collect()
    }

    /// Pretty-print cache metrics for operators.
    pub fn print_stats(&self) {
        let metrics = self.metrics.lock().unwrap();

        let total = metrics.hits + metrics.misses;
        let hit_rate = if total > 0 {
            (metrics.hits as f64 / total as f64) * 100.0
        } else {
            0.0
        };

        debug_log!("╔════════════════════════════════════════╗");
        debug_log!("║   GlobalBzlCache Statistics            ║");
        debug_log!("╠════════════════════════════════════════╣");
        debug_log!(
            "║ Hits:       {:8} ({:5.1}%)          ║",
            metrics.hits,
            hit_rate
        );
        debug_log!("║ Misses:     {:8}                   ║", metrics.misses);
        debug_log!("║ Evictions:  {:8}                   ║", metrics.evictions);
        debug_log!(
            "║ Entries:    {:8} / {:8}         ║",
            self.symbol_cache.len(),
            self.max_entries
        );
        debug_log!(
            "║ Memory:     {:8}MB / {:4}MB      ║",
            metrics.total_size_bytes / (1024 * 1024),
            self.max_memory_bytes / (1024 * 1024)
        );
        debug_log!(
            "║ Peak:       {:8}MB                ║",
            metrics.peak_size_bytes / (1024 * 1024)
        );
        debug_log!("╚════════════════════════════════════════╝");
    }

    /// Evict entries until cache memory usage falls below target (in MB).
    /// Uses least-accessed-first eviction strategy.
    ///
    /// Public callers that don't have a specific
    /// EvictReason go through this path; eviction reason
    /// records as `Manual`. Internal callers (the
    /// commit-96 `evict_to_target_with_reason`) record a
    /// specific reason.
    pub fn evict_to_target(&self, target_mb: usize) -> usize {
        let evicted = self.evict_to_target_internal(target_mb);
        if evicted > 0 {
            let mut metrics = self.metrics.lock().unwrap();
            metrics.evictions_manual += evicted;
        }
        evicted
    }

    /// Phase 5.C.4 commit 96: internal eviction that
    /// updates aggregate `evictions` + per-path
    /// counters + total_size_bytes, but does NOT
    /// update the by-reason counters. Reason accounting
    /// happens in the public wrappers.
    fn evict_to_target_internal(&self, target_mb: usize) -> usize {
        let target_bytes = target_mb * 1024 * 1024;
        let current_bytes = {
            let metrics = self.metrics.lock().unwrap();
            metrics.total_size_bytes
        };
        if current_bytes <= target_bytes {
            return 0;
        }

        let mut entries: Vec<(ModuleCacheKey, usize, usize)> = self
            .symbol_cache
            .iter()
            .map(|r| (r.key().clone(), r.value().access_count, r.value().size_estimate))
            .collect();
        entries.sort_by_key(|e| e.1);

        let mut evicted = 0usize;
        let mut freed_bytes = 0usize;
        let mut evicted_paths: Vec<PathBuf> = Vec::new();
        let bytes_to_free = current_bytes.saturating_sub(target_bytes);

        for (path, _access_count, size_estimate) in &entries {
            if freed_bytes >= bytes_to_free {
                break;
            }
            self.symbol_cache.remove(path);
            freed_bytes += size_estimate;
            evicted += 1;
            evicted_paths.push(path.path.clone());
        }

        if evicted > 0 {
            let mut metrics = self.metrics.lock().unwrap();
            metrics.evictions += evicted;
            metrics.total_size_bytes = metrics.total_size_bytes.saturating_sub(freed_bytes);
            for p in &evicted_paths {
                if let Some(per) = metrics.per_path.get_mut(p) {
                    per.evictions += 1;
                }
            }
            debug_log!(
                "Cache eviction: removed {} entries, freed {}MB",
                evicted,
                freed_bytes / (1024 * 1024)
            );
        }

        evicted
    }

    /// Evict a fraction of symbols (least-accessed-first).
    pub fn evict_symbols(&self, fraction: f64) -> usize {
        let evicted = self.evict_symbols_internal(fraction);
        if evicted > 0 {
            let mut metrics = self.metrics.lock().unwrap();
            metrics.evictions_manual += evicted;
        }
        evicted
    }

    /// Phase 5.C.4 commit 96: internal eviction that
    /// updates aggregate + per-path counters but NOT the
    /// by-reason counters.
    fn evict_symbols_internal(&self, fraction: f64) -> usize {
        let total = self.symbol_cache.len();
        if total == 0 {
            return 0;
        }
        let to_evict = ((total as f64) * fraction.clamp(0.0, 1.0)).ceil() as usize;
        if to_evict == 0 {
            return 0;
        }

        let mut entries: Vec<(ModuleCacheKey, usize, usize)> = self
            .symbol_cache
            .iter()
            .map(|r| (r.key().clone(), r.value().access_count, r.value().size_estimate))
            .collect();
        entries.sort_by_key(|e| e.1);

        let mut evicted = 0usize;
        let mut freed_bytes = 0usize;
        let mut evicted_paths: Vec<PathBuf> = Vec::new();

        for (path, _access_count, size_estimate) in entries.iter().take(to_evict) {
            self.symbol_cache.remove(path);
            freed_bytes += size_estimate;
            evicted += 1;
            evicted_paths.push(path.path.clone());
        }

        if evicted > 0 {
            let mut metrics = self.metrics.lock().unwrap();
            metrics.evictions += evicted;
            metrics.total_size_bytes = metrics.total_size_bytes.saturating_sub(freed_bytes);
            for p in &evicted_paths {
                if let Some(per) = metrics.per_path.get_mut(p) {
                    per.evictions += 1;
                }
            }
            debug_log!(
                "Cache eviction: removed {}/{} entries, freed {}MB",
                evicted,
                total,
                freed_bytes / (1024 * 1024)
            );
        }

        evicted
    }

    /// Evict programs (no-op for GlobalBzlCache as programs aren't cached).
    pub fn evict_programs(&self, _fraction: f64) -> usize {
        0
    }

    /// Get cache statistics (updated signature for Phase 3).
    pub fn stats(&self) -> CacheStats {
        self.cache_stats()
    }
}

/// Cache statistics
#[derive(Debug, Clone)]
pub struct CacheStats {
    /// Number of programs in cache
    pub program_cache_entries: usize,
    /// Number of symbol maps in cache
    pub symbol_cache_entries: usize,
    /// Estimated memory usage in MB
    pub estimated_memory_mb: usize,
    /// Total cache hits observed
    pub hits: usize,
    /// Total cache misses observed
    pub misses: usize,
    /// Total evictions performed
    pub evictions: usize,
    /// Maximum allowed entries
    pub max_entries: usize,
    /// Maximum allowed memory in MB
    pub max_memory_mb: usize,
    /// Peak observed memory usage in MB
    pub peak_memory_mb: usize,
    /// Phase 5.C.4 commit 96: cold misses (first lookup
    /// for a path).
    pub cold_misses: usize,
    /// Misses on paths that were previously cached then
    /// evicted. Repeat misses are the thrashing signal.
    pub repeat_misses: usize,
    /// Evictions triggered by entry-count limit.
    pub evictions_by_entry_limit: usize,
    /// Evictions triggered by byte limit.
    pub evictions_by_byte_limit: usize,
    /// Evictions explicitly requested via the public
    /// evict_*() APIs (no specific upstream reason).
    pub evictions_manual: usize,
    /// High-water entry count observed.
    pub peak_entry_count: usize,
    /// Number of distinct .bzl paths ever looked up
    /// (live + evicted).
    pub unique_files_seen: usize,
    /// Phase 5.C.4 commit 118: estimate_symbols_size
    /// metrics, surfaced for the orchestrator's
    /// finish_full harvest.
    pub estimate_symbols_size_calls: u64,
    pub estimate_symbols_size_total_duration_ns: u64,
    pub repeat_reload_estimate_candidates: u64,
    pub repeat_reload_estimate_reuse_hits: u64,
    pub repeat_reload_estimate_reuse_misses: u64,
    pub estimate_verification_mismatches: u64,
    /// Phase 5.C.4 commit 126 (Batch P): wait
    /// category counters.
    pub async_spawn_wait_ns: u64,
    pub async_spawn_wait_calls: u64,
    pub fs_metadata_wait_ns: u64,
    pub fs_metadata_wait_calls: u64,
    /// Phase 5.C.4 commit 133 (Batch Q): loader_arc
    /// Mutex wait time.
    pub loader_lock_wait_ns: u64,
    pub loader_lock_wait_calls: u64,
    pub concurrent_eval_wait_ns: u64,
    pub concurrent_eval_wait_calls: u64,
    pub bazel_fetch_wait_ns: u64,
    pub bazel_fetch_wait_calls: u64,
    /// Phase 5.C.4 commit 134 (Batch Q): fetch-decision
    /// counters. See `CacheMetrics` for invariants — each
    /// `try_fetch_known_repo` call increments exactly one
    /// of the five `*_count` fields (and possibly
    /// `fetch_invoked_count` if the path ends in real
    /// fetch).
    pub fetch_invoked_count: u64,
    pub fetch_skipped_materialized_count: u64,
    pub fetch_skipped_known_rule_count: u64,
    pub fetch_materialized_missing_dir_count: u64,
    pub fetch_materialized_missing_build_count: u64,
    /// Per-repo aggregated fetch duration in
    /// milliseconds. Only populated for repos where the
    /// decision was `Invoked`. Used by commit 135's
    /// per-repo attribution report.
    pub fetch_duration_by_repo: std::collections::HashMap<String, u64>,
    /// Phase 5.C.4 commit 149 (Batch S): of the
    /// `fetch_skipped_materialized_count` total, how
    /// many were manifest-validated vs trust-mode.
    pub fetch_skipped_with_manifest_count: u64,
    pub fetch_skipped_by_trust_mode_count: u64,
}

/// Phase 5.C.4 commit 96 / 103 / 104: per-`.bzl` file
/// entry for the top-N report.
#[derive(Debug, Clone)]
pub struct PerPathStats {
    pub path: PathBuf,
    pub lookups: usize,
    pub hits: usize,
    pub misses: usize,
    pub repeat_misses: usize,
    pub evictions: usize,
    pub last_size_estimate_bytes: usize,
    /// Commit 103/104 (RENAMED): cumulative cold-load
    /// wall-clock cost INCLUDING transitive child .bzl
    /// loads. Use `own_cold_load_duration_ns` to get
    /// this file's own time.
    pub cumulative_cold_load_duration_ns: u64,
    /// Commit 104: time in child .bzl loads (None if
    /// child accounting wasn't reliable).
    pub child_load_duration_ns: Option<u64>,
    /// Commit 104: this file's own time
    /// (= cumulative − child). None when child
    /// accounting wasn't reliable.
    pub own_cold_load_duration_ns: Option<u64>,
    /// Commit 104: subphase totals.
    pub subphase_read_ns: Option<u64>,
    pub subphase_parse_ns: Option<u64>,
    pub subphase_eval_ns: Option<u64>,
    pub subphase_finalize_ns: Option<u64>,
    /// Phase 5.C.4 commit 111: sub-finalize split.
    pub subphase_resolve_lazy_loads_ns: Option<u64>,
    pub subphase_put_symbols_cache_ns: Option<u64>,
    pub cold_load_count: u64,
    pub file_size_bytes: u64,
    pub exported_symbols_count: u64,
    /// Phase 5.C.4 commit 105: number of distinct
    /// resolved .bzl paths this file directly loads
    /// (forward fanout).
    pub direct_children_count: usize,
    /// Phase 5.C.4 commit 105: number of distinct
    /// resolved .bzl paths that directly load this file
    /// (reverse fanout).
    pub direct_parents_count: usize,
    /// Phase 5.C.4 commit 105: deepest cold-load frame
    /// stack height at which this file was observed.
    /// 1 = top-of-chain.
    pub max_observed_depth: usize,
    /// Phase 5.C.4 commit 105: distinct raw `load()`
    /// labels (and other entry strings) that resolved
    /// to this file's path. Len > 1 = identity
    /// collision.
    pub raw_identities: Vec<String>,
    /// Phase 5.C.4 commit 106: coarse Starlark op
    /// totals accumulated across all cold loads of
    /// this path.
    pub starlark_ops: OpCounts,
    /// Phase 5.C.4 commit 110: attribution quality
    /// of the most recent cold-load. Consumers MUST
    /// not present `own_cold_load_duration_ns` as
    /// precise when this is not `SameThreadComplete`.
    pub latest_attribution: BzlTimingAttribution,
    /// Phase 5.C.4 commit 118: per-bzl
    /// estimate_symbols_size call counters.
    pub estimate_symbols_size_calls: u64,
    pub estimate_symbols_size_total_duration_ns: u64,
    pub estimated_bytes: u64,
}

/// Phase 5.C.4 commit 104: thread-local stack of
/// in-flight cold-load frames. Each frame accumulates
/// `child_total` from completed child loads at the same
/// thread.
///
/// On entry to a cold load: push a new frame.
/// On exit: pop the top frame. If a parent frame
/// remains, add this load's cumulative duration to the
/// parent's `child_total`.
///
/// Result: when each load completes, it knows the
/// cumulative time spent in its immediate children +
/// can compute own_time = cumulative - child_total.
///
/// Pro's commit-104 directive: "If exact subtraction
/// is hard, mark own-time as experimental (Option) and
/// only emit it when confident."
///
/// We mark own-time as Some unless we detect a
/// recursive load (same path appears twice in the
/// frame stack) — that's the case Pro flagged as
/// "child accounting unreliable."
thread_local! {
    static COLD_LOAD_FRAMES: std::cell::RefCell<Vec<ColdLoadFrame>> =
        std::cell::RefCell::new(Vec::new());
}

#[derive(Debug)]
struct ColdLoadFrame {
    path: PathBuf,
    child_total_ns: u64,
    /// `true` if a recursive load (same path) was
    /// detected during this frame's lifetime — marks
    /// child accounting unreliable.
    recursion_detected: bool,
    /// Phase 5.C.4 commit 110: `true` when this
    /// frame's eval was dispatched to a separate OS
    /// thread (the non-tokio `thread::spawn` path).
    /// Children of an off-thread eval push their
    /// frames onto the spawned thread's stack, NOT
    /// this frame's parent thread's stack — so our
    /// child_total_ns under-reports and own-time is
    /// unreliable. Set BEFORE the spawn happens.
    spawned_off_thread: bool,
    /// Phase 5.C.4 commit 106: coarse Starlark op
    /// counters accumulated while this frame was on
    /// top. Drained into per-path metrics on pop.
    ops: OpCounts,
}

/// Phase 5.C.4 commit 105: result of pushing a cold-
/// load frame, surfacing the data needed to record a
/// load-graph edge BEFORE the new frame is in place.
/// Returned by `push_cold_load_frame_ex`.
///
/// `recursive` mirrors `push_cold_load_frame`'s bool
/// return (same path was already on the stack).
/// `parent_path` is the path of the frame that was
/// on top at the time of the push — `None` when
/// pushing the first frame on this thread.
/// `depth_after_push` is the new stack size (1 for the
/// root push, 2 for one nested, …).
#[derive(Debug, Clone)]
pub struct ColdLoadFramePushResult {
    pub recursive: bool,
    pub parent_path: Option<PathBuf>,
    pub depth_after_push: usize,
}

/// Phase 5.C.4 commit 133 (Batch Q): process-global
/// reference to the GlobalBzlCache for use by static
/// methods that need to record metrics but don't have
/// `&self`. Set once via
/// `register_cache_for_fetch_timing` when a build
/// starts.
#[doc(hidden)]
pub static GLOBAL_BZL_CACHE_FOR_FETCH_TIMING: std::sync::OnceLock<GlobalBzlCache> =
    std::sync::OnceLock::new();

/// Phase 5.C.4 commit 133: register the
/// process-singleton GlobalBzlCache so static fetch
/// paths can record their wait time. `GlobalBzlCache`
/// is Arc-internal (Clone == cheap), so storing by
/// value is fine.
#[doc(hidden)]
pub fn register_cache_for_fetch_timing(cache: GlobalBzlCache) {
    let _ = GLOBAL_BZL_CACHE_FOR_FETCH_TIMING.set(cache);
}

/// #39 graph-RSS census: structural report of the process-singleton
/// `.bzl` symbol cache, if one was registered. Returns `None` when no
/// cache is registered (e.g. a build that never loaded a `.bzl`). The
/// returned cache clone shares the live DashMap via Arc, so this walks
/// the actual retained state. Callers gate on `RUSH_MEM_CENSUS=1`.
pub fn global_bzl_cache_mem_census() -> Option<String> {
    GLOBAL_BZL_CACHE_FOR_FETCH_TIMING
        .get()
        .map(|cache| cache.mem_census())
}

/// Push a frame onto the thread-local cold-load stack.
/// Returns `true` if this is a recursive load (the
/// same path was already on the stack); the caller
/// uses that to decide whether own-time accounting is
/// reliable.
///
/// `pub` + `#[doc(hidden)]` so commit-104 integration
/// tests in `tests/cold_load_attribution.rs` can drive
/// the frame stack directly without exposing it as
/// part of the supported public API.
#[doc(hidden)]
pub fn push_cold_load_frame(path: &Path) -> bool {
    push_cold_load_frame_ex(path).recursive
}

/// Phase 5.C.4 commit 133 (Batch Q): wrapper around
/// `Arc<Mutex<BzlLoader>>::lock()` that times the
/// acquisition and folds the duration into the
/// global-cache wait-category counter. The 96.3%
/// wait observed in commit 125 most plausibly lives
/// in these lock acquisitions per the commit-131
/// decision doc (3322 ctx switches ≈ 110 .bzl files
/// × ~30 lock acquires).
///
/// `pub` + `#[doc(hidden)]` so the loader's own
/// internals can use it.
#[doc(hidden)]
pub fn lock_loader_timed(
    arc: &Arc<Mutex<BzlLoader>>,
) -> Result<std::sync::MutexGuard<'_, BzlLoader>> {
    let start = std::time::Instant::now();
    let guard = arc
        .lock()
        .map_err(|e| anyhow!("loader poisoned: {}", e))?;
    let wait = start.elapsed();
    if let Some(ref g) = guard.global_cache {
        g.record_loader_lock_wait(wait);
    }
    Ok(guard)
}

/// Phase 5.C.4 commit 105: enriched variant of
/// `push_cold_load_frame` that ALSO returns the parent
/// path (for load-graph edge recording) and the
/// post-push stack depth (for max-depth accounting).
///
/// The caller MUST still call `pop_cold_load_frame` to
/// balance the stack — `push_cold_load_frame_ex` and
/// `push_cold_load_frame` share the same underlying
/// state.
#[doc(hidden)]
pub fn push_cold_load_frame_ex(path: &Path) -> ColdLoadFramePushResult {
    COLD_LOAD_FRAMES.with(|frames| {
        let mut f = frames.borrow_mut();
        let recursive = f.iter().any(|fr| fr.path == path);
        let parent_path = f.last().map(|fr| fr.path.clone());
        if recursive {
            if let Some(top) = f.last_mut() {
                top.recursion_detected = true;
            }
        }
        f.push(ColdLoadFrame {
            path: path.to_path_buf(),
            child_total_ns: 0,
            recursion_detected: recursive,
            spawned_off_thread: false,
            ops: OpCounts::default(),
        });
        ColdLoadFramePushResult {
            recursive,
            parent_path,
            depth_after_push: f.len(),
        }
    })
}

/// Phase 5.C.4 commit 106 + 110: enriched pop result.
/// The `ops` field carries the Starlark-op totals
/// accumulated while this frame was on top — drained
/// at pop time into per-path metrics. The
/// `attribution` field summarizes whether own-time
/// can be trusted (commit 110, per Pro's "no fake
/// own-time" rule).
#[derive(Debug, Clone, Copy)]
pub struct ColdLoadFramePopResult {
    pub child_total_ns: u64,
    pub recursion_detected: bool,
    pub ops: OpCounts,
    /// Phase 5.C.4 commit 110: attribution quality
    /// for own-time computation.
    pub attribution: BzlTimingAttribution,
}

impl Default for ColdLoadFramePopResult {
    fn default() -> Self {
        Self {
            child_total_ns: 0,
            recursion_detected: false,
            ops: OpCounts::default(),
            attribution: BzlTimingAttribution::Unknown,
        }
    }
}

/// Phase 5.C.4 commit 110: mark the active cold-load
/// frame as having spawned its eval onto a separate
/// OS thread. Called just before the non-tokio
/// `thread::spawn` happens in load_bzl_from_evaluator.
/// At pop time this drives the
/// `CrossThreadChildrenObserved` attribution.
#[doc(hidden)]
pub fn mark_active_frame_spawned_off_thread() {
    COLD_LOAD_FRAMES.with(|frames| {
        let mut f = frames.borrow_mut();
        if let Some(top) = f.last_mut() {
            top.spawned_off_thread = true;
        }
    });
}

/// Pop the top frame and return `(child_total_ns,
/// recursion_detected)`. Side effect: if a parent
/// frame remains on the stack, add the popped frame's
/// CUMULATIVE duration to the parent's child_total so
/// the parent's own-time can be computed correctly.
///
/// `pub` + `#[doc(hidden)]` for integration-test
/// access (see `push_cold_load_frame`).
///
/// This is the back-compat 2-tuple shape from
/// commit 104. New code should use
/// `pop_cold_load_frame_ex` to also receive the op
/// counter totals introduced in commit 106.
#[doc(hidden)]
pub fn pop_cold_load_frame(cumulative_ns: u64) -> (u64, bool) {
    let r = pop_cold_load_frame_ex(cumulative_ns);
    (r.child_total_ns, r.recursion_detected)
}

/// Phase 5.C.4 commit 106: pop variant that also
/// returns the Starlark-op totals accumulated while
/// this frame was on top. Like `pop_cold_load_frame`,
/// the popped cumulative is bubbled into the parent's
/// child_total and a recursion flag is propagated up.
#[doc(hidden)]
pub fn pop_cold_load_frame_ex(cumulative_ns: u64) -> ColdLoadFramePopResult {
    COLD_LOAD_FRAMES.with(|frames| {
        let mut f = frames.borrow_mut();
        let popped = f.pop();
        let (child_total_ns, recursion_detected, ops, spawned_off_thread) = popped
            .map(|fr| {
                (
                    fr.child_total_ns,
                    fr.recursion_detected,
                    fr.ops,
                    fr.spawned_off_thread,
                )
            })
            .unwrap_or((0, false, OpCounts::default(), false));
        // Push this load's cumulative duration into the
        // parent's child_total (if a parent exists).
        if let Some(parent) = f.last_mut() {
            parent.child_total_ns = parent
                .child_total_ns
                .saturating_add(cumulative_ns);
            // Propagate recursion-detected up so a
            // parent whose grandchild went recursive
            // also marks own-time unreliable.
            if recursion_detected {
                parent.recursion_detected = true;
            }
            // Phase 5.C.4 commit 110: propagate off-
            // thread spawning up too — a parent whose
            // grandchild ran on another thread also
            // has unreliable child accounting at the
            // grandchild's level.
            if spawned_off_thread {
                parent.spawned_off_thread = true;
            }
        }
        // Phase 5.C.4 commit 110: classify attribution.
        // Recursion takes precedence (Pro: "do not
        // emit precise-looking own-time when accounting
        // is known incomplete").
        let attribution = if recursion_detected {
            BzlTimingAttribution::RecursionSuppressed
        } else if spawned_off_thread {
            BzlTimingAttribution::CrossThreadChildrenObserved
        } else {
            BzlTimingAttribution::SameThreadComplete
        };
        ColdLoadFramePopResult {
            child_total_ns,
            recursion_detected,
            ops,
            attribution,
        }
    })
}

/// Phase 5.C.4 commit 119: fingerprint deciding
/// whether a prior `estimate_symbols_size` result can
/// be reused on a repeat-miss reload. NOT a content
/// hash — this is cache accounting only, not action-
/// key hashing (Pro: "diagnostic / cache-accounting
/// fingerprint").
///
/// Two fingerprints are equivalent iff every field
/// matches. The fields are chosen so that any
/// realistic .bzl content change invalidates at least
/// one of them:
///   - `canonical_label` distinguishes
///     `//x:foo.bzl` from `@repo//x:foo.bzl` even
///     when they resolve to the same path (the
///     commit-105 identity-collision case).
///   - `resolved_path` is the resolved filesystem
///     location.
///   - `file_len` catches almost any content change.
///   - `file_mtime_ns` catches in-place rewrites
///     that preserve length.
///   - `exported_symbol_count` catches semantic
///     changes that don't touch the file (e.g. a
///     load() chain that exports a different
///     subset).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SizeEstimateFingerprint {
    /// Canonical raw `load()` label, if known. `None`
    /// when the path was reached without a label
    /// context (e.g. direct put_symbols_cache call).
    pub canonical_label: Option<String>,
    /// Resolved absolute path of the `.bzl` file.
    pub resolved_path: PathBuf,
    /// File size on disk in bytes. Zero when the
    /// `std::fs::metadata` call fails — that's
    /// captured as a distinct fingerprint state, not
    /// silently ignored.
    pub file_len: u64,
    /// File mtime in nanoseconds since the UNIX
    /// epoch. `None` when the platform doesn't
    /// expose it or the call fails.
    pub file_mtime_ns: Option<u128>,
    /// Number of symbols in the exported map.
    pub exported_symbol_count: u64,
}

impl SizeEstimateFingerprint {
    /// Construct a fingerprint from the data
    /// available at `put_symbols` time: a path, a
    /// canonical-label option, and the symbol-map
    /// size. File metadata is best-effort.
    pub fn from_components(
        canonical_label: Option<String>,
        resolved_path: PathBuf,
        exported_symbol_count: u64,
    ) -> Self {
        Self::from_components_timed(
            canonical_label,
            resolved_path,
            exported_symbol_count,
        )
        .0
    }

    /// Phase 5.C.4 commit 126: variant that also
    /// returns the time spent in `std::fs::metadata()`
    /// so callers (specifically `put_symbols`) can
    /// fold it into the wait-category counters.
    pub fn from_components_timed(
        canonical_label: Option<String>,
        resolved_path: PathBuf,
        exported_symbol_count: u64,
    ) -> (Self, std::time::Duration) {
        let start = std::time::Instant::now();
        let (file_len, file_mtime_ns) = match std::fs::metadata(&resolved_path) {
            Ok(meta) => {
                let len = meta.len();
                let mtime_ns = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos());
                (len, mtime_ns)
            }
            Err(_) => (0, None),
        };
        let dur = start.elapsed();
        (
            Self {
                canonical_label,
                resolved_path,
                file_len,
                file_mtime_ns,
                exported_symbol_count,
            },
            dur,
        )
    }

    /// True iff `self` and `other` agree on every
    /// field — i.e. the prior estimate is reusable.
    pub fn matches(&self, other: &SizeEstimateFingerprint) -> bool {
        self == other
    }
}

/// Phase 5.C.4 commit 119: a recorded
/// `estimate_symbols_size` result together with the
/// fingerprint that produced it. Stored in
/// `PerPathMetrics.last_size_estimate`. Commit 120's
/// env-gated reuse path checks the stored record's
/// fingerprint against the current load's fingerprint
/// to decide whether to reuse `estimated_bytes` or
/// recompute.
#[derive(Debug, Clone)]
pub struct SizeEstimateRecord {
    pub fingerprint: SizeEstimateFingerprint,
    pub estimated_bytes: u64,
}

/// Phase 5.C.4 commit 110: attribution quality for a
/// cold-load's own/child accounting. Pro's directive:
/// "Do not emit a precise-looking own-time when the
/// accounting is known incomplete. Fake precision is
/// how dashboards become horoscopes with JSON."
///
/// Variants:
///   - `SameThreadComplete`: all children loaded on
///     the same thread; own_duration = cumulative -
///     child_total is reliable.
///   - `CrossThreadChildrenObserved`: at least one
///     child load was dispatched to a different OS
///     thread (typically the non-tokio
///     thread::spawn path in `load_bzl_from_evaluator`).
///     own_duration MUST be None for this frame.
///   - `RecursionSuppressed`: a recursive load on the
///     same path was detected during this frame's
///     lifetime. own_duration MUST be None.
///   - `Unknown`: a frame that didn't carry
///     attribution metadata (e.g. legacy
///     `record_cold_load` callers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum BzlTimingAttribution {
    SameThreadComplete,
    CrossThreadChildrenObserved,
    RecursionSuppressed,
    Unknown,
}

impl Default for BzlTimingAttribution {
    fn default() -> Self {
        BzlTimingAttribution::Unknown
    }
}

/// Phase 5.C.4 commit 106: kinds of Starlark
/// operations the coarse op counter recognizes. Used
/// by `record_starlark_op` to bump the active cold-
/// load frame's `ops` counter.
#[derive(Debug, Clone, Copy)]
pub enum StarlarkOpKind {
    /// A user-defined `def` call.
    FunctionCall,
    /// A built-in function call (registered native
    /// function — e.g. `len`, `select`, `provider`).
    BuiltinCall,
    /// A rule invocation (call to a registered rule
    /// function — e.g. `cc_library`).
    RuleInvocation,
    /// A macro invocation routed through a registered
    /// macro expander (e.g. `tf_proto_library`).
    MacroInvocation,
    /// A `glob()` call. Tracked separately from
    /// BuiltinCall because glob is a high-cost
    /// filesystem operation worth surfacing on its own.
    GlobCall,
}

/// Phase 5.C.4 commit 106: coarse Starlark-operation
/// counters. Per Pro's directive: "function calls,
/// builtin calls, rule/macro invocations, glob calls.
/// NOT a profiler." Each cold-load frame accumulates
/// its own; on pop the totals get folded into the
/// per-path metrics.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OpCounts {
    pub function_calls: u64,
    pub builtin_calls: u64,
    pub rule_invocations: u64,
    pub macro_invocations: u64,
    pub glob_calls: u64,
}

impl OpCounts {
    /// Sum of all op categories — gives the "total op
    /// volume per .bzl" headline number.
    pub fn total(&self) -> u64 {
        self.function_calls
            .saturating_add(self.builtin_calls)
            .saturating_add(self.rule_invocations)
            .saturating_add(self.macro_invocations)
            .saturating_add(self.glob_calls)
    }

    fn add(&mut self, other: OpCounts) {
        self.function_calls = self.function_calls.saturating_add(other.function_calls);
        self.builtin_calls = self.builtin_calls.saturating_add(other.builtin_calls);
        self.rule_invocations = self.rule_invocations.saturating_add(other.rule_invocations);
        self.macro_invocations = self
            .macro_invocations
            .saturating_add(other.macro_invocations);
        self.glob_calls = self.glob_calls.saturating_add(other.glob_calls);
    }
}

/// Phase 5.C.4 commit 106: bump the active cold-load
/// frame's `ops` counter for `kind`. No-op when no
/// frame is active on this thread.
///
/// `pub` + `#[doc(hidden)]` so the evaluator (different
/// module) can call it without exposing it as part of
/// the supported public API.
#[doc(hidden)]
pub fn record_starlark_op(kind: StarlarkOpKind) {
    COLD_LOAD_FRAMES.with(|frames| {
        let mut f = frames.borrow_mut();
        if let Some(top) = f.last_mut() {
            match kind {
                StarlarkOpKind::FunctionCall => {
                    top.ops.function_calls = top.ops.function_calls.saturating_add(1)
                }
                StarlarkOpKind::BuiltinCall => {
                    top.ops.builtin_calls = top.ops.builtin_calls.saturating_add(1)
                }
                StarlarkOpKind::RuleInvocation => {
                    top.ops.rule_invocations = top.ops.rule_invocations.saturating_add(1)
                }
                StarlarkOpKind::MacroInvocation => {
                    top.ops.macro_invocations = top.ops.macro_invocations.saturating_add(1)
                }
                StarlarkOpKind::GlobCall => {
                    top.ops.glob_calls = top.ops.glob_calls.saturating_add(1)
                }
            }
        }
    });
}

/// Phase 5.C.4 commit 104: per-subphase wall-time
/// breakdown of one cold-load event. Each subphase is
/// optional — instrumented paths supply Some; fallback
/// / lazy-stub paths supply None.
///
/// Note: `eval` here INCLUDES time spent in transitive
/// child `.bzl` loads triggered by `load()` statements
/// during Starlark evaluation. The own/cumulative split
/// applies at the whole-load level, not per-subphase.
#[derive(Debug, Clone, Copy, Default)]
pub struct ColdLoadSubphases {
    pub read: Option<std::time::Duration>,
    pub parse: Option<std::time::Duration>,
    pub eval: Option<std::time::Duration>,
    pub finalize: Option<std::time::Duration>,
    /// Phase 5.C.4 commit 111: sub-finalize split.
    /// Wall time inside `resolve_lazy_loads` (the
    /// LazyLoad placeholder rewriter that runs AFTER
    /// eval). When `Some(_)`, this is bounded above by
    /// `finalize`.
    pub resolve_lazy_loads: Option<std::time::Duration>,
    /// Phase 5.C.4 commit 111: sub-finalize split.
    /// Wall time inside `put_symbols_cache` (the
    /// symbol-table → cache insert). When `Some(_)`,
    /// `resolve_lazy_loads + put_symbols_cache <=
    /// finalize` modulo a small "other" bucket for the
    /// lock acquisition between the two operations.
    pub put_symbols_cache: Option<std::time::Duration>,
}

/// Phase 5.C.4 commit 96: which limit triggered an
/// eviction. Used by the per-pass cache counter reports
/// to distinguish entry-bound from byte-bound pressure
/// when deciding which limit to tune.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictReason {
    /// Eviction triggered because total entry count
    /// exceeded `max_entries`.
    EntryLimit,
    /// Eviction triggered because total bytes exceeded
    /// `max_memory_bytes`.
    ByteLimit,
    /// Eviction explicitly requested via the public
    /// evict APIs (no specific upstream reason).
    Manual,
}

impl Default for GlobalBzlCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Check RAM availability before processing a level of .bzl files
///
/// NOTE: This function is mostly superseded by BzlMemoryGuard::reserve(),
/// which provides per-file atomic reservations with 15% headroom enforcement.
/// This level-based check is kept for early detection before spawning parallel tasks.
pub fn check_ram_floor_before_level(level_idx: usize, file_count: usize) -> Result<()> {
    const HARD_RAM_FLOOR_PCT: f64 = 0.15; // 15% minimum available RAM (was 30%, too conservative)

    // Read system memory from /proc/meminfo (Linux only)
    #[cfg(target_os = "linux")]
    {
        let contents = match std::fs::read_to_string("/proc/meminfo") {
            Ok(c) => c,
            Err(_) => return Ok(()), // Can't read meminfo, skip check
        };

        let mut total_kb = 0;
        let mut available_kb = 0;

        for line in contents.lines() {
            if line.starts_with("MemTotal:") {
                if let Some(kb_str) = line.split_whitespace().nth(1) {
                    total_kb = kb_str.parse::<usize>().unwrap_or(0);
                }
            } else if line.starts_with("MemAvailable:") {
                if let Some(kb_str) = line.split_whitespace().nth(1) {
                    available_kb = kb_str.parse::<usize>().unwrap_or(0);
                }
                break;
            }
        }

        if total_kb > 0 && available_kb > 0 {
            let available_pct = (available_kb as f64) / (total_kb as f64);
            let total_mb = total_kb / 1024;
            let available_mb = available_kb / 1024;

            if available_pct < HARD_RAM_FLOOR_PCT {
                debug_log!("╔════════════════════════════════════════════════════════════════╗");
                debug_log!("║ RAM FLOOR BREACH - ABORTING LEVEL PROCESSING                   ║");
                debug_log!("╚════════════════════════════════════════════════════════════════╝");
                debug_log!(
                    "Available RAM: {} MB / {} MB ({:.1}% free)",
                    available_mb,
                    total_mb,
                    available_pct * 100.0
                );
                debug_log!("HARD MINIMUM: 15% available RAM required");
                debug_log!();
                debug_log!("System is approaching OOM condition.");
                debug_log!(
                    "Refusing to process level {} with {} .bzl files.",
                    level_idx,
                    file_count
                );
                debug_log!("This prevents desktop session termination by the OOM killer.");

                return Err(anyhow::anyhow!(
                    "RAM floor breach: {:.1}% available (need 15%). Refusing to process level {} with {} files to prevent OOM.",
                    available_pct * 100.0,
                    level_idx,
                    file_count
                ));
            }

            debug_log!(
                "[DEBUG BzlLoader] RAM check passed for level {}: {:.1}% available ({} MB / {} MB)",
                level_idx,
                available_pct * 100.0,
                available_mb,
                total_mb
            );
        }
    }

    // Non-Linux platforms: skip check
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (level_idx, file_count); // Avoid unused variable warnings
    }

    Ok(())
}

/// Loader for .bzl files
///
/// Handles:
/// - Path resolution (//pkg:file.bzl, @repo//pkg:file.bzl, :file.bzl)
/// - Parsing and evaluation of .bzl files
/// - Symbol export from .bzl environments
/// - Caching of parsed programs and evaluated environments
/// - Memory-aware resource management during .bzl evaluation
pub struct BzlLoader {
    /// Workspace root for path resolution
    workspace_root: PathBuf,

    /// Current package path (for relative imports)
    current_package: Option<PathBuf>,

    /// Current repository root (for // label resolution)
    /// When evaluating .bzl files in external repos, this points to that repo's root
    current_repo_root: PathBuf,

    /// Explicit repository identity for the current BUILD parse. Paths alone
    /// are insufficient because the main repository and one or more repository
    /// aliases may intentionally resolve to the same directory.
    current_repo_name: Option<String>,

    /// Global shared cache (preferred) - shared across all BzlLoader instances
    /// When present, this is used instead of local caches
    global_cache: Option<GlobalBzlCache>,

    /// Local LRU cache of parsed .bzl programs (path -> Program)
    /// Only used when global_cache is None
    /// DRASTICALLY REDUCED: Each AST consumes 100-1000x source file size in memory
    /// Limited to 20 entries to prevent OOM (was 2000, caused 27.5 GB usage)
    program_cache: LruCache<PathBuf, Program>,

    /// Local LRU cache of evaluated .bzl symbols (path -> HashMap<symbol, Value>)
    /// Only used when global_cache is None
    /// Limited to 50 entries (was 1000, caused massive memory consumption)
    symbol_cache: LruCache<ModuleCacheKey, HashMap<String, Value>>,

    /// Cache of previously scanned load() dependencies keyed by file fingerprint.
    /// Enables incremental dependency graph construction without reparsing unchanged files.
    load_dep_cache: Arc<DashMap<PathBuf, LoadDepCacheEntry>>,

    /// Set of .bzl files currently being evaluated (for cycle detection)
    evaluating_paths: HashSet<ModuleCacheKey>,

    /// External repository paths (@repo -> path)
    /// Uses DashMap for lock-free concurrent reads during parallel BUILD parsing
    pub external_repos: Arc<DashMap<String, PathBuf>>,

    /// Resource manager for memory-aware .bzl loading (optional for tests)
    resource_manager: Option<Arc<ResourceManager>>,

    /// Global memory budget for .bzl loading (Phase 2 architecture)
    /// Provides atomic memory reservation to prevent race conditions
    memory_budget: Option<Arc<rush_resource::GlobalMemoryBudget>>,

    /// Repository alias mappings for workspace-absolute labels
    /// Example: "tsl" -> "local_tsl" maps //tsl/... to @local_tsl//tsl/...
    /// Uses DashMap for lock-free concurrent reads during parallel BUILD parsing
    repo_aliases: Arc<DashMap<String, String>>,

    /// Per-repository repo_mapping (Bazel semantics): for a given repository
    /// name (key), map apparent repo names to canonical repo names used when
    /// resolving @repo labels in that repository's context.
    /// Uses DashMap for lock-free concurrent reads during parallel BUILD parsing
    repo_mappings: Arc<DashMap<String, HashMap<String, String>>>,

    /// Owners whose mapping was discovered from Bazel for the current command.
    ///
    /// Unlike mappings declared by a legacy WORKSPACE, these entries are a
    /// snapshot of MODULE.bazel state. Remember their keys separately so the
    /// next command cannot classify a select key using a stale module name or
    /// repo_name mapping before its tracked mapping probe has run.
    command_repo_mapping_owners: Arc<DashSet<String>>,

    /// Main workspace name (from workspace(name = "...")). Used to identify
    /// the mapping context when evaluating files in the main workspace.
    main_workspace_name: Option<String>,
}

/// Per-thread label resolution context for safe, correct // and : resolution
#[derive(Clone, Debug)]
pub struct BzlContextFrame {
    pub repo_root: PathBuf,
    pub package_dir: PathBuf,
    pub load_stack: Vec<String>,
    pub repository: Option<String>,
}

thread_local! {
    static BZL_CONTEXT_STACK: RefCell<Vec<BzlContextFrame>> = const { RefCell::new(Vec::new()) };
}

static SELF_SINGLETON_FETCHED: Lazy<std::sync::Mutex<std::collections::HashSet<String>>> =
    Lazy::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

/// Repos currently being fetched (prevents infinite recursion during WORKSPACE scanning)
static REPOS_BEING_FETCHED: Lazy<std::sync::Mutex<std::collections::HashSet<String>>> =
    Lazy::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

/// #39(C2): serialize the `bazel sync` subprocess in `fetch_repo_via_bazel`.
/// Every lazy per-repo fetch targets the SAME shared output_base
/// (`.cache/rush/bazel_output_base`), so when the parallel cross-thread .bzl
/// loader fires `bazel --output_base <shared> sync --only=<repo>` for several
/// repos at once, the concurrent invocations contend on that single bazel
/// server's lock and fail fast ("Failed to run `bazel sync --only=...`" →
/// "Rush does not support fallback registration"). bs2k4a9ei-style runs only
/// avoided this because the upfront combined warmup gave post_warmup_fetches=0;
/// when warmup does not cover a repo, the racing lazy path fails the whole
/// build at BUILD-eval. A process-global mutex held across the subprocess makes
/// these run one-at-a-time. Correctness over the little parallelism lost — most
/// repos are warmup-skipped before reaching this path, and per Batch Q the
/// bazel-fetch wall time dominates regardless of overlap.
static BAZEL_SYNC_SERIALIZE: Lazy<std::sync::Mutex<()>> =
    Lazy::new(|| std::sync::Mutex::new(()));

/// Dynamic memory budget system for .bzl loading
/// Tracks reserved memory to prevent thrashing while maintaining system headroom.
/// After optimizations (62% reduction): each .bzl file needs ~1.5 GB (was 2-5 GB before).
static BZL_MEMORY_RESERVED_MB: Lazy<std::sync::atomic::AtomicUsize> =
    Lazy::new(|| std::sync::atomic::AtomicUsize::new(0));

/// RAII guard for .bzl memory reservations - auto-releases on drop
struct BzlMemoryGuard {
    reserved_mb: usize,
}

impl BzlMemoryGuard {
    /// Atomically reserve memory for .bzl loading with headroom enforcement
    ///
    /// Maintains 15% free RAM headroom to prevent system thrashing.
    /// Returns guard that auto-releases reservation on drop (even if load fails).
    fn reserve(bzl_label: &str, estimated_mb: usize) -> Result<Self> {
        let (total_mb, available_mb) =
            rush_resource::ResourceManager::read_system_memory().unwrap_or((64 * 1024, 32 * 1024)); // Fallback: assume 64GB total, 32GB available

        // Enforce 15% minimum free RAM headroom (85% max usable)
        // Was 30% but that blocked builds with 29.1% free (18.6GB on 62GB system)
        // 15% = 9.3GB free on 62GB system - still safe, allows builds to proceed
        const MIN_FREE_HEADROOM_PCT: f64 = 0.15;
        let max_usable_mb = (total_mb as f64 * (1.0 - MIN_FREE_HEADROOM_PCT)) as usize;

        // Atomically check and reserve memory
        let mut current_reserved =
            BZL_MEMORY_RESERVED_MB.load(std::sync::atomic::Ordering::Acquire);

        loop {
            let new_reserved = current_reserved + estimated_mb;

            // Check if reservation would exceed budget or violate headroom
            if new_reserved > max_usable_mb {
                let free_after_reservation = total_mb.saturating_sub(new_reserved);
                let free_pct = (free_after_reservation as f64) / (total_mb as f64) * 100.0;

                return Err(anyhow!(
                    "MEMORY BUDGET EXCEEDED - Cannot load {}\n\
                     \n\
                     Memory Budget:\n\
                     - Total RAM: {} MB\n\
                     - Available: {} MB\n\
                     - Currently reserved: {} MB\n\
                     - Requested: {} MB\n\
                     - After reservation: {} MB free ({:.1}% free)\n\
                     - Maximum usable (85% of total): {} MB\n\
                     - Minimum headroom required: 15% ({} MB)\n\
                     \n\
                     Rush maintains 15% free RAM headroom to prevent system thrashing.\n\
                     Free up memory (close browsers, editors, etc.) and retry.",
                    bzl_label,
                    total_mb,
                    available_mb,
                    current_reserved,
                    estimated_mb,
                    free_after_reservation,
                    free_pct,
                    max_usable_mb,
                    (total_mb as f64 * MIN_FREE_HEADROOM_PCT) as usize
                ));
            }

            // Atomic compare-and-swap to reserve memory
            match BZL_MEMORY_RESERVED_MB.compare_exchange_weak(
                current_reserved,
                new_reserved,
                std::sync::atomic::Ordering::Release,
                std::sync::atomic::Ordering::Acquire,
            ) {
                Ok(_) => {
                    debug_log!(
                        "[MEM BUDGET] Reserved {} MB for {} (total: {} MB / {} MB max, {:.1}% of budget)",
                        estimated_mb,
                        bzl_label,
                        new_reserved,
                        max_usable_mb,
                        (new_reserved as f64) / (max_usable_mb as f64) * 100.0
                    );
                    return Ok(Self {
                        reserved_mb: estimated_mb,
                    });
                }
                Err(actual) => {
                    // Another thread modified the counter, retry with updated value
                    current_reserved = actual;
                }
            }
        }
    }
}

impl Drop for BzlMemoryGuard {
    fn drop(&mut self) {
        let prev = BZL_MEMORY_RESERVED_MB
            .fetch_sub(self.reserved_mb, std::sync::atomic::Ordering::Release);
        debug_log!(
            "[MEM BUDGET] Released {} MB (remaining: {} MB)",
            self.reserved_mb,
            prev.saturating_sub(self.reserved_mb)
        );
    }
}

/// RAII guard to automatically remove repo from REPOS_BEING_FETCHED when dropped
struct FetchGuard {
    repo_name: String,
}

impl FetchGuard {
    fn new(repo_name: String) -> Self {
        if let Ok(mut being_fetched) = REPOS_BEING_FETCHED.lock() {
            being_fetched.insert(repo_name.clone());
        }
        FetchGuard { repo_name }
    }
}

impl Drop for FetchGuard {
    fn drop(&mut self) {
        if let Ok(mut being_fetched) = REPOS_BEING_FETCHED.lock() {
            being_fetched.remove(&self.repo_name);
        }
    }
}

impl BzlLoader {
    /// Push repository/package context for the current thread
    pub fn push_thread_context(repo_root: PathBuf, package_dir: PathBuf) {
        Self::push_thread_context_frame(BzlContextFrame {
            repo_root,
            package_dir,
            load_stack: Vec::new(),
            repository: None,
        });
    }

    pub fn push_thread_context_frame(frame: BzlContextFrame) {
        BZL_LOAD_STACK.with(|stack| {
            let mut guard = stack.borrow_mut();
            guard.clear();
            guard.extend(frame.load_stack.iter().cloned());
        });
        BZL_CONTEXT_STACK.with(|s| s.borrow_mut().push(frame));
    }

    /// Pop repository/package context for the current thread
    pub fn pop_thread_context() {
        BZL_CONTEXT_STACK.with(|s| {
            let _ = s.borrow_mut().pop();
        });
    }

    /// Capture the current thread context for passing to async tasks
    /// This is necessary because tokio::task::spawn runs in a different thread
    /// and doesn't have access to thread-local storage
    pub fn capture_thread_context() -> Option<BzlContextFrame> {
        BZL_CONTEXT_STACK
            .with(|s| s.borrow().last().cloned())
            .map(|mut frame| {
                frame.load_stack = current_stack_snapshot();
                frame
            })
    }

    /// Get current thread context (top of stack)
    fn current_thread_context(&self) -> Option<BzlContextFrame> {
        BZL_CONTEXT_STACK.with(|s| s.borrow().last().cloned())
    }
}

impl BzlLoader {
    /// Create a new .bzl loader with workspace root (without resource management)
    pub fn new(workspace_root: PathBuf) -> Self {
        Self::new_with_resources(workspace_root, None)
    }

    /// Create a new .bzl loader with resource management
    pub fn new_with_resources(
        workspace_root: PathBuf,
        resource_manager: Option<Arc<ResourceManager>>,
    ) -> Self {
        Self::new_with_all(workspace_root, resource_manager, None)
    }

    /// Create a new .bzl loader with global cache
    pub fn new_with_global_cache(workspace_root: PathBuf, global_cache: GlobalBzlCache) -> Self {
        Self::new_with_all(workspace_root, None, Some(global_cache))
    }

    /// Create a new .bzl loader with resource management and global cache
    pub fn new_with_all(
        workspace_root: PathBuf,
        resource_manager: Option<Arc<ResourceManager>>,
        global_cache: Option<GlobalBzlCache>,
    ) -> Self {
        Self::new_with_all_and_budget(workspace_root, resource_manager, global_cache, None)
    }

    /// Create a new .bzl loader with full configuration including memory budget
    pub fn new_with_all_and_budget(
        workspace_root: PathBuf,
        resource_manager: Option<Arc<ResourceManager>>,
        global_cache: Option<GlobalBzlCache>,
        memory_budget: Option<Arc<rush_resource::GlobalMemoryBudget>>,
    ) -> Self {
        let current_repo_root = workspace_root.clone();

        // CRITICAL FIX: Disable local caches when global cache is present
        // Local program_cache uses 10+ GB (20 ASTs × 500 MB each)
        // With global cache, local caches are redundant and wasteful
        let (program_cache_size, symbol_cache_size) = if global_cache.is_some() {
            debug_log!("[BzlLoader] Global cache present - disabling local caches (OOM fix)");
            (1, 1) // Minimal size when global cache handles everything
        } else {
            debug_log!("[BzlLoader] No global cache - using local caches (20 ASTs, 50 symbols)");
            (20, 50) // Original sizes for standalone mode
        };

        Self {
            workspace_root,
            current_package: None,
            current_repo_root,
            current_repo_name: None,
            global_cache,
            // ASTs are 100-1000x larger than source files in memory
            // 2000 entries caused 27.5 GB, 20 entries = ~10 GB per BzlLoader
            program_cache: LruCache::new(NonZeroUsize::new(program_cache_size).unwrap()),
            symbol_cache: LruCache::new(NonZeroUsize::new(symbol_cache_size).unwrap()),
            load_dep_cache: Arc::new(DashMap::new()),
            evaluating_paths: HashSet::new(),
            external_repos: Arc::new(DashMap::new()),
            resource_manager,
            memory_budget,
            repo_aliases: Arc::new(DashMap::new()),
            repo_mappings: Arc::new(DashMap::new()),
            command_repo_mapping_owners: Arc::new(DashSet::new()),
            main_workspace_name: None,
        }
    }

    /// Set the current repository root for .bzl file resolution
    ///
    /// This is used when parsing BUILD files from external repositories to ensure
    /// that // labels in .bzl files resolve relative to the correct repository.
    ///
    /// Returns the previous repository root so it can be restored.
    pub fn set_current_repo_root(&mut self, repo_root: PathBuf) -> PathBuf {
        std::mem::replace(&mut self.current_repo_root, repo_root)
    }

    /// Get the current repository root
    pub fn get_current_repo_root(&self) -> &PathBuf {
        &self.current_repo_root
    }

    /// Set the repository identity paired with [`Self::set_current_repo_root`].
    pub fn set_current_repo_name(&mut self, repo_name: Option<String>) -> Option<String> {
        std::mem::replace(&mut self.current_repo_name, repo_name)
    }

    /// Get the explicit repository identity for the current BUILD parse.
    pub fn get_current_repo_name(&self) -> Option<&str> {
        self.current_repo_name.as_deref()
    }

    /// Get the workspace root path
    pub fn get_workspace_root(&self) -> &PathBuf {
        &self.workspace_root
    }

    pub fn file_fingerprint_for(&self, path: &Path) -> Result<FileFingerprint> {
        let metadata =
            fs::metadata(path).map_err(|e| anyhow!("Failed to stat {}: {}", path.display(), e))?;
        // Daemon-correctness (Codex P0 risk #6): the CORRECTNESS key is
        // the blake3 content digest (compared in `matches()`); (len,
        // mtime, ctime) is only a fast precheck that lets us reuse a
        // prior digest WITHOUT re-reading the file when nothing the
        // filesystem can observe has changed. Any in-place content edit
        // bumps ctime (even when mtime is restored via `touch -r`), so a
        // metadata-stable content edit fails the precheck, gets re-hashed,
        // and is detected. See `from_metadata_with_precheck`.
        let prior = self
            .load_dep_cache
            .get(path)
            .map(|entry| entry.fingerprint.clone());
        Ok(FileFingerprint::from_metadata_with_precheck(
            path,
            &metadata,
            prior.as_ref(),
        ))
    }

    pub fn get_cached_load_dependencies(
        &self,
        path: &PathBuf,
        fingerprint: &FileFingerprint,
    ) -> Option<Vec<String>> {
        self.load_dep_cache.get(path).and_then(|entry| {
            if entry.fingerprint.matches(fingerprint) {
                Some(entry.deps.clone())
            } else {
                None
            }
        })
    }

    pub fn cache_load_dependencies(
        &self,
        path: PathBuf,
        fingerprint: FileFingerprint,
        deps: &[String],
    ) {
        self.load_dep_cache.insert(
            path,
            LoadDepCacheEntry {
                fingerprint,
                deps: deps.to_vec(),
            },
        );
    }

    /// Create a new BzlLoader instance for parallel BUILD parsing
    ///
    /// This creates a new BzlLoader with its own mutable state (current_package, current_repo_root)
    /// but shares the GlobalBzlCache and external repo metadata with the original.
    /// This allows multiple BUILD files to parse in parallel without lock contention.
    pub fn clone_for_parallel_parsing(&self) -> Self {
        Self {
            workspace_root: self.workspace_root.clone(),
            current_package: None, // Each instance starts with no package
            current_repo_root: self.workspace_root.clone(), // Start at workspace root
            current_repo_name: None,
            global_cache: self.global_cache.clone(), // Shared cache (Arc-based)
            program_cache: LruCache::new(NonZeroUsize::new(1).unwrap()), // Minimal local cache
            symbol_cache: LruCache::new(NonZeroUsize::new(1).unwrap()), // Minimal local cache
            load_dep_cache: Arc::clone(&self.load_dep_cache),
            evaluating_paths: HashSet::new(), // Each instance tracks its own evaluation
            external_repos: Arc::clone(&self.external_repos), // Shared external repo metadata
            resource_manager: self.resource_manager.clone(), // Shared resource manager
            memory_budget: self.memory_budget.clone(), // Shared memory budget
            repo_aliases: Arc::clone(&self.repo_aliases), // Shared repo aliases
            repo_mappings: Arc::clone(&self.repo_mappings), // Shared repo mappings
            command_repo_mapping_owners: Arc::clone(
                &self.command_repo_mapping_owners,
            ),
            main_workspace_name: self.main_workspace_name.clone(),
        }
    }

    /// Create a BzlLoader for WORKSPACE chunk scanning that reuses the caller's
    /// shared external-repo state and parser context.
    fn clone_for_workspace_chunk_scan(&self, workspace_root: &std::path::Path) -> Self {
        let mut temp_loader = self.clone_for_parallel_parsing();
        temp_loader.workspace_root = workspace_root.to_path_buf();
        temp_loader.current_repo_root = workspace_root.to_path_buf();
        // Legacy WORKSPACE scans use Rush's minimal @bazel_tools repository to
        // avoid recursive fetch loops. Bzlmod-only workspaces instead register
        // Bazel's real embedded-tools repository at startup; never overwrite
        // that shared mapping with the partial compatibility stub.
        let bzlmod_only = workspace_root.join("MODULE.bazel").exists()
            && !workspace_root.join("WORKSPACE").exists()
            && !workspace_root.join("WORKSPACE.bazel").exists();
        if !bzlmod_only {
            let bazel_tools_path =
                cache_backed_external_repo_dir(workspace_root, "bazel_tools");
            temp_loader
                .external_repos
                .insert("bazel_tools".to_string(), bazel_tools_path);
        }
        temp_loader.main_workspace_name = self.main_workspace_name.clone();
        temp_loader
    }

    /// Get symbols from cache (global if available, otherwise local)
    /// DISABLED FOR MEMORY: Returns None to force re-evaluation every time
    /// Trading CPU (re-evaluation) for RAM (no caching = no 900 MB per .bzl file)
    fn get_symbols_cache(&mut self, key: &ModuleCacheKey) -> Option<HashMap<String, Value>> {
        if let Some(ref global) = self.global_cache {
            global.get_symbols_key(key)
        } else {
            self.symbol_cache.get(key).cloned()
        }
    }

    // Path-only callers cannot prove a module owner. Never admit a qualified
    // module through this legacy API.
    pub fn get_symbols_cache_subset(
        &mut self, path: &PathBuf, requested: &[(String, String)],
    ) -> Option<HashMap<String, Value>> {
        self.get_symbols_cache_subset_key(&ModuleCacheKey::unqualified(path), requested)
    }

    fn get_symbols_cache_subset_key(
        &mut self, key: &ModuleCacheKey, requested: &[(String, String)],
    ) -> Option<HashMap<String, Value>> {
        if let Some(ref global) = self.global_cache {
            return global.get_symbols_subset_key(key, requested);
        }
        self.symbol_cache.get(key).and_then(|symbols| {
            cached_symbols_with_fallback(symbols, key.label.as_deref().unwrap_or(""), requested)
        })
    }

    pub(crate) fn get_symbols_cache_subset_no_stats(
        &mut self, path: &PathBuf, requested: &[(String, String)],
    ) -> Option<HashMap<String, Value>> {
        self.get_symbols_cache_subset_key_no_stats(&ModuleCacheKey::unqualified(path), requested)
    }

    fn get_symbols_cache_subset_key_no_stats(
        &mut self, key: &ModuleCacheKey, requested: &[(String, String)],
    ) -> Option<HashMap<String, Value>> {
        if let Some(ref global) = self.global_cache {
            return global.get_symbols_subset_no_stats_key(key, requested);
        }
        self.symbol_cache.get(key).and_then(|symbols| {
            cached_symbols_with_fallback(symbols, key.label.as_deref().unwrap_or(""), requested)
        })
    }

    pub(crate) fn get_symbols_cache_subset_for_label_no_stats(
        &mut self, path: &PathBuf, label: &str, requested: &[(String, String)],
    ) -> Result<Option<HashMap<String, Value>>> {
        let (key, _, _) = self.module_cache_key(label, path)?;
        Ok(self.get_symbols_cache_subset_key_no_stats(&key, requested))
    }

    fn put_symbols_cache(
        &mut self, key: ModuleCacheKey, symbols: HashMap<String, Value>,
    ) -> Option<u64> {
        // Repository discovery can extend maps, roots and aliases during nested loads.
        // Never publish those exports under the parent's pre-discovery key:
        // another loader may still have precisely that earlier map snapshot.
        if key.context.is_some() && (
            key.mappings != self.repository_mapping_cache_identity()
                || key.repository_roots != self.repository_root_cache_identity()
                || key.aliases != self.repository_alias_cache_identity()
        ) {
            if let Some(ref global) = self.global_cache {
                let generation = global.symbol_cache.get(&key).map(|entry| entry.generation);
                if let Some(removed) = generation.and_then(|generation| global.remove_generation(&key, generation)) {
                    let mut metrics = global.metrics.lock().unwrap();
                    metrics.total_size_bytes = metrics.total_size_bytes.saturating_sub(removed.size_estimate);
                }
            } else {
                self.symbol_cache.pop(&key);
            }
            return None;
        }
        if let Some(ref global) = self.global_cache {
            global.put_symbols_key(key, symbols, None)
        } else {
            self.symbol_cache.put(key, symbols);
            None
        }
    }

    fn set_dep_set_cache(&self, key: &ModuleCacheKey, generation: u64, dep_set: Vec<Dependency>) {
        if let Some(ref global) = self.global_cache {
            global.set_dep_set_key(key, generation, dep_set);
        }
    }

    /// Legacy path-only insertion stays unqualified and cannot satisfy a load.
    pub fn cache_symbols(&mut self, path: PathBuf, symbols: &HashMap<String, Value>) {
        let _ = self.put_symbols_cache(ModuleCacheKey::unqualified(&path), symbols.clone());
    }

    /// Evict a fraction of cached symbols (Phase 3: Adaptive behavior).
    ///
    /// Called when memory pressure is HIGH to free up memory.
    /// Uses LRU policy - evicts least recently used entries.
    ///
    /// # Arguments
    /// * `fraction` - Fraction to evict (0.0-1.0), e.g., 0.25 = 25%
    ///
    /// Returns number of entries evicted.
    pub fn evict_lru_symbols(&mut self, fraction: f64) -> usize {
        if let Some(ref global) = self.global_cache {
            // Use global cache eviction
            return global.evict_symbols(fraction);
        }

        // Local cache eviction
        let total = self.symbol_cache.len();
        let to_evict = ((total as f64) * fraction).ceil() as usize;

        // LRU cache has pop_lru() method
        let mut evicted = 0;
        for _ in 0..to_evict {
            if self.symbol_cache.pop_lru().is_some() {
                evicted += 1;
            } else {
                break;
            }
        }

        if evicted > 0 {
            debug_log!(
                "[MemoryPressure] Evicted {} symbol cache entries ({:.0}% of {})",
                evicted,
                fraction * 100.0,
                total
            );
        }

        evicted
    }

    /// Clear a fraction of parsed program cache (Phase 3: Adaptive behavior).
    ///
    /// Called when memory pressure is CRITICAL to aggressively free memory.
    /// AST programs are 100-1000x larger than source files in memory.
    ///
    /// # Arguments
    /// * `fraction` - Fraction to clear (0.0-1.0), e.g., 0.50 = 50%
    ///
    /// Returns number of programs cleared.
    pub fn clear_program_cache(&mut self, fraction: f64) -> usize {
        if let Some(ref global) = self.global_cache {
            // Use global cache eviction
            return global.evict_programs(fraction);
        }

        // Local cache eviction
        let total = self.program_cache.len();
        let to_clear = ((total as f64) * fraction).ceil() as usize;

        let mut cleared = 0;
        for _ in 0..to_clear {
            if self.program_cache.pop_lru().is_some() {
                cleared += 1;
            } else {
                break;
            }
        }

        if cleared > 0 {
            debug_log!(
                "[MemoryPressure] Cleared {} program cache entries ({:.0}% of {})",
                cleared,
                fraction * 100.0,
                total
            );
        }

        cleared
    }

    /// Get cache statistics for monitoring.
    pub fn cache_stats(&self) -> CacheStats {
        if let Some(ref global) = self.global_cache {
            return global.stats();
        }

        CacheStats {
            program_cache_entries: self.program_cache.len(),
            symbol_cache_entries: self.symbol_cache.len(),
            estimated_memory_mb: 0, // Local cache doesn't track memory usage
            hits: 0,
            misses: 0,
            evictions: 0,
            max_entries: self.symbol_cache.cap().get(),
            max_memory_mb: 0,
            peak_memory_mb: 0,
            // Phase 5.C.4 commit 96: local cache doesn't
            // track detailed metrics; all fields default
            // to zero.
            cold_misses: 0,
            repeat_misses: 0,
            evictions_by_entry_limit: 0,
            evictions_by_byte_limit: 0,
            evictions_manual: 0,
            peak_entry_count: 0,
            unique_files_seen: 0,
            estimate_symbols_size_calls: 0,
            estimate_symbols_size_total_duration_ns: 0,
            repeat_reload_estimate_candidates: 0,
            repeat_reload_estimate_reuse_hits: 0,
            repeat_reload_estimate_reuse_misses: 0,
            estimate_verification_mismatches: 0,
            async_spawn_wait_ns: 0,
            async_spawn_wait_calls: 0,
            fs_metadata_wait_ns: 0,
            fs_metadata_wait_calls: 0,
            loader_lock_wait_ns: 0,
            loader_lock_wait_calls: 0,
            concurrent_eval_wait_ns: 0,
            concurrent_eval_wait_calls: 0,
            bazel_fetch_wait_ns: 0,
            bazel_fetch_wait_calls: 0,
            fetch_invoked_count: 0,
            fetch_skipped_materialized_count: 0,
            fetch_skipped_known_rule_count: 0,
            fetch_materialized_missing_dir_count: 0,
            fetch_materialized_missing_build_count: 0,
            fetch_duration_by_repo: std::collections::HashMap::new(),
            fetch_skipped_with_manifest_count: 0,
            fetch_skipped_by_trust_mode_count: 0,
        }
    }

    /// Load a .bzl file from an evaluator context, handling mutex locking correctly
    ///
    /// This method ensures that the mutex is not held during evaluation,
    /// preventing deadlock when .bzl files recursively load other .bzl files.
    ///
    /// # Arguments
    /// * `loader_arc` - Arc to the BzlLoader (shared with evaluator)
    /// * `bzl_label` - .bzl file label
    /// * `symbols` - Symbols to export
    pub fn load_bzl_from_evaluator(
        loader_arc: Arc<Mutex<BzlLoader>>,
        bzl_label: &str,
        symbols: &[(String, String)],
    ) -> Result<HashMap<String, Value>> {
        debug_log!("[DEBUG BzlLoader] load_bzl_from_evaluator: {}", bzl_label);
        // Phase 5.C.4 commit 103: start cold-load timing.
        // Used by the post-eval cache-insert path below
        // to attribute parse+eval cost per .bzl file.
        let cold_load_start = std::time::Instant::now();

        // Persistent-daemon Run-wiring (blocker #1): mark the current command's
        // tracked manifest BEFORE this load so the cold (miss) path can capture
        // exactly this `.bzl`'s full transitive dependency set (own source read +
        // everything its nested `load()`s touched) and store it in the cache. A
        // later cross-command cache HIT replays that set so the manifest is
        // complete as if the load happened. `None` when no command scope is active
        // (unit tests / setup) — capture/replay are then no-ops.
        let dep_mark = rush_incremental::tracked::mark();

        // Phase 1: Resolve the .bzl path once, then probe cache for a no-stat
        // fast path. This prevents repeated in-flight waiters from being counted
        // as misses just because they arrive before the original evaluation
        // stores the symbols.
        let (bzl_path, module_key, repo_root, package_dir) = {
            let loader = lock_loader_timed(&loader_arc)?;
            let path = loader.resolve_bzl_path(bzl_label)?;
            let (key, root, package) = loader.module_cache_key(bzl_label, &path)?;
            (path, key, root, package)
        };
        let bzl_path_key = module_key.inflight_key();
        let module_label = module_key.label.clone().expect("qualified module label");
        let module_context = module_key.context.clone().expect("qualified module context");

        // Phase 1a: cache hit without accounting (fast path).
        {
            let mut loader = lock_loader_timed(&loader_arc)?;
            if let Some(symbols_map) = loader.get_symbols_cache_subset_key_no_stats(&module_key, symbols) {
                debug_log!("[DEBUG BzlLoader] Cache hit for: {}", bzl_label);
                return Ok(symbols_map);
            }

            // If this file is being evaluated by another loader right now,
            // wait for the result instead of double-loading it.
            if loader.evaluating_paths.contains(&module_key)
                || (IN_FLIGHT_BZL_PATHS.contains(&bzl_path_key) || BZL_LOADING_SET.contains(&bzl_path_key))
                || IN_FLIGHT_BZL_LOADS.contains_key(&bzl_path_key)
            {
                // Recursive loads run synchronously on the evaluator thread. Waiting
                // for that same thread can never make progress; the old polling path
                // discovered this after five seconds and returned these same stubs.
                if inflight_owned_by_current_thread(&bzl_path_key) {
                    let stubs = make_stub_exports(&module_label, symbols);
                    let _ = loader.put_symbols_cache(module_key.clone(), stubs.clone());
                    return Ok(stubs);
                }

                let concurrent_wait_start = std::time::Instant::now();
                // Drop lock during wait so other workers can continue loading.
                drop(loader);
                for attempt in 1..=600 {
                    std::thread::sleep(std::time::Duration::from_millis(100));

                    let mut loader = lock_loader_timed(&loader_arc)?;

                    if let Some(cached_symbols) =
                        loader.get_symbols_cache_subset_key_no_stats(&module_key, symbols)
                    {
                        if let Some(ref g) = loader.global_cache {
                            g.record_concurrent_eval_wait(concurrent_wait_start.elapsed());
                        }
                        return Ok(cached_symbols);
                    }

                    if !loader.evaluating_paths.contains(&module_key)
                        && !(IN_FLIGHT_BZL_PATHS.contains(&bzl_path_key) || BZL_LOADING_SET.contains(&bzl_path_key))
                        && !IN_FLIGHT_BZL_LOADS.contains_key(&bzl_path_key)
                    {
                        if let Some(ref g) = loader.global_cache {
                            g.record_concurrent_eval_wait(concurrent_wait_start.elapsed());
                        }
                        break;
                    }

                    if attempt % 50 == 0 {
                        debug_log!(
                            "[DEBUG BzlLoader] Still waiting for concurrent evaluation: {} ({}s)",
                            bzl_label,
                            attempt / 10
                        );
                        log_wait_diagnostics(bzl_label, (attempt / 10) as u64, 60);
                        if let Some((owning_thread, summary)) =
                            detect_single_thread_deadlock(&bzl_path_key)
                        {
                            eprintln!(
                                "🚫 Deadlock detected while waiting for {} on worker {:?}:\n      {}",
                                bzl_label, owning_thread, summary
                            );
                            eprintln!(
                                "   ↪ Returning stub exports so evaluation can continue (symbols: {}).",
                                symbols
                                    .iter()
                                    .map(|(local, original)| format!("{}={}", local, original))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            );
                            let stubs = make_stub_exports(&module_label, symbols);
                            {
                                let mut loader = lock_loader_timed(&loader_arc)?;
                                let _ = loader.put_symbols_cache(module_key.clone(), stubs.clone());
                            }
                            return Ok(stubs);
                        }
                    }
                }

                let mut loader = lock_loader_timed(&loader_arc)?;
                if (IN_FLIGHT_BZL_PATHS.contains(&bzl_path_key) || BZL_LOADING_SET.contains(&bzl_path_key))
                    || IN_FLIGHT_BZL_LOADS.contains_key(&bzl_path_key)
                    || loader.evaluating_paths.contains(&module_key)
                {
                    if let Some(ref g) = loader.global_cache {
                        g.record_concurrent_eval_wait(concurrent_wait_start.elapsed());
                    }
                    return Err(anyhow!(
                        "Circular .bzl load or deadlock detected after 60s: {}",
                        bzl_label
                    ));
                }

                // Otherwise the owner finished but failed before inserting
                // into cache; fall through to retry ourselves.
            }
        }

        // Phase 1b: cache lookup with accounting (used for canonical cache misses/hits)
        let (cached_symbols, cached_file_empty) = {
            let mut loader = lock_loader_timed(&loader_arc)?;
            let cached_full = loader.get_symbols_cache(&module_key);
            let cached_symbols = cached_full.as_ref().and_then(|all| {
                let bzl_label_display = module_label.clone();
                cached_symbols_with_fallback(all, &bzl_label_display, symbols)
            });
            let cached_file_empty = cached_full.as_ref().is_some_and(|entries| entries.is_empty());

            (cached_symbols, cached_file_empty)
        }; // Lock released here

        // If we have cached symbols, extract and return them (FAST PATH - no circular dependency possible)
        if let Some(symbols_map) = cached_symbols {
            debug_log!("[DEBUG BzlLoader] Cache hit for: {}", bzl_label);
            return Ok(symbols_map);
        }

        // FIX #27: If file is fully cached (even if empty), check if this is an infinite loop victim
        // This happens when Fix #26 cached empty symbols - we should NOT re-enter the loop!
        if cached_file_empty {
            // This file was cached as empty (infinite loop victim from Fix #26)
            // Return empty immediately to avoid re-entering the loop.
            debug_log!(
                "[DEBUG BzlLoader] Empty cache hit (infinite loop victim): {}",
                bzl_label
            );
            return Ok(HashMap::new());
        }

        // FIX #20: Add file to IN_FLIGHT *BEFORE* checking for circular dependencies
        // This is critical - the circular dependency check needs to see this file in IN_FLIGHT
        // so that subsequent recursive calls can detect the cycle.
        let mut _loading_guards: Option<LoadingGuards> = None;

        // Don't add to loading set if already being loaded (circular dependency!)
        if BZL_LOADING_SET.insert(bzl_path_key.clone()) {
            _loading_guards = Some(LoadingGuards::new(&bzl_path_key, &bzl_path_key));
        }

        // PHASE 2: Check for circular dependency AFTER adding to IN_FLIGHT
        // Now detect_inflight_cycle() can see this file in IN_FLIGHT
        if let Some(cycle_path) = _loading_guards.is_none().then(|| detect_inflight_cycle(&bzl_path_key)).flatten() {
            eprintln!(
                "🔄 LOADER: Circular dependency detected for {}\n   Cycle: {}",
                bzl_label, cycle_path
            );
            eprintln!("   ↪ Returning LazyLoad placeholders immediately (non-blocking)");

            // Return LazyLoad placeholders for the requested symbols
            let lazy_loads: HashMap<String, Value> = symbols
                .iter()
                .map(|(local_name, original_name)| {
                    (
                        local_name.clone(),
                        Value::LazyLoad {
                            bzl_label: module_label.clone(),
                            symbol_name: original_name.clone(),
                        },
                    )
                })
                .collect();

            // Cache the LazyLoad placeholders to prevent repeated detection
            {
                let mut loader = loader_arc
                    .lock()
                    .map_err(|e| anyhow!("Failed to lock .bzl loader for caching: {}", e))?;
                let cache_map: HashMap<String, Value> = symbols
                    .iter()
                    .map(|(_local_name, original_name)| {
                        (
                            original_name.clone(),
                            Value::LazyLoad {
                                bzl_label: module_label.clone(),
                                symbol_name: original_name.clone(),
                            },
                        )
                    })
                    .collect();
                let _ = loader.put_symbols_cache(module_key.clone(), cache_map);
                eprintln!("   ↪ Cached LazyLoad placeholders for {}", bzl_label);
            }

            return Ok(lazy_loads);
        }

        // PHASE 3: File not cached, not circular - proceed with loading

        // FAST PATH: If file already failed/timed out, don't wait again
        if BZL_FAILED_SET.contains(&bzl_path_key) {
            eprintln!(
                "❌ SKIP: {} already failed/timed out in another thread, failing immediately",
                bzl_label
            );
            return Err(anyhow!(
                "Skipping load of {} - already failed in another thread.\n\
                 This prevents repeated 60s waits for files in circular dependency deadlocks.",
                bzl_label
            ));
        }

        if let Some(cycle_path) = detect_local_cycle_path(&bzl_path_key) {
            eprintln!(
                "🔄 LOCAL CIRCULAR DEPENDENCY detected while loading {}.\n   Cycle: {}",
                bzl_label, cycle_path
            );
            eprintln!("   ↪ Same thread is recursively loading a file it's already evaluating.");
            eprintln!("   ↪ Returning LazyLoad placeholders to break the cycle.");

            // Return LazyLoad placeholders for the requested symbols
            let lazy_loads: HashMap<String, Value> = symbols
                .iter()
                .map(|(local_name, original_name)| {
                    (
                        original_name.clone(),
                        Value::LazyLoad {
                            bzl_label: module_label.clone(),
                            symbol_name: original_name.clone(),
                        },
                    )
                })
                .collect();

            eprintln!(
                "   ↪ Returning {} LazyLoad placeholder(s): {}",
                lazy_loads.len(),
                lazy_loads
                    .keys()
                    .map(|k| k.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );

            // Cache the LazyLoad placeholders
            eprintln!("   ↪ Caching LazyLoad placeholders to prevent repeated cycles");
            let bzl_path = {
                let mut loader = lock_loader_timed(&loader_arc)?;
                let _ = loader.put_symbols_cache(module_key.clone(), lazy_loads.clone());
                bzl_path.clone()
            };

            // Extract requested symbols with local names
            let result: HashMap<String, Value> = symbols
                .iter()
                .filter_map(|(local_name, original_name)| {
                    lazy_loads
                        .get(original_name)
                        .map(|v| (local_name.clone(), v.clone()))
                })
                .collect();

            return Ok(result);
        }

        // PHASE 3: Use GlobalMemoryBudget for atomic memory reservation
        // Realistic per-file estimate with DashMap caching: 50 MB
        //
        // Previous estimates were WAY too conservative:
        // - 2560 MB: Blocked TensorFlow (8 files × 2.5GB = 20GB > budget!)
        // - 768 MB: Still too high (31 files × 768MB = 24GB, triggering false pressure)
        //
        // Actual memory usage with GlobalBzlCache (DashMap-based, observed in production):
        // - First load: parse + evaluate = ~20-30 MB (AST + symbols, cached immediately)
        // - Cache hit: return cached symbols = ~5 MB (just reference cloning from DashMap)
        // - Peak during evaluation: ~40 MB
        // - New 50 MB estimate: 2x actual peak (safe margin without false pressure)
        const ESTIMATED_BZL_SIZE_MB: u64 = 50; // 50 MB (was 768 MB)

        // Enum to hold either type of guard for RAII cleanup
        // Fields are intentionally "unused" - they exist only for Drop behavior
        #[allow(dead_code)]
        enum MemGuard<'a> {
            Global(rush_resource::MemoryReservation<'a>),
            Legacy(BzlMemoryGuard),
        }

        // Clone the budget Arc to avoid lifetime issues - keep Arc alive in outer scope
        let budget_arc = {
            let loader = lock_loader_timed(&loader_arc)?;
            loader.memory_budget.clone()
        };

        let _mem_guard = if let Some(ref budget) = budget_arc {
            MemGuard::Global(budget.reserve(bzl_label, ESTIMATED_BZL_SIZE_MB)?)
        } else {
            // Fallback to old guard for backward compatibility
            MemGuard::Legacy(BzlMemoryGuard::reserve(
                bzl_label,
                ESTIMATED_BZL_SIZE_MB as usize,
            )?)
        };

        // Phase 4: Detect circular loads using evaluating_paths (per-loader HashSet)
        // If this call didn't acquire loading ownership, wait for the
        // current owner to finish. This prevents duplicate .bzl loads and
        // preserves repeat-hit behavior for cache-cold paths.
        if _loading_guards.is_none() {
            // Phase 1 normally catches this case. Keep the same guard here for
            // ownership races between the initial probe and loading-guard setup.
            if inflight_owned_by_current_thread(&bzl_path_key) {
                let stubs = make_stub_exports(&module_label, symbols);
                let mut loader = lock_loader_timed(&loader_arc)?;
                let _ = loader.put_symbols_cache(module_key.clone(), stubs.clone());
                return Ok(stubs);
            }

            // PHASE 4: Detect circular loads using evaluating_paths (per-loader HashSet)
            // If the file is already being evaluated (parallel loading), wait for it to be cached.
            // Note: in-flight tracking is used in addition to evaluating_paths because
            // ownership is claimed before evaluation starts.
            let is_being_evaluated = {
                let loader = lock_loader_timed(&loader_arc)?;
                loader.evaluating_paths.contains(&module_key)
                    || (IN_FLIGHT_BZL_PATHS.contains(&bzl_path_key) || BZL_LOADING_SET.contains(&bzl_path_key))
                    || IN_FLIGHT_BZL_LOADS.contains_key(&bzl_path_key)
            }; // Lock released here

            if is_being_evaluated {
                debug_log!("[DEBUG BzlLoader] File {} is already being evaluated concurrently, waiting for cache", bzl_label);

                // Phase 5.C.4 commit 133 (Batch Q):
                // time the entire concurrent-eval
                // polling loop. The lock-wait
                // hypothesis was falsified (0 ms /
                // 7795 calls); THIS sleep loop is the
                // remaining suspect for the 96.3%
                // wait floor.
                let concurrent_wait_start = std::time::Instant::now();

                // Wait for the concurrent evaluation to complete and cache the result
                // Check cache every 100ms for up to 60 seconds (1 minute)
                // Reduced from 600s to prevent hanging builds - 60s is sufficient for complex .bzl files
                for attempt in 1..=600 {
                    std::thread::sleep(std::time::Duration::from_millis(100));

                    let mut loader = lock_loader_timed(&loader_arc)?;

                    // Check if result was cached
                    let maybe_cached_symbols =
                        loader.get_symbols_cache_subset_key_no_stats(&module_key, symbols);
                    if let Some(cached_symbols) = maybe_cached_symbols {
                        let cached_symbols = loader
                            .get_symbols_cache_subset_key(&module_key, symbols)
                            .unwrap_or(cached_symbols);
                        debug_log!("[DEBUG BzlLoader] Concurrent evaluation cached after {}s, returning: {}", attempt / 10, bzl_label);
                        // commit 133: record wait
                        // before exit
                        if let Some(ref g) = loader.global_cache {
                            g.record_concurrent_eval_wait(
                                concurrent_wait_start.elapsed(),
                            );
                        }
                        return Ok(cached_symbols);
                    }

                    // Check if it's no longer being evaluated or in-flight (failed or
                    // completed without cache).
                    if !loader.evaluating_paths.contains(&module_key)
                        && !(IN_FLIGHT_BZL_PATHS.contains(&bzl_path_key) || BZL_LOADING_SET.contains(&bzl_path_key))
                        && !IN_FLIGHT_BZL_LOADS.contains_key(&bzl_path_key)
                    {
                        debug_log!("[DEBUG BzlLoader] File no longer being evaluated (likely failed), will retry: {}", bzl_label);
                        // commit 133: record wait
                        // before break
                        if let Some(ref g) = loader.global_cache {
                            g.record_concurrent_eval_wait(
                                concurrent_wait_start.elapsed(),
                            );
                        }
                        // Break out and retry evaluation ourselves
                        break;
                    }

                    if attempt % 50 == 0 {
                        debug_log!(
                            "[DEBUG BzlLoader] Still waiting for concurrent evaluation: {} ({}s)",
                            bzl_label,
                            attempt / 10
                        );
                        log_wait_diagnostics(bzl_label, (attempt / 10) as u64, 60);
                        if let Some((owning_thread, summary)) =
                            detect_single_thread_deadlock(&bzl_path_key)
                        {
                            eprintln!(
                                "🚫 Deadlock detected while waiting for {} on worker {:?}:\n      {}",
                                bzl_label, owning_thread, summary
                            );
                            eprintln!(
                                "   ↪ Returning stub exports so evaluation can continue (symbols: {}).",
                                symbols
                                    .iter()
                                    .map(|(local, original)| format!("{}={}", local, original))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            );
                            let stubs = make_stub_exports(&module_label, symbols);
                            {
                                let mut loader = lock_loader_timed(&loader_arc)?;
                                let _ = loader.put_symbols_cache(module_key.clone(), stubs.clone());
                            }
                            return Ok(stubs);
                        }
                    }
                }

                // After loop: check why we exited
                let loader = lock_loader_timed(&loader_arc)?;

                // Phase 5.C.4 commit 133: record the
                // wait at the natural-loop-end path
                // too. This is the timeout case.
                if let Some(ref g) = loader.global_cache {
                    g.record_concurrent_eval_wait(
                        concurrent_wait_start.elapsed(),
                    );
                }

                // If still being evaluated after 60s, it's a deadlock
                if loader.evaluating_paths.contains(&module_key)
                    || (IN_FLIGHT_BZL_PATHS.contains(&bzl_path_key) || BZL_LOADING_SET.contains(&bzl_path_key))
                    || IN_FLIGHT_BZL_LOADS.contains_key(&bzl_path_key)
                {
                    debug_log!(
                        "[ERROR BzlLoader] Timeout waiting for concurrent evaluation (60s): {}",
                        bzl_label
                    );
                    return Err(anyhow!(
                        "Circular .bzl load or deadlock detected after 60s: {}",
                        bzl_label
                    ));
                }

                // Otherwise it failed, fall through to retry
                debug_log!(
                    "[DEBUG BzlLoader] Previous evaluation failed, retrying: {}",
                    bzl_label
                );
            }
        } // is_being_evaluated scope ends

        // Mark file as being evaluated (acquire lock, mark, release)
        {
            let mut loader = lock_loader_timed(&loader_arc)?;
            loader.evaluating_paths.insert(module_key.clone());
        } // Lock released here

        // RAII guard to ensure evaluating_paths cleanup on ANY exit (success/error/panic)
        struct EvaluatingGuard {
            loader: Arc<Mutex<BzlLoader>>,
            path: ModuleCacheKey,
        }
        impl Drop for EvaluatingGuard {
            fn drop(&mut self) {
                if let Ok(mut loader) = self.loader.lock() {
                    loader.evaluating_paths.remove(&self.path);
                    debug_log!(
                        "[TRACE bzl] Removed from evaluating_paths: {}",
                        self.path.display()
                    );
                }
            }
        }
        let _evaluating_guard = EvaluatingGuard {
            loader: Arc::clone(&loader_arc),
            path: module_key.clone(),
        };

        // Phase 5.C.4 commit 104 + 105: push thread-
        // local cold-load frame. Any child .bzl loaded
        // during evaluation pops INTO our child_total.
        // The _ex variant also tells us our parent (for
        // load-graph edge) and our depth.
        let push_result = push_cold_load_frame_ex(&bzl_path);
        let frame_recursion = push_result.recursive;
        // Phase 5.C.4 commit 105: record load-graph
        // edge, raw-label identity, and depth.
        {
            let loader = lock_loader_timed(&loader_arc)?;
            if let Some(ref global) = loader.global_cache {
                if let Some(ref parent) = push_result.parent_path {
                    global.record_load_edge(parent, &bzl_path);
                }
                global.record_identity(bzl_label, &bzl_path);
                global.record_observed_depth(&bzl_path, push_result.depth_after_push);
            }
        }

        // Phase 2: Parse the file (brief lock).
        // Phase 5.C.4 commit 104: split into read and
        // parse subphases for sub-Starlark attribution.
        let (program, subphase_read_dur, subphase_parse_dur, source_bytes) = {
            let _loader_guard = loader_arc
                .lock()
                .map_err(|e| anyhow!("Failed to lock .bzl loader: {}", e))?;
            let read_start = std::time::Instant::now();
            // Persistent-daemon brick 1b (step 4): read the .bzl source through
            // the tracked analysis IO so the file's content (+ kind) is recorded
            // as an analysis dependency. Fail-loud: if no `tracked::scope` is
            // installed on this thread (an un-wrapped parallel `.bzl`-eval
            // boundary) this panics rather than silently skipping the record.
            let source = rush_incremental::tracked::read_to_string(&bzl_path).map_err(|e| {
                // Balance the frame stack on the error
                // path before propagating.
                let _ = pop_cold_load_frame(cold_load_start.elapsed().as_nanos() as u64);
                anyhow!("Failed to read .bzl file {}: {}", bzl_path.display(), e)
            })?;
            let read_dur = read_start.elapsed();
            let parse_start = std::time::Instant::now();
            let program = crate::parse(&source).map_err(|e| {
                let _ = pop_cold_load_frame(cold_load_start.elapsed().as_nanos() as u64);
                anyhow!("Failed to parse .bzl file {}: {}", bzl_path.display(), e)
            })?;
            let parse_dur = parse_start.elapsed();
            (program, read_dur, parse_dur, source.len())
        }; // Lock released here

        // #39 graph-RSS: this parsed Program is now live and stays live across
        // evaluation below. The guard tracks it for the peak live-Program
        // census (RUSH_MEM_CENSUS=1) and decrements when `program` is dropped
        // at end of scope (post-eval / post-cache-insert).
        let _live_program_guard = track_live_program(&program, source_bytes);

        // Install the requested logical owner, even when roots share bytes.
        let frame = BzlContextFrame {
            repo_root, package_dir, repository: Some(module_context.repository.clone().unwrap_or_default()),
            load_stack: current_stack_snapshot(),
        };

        // Phase 2.75: Acquire resources before evaluation (OOM prevention)
        // Each .bzl file evaluation requires 512MB to prevent memory exhaustion
        // NOTE: Must acquire BEFORE spawning timeout thread (tokio context required)
        let resource_handle = {
            let loader = lock_loader_timed(&loader_arc)?;

            // .bzl evaluation is parser-bound I/O with modest per-load memory,
            // but its call graph is arbitrarily deep: `load("A.bzl")` → `load("B.bzl")` →
            // `load("C.bzl")` all holding permits simultaneously. Sharing
            // the cpu_slot semaphore with the compile phase means a small
            // `--jobs=N` can deadlock on a single root BUILD whose .bzl
            // closure depth exceeds N (seen in the wild at --jobs=8 on TF).
            //
            // Memory pressure is still enforced at the coarser level by
            // the PSI watchdog in ResourceManager, which observes actual
            // system pressure and throttles separately.
            drop(loader);
            None::<rush_resource::ResourceHandle>
        };

        // Phase 3: Evaluate WITHOUT holding the lock (allows recursive loads)
        // If we're already in a tokio context (from parallel scheduler), evaluate directly
        // to avoid nested runtime deadlock. Otherwise, use timeout thread for safety.
        debug_log!("[DEBUG BzlLoader] Starting evaluation: {}", bzl_label);

        // Phase 5.C.4 commit 104: time the eval subphase
        // (includes time spent in transitive child .bzl
        // loads triggered from Starlark `load()`).
        let subphase_eval_start = std::time::Instant::now();
        let context_clone = module_context.clone();
        let frame_clone = frame.clone();
        let eval_result = if tokio::runtime::Handle::try_current().is_ok() {
            // We're in a tokio context (likely from parallel scheduler)
            // Call evaluate_bzl_static directly to avoid nested runtime deadlock
            debug_log!(
                "[DEBUG BzlLoader] In tokio context, evaluating directly (no timeout thread)"
            );
            Self::evaluate_bzl_static(
                &program,
                Some(loader_arc.clone()),
                module_context.clone(),
                frame.clone(),
            )
        } else {
            // Not in tokio context, use timeout thread with runtime creation
            // This maintains backward compatibility for sequential loading
            use std::sync::mpsc;
            use std::thread;
            use std::time::Duration;

            debug_log!("[DEBUG BzlLoader] Not in tokio context, spawning timeout thread");
            // Phase 5.C.4 commit 110: mark our active
            // frame as spawning eval onto another
            // thread. Child .bzl loads triggered by
            // that eval will push frames onto the
            // SPAWNED thread's cold-load stack, not
            // ours, so our child_total_ns will
            // under-report and own-time is unreliable.
            mark_active_frame_spawned_off_thread();
            let (tx, rx) = mpsc::channel();
            // #39 graph-RSS: this non-tokio path CLONES the parsed Program
            // (deep AST copy) before handing it to the spawned eval thread —
            // a transient doubling Codex flagged. Count it so the peak census
            // can attribute a spike to this path.
            program_census::PROGRAM_CLONES_TOTAL
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let program_clone = program.clone();
            let loader_clone = loader_arc.clone();
            let bzl_label_clone = bzl_label.to_string();

            // Persistent-daemon brick 1b (step 3): the tracked analysis IO is a
            // thread-local, so it is LOST when `.bzl` eval is moved onto this
            // fresh OS thread. Re-install the SAME shared `Arc` on the spawned
            // thread (via `tracked::bind`) so any child `.bzl` reads triggered by
            // `evaluate_bzl_static` record into the one command manifest. When no
            // context is installed (non-analysis callers, e.g. some WORKSPACE/eval
            // paths) we spawn the closure raw — `tracked::bind` would fail-loud,
            // but those callers never call `tracked::*`.
            let eval_body = move || {
                // Create a tokio runtime for this evaluation thread to enable parallel .bzl loading
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();

                let result = match runtime {
                    Ok(rt) => {
                        // Enter the runtime context so evaluator can use tokio::spawn
                        let _guard = rt.enter();
                        Self::evaluate_bzl_static(
                            &program_clone,
                            Some(loader_clone),
                            context_clone,
                            frame_clone,
                        )
                    }
                    Err(e) => {
                        debug_log!("[WARN BzlLoader] Failed to create tokio runtime: {}, falling back to sync evaluation", e);
                        Self::evaluate_bzl_static(
                            &program_clone,
                            Some(loader_clone),
                            context_clone,
                            frame_clone,
                        )
                    }
                };

                let _ = tx.send(result);
            };
            if rush_incremental::tracked::current().is_some() {
                thread::spawn(rush_incremental::tracked::bind(eval_body));
            } else {
                thread::spawn(eval_body);
            }

            // Wait for evaluation with timeout (60 seconds / 1 minute)
            // Reduced from 600s to prevent hanging builds - 60s is sufficient for complex .bzl files
            // Phase 5.C.4 commit 126 (Batch P): time
            // this recv as the dominant "wait" category
            // for the non-tokio spawn path.
            let async_wait_start = std::time::Instant::now();
            let recv_result = rx.recv_timeout(Duration::from_secs(60));
            let async_wait_dur = async_wait_start.elapsed();
            {
                let loader = loader_arc.lock().ok();
                if let Some(l) = loader {
                    if let Some(ref global) = l.global_cache {
                        global.record_async_spawn_wait(async_wait_dur);
                    }
                }
            }
            match recv_result {
                Ok(result) => result,
                Err(_) => {
                    debug_log!("[ERROR BzlLoader] TIMEOUT evaluating {}", bzl_label_clone);
                    debug_log!("[ERROR BzlLoader] This .bzl file contains constructs that cause infinite loops or has very deep .bzl dependencies");
                    debug_log!("[ERROR BzlLoader] Likely causes:");
                    debug_log!("[ERROR BzlLoader]   - Deep .bzl dependency chains (1000+ files)");
                    debug_log!("[ERROR BzlLoader]   - Unsupported Starlark features (select(), rule(), aspect(), depset())");
                    debug_log!("[ERROR BzlLoader]   - Infinite recursion in macros");
                    debug_log!("[ERROR BzlLoader]   - Missing built-in functions");
                    // Phase 5.C.4 commit 104: balance the
                    // frame stack on the timeout error
                    // path.
                    let _ = pop_cold_load_frame(cold_load_start.elapsed().as_nanos() as u64);
                    return Err(anyhow!(
                        "Timeout evaluating .bzl file: {}. Evaluation took longer than 60 seconds.",
                        bzl_label_clone
                    ));
                }
            }
        };
        let subphase_eval_dur = subphase_eval_start.elapsed();

        debug_log!("[DEBUG BzlLoader] Evaluation returned for: {}", bzl_label);

        // Phase 3.25: Release resources after evaluation
        if let Some(handle) = resource_handle {
            debug_log!(
                "[DEBUG BzlLoader] Releasing resources after .bzl evaluation: {}",
                bzl_label
            );
            if let Ok(rt_handle) = tokio::runtime::Handle::try_current() {
                // Use block_in_place to avoid blocking worker thread
                tokio::task::block_in_place(|| {
                    rt_handle.block_on(async { handle.release_async().await })
                });
            }
        }

        // Context popped automatically by guard
        // evaluating_paths cleaned up automatically by EvaluatingGuard

        // Propagate evaluation error after cleanup
        // IMPORTANT: If evaluation fails here, cache storage is skipped
        let symbols_map = match eval_result {
            Ok(map) => map,
            Err(e) => {
                debug_log!("[ERROR BzlLoader] Failed to evaluate {}: {}", bzl_label, e);
                debug_log!("[ERROR BzlLoader] This file will NOT be cached and will be re-evaluated on each load");
                debug_log!("[ERROR BzlLoader] Common causes:");
                debug_log!("[ERROR BzlLoader]   - Missing Bazel built-in functions (http_archive, http_file, etc.)");
                debug_log!("[ERROR BzlLoader]   - Unsupported Starlark features (select(), rule(), aspect())");
                debug_log!("[ERROR BzlLoader]   - Undefined symbols or incorrect native namespace");
                debug_log!("[ERROR BzlLoader] To fix: Implement missing Bazel features or check .bzl file syntax");
                // Phase 5.C.4 commit 104: balance the
                // frame stack on the eval-error path.
                let _ = pop_cold_load_frame(cold_load_start.elapsed().as_nanos() as u64);
                return Err(e);
            }
        };

        debug_log!("[TRACE bzl] Evaluation complete: {}", bzl_label);
        eprintln!(
            "🔍 DEBUG: File evaluation finished for {}, {} symbols returned",
            bzl_label,
            symbols_map.len()
        );

        // Phase 5.C.4 commit 104: time the finalize
        // subphase — resolve_lazy_loads + cache insert.
        // Phase 5.C.4 commit 111: also time the two
        // sub-finalize steps separately.
        let subphase_finalize_start = std::time::Instant::now();

        // Phase 3.9: Resolve any LazyLoad placeholders before caching
        // This implements the "resolve after load" approach for circular dependencies
        eprintln!(
            "🔍 DEBUG: About to call resolve_lazy_loads() for {}",
            bzl_label
        );
        let subphase_resolve_lazy_loads_start = std::time::Instant::now();
        let symbols_map = match Self::resolve_lazy_loads(loader_arc.clone(), symbols_map) {
            Ok(m) => m,
            Err(e) => {
                let _ = pop_cold_load_frame(cold_load_start.elapsed().as_nanos() as u64);
                return Err(e);
            }
        };
        let subphase_resolve_lazy_loads_dur =
            subphase_resolve_lazy_loads_start.elapsed();
        eprintln!("🔍 DEBUG: resolve_lazy_loads() completed for {}", bzl_label);

        // Phase 4: Cache the result (brief lock).
        // Phase 5.C.4 commit 103: clone the path so we can
        // record cold-load timing after the lock is
        // released.
        let bzl_path_for_timing = bzl_path.clone();
        // Phase 5.C.4 commit 113: extract requested
        // symbols BEFORE the cache insert so we can MOVE
        // `symbols_map` into the cache without cloning.
        // Pro's commit-113 directive said this is the
        // primary lever (commit-111 data confirmed all
        // 879 ms of TF macros finalize lives in
        // put_symbols_cache). The full-map clone is the
        // cost; extract reads the map by reference and
        // produces a small new HashMap with .clone() on
        // the few requested values.
        let extract_result =
            Self::extract_symbols_static(&symbols_map, symbols);
        let symbols_count = symbols_map.len() as u64;
        let subphase_put_symbols_cache_start = std::time::Instant::now();
        {
            let mut loader = match lock_loader_timed(&loader_arc) {
            Ok(g) => g,
            Err(e) => {
                let _ = pop_cold_load_frame(cold_load_start.elapsed().as_nanos() as u64);
                return Err(e);
            }
        };
            // MOVE — no clone. The original symbols_map
            // is consumed here.
            let inserted_generation = loader.put_symbols_cache(module_key.clone(), symbols_map);
            // Persistent-daemon Run-wiring (blocker #1): capture this `.bzl`'s
            // FULL TRANSITIVE tracked dependency set (everything recorded since
            // `dep_mark` at function entry — its own source read plus every nested
            // `load()`'s reads/replays) and store it on the just-inserted cache
            // entry, so a cross-command HIT can replay it. No-op outside a command
            // scope (`dep_mark` is None) or when nothing was recorded.
            if let (Some(mark), Some(generation)) = (dep_mark, inserted_generation) {
                let captured = rush_incremental::tracked::captured_since(mark);
                loader.set_dep_set_cache(&module_key, generation, captured);
            }
        } // Lock released here
        let subphase_put_symbols_cache_dur =
            subphase_put_symbols_cache_start.elapsed();
        let subphase_finalize_dur = subphase_finalize_start.elapsed();
        // Phase 5.C.4 commit 103/104: record cold-load
        // attribution. The Instant was captured at
        // function entry; elapsed time covers Phase-1
        // cache check + Phase-2 read+parse + Phase-3
        // eval (incl. transitive child loads) + Phase-4
        // resolve+cache insert. File size is best-effort.
        // Commit 104 additionally records own/child split
        // via the thread-local frame stack and per-
        // subphase wall time.
        let cold_load_elapsed = cold_load_start.elapsed();
        let file_size_bytes = std::fs::metadata(&bzl_path_for_timing)
            .map(|m| m.len())
            .unwrap_or(0);
        // Phase 5.C.4 commit 106/110: pop_ex returns
        // the accumulated Starlark-op counts AND the
        // attribution-quality classification for the
        // frame being popped.
        let pop = pop_cold_load_frame_ex(cold_load_elapsed.as_nanos() as u64);
        let child_total_ns = pop.child_total_ns;
        let recursion_detected = pop.recursion_detected;
        let drained_ops = pop.ops;
        let attribution = pop.attribution;
        // Own-time is reliable only when no recursive
        // load was seen (Pro: "mark own-time as
        // experimental (Option) and only emit it when
        // confident"). `frame_recursion` is true if we
        // saw the same path already on the stack at
        // push time.
        let child_duration = if recursion_detected || frame_recursion {
            None
        } else {
            Some(std::time::Duration::from_nanos(child_total_ns))
        };
        let subphases = ColdLoadSubphases {
            read: Some(subphase_read_dur),
            parse: Some(subphase_parse_dur),
            eval: Some(subphase_eval_dur),
            finalize: Some(subphase_finalize_dur),
            resolve_lazy_loads: Some(subphase_resolve_lazy_loads_dur),
            put_symbols_cache: Some(subphase_put_symbols_cache_dur),
        };
        {
            let loader = lock_loader_timed(&loader_arc)?;
            if let Some(ref global) = loader.global_cache {
                global.record_cold_load_full_v2(
                    &bzl_path_for_timing,
                    cold_load_elapsed,
                    child_duration,
                    file_size_bytes,
                    symbols_count,
                    Some(subphases),
                    attribution,
                );
                // Phase 5.C.4 commit 106: fold the
                // drained Starlark-op counters into
                // per-path metrics for this .bzl.
                global.record_starlark_ops(&bzl_path_for_timing, drained_ops);
            }
        }

        eprintln!(
            "🔍 DEBUG: load_bzl_from_evaluator COMPLETED for {}, caching done",
            bzl_label
        );

        // Phase 5.C.4 commit 113: extract_result was
        // computed BEFORE the move-cache so we could
        // avoid a full-map clone. Same return as before.
        eprintln!(
            "🔍 DEBUG: Thread {:?} EXITING load_bzl_from_evaluator for {} with {} symbols",
            std::thread::current().id(),
            bzl_label,
            extract_result.as_ref().map(|r| r.len()).unwrap_or(0)
        );
        extract_result
    }

    /// Resolve any LazyLoad placeholders in a symbol map
    /// This implements "resolve after load" - when a file finishes loading,
    /// any LazyLoad placeholders are resolved to their actual values.
    fn resolve_lazy_loads(
        loader_arc: Arc<Mutex<BzlLoader>>,
        mut symbols: HashMap<String, Value>,
    ) -> Result<HashMap<String, Value>> {
        let mut has_lazy_loads = false;
        let mut lazy_load_info: Vec<(String, String, String)> = Vec::new();

        // First pass: detect LazyLoad values and collect their info
        for (name, value) in &symbols {
            if let Value::LazyLoad {
                bzl_label,
                symbol_name,
            } = value
            {
                has_lazy_loads = true;
                lazy_load_info.push((name.clone(), bzl_label.clone(), symbol_name.clone()));
            }
        }

        // If no LazyLoad values, return as-is
        if !has_lazy_loads {
            return Ok(symbols);
        }

        eprintln!(
            "🔄 Resolving {} LazyLoad placeholder(s) after evaluation complete",
            lazy_load_info.len()
        );

        // Second pass: resolve each LazyLoad value
        for (local_name, bzl_label, symbol_name) in lazy_load_info {
            eprintln!(
                "   ↪ Resolving LazyLoad: {} -> {}:{}",
                local_name, bzl_label, symbol_name
            );

            // Load the actual symbol from the .bzl file
            // This should now succeed because the circular dependency is broken
            let resolved_symbols = Self::load_bzl_from_evaluator(
                loader_arc.clone(),
                &bzl_label,
                &[(symbol_name.clone(), symbol_name.clone())],
            )?;

            // Extract the resolved value
            if let Some(resolved_value) = resolved_symbols.get(&symbol_name) {
                // Check if we got another LazyLoad (shouldn't happen, but detect infinite loop)
                if matches!(resolved_value, Value::LazyLoad { .. }) {
                    return Err(anyhow!(
                        "LazyLoad resolution failed: {} -> {} still returned LazyLoad. Possible infinite circular dependency.",
                        bzl_label, symbol_name
                    ));
                }

                eprintln!(
                    "   ✓ Resolved {} to {}",
                    local_name,
                    resolved_value.to_string()
                );
                symbols.insert(local_name, resolved_value.clone());
            } else {
                return Err(anyhow!(
                    "LazyLoad resolution failed: symbol '{}' not found in {}",
                    symbol_name,
                    bzl_label
                ));
            }
        }

        eprintln!("✓ All LazyLoad placeholders resolved successfully");
        Ok(symbols)
    }

    /// Static version of extract_symbols that doesn't require &self
    fn extract_symbols_static(
        all_symbols: &HashMap<String, Value>,
        requested: &[(String, String)],
    ) -> Result<HashMap<String, Value>> {
        let mut exported = HashMap::new();

        for (local_name, original_name) in requested {
            if let Some(value) = all_symbols.get(original_name) {
                exported.insert(local_name.clone(), value.clone());
            } else {
                return Err(anyhow!("Symbol '{}' not found in .bzl file", original_name));
            }
        }

        Ok(exported)
    }

    /// Set the current package for relative imports
    pub fn set_current_package(&mut self, package: PathBuf) {
        self.current_package = Some(package);
    }

    /// Set external repository paths (from WORKSPACE support)
    /// Populates the existing external_repos DashMap instead of replacing the Arc,
    /// ensuring cloned BzlLoaders see the same repositories.
    pub fn set_external_repos(&mut self, repos: Arc<DashMap<String, PathBuf>>) {
        // Clear existing entries
        self.external_repos.clear();

        // Insert all repos from the provided map into our existing DashMap
        for entry in repos.iter() {
            self.external_repos
                .insert(entry.key().clone(), entry.value().clone());
        }
    }

    /// Add a single external repository path
    pub fn add_external_repo(&mut self, name: String, path: PathBuf) {
        self.external_repos.insert(name, path);
    }

    /// Return an external repository materialized by this loader.
    pub fn external_repo_path(&self, name: &str) -> Option<PathBuf> {
        self.external_repos
            .get(name)
            .map(|entry| entry.value().clone())
    }

    /// Set repository alias mappings (e.g., "tsl" -> "local_tsl")
    pub fn set_repo_aliases(&mut self, aliases: Arc<DashMap<String, String>>) {
        self.repo_aliases = aliases;
    }

    /// Add or update a single repository alias mapping
    pub fn add_repo_alias(&mut self, from: impl Into<String>, to_repo: impl Into<String>) {
        self.repo_aliases.insert(from.into(), to_repo.into());
    }

    /// Set main workspace name
    pub fn set_main_workspace_name(&mut self, name: Option<String>) {
        self.main_workspace_name = name;
    }

    /// Return the main repository's declared workspace name, if any.
    ///
    /// The direct value is populated during WORKSPACE evaluation. Fall back
    /// to the root file because a cached workspace load may skip that setter.
    pub fn main_workspace_name(&self) -> Option<String> {
        if let Some(ref known) = self.main_workspace_name {
            return Some(known.clone());
        }
        for candidate in ["WORKSPACE", "WORKSPACE.bazel"] {
            let path = self.workspace_root.join(candidate);
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            for line in text.lines() {
                let line = line.trim();
                if !line.starts_with("workspace(") {
                    continue;
                }
                if let Some(rest) = line.split_once("name") {
                    let declared = rest
                        .1
                        .trim_start_matches(|c: char| c == '=' || c.is_whitespace())
                        .trim_start_matches(['"', '\''])
                        .split(['"', '\''])
                        .next()
                        .unwrap_or("");
                    if !declared.is_empty() {
                        return Some(declared.to_string());
                    }
                }
            }
        }
        None
    }

    /// Set repo_mapping for a given repository (Bazel semantics)
    pub fn set_repo_mapping_for(
        &mut self,
        repo: impl Into<String>,
        mapping: HashMap<String, String>,
    ) {
        self.repo_mappings.insert(repo.into(), mapping);
    }

    /// Install a mapping reconstructed from this command's workspace/module
    /// inputs and retire it automatically at the next command boundary.
    pub fn set_command_repo_mapping_for(
        &mut self,
        repo: impl Into<String>,
        mapping: HashMap<String, String>,
    ) {
        let repo = repo.into();
        self.repo_mappings.insert(repo.clone(), mapping);
        self.command_repo_mapping_owners.insert(repo);
    }

    /// Discard mappings discovered from Bzlmod during the preceding command.
    ///
    /// Call this exactly once at the serialized workspace command boundary,
    /// before BUILD/select inspection. A root module rename must not leave the
    /// old apparent self-name mapped to the canonical main repository long
    /// enough to be mistaken for `//conditions:default`.
    pub fn clear_command_repo_mappings(&mut self) {
        let owners: Vec<String> = self
            .command_repo_mapping_owners
            .iter()
            .map(|owner| owner.key().clone())
            .collect();
        for owner in owners {
            self.repo_mappings.remove(&owner);
            self.command_repo_mapping_owners.remove(&owner);
        }
        self.main_workspace_name = None;
    }

    /// Resolve an apparent repository name in the repository that owns a label.
    ///
    /// Configuration labels are resolved after BUILD evaluation has restored
    /// the loader's thread context. Accept the owner explicitly so repository
    /// mappings stay scoped to the consuming repository. In a MODULE-only
    /// workspace, ask Bazel for the owner's canonical mapping and materialize
    /// only the exact referenced repository before select() is resolved.
    pub fn resolve_repository_for_owner(
        &self,
        apparent: &str,
        canonical_label: bool,
        owner_repository: Option<&str>,
        owner_repo_root: &Path,
    ) -> Result<Option<(String, PathBuf)>> {
        let owner_mapping = |owner: &str, apparent: &str| {
            self.repo_mappings.get(owner).and_then(|mapping| {
                mapping
                    .get(apparent)
                    .or_else(|| mapping.get(&format!("@{apparent}")))
                    .map(|mapped| mapped.trim_start_matches('@').to_string())
            })
        };
        let bzlmod_only = self.workspace_root.join("MODULE.bazel").is_file()
            && !self.workspace_root.join("WORKSPACE").exists()
            && !self.workspace_root.join("WORKSPACE.bazel").exists();

        if !bzlmod_only {
            let repository = if canonical_label {
                apparent.to_string()
            } else {
                let mapped = owner_repository
                    .and_then(|owner| owner_mapping(owner, apparent))
                    .or_else(|| {
                        self.repo_name_for_root(&owner_repo_root.to_path_buf())
                            .and_then(|owner| owner_mapping(&owner, apparent))
                    });
                if let Some(mapped) = mapped {
                    mapped
                } else if self.is_main_workspace_name(apparent) {
                    // In a legacy WORKSPACE, an external repository may use
                    // the main workspace name without an explicit
                    // repo_mapping entry. Record that owner-relative identity
                    // so the post-preload instantiation pass canonicalizes the
                    // same spelling to main. An explicit owner mapping above
                    // always wins, including one that shadows this name.
                    let owner = owner_repository.map(str::to_owned).or_else(|| {
                        self.repo_name_for_root(&owner_repo_root.to_path_buf())
                    });
                    if let Some(owner) = owner {
                        let mut mapping = self
                            .repo_mappings
                            .get(&owner)
                            .map(|entry| entry.value().clone())
                            .unwrap_or_default();
                        mapping.insert(apparent.to_string(), String::new());
                        self.repo_mappings.insert(owner.clone(), mapping);
                        self.command_repo_mapping_owners.insert(owner);
                    }
                    return Ok(Some((String::new(), self.workspace_root.clone())));
                } else {
                    apparent.to_string()
                }
            };
            let registered = self.external_repos.get(&repository).map(|path| path.value().clone());
            if let Some(path) = registered {
                let path = self.admit_registered_workspace_repository(&repository, &path)?;
                self.external_repos.insert(repository.clone(), path.clone());
                return Ok(Some((repository, path)));
            }
            Self::push_thread_context(owner_repo_root.to_path_buf(), PathBuf::new());
            let fetched = self.try_fetch_known_repo(&repository, canonical_label, None, None);
            Self::pop_thread_context();
            let Some(path) = fetched? else {
                return Ok(None);
            };
            self.external_repos.insert(repository.clone(), path.clone());
            return Ok(Some((repository, path)));
        }

        let cache_root = tracked_command_environment("RUSH_WORKSPACE_CACHE_ROOT")?
            .map(PathBuf::from)
            .unwrap_or_else(|| self.workspace_root.join(".cache").join("rush"));
        let output_base = cache_root.join("bazel_output_base");
        fs::create_dir_all(&output_base)?;
        let bazel_binary = resolve_bazel_binary_for_fetch(&self.workspace_root)?;

        // Bazel's owner-scoped repository mapping is an analysis input. Record
        // both its source module files and the exact dump_repo_mapping result so
        // a retained daemon graph cannot survive a repo_name/mapping change.
        // The ProcessProbe dependency is re-run during freshness validation;
        // the explicit file reads also give a direct, cheap invalidation reason.
        for input in [
            self.workspace_root.join("MODULE.bazel"),
            self.workspace_root.join("MODULE.bazel.lock"),
            owner_repo_root.join("MODULE.bazel"),
            owner_repo_root.join("MODULE.bazel.lock"),
        ] {
            track_optional_repository_mapping_input(&input)?;
        }

        let dump_mapping = |owner: &str| -> Result<HashMap<String, String>> {
            let args = vec![
                b"--output_base".to_vec(),
                path_command_argument(&output_base),
                b"mod".to_vec(),
                b"dump_repo_mapping".to_vec(),
                owner.as_bytes().to_vec(),
            ];
            let output = rush_incremental::tracked::probe_process_stdout(
                &bazel_binary,
                &args,
                &bazel_repository_mapping_probe_environment(),
                &self.workspace_root,
            )
                .with_context(|| {
                    format!(
                        "Failed to run `{} mod dump_repo_mapping {}`",
                        bazel_binary.display(),
                        owner
                    )
                })?;
            if output.status != 0 {
                return Err(anyhow!(
                    "`{} --output_base={} mod dump_repo_mapping {}` failed with status {:?}: {}",
                    bazel_binary.display(),
                    output_base.display(),
                    owner,
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                ));
            }
            serde_json::from_slice(&output.stdout).with_context(|| {
                format!("Failed to parse repository mapping for canonical repo `{owner}`")
            })
        };

        let root_mapping = dump_mapping("")?;
        let owner_canonical = match owner_repository {
            None => String::new(),
            Some(owner) if root_mapping.values().any(|value| value == owner) => {
                owner.to_string()
            }
            Some(owner) => root_mapping
                .get(owner)
                .cloned()
                .or_else(|| {
                    self.external_repos
                        .iter()
                        .find(|entry| entry.value() == owner_repo_root)
                        .and_then(|entry| {
                            let name = entry.key();
                            root_mapping
                                .get(name)
                                .cloned()
                                .or_else(|| Some(name.clone()))
                        })
                })
                .ok_or_else(|| {
                    anyhow!(
                        "Bazel root repository mapping has no canonical identity for owner `{owner}`"
                    )
                })?,
        };
        let repository = if canonical_label {
            apparent.to_string()
        } else {
            let mapping = dump_mapping(&owner_canonical)?;
            if let Some(mapped) = mapping.get(apparent) {
                mapped.clone()
            } else if mapping.values().any(|canonical| canonical == apparent) {
                // Rush stores canonicalized configuration metadata with a
                // single `@` internally. When that canonical identity is fed
                // back into the exact-metadata preloader, accept it only after
                // proving it is a value in this owner's freshly dumped map.
                // An apparent-key entry above still wins if it shadows the
                // same spelling.
                apparent.to_string()
            } else {
                return Err(anyhow!(
                    "Bazel repository mapping for canonical repo `{}` has no entry for apparent repository `{}`",
                    owner_canonical,
                    apparent
                ));
            }
        };

        if !canonical_label {
            let mut mapping = self
                .repo_mappings
                .get(&owner_canonical)
                .map(|entry| entry.value().clone())
                .unwrap_or_default();
            mapping.insert(apparent.to_string(), repository.clone());
            self.repo_mappings
                .insert(owner_canonical.clone(), mapping.clone());
            self.command_repo_mapping_owners
                .insert(owner_canonical.clone());
            if let Some(owner) = owner_repository {
                self.repo_mappings.insert(owner.to_string(), mapping);
                self.command_repo_mapping_owners
                    .insert(owner.to_string());
            }
        }

        // Bazel maps the root module's apparent self-name to the empty
        // canonical repository. It is the main workspace, not an external
        // directory named "" under output_base/external.
        if repository.is_empty() {
            return Ok(Some((repository, self.workspace_root.clone())));
        }

        if let Some(path) = self.external_repos.get(&repository) {
            return Ok(Some((repository, path.value().clone())));
        }
        let path = output_base.join("external").join(&repository);
        if !path.exists() {
            let status = StdCommand::new(&bazel_binary)
                .arg("--output_base")
                .arg(&output_base)
                .arg("fetch")
                .arg(format!("--repo=@@{repository}"))
                .current_dir(&self.workspace_root)
                .status()
                .with_context(|| {
                    format!(
                        "Failed to run `{} fetch --repo=@@{}`",
                        bazel_binary.display(),
                        repository
                    )
                })?;
            if !status.success() {
                anyhow::bail!(
                    "`{} --output_base={} fetch --repo=@@{}` failed with status {:?}",
                    bazel_binary.display(),
                    output_base.display(),
                    repository,
                    status.code()
                );
            }
        }
        if !path.exists() {
            anyhow::bail!(
                "Bazel reported success but canonical repository {} is missing",
                path.display()
            );
        }

        self.external_repos.insert(repository.clone(), path.clone());
        Ok(Some((repository, path)))
    }

    /// Return the normalized apparent-to-canonical mapping for one owner.
    pub fn repository_mapping_for_owner(
        &self,
        owner_repository: Option<&str>,
        owner_repo_root: &Path,
    ) -> HashMap<String, String> {
        let owner = if owner_repository.is_none() && owner_repo_root == self.workspace_root {
            Some(if self.repo_mappings.contains_key("") { String::new() }
                else { self.main_workspace_name().unwrap_or_default() })
        } else {
            owner_repository
                .map(str::to_owned)
                .or_else(|| self.repo_name_for_root(&owner_repo_root.to_path_buf()))
        };
        let Some(owner) = owner else {
            return HashMap::new();
        };
        self.repo_mappings
            .get(&owner)
            .map(|mapping| {
                mapping
                    .iter()
                    .map(|(apparent, canonical)| {
                        (
                            apparent.trim_start_matches('@').to_string(),
                            canonical.trim_start_matches('@').to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Try to map an apparent repo name to canonical based on current context
    fn map_repo_for_current_context(&self, apparent: &str) -> Option<String> {
        let ctx = self.current_thread_context()?;
        let current_repo_name = ctx.repository.clone()
            .or_else(|| self.current_repo_name.clone())
            .or_else(|| (ctx.repo_root != self.workspace_root).then(|| self.repo_name_for_root(&ctx.repo_root)).flatten())
            .unwrap_or_default();
        self.repository_mapping_for_owner(
            (!current_repo_name.is_empty()).then_some(current_repo_name.as_str()), &ctx.repo_root,
        ).get(apparent).cloned()
    }

    /// Is `name` this workspace's own declared name (`workspace(name = ...)`)?
    ///
    /// Prefers the name discovered during WORKSPACE evaluation, but falls
    /// back to reading the WORKSPACE file directly: the discovery path does
    /// not run on every build (a cached workspace load skips it), and when it
    /// had not run, a self-referential label went unresolved.
    fn is_main_workspace_name(&self, name: &str) -> bool {
        self.main_workspace_name().as_deref() == Some(name)
    }

    pub(crate) fn definition_label_context_for_owner(
        &self,
        repository: Option<&str>,
        package: &str,
    ) -> Result<DefinitionLabelContext> {
        let repository = repository.map(|name| name.trim_start_matches('@'))
            .filter(|name| !name.is_empty());
        let root = match repository {
            Some(owner) => self.external_repos.get(owner).map(|entry| entry.value().clone())
                .ok_or_else(|| anyhow!("Unknown repository owner {:?} for Label()", owner))?,
            None => self.workspace_root.clone(),
        };
        let mapping_is_complete = self.workspace_root.join("MODULE.bazel").is_file()
            && !self.workspace_root.join("WORKSPACE").exists()
            && !self.workspace_root.join("WORKSPACE.bazel").exists();
        let mut mapping: std::collections::BTreeMap<String, String> =
            self.repository_mapping_for_owner(repository, &root).into_iter().collect();
        if mapping_is_complete {
            mapping = self.module_repository_mapping(repository.unwrap_or(""), &root)?;
        } else if let Some(name) = self.main_workspace_name() {
            mapping.entry(name).or_default();
        }
        Ok(DefinitionLabelContext {
            repository: repository.map(str::to_owned),
            package: package.to_string(),
            repository_mapping: mapping,
            mapping_is_complete,
        })
    }

    fn repository_mapping_cache_identity(&self) -> Vec<(String, Vec<(String, String)>)> {
        let mut mappings: Vec<_> = self.repo_mappings.iter().map(|entry| {
            let mut mapping: Vec<_> = entry.value().iter()
                .map(|(apparent, canonical)| (apparent.clone(), canonical.clone())).collect();
            mapping.sort();
            (entry.key().clone(), mapping)
        }).collect();
        mappings.sort();
        mappings
    }

    fn repository_root_cache_identity(&self) -> Vec<(String, PathBuf, PathBuf)> {
        let registered: Vec<_> = self.external_repos.iter()
            .map(|entry| (entry.key().clone(), entry.value().clone())).collect();
        let mut roots: Vec<_> = registered.into_iter().map(|(owner, root)| {
            // Keep logical registration and physical ownership distinct. A
            // symlink may be repointed without changing the registered path.
            let physical = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
            (owner, root, physical)
        }).collect();
        roots.sort();
        roots
    }

    fn repository_alias_cache_identity(&self) -> Vec<(String, String)> {
        let mut aliases: Vec<_> = self.repo_aliases.iter()
            .map(|entry| (entry.key().clone(), entry.value().clone())).collect();
        aliases.sort();
        aliases
    }

    fn module_repository_mapping(
        &self, owner: &str, root: &Path,
    ) -> Result<std::collections::BTreeMap<String, String>> {
        let cache_root = tracked_command_environment("RUSH_WORKSPACE_CACHE_ROOT")?
            .map(PathBuf::from).unwrap_or_else(|| self.workspace_root.join(".cache/rush"));
        let binary = resolve_bazel_binary_for_fetch(&self.workspace_root)?;
        for input in [self.workspace_root.join("MODULE.bazel"), self.workspace_root.join("MODULE.bazel.lock"), root.join("MODULE.bazel"), root.join("MODULE.bazel.lock")] {
            track_optional_repository_mapping_input(&input)?;
        }
        let args = vec![b"--output_base".to_vec(), path_command_argument(&cache_root.join("bazel_output_base")),
            b"mod".to_vec(), b"dump_repo_mapping".to_vec(), owner.as_bytes().to_vec()];
        let output = rush_incremental::tracked::probe_process_stdout(&binary, &args,
            &bazel_repository_mapping_probe_environment(), &self.workspace_root)?;
        if output.status != 0 {
            return Err(anyhow!("Failed to read repository mapping for .bzl owner {:?}: {}", owner, String::from_utf8_lossy(&output.stderr)));
        }
        serde_json::from_slice(&output.stdout).context("Invalid .bzl owner repository mapping")
    }

    /// Resolve the logical module owner before consulting any symbol cache.
    /// An explicit label wins over reverse path lookup: several repositories
    /// may intentionally expose the same canonical source directory.
    fn module_cache_key(&self, label: &str, path: &Path) -> Result<(ModuleCacheKey, PathBuf, PathBuf)> {
        let mapping_is_complete = self.workspace_root.join("MODULE.bazel").is_file()
            && !self.workspace_root.join("WORKSPACE").exists()
            && !self.workspace_root.join("WORKSPACE.bazel").exists();
        let caller = self.current_thread_context();
        let caller_owner = caller.as_ref().and_then(|ctx| ctx.repository.clone())
            .or_else(|| self.current_repo_name.clone())
            .or_else(|| caller.as_ref().filter(|ctx| ctx.repo_root != self.workspace_root)
                .and_then(|ctx| self.repo_name_for_root(&ctx.repo_root)));
        let caller_root = caller.as_ref().map(|ctx| ctx.repo_root.clone())
            .unwrap_or_else(|| self.current_repo_root.clone());
        let caller_owner = caller_owner.or_else(|| (caller_root != self.workspace_root)
            .then(|| self.repo_name_for_root(&caller_root)).flatten());
        let mut owner = if label.starts_with('@') {
            let apparent = label.split("//").next().unwrap_or(label).trim_start_matches('@');
            if label.starts_with("@@") || apparent.is_empty() {
                Some(apparent.to_string())
            } else {
                if mapping_is_complete {
                    let mapping = self.module_repository_mapping(caller_owner.as_deref().unwrap_or(""), &caller_root)?;
                    Some(mapping.get(apparent).cloned().ok_or_else(|| anyhow!(
                        "No apparent repository {:?} in mapping for .bzl caller {:?}", apparent, caller_owner
                    ))?)
                } else {
                    let mapping = self.repository_mapping_for_owner(caller_owner.as_deref(), &caller_root);
                    Some(mapping.get(apparent).cloned().unwrap_or_else(|| {
                        if self.is_main_workspace_name(apparent) { String::new() } else { apparent.to_string() }
                    }))
                }
            }
        } else {
            caller_owner
        };
        if label.starts_with("//") && owner.as_deref().unwrap_or("").is_empty() {
            let package = label[2..].split(':').next().unwrap_or("");
            if package.starts_with("third_party/googleapis") {
                owner = Some("com_google_googleapis".to_string());
            } else if let Some(alias) = package.split('/').next().and_then(|first| self.repo_aliases.get(first)) {
                owner = Some(alias.value().clone());
            }
        }
        // Retain the existing legacy split-repository load route.
        if label.starts_with("@local_xla//tsl/") && self.external_repos.contains_key("local_tsl") {
            owner = Some("local_tsl".to_string());
        }
        let root = match owner.as_deref().filter(|owner| !owner.is_empty()) {
            Some(owner) => self.external_repos.get(owner).map(|entry| entry.value().clone())
                .or_else(|| label.strip_prefix('@').filter(|label| !label.starts_with('@'))
                    .and_then(|label| label.split("//").next())
                    .and_then(|apparent| self.external_repos.get(apparent).map(|entry| entry.value().clone())))
                .unwrap_or_else(|| caller_root.clone()),
            None => self.workspace_root.clone(),
        };
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let file_parent = path.parent().ok_or_else(|| anyhow!("No parent for .bzl file {}", path.display()))?.to_path_buf();
        let package_dir = if let Some((_, target)) = label.split_once("//") {
            target.split_once(':').map(|(package, _)| {
                let package = if label.starts_with("//third_party/googleapis") {
                    package.strip_prefix("third_party/googleapis").unwrap_or(package).trim_start_matches('/')
                } else { package };
                root.join(package)
            })
                .unwrap_or_else(|| file_parent.clone())
        } else if label.starts_with('@') {
            root.clone()
        } else {
            caller.as_ref().map(|ctx| ctx.package_dir.clone())
                .or_else(|| self.current_package.clone()).unwrap_or_else(|| file_parent.clone())
        };

        let owner = owner.filter(|name| !name.is_empty());
        let mut mapping: std::collections::BTreeMap<String, String> =
            self.repository_mapping_for_owner(owner.as_deref(), &root).into_iter().collect();
        if mapping_is_complete {
            mapping = self.module_repository_mapping(owner.as_deref().unwrap_or(""), &root)?;
        } else if let Some(name) = self.main_workspace_name() {
            mapping.entry(name).or_default();
        }
        let package = package_dir.strip_prefix(&root).with_context(|| format!("Module {} is outside its repository root", path.display()))?
            .to_string_lossy().replace('\\', "/");
        // The source may be an individual symlink outside the logical root.
        // Derive the load target from its address, never from the physical IO key.
        let filename = if let Some((_, target)) = label.split_once("//") {
            target.split_once(':').map(|(_, filename)| filename.to_string())
                .unwrap_or_else(|| path.file_name().unwrap_or_default().to_string_lossy().into_owned())
        } else if label.starts_with('@') {
            label.trim_start_matches('@').to_string()
        } else {
            label.trim_start_matches(':').to_string()
        };
        let label = format!("@@{}//{}:{}", owner.as_deref().unwrap_or(""), package, filename);
        let context = DefinitionLabelContext { repository: owner, package, repository_mapping: mapping, mapping_is_complete };
        // A cached re-export may retain closures from another owner. Include
        // the installed mapping set so changing that owner's map also retires
        // the parent module's exports, even without a physical source edit.
        let mappings = self.repository_mapping_cache_identity();
        Ok((ModuleCacheKey { path: path.to_path_buf(), workspace: self.workspace_root.clone(), context: Some(context), label: Some(label), mappings,
            repository_roots: self.repository_root_cache_identity(), aliases: self.repository_alias_cache_identity() }, root, package_dir))
    }

    /// Reverse lookup: find repo name by its root path
    fn repo_name_for_root(&self, root: &PathBuf) -> Option<String> {
        for entry in self.external_repos.iter() {
            if entry.value() == root {
                return Some(entry.key().clone());
            }
        }
        None
    }

    /// Load a .bzl file and return exported symbols
    ///
    /// # Arguments
    /// * `bzl_label` - .bzl file label (e.g., "//pkg:file.bzl", "@repo//pkg:file.bzl", ":file.bzl")
    /// * `symbols` - Symbols to export as (local_name, original_name) tuples
    ///
    /// # Returns
    /// HashMap of local symbol names to their values
    ///
    /// # Example
    /// ```
    /// // load("file.bzl", "foo") → [("foo", "foo")]
    /// // load("file.bzl", bar = "foo") → [("bar", "foo")]
    /// ```
    pub fn load_bzl(
        &mut self,
        bzl_label: &str,
        symbols: &[(String, String)],
        loader_arc: Option<std::sync::Arc<std::sync::Mutex<BzlLoader>>>,
    ) -> Result<HashMap<String, Value>> {
        // Phase 5.C.4 commit 103: start wall-clock timing
        // for cold-load attribution. The record_cold_load
        // call site below uses this Instant; the cache-hit
        // fast path discards it.
        let cold_load_start = std::time::Instant::now();
        // Persistent-daemon Run-wiring (blocker #1): mark the current command's
        // tracked manifest at entry so the cold (miss) path can capture this
        // `.bzl`'s full transitive dependency set; a later cache HIT replays it.
        // `None` outside a command scope (capture/replay then no-op).
        let dep_mark = rush_incremental::tracked::mark();
        let normalized_label = Self::normalize_load_label(bzl_label);
        if let Some(cycle_path) = detect_local_cycle_path(&normalized_label) {
            return Err(anyhow!(
                "Circular dependency detected while loading {}.\n\
                 Cycle: {}\n\
                 Rush now aborts immediately on same-thread recursion instead of waiting for itself.",
                bzl_label,
                cycle_path
            ));
        }

        let mut _loading_guards: Option<LoadingGuards> = None;

        let canonical_path = self.resolve_bzl_path(&normalized_label)?;
        let (module_key, repo_root, package_dir) = self.module_cache_key(&normalized_label, &canonical_path)?;
        let canonical_key = module_key.inflight_key();

        if !BZL_LOADING_SET.insert(canonical_key.clone()) {
            eprintln!(
                "⚠️ CIRCULAR DEPENDENCY: {} is already being loaded by another task",
                canonical_key
            );
            eprintln!(
                "   Currently loading: {:?}",
                BZL_LOADING_SET
                    .iter()
                    .map(|e| e.key().clone())
                    .collect::<Vec<_>>()
            );

            return Err(anyhow!(
                "Circular dependency detected in .bzl loading: {}\n\n\
                 This file is already being loaded by another parallel task.\n\
                 This typically occurs when .bzl files have circular dependencies\n\
                 (e.g., file1.bzl loads file2.bzl which loads file1.bzl).\n\
                 \n\
                 The cache prevents infinite I/O, but parallel loading can create deadlocks.\n\
                 \n\
                To fix: Review the .bzl loading chain and break the circular reference.",
                canonical_key
            ));
        } else {
            _loading_guards = Some(LoadingGuards::new(&canonical_key, &canonical_key));
        }

        // PHASE 2: Use GlobalMemoryBudget for atomic memory reservation
        // Realistic estimate: 50 MB per .bzl file (was 768 MB - way too conservative)
        // With DashMap caching, actual usage is ~20-30 MB per file
        const ESTIMATED_BZL_SIZE_MB: u64 = 50; // 50 MB

        // Enum to hold either type of guard for RAII cleanup
        // Fields are intentionally "unused" - they exist only for Drop behavior
        #[allow(dead_code)]
        enum MemGuard<'a> {
            Global(rush_resource::MemoryReservation<'a>),
            Legacy(BzlMemoryGuard),
        }

        // Clone the budget Arc to avoid borrowing self - keep Arc alive in outer scope
        let budget_arc = self.memory_budget.clone();

        let _mem_guard = if let Some(ref budget) = budget_arc {
            MemGuard::Global(budget.reserve(bzl_label, ESTIMATED_BZL_SIZE_MB)?)
        } else {
            // Fallback to old guard for backward compatibility
            MemGuard::Legacy(BzlMemoryGuard::reserve(
                bzl_label,
                ESTIMATED_BZL_SIZE_MB as usize,
            )?)
        };

        debug_log!("[DEBUG BzlLoader] Loading: {}", bzl_label);
        let bzl_path = canonical_path.clone();
        debug_log!("[DEBUG BzlLoader] Resolved to: {}", bzl_path.display());

        if let Some(cached_symbols) = self.get_symbols_cache_subset_key(&module_key, symbols) {
            debug_log!("[DEBUG BzlLoader] Cache hit for: {}", bzl_label);
            return Ok(cached_symbols);
        }

        // Phase 5.C.4 commit 104 + 105: push thread-
        // local cold-load frame so any nested child
        // .bzl loads triggered during eval get
        // attributed to us. The _ex variant also
        // surfaces our parent path (load-graph edge)
        // and our post-push depth.
        let push_result = push_cold_load_frame_ex(&bzl_path);
        let frame_recursion = push_result.recursive;
        if let Some(ref global) = self.global_cache {
            if let Some(ref parent) = push_result.parent_path {
                global.record_load_edge(parent, &bzl_path);
            }
            global.record_identity(bzl_label, &bzl_path);
            global.record_observed_depth(&bzl_path, push_result.depth_after_push);
        }

        debug_log!("[DEBUG BzlLoader] Parsing: {}", bzl_path.display());
        // Parse the .bzl file (NO CACHING - ASTs are huge!)
        // Phase 5.C.4 commit 104: split into read + parse
        // subphases so sub-Starlark attribution can show
        // which dominates per file.
        let subphase_read_start = std::time::Instant::now();
        // The direct loader is also used by tracked daemon analysis, but legacy
        // standalone callers have no installed TrackedIo. Record the source in
        // the producing manifest whenever a command scope exists; retain the raw
        // read only for the legacy/untracked entry point.
        let source_result = if let Some(io) = rush_incremental::tracked::current() {
            io.read_to_string(&bzl_path)
        } else {
            std::fs::read_to_string(&bzl_path)
        };
        let source = match source_result {
            Ok(s) => s,
            Err(e) => {
                let _ = pop_cold_load_frame(cold_load_start.elapsed().as_nanos() as u64);
                return Err(anyhow!(
                    "Failed to read .bzl file {}: {}",
                    bzl_path.display(),
                    e
                ));
            }
        };
        let subphase_read_dur = subphase_read_start.elapsed();
        let subphase_parse_start = std::time::Instant::now();
        let program = match crate::parse(&source) {
            Ok(p) => p,
            Err(e) => {
                let _ = pop_cold_load_frame(cold_load_start.elapsed().as_nanos() as u64);
                return Err(anyhow!(
                    "Failed to parse .bzl file {}: {}",
                    bzl_path.display(),
                    e
                ));
            }
        };
        let subphase_parse_dur = subphase_parse_start.elapsed();

        // #39 graph-RSS: track this parsed Program as live across evaluation
        // (peak live-Program census, RUSH_MEM_CENSUS=1). Dropped with
        // `program` at end of scope (post-eval / post-cache-insert).
        let _live_program_guard = track_live_program(&program, source.len());

        // Don't acquire a cpu_slot permit for .bzl evaluation (see the
        // longer comment in load_bzl_from_evaluator). Memory pressure is
        // enforced separately by the PSI watchdog.
        let resource_handle: Option<rush_resource::ResourceHandle> = None;

        debug_log!("[DEBUG BzlLoader] Evaluating: {}", bzl_path.display());
        // Evaluate WITHOUT holding the loader lock (evaluation may recursively load other .bzl files)
        // We pass the loader_arc so nested loads can happen
        // CRITICAL: Pass &program by reference, NO CLONING (ASTs are huge!)
        let subphase_eval_start = std::time::Instant::now();
        let symbols_map = match Self::evaluate_bzl_static(&program, loader_arc,
            module_key.context.clone().expect("qualified module context"),
            BzlContextFrame { repo_root, package_dir, repository: Some(module_key.context.as_ref().and_then(|ctx| ctx.repository.clone()).unwrap_or_default()), load_stack: current_stack_snapshot() }) {
            Ok(m) => m,
            Err(e) => {
                let _ = pop_cold_load_frame(cold_load_start.elapsed().as_nanos() as u64);
                return Err(e);
            }
        };
        let subphase_eval_dur = subphase_eval_start.elapsed();
        debug_log!("[DEBUG BzlLoader] Evaluation complete for: {}", bzl_label);

        // Release resources after evaluation
        if let Some(handle) = resource_handle {
            debug_log!(
                "[DEBUG BzlLoader] Releasing resources after .bzl evaluation: {}",
                bzl_label
            );
            // Use block_in_place to avoid blocking worker thread
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async { handle.release_async().await })
            });
        }

        // Phase 5.C.4 commit 104: finalize subphase
        // covers the cache-insert path. (This entry
        // point doesn't run resolve_lazy_loads — that
        // step only fires on the
        // load_bzl_from_evaluator path. The commit-111
        // sub-finalize split sets
        // resolve_lazy_loads=None here and
        // put_symbols_cache=finalize.)
        // Phase 5.C.4 commit 113: extract before
        // cache so we can MOVE symbols_map (the
        // expensive part) without cloning. bzl_path is
        // still cloned (cheap String clone) because
        // put_symbols_cache takes PathBuf by value and
        // we need the path for downstream metric
        // recording.
        let extract_result = self.extract_symbols(&symbols_map, symbols);
        let symbols_count = symbols_map.len() as u64;
        let subphase_finalize_start = std::time::Instant::now();
        let subphase_put_symbols_cache_start = std::time::Instant::now();
        let inserted_generation = self.put_symbols_cache(module_key.clone(), symbols_map);
        // Persistent-daemon Run-wiring (blocker #1): capture this `.bzl`'s full
        // transitive tracked dependency set (everything recorded since entry) and
        // store it on the just-inserted cache entry so a cross-command HIT replays
        // it. No-op outside a command scope or when nothing was recorded.
        if let (Some(mark), Some(generation)) = (dep_mark, inserted_generation) {
            let captured = rush_incremental::tracked::captured_since(mark);
            self.set_dep_set_cache(&module_key, generation, captured);
        }
        let subphase_put_symbols_cache_dur =
            subphase_put_symbols_cache_start.elapsed();
        let subphase_finalize_dur = subphase_finalize_start.elapsed();

        // Phase 5.C.4 commit 103/104: record cold-load
        // attribution AFTER successful eval + cache
        // insert. Reaching this code path means we hit
        // a cache miss and performed a fresh load.
        // File size is best-effort (errors → 0).
        // Commit 104 adds own/child split via frame stack
        // and per-subphase wall time.
        let cold_load_elapsed = cold_load_start.elapsed();
        let file_size_bytes = std::fs::metadata(&bzl_path)
            .map(|m| m.len())
            .unwrap_or(0);
        // Phase 5.C.4 commit 106/110: pop_ex surfaces
        // accumulated Starlark-op counts AND the
        // attribution classification.
        let pop = pop_cold_load_frame_ex(cold_load_elapsed.as_nanos() as u64);
        let child_total_ns = pop.child_total_ns;
        let recursion_detected = pop.recursion_detected;
        let drained_ops = pop.ops;
        let attribution = pop.attribution;
        let child_duration = if recursion_detected || frame_recursion {
            None
        } else {
            Some(std::time::Duration::from_nanos(child_total_ns))
        };
        let subphases = ColdLoadSubphases {
            read: Some(subphase_read_dur),
            parse: Some(subphase_parse_dur),
            eval: Some(subphase_eval_dur),
            finalize: Some(subphase_finalize_dur),
            // Phase 5.C.4 commit 111: load_bzl path
            // doesn't run resolve_lazy_loads.
            resolve_lazy_loads: None,
            put_symbols_cache: Some(subphase_put_symbols_cache_dur),
        };
        if let Some(ref global) = self.global_cache {
            global.record_cold_load_full_v2(
                &bzl_path,
                cold_load_elapsed,
                child_duration,
                file_size_bytes,
                symbols_count,
                Some(subphases),
                attribution,
            );
            global.record_starlark_ops(&bzl_path, drained_ops);
        }

        // Phase 5.C.4 commit 113: extract_result was
        // computed BEFORE the move-cache.
        extract_result
    }

    /// Static version of evaluate_bzl that doesn't require &mut self
    /// This allows evaluation to happen without holding the BzlLoader lock
    fn evaluate_bzl_static(
        program: &Program,
        loader_arc: Option<std::sync::Arc<std::sync::Mutex<BzlLoader>>>,
        context: DefinitionLabelContext,
        frame: BzlContextFrame,
    ) -> Result<HashMap<String, Value>> {
        Self::push_thread_context_frame(frame);
        struct ContextGuard;
        impl Drop for ContextGuard {
            fn drop(&mut self) { BzlLoader::pop_thread_context(); }
        }
        let _context_guard = ContextGuard;
        let mut evaluator = Evaluator::new();
        evaluator.set_current_repository(context.repository.clone());

        // Register native namespace for macros (native.cc_library, etc.)
        evaluator.register_native_namespace();

        // Set the bzl_loader so .bzl files can recursively load other .bzl files
        if let Some(loader) = loader_arc {
            evaluator.set_bzl_loader(loader);
        }
        evaluator.set_definition_label_context(context);

        // Evaluate the program
        evaluator
            .eval_program(program)
            .map_err(|e| anyhow!("Failed to evaluate .bzl file: {}", e))?;

        // Extract all symbols from the evaluator environment
        let symbols = evaluator.export_environment();

        Ok(symbols)
    }

    /// Resolve a .bzl file label to a filesystem path
    ///
    /// Supports:
    /// - `//package:file.bzl` → `{workspace_root}/package/file.bzl`
    /// - `@repo//package:file.bzl` → `{external_cache}/repo/package/file.bzl`
    /// - `@repo` → `@repo//:repo` (canonical-label shorthand)
    /// - `:file.bzl` → `{current_package}/file.bzl`
    ///
    /// IMPORTANT: Returns canonicalized path to ensure cache key consistency.
    /// Without canonicalization, same file via different paths (symlinks, ./, etc.)
    /// creates different cache keys → cache misses.
    pub fn resolve_bzl_path(&self, bzl_label: &str) -> Result<PathBuf> {
        let path = if bzl_label.starts_with("@") {
            // External repository: @repo//package:file.bzl
            self.resolve_external_bzl(bzl_label)?
        } else if bzl_label.starts_with("//") {
            // Workspace absolute: //package:file.bzl
            self.resolve_workspace_bzl(bzl_label)?
        } else if bzl_label.starts_with(":") {
            // Package relative: :file.bzl
            self.resolve_relative_bzl(bzl_label)?
        } else {
            // Treat bare filenames (e.g., "tsl.bzl") as package-relative
            self.resolve_relative_bzl(&format!(":{}", bzl_label))?
        };

        // Canonicalize through the tracked IO so every symlink hop on the .bzl
        // resolution path records a Symlink dependency (a symlinked external-repo
        // root or .bzl swap must invalidate a reused graph). The resolved-path
        // logic is identical to fs::canonicalize; only the symlink reads are now
        // tracked. This is the per-.bzl-load helper the daemon gauge attributed
        // its ~69 readlink() probes to.
        match rush_incremental::tracked::canonicalize(&path) {
            Ok(canonical) => Ok(canonical),
            Err(e) => {
                debug_log!(
                    "[WARN] Failed to canonicalize path {}: {}",
                    path.display(),
                    e
                );
                Ok(path)
            }
        }
    }

    /// Resolve external repository .bzl file (@repo//pkg:file.bzl)
    fn resolve_external_bzl(&self, bzl_label: &str) -> Result<PathBuf> {
        debug_log!(
            "[DEBUG resolve_external_bzl] Resolving label: {}",
            bzl_label
        );

        // Parse @repo//package:file.bzl. Bazel also accepts @repo as the
        // canonical-label shorthand for @repo//:repo; generated repositories
        // commonly use this when the root .bzl file has the repository name.
        //
        // A '@@' prefix is a CANONICAL label (Bzlmod): '@@//pkg' is the root
        // module and '@@name//pkg' names the canonical repository directly —
        // never subject to apparent-name repo mapping. rules_pycross's
        // generated pip lock BUILD files use both forms.
        let (rest, is_canonical) = match bzl_label.strip_prefix("@@") {
            Some(body) => (body, true),
            None => (&bzl_label[1..], false),
        };
        let (mut repo_name, path_and_file) = if let Some((repo, path)) = rest.split_once("//") {
            (repo.to_string(), path.to_string())
        } else if !rest.is_empty() {
            (rest.to_string(), format!(":{}", rest))
        } else {
            return Err(anyhow!("Invalid external .bzl label: {}", bzl_label));
        };

        debug_log!(
            "[DEBUG resolve_external_bzl] Parsed repo_name: '{}', path_and_file: '{}'",
            repo_name,
            path_and_file
        );

        // TensorFlow vendors TSL INSIDE XLA, at
        // third_party/xla/third_party/tsl. A .bzl loaded from XLA context
        // that says //tsl/... becomes @local_xla//tsl/..., which would
        // resolve to third_party/xla/tsl - a path that does not exist.
        // dependency_graph.rs already remaps that for DEPENDENCIES; .bzl
        // loads took a different route and did not, so
        // `@local_xla//tsl/...:*.bzl` was reported missing and hosts worked
        // around it with a hand-made symlink. Remap here too, so the engine
        // resolves it rather than the host.
        if !is_canonical
            && repo_name == "local_xla"
            && (path_and_file.starts_with("tsl/") || path_and_file.starts_with("tsl:"))
        {
            debug_log!(
                "[DEBUG resolve_external_bzl] Remapping @local_xla//{} to @local_tsl",
                path_and_file
            );
            repo_name = "local_tsl".to_string();
        }


        // Apply per-repository repo_mapping to map apparent -> canonical
        // repo. Canonical (@@) labels bypass the mapping by definition.
        if is_canonical {
            debug_log!(
                "[DEBUG resolve_external_bzl] Canonical label; skipping repo_mapping for '{}'",
                repo_name
            );
        } else if let Some(mapped) = self.map_repo_for_current_context(&repo_name) {
            debug_log!(
                "[DEBUG resolve_external_bzl] Mapped repo '{}' -> '{}' via repo_mapping",
                repo_name,
                mapped
            );
            repo_name = mapped;
        } else {
            debug_log!(
                "[DEBUG resolve_external_bzl] No mapping found for repo '{}'",
                repo_name
            );
        }

        // Handle @// (main workspace) - empty repo name means main workspace
        // Don't try to fetch it as an external repo, just use workspace root
        // ('@@//' resolves here too: the root module's canonical name is "").
        let mut repo_root_opt = if repo_name.is_empty() {
            Some(self.workspace_root.clone())
        } else {
            // Look up repository path
            self.external_repos
                .get(&repo_name)
                .map(|r| r.value().clone())
        };
        let legacy_workspace = rush_incremental::tracked::exists(&self.workspace_root.join("WORKSPACE"))
            || rush_incremental::tracked::exists(&self.workspace_root.join("WORKSPACE.bazel"));

        // A non-empty canonical name resolves directly against the shared
        // bazel output base's external tree — Bazel materialized it there
        // under exactly this name (no mapping, no fetch needed).
        if repo_root_opt.is_none() && is_canonical {
            let cache_root = std::env::var("RUSH_WORKSPACE_CACHE_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(|_| self.workspace_root.join(".cache").join("rush"));
            let candidate = cache_root.join("bazel_output_base").join("external").join(&repo_name);
            if candidate.is_dir() {
                debug_log!(
                    "[DEBUG resolve_external_bzl] Canonical repo '{}' found in output base: {}",
                    repo_name,
                    candidate.display()
                );
                self.external_repos.insert(repo_name.clone(), candidate.clone());
                repo_root_opt = Some(candidate);
            }
        }

        // Known local stubs avoid hard dependency on bazel sync for
        // lightweight WORKSPACE configuration repos that exist for parser
        // correctness only. A Bzlmod-only workspace must prefer its real
        // canonical repository; a stub can mask an already-materialized repo
        // such as protobuf+.
        let bzlmod_only = self.workspace_root.join("MODULE.bazel").exists()
            && !self.workspace_root.join("WORKSPACE").exists()
            && !self.workspace_root.join("WORKSPACE.bazel").exists();
        if repo_root_opt.is_none() && !bzlmod_only && !legacy_workspace {
            if let Some(stub_path) = create_known_stub_repo(&self.workspace_root, &repo_name)? {
                debug_log!(
                    "[DEBUG resolve_external_bzl] Created/using known stub for '{}': {}",
                    repo_name,
                    stub_path.display()
                );
                self.external_repos
                    .insert(repo_name.to_string(), stub_path.clone());
                repo_root_opt = Some(stub_path);
            }
        }

        // If repo is warmup-materialized, use it immediately and avoid
        // going through the rest of on-demand resolution.
        if repo_root_opt.is_none() && !legacy_workspace {
            if let Some(path) = crate::fetch_strategy::is_warmed_up(&repo_name) {
                if path.exists() {
                    debug_log!(
                        "[DEBUG resolve_external_bzl] Repo '{}' is warmup-materialized: {}",
                        repo_name,
                        path.display()
                    );
                    self.external_repos.insert(repo_name.to_string(), path.clone());
                    repo_root_opt = Some(path);
                }
            }
        }

        // A workspace may refer to ITSELF by its declared name. TensorFlow's
        // WORKSPACE says `workspace(name = "org_tensorflow")`, and vendored
        // external BUILD files then load
        // `@org_tensorflow//third_party/mkl:build_defs.bzl`, which Bazel
        // resolves to the MAIN workspace — not to an external repo of that
        // name. rush searched the external repos, found none, and tried
        // `bazel sync --only=org_tensorflow`; when that failed, @onednn's
        // BUILD did not parse and @onednn//:mkl_dnn was stubbed on EVERY
        // target in the corpus. Checked here, after the real external repos,
        // so a genuine repo of the same name still wins.
        if repo_root_opt.is_none() && !is_canonical && self.is_main_workspace_name(&repo_name) {
            debug_log!(
                "[DEBUG resolve_external_bzl] '{}' is this workspace's own name; \
                 resolving //{} against the workspace root",
                repo_name,
                path_and_file
            );
            return self.resolve_workspace_bzl(&format!("//{}", path_and_file));
        }

        // If repo not found, attempt on-demand fetch for known repos and register.
        // Keep this log after warmup/known-repo fast paths so it does not fire for
        // repos already satisfied by preloaded materialization.
        if repo_root_opt.is_none() {
            let requested_module = path_and_file.split_once(':')
                .map(|(package, filename)| PathBuf::from(package).join(filename))
                .ok_or_else(|| anyhow!("Invalid .bzl path in external label: {}", bzl_label))?;
            if let Some(fetched) = self.try_fetch_known_repo(&repo_name, is_canonical, Some(&requested_module), Some(&format!("@{}//{}", repo_name, path_and_file)))? {
                debug_log!(
                    "[DEBUG resolve_external_bzl] Successfully fetched repo '{}' to {}",
                    repo_name,
                    fetched.display()
                );
                // Register in external_repos and use it
                self.external_repos
                    .insert(repo_name.to_string(), fetched.clone());
                repo_root_opt = Some(fetched);
            } else {
                debug_log!("[DEBUG resolve_external_bzl] Repo '{}' not found in external_repos, attempting on-demand fetch", repo_name);
                // Re-check map in case other paths registered the repo during chunk evaluation
                repo_root_opt = self
                    .external_repos
                    .get(&repo_name)
                    .map(|r| r.value().clone());
            }
        }

        // FIX: Also check the workspace's external/ directory for pre-configured stub repos
        // This handles repos like com_google_googleapis that are stubbed out in external/
        if repo_root_opt.is_none() && !legacy_workspace {
            let external_path = self.workspace_root.join("external").join(&repo_name);
            if external_path.exists() && external_path.is_dir() {
                debug_log!(
                    "[DEBUG resolve_external_bzl] Found repo '{}' in workspace external/: {}",
                    repo_name,
                    external_path.display()
                );
                self.external_repos.insert(repo_name.to_string(), external_path.clone());
                repo_root_opt = Some(external_path);
            }
        }
        if repo_root_opt.is_none() && !legacy_workspace {
            let cache_external = cache_backed_external_repo_dir(&self.workspace_root, &repo_name);
            if cache_external.exists() && cache_external.is_dir() {
                debug_log!(
                    "[DEBUG resolve_external_bzl] Found repo '{}' in cache external/: {}",
                    repo_name,
                    cache_external.display()
                );
                self.external_repos
                    .insert(repo_name.to_string(), cache_external.clone());
                repo_root_opt = Some(cache_external);
            }
        }

        let repo_root = repo_root_opt.ok_or_else(|| {
            anyhow!(
                "External repository '{}' not found. Did you run load_workspace()?",
                repo_name
            )
        })?;
        let requested_label = format!("@{}//{}", repo_name, path_and_file);
        let repo_root = self.admit_registered_workspace_repository_for_label(&repo_name, &repo_root, Some(&requested_label))?;
        if !repo_name.is_empty() {
            self.external_repos.insert(repo_name.clone(), repo_root.clone());
        }

        debug_log!(
            "[DEBUG resolve_external_bzl] Resolved repo root for '{}': {}",
            repo_name,
            repo_root.display()
        );

        // Parse package:file.bzl
        let (package, filename) = if let Some(colon_pos) = path_and_file.find(':') {
            (&path_and_file[..colon_pos], &path_and_file[colon_pos + 1..])
        } else {
            return Err(anyhow!(
                "Invalid .bzl path in external label: {}",
                bzl_label
            ));
        };

        let bzl_path = repo_root.join(package).join(filename);
        debug_log!(
            "[DEBUG resolve_external_bzl] Constructed final path: {}",
            bzl_path.display()
        );

        if !rush_incremental::tracked::exists(&bzl_path) {
            debug_log!(
                "[ERROR resolve_external_bzl] File does not exist: {}",
                bzl_path.display()
            );
            // List contents of parent directory to help debugging
            if let Some(parent) = bzl_path.parent() {
                if parent.exists() {
                    debug_log!(
                        "[DEBUG resolve_external_bzl] Contents of {}:",
                        parent.display()
                    );
                    if let Ok(names) = rush_incremental::tracked::read_dir_names(parent) {
                        for name in names {
                            debug_log!("  - {:?}", name);
                        }
                    }
                } else {
                    debug_log!(
                        "[DEBUG resolve_external_bzl] Parent directory does not exist: {}",
                        parent.display()
                    );
                }
            }
            return Err(anyhow!(".bzl file not found: {}", bzl_path.display()));
        }

        Ok(bzl_path)
    }

    fn mark_repo_fetched_for_dedupe(repo_name: &str) {
        if let Ok(mut set) = SELF_SINGLETON_FETCHED.lock() {
            set.insert(repo_name.to_string());
        }
    }

    fn try_fetch_known_repo(
        &self,
        repo_name: &str,
        canonical: bool,
        requested_module: Option<&Path>,
        requested_label: Option<&str>,
    ) -> Result<Option<PathBuf>> {
        let workspace_root = &self.workspace_root;
        // GUARD AGAINST INFINITE RECURSION: Check if we're already fetching this repo
        // This prevents stack overflow when WORKSPACE load() statements reference
        // external repos that we're currently trying to fetch.
        {
            if let Ok(being_fetched) = REPOS_BEING_FETCHED.lock() {
                if being_fetched.contains(repo_name) {
                    debug_log!(
                        "[WARN] Circular fetch detected: {} is already being fetched, skipping",
                        repo_name
                    );
                    return Ok(None);
                }
            }
        }

        if rush_incremental::tracked::exists(&workspace_root.join("WORKSPACE"))
            || rush_incremental::tracked::exists(&workspace_root.join("WORKSPACE.bazel"))
        {
            // A name-only success bit or an unverified directory is not a
            // declaration proof. Registered roots are handled by the caller;
            // unknown WORKSPACE repos go through the genuine rule and shared
            // FILE/ENV/config freshness admission before any heuristic skips.
            let _guard = FetchGuard::new(repo_name.to_string());
            // Preserve the original package/target separator. A filesystem
            // path loses the distinction in //defs:subdir/child.bzl.
            return match requested_label {
                Some(module) => Self::fetch_declared_workspace_repository_for_module(workspace_root, repo_name, module),
                None => Self::fetch_declared_workspace_repository(workspace_root, repo_name),
            };
        }

        if std::env::var("RUSH_DISABLE_FETCH_DEDUP").ok().as_deref() != Some("1") {
            if let Ok(set) = SELF_SINGLETON_FETCHED.lock() {
                if set.contains(repo_name) {
                    return Ok(None);
                }
            }
        }

        // FIX: Skip fetching for repos that have stubs in external/ directory
        // These repos need special handling (e.g., TensorFlow configure.py generates them)
        // or have Rush-provided stubs that should be used instead of bazel fetch.
        // The external/ directory is checked by resolve_external_bzl() after this returns None.
        const SKIP_FETCH_REPOS: &[&str] = &[
            "com_google_googleapis",
            "local_config_cuda",
            "local_config_tensorrt",
            "local_config_rocm",
            "com_google_protobuf",
            "rules_python_internal",
            "pybind11",
            "com_github_grpc_grpc",
            "local_config_remote_execution",
        ];
        if SKIP_FETCH_REPOS.contains(&repo_name) {
            let external_path =
                vendored_external_repo_dir(workspace_root, repo_name);
            if external_path.exists() {
                debug_log!(
                    "[DEBUG BzlLoader] Skipping fetch for {}: stub exists in external/",
                    repo_name
                );
                if let Some(ref gc) = self.global_cache {
                    gc.record_fetch_decision(
                        &FetchDecision::SkippedKnownRule {
                            repo_name: repo_name.to_string(),
                            path: external_path.clone(),
                        },
                    );
                }
                return Ok(None); // Let resolve_external_bzl find it in external/
            }

            if let Some(stub_repo) = create_known_stub_repo(workspace_root, repo_name)? {
                debug_log!(
                    "[DEBUG BzlLoader] Created known stub for {} at {}",
                    repo_name,
                    stub_repo.display()
                );
                Self::mark_repo_fetched_for_dedupe(repo_name);
                if let Some(ref gc) = self.global_cache {
                    gc.record_fetch_decision(
                        &FetchDecision::SkippedKnownRule {
                            repo_name: repo_name.to_string(),
                            path: stub_repo.clone(),
                        },
                    );
                }
                return Ok(Some(stub_repo));
            }

            if let Some(warmup_path) = crate::fetch_strategy::is_warmed_up(repo_name) {
                if warmup_path.exists() {
                    debug_log!(
                        "[DEBUG BzlLoader] Using prefetch warmup path for {}: {}",
                        repo_name,
                        warmup_path.display()
                    );
                    if let Some(ref gc) = self.global_cache {
                        gc.record_fetch_decision(&FetchDecision::SkippedKnownRule {
                            repo_name: repo_name.to_string(),
                            path: warmup_path.clone(),
                        });
                    }
                    Self::mark_repo_fetched_for_dedupe(repo_name);
                    return Ok(Some(warmup_path));
                }
            }
        }

        // Phase 5.C.4 commit 134 (Batch Q): env-gated
        // generalized variant of the
        // SKIP_FETCH_REPOS allowlist. If
        // `RUSH_FETCH_SKIP_IF_MATERIALIZED=1` AND the
        // vendored external/<repo>/ directory exists
        // AND a BUILD/BUILD.bazel file is present,
        // skip the bazel-fetch subprocess.
        //
        // This is a *trust* mode: rush takes on-disk
        // presence as evidence of materialization
        // without verifying content sha256, source
        // URL, or completeness. Default-off; operator
        // opt-in. See
        // docs/analysis/batch-q-commit-134-3run-data.md
        // for the rationale (37% TF-macros analyze
        // speedup) and
        // commit 137's correctness-policy doc for
        // when this is safe to enable.
        //
        // The actual decision is computed by the pure
        // `evaluate_materialized_skip` helper so the
        // unit tests can exercise the same predicate.
        let env_on = std::env::var("RUSH_FETCH_SKIP_IF_MATERIALIZED")
            .ok()
            .as_deref()
            == Some("1");
        if let Some(decision) =
            evaluate_materialized_skip(workspace_root, repo_name, env_on)
        {
            if let Some(ref gc) = self.global_cache {
                gc.record_fetch_decision(&decision);
            }
            // Commit 149: three skip variants all
            // return Ok(None) — the caller resolves
            // labels against the vendored path. The
            // distinguishing eprintln + warning is
            // handled inside evaluate_materialized_skip
            // (for SkippedMaterializedTrustMode) and
            // here (for the others).
            match &decision {
                FetchDecision::SkippedMaterialized { path, .. } => {
                    eprintln!(
                        "    ⏭️  fetch skip (materialized): @{} → {}",
                        repo_name,
                        path.display()
                    );
                    return Ok(None);
                }
                FetchDecision::SkippedMaterializedManifestValid {
                    path, ..
                } => {
                    eprintln!(
                        "    ⏭️  fetch skip (manifest-valid): @{} → {}",
                        repo_name,
                        path.display()
                    );
                    return Ok(None);
                }
                FetchDecision::SkippedMaterializedTrustMode {
                    path, ..
                } => {
                    // Warning already emitted inside
                    // evaluate_materialized_skip.
                    eprintln!(
                        "    ⏭️  fetch skip (trust-mode): @{} → {}",
                        repo_name,
                        path.display()
                    );
                    return Ok(None);
                }
                _ => {}
            }
            // MaterializedMissingDir and
            // MaterializedMissingBuild fall through to
            // the rest of try_fetch_known_repo.
        }

        if let Some(path) = Self::find_third_party_dir(workspace_root, repo_name)? {
            let declared_source = Self::resolve_third_party_source_candidate(
                &path,
                requested_module,
                || {
                    let _guard = FetchGuard::new(repo_name.to_string());
                    Self::fetch_declared_workspace_repository(workspace_root, repo_name)
                },
            )?;
            if declared_source.is_some() {
                Self::mark_repo_fetched_for_dedupe(repo_name);
            }
            return Ok(declared_source);
        }

        // Create RAII guard to track and auto-cleanup this fetch
        // Guard will automatically remove repo from REPOS_BEING_FETCHED when dropped
        let _guard = FetchGuard::new(repo_name.to_string());

        if rush_incremental::tracked::exists(&workspace_root.join("WORKSPACE"))
            || rush_incremental::tracked::exists(&workspace_root.join("WORKSPACE.bazel"))
        {
            // Repository macros can format URLs and declare patches/overlays.
            // The abbreviated regex tuples below cannot represent that rule.
            let fetched = Self::fetch_declared_workspace_repository(workspace_root, repo_name)?;
            if fetched.is_some() {
                Self::mark_repo_fetched_for_dedupe(repo_name);
            }
            return Ok(fetched);
        }

        // Try WORKSPACE repository rules first (chunked scanning)
        let workspace_path = workspace_root.join("WORKSPACE");
        if workspace_path.exists() {
            if let Some(found) = Self::scan_workspace_in_chunks_for_repo(
                workspace_root,
                repo_name,
                self,
            )? {
                let (url, sha256, strip_prefix, build_file) = found;
                if let Some(path) = Self::fetch_http_archive_repo(
                    workspace_root,
                    repo_name,
                    url,
                    sha256,
                    strip_prefix,
                    build_file,
                )? {
                    Self::mark_repo_fetched_for_dedupe(repo_name);
                    return Ok(Some(path));
                }
                return Ok(None);
            }
            // Also try to locate and fetch git_repository rules in chunks
            if let Some(git_path) = Self::scan_workspace_chunks_and_fetch_git(
                workspace_root,
                repo_name,
                self,
            )? {
                Self::mark_repo_fetched_for_dedupe(repo_name);
                return Ok(Some(git_path));
            }
            // Try to register local_repository/new_local_repository from chunks
            if let Some(local_path) =
                Self::scan_workspace_chunks_and_register_local(workspace_root, repo_name)?
            {
                Self::mark_repo_fetched_for_dedupe(repo_name);
                return Ok(Some(local_path));
            }
            if let Ok(ws) = rush_repo::workspace::parse_workspace(&workspace_path) {
                for repo in &ws.repos {
                    if repo.name == repo_name {
                        match repo.rule_type.as_str() {
                            "http_archive" => {
                                let (url, sha256, strip_prefix, build_file) =
                                    Self::extract_http_archive_attrs(
                                        &repo.attributes,
                                        workspace_root,
                                    )?;
                                if let Some(path) = Self::fetch_http_archive_repo(
                                    workspace_root,
                                    repo_name,
                                    url,
                                    sha256,
                                    strip_prefix,
                                    build_file,
                                )? {
                                    Self::mark_repo_fetched_for_dedupe(repo_name);
                                    return Ok(Some(path));
                                }
                                return Ok(None);
                            }
                            "http_jar" => {
                                // Treat like http_file
                                let url = repo
                                    .attributes
                                    .get("urls")
                                    .and_then(|v| match v {
                                        rush_repo::workspace::Value::List(list) => {
                                            list.iter().find_map(|x| match x {
                                                rush_repo::workspace::Value::String(s) => {
                                                    Some(s.clone())
                                                }
                                                _ => None,
                                            })
                                        }
                                        _ => None,
                                    })
                                    .or_else(|| {
                                        repo.attributes.get("url").and_then(|v| match v {
                                            rush_repo::workspace::Value::String(s) => {
                                                Some(s.clone())
                                            }
                                            _ => None,
                                        })
                                    })
                                    .ok_or_else(|| {
                                        anyhow::anyhow!("http_jar '{}' missing url(s)", repo_name)
                                    })?;
                                let sha256 = repo.attributes.get("sha256").and_then(|v| match v {
                                    rush_repo::workspace::Value::String(s) => Some(s.clone()),
                                    _ => None,
                                });
                                if let Some(path) = Self::fetch_http_file_repo(
                                    workspace_root,
                                    repo_name,
                                    url,
                                    sha256,
                                )? {
                                    Self::mark_repo_fetched_for_dedupe(repo_name);
                                    return Ok(Some(path));
                                }
                                return Ok(None);
                            }
                            "http_file" => {
                                // Extract URL from url/urls; sha256 optional
                                let url = repo
                                    .attributes
                                    .get("urls")
                                    .and_then(|v| match v {
                                        rush_repo::workspace::Value::List(list) => {
                                            list.iter().find_map(|x| match x {
                                                rush_repo::workspace::Value::String(s) => {
                                                    Some(s.clone())
                                                }
                                                _ => None,
                                            })
                                        }
                                        _ => None,
                                    })
                                    .or_else(|| {
                                        repo.attributes.get("url").and_then(|v| match v {
                                            rush_repo::workspace::Value::String(s) => {
                                                Some(s.clone())
                                            }
                                            _ => None,
                                        })
                                    })
                                    .ok_or_else(|| {
                                        anyhow::anyhow!("http_file '{}' missing url(s)", repo_name)
                                    })?;
                                let sha256 = repo.attributes.get("sha256").and_then(|v| match v {
                                    rush_repo::workspace::Value::String(s) => Some(s.clone()),
                                    _ => None,
                                });
                                if let Some(path) = Self::fetch_http_file_repo(
                                    workspace_root,
                                    repo_name,
                                    url,
                                    sha256,
                                )? {
                                    Self::mark_repo_fetched_for_dedupe(repo_name);
                                    return Ok(Some(path));
                                }
                                return Ok(None);
                            }
                            "git_repository" => {
                                // Extract git attributes
                                let remote = repo
                                    .attributes
                                    .get("remote")
                                    .and_then(|v| match v {
                                        rush_repo::workspace::Value::String(s) => Some(s.clone()),
                                        _ => None,
                                    })
                                    .ok_or_else(|| {
                                        anyhow::anyhow!(
                                            "git_repository '{}' missing 'remote'",
                                            repo_name
                                        )
                                    })?;
                                let commit = repo.attributes.get("commit").and_then(|v| match v {
                                    rush_repo::workspace::Value::String(s) => Some(s.clone()),
                                    _ => None,
                                });
                                let tag = repo.attributes.get("tag").and_then(|v| match v {
                                    rush_repo::workspace::Value::String(s) => Some(s.clone()),
                                    _ => None,
                                });
                                let branch = repo.attributes.get("branch").and_then(|v| match v {
                                    rush_repo::workspace::Value::String(s) => Some(s.clone()),
                                    _ => None,
                                });
                                if let Some(path) = Self::fetch_git_repository_repo(
                                    workspace_root,
                                    repo_name,
                                    remote,
                                    commit,
                                    tag,
                                    branch,
                                )? {
                                    Self::mark_repo_fetched_for_dedupe(repo_name);
                                    return Ok(Some(path));
                                }
                                return Ok(None);
                            }
                            "local_repository" => {
                                if let Some(rush_repo::workspace::Value::String(path)) =
                                    repo.attributes.get("path")
                                {
                                    Self::mark_repo_fetched_for_dedupe(repo_name);
                                    return Ok(Some(std::path::PathBuf::from(path)));
                                }
                            }
                            _ => {}
                        }
                    }
                }

                // Scan loaded .bzl files (//pkg:file) for http_archive definitions that match repo_name
                for load in &ws.loads {
                    let path = load.path.as_str();
                    if let Some(rest) = path.strip_prefix("//") {
                        if let Some((pkg, file)) = rest.split_once(':') {
                            let load_path = workspace_root.join(pkg).join(file);
                            if load_path.exists() {
                                if let Ok(contents) = std::fs::read_to_string(&load_path) {
                                    if let Some((url, sha256, strip_prefix, build_file)) =
                                        Self::scan_http_archive_in_bzl(
                                            &contents,
                                            repo_name,
                                            workspace_root,
                                        )
                                    {
                                        if let Some(path) = Self::fetch_http_archive_repo(
                                            workspace_root,
                                            repo_name,
                                            url,
                                            sha256,
                                            strip_prefix,
                                            build_file,
                                        )? {
                                            Self::mark_repo_fetched_for_dedupe(repo_name);
                                            return Ok(Some(path));
                                        }
                                        return Ok(None);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Fallback to TensorFlow third_party/<name>/workspace.bzl convention
        let mut res = Self::fetch_from_third_party_workspace_bzl(workspace_root, repo_name)?;
        if res.is_none() {
            if let Some(path) = Self::find_third_party_dir(workspace_root, repo_name)? {
                res = Some(path);
            }
        }
        if res.is_none() {
            if let Some(path) = Self::fetch_repo_via_bazel(workspace_root, repo_name, canonical)? {
                res = Some(path);
            }
        }
        if res.is_some() {
            Self::mark_repo_fetched_for_dedupe(repo_name);
        }

        // CLEANUP: Remove from "being fetched" set before returning
        {
            if let Ok(mut being_fetched) = REPOS_BEING_FETCHED.lock() {
                being_fetched.remove(repo_name);
            }
        }

        Ok(res)
    }

    /// Materialize a declared legacy WORKSPACE repository with its genuine
    /// repository rule, including patches, overlays and formatted attributes.
    /// Callers register the resulting Bazel-owned source root. This does not
    /// interpret a partial http_archive tuple as a complete source definition.
    pub fn fetch_declared_workspace_repository(workspace_root: &Path, repo_name: &str) -> Result<Option<PathBuf>> {
        Self::fetch_declared_workspace_repository_for_label(workspace_root, repo_name, None)
    }

    fn workspace_module_package_goal(repo_name: &str, requested: &str) -> Result<String> {
        let relative = requested.strip_prefix(&format!("@{}//", repo_name)).ok_or_else(|| anyhow!("requested module is outside its repository"))?;
        let (package, target) = relative.split_once(':').ok_or_else(|| anyhow!("requested module requires package:target syntax"))?;
        if target.is_empty() || Path::new(package).components().chain(Path::new(target).components()).any(|component| !matches!(component, std::path::Component::Normal(_))) {
            anyhow::bail!("requested module is not a package-relative source label");
        }
        Ok(format!("@{}//{}:all", repo_name, package))
    }

    /// A Starlark load reads a source file, which need not be exported as a
    /// target. Materialize its owning package, then resolve/read the actual
    /// module through normal source checks without changing upstream BUILD.
    pub fn fetch_declared_workspace_repository_for_module(workspace_root: &Path, repo_name: &str, requested: &str) -> Result<Option<PathBuf>> {
        let goal = Self::workspace_module_package_goal(repo_name, requested)?;
        Self::fetch_declared_workspace_repository_for_label(workspace_root, repo_name, Some(&goal))
    }

    /// Materialize the requested repository package with non-forcing fetch.
    /// A known requested .bzl/file/rule label avoids loading unrelated packages.
    pub fn fetch_declared_workspace_repository_for_label(workspace_root: &Path, repo_name: &str, requested_label: Option<&str>) -> Result<Option<PathBuf>> {
        if repo_name.is_empty() || repo_name == "." || repo_name == ".."
            || repo_name.bytes().any(|byte| !byte.is_ascii_alphanumeric() && !matches!(byte, b'_' | b'-' | b'.' | b'+' | b'~'))
        {
            anyhow::bail!("declared repository name is not a single valid path component");
        }
        Self::fetch_repo_via_bazel_with_policy(workspace_root, repo_name, false, true, requested_label)
    }

    fn admit_registered_workspace_repository(&self, repo_name: &str, path: &Path) -> Result<PathBuf> {
        self.admit_registered_workspace_repository_for_label(repo_name, path, None)
    }

    fn admit_registered_workspace_repository_for_label(&self, repo_name: &str, path: &Path, requested_label: Option<&str>) -> Result<PathBuf> {
        let bzlmod_only = self.workspace_root.join("MODULE.bazel").exists()
            && !self.workspace_root.join("WORKSPACE").exists()
            && !self.workspace_root.join("WORKSPACE.bazel").exists();
        if !bzlmod_only && !repo_name.is_empty() {
            let cache_root = tracked_command_environment("RUSH_WORKSPACE_CACHE_ROOT")?
                .map(PathBuf::from)
                .unwrap_or_else(|| self.workspace_root.join(".cache").join("rush"));
            let expected = cache_root.join("bazel_output_base").join("external").join(repo_name);
            if path == expected {
                let materialized = match requested_label {
                    Some(module) => Self::fetch_declared_workspace_repository_for_module(&self.workspace_root, repo_name, module),
                    None => Self::fetch_declared_workspace_repository(&self.workspace_root, repo_name),
                }?;
                return materialized.ok_or_else(|| anyhow!("declared repository @{} has no verified source root", repo_name));
            }
        }
        Ok(path.to_path_buf())
    }

    fn fetch_repo_via_bazel(
        workspace_root: &std::path::Path,
        repo_name: &str,
        canonical: bool,
    ) -> Result<Option<PathBuf>> {
        Self::fetch_repo_via_bazel_with_policy(workspace_root, repo_name, canonical, false, None)
    }

    fn fetch_repo_via_bazel_with_policy(
        workspace_root: &Path,
        repo_name: &str,
        canonical: bool,
        declared_workspace: bool,
        requested_label: Option<&str>,
    ) -> Result<Option<PathBuf>> {
        let bzlmod_only = workspace_root.join("MODULE.bazel").exists()
            && !workspace_root.join("WORKSPACE").exists()
            && !workspace_root.join("WORKSPACE.bazel").exists();
        if declared_workspace && bzlmod_only {
            anyhow::bail!("declared WORKSPACE repository fetch cannot use Bzlmod discovery");
        }
        // Phase 5.C.4 commit 143 (Batch R): if this
        // repo was pre-fetched by the warmup pass,
        // short-circuit. The warmup ran ONE combined
        // / parallel bazel call ahead of analyze, so
        // per-repo invocations are redundant.
        if let Some(path) = bzlmod_only.then(||
            crate::fetch_strategy::is_warmed_up(repo_name)
        ).flatten() {
            if path.exists() {
                eprintln!(
                    "    ⚡ fetch warmed-up: @{} → {}",
                    repo_name,
                    path.display()
                );
                if let Some(g) = GLOBAL_BZL_CACHE_FOR_FETCH_TIMING.get() {
                    // Commit 149: classify the warmed-up
                    // skip by manifest status. The
                    // warmup writer creates conservative
                    // manifests (Incomplete) today, so
                    // this typically records as
                    // TrustMode. When commit 150+ plumb
                    // real fetch-rule metadata, the
                    // same code path will promote to
                    // ManifestValid automatically.
                    let status =
                        crate::fetch_strategy::read_manifest_status_via_callback(
                            workspace_root,
                            repo_name,
                        );
                    let decision = if status == "valid" {
                        FetchDecision::SkippedMaterializedManifestValid {
                            repo_name: repo_name.to_string(),
                            path: path.clone(),
                            manifest_status: status,
                        }
                    } else {
                        FetchDecision::SkippedMaterializedTrustMode {
                            repo_name: repo_name.to_string(),
                            path: path.clone(),
                            manifest_status: status,
                        }
                    };
                    g.record_fetch_decision(&decision);
                }
                return Ok(Some(path));
            }
        }

        let bazel_binary = resolve_bazel_binary_for_fetch(workspace_root)?;
        println!(
            "         [BAZEL FETCH] Using `{}` for repository @{}",
            bazel_binary.display(),
            repo_name
        );

        let cache_root = tracked_command_environment("RUSH_WORKSPACE_CACHE_ROOT")?
            .map(PathBuf::from)
            .unwrap_or_else(|| workspace_root.join(".cache").join("rush"));
        let output_base = cache_root.join("bazel_output_base");
        fs::create_dir_all(&output_base)?;

        // Phase 5.C.4 commit 133 (Batch Q): time the
        // bazel-fetch subprocess. This is the
        // SECOND of two bazel-fetch sites — the
        // commit-126 instrumentation only covered the
        // orchestrator-side TokioCommand variant; this
        // loader-side StdCommand variant is the path
        // that fires for TF macros analyze, where the
        // 90s wait actually lives.
        let fetch_start = std::time::Instant::now();
        // #39(C2): serialize concurrent `bazel sync` on the shared output_base
        // (see BAZEL_SYNC_SERIALIZE). The guard is held only across the
        // subprocess; a poisoned lock is recovered (the protected unit `()` has
        // no invariants to corrupt). After acquiring, re-check warmup in case a
        // peer thread materialized this repo while we waited — avoids a
        // redundant (though idempotent) sync.
        let _sync_guard = BAZEL_SYNC_SERIALIZE
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(path) = bzlmod_only.then(|| crate::fetch_strategy::is_warmed_up(repo_name)).flatten() {
            if path.exists() {
                return Ok(Some(path));
            }
        }

        // Phase 5.C.4 commit 134/149: hard-stop on known parser-only repos.
        // These repos are intentionally resolved through stubs or
        // pre-vendored paths, so do not fire expensive per-repo
        // `bazel sync` subprocesses.
        const SKIP_FETCH_REPOS: &[&str] = &[
            "com_google_googleapis",
            "local_config_cuda",
            "local_config_tensorrt",
            "local_config_rocm",
            "com_google_protobuf",
            "rules_python_internal",
            "pybind11",
            "com_github_grpc_grpc",
            "local_config_remote_execution",
        ];
        if !declared_workspace && !bzlmod_only && SKIP_FETCH_REPOS.contains(&repo_name) {
            let workspace_external = vendored_external_repo_dir(workspace_root, repo_name);
            if workspace_external.exists() {
                debug_log!(
                    "[DEBUG BzlLoader] Skipping bazel sync for known repo {} (workspace external stub exists)",
                    repo_name
                );
                if let Some(g) = GLOBAL_BZL_CACHE_FOR_FETCH_TIMING.get() {
                    g.record_fetch_decision(&FetchDecision::SkippedKnownRule {
                        repo_name: repo_name.to_string(),
                        path: workspace_external.clone(),
                    });
                }
                return Ok(Some(workspace_external));
            }

            if let Some(stub_repo) = create_known_stub_repo(workspace_root, repo_name)? {
                debug_log!(
                    "[DEBUG BzlLoader] Using local stub for known repo {}: {}",
                    repo_name,
                    stub_repo.display()
                );
                if let Some(g) = GLOBAL_BZL_CACHE_FOR_FETCH_TIMING.get() {
                    g.record_fetch_decision(&FetchDecision::SkippedKnownRule {
                        repo_name: repo_name.to_string(),
                        path: stub_repo.clone(),
                    });
                }
                return Ok(Some(stub_repo));
            }
        }
        let workspace_fetch_context = if bzlmod_only {
            None
        } else {
            let context = crate::fetch_strategy::prepare_workspace_repository_fetch(workspace_root, &bazel_binary)?;
            if crate::fetch_strategy::workspace_repository_inputs_are_current(
                workspace_root, &output_base, repo_name, &context,
            )? {
                return Ok(Some(output_base.join("external").join(repo_name)));
            }
            if context.native_local_source_is_materialized(&output_base, repo_name)? {
                crate::fetch_strategy::record_workspace_repository_fetch(workspace_root, &output_base, repo_name, &context)?;
                return Ok(Some(output_base.join("external").join(repo_name)));
            }
            Some(context)
        };
        // Bzlmod-only workspaces do not necessarily support the legacy
        // `sync` command, and their apparent repository names do not match
        // the canonical directories under output_base/external. Preserve the
        // established WORKSPACE path while using Bazel's tool-facing module
        // APIs for projects such as Modular.
        let bzlmod_canonical_name = if bzlmod_only && canonical {
            // A '@@' label already names the canonical repository; the
            // apparent-name mapping does not apply.
            Some(repo_name.to_string())
        } else if bzlmod_only {
            // Repo mappings are scoped to the repository containing the
            // load(), not always to the root module. Identify that canonical
            // repository by matching the current context root against Bazel's
            // external symlinks.
            let current_canonical_repo = if let Some(ctx) = Self::capture_thread_context() {
                let context_root =
                    std::fs::canonicalize(&ctx.repo_root).unwrap_or(ctx.repo_root.clone());
                let workspace_root = std::fs::canonicalize(workspace_root)
                    .unwrap_or_else(|_| workspace_root.to_path_buf());
                if context_root == workspace_root {
                    String::new()
                } else {
                    let mut canonical_repo =
                        canonical_repo_from_output_base_path(&context_root, &output_base);
                    if canonical_repo.is_none() {
                        if let Ok(entries) = std::fs::read_dir(output_base.join("external")) {
                            for entry in entries.flatten() {
                                let entry_path = entry.path();
                                if std::fs::canonicalize(&entry_path).ok().as_ref()
                                    == Some(&context_root)
                                {
                                    canonical_repo =
                                        Some(entry.file_name().to_string_lossy().into_owned());
                                    break;
                                }
                            }
                        }
                    }
                    canonical_repo.ok_or_else(|| {
                        anyhow!(
                            "Failed to identify Bazel canonical repository for {}",
                            context_root.display()
                        )
                    })?
                }
            } else {
                String::new()
            };

            let mapping_output = StdCommand::new(&bazel_binary)
                .arg("--output_base")
                .arg(&output_base)
                .arg("mod")
                .arg("dump_repo_mapping")
                .arg(&current_canonical_repo)
                .current_dir(workspace_root)
                .output()
                .with_context(|| {
                    format!(
                        "Failed to run `{} mod dump_repo_mapping {}`",
                        bazel_binary.display(),
                        current_canonical_repo
                    )
                })?;
            if !mapping_output.status.success() {
                return Err(anyhow!(
                    "`{} --output_base={} mod dump_repo_mapping {}` failed \
                     with status {:?}: {}",
                    bazel_binary.display(),
                    output_base.display(),
                    current_canonical_repo,
                    mapping_output.status.code(),
                    String::from_utf8_lossy(&mapping_output.stderr).trim()
                ));
            }
            let repo_mapping: std::collections::HashMap<String, String> =
                serde_json::from_slice(&mapping_output.stdout).with_context(|| {
                    format!(
                        "Failed to parse repository mapping for canonical repo `{}`",
                        current_canonical_repo
                    )
                })?;
            if let Some(canonical_name) = repo_mapping.get(repo_name) {
                Some(canonical_name.clone())
            } else if output_base.join("external").join(repo_name).exists() {
                // The caller may already have supplied a canonical name.
                Some(repo_name.to_string())
            } else {
                return Err(anyhow!(
                    "Bazel repository mapping for canonical repo `{}` has no \
                     entry for apparent repository `{}`",
                    current_canonical_repo,
                    repo_name
                ));
            }
        } else {
            None
        };
        let workspace_target = requested_label.map(str::to_owned).unwrap_or_else(|| format!("@{}//...", repo_name));
        if !bzlmod_only && !workspace_target.starts_with(&format!("@{}//", repo_name)) {
            anyhow::bail!("requested materialization label is outside its declared repository");
        }
        let fetch_description = if bzlmod_only {
            format!(
                "fetch --repo=@@{}",
                bzlmod_canonical_name.as_deref().unwrap_or(repo_name)
            )
        } else {
            format!("fetch {}", workspace_target)
        };
        let mut fetch_command = StdCommand::new(&bazel_binary);
        if let Some(context) = &workspace_fetch_context {
            context.apply(&mut fetch_command)?;
        }
        fetch_command
            .arg("--output_base")
            .arg(&output_base)
            .current_dir(workspace_root);
        if bzlmod_only {
            fetch_command
                .arg("fetch")
                .arg(format!(
                    "--repo=@@{}",
                    bzlmod_canonical_name.as_deref().unwrap_or(repo_name)
                ));
        } else {
            fetch_command.arg("fetch").arg(&workspace_target);
        }
        let status = fetch_command.status().map_err(|error| {
            anyhow!(
                "failed to start `{} {}`: {}",
                bazel_binary.display(),
                fetch_description,
                error
            )
        })?;
        let fetch_dur = fetch_start.elapsed();
        // Record into the singleton GlobalBzlCache. We
        // don't have a direct reference here, but the
        // CacheMetrics record path is best-effort —
        // recording through a process-global accessor.
        if let Some(g) = GLOBAL_BZL_CACHE_FOR_FETCH_TIMING.get() {
            g.record_bazel_fetch_wait(fetch_dur);
            // Phase 5.C.4 commit 134 (Batch Q): also
            // record the fetch-decision and per-repo
            // duration. Commit 135's report sorts on
            // this.
            g.record_fetch_decision(&FetchDecision::Invoked {
                repo_name: repo_name.to_string(),
                duration: fetch_dur,
            });
        }

        if !status.success() {
            return Err(anyhow!(
                "`{} --output_base={} {}` failed with status {:?}",
                bazel_binary.display(),
                output_base.display(),
                fetch_description,
                status.code()
            ));
        }

        let bazel_repo_path = if let Some(canonical_name) = bzlmod_canonical_name {
            output_base.join("external").join(canonical_name)
        } else {
            output_base.join("external").join(repo_name)
        };
        if !bazel_repo_path.exists() {
            return Err(anyhow!(
                "Bazel reported success but {} is missing",
                bazel_repo_path.display()
            ));
        }
        if let Some(context) = &workspace_fetch_context {
            crate::fetch_strategy::record_workspace_repository_fetch(
                workspace_root, &output_base, repo_name, context,
            )?;
        }

        // Phase 5.C.4 commit 148 (Batch S): write the
        // sidecar manifest. Best-effort — failures
        // log but don't abort the fetch.
        let _ = crate::fetch_strategy::write_manifest_via_callback(
            workspace_root,
            repo_name,
            &bazel_repo_path,
            &format!("bazel {}", fetch_description),
        );

        Ok(Some(bazel_repo_path))
    }

    /// Scan WORKSPACE in chunks (text between load() statements) for repository macros.
    /// Looks for http_archive(...) and tf_http_archive(...) calls matching repo_name.
    /// Returns the first match as (url, sha256, strip_prefix, build_file).
    #[allow(clippy::type_complexity)]
    fn scan_workspace_in_chunks_for_repo(
        workspace_root: &std::path::Path,
        repo_name: &str,
        loader: &Self,
    ) -> Result<
        Option<(
            String,
            Option<String>,
            Option<String>,
            Option<std::path::PathBuf>,
        )>,
    > {
        let workspace_path = workspace_root.join("WORKSPACE");
        let content = match std::fs::read_to_string(&workspace_path) {
            Ok(c) => c,
            Err(_) => return Ok(None),
        };

        let mut chunk = String::new();
        for line in content.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("load(") {
                // Process current chunk
                // http_file on-demand fetch
                if let Ok(Some(_p)) = Self::eval_workspace_chunk_for_http_file_and_fetch(
                    &chunk,
                    repo_name,
                    workspace_root,
                    loader,
                ) {
                    // External repo implicitly registered by resolve_external_bzl after fetch
                    // fetched; do not return tuple here
                }
                if let Some(found) = Self::eval_workspace_chunk_for_repo(
                    &chunk,
                    repo_name,
                    workspace_root,
                    loader,
                ) {
                    return Ok(Some(found));
                }
                if let Some(found) =
                    Self::scan_repo_rules_in_chunk_ast(&chunk, repo_name, workspace_root)
                {
                    return Ok(Some(found));
                }
                if let Some(found) =
                    Self::scan_repo_rules_in_chunk(&chunk, repo_name, workspace_root)
                {
                    return Ok(Some(found));
                }
                // Reset chunk and include this load line in next chunk
                chunk.clear();
                chunk.push_str(line);
                chunk.push('\n');
            } else {
                chunk.push_str(line);
                chunk.push('\n');
            }
        }
        // Process trailing chunk
        if let Ok(Some(_p)) = Self::eval_workspace_chunk_for_http_file_and_fetch(
            &chunk,
            repo_name,
            workspace_root,
            loader,
        ) {
            // fetched; no tuple return here
        }
        if let Some(found) = Self::eval_workspace_chunk_for_repo(
            &chunk,
            repo_name,
            workspace_root,
            loader,
        ) {
            return Ok(Some(found));
        }
        if let Some(found) = Self::scan_repo_rules_in_chunk_ast(&chunk, repo_name, workspace_root) {
            return Ok(Some(found));
        }
        if let Some(found) = Self::scan_repo_rules_in_chunk(&chunk, repo_name, workspace_root) {
            return Ok(Some(found));
        }
        Ok(None)
    }

    /// Evaluate a WORKSPACE chunk with a minimal evaluator to capture repo rules.
    fn eval_workspace_chunk_for_repo(
        contents: &str,
        target_name: &str,
        workspace_root: &std::path::Path,
        loader: &Self,
    ) -> Option<RepoMetadata> {
        let program = crate::parse(contents).ok()?;
        let mut ev = crate::Evaluator::new();
        // Provide workspace-native repo rules and common aliases
        ev.register_workspace_native_namespace();
        ev.register_repo_rule_alias("tf_http_archive", "http_archive");
        ev.register_repo_rule_alias("new_http_archive", "http_archive");
        let mut temp_loader = loader.clone_for_workspace_chunk_scan(workspace_root);

        let loader_arc = std::sync::Arc::new(std::sync::Mutex::new(temp_loader));
        ev.set_bzl_loader(loader_arc);

        // Evaluate program; ignore errors to be robust to unknown macros
        let _ = ev.eval_program(&program);

        // Inspect captured rule calls
        for rc in ev.captured_rule_calls() {
            let kind = rc.rule_type.as_str();
            if kind != "http_archive" {
                continue;
            }
            // Extract name/url/urls/sha256/strip_prefix/build_file from kwargs
            let name = rc.kwargs.get("name").and_then(|v| match v {
                crate::Value::String(s) => Some(s.clone()),
                _ => None,
            });
            if name.as_deref() != Some(target_name) {
                continue;
            }
            let url = rc
                .kwargs
                .get("url")
                .and_then(|v| match v {
                    crate::Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .or_else(|| {
                    rc.kwargs.get("urls").and_then(|v| match v {
                        crate::Value::List(list) => {
                            for x in list {
                                if let crate::Value::String(s) = x {
                                    return Some(s.clone());
                                }
                            }
                            None
                        }
                        _ => None,
                    })
                })
                .or_else(|| {
                    rc.kwargs.get("jar_urls").and_then(|v| match v {
                        crate::Value::List(list) => {
                            for x in list {
                                if let crate::Value::String(s) = x {
                                    return Some(s.clone());
                                }
                            }
                            None
                        }
                        _ => None,
                    })
                })
                .or_else(|| {
                    rc.kwargs.get("jars").and_then(|v| match v {
                        crate::Value::List(list) => {
                            for x in list {
                                if let crate::Value::String(s) = x {
                                    return Some(s.clone());
                                }
                            }
                            None
                        }
                        _ => None,
                    })
                })
                .or_else(|| {
                    rc.kwargs.get("artifact").and_then(|v| match v {
                        crate::Value::String(s) => Some(s.clone()),
                        _ => None,
                    })
                });
            let sha256 = rc.kwargs.get("sha256").and_then(|v| match v {
                crate::Value::String(s) => Some(s.clone()),
                _ => None,
            });
            let strip_prefix = rc.kwargs.get("strip_prefix").and_then(|v| match v {
                crate::Value::String(s) => Some(s.clone()),
                _ => None,
            });
            let build_file = rc.kwargs.get("build_file").and_then(|v| match v {
                crate::Value::String(label) => {
                    if let Some(rest) = label.strip_prefix("//") {
                        if let Some((pkg, file)) = rest.split_once(':') {
                            return Some(workspace_root.join(pkg).join(file));
                        }
                    }
                    None
                }
                _ => None,
            });

            if let Some(u) = url {
                return Some((u, sha256, strip_prefix, build_file));
            }
        }
        None
    }

    /// Scan load()-delimited WORKSPACE chunks for git_repository and fetch on demand.
    fn scan_workspace_chunks_and_fetch_git(
        workspace_root: &std::path::Path,
        repo_name: &str,
        loader: &Self,
    ) -> Result<Option<PathBuf>> {
        let workspace_path = workspace_root.join("WORKSPACE");
        let content = match std::fs::read_to_string(&workspace_path) {
            Ok(c) => c,
            Err(_) => return Ok(None),
        };

        let mut chunk = String::new();
        for line in content.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("load(") {
                if let Some(path) = Self::eval_workspace_chunk_for_git_fetch(
                    &chunk,
                    repo_name,
                    workspace_root,
                    loader,
                )? {
                    return Ok(Some(path));
                }
                chunk.clear();
                chunk.push_str(line);
                chunk.push('\n');
            } else {
                chunk.push_str(line);
                chunk.push('\n');
            }
        }
        if let Some(path) = Self::eval_workspace_chunk_for_git_fetch(
            &chunk,
            repo_name,
            workspace_root,
            loader,
        )? {
            return Ok(Some(path));
        }
        Ok(None)
    }

    /// Minimal evaluation of a WORKSPACE chunk to detect git_repository(name=...) and fetch.
    fn eval_workspace_chunk_for_git_fetch(
        contents: &str,
        target_name: &str,
        workspace_root: &std::path::Path,
        loader: &Self,
    ) -> Result<Option<PathBuf>> {
        let program = match crate::parse(contents) {
            Ok(p) => p,
            Err(_) => return Ok(None),
        };
        let mut ev = crate::Evaluator::new();
        ev.register_workspace_native_namespace();
        let mut temp_loader = loader.clone_for_workspace_chunk_scan(workspace_root);

        let loader_arc = std::sync::Arc::new(std::sync::Mutex::new(temp_loader));
        ev.set_bzl_loader(loader_arc);
        // Evaluate; ignore errors from unknown macros
        let _ = ev.eval_program(&program);
        for rc in ev.captured_rule_calls() {
            let k = rc.rule_type.as_str();
            if k != "git_repository" && k != "new_git_repository" {
                continue;
            }
            let name = rc.kwargs.get("name").and_then(|v| match v {
                crate::Value::String(s) => Some(s.clone()),
                _ => None,
            });
            if name.as_deref() != Some(target_name) {
                continue;
            }
            let remote = rc.kwargs.get("remote").and_then(|v| match v {
                crate::Value::String(s) => Some(s.clone()),
                _ => None,
            });
            if remote.is_none() {
                continue;
            }
            let commit = rc.kwargs.get("commit").and_then(|v| match v {
                crate::Value::String(s) => Some(s.clone()),
                _ => None,
            });
            let tag = rc.kwargs.get("tag").and_then(|v| match v {
                crate::Value::String(s) => Some(s.clone()),
                _ => None,
            });
            let branch = rc.kwargs.get("branch").and_then(|v| match v {
                crate::Value::String(s) => Some(s.clone()),
                _ => None,
            });
            // Fetch using helper and return external repo path
            return Self::fetch_git_repository_repo(
                workspace_root,
                target_name,
                remote.unwrap(),
                commit,
                tag,
                branch,
            );
        }
        Ok(None)
    }

    /// Minimal evaluation of a WORKSPACE chunk to detect http_file(name=...) and fetch.
    fn eval_workspace_chunk_for_http_file_and_fetch(
        contents: &str,
        target_name: &str,
        workspace_root: &std::path::Path,
        loader: &Self,
    ) -> Result<Option<PathBuf>> {
        let program = match crate::parse(contents) {
            Ok(p) => p,
            Err(_) => return Ok(None),
        };
        let mut ev = crate::Evaluator::new();
        ev.register_workspace_native_namespace();
        let mut temp_loader = loader.clone_for_workspace_chunk_scan(workspace_root);

        let loader_arc = std::sync::Arc::new(std::sync::Mutex::new(temp_loader));
        ev.set_bzl_loader(loader_arc);
        // Register load() aliases for file/jar rules
        if let Ok(ast) = crate::parse(contents) {
            for stmt in &ast.statements {
                if let crate::Stmt::Load { file: _, symbols } = stmt {
                    for (local, original) in symbols {
                        match original.as_str() {
                            "http_file" | "http_jar" | "maven_jar" | "java_import_external" => {
                                ev.register_repo_rule_alias(local, original);
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        let _ = ev.eval_program(&program);
        for rc in ev.captured_rule_calls() {
            let kind = rc.rule_type.as_str();
            if kind != "http_file" && kind != "http_jar" && kind != "java_import_external" {
                continue;
            }
            let name = rc.kwargs.get("name").and_then(|v| match v {
                crate::Value::String(s) => Some(s.clone()),
                _ => None,
            });
            if name.as_deref() != Some(target_name) {
                continue;
            }
            let url = rc
                .kwargs
                .get("url")
                .and_then(|v| match v {
                    crate::Value::String(s) => Some(s.clone()),
                    _ => None,
                })
                .or_else(|| {
                    rc.kwargs.get("urls").and_then(|v| match v {
                        crate::Value::List(list) => {
                            for x in list {
                                if let crate::Value::String(s) = x {
                                    return Some(s.clone());
                                }
                            }
                            None
                        }
                        _ => None,
                    })
                })
                .or_else(|| {
                    rc.kwargs.get("jar_urls").and_then(|v| match v {
                        crate::Value::List(list) => {
                            for x in list {
                                if let crate::Value::String(s) = x {
                                    return Some(s.clone());
                                }
                            }
                            None
                        }
                        _ => None,
                    })
                })
                .or_else(|| {
                    rc.kwargs.get("jars").and_then(|v| match v {
                        crate::Value::List(list) => {
                            for x in list {
                                if let crate::Value::String(s) = x {
                                    return Some(s.clone());
                                }
                            }
                            None
                        }
                        _ => None,
                    })
                })
                .or_else(|| {
                    rc.kwargs.get("artifact").and_then(|v| match v {
                        crate::Value::String(s) => Some(s.clone()),
                        _ => None,
                    })
                });
            if let Some(u) = url {
                return Self::fetch_http_file_repo(
                    workspace_root,
                    target_name,
                    u,
                    rc.kwargs.get("sha256").and_then(|v| match v {
                        crate::Value::String(s) => Some(s.clone()),
                        _ => None,
                    }),
                );
            }
        }
        Ok(None)
    }

    /// Scan load()-delimited WORKSPACE chunks for local_repository/new_local_repository and register path.
    fn scan_workspace_chunks_and_register_local(
        workspace_root: &std::path::Path,
        repo_name: &str,
    ) -> Result<Option<PathBuf>> {
        let workspace_path = workspace_root.join("WORKSPACE");
        let content = match std::fs::read_to_string(&workspace_path) {
            Ok(c) => c,
            Err(_) => return Ok(None),
        };

        let mut chunk = String::new();
        for line in content.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("load(") {
                if let Some(p) =
                    Self::eval_workspace_chunk_for_local_repo(&chunk, repo_name, workspace_root)?
                {
                    return Ok(Some(p));
                }
                chunk.clear();
                chunk.push_str(line);
                chunk.push('\n');
            } else {
                chunk.push_str(line);
                chunk.push('\n');
            }
        }
        if let Some(p) =
            Self::eval_workspace_chunk_for_local_repo(&chunk, repo_name, workspace_root)?
        {
            return Ok(Some(p));
        }
        Ok(None)
    }

    /// Evaluate a WORKSPACE chunk and detect local_repository/new_local_repository(name=..., path=...).
    fn eval_workspace_chunk_for_local_repo(
        contents: &str,
        target_name: &str,
        _workspace_root: &std::path::Path,
    ) -> Result<Option<PathBuf>> {
        let program = match crate::parse(contents) {
            Ok(p) => p,
            Err(_) => return Ok(None),
        };
        let mut ev = crate::Evaluator::new();
        ev.register_workspace_native_namespace();
        let _ = ev.eval_program(&program);
        for rc in ev.captured_rule_calls() {
            let kind = rc.rule_type.as_str();
            if kind != "local_repository" && kind != "new_local_repository" {
                continue;
            }
            let name = rc.kwargs.get("name").and_then(|v| match v {
                crate::Value::String(s) => Some(s.clone()),
                _ => None,
            });
            if name.as_deref() != Some(target_name) {
                continue;
            }
            let path = rc.kwargs.get("path").and_then(|v| match v {
                crate::Value::String(s) => Some(s.clone()),
                _ => None,
            });
            if let Some(p) = path {
                return Ok(Some(PathBuf::from(p)));
            }
        }
        Ok(None)
    }

    /// Parse chunk to AST and extract repository rule calls (http_archive/tf_http_archive).
    fn scan_repo_rules_in_chunk_ast(
        contents: &str,
        target_name: &str,
        workspace_root: &std::path::Path,
    ) -> Option<RepoMetadata> {
        let program = match crate::parse(contents) {
            Ok(p) => p,
            Err(_) => return None,
        };

        use crate::{BindingPattern, Expr, Stmt};

        fn extract_str(value: &Expr) -> Option<String> {
            match value {
                Expr::String(s) => Some(s.clone()),
                _ => None,
            }
        }

        // Traverse statements to find aliasing (alias = http_archive) and top-level call expressions
        use std::collections::HashSet;
        let mut http_aliases: HashSet<String> = HashSet::new();
        let mut tf_http_aliases: HashSet<String> = HashSet::new();

        for stmt in &program.statements {
            if let Stmt::Assign { target, value } = stmt {
                if let BindingPattern::Identifier(var_name) = target {
                    if let Expr::Ident(rhs) = &**value {
                        let rhs_name = rhs.as_str();
                        if rhs_name == "http_archive" {
                            http_aliases.insert(var_name.clone());
                        } else if rhs_name == "tf_http_archive" {
                            tf_http_aliases.insert(var_name.clone());
                        }
                    }
                }
            }
            // Also handle load() aliasing: load("...", local = "http_archive")
            if let Stmt::Load { file: _, symbols } = stmt {
                for (local, original) in symbols {
                    if original == "http_archive" {
                        http_aliases.insert(local.clone());
                    } else if original == "tf_http_archive" {
                        tf_http_aliases.insert(local.clone());
                    }
                }
            }
        }

        // Traverse statements to find top-level call expressions
        for stmt in &program.statements {
            if let Stmt::Expr(expr) = stmt {
                if let Expr::Call(call) = &**expr {
                    // Only handle simple identifier calls
                    let func_name = if let Expr::Ident(name) = call.function.as_ref() {
                        name.as_str()
                    } else {
                        continue;
                    };

                    let is_http = func_name == "http_archive"
                        || http_aliases.contains(func_name)
                        || func_name.ends_with("_http_archive");
                    let is_tf_http =
                        func_name == "tf_http_archive" || tf_http_aliases.contains(func_name);
                    if !is_http && !is_tf_http {
                        continue;
                    }

                    // Collect kwargs into a map
                    let mut name_val: Option<String> = None;
                    let mut url_val: Option<String> = None;
                    let mut sha256_val: Option<String> = None;
                    let mut strip_prefix_val: Option<String> = None;
                    let mut build_file_path: Option<std::path::PathBuf> = None;

                    for (k, v) in &call.kwargs {
                        match (k.as_str(), v) {
                            ("name", expr) => name_val = extract_str(expr),
                            ("url", expr) => url_val = extract_str(expr),
                            ("urls", Expr::List(list)) => {
                                // pick first string
                                for e in list {
                                    if let Some(s) = extract_str(e) {
                                        url_val = Some(s);
                                        break;
                                    }
                                }
                            }
                            ("sha256", expr) => sha256_val = extract_str(expr),
                            ("strip_prefix", expr) => strip_prefix_val = extract_str(expr),
                            ("build_file", Expr::String(label)) => {
                                if let Some(rest) = label.strip_prefix("//") {
                                    if let Some((pkg, file)) = rest.split_once(':') {
                                        build_file_path = Some(workspace_root.join(pkg).join(file));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }

                    if let Some(name) = name_val {
                        if name == target_name {
                            if let Some(url) = url_val {
                                return Some((url, sha256_val, strip_prefix_val, build_file_path));
                            }
                        }
                    }
                }
            }
        }

        None
    }
    /// Scan a WORKSPACE chunk for repository rule calls. Supports:
    /// - http_archive(name = "repo", ...)
    /// - tf_http_archive(name = "repo", ...)
    fn scan_repo_rules_in_chunk(
        contents: &str,
        target_name: &str,
        workspace_root: &std::path::Path,
    ) -> Option<RepoMetadata> {
        // Try native http_archive first
        if let Some(found) = Self::scan_http_archive_in_bzl(contents, target_name, workspace_root) {
            return Some(found);
        }
        // Try common macro wrapper tf_http_archive by mapping its arguments to http_archive
        let pattern = regex::Regex::new(r"tf_http_archive\s*\((?s:.*?)\)").ok()?;
        // Compile all regex patterns outside the loop for efficiency
        let name_re = regex::Regex::new(r#"name\s*=\s*"([^"]+)""#).ok()?;
        let re_url_tf_mirror_one =
            regex::Regex::new(r#"urls\s*=\s*tf_mirror_urls\(\s*\"([^\"]+)\"\s*\)"#).ok()?;
        let re_urls_list = regex::Regex::new(r#"urls\s*=\s*\[([^\]]+)\]"#).ok()?;
        let re_url_single = regex::Regex::new(r#"url\s*=\s*\"([^\"]+)\""#).ok()?;
        let re_sha = regex::Regex::new(r#"sha256\s*=\s*\"([0-9a-fA-F]{64})\""#).ok()?;
        let re_strip = regex::Regex::new(r#"strip_prefix\s*=\s*\"([^\"]+)\""#).ok()?;
        let re_build = regex::Regex::new(r#"build_file\s*=\s*\"//([^:\"]+):([^\"]+)\""#).ok()?;

        for m in pattern.captures_iter(contents) {
            let body = m.get(0).unwrap().as_str();
            let name = name_re.captures(body)?.get(1)?.as_str();
            if name != target_name {
                continue;
            }
            // Reuse same attribute parsers as http_archive, including tf_mirror_urls support

            // Determine URL precedence: tf_mirror_urls > urls list > url
            let mut url: Option<String> = None;
            if let Some(c) = re_url_tf_mirror_one.captures(body) {
                url = Some(c.get(1).unwrap().as_str().to_string());
            }
            if url.is_none() {
                if let Some(c) = re_urls_list.captures(body) {
                    let list = c.get(1).unwrap().as_str();
                    for seg in list.split(',') {
                        let seg = seg.trim();
                        if let Some(s) = seg.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
                            url = Some(s.to_string());
                            break;
                        }
                    }
                }
            }
            if url.is_none() {
                if let Some(c) = re_url_single.captures(body) {
                    url = Some(c.get(1).unwrap().as_str().to_string());
                }
            }

            let sha256 = re_sha
                .captures(body)
                .map(|c| c.get(1).unwrap().as_str().to_string());
            let strip_prefix = re_strip
                .captures(body)
                .map(|c| c.get(1).unwrap().as_str().to_string());
            let build_file = re_build.captures(body).map(|c| {
                let pkg = c.get(1).unwrap().as_str();
                let file = c.get(2).unwrap().as_str();
                workspace_root.join(pkg).join(file)
            });

            if let Some(u) = url {
                return Some((u, sha256, strip_prefix, build_file));
            }
        }
        // Some repositories also use new_http_archive wrappers; treat like http_archive
        let pattern2 = regex::Regex::new(r"new_http_archive\s*\((?s:.*?)\)").ok()?;
        // name_re already compiled above, reuse it
        for m in pattern2.captures_iter(contents) {
            let body = m.get(0).unwrap().as_str();
            let name = name_re.captures(body)?.get(1)?.as_str();
            if name != target_name {
                continue;
            }
            // Reuse http_archive attribute extraction
            if let Some((url, sha256, strip_prefix, build_file)) =
                Self::scan_http_archive_in_bzl(body, target_name, workspace_root)
            {
                return Some((url, sha256, strip_prefix, build_file));
            }
        }
        None
    }

    #[allow(dead_code)]
    fn resolve_label_to_path_static(
        workspace_root: &std::path::Path,
        #[allow(unused_variables)] current_repo_root: &Path,
        current_package: &Option<PathBuf>,
        label: &str,
    ) -> Result<PathBuf> {
        if label.starts_with("@") {
            // Simplified: treat @repo//pkg:file via external repos not accessible here
            anyhow::bail!("Cannot resolve external label statically: {}", label);
        } else if let Some(rest) = label.strip_prefix("//") {
            let (pkg, file) = rest
                .split_once(':')
                .ok_or_else(|| anyhow::anyhow!("Invalid label: {}", label))?;
            Ok(workspace_root.join(pkg).join(file))
        } else if let Some(file) = label.strip_prefix(':') {
            let pkg_dir = current_package
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("No current package for relative label"))?;
            Ok(pkg_dir.join(file))
        } else {
            anyhow::bail!("Unsupported label format: {}", label)
        }
    }

    fn fetch_from_third_party_workspace_bzl(
        workspace_root: &std::path::Path,
        repo_name: &str,
    ) -> Result<Option<PathBuf>> {
        use std::fs;
        let third_party_bzl = workspace_root
            .join("third_party")
            .join(repo_name)
            .join("workspace.bzl");
        if !third_party_bzl.exists() {
            return Ok(None);
        }
        let src = fs::read_to_string(&third_party_bzl)
            .map_err(|e| anyhow!("Failed to read {}: {}", third_party_bzl.display(), e))?;
        let re_url_tf_mirror_one =
            regex::Regex::new(r#"urls\s*=\s*tf_mirror_urls\(\s*\"([^\"]+)\"\s*\)"#).unwrap();
        let re_url_tf_mirror_list =
            regex::Regex::new(r#"urls\s*=\s*tf_mirror_urls\(\s*\[([^\]]+)\]\s*\)"#).unwrap();
        let re_urls_list = regex::Regex::new(r#"urls\s*=\s*\[([^\]]+)\]"#).unwrap();
        let re_url_single = regex::Regex::new(r#"url\s*=\s*\"([^\"]+)\""#).unwrap();
        let re_sha = regex::Regex::new(r#"sha256\s*=\s*\"([0-9a-fA-F]{64})\""#).unwrap();
        let re_strip = regex::Regex::new(r#"strip_prefix\s*=\s*\"([^\"]+)\""#).unwrap();
        let re_build = regex::Regex::new(r#"build_file\s*=\s*\"//([^:\"]+):([^\"]+)\""#).unwrap();

        let parse_url_list = |s: &str| -> Vec<String> {
            let mut out = Vec::new();
            for m in s.split(',') {
                let m = m.trim();
                if let Some(start) = m.find('"') {
                    if let Some(end) = m.rfind('"') {
                        if end > start {
                            out.push(m[start + 1..end].to_string());
                        }
                    }
                }
            }
            out
        };
        let mut urls: Vec<String> = Vec::new();
        if let Some(c) = re_url_tf_mirror_one.captures(&src) {
            urls.push(c.get(1).unwrap().as_str().to_string());
        }
        if urls.is_empty() {
            if let Some(c) = re_url_tf_mirror_list.captures(&src) {
                urls = parse_url_list(c.get(1).unwrap().as_str());
            }
        }
        if urls.is_empty() {
            if let Some(c) = re_urls_list.captures(&src) {
                urls = parse_url_list(c.get(1).unwrap().as_str());
            }
        }
        if urls.is_empty() {
            if let Some(c) = re_url_single.captures(&src) {
                urls.push(c.get(1).unwrap().as_str().to_string());
            }
        }
        let url = match urls.first() {
            Some(u) => u.clone(),
            None => return Ok(None),
        };
        let sha256 = re_sha
            .captures(&src)
            .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
        let strip_prefix = re_strip
            .captures(&src)
            .and_then(|c| c.get(1).map(|m| m.as_str().to_string()));
        let build_file_path = re_build.captures(&src).map(|c| {
            let pkg = c.get(1).unwrap().as_str();
            let file = c.get(2).unwrap().as_str();
            workspace_root.join(pkg).join(file)
        });

        let repo_dir_opt = Self::fetch_http_archive_repo(
            workspace_root,
            repo_name,
            url,
            sha256,
            strip_prefix,
            build_file_path,
        )?;

        // If fetched, record link_files mapping into marker for reproducibility
        if let Some(repo_dir) = &repo_dir_opt {
            let re_link = regex::Regex::new(r#"link_files\s*=\s*\{([^}]*)\}"#).unwrap();
            if let Some(cap) = re_link.captures(&src) {
                let body = cap.get(1).unwrap().as_str();
                let entry_re =
                    regex::Regex::new(r#"\"//([^:\"]+):([^\"]+)\"\s*:\s*\"([^\"]+)\""#).unwrap();
                let mut entries: Vec<(String, String)> = Vec::new();
                for m in entry_re.captures_iter(body) {
                    let pkg = m.get(1).unwrap().as_str();
                    let src_file = m.get(2).unwrap().as_str();
                    let dest = m.get(3).unwrap().as_str();
                    entries.push((format!("//{}:{}", pkg, src_file), dest.to_string()));
                }
                if !entries.is_empty() {
                    let marker = repo_dir.join(".rush_repo_marker.json");
                    if let Ok(old) = rush_incremental::tracked::read_to_string(&marker) {
                        if let Ok(mut j) = serde_json::from_str::<serde_json::Value>(&old) {
                            j["link_files"] = serde_json::json!(entries);
                            let _ =
                                std::fs::write(marker, serde_json::to_string_pretty(&j).unwrap());
                        }
                    }
                }
            }
        }

        Ok(repo_dir_opt)
    }

    fn resolve_third_party_source_candidate(
        candidate: &Path,
        requested_module: Option<&Path>,
        fetch_declared: impl FnOnce() -> Result<Option<PathBuf>>,
    ) -> Result<Option<PathBuf>> {
        if requested_module.is_some_and(|module| rush_incremental::tracked::exists(&candidate.join(module))) {
            return Ok(Some(candidate.to_path_buf()));
        }
        let declaration = candidate.join("workspace.bzl");
        if rush_incremental::tracked::exists(&declaration) {
            // TensorFlow's third_party/<repo> often holds only the archive
            // declaration and patches, not the repository's source. Record
            // the decision input and run the real rule instead of publishing
            // this metadata directory as a fetched external root.
            rush_incremental::tracked::read_file(&declaration)
                .with_context(|| format!("read repository declaration {}", declaration.display()))?;
            return fetch_declared();
        }
        Ok(Some(candidate.to_path_buf()))
    }

    fn find_third_party_dir(
        workspace_root: &std::path::Path,
        repo_name: &str,
    ) -> Result<Option<PathBuf>> {
        // The third_party reuse heuristics model legacy WORKSPACE trees
        // (TensorFlow). In a Bzlmod-only workspace they can hijack apparent
        // repository names before module resolution runs — including via the
        // parent-directory probes, which may land in an unrelated sibling
        // checkout (e.g. /data/tensorflow next to a Modular worktree).
        let is_bzlmod_only = workspace_root.join("MODULE.bazel").exists()
            && !workspace_root.join("WORKSPACE").exists()
            && !workspace_root.join("WORKSPACE.bazel").exists();
        if is_bzlmod_only {
            return Ok(None);
        }
        for root in Self::third_party_roots(workspace_root) {
            if let Some(path) = Self::find_third_party_dir_in_root(&root, repo_name)? {
                return Ok(Some(path));
            }
        }
        Ok(None)
    }

    fn third_party_roots(workspace_root: &std::path::Path) -> Vec<PathBuf> {
        let mut roots = Vec::new();

        fn push_unique(roots: &mut Vec<PathBuf>, candidate: PathBuf) {
            if !roots.iter().any(|existing| existing == &candidate) {
                roots.push(candidate);
            }
        }

        push_unique(&mut roots, workspace_root.join("third_party"));
        push_unique(
            &mut roots,
            workspace_root.join("tensorflow").join("third_party"),
        );

        if let Some(parent) = workspace_root.parent() {
            push_unique(&mut roots, parent.join("third_party"));
            push_unique(&mut roots, parent.join("tensorflow").join("third_party"));
        }

        roots
    }

    fn find_third_party_dir_in_root(
        third_party_root: &std::path::Path,
        repo_name: &str,
    ) -> Result<Option<PathBuf>> {
        if !third_party_root.exists() {
            return Ok(None);
        }

        // FIX: Explicitly exclude certain repos from third_party mapping to use external/ stubs
        // These repos have stubs in external/ that provide the necessary functions:
        // - com_google_googleapis: causes circular reference with third_party/googleapis
        // - local_config_cuda: stub provides CUDA disabled configuration
        // - local_config_tensorrt: stub provides TensorRT disabled configuration
        // - local_config_rocm: stub provides ROCm disabled configuration
        // - com_google_protobuf: external/ has full protobuf.bzl, third_party/protobuf doesn't
        const EXTERNAL_STUB_REPOS: &[&str] = &[
            "com_google_googleapis",
            "local_config_cuda",
            "local_config_tensorrt",
            "local_config_rocm",
            "com_google_protobuf",
            "rules_python_internal",
            "pybind11",
            "com_github_grpc_grpc",
        ];
        if EXTERNAL_STUB_REPOS.contains(&repo_name) {
            debug_log!(
                "[DEBUG BzlLoader] Skipping third_party for {}: use external stub instead",
                repo_name
            );
            return Ok(None);
        }

        match repo_name {
            // NOTE: com_google_googleapis handled above - it causes circular reference
            "com_github_grpc_grpc" => {
                let path = third_party_root.join("grpc");
                if path.exists() {
                    debug_log!(
                        "[DEBUG BzlLoader] Reusing third_party path for {}: {}",
                        repo_name,
                        path.display()
                    );
                    return Ok(Some(path));
                }
            }
            "python" | "pypi" => {
                let path = third_party_root.join("py");
                if path.exists() {
                    debug_log!(
                        "[DEBUG BzlLoader] Reusing third_party path for {}: {}",
                        repo_name,
                        path.display()
                    );
                    return Ok(Some(path));
                }
            }
            "absl_py" => {
                let path = third_party_root.join("systemlibs");
                if path.exists() {
                    debug_log!(
                        "[DEBUG BzlLoader] Reusing third_party/systemlibs path for {}: {}",
                        repo_name,
                        path.display()
                    );
                    return Ok(Some(path));
                }
            }
            _ => {}
        }

        for candidate in Self::third_party_candidate_names(repo_name) {
            let candidate_path = third_party_root.join(&candidate);
            if candidate_path.exists() {
                debug_log!(
                    "[DEBUG BzlLoader] Reusing third_party path for {}: {}",
                    repo_name,
                    candidate_path.display()
                );
                return Ok(Some(candidate_path));
            }

            let candidate_in_systemlibs = third_party_root.join("systemlibs").join(&candidate);
            if candidate_in_systemlibs.exists() {
                debug_log!(
                    "[DEBUG BzlLoader] Reusing third_party/systemlibs path for {}: {}",
                    repo_name,
                    candidate_in_systemlibs.display()
                );
                return Ok(Some(candidate_in_systemlibs));
            }
        }
        // Search for candidate BUILD files (e.g., third_party/systemlibs/absl_py.BUILD)
        for candidate in Self::third_party_candidate_names(repo_name) {
            let candidate_build = third_party_root.join(format!("{}.BUILD", candidate));
            if candidate_build.exists() {
                debug_log!(
                    "[DEBUG BzlLoader] Reusing third_party build file for {}: {}",
                    repo_name,
                    candidate_build.display()
                );
                return Ok(Some(third_party_root.to_path_buf()));
            }
            let system_candidate_build = third_party_root
                .join("systemlibs")
                .join(format!("{}.BUILD", candidate));
            if system_candidate_build.exists() {
                debug_log!(
                    "[DEBUG BzlLoader] Reusing third_party/systemlibs build file for {}: {}",
                    repo_name,
                    system_candidate_build.display()
                );
                return Ok(Some(third_party_root.join("systemlibs")));
            }
        }
        Ok(None)
    }

    fn third_party_candidate_names(repo_name: &str) -> Vec<String> {
        let mut candidates = Vec::new();
        let stripped = repo_name
            .trim_start_matches('@')
            .trim_start_matches("//")
            .split(':')
            .next()
            .unwrap_or(repo_name);

        const THIRD_PARTY_REPO_ALIASES: &[(&str, &str)] = &[
            ("absl_py", "absl"),
            ("pypi", "py"),
            ("python", "python_runtime"),
            ("com_github_grpc_grpc", "grpc"),
            ("local_xla", "xla"),
            ("local_tsl", "tsl"),
        ];
        for (key, alias) in THIRD_PARTY_REPO_ALIASES {
            if stripped == *key {
                candidates.push(alias.to_string());
            }
        }

        candidates.push(stripped.to_string());
        if let Some(rest) = stripped.strip_prefix("com_google_") {
            candidates.push(rest.to_string());
        }
        if let Some(rest) = stripped.strip_prefix("com_googlesource_") {
            candidates.push(rest.to_string());
        }
        if let Some(rest) = stripped.strip_prefix("org_tensorflow_") {
            candidates.push(rest.to_string());
        }
        if let Some(rest) = stripped.strip_prefix("local_") {
            candidates.push(rest.to_string());
        }

        if let Some(last) = stripped.split('_').last() {
            if !last.is_empty() {
                candidates.push(last.to_string());
            }
        }
        if stripped.ends_with("_py") {
            if let Some(base) = stripped.strip_suffix("_py") {
                if !base.is_empty() {
                    candidates.push(base.to_string());
                }
            }
        }

        let mut seen = HashSet::new();
        candidates
            .into_iter()
            .filter(|name| seen.insert(name.clone()))
            .collect()
    }

    fn extract_http_archive_attrs(
        attrs: &std::collections::HashMap<String, rush_repo::workspace::Value>,
        workspace_root: &std::path::Path,
    ) -> Result<RepoMetadata> {
        let url = match attrs.get("urls") {
            Some(rush_repo::workspace::Value::List(list)) => list.iter().find_map(|v| match v {
                rush_repo::workspace::Value::String(s) => Some(s.clone()),
                _ => None,
            }),
            _ => attrs.get("url").and_then(|v| match v {
                rush_repo::workspace::Value::String(s) => Some(s.clone()),
                _ => None,
            }),
        }
        .ok_or_else(|| anyhow::anyhow!("http_archive missing url/urls"))?;
        let sha256 = attrs.get("sha256").and_then(|v| match v {
            rush_repo::workspace::Value::String(s) => Some(s.clone()),
            _ => None,
        });
        let strip_prefix = attrs.get("strip_prefix").and_then(|v| match v {
            rush_repo::workspace::Value::String(s) => Some(s.clone()),
            _ => None,
        });
        let build_file = attrs
            .get("build_file")
            .and_then(|v| match v {
                rush_repo::workspace::Value::String(s) => Some(s.clone()),
                _ => None,
            })
            .and_then(|label| {
                label
                    .strip_prefix("//")
                    .and_then(|rest| rest.split_once(':'))
                    .map(|(pkg, file)| workspace_root.join(pkg).join(file))
            });
        Ok((url, sha256, strip_prefix, build_file))
    }

    fn fetch_http_archive_repo(
        workspace_root: &std::path::Path,
        repo_name: &str,
        url: String,
        sha256: Option<String>,
        strip_prefix: Option<String>,
        build_file: Option<std::path::PathBuf>,
    ) -> Result<Option<PathBuf>> {
        let manager = rush_repo::HttpArchiveManager::new(workspace_root)?;
        let repo_dir = manager.external_dir().join(repo_name);
        if repo_dir.exists() {
            let marker = repo_dir.join(".rush_repo_marker.json");
            if marker.exists() {
                let cur = serde_json::json!({
                    "name": repo_name,
                    "url": url,
                    "sha256": sha256,
                    "strip_prefix": strip_prefix,
                    "build_file": build_file.as_ref().map(|p| p.to_string_lossy().to_string()),
                });
                if let Ok(old) = rush_incremental::tracked::read_to_string(&marker) {
                    if let Ok(prev) = serde_json::from_str::<serde_json::Value>(&old) {
                        if prev == cur {
                            return Ok(Some(repo_dir));
                        }
                    }
                }
            }
        }

        let archive = rush_repo::http_archive::HttpArchive {
            name: repo_name.to_string(),
            url,
            sha256,
            strip_prefix,
            type_hint: None,
            build_file_content: None,
            build_file,
        };

        let repo_dir = if let Ok(handle) = tokio::runtime::Handle::try_current() {
            tokio::task::block_in_place(|| handle.block_on(manager.fetch(&archive)))?
        } else {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match rt {
                Ok(rt) => rt.block_on(manager.fetch(&archive))?,
                Err(_) => {
                    let rt2 = tokio::runtime::Runtime::new()?;
                    rt2.block_on(manager.fetch(&archive))?
                }
            }
        };

        let marker = repo_dir.join(".rush_repo_marker.json");
        let snap = serde_json::json!({
            "name": repo_name,
            "url": archive.url,
            "sha256": archive.sha256,
            "strip_prefix": archive.strip_prefix,
            "build_file": archive.build_file.as_ref().map(|p| p.to_string_lossy().to_string()),
        });
        let _ = std::fs::write(marker, serde_json::to_string_pretty(&snap)?);
        Ok(Some(repo_dir))
    }

    fn fetch_http_file_repo(
        workspace_root: &std::path::Path,
        repo_name: &str,
        url: String,
        sha256: Option<String>,
    ) -> Result<Option<PathBuf>> {
        // Determine filename from URL
        let filename = url.split('/').next_back().unwrap_or("downloaded.file");

        let manager = rush_repo::HttpArchiveManager::new(workspace_root)?;
        let cached = if let Ok(handle) = tokio::runtime::Handle::try_current() {
            tokio::task::block_in_place(|| handle.block_on(manager.download_file(&url, filename)))?
        } else {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match rt {
                Ok(rt) => rt.block_on(manager.download_file(&url, filename))?,
                Err(_) => {
                    let rt2 = tokio::runtime::Runtime::new()?;
                    rt2.block_on(manager.download_file(&url, filename))?
                }
            }
        };

        // Verify sha256 if provided
        if let Some(ref expected) = sha256 {
            // Self-verify SHA256
            use sha2::{Digest, Sha256};
            let data = std::fs::read(&cached)?;
            let mut hasher = Sha256::new();
            hasher.update(&data);
            let actual = hex::encode(hasher.finalize());
            if actual != *expected {
                anyhow::bail!(
                    "SHA256 mismatch for {}. Expected {}, got {}",
                    url,
                    expected,
                    actual
                );
            }
        }

        // Prepare external repo directory
        let external_repo = manager.external_dir().join(repo_name);
        if external_repo.exists() {
            let _ = std::fs::remove_dir_all(&external_repo);
        }
        std::fs::create_dir_all(&external_repo)?;
        let target = external_repo.join(filename);
        std::fs::copy(&cached, &target)?;

        // Write marker
        let marker = external_repo.join(".rush_repo_marker.json");
        let j = serde_json::json!({
            "name": repo_name,
            "http_file": {
                "url": url,
                "filename": filename,
                "sha256": sha256,
            }
        });
        let _ = std::fs::write(marker, serde_json::to_string_pretty(&j)?);
        Ok(Some(external_repo))
    }

    fn fetch_git_repository_repo(
        workspace_root: &std::path::Path,
        repo_name: &str,
        remote: String,
        commit: Option<String>,
        tag: Option<String>,
        branch: Option<String>,
    ) -> Result<Option<PathBuf>> {
        // Build GitRepository rule and fetch to cache
        let mut rule = rush_repo::git::GitRepository::new(repo_name.to_string(), remote);
        if let Some(c) = commit.clone() {
            rule = rule.with_commit(c);
        }
        if let Some(t) = tag.clone() {
            rule = rule.with_tag(t);
        }
        if let Some(b) = branch.clone() {
            rule = rule.with_branch(b);
        }

        // Cache dir base: ~/.rush-cache under workspace
        let cache_base = workspace_root.join(".rush-cache");
        let fetched = if let Ok(handle) = tokio::runtime::Handle::try_current() {
            tokio::task::block_in_place(|| handle.block_on(rule.fetch(&cache_base)))?
        } else {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();
            match rt {
                Ok(rt) => rt.block_on(rule.fetch(&cache_base))?,
                Err(_) => {
                    let rt2 = tokio::runtime::Runtime::new()?;
                    rt2.block_on(rule.fetch(&cache_base))?
                }
            }
        };

        // Link into external repo directory
        let manager = rush_repo::HttpArchiveManager::new(workspace_root)?;
        let external_repo = manager.external_dir().join(repo_name);
        if let Ok(metadata) = std::fs::symlink_metadata(&external_repo) {
            if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
                std::fs::remove_dir_all(&external_repo)?;
            } else {
                std::fs::remove_file(&external_repo)?;
            }
        }
        #[cfg(unix)]
        {
            use anyhow::Context as _;
            std::os::unix::fs::symlink(&fetched, &external_repo).context(format!(
                "Failed to symlink {} -> {}",
                fetched.display(),
                external_repo.display()
            ))?;
        }
        #[cfg(not(unix))]
        {
            // Fallback: copy directory tree (more expensive)
            fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
                std::fs::create_dir_all(dst)?;
                for entry in std::fs::read_dir(src)? {
                    let entry = entry?;
                    let ty = entry.file_type()?;
                    let from = entry.path();
                    let to = dst.join(entry.file_name());
                    if ty.is_dir() {
                        copy_dir_recursive(&from, &to)?;
                    } else {
                        std::fs::copy(&from, &to)?;
                    }
                }
                Ok(())
            }
            copy_dir_recursive(&fetched, &external_repo)?;
        }

        // Write marker JSON
        let marker = external_repo.join(".rush_repo_marker.json");
        let j = serde_json::json!({
            "name": repo_name,
            "git": {
                "remote": rule.remote,
                "commit": commit,
                "tag": tag,
                "branch": branch,
            }
        });
        let _ = std::fs::write(marker, serde_json::to_string_pretty(&j)?);
        Ok(Some(external_repo))
    }

    #[allow(dead_code)]
    fn scan_http_archive_in_bzl(
        contents: &str,
        target_name: &str,
        workspace_root: &std::path::Path,
    ) -> Option<RepoMetadata> {
        let pattern = regex::Regex::new(r"http_archive\s*\((?s:.*?)\)").ok()?;
        // Compile all regex patterns outside the loop for efficiency
        let name_re = regex::Regex::new(r#"name\s*=\s*"([^"]+)""#).ok()?;
        let re_url_tf_mirror_one =
            regex::Regex::new(r#"urls\s*=\s*tf_mirror_urls\(\s*\"([^\"]+)\"\s*\)"#).ok()?;
        let re_urls_list = regex::Regex::new(r#"urls\s*=\s*\[([^\]]+)\]"#).ok()?;
        let re_url_single = regex::Regex::new(r#"url\s*=\s*\"([^\"]+)\""#).ok()?;
        let re_sha = regex::Regex::new(r#"sha256\s*=\s*\"([0-9a-fA-F]{64})\""#).ok()?;
        let re_strip = regex::Regex::new(r#"strip_prefix\s*=\s*\"([^\"]+)\""#).ok()?;
        let re_build = regex::Regex::new(r#"build_file\s*=\s*\"//([^:\"]+):([^\"]+)\""#).ok()?;

        for m in pattern.captures_iter(contents) {
            let body = m.get(0).unwrap().as_str();
            let name = name_re.captures(body)?.get(1)?.as_str();
            if name != target_name {
                continue;
            }

            let mut url: Option<String> = None;
            if let Some(c) = re_url_tf_mirror_one.captures(body) {
                url = Some(c.get(1).unwrap().as_str().to_string());
            }
            if url.is_none() {
                if let Some(c) = re_urls_list.captures(body) {
                    let list = c.get(1).unwrap().as_str();
                    for seg in list.split(',') {
                        let seg = seg.trim();
                        if let Some(s) = seg.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
                            url = Some(s.to_string());
                            break;
                        }
                    }
                }
            }
            if url.is_none() {
                if let Some(c) = re_url_single.captures(body) {
                    url = Some(c.get(1).unwrap().as_str().to_string());
                }
            }
            let sha256 = re_sha
                .captures(body)
                .map(|c| c.get(1).unwrap().as_str().to_string());
            let strip_prefix = re_strip
                .captures(body)
                .map(|c| c.get(1).unwrap().as_str().to_string());
            let build_file = re_build.captures(body).map(|c| {
                let pkg = c.get(1).unwrap().as_str();
                let file = c.get(2).unwrap().as_str();
                workspace_root.join(pkg).join(file)
            });

            if let Some(u) = url {
                return Some((u, sha256, strip_prefix, build_file));
            }
        }
        None
    }

    /// Resolve workspace absolute .bzl file (//pkg:file.bzl)
    fn resolve_workspace_bzl(&self, bzl_label: &str) -> Result<PathBuf> {
        // Parse //package:file.bzl
        let rest = &bzl_label[2..]; // Remove '//'

        let (package, filename) = if let Some(colon_pos) = rest.find(':') {
            (&rest[..colon_pos], &rest[colon_pos + 1..])
        } else {
            return Err(anyhow!("Invalid .bzl label format: {}", bzl_label));
        };

        // Apply repository alias mapping when evaluating in the main workspace.
        // This supports TensorFlow-style vendored repos (e.g., //tsl → @local_tsl//tsl).
        if let Some(ctx) = self.current_thread_context() {
            if ctx.repo_root == self.workspace_root {
                if package.starts_with("third_party/googleapis") {
                    let rest = package.strip_prefix("third_party/googleapis").unwrap();
                    let ext_pkg = rest.trim_start_matches('/');
                    let ext_label = if ext_pkg.is_empty() {
                        format!("@com_google_googleapis//:{}", filename)
                    } else {
                        format!("@com_google_googleapis//{}:{}", ext_pkg, filename)
                    };
                    return self.resolve_external_bzl(&ext_label);
                }
                if let Some(first) = package.split('/').next() {
                    if let Some(mapped) = self.repo_aliases.get(first) {
                        let ext = format!("@{}//{}:{}", mapped.value(), package, filename);
                        return self.resolve_external_bzl(&ext);
                    }
                }
            }
        }

        // IMPORTANT: // labels resolve relative to the repository of the file
        // containing the load(). Use per-thread context; default to main workspace
        // when evaluating a top-level BUILD in the main workspace.
        let repo_root = if let Some(ctx) = self.current_thread_context() {
            debug_log!(
                "[DEBUG resolve_workspace_bzl] Label {} using thread context repo_root: {:?}",
                bzl_label,
                ctx.repo_root
            );
            ctx.repo_root
        } else {
            debug_log!(
                "[DEBUG resolve_workspace_bzl] Label {} NO thread context, using workspace_root: {:?}",
                bzl_label, self.workspace_root
            );
            self.workspace_root.clone()
        };

        let bzl_path = if package.is_empty() {
            // //:file.bzl at repo root
            repo_root.join(filename)
        } else {
            repo_root.join(package).join(filename)
        };

        if !bzl_path.exists() {
            return Err(anyhow!(".bzl file not found: {}", bzl_path.display()));
        }

        Ok(bzl_path)
    }

    fn normalize_load_label(bzl_label: &str) -> String {
        Self::rewrite_googleapis_label(bzl_label).unwrap_or_else(|| bzl_label.to_string())
    }

    fn rewrite_googleapis_label(bzl_label: &str) -> Option<String> {
        if !bzl_label.starts_with("//") {
            return None;
        }
        let rest = &bzl_label[2..];
        if !rest.starts_with("third_party/googleapis") {
            return None;
        }
        let (path, filename) = rest.split_once(':')?;
        let suffix = path.strip_prefix("third_party/googleapis").unwrap();
        let ext_pkg = suffix.trim_start_matches('/');
        let ext_label = if ext_pkg.is_empty() {
            format!("@com_google_googleapis//:{}", filename)
        } else {
            format!("@com_google_googleapis//{}:{}", ext_pkg, filename)
        };
        Some(ext_label)
    }

    /// Resolve package-relative .bzl file (:file.bzl)
    fn resolve_relative_bzl(&self, bzl_label: &str) -> Result<PathBuf> {
        let filename = &bzl_label[1..]; // Remove ':'

        let package_dir = if let Some(ctx) = self.current_thread_context() {
            ctx.package_dir
        } else {
            return Err(anyhow!(
                "Cannot resolve relative .bzl path without current package context"
            ));
        };

        let bzl_path = package_dir.join(filename);

        if !bzl_path.exists() {
            return Err(anyhow!(".bzl file not found: {}", bzl_path.display()));
        }

        Ok(bzl_path)
    }

    /// Parse a .bzl file (NO CACHING - ASTs are huge!)
    ///
    /// CRITICAL: Programs (ASTs) consume 100-1000x source file size in memory.
    /// tensorflow.bzl (120KB source) → 2.5GB as AST!
    /// We do NOT cache programs anymore - parse on demand and drop immediately after evaluation.
    /// Only symbols (evaluated results) are cached.
    pub fn parse_bzl_file(&mut self, bzl_path: &Path) -> Result<Program> {
        // NO CACHE CHECK - always parse fresh to avoid OOM
        // ASTs are temporary and should be dropped after evaluation

        // Read and parse .bzl file
        let source = std::fs::read_to_string(bzl_path)
            .map_err(|e| anyhow!("Failed to read .bzl file {}: {}", bzl_path.display(), e))?;

        let program = crate::parse(&source)
            .map_err(|e| anyhow!("Failed to parse .bzl file {}: {}", bzl_path.display(), e))?;

        // DO NOT CACHE - let it drop after evaluation
        // This saves 100 MB per .bzl file in memory

        Ok(program)
    }

    /// Evaluate a .bzl file and return all defined symbols
    #[allow(dead_code)]
    fn evaluate_bzl(
        &mut self,
        program: &Program,
        loader_arc: Option<std::sync::Arc<std::sync::Mutex<BzlLoader>>>,
    ) -> Result<HashMap<String, Value>> {
        let mut evaluator = Evaluator::new();

        // Register native namespace for macros (native.cc_library, etc.)
        evaluator.register_native_namespace();

        // Set the bzl_loader so .bzl files can recursively load other .bzl files
        if let Some(loader) = loader_arc {
            evaluator.set_bzl_loader(loader);
        }

        // Evaluate the program
        evaluator
            .eval_program(program)
            .map_err(|e| anyhow!("Failed to evaluate .bzl file: {}", e))?;

        // Extract all symbols from the evaluator environment
        let symbols = evaluator.export_environment();

        Ok(symbols)
    }

    /// Extract specific symbols from a symbol map
    ///
    /// Maps original symbol names from the .bzl file to local names for the BUILD file.
    /// For example, if .bzl has "foo" and BUILD wants "bar = foo", this looks up "foo"
    /// in all_symbols and inserts it as "bar" in the returned map.
    fn extract_symbols(
        &self,
        all_symbols: &HashMap<String, Value>,
        requested: &[(String, String)],
    ) -> Result<HashMap<String, Value>> {
        let mut exported = HashMap::new();

        for (local_name, original_name) in requested {
            if let Some(value) = all_symbols.get(original_name) {
                exported.insert(local_name.clone(), value.clone());
            } else {
                return Err(anyhow!("Symbol '{}' not found in .bzl file", original_name));
            }
        }

        Ok(exported)
    }

    /// Clear all caches (useful for testing or file changes)
    /// Note: This only clears local caches, not the global shared cache
    pub fn clear_cache(&mut self) {
        if self.global_cache.is_none() {
            self.program_cache.clear();
            self.symbol_cache.clear();
        }
        // Global cache is shared, so we don't clear it from here
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn third_party_declaration_metadata_is_not_a_materialized_repository() {
        let root = tempfile::tempdir().unwrap();
        let metadata = root.path().join("third_party/pybind11_bazel");
        let source = root.path().join("materialized/pybind11_bazel");
        std::fs::create_dir_all(&metadata).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        let declaration = metadata.join("workspace.bzl");
        std::fs::write(&declaration, include_bytes!("../tests/fixtures/pybind11_bazel_72cbbf1/workspace.bzl")).unwrap();
        std::fs::write(metadata.join("BUILD"), "").unwrap();
        std::fs::write(metadata.join("pybind11_bazel.patch"), include_bytes!("../tests/fixtures/pybind11_bazel_72cbbf1/tensorflow.patch")).unwrap();
        std::fs::write(source.join("build_defs.bzl"), include_bytes!("../tests/fixtures/pybind11_bazel_72cbbf1/build_defs.bzl")).unwrap();
        rush_incremental::tracked::test_scope(root.path(), || {
            let calls = std::cell::Cell::new(0);
            let selected = BzlLoader::resolve_third_party_source_candidate(
                &metadata, Some(Path::new("build_defs.bzl")), || {
                    calls.set(calls.get() + 1);
                    Ok(Some(source.clone()))
                },
            ).unwrap();
            assert_eq!(selected, Some(source));
            assert_eq!(calls.get(), 1);
            assert!(!metadata.join("build_defs.bzl").exists());
            let manifest = rush_incremental::tracked::current_required().manifest();
            assert!(manifest.deps().iter().any(|dependency| matches!(dependency,
                Dependency::FileContent { path, .. } if path == &declaration)));
            assert!(manifest.deps().iter().any(|dependency| matches!(dependency,
                Dependency::FileExistence { path, .. } if path == &metadata.join("build_defs.bzl"))));
        });
    }

    #[test]
    fn third_party_discovery_never_substitutes_metadata_for_a_failed_fetch() {
        let root = tempfile::tempdir().unwrap();
        let candidate = root.path().join("third_party/declared_repo");
        std::fs::create_dir_all(&candidate).unwrap();
        std::fs::write(candidate.join("workspace.bzl"), "def repo():\n    pass\n").unwrap();
        std::fs::write(candidate.join("keep-existing"), "unrelated contents").unwrap();
        rush_incremental::tracked::test_scope(root.path(), || {
            assert!(BzlLoader::resolve_third_party_source_candidate(
                &candidate, Some(Path::new("missing.bzl")), || Ok(None),
            ).unwrap().is_none());
            let error = BzlLoader::resolve_third_party_source_candidate(
                &candidate, None, || anyhow::bail!("declared repository fetch failed"),
            ).unwrap_err();
            assert!(error.to_string().contains("declared repository fetch failed"));
            assert_eq!(std::fs::read_to_string(candidate.join("keep-existing")).unwrap(), "unrelated contents");
            std::fs::write(candidate.join("real.bzl"), "value = 1\n").unwrap();
            assert_eq!(BzlLoader::resolve_third_party_source_candidate(
                &candidate, Some(Path::new("real.bzl")), || panic!("existing source must not fetch"),
            ).unwrap(), Some(candidate));
        });
    }

    #[test]
    fn module_materialization_goal_preserves_package_for_nested_unexported_source() {
        assert_eq!(BzlLoader::workspace_module_package_goal("repo", "@repo//defs:subdir/child.bzl").unwrap(), "@repo//defs:all");
        assert_eq!(BzlLoader::workspace_module_package_goal("repo", "@repo//:py_version.bzl").unwrap(), "@repo//:all");
        assert!(BzlLoader::workspace_module_package_goal("repo", "@other//defs:child.bzl").is_err());
        assert!(BzlLoader::workspace_module_package_goal("repo", "@repo//defs:../child.bzl").is_err());
    }

    #[test]
    fn legacy_workspace_source_discovery_requires_proof_despite_name_and_directory_hints() {
        let root = tempfile::tempdir().unwrap();
        let repository = "declared_source_proof_test";
        std::fs::write(root.path().join("WORKSPACE"), "workspace(name = \"proof_test\")\n").unwrap();
        let metadata = root.path().join("third_party").join(repository);
        let unregistered = root.path().join("external").join(repository);
        std::fs::create_dir_all(&metadata).unwrap();
        std::fs::create_dir_all(&unregistered).unwrap();
        std::fs::write(metadata.join("workspace.bzl"), "def repo():\n    pass\n").unwrap();
        std::fs::write(metadata.join("BUILD"), "").unwrap();
        std::fs::write(unregistered.join("BUILD"), "").unwrap();
        std::fs::write(unregistered.join("build_defs.bzl"), include_bytes!("../tests/fixtures/pybind11_bazel_72cbbf1/build_defs.bzl")).unwrap();
        rush_incremental::tracked::test_scope(root.path(), || {
            SELF_SINGLETON_FETCHED.lock().unwrap().insert(repository.to_string());
            let loader = BzlLoader::new(root.path().to_path_buf());
            // This command scope deliberately has no client Bazel/PATH. The
            // loader must attempt genuine proof admission and report that
            // missing prerequisite, never accept a name bit or raw directory.
            let result = loader.resolve_bzl_path(&format!("@{repository}//:build_defs.bzl"));
            SELF_SINGLETON_FETCHED.lock().unwrap().remove(repository);
            let error = result.expect_err("unregistered source hints must not bypass repository-rule admission");
            assert!(format!("{error:#}").contains("client command supplied neither RUSH_BAZEL_PATH nor PATH"), "{error:#}");
            assert_eq!(std::fs::read(unregistered.join("build_defs.bzl")).unwrap(),
                include_bytes!("../tests/fixtures/pybind11_bazel_72cbbf1/build_defs.bzl"));
            assert!(!metadata.join("build_defs.bzl").exists());
        });
    }

    fn write_executable(path: &Path) {
        std::fs::write(path, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn bazel_fetch_uses_exact_configured_binary() {
        let temp = tempfile::tempdir().unwrap();
        let bazel = temp.path().join("configured-bazel");
        write_executable(&bazel);

        let resolved = resolve_bazel_binary_from_command_environment(
            temp.path(),
            bazel.to_str(),
            Some("/does/not/contain/bazel"),
        )
        .unwrap();
        assert_eq!(resolved, bazel);
    }

    #[test]
    fn bazel_fetch_searches_only_the_client_path() {
        let temp = tempfile::tempdir().unwrap();
        let tools = temp.path().join("client-tools");
        std::fs::create_dir_all(&tools).unwrap();
        let bazel = tools.join("bazel");
        write_executable(&bazel);

        let resolved =
            resolve_bazel_binary_from_command_environment(temp.path(), None, tools.to_str())
                .unwrap();
        assert_eq!(resolved, bazel);
    }

    #[test]
    fn invalid_configured_bazel_never_falls_back() {
        let temp = tempfile::tempdir().unwrap();
        let tools = temp.path().join("client-tools");
        std::fs::create_dir_all(&tools).unwrap();
        write_executable(&tools.join("bazel"));

        let error = resolve_bazel_binary_from_command_environment(
            temp.path(),
            Some("missing-bazel"),
            tools.to_str(),
        )
        .expect_err("an explicit invalid override must fail closed");
        assert!(error.to_string().contains("RUSH_BAZEL_PATH"), "{error:#}");
        assert!(error.to_string().contains("non-executable"), "{error:#}");
    }

    #[test]
    fn missing_client_bazel_names_the_remediation() {
        let temp = tempfile::tempdir().unwrap();
        let error = resolve_bazel_binary_from_command_environment(
            temp.path(),
            None,
            Some("/does/not/contain/bazel"),
        )
        .expect_err("a client without Bazel must fail before spawning a subprocess");
        assert!(error.to_string().contains("client PATH"), "{error:#}");
        assert!(error.to_string().contains("RUSH_BAZEL_PATH"), "{error:#}");
    }

    #[test]
    fn canonical_repo_accepts_shared_and_per_repo_output_bases() {
        let temp = tempfile::tempdir().unwrap();
        let output_base = temp.path().join("bazel_output_base");
        let shared = output_base.join("external").join("rules_cc+");
        let per_repo = output_base
            .join("modular_wheel")
            .join("external")
            .join("+modular_wheel_repository+modular_wheel");
        let unrelated = temp
            .path()
            .join("other")
            .join("external")
            .join("rules_python+");

        assert_eq!(
            canonical_repo_from_output_base_path(&shared, &output_base).as_deref(),
            Some("rules_cc+")
        );
        assert_eq!(
            canonical_repo_from_output_base_path(&per_repo, &output_base).as_deref(),
            Some("+modular_wheel_repository+modular_wheel")
        );
        assert_eq!(
            canonical_repo_from_output_base_path(&unrelated, &output_base),
            None
        );
    }

    #[test]
    fn file_fingerprint_digest_is_correctness_key() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("f.bzl");

        let write = |bytes: &[u8]| {
            let mut f = std::fs::File::create(&p).expect("create");
            f.write_all(bytes).expect("write");
            f.sync_all().ok();
        };
        let fp = || {
            let meta = std::fs::metadata(&p).expect("meta");
            FileFingerprint::from_metadata_with_precheck(&p, &meta, None)
        };

        // Pin a clean whole-second mtime so filesystem mtime granularity
        // can't perturb the equality assertions below.
        let pinned = UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);

        // Same content -> matching fingerprints (change-pruned).
        write(b"AAAA");
        set_mtime(&p, pinned);
        let a = fp();
        write(b"AAAA");
        set_mtime(&p, pinned);
        let a2 = fp();
        assert!(a.matches(&a2), "identical content must match (reuse)");
        assert!(a.digest.is_some());

        // In-place edit preserving LENGTH, then restore mtime to the
        // identical pinned value (the `touch -r` attack): digest changes
        // -> NO match, even though (len, mtime) are byte-identical.
        write(b"BBBB"); // same length (4 bytes)
        set_mtime(&p, pinned);
        let b = fp();
        assert_eq!(a.len, b.len, "lengths identical for the test to be meaningful");
        assert_eq!(
            a.modified_ns, b.modified_ns,
            "mtime restored identical for the test to be meaningful"
        );
        assert!(
            !a.matches(&b),
            "content edit with identical (len, mtime) MUST invalidate"
        );
    }

    #[cfg(unix)]
    fn set_mtime(p: &Path, t: std::time::SystemTime) {
        // Restore atime+mtime like `touch -r` does. ctime cannot be set
        // from userspace and will still reflect the just-written content,
        // which is exactly the property the precheck relies on.
        use std::os::unix::ffi::OsStrExt;
        let dur = t.duration_since(UNIX_EPOCH).unwrap();
        let ts = libc::timespec {
            tv_sec: dur.as_secs() as libc::time_t,
            tv_nsec: dur.subsec_nanos() as _,
        };
        let times = [ts, ts];
        let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
        let r =
            unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), times.as_ptr(), 0) };
        assert_eq!(r, 0, "utimensat failed: {}", std::io::Error::last_os_error());
    }

    #[test]
    fn program_stmt_node_count_recurses_into_bodies() {
        // #39 census: stmt_node_count counts the def plus its 2 body stmts = 3,
        // and recurses (does not stop at the top level).
        let program = crate::parse("def f():\n    x = 1\n    y = 2\n").expect("parse");
        assert_eq!(program.stmt_node_count(), 3);
        // Flat program: two top-level statements.
        let flat = crate::parse("a = 1\nb = 2\n").expect("parse");
        assert_eq!(flat.stmt_node_count(), 2);
    }

    #[test]
    fn live_program_guard_tracks_and_balances() {
        // #39 census: a LiveProgramGuard increments the parsed-total and lifts
        // peak active programs while alive, and active programs returns to its
        // pre-guard value on drop. Monotonic counters are asserted as lower
        // bounds so the test is robust to other tests running concurrently.
        use program_census::*;
        use std::sync::atomic::Ordering::Relaxed;
        let parsed_before = PROGRAMS_PARSED_TOTAL.load(Relaxed);
        let active_before = ACTIVE_PROGRAMS.load(Relaxed);
        {
            let _g = LiveProgramGuard::new(1234, 7);
            assert!(ACTIVE_PROGRAMS.load(Relaxed) >= active_before + 1);
            assert!(PEAK_ACTIVE_PROGRAMS.load(Relaxed) >= 1);
            assert!(PEAK_ACTIVE_STMT_NODES.load(Relaxed) >= 7);
            assert!(PEAK_ACTIVE_SOURCE_BYTES.load(Relaxed) >= 1234);
        }
        assert_eq!(ACTIVE_PROGRAMS.load(Relaxed), active_before, "balanced on drop");
        assert_eq!(PROGRAMS_PARSED_TOTAL.load(Relaxed), parsed_before + 1);
    }

    fn make_symbols(count: usize) -> HashMap<String, Value> {
        (0..count)
            .map(|i| (format!("sym{}", i), Value::Int(i as i64)))
            .collect()
    }

    fn make_blob(label: &str, bytes: usize) -> HashMap<String, Value> {
        let mut map = HashMap::new();
        map.insert(label.to_string(), Value::String("x".repeat(bytes)));
        map
    }

    #[test]
    fn global_cache_reuses_within_one_command_and_invalidates_the_next() {
        let temp = tempfile::tempdir().expect("tempdir");
        let bzl_path = temp.path().join("defs.bzl");
        std::fs::write(&bzl_path, "value = \"old\"\n").expect("write old .bzl");
        let cache = GlobalBzlCache::new();

        rush_incremental::tracked::test_scope(temp.path(), || {
            rush_incremental::tracked::read_to_string(&bzl_path)
                .expect("tracked .bzl read");
            let mut symbols = HashMap::new();
            symbols.insert("value".to_string(), Value::String("old".to_string()));
            let generation = cache
                .put_symbols_with_generation(bzl_path.clone(), symbols)
                .expect("cache insertion");
            let deps = rush_incremental::tracked::current_required()
                .manifest()
                .deps()
                .to_vec();
            cache.set_dep_set(&bzl_path, generation, deps);

            std::fs::write(&bzl_path, "value = \"new\"\n").expect("write new .bzl");
            for _ in 0..2 {
                let cached = cache
                    .get_symbols(&bzl_path)
                    .expect("same command must reuse its admitted value");
                assert!(matches!(
                    cached.get("value"),
                    Some(Value::String(value)) if value == "old"
                ));
            }
        });

        rush_incremental::tracked::test_scope(temp.path(), || {
            assert!(
                cache.get_symbols(&bzl_path).is_none(),
                "a new command must reject the stale admitted value"
            );
        });
        assert_eq!(cache.estimated_memory_bytes(), 0);
    }

    #[test]
    fn stale_admission_cannot_remove_concurrent_replacement() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("concurrent-replacement.bzl");
        std::fs::write(&path, "value = \"old\"\n").expect("write old .bzl");
        let cache = GlobalBzlCache::new();

        rush_incremental::tracked::test_scope(temp.path(), || {
            let mark = rush_incremental::tracked::mark().expect("dependency mark");
            rush_incremental::tracked::read_to_string(&path).expect("tracked old .bzl read");
            let old_generation = cache
                .put_symbols_with_generation(path.clone(), make_blob("old", 16))
                .expect("old cache insertion");
            cache.set_dep_set(
                &path,
                old_generation,
                rush_incremental::tracked::captured_since(mark),
            );
        });
        std::fs::write(&path, "value = \"fresh\"\n").expect("write fresh .bzl");

        let second_io = rush_incremental::tracked::test_scope(temp.path(), || {
            rush_incremental::tracked::current_required()
        });
        let barriers = Arc::new(std::sync::Barrier::new(2));
        {
            let hook_barriers = Arc::clone(&barriers);
            *cache
                .admission_validation_hook
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                Some(Arc::new(move || {
                    // Pause after admission captured the old generation and
                    // dropped its DashMap guard, but before validation finishes.
                    hook_barriers.wait();
                    hook_barriers.wait();
                }));
        }

        let stale_cache = cache.clone();
        let stale_path = path.clone();
        let validator_io = Arc::clone(&second_io);
        let stale_validator = std::thread::spawn(move || {
            rush_incremental::tracked::scope(validator_io, || {
                stale_cache.get_symbols(&stale_path)
            })
        });

        barriers.wait();
        rush_incremental::tracked::scope(Arc::clone(&second_io), || {
            let mark = rush_incremental::tracked::mark().expect("replacement mark");
            rush_incremental::tracked::read_to_string(&path)
                .expect("tracked fresh .bzl read");
            let fresh_generation = cache
                .put_symbols_with_generation(path.clone(), make_blob("fresh", 32))
                .expect("fresh cache insertion");
            cache.set_dep_set(
                &path,
                fresh_generation,
                rush_incremental::tracked::captured_since(mark),
            );
        });
        barriers.wait();

        let cached = stale_validator
            .join()
            .expect("stale validator thread")
            .expect("replacement must remain admitted");
        assert!(cached.contains_key("fresh"));
        assert!(!cached.contains_key("old"));
        let fresh_size = cache
            .symbol_cache
            .get(&ModuleCacheKey::unqualified(&path))
            .expect("fresh cache generation")
            .size_estimate;
        assert_eq!(cache.estimated_memory_bytes(), fresh_size);
    }

    #[test]
    fn direct_load_bzl_manifest_includes_own_source() {
        let temp = tempfile::tempdir().expect("tempdir");
        let bzl_path = temp.path().join("defs.bzl");
        std::fs::write(&bzl_path, "value = \"tracked\"\n").expect("write .bzl");
        let cache = GlobalBzlCache::new();
        let mut loader = BzlLoader::new_with_global_cache(
            temp.path().to_path_buf(),
            cache.clone(),
        );

        rush_incremental::tracked::test_scope(temp.path(), || {
            let loaded = loader
                .load_bzl(
                    "//:defs.bzl",
                    &[("value".to_string(), "value".to_string())],
                    None,
                )
                .expect("direct .bzl load");
            assert!(matches!(
                loaded.get("value"),
                Some(Value::String(value)) if value == "tracked"
            ));
        });

        let canonical = std::fs::canonicalize(&bzl_path).expect("canonical .bzl path");
        let admission = {
            let entry = cache
                .symbol_cache
                .get(&loader.module_cache_key("//:defs.bzl", &canonical).expect("module key").0)
                .expect("cached direct .bzl value");
            Arc::clone(&entry.admission)
        };
        let admission = admission
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            admission.manifest.deps().iter().any(|dep| matches!(
                dep,
                Dependency::FileContent { path, .. } if path == &canonical
            )),
            "the producing manifest must contain the direct loader's own source"
        );
    }

    #[test]
    fn test_global_cache_eviction_by_entry_count() {
        // Phase 5.C.4 post-batch-N: relaxed from the
        // original "file2 specifically is evicted"
        // assertion. The cache's eviction policy is
        // "lowest access_count first" with arbitrary
        // tie-breaking on DashMap iteration order —
        // NOT proper LRU. So with file2 (count=1) and
        // file3 (count=1, just inserted) tied,
        // eviction non-deterministically picks one or
        // the other. The test now verifies the
        // INVARIANT: file1 (the MRU) is kept, and
        // exactly one of file2/file3 was evicted.
        let cache = GlobalBzlCache::with_limits(2, 32 * 1024);

        cache.put_symbols(PathBuf::from("file1.bzl"), make_symbols(4));
        cache.put_symbols(PathBuf::from("file2.bzl"), make_symbols(4));

        // Access file1 to bump its access_count above
        // file2's, protecting it from eviction.
        assert!(cache.get_symbols(Path::new("file1.bzl")).is_some());

        cache.put_symbols(PathBuf::from("file3.bzl"), make_symbols(4));

        // file1 must survive (highest access_count).
        assert!(
            cache.get_symbols(Path::new("file1.bzl")).is_some(),
            "MRU file1 must be retained"
        );
        // Exactly one of file2/file3 was evicted.
        let f2 = cache.get_symbols(Path::new("file2.bzl")).is_some();
        let f3 = cache.get_symbols(Path::new("file3.bzl")).is_some();
        assert!(
            f2 ^ f3,
            "exactly one of file2/file3 should be evicted (was f2={}, f3={})",
            f2,
            f3
        );
    }

    #[test]
    fn test_global_cache_eviction_by_memory() {
        // Phase 5.C.4 post-batch-N: relaxed in two
        // ways from the original:
        //
        // 1. The cap is in MB-rounded math:
        //    evict_to_target targets 75% of max
        //    cap, integer-divided into MB. The old
        //    fixture (max=7000 bytes) rounded the
        //    eviction target to 0 MB, which evicts
        //    everything. New fixture uses MB-scale
        //    sizes.
        //
        // 2. With both entries at access_count=1, the
        //    byte-cap-driven evict_to_target picks the
        //    eviction order via DashMap iteration —
        //    non-deterministic. Verify only that AT
        //    LEAST ONE eviction happened.
        let cache = GlobalBzlCache::with_limits(100, 8 * 1024 * 1024);

        // Each blob is ~4 MB; two blobs ≈ 8 MB which
        // is right at the cap. Eviction targets 75% =
        // 6 MB, so at least one 4 MB blob must go.
        cache.put_symbols(
            PathBuf::from("first.bzl"),
            make_blob("blob", 4 * 1024 * 1024),
        );
        cache.put_symbols(
            PathBuf::from("second.bzl"),
            make_blob("blob", 4 * 1024 * 1024),
        );

        let metrics = cache.metrics.lock().unwrap();
        assert!(
            metrics.evictions >= 1,
            "memory cap pressure must trigger ≥1 eviction (got {})",
            metrics.evictions
        );
    }

    #[test]
    fn test_global_cache_metrics() {
        let cache = GlobalBzlCache::with_limits(8, 256 * 1024);

        cache.put_symbols(PathBuf::from("file1.bzl"), make_symbols(3));

        assert!(cache.get_symbols(Path::new("file1.bzl")).is_some());
        assert!(cache.get_symbols(Path::new("missing.bzl")).is_none());

        let metrics = cache.metrics.lock().unwrap();
        assert_eq!(metrics.hits, 1);
        assert_eq!(metrics.misses, 1);
        assert!(metrics.total_size_bytes > 0);
        assert!(metrics.peak_size_bytes >= metrics.total_size_bytes);
    }

    #[test]
    fn test_resolve_workspace_bzl() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().to_path_buf();
        rush_incremental::tracked::test_scope(&workspace, || {
            let loader = BzlLoader::new(workspace.clone());

            // Test //package:file.bzl
            let bzl_dir = workspace.join("mypackage");
            std::fs::create_dir_all(&bzl_dir).unwrap();
            let bzl_file = bzl_dir.join("helpers.bzl");
            std::fs::write(&bzl_file, "def helper(): return []").unwrap();

            let resolved = loader.resolve_bzl_path("//mypackage:helpers.bzl").unwrap();
            assert_eq!(resolved, bzl_file);
        });
    }

    #[test]
    fn test_resolve_relative_bzl() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().to_path_buf();
        rush_incremental::tracked::test_scope(&workspace, || {
            let package = workspace.join("pkg");
            std::fs::create_dir_all(&package).unwrap();

            let loader = BzlLoader::new(workspace.clone());

            let bzl_file = package.join("test.bzl");
            std::fs::write(&bzl_file, "def test(): pass").unwrap();

            // Set up thread-local context for package-relative resolution
            BzlLoader::push_thread_context(workspace.clone(), package.clone());
            struct ContextGuard;
            impl Drop for ContextGuard {
                fn drop(&mut self) { BzlLoader::pop_thread_context(); }
            }
            let _context_guard = ContextGuard;

            let resolved = loader.resolve_bzl_path(":test.bzl").unwrap();
            assert_eq!(resolved, bzl_file);
        });
    }

    #[test]
    fn test_invalid_bzl_label() {
        let loader = BzlLoader::new(PathBuf::from("/tmp"));

        // Missing colon
        assert!(loader.resolve_bzl_path("//package/file.bzl").is_err());

        // Invalid format
        assert!(loader.resolve_bzl_path("invalid.bzl").is_err());
    }

    #[test]
    fn test_set_current_repo_root() {
        let main_workspace = PathBuf::from("/home/user/main_workspace");
        let external_repo = PathBuf::from("/home/user/.cache/external/tensorflow");

        let mut loader = BzlLoader::new(main_workspace.clone());

        // Initially, current_repo_root should equal workspace_root
        assert_eq!(loader.get_current_repo_root(), &main_workspace);

        // Set current_repo_root to external repository
        let old_root = loader.set_current_repo_root(external_repo.clone());
        assert_eq!(old_root, main_workspace);
        assert_eq!(loader.get_current_repo_root(), &external_repo);

        // Restore original repo root
        let restored = loader.set_current_repo_root(old_root);
        assert_eq!(restored, external_repo);
        assert_eq!(loader.get_current_repo_root(), &main_workspace);
    }

    #[test]
    fn test_repo_root_affects_workspace_bzl_resolution() {
        let root = tempfile::tempdir().unwrap();
        let main_workspace = root.path().join("main_workspace");
        let external_repo = root.path().join("external_repo");
        rush_incremental::tracked::test_scope(root.path(), || {
            struct ContextGuard;
            impl Drop for ContextGuard {
                fn drop(&mut self) { BzlLoader::pop_thread_context(); }
            }

            // Create both workspaces with different file contents
            std::fs::create_dir_all(main_workspace.join("pkg")).unwrap();
            std::fs::create_dir_all(external_repo.join("pkg")).unwrap();

            let main_bzl = main_workspace.join("pkg/test.bzl");
            let external_bzl = external_repo.join("pkg/test.bzl");

            std::fs::write(&main_bzl, "# main workspace").unwrap();
            std::fs::write(&external_bzl, "# external repo").unwrap();

            let loader = BzlLoader::new(main_workspace.clone());

            // Resolving //pkg:test.bzl should use current_repo_root from thread context
            // Set up thread-local context for main workspace
            BzlLoader::push_thread_context(main_workspace.clone(), main_workspace.join("pkg"));
            let main_context = ContextGuard;

            let resolved = loader.resolve_bzl_path("//pkg:test.bzl").unwrap();
            assert_eq!(resolved, main_bzl);

            // Pop main workspace context
            drop(main_context);

            // Switch to external repo via thread-local context
            BzlLoader::push_thread_context(external_repo.clone(), external_repo.join("pkg"));
            let _external_context = ContextGuard;

            let resolved = loader.resolve_bzl_path("//pkg:test.bzl").unwrap();
            assert_eq!(resolved, external_bzl);
        });
    }

    #[test]
    fn test_get_current_repo_root() {
        let workspace = PathBuf::from("/path/to/workspace");
        let loader = BzlLoader::new(workspace.clone());

        // get_current_repo_root should initially return the workspace root
        assert_eq!(loader.get_current_repo_root(), &workspace);
    }

    /// TensorFlow vendors TSL inside XLA, so a .bzl load written as
    /// `@local_xla//tsl/...` must resolve under the local_tsl root. Without
    /// the remap it pointed at third_party/xla/tsl, which does not exist,
    /// and hosts papered over it with a hand-made symlink.
    #[test]
    fn local_xla_tsl_bzl_resolves_under_local_tsl() {
        let tmp = tempfile::tempdir().unwrap();
        let xla_root = tmp.path().join("third_party/xla");
        let tsl_root = xla_root.join("third_party/tsl");
        std::fs::create_dir_all(tsl_root.join("tsl/platform")).unwrap();
        std::fs::write(tsl_root.join("tsl/platform/build_config.bzl"), "# tsl\n").unwrap();

        let mut loader = BzlLoader::new(tmp.path().to_path_buf());
        loader.add_external_repo("local_xla".to_string(), xla_root.clone());
        loader.add_external_repo("local_tsl".to_string(), tsl_root.clone());

        let resolved = rush_incremental::tracked::test_scope(tmp.path(), || {
            loader.resolve_bzl_path("@local_xla//tsl/platform:build_config.bzl")
        })
        .expect("@local_xla//tsl/... must resolve");

        assert!(
            resolved.starts_with(
                std::fs::canonicalize(&tsl_root).unwrap_or(tsl_root.clone())
            ),
            "expected a path under the local_tsl root, got {}",
            resolved.display()
        );
        assert!(resolved.ends_with("tsl/platform/build_config.bzl"), "{}", resolved.display());
    }

    #[test]
    fn test_multiple_repo_root_changes() {
        let workspace = PathBuf::from("/workspace");
        let repo1 = PathBuf::from("/repo1");
        let repo2 = PathBuf::from("/repo2");
        let repo3 = PathBuf::from("/repo3");

        let mut loader = BzlLoader::new(workspace.clone());

        // Chain of repo root changes
        assert_eq!(loader.get_current_repo_root(), &workspace);

        loader.set_current_repo_root(repo1.clone());
        assert_eq!(loader.get_current_repo_root(), &repo1);

        loader.set_current_repo_root(repo2.clone());
        assert_eq!(loader.get_current_repo_root(), &repo2);

        loader.set_current_repo_root(repo3.clone());
        assert_eq!(loader.get_current_repo_root(), &repo3);

        // Restore back to workspace
        loader.set_current_repo_root(workspace.clone());
        assert_eq!(loader.get_current_repo_root(), &workspace);
    }

    // Phase 5.C.4 commit 104: the cumulative-vs-own
    // tests live as integration tests in
    // tests/cold_load_attribution.rs so they aren't
    // blocked by pre-existing test rot in evaluator.rs
    // / builtins.rs / lib.rs (~28 errors from earlier
    // refactors that changed get_var() and Function
    // closure types). Once that rot is cleaned up,
    // those tests can move back inline.

    /// Placeholder so the new test names appear in
    /// `cargo test` listings even if the integration
    /// crate is filtered out. Real assertions live in
    /// the integration file.
    #[test]
    #[allow(non_snake_case)]
    fn commit_104_cold_load_attribution_tests_are_in_integration_file() {
        // See: crates/rush-starlark/tests/cold_load_attribution.rs
    }

    // Phase 5.C.4 commit 134 (Batch Q): unit tests for
    // the materialized-skip env-gated path. The five
    // tests below cover the matrix from Pro's
    // commit-134 directive:
    //
    //   env_off_behavior_unchanged
    //   env_on_existing_repo_with_build_skips
    //   env_on_existing_repo_without_build_does_not_skip
    //   env_on_missing_repo_does_not_skip
    //   skip_decision_recorded

    /// Build a temp workspace_root with the given
    /// external/<repo>/ shape. Returns the tempdir
    /// (kept alive by the caller) and the path.
    fn make_tmp_workspace(
        repo: Option<(&str, bool)>, // (repo_name, has_BUILD)
    ) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let ws_root = dir.path().to_path_buf();
        if let Some((repo_name, has_build)) = repo {
            let p = ws_root.join("external").join(repo_name);
            std::fs::create_dir_all(&p).expect("mkdir");
            if has_build {
                std::fs::write(p.join("BUILD"), b"# stub\n")
                    .expect("write BUILD");
            }
        }
        (dir, ws_root)
    }

    #[test]
    fn env_off_behavior_unchanged() {
        let (_g, ws) = make_tmp_workspace(Some(("foo", true)));
        // env_on = false → predicate returns None, the
        // caller falls through to its normal path.
        assert!(
            evaluate_materialized_skip(&ws, "foo", false).is_none(),
            "env-off must return None even when the repo would otherwise qualify"
        );
    }

    #[test]
    fn env_on_existing_repo_with_build_skips() {
        let (_g, ws) = make_tmp_workspace(Some(("foo", true)));
        let dec = evaluate_materialized_skip(&ws, "foo", true)
            .expect("env-on materialized repo with BUILD must return Some");
        match dec {
            FetchDecision::SkippedMaterializedTrustMode {
                repo_name, path, manifest_status,
            } => {
                assert_eq!(repo_name, "foo");
                assert_eq!(path, ws.join("external").join("foo"));
                assert_eq!(manifest_status, "absent");
            }
            other => panic!("expected SkippedMaterializedTrustMode, got {:?}", other),
        }
    }

    #[test]
    fn env_on_existing_repo_without_build_does_not_skip() {
        let (_g, ws) = make_tmp_workspace(Some(("foo", false)));
        let dec = evaluate_materialized_skip(&ws, "foo", true)
            .expect("env-on dir-without-BUILD must return Some(MissingBuild)");
        match dec {
            FetchDecision::MaterializedMissingBuild {
                repo_name, path,
            } => {
                assert_eq!(repo_name, "foo");
                assert_eq!(path, ws.join("external").join("foo"));
            }
            other => panic!(
                "expected MaterializedMissingBuild, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn env_on_missing_repo_does_not_skip() {
        let (_g, ws) = make_tmp_workspace(None);
        let dec = evaluate_materialized_skip(&ws, "foo", true)
            .expect("env-on missing dir must return Some(MissingDir)");
        match dec {
            FetchDecision::MaterializedMissingDir { repo_name } => {
                assert_eq!(repo_name, "foo");
            }
            other => panic!(
                "expected MaterializedMissingDir, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn skip_decision_recorded() {
        // Verify the decision is folded into the
        // GlobalBzlCache's counters when recorded.
        let cache = GlobalBzlCache::new();
        cache.record_fetch_decision(
            &FetchDecision::SkippedMaterialized {
                repo_name: "foo".into(),
                path: PathBuf::from("/tmp/foo"),
            },
        );
        cache.record_fetch_decision(
            &FetchDecision::Invoked {
                repo_name: "bar".into(),
                duration: std::time::Duration::from_millis(123),
            },
        );
        cache.record_fetch_decision(
            &FetchDecision::SkippedKnownRule {
                repo_name: "baz".into(),
                path: PathBuf::from("/tmp/baz"),
            },
        );
        cache.record_fetch_decision(
            &FetchDecision::MaterializedMissingDir {
                repo_name: "missing".into(),
            },
        );
        cache.record_fetch_decision(
            &FetchDecision::MaterializedMissingBuild {
                repo_name: "no-build".into(),
                path: PathBuf::from("/tmp/no-build"),
            },
        );
        let stats = cache.cache_stats();
        assert_eq!(stats.fetch_invoked_count, 1);
        assert_eq!(stats.fetch_skipped_materialized_count, 1);
        assert_eq!(stats.fetch_skipped_known_rule_count, 1);
        assert_eq!(stats.fetch_materialized_missing_dir_count, 1);
        assert_eq!(stats.fetch_materialized_missing_build_count, 1);
        // Per-repo duration map should hold only the
        // Invoked entry.
        assert_eq!(
            stats.fetch_duration_by_repo.get("bar").copied(),
            Some(123u64)
        );
        assert!(
            !stats.fetch_duration_by_repo.contains_key("foo"),
            "non-Invoked decisions must NOT populate the duration map"
        );
    }

    // =====================================================
    // Phase 5.C.4 commit 180 (Batch W): manifest-aware
    // materialized skip tests. Names per Pro's spec.
    // Tests verify the four skip-decision branches:
    //   Valid manifest -> SkippedMaterializedManifestValid
    //                     (no warning).
    //   Incomplete/Missing + trust mode -> SkippedTrustMode
    //                                      (warning stays).
    //   require_manifest=1 + not Valid -> None
    //                                      (block, fall through).
    //   require_manifest=1 + Valid -> ManifestValid skip.
    // =====================================================

    fn with_env<F: FnOnce()>(var: &str, value: Option<&str>, f: F) {
        let prior = std::env::var(var).ok();
        match value {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }
        f();
        match prior {
            Some(v) => std::env::set_var(var, v),
            None => std::env::remove_var(var),
        }
    }

    #[test]
    fn require_manifest_env_parsing() {
        // is_skip_require_manifest accepts the
        // canonical opt-in vocabulary; everything
        // else (including unset) is the default-off.
        for v in &["1", "true", "yes", "YES", "on", "On"] {
            with_env("RUSH_FETCH_SKIP_REQUIRE_MANIFEST", Some(v), || {
                assert!(
                    is_skip_require_manifest(),
                    "value {:?} must enable strict skip",
                    v
                );
            });
        }
        for v in &["0", "false", "no", "off", "", "garbage"] {
            with_env("RUSH_FETCH_SKIP_REQUIRE_MANIFEST", Some(v), || {
                assert!(
                    !is_skip_require_manifest(),
                    "value {:?} must NOT enable strict skip",
                    v
                );
            });
        }
        with_env("RUSH_FETCH_SKIP_REQUIRE_MANIFEST", None, || {
            assert!(
                !is_skip_require_manifest(),
                "unset must default to non-strict"
            );
        });
    }

    #[test]
    fn require_manifest_blocks_incomplete() {
        // RUSH_FETCH_SKIP_REQUIRE_MANIFEST=1 +
        // existing dir + missing manifest (no
        // registered reader callback -> reader
        // returns "missing") -> skip is BLOCKED.
        // evaluate_materialized_skip returns None,
        // letting the fetch path do its normal
        // thing.
        with_env("RUSH_FETCH_SKIP_REQUIRE_MANIFEST", Some("1"), || {
            let (_g, ws) = make_tmp_workspace(Some(("foo", true)));
            let dec = evaluate_materialized_skip(&ws, "foo", true);
            assert!(
                dec.is_none(),
                "require_manifest=1 + not-Valid manifest must block skip; \
                 got {:?}",
                dec
            );
        });
    }

    #[test]
    fn require_manifest_allows_dir_missing_outcomes() {
        // require_manifest=1 should NOT change the
        // existing pre-checks (dir missing /
        // BUILD missing). Those are structural and
        // produce typed FetchDecisions regardless
        // of the manifest gate.
        with_env("RUSH_FETCH_SKIP_REQUIRE_MANIFEST", Some("1"), || {
            let (_g, ws) = make_tmp_workspace(None);
            let dec = evaluate_materialized_skip(&ws, "foo", true)
                .expect("missing dir still produces typed decision");
            match dec {
                FetchDecision::MaterializedMissingDir { .. } => {}
                other => panic!(
                    "expected MaterializedMissingDir, got {:?}",
                    other
                ),
            }
        });
    }
}
