// Rush Build - Dynamic Resource Manager
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Dynamic resource management for build actions.
//!
//! Unlike static semaphores that limit job count, this module tracks actual
//! memory and CPU resources allocated to running actions. This prevents OOM
//! by accounting for heterogeneous resource requirements across different
//! compilation units.
//!
//! ## PSI Memory Monitoring (Three-Tier Approach)
//!
//! On Linux 5.2+, this module uses PSI (Pressure Stall Information) for
//! real-time memory pressure detection with three fallback tiers:
//!
//! 1. **PSI Triggers (Root Only)**: Event-driven pressure detection using kernel
//!    notifications. Requires UID 0 - `setcap` is insufficient. Most responsive.
//!
//! 2. **Passive PSI Polling (Recommended)**: Reads PSI metrics every 50ms without
//!    requiring root. Parses `some avg10` and `full avg10` from `/proc/pressure/memory`.
//!    Nearly as responsive as triggers without privileges. **This is the recommended
//!    approach for non-root users.**
//!
//! 3. **MemInfo Fallback**: Basic capacity-based detection via `/proc/meminfo`.
//!    Used only when PSI is unavailable (kernel < 5.2).
//!
//! The system automatically selects the best available tier at startup.
//!
//! Inspired by Bazel's --local_ram_resources and per-action resource hints.

use anyhow::{bail, Result};
use std::fs::{File, OpenOptions};
#[cfg(target_os = "linux")]
use std::io;
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::io::unix::AsyncFd;
use tokio::sync::{watch, Mutex, OwnedSemaphorePermit, Semaphore};
use tracing::{debug, info, warn};

// Debug logging helper - only outputs if RUSH_DEBUG=1 is set
macro_rules! debug_log {
    ($($arg:tt)*) => {
        if std::env::var("RUSH_DEBUG").unwrap_or_default() == "1" {
            eprintln!($($arg)*);
        }
    };
}

/// Task type for parallelization tracking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskType {
    /// Loading tasks: BUILD parsing, .bzl loading, etc.
    Loading,
    /// Building tasks: compilation, linking, etc.
    Building,
}

/// Resource requirements for a build action.
#[derive(Debug, Clone, Copy)]
pub struct ResourceRequest {
    /// Memory required in MB. Literal requests must use a non-zero value.
    pub memory_mb: usize,

    /// CPU slots required (usually 1). Literal requests must use a non-zero value.
    pub cpu_slots: usize,

    /// Task type for parallelization tracking.
    pub task_type: TaskType,
}

impl ResourceRequest {
    /// Create a new resource request, normalizing either zero quantity to one.
    /// Literal struct construction bypasses this normalization; admission
    /// rejects literal zero quantities at both public entry points.
    pub fn new(memory_mb: usize, cpu_slots: usize, task_type: TaskType) -> Self {
        Self {
            memory_mb: memory_mb.max(1),
            cpu_slots: cpu_slots.max(1),
            task_type,
        }
    }

    fn validate_for_manager(self, hardware_cpu_slots: usize) -> Result<(Self, u32)> {
        if self.memory_mb == 0 {
            bail!("resource request memory_mb must be greater than zero");
        }
        if self.cpu_slots == 0 {
            bail!("resource request cpu_slots must be greater than zero");
        }
        if self.cpu_slots > hardware_cpu_slots {
            bail!(
                "resource request needs {} CPU slots, but hardware capacity is {}",
                self.cpu_slots,
                hardware_cpu_slots
            );
        }
        let semaphore_slots = u32::try_from(self.cpu_slots)
            .map_err(|_| anyhow::anyhow!("resource request CPU slots exceed semaphore capacity"))?;
        Ok((self, semaphore_slots))
    }

    /// Default resource request for C++ compilation.
    /// Tuned based on real-world data: Bazel uses 250MB (too low, causes OOM),
    /// we started at 6GB (safe but slow). 2.5GB is the sweet spot for most
    /// C++ compilations while allowing higher parallelism.
    pub fn cpp_compile() -> Self {
        Self::new(2560, 1, TaskType::Building) // 2.5GB per C++ compile (2560 MB)
    }

    /// Default resource request for C++ linking.
    /// Links are memory-intensive: Bazel estimates 50MB but reality is 8-20GB.
    /// We use 4GB as a balanced estimate that prevents OOM on large binaries
    /// while not being too conservative.
    pub fn cpp_link() -> Self {
        Self::new(4 * 1024, 1, TaskType::Building) // 4GB for linking
    }

    /// Default resource request for Rust compilation.
    pub fn rust_compile() -> Self {
        Self::new(4 * 1024, 1, TaskType::Building) // 4GB per Rust compile
    }

    /// Default resource request for Rust linking.
    pub fn rust_link() -> Self {
        Self::new(3 * 1024, 1, TaskType::Building) // 3GB for linking
    }

    /// Minimal resource request for lightweight actions.
    pub fn minimal() -> Self {
        Self::new(512, 1, TaskType::Building) // 512MB for scripts, etc.
    }

    /// Resource request for BUILD file parsing.
    pub fn build_parse() -> Self {
        Self::new(256, 1, TaskType::Loading) // 256MB for parsing
    }

    /// Resource request for .bzl file loading.
    pub fn bzl_load() -> Self {
        Self::new(512, 1, TaskType::Loading) // 512MB for .bzl loading
    }
}

/// Phase 5.C.4 commit 21: typed action-class seam for permit
/// policy. Replaces ad-hoc mnemonic-string matching scattered
/// across `resource_request_for_request` /
/// `resource_request_for_action` with a single classification
/// step: mnemonic → `ResourceClass` → default `ResourceRequest`.
///
/// Pro's Q5 directive: "Add `ResourceClass` now. Do not tune
/// per-mnemonic limits yet. Before TF-scale scheduler tests,
/// make `ProtocCompile` not permanently indistinguishable from
/// `CppCompile`. Otherwise you will eventually need to grep for
/// mnemonics and retrofit policy in the middle of a TF-scale
/// run, which is not a debugging strategy so much as a
/// punishment."
///
/// Current implementation routes every class through the
/// existing pool (`CppCompile` to the cpp_compile bucket,
/// `Link` to cpp_link, etc.) — no policy tuning. The seam's
/// purpose is to land BEFORE `tf_proto_library` migration adds
/// more codegen actions, so future per-class limits attach to
/// `ResourceClass` values rather than to mnemonic strings.
///
/// Future tuning lives here:
///   `ResourceClass::CodegenProtoc` → low-memory codegen bucket
///     (protoc is lightweight; doesn't compete with CppCompile)
///   `ResourceClass::CodegenTblgen` → separate Tblgen bucket
///     (mlir-tblgen / llvm-tblgen — different memory profile)
///   `ResourceClass::Link` → conservative link permits
///     (long-tail memory risk; 8-20GB peaks)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResourceClass {
    /// `CppCompile` / `CCompile` — C/C++ source compilation.
    /// 2.5GB default per `ResourceRequest::cpp_compile`.
    CppCompile,
    /// Linux Kbuild compiler or assembler action. Kept separate from generic
    /// C++ compilation so measured kernel workloads can use their own permit
    /// policy without weakening the conservative C++ default.
    KernelCompile,
    /// Linux Kbuild final or relocatable link action.
    KernelLink,
    /// Linux Kbuild source or metadata generation action.
    KernelCodegen,
    /// `ProtocCompile` — protoc invocation. Generates .pb.h /
    /// .pb.cc (and .grpc.pb.* when grpc plugin is invoked).
    /// Lightweight; legacy bucket = minimal.
    CodegenProtoc,
    /// `MlirTblgen` / `LlvmTblgen` — gentbl codegen.
    /// Memory profile differs from CppCompile; gets its own
    /// class so future tuning can split.
    CodegenTblgen,
    /// `CcArchive` — `ar rcs` static-library packing.
    /// I/O-bound; minimal memory.
    Archive,
    /// `CcLink` / `Link` / `CppLink` — final link step
    /// (cc_binary, cc_test). 4GB default per
    /// `ResourceRequest::cpp_link`; long-tail memory risk.
    Link,
    /// Fallback for unrecognized mnemonics. Routes to
    /// `ResourceRequest::minimal`.
    Other,
}

impl ResourceClass {
    /// Phase 5.C.4 commit 21: classify a mnemonic. Centralizes
    /// the mnemonic-to-class match that was previously inlined
    /// in two places (`resource_request_for_request` in
    /// rush-exec and `resource_request_for_action` in
    /// rush-build). Future codegen mnemonics
    /// (`MlirTblgen`, etc.) classify here once and propagate.
    pub fn from_mnemonic(mnemonic: &str) -> Self {
        match mnemonic {
            "CppCompile" | "CCompile" => Self::CppCompile,
            "KernelCompile" => Self::KernelCompile,
            "KernelLink" => Self::KernelLink,
            "KernelCodegen" => Self::KernelCodegen,
            "ProtocCompile" => Self::CodegenProtoc,
            "MlirTblgen" | "LlvmTblgen" => Self::CodegenTblgen,
            "CcArchive" => Self::Archive,
            "CcLink" | "Link" | "CppLink" => Self::Link,
            _ => Self::Other,
        }
    }

    /// Phase 5.C.4 commit 21: default `ResourceRequest` for
    /// this class. Identity mapping to the existing pool today
    /// — `CppCompile` → cpp_compile bucket, `Link` → cpp_link,
    /// codegen / archive / other → minimal. Future tuning
    /// (separate Tblgen bucket, conservative link permits, etc.)
    /// changes the right-hand side here without affecting
    /// callers.
    pub fn default_request(self) -> ResourceRequest {
        match self {
            Self::CppCompile => ResourceRequest::cpp_compile(),
            Self::KernelCompile => ResourceRequest::minimal(),
            Self::Link | Self::KernelLink => ResourceRequest::cpp_link(),
            Self::KernelCodegen
            | Self::CodegenProtoc
            | Self::CodegenTblgen
            | Self::Archive
            | Self::Other => {
                ResourceRequest::minimal()
            }
        }
    }
}

// Export memory budget module
pub mod memory_budget;
pub use memory_budget::{GlobalMemoryBudget, MemoryPressure, MemoryReservation};

// Phase 5.C.4 commit 36: tool probe cache skeleton.
pub mod tool_probe;
pub use tool_probe::{
    format_probe_cache, probe_runtime_status, ProbeResult, ProbeSource,
    RuntimeStatus, ToolAuditStatus, ToolId, ToolProbeCache,
};

/// RAII guard for resource allocation.
///
/// Resources must be explicitly released using `release_async()`.
pub struct ResourceHandle {
    manager: Arc<ResourceManager>,
    allocated: ResourceRequest,
    permit: Option<OwnedSemaphorePermit>,
    admission: ResourceAdmissionStats,
}

impl ResourceHandle {
    /// Observed wait and retry counters for this admission.
    pub fn admission_stats(&self) -> ResourceAdmissionStats {
        self.admission
    }

    /// Explicitly release resources asynchronously.
    pub async fn release_async(mut self) {
        debug_log!("[TRACE resource] Releasing resource state before CPU permit");
        self.manager.release_async(self.allocated).await;
        drop(self.permit.take());
        debug_log!("[TRACE resource] Resource state and CPU permit released");
    }
}

/// Measurements collected while one blocking resource request waits.
#[derive(Debug, Clone, Copy, Default)]
pub struct ResourceAdmissionStats {
    /// Total wall time from request validation through final admission.
    pub wait: Duration,
    /// Retries caused by direct or monitored memory pressure.
    pub pressure_retries: u64,
    /// Retries caused by insufficient accounted memory.
    pub memory_retries: u64,
    /// Retries caused by an adaptive CPU limit.
    pub cpu_limit_retries: u64,
    /// Wall time spent awaiting the hardware CPU semaphore.
    pub cpu_permit_wait: Duration,
    /// Retries caused by a failed post-semaphore memory refresh.
    pub memory_refresh_retries: u64,
}

/// Abort an owned background task when its owning resource manager is dropped.
struct BackgroundTask(tokio::task::JoinHandle<()>);

impl BackgroundTask {
    fn new(handle: tokio::task::JoinHandle<()>) -> Self {
        Self(handle)
    }

    #[cfg(test)]
    fn abort_handle(&self) -> tokio::task::AbortHandle {
        self.0.abort_handle()
    }
}

impl Drop for BackgroundTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Dynamic resource manager that tracks memory and CPU availability.
pub struct ResourceManager {
    /// Inner state protected by mutex.
    state: Arc<Mutex<ResourceState>>,

    /// Semaphore for CPU slot limiting.
    cpu_semaphore: Arc<Semaphore>,

    /// Memory pressure notification channel.
    pressure_rx: watch::Receiver<bool>,

    /// Hardware CPU slots detected (upper bound for dynamic throttling).
    hardware_cpu_slots: usize,

    /// Selects host-sampled or explicit capacity accounting.
    memory_mode: MemoryMode,

    /// Issues a total order for memory samples before their reads begin.
    memory_sample_generation: AtomicU64,

    /// PSI producer task owned by this manager.
    _pressure_monitor_task: Option<BackgroundTask>,

    /// Adaptive controller task installed after the manager enters an Arc.
    _pressure_controller_task: StdMutex<Option<BackgroundTask>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MemoryMode {
    System,
    Explicit,
}

#[derive(Debug, Clone, Copy)]
struct MemorySample {
    total_memory_mb: usize,
    available_memory_mb: usize,
    generation: u64,
    started_at: Instant,
}

struct ResourceState {
    /// Immutable upper bound for sampled total and available memory.
    memory_ceiling_mb: usize,

    /// Total system memory in MB (updated periodically).
    total_memory_mb: usize,

    /// Available memory in MB (updated periodically).
    available_memory_mb: usize,

    /// Newest memory sample applied to this state.
    memory_sample_generation: u64,

    /// Currently allocated memory in MB (for running actions).
    allocated_memory_mb: usize,

    /// Cache memory in MB (GlobalBzlCache, BUILD file cache, etc.).
    /// This memory is tracked separately because it's shared across all builds
    /// and can be evicted when pressure is detected.
    cache_memory_mb: usize,

    /// Operator-configured CPU ceiling.
    configured_cpu_slots: usize,

    /// Temporary pressure-controller ceiling, when throttling is active.
    pressure_cpu_slots: Option<usize>,

    /// Currently allocated CPU slots.
    allocated_cpu_slots: usize,

    /// Currently active loading tasks (BUILD parsing, .bzl loading).
    active_loading_tasks: usize,

    /// Currently active building tasks (compilation, linking).
    active_building_tasks: usize,

    /// Last time we checked system memory.
    last_memory_check: Instant,

    /// Memory check interval.
    memory_check_interval: Duration,

    /// Peak allocated memory (for stats).
    peak_memory_mb: usize,

    /// Peak cache memory (for stats).
    peak_cache_mb: usize,

    /// Peak loading tasks.
    peak_loading_tasks: usize,

    /// Peak building tasks.
    peak_building_tasks: usize,

    /// Total number of allocations.
    total_allocations: usize,

    /// Reserved memory safety margin (MB) kept free to avoid thrashing.
    reserve_memory_mb: usize,
}

impl ResourceState {
    fn apply_memory_sample(&mut self, sample: MemorySample) -> bool {
        if sample.generation <= self.memory_sample_generation {
            return false;
        }

        self.total_memory_mb = sample.total_memory_mb.min(self.memory_ceiling_mb);
        self.available_memory_mb = sample
            .available_memory_mb
            .min(self.total_memory_mb)
            .min(self.memory_ceiling_mb);
        self.memory_sample_generation = sample.generation;
        self.last_memory_check = sample.started_at;
        true
    }

    fn effective_cpu_slots(&self) -> usize {
        self.pressure_cpu_slots
            .map_or(self.configured_cpu_slots, |pressure_limit| {
                std::cmp::min(self.configured_cpu_slots, pressure_limit)
            })
    }

    fn memory_budget_mb(&self) -> usize {
        self.available_memory_mb
            .saturating_sub(self.allocated_memory_mb)
            .saturating_sub(self.cache_memory_mb)
            .saturating_sub(self.reserve_memory_mb)
    }

    fn cpu_capacity_available(&self, requested_slots: usize) -> bool {
        requested_slots
            <= self
                .effective_cpu_slots()
                .saturating_sub(self.allocated_cpu_slots)
    }

    fn admission_block(
        &self,
        request: ResourceRequest,
        under_pressure: bool,
    ) -> Option<AdmissionBlock> {
        if under_pressure {
            return Some(AdmissionBlock::Pressure);
        }
        if !self.cpu_capacity_available(request.cpu_slots) {
            return Some(AdmissionBlock::CpuLimit);
        }

        let memory_budget_mb = self.memory_budget_mb();
        if request.memory_mb > memory_budget_mb {
            return Some(AdmissionBlock::Memory { memory_budget_mb });
        }
        None
    }

    fn record_allocation(&mut self, request: ResourceRequest) {
        self.allocated_memory_mb += request.memory_mb;
        self.allocated_cpu_slots += request.cpu_slots;
        self.total_allocations += 1;

        match request.task_type {
            TaskType::Loading => {
                self.active_loading_tasks += 1;
                self.peak_loading_tasks =
                    std::cmp::max(self.peak_loading_tasks, self.active_loading_tasks);
            }
            TaskType::Building => {
                self.active_building_tasks += 1;
                self.peak_building_tasks =
                    std::cmp::max(self.peak_building_tasks, self.active_building_tasks);
            }
        }

        self.peak_memory_mb = std::cmp::max(self.peak_memory_mb, self.allocated_memory_mb);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdmissionBlock {
    Pressure,
    CpuLimit,
    Memory { memory_budget_mb: usize },
}

fn claim_psi_diagnostic_slot(last_diagnostic: &AtomicU64, now: u64, interval: u64) -> bool {
    if interval == 0 {
        return true;
    }

    let mut observed = last_diagnostic.load(Ordering::Relaxed);
    loop {
        if now.saturating_sub(observed) < interval {
            return false;
        }

        match last_diagnostic.compare_exchange_weak(
            observed,
            now,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return true,
            Err(actual) => observed = actual,
        }
    }
}

/// Parsed PSI memory pressure metrics.
#[derive(Debug, Clone, Copy)]
struct PsiMetrics {
    /// "some" avg10: percentage of time at least one task stalled (0.0-1.0)
    some_avg10: f64,
    /// "full" avg10: percentage of time all tasks stalled (0.0-1.0)
    full_avg10: f64,
    /// "some" avg60: 60-second average (0.0-1.0)
    some_avg60: f64,
    /// "full" avg60: 60-second average (0.0-1.0)
    full_avg60: f64,
    /// "some" avg300: 5-minute average (0.0-1.0)
    some_avg300: f64,
    /// "full" avg300: 5-minute average (0.0-1.0)
    full_avg300: f64,
}

impl PsiMetrics {
    /// Read and parse PSI metrics from /proc/pressure/memory
    fn read() -> Result<Self> {
        let file = File::open("/proc/pressure/memory")?;
        let reader = BufReader::new(file);

        let mut some_avg10 = 0.0;
        let mut some_avg60 = 0.0;
        let mut some_avg300 = 0.0;
        let mut full_avg10 = 0.0;
        let mut full_avg60 = 0.0;
        let mut full_avg300 = 0.0;

        for line in reader.lines() {
            let line = line?;
            if line.starts_with("some ") {
                // Parse: "some avg10=0.15 avg60=0.08 avg300=0.05 total=982753551"
                // Kernel reports percentages (0-100), convert to decimal (0-1)
                for part in line.split_whitespace() {
                    if let Some(value_str) = part.strip_prefix("avg10=") {
                        some_avg10 = value_str.parse().unwrap_or(0.0) / 100.0;
                    } else if let Some(value_str) = part.strip_prefix("avg60=") {
                        some_avg60 = value_str.parse().unwrap_or(0.0) / 100.0;
                    } else if let Some(value_str) = part.strip_prefix("avg300=") {
                        some_avg300 = value_str.parse().unwrap_or(0.0) / 100.0;
                    }
                }
            } else if line.starts_with("full ") {
                // Parse: "full avg10=0.00 avg60=0.00 avg300=0.00 total=0"
                // Kernel reports percentages (0-100), convert to decimal (0-1)
                for part in line.split_whitespace() {
                    if let Some(value_str) = part.strip_prefix("avg10=") {
                        full_avg10 = value_str.parse().unwrap_or(0.0) / 100.0;
                    } else if let Some(value_str) = part.strip_prefix("avg60=") {
                        full_avg60 = value_str.parse().unwrap_or(0.0) / 100.0;
                    } else if let Some(value_str) = part.strip_prefix("avg300=") {
                        full_avg300 = value_str.parse().unwrap_or(0.0) / 100.0;
                    }
                }
            }
        }

        Ok(PsiMetrics {
            some_avg10,
            full_avg10,
            some_avg60,
            full_avg60,
            some_avg300,
            full_avg300,
        })
    }

    /// Check for ANY significant memory pressure (AGGRESSIVE defaults).
    /// This triggers CPU throttling and allocation delays.
    fn has_memory_pressure(&self) -> bool {
        // AGGRESSIVE: Trigger on any significant pressure
        self.full_avg10 > 0.02      // 2% full stall right now (was 10%)
        || self.some_avg10 > 0.15    // 15% partial stall right now (was 50%)
        || self.full_avg60 > 0.01    // 1% sustained full stall (NEW)
        || self.some_avg60 > 0.10 // 10% sustained partial stall (NEW)
    }

    /// Check for CRITICAL thrashing (should pause ALL new allocations).
    /// This indicates severe memory starvation - system is actively thrashing.
    ///
    /// Uses a context-aware approach that combines PSI with actual available memory:
    /// - If available memory > 40% of total: Allow PSI up to 30% (plenty of RAM)
    /// - If available memory > 20% of total: Allow PSI up to 20% (moderate RAM)
    /// - If available memory < 20% of total: Strict PSI 10% (low RAM)
    ///
    /// Environment variables (override defaults):
    /// - RUSH_PSI_CRITICAL_FULL10: full_avg10 threshold % (default: context-aware)
    /// - RUSH_PSI_CRITICAL_FULL60: full_avg60 threshold % (default: half of full10)
    /// - RUSH_PSI_CRITICAL_SOME60: some_avg60 threshold % (default: 50%)
    ///
    /// Rationale: PSI measures memory pressure (processes waiting for memory),
    /// not absolute availability. 19% PSI with 47 GB available is NOT thrashing.
    /// We should be conservative when RAM is tight, permissive when it's plentiful.
    ///
    /// Context-aware critical pressure check with memory info.
    /// If mem_info is None, falls back to static thresholds.
    fn has_critical_pressure_with_context(&self, mem_info: Option<(usize, usize)>) -> bool {
        use tracing::error;

        // Get configurable thresholds or use context-aware defaults
        let full10_threshold = if let Some(env_val) = std::env::var("RUSH_PSI_CRITICAL_FULL10")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
        {
            env_val / 100.0
        } else if let Some((total_mb, available_mb)) = mem_info {
            // Context-aware: adjust threshold based on available memory
            let available_pct = (available_mb as f64) / (total_mb as f64);
            if available_pct > 0.40 {
                0.30 // 30% PSI OK when > 40% RAM available
            } else if available_pct > 0.20 {
                0.20 // 20% PSI OK when > 20% RAM available
            } else {
                0.10 // 10% PSI strict when < 20% RAM available
            }
        } else {
            0.20 // Default: 20% if no memory context available (was 15%)
        };

        let full60_threshold = if let Some(env_val) = std::env::var("RUSH_PSI_CRITICAL_FULL60")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
        {
            env_val / 100.0
        } else {
            full10_threshold / 2.0 // Already decimal, don't divide by 100 again
        };

        let some60_threshold = std::env::var("RUSH_PSI_CRITICAL_SOME60")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .map(|v| v / 100.0)
            .unwrap_or(0.50); // Already decimal (50%)

        // Use context-aware full300 threshold (double the full10 threshold)
        // This prevents false positives when the system has plenty of RAM
        let full300_threshold = full10_threshold * 2.0; // 60% when > 40% RAM, 40% when > 20% RAM, 20% when low RAM

        // Check each condition individually for diagnostic purposes
        let full10_exceeded = self.full_avg10 > full10_threshold;
        let full60_exceeded = self.full_avg60 > full60_threshold;
        let some60_exceeded = self.some_avg60 > some60_threshold;
        let full300_exceeded = self.full_avg300 > full300_threshold;

        let is_critical = full10_exceeded || full60_exceeded || some60_exceeded || full300_exceeded;

        // Log diagnostic information when critical pressure is detected,
        // but rate-limit to once every PSI_DIAG_INTERVAL_SECS (default 30 s).
        // The 50 ms poll cadence × ~17 lines per diagnostic block would
        // produce tens of thousands of redundant lines under sustained
        // pressure (observed: 28 GB rush log during one large build).
        // The rate limit keeps the signal but kills
        // the flood. Override via RUSH_PSI_DIAG_INTERVAL_SECS.
        use std::sync::OnceLock;
        static LAST_DIAG_UNIX_SECS: AtomicU64 = AtomicU64::new(0);
        static DIAG_INTERVAL: OnceLock<u64> = OnceLock::new();
        let interval = *DIAG_INTERVAL.get_or_init(|| {
            std::env::var("RUSH_PSI_DIAG_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(30)
        });
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let should_log_diag = is_critical
            && claim_psi_diagnostic_slot(&LAST_DIAG_UNIX_SECS, now_unix, interval);
        if should_log_diag {
            error!("╔════════════════════════════════════════════════════════════════╗");
            error!("║ PSI THRESHOLD DIAGNOSTIC                                       ║");
            error!("╚════════════════════════════════════════════════════════════════╝");

            // Show memory context
            if let Some((total_mb, available_mb)) = mem_info {
                let available_pct = (available_mb as f64) / (total_mb as f64) * 100.0;
                error!(
                    "Memory Context: {} MB / {} MB available ({:.1}% free)",
                    available_mb, total_mb, available_pct
                );
            } else {
                error!("Memory Context: NONE (using static thresholds)");
            }

            // Show calculated thresholds
            error!("Thresholds:");
            error!("  full10:  {:.1}%", full10_threshold * 100.0);
            error!("  full60:  {:.1}%", full60_threshold * 100.0);
            error!("  some60:  {:.1}%", some60_threshold * 100.0);
            error!("  full300: {:.1}%", full300_threshold * 100.0);

            // Show actual PSI values
            error!("Actual PSI:");
            error!(
                "  full10:  {:.2}%  {}",
                self.full_avg10 * 100.0,
                if full10_exceeded {
                    "❌ EXCEEDED"
                } else {
                    "✓ OK"
                }
            );
            error!(
                "  full60:  {:.2}%  {}",
                self.full_avg60 * 100.0,
                if full60_exceeded {
                    "❌ EXCEEDED"
                } else {
                    "✓ OK"
                }
            );
            error!(
                "  some60:  {:.2}%  {}",
                self.some_avg60 * 100.0,
                if some60_exceeded {
                    "❌ EXCEEDED"
                } else {
                    "✓ OK"
                }
            );
            error!(
                "  full300: {:.2}%  {}",
                self.full_avg300 * 100.0,
                if full300_exceeded {
                    "❌ EXCEEDED"
                } else {
                    "✓ OK"
                }
            );

            // Identify which condition triggered
            if full10_exceeded {
                error!(
                    "Triggered by: full10 ({:.2}% > {:.1}%)",
                    self.full_avg10 * 100.0,
                    full10_threshold * 100.0
                );
            }
            if full60_exceeded {
                error!(
                    "Triggered by: full60 ({:.2}% > {:.1}%)",
                    self.full_avg60 * 100.0,
                    full60_threshold * 100.0
                );
            }
            if some60_exceeded {
                error!(
                    "Triggered by: some60 ({:.2}% > {:.1}%)",
                    self.some_avg60 * 100.0,
                    some60_threshold * 100.0
                );
            }
            if full300_exceeded {
                error!(
                    "Triggered by: full300 ({:.2}% > {:.1}%)",
                    self.full_avg300 * 100.0,
                    full300_threshold * 100.0
                );
            }
            error!("");
        }

        is_critical
    }

    /// Check if system has no memory pressure (some < 5%, full == 0)
    fn is_no_pressure(&self) -> bool {
        self.some_avg10 < 0.05
        && self.full_avg10 < 0.01
        && self.some_avg60 < 0.05    // Also check 60-second average
        && self.full_avg60 < 0.01
    }
}

/// Check whether available RAM is below the 4 GB hard floor.
///
/// Startup callers reject a build below this floor. Runtime pressure monitors
/// pause admission and reduce concurrency until memory recovers.
/// Percentage-based budgeting handles ordinary capacity management above the
/// floor.
///
/// Returns (is_critical, available_pct) for error reporting.
pub fn check_ram_floor_critical(mem_info: Option<(usize, usize)>) -> (bool, f64) {
    const HARD_RAM_FLOOR_MB: usize = 4096;

    if let Some((total_mb, available_mb)) = mem_info {
        let available_pct = (available_mb as f64) / (total_mb as f64);
        (available_mb < HARD_RAM_FLOOR_MB, available_pct * 100.0)
    } else {
        // No memory info available - can't check, assume OK
        (false, 100.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PressureLevel {
    Clear,
    Soft,
    Critical,
    RamFloor,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PressureConditions {
    soft: bool,
    critical: bool,
    ram_floor: bool,
}

impl PressureConditions {
    fn level(self) -> PressureLevel {
        PressureLevel::from_conditions(self.soft, self.critical, self.ram_floor)
    }

    fn critical_cleared_since(self, previous: Self) -> bool {
        previous.critical && !self.critical
    }

    fn ram_floor_cleared_since(self, previous: Self) -> bool {
        previous.ram_floor && !self.ram_floor
    }
}

impl PressureLevel {
    fn from_conditions(soft: bool, critical: bool, ram_floor: bool) -> Self {
        if ram_floor {
            Self::RamFloor
        } else if critical {
            Self::Critical
        } else if soft {
            Self::Soft
        } else {
            Self::Clear
        }
    }

    fn is_pressure(self) -> bool {
        self != Self::Clear
    }

    fn is_hard(self) -> bool {
        matches!(self, Self::Critical | Self::RamFloor)
    }
}

fn publish_pressure_level(
    pressure_tx: &watch::Sender<bool>,
    last_level: &mut PressureLevel,
    level: PressureLevel,
) -> bool {
    if level == *last_level {
        return false;
    }

    // watch::Sender::send() wakes receivers even when the Boolean stays true.
    // Preserve that generation event for soft-to-hard and hard-reason changes.
    let _ = pressure_tx.send(level.is_pressure());
    *last_level = level;
    true
}

fn pressure_controller_limit(
    current_limit: usize,
    configured_limit: usize,
    level: PressureLevel,
    healthy_clear_sample: bool,
    pressure_transition: bool,
) -> usize {
    if level.is_hard() {
        1
    } else if level == PressureLevel::Soft && pressure_transition {
        std::cmp::max(1, current_limit / 2)
    } else if healthy_clear_sample {
        std::cmp::min(configured_limit, current_limit.saturating_add(1))
    } else {
        current_limit
    }
}

const PSI_READ_LOG_INTERVAL: Duration = Duration::from_secs(30);
const PRESSURE_RECOVERY_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Default)]
struct PsiReadLogState {
    failed: bool,
    warned_for_episode: bool,
    last_warning: Option<Instant>,
}

impl PsiReadLogState {
    fn record_failure(&mut self, now: Instant) -> bool {
        if self.failed {
            return false;
        }
        self.failed = true;
        let may_warn = self.last_warning.map_or(true, |last| {
            now.duration_since(last) >= PSI_READ_LOG_INTERVAL
        });
        self.warned_for_episode = may_warn;
        if may_warn {
            self.last_warning = Some(now);
        }
        may_warn
    }

    fn record_success(&mut self) -> bool {
        if !self.failed {
            return false;
        }
        self.failed = false;
        let may_log = self.warned_for_episode;
        self.warned_for_episode = false;
        may_log
    }
}

/// Start PSI (Pressure Stall Information) monitoring for memory pressure.
///
/// This implements a three-tier approach:
/// 1. **PSI Triggers (Root Only)**: Event-driven via kernel notifications (optimal)
/// 2. **Passive PSI Polling**: Reads metrics every 50ms (no root required, recommended)
/// 3. **No PSI Background Monitor**: Uses /proc/meminfo accounting only
///
/// Returns the pressure receiver and its owned producer task, when available.
#[cfg(target_os = "linux")]
fn select_psi_file<ReadWrite, ReadOnly>(
    open_read_write: ReadWrite,
    open_read_only: ReadOnly,
) -> io::Result<(File, bool)>
where
    ReadWrite: FnOnce() -> io::Result<File>,
    ReadOnly: FnOnce() -> io::Result<File>,
{
    match open_read_write() {
        Ok(file) => Ok((file, false)),
        Err(read_write_error) => match open_read_only() {
            Ok(file) => Ok((file, true)),
            Err(read_only_error) => Err(io::Error::new(
                read_only_error.kind(),
                format!(
                    "read-write open failed: {}; read-only open failed: {}",
                    read_write_error, read_only_error
                ),
            )),
        },
    }
}

#[cfg(target_os = "linux")]
async fn run_passive_psi_monitor(pressure_tx: watch::Sender<bool>) {
    let mut last_level = PressureLevel::Clear;
    let mut last_conditions = PressureConditions::default();
    let mut read_log_state = PsiReadLogState::default();

    loop {
        if pressure_tx.is_closed() {
            break;
        }

        match PsiMetrics::read() {
            Ok(metrics) => {
                if read_log_state.record_success() {
                    info!("PSI (passive): metric reads recovered");
                }

                let mem_info = ResourceManager::read_system_memory().ok();
                let current_pressure = metrics.has_memory_pressure();
                let current_critical = metrics.has_critical_pressure_with_context(mem_info);
                let (ram_floor_exceeded, available_pct) = check_ram_floor_critical(mem_info);
                let conditions = PressureConditions {
                    soft: current_pressure,
                    critical: current_critical,
                    ram_floor: ram_floor_exceeded,
                };
                let level = conditions.level();

                if level != last_level {
                    if conditions.ram_floor_cleared_since(last_conditions) {
                        info!("PSI (passive): RAM-floor pressure cleared");
                    }
                    if conditions.critical_cleared_since(last_conditions) {
                        info!("PSI (passive): Critical pressure cleared");
                    }

                    match level {
                        PressureLevel::RamFloor => {
                            warn!("╔════════════════════════════════════════════════════════════════╗");
                            warn!("║ RAM FLOOR BREACH - PAUSING NEW ALLOCATIONS                    ║");
                            warn!("╚════════════════════════════════════════════════════════════════╝");
                            if let Some((total, available)) = mem_info {
                                warn!(
                                    "Available RAM: {} MB / {} MB ({:.1}% free)",
                                    available, total, available_pct
                                );
                            }
                            warn!("Hard memory pressure detected; new allocations are paused");
                        }
                        PressureLevel::Critical => {
                            warn!("╔════════════════════════════════════════════════════════════════╗");
                            warn!("║ PSI CRITICAL MEMORY PRESSURE - PAUSING NEW ALLOCATIONS        ║");
                            warn!("╚════════════════════════════════════════════════════════════════╝");
                            warn!("PSI Metrics:");
                            warn!(
                                "  some10={:.2}%  some60={:.2}%  some300={:.2}%",
                                metrics.some_avg10 * 100.0,
                                metrics.some_avg60 * 100.0,
                                metrics.some_avg300 * 100.0
                            );
                            warn!(
                                "  full10={:.2}%  full60={:.2}%  full300={:.2}%",
                                metrics.full_avg10 * 100.0,
                                metrics.full_avg60 * 100.0,
                                metrics.full_avg300 * 100.0
                            );
                            warn!("Hard memory pressure detected; new allocations are paused");
                        }
                        PressureLevel::Soft => {
                            debug!("PSI (passive): Memory pressure detected (some10={:.2}% some60={:.2}%, full10={:.2}% full60={:.2}%)",
                               metrics.some_avg10 * 100.0, metrics.some_avg60 * 100.0,
                               metrics.full_avg10 * 100.0, metrics.full_avg60 * 100.0);
                        }
                        PressureLevel::Clear => {
                            debug!("PSI (passive): Memory pressure cleared");
                        }
                    }

                    publish_pressure_level(&pressure_tx, &mut last_level, level);
                }
                last_conditions = conditions;
            }
            Err(e) => {
                if read_log_state.record_failure(Instant::now()) {
                    warn!(
                        "Failed to read PSI metrics; retaining the last pressure state: {}",
                        e
                    );
                }
            }
        }

        tokio::select! {
            _ = pressure_tx.closed() => break,
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
    }
}

#[cfg(target_os = "linux")]
fn spawn_passive_psi_monitor(pressure_tx: watch::Sender<bool>) -> BackgroundTask {
    BackgroundTask::new(tokio::spawn(run_passive_psi_monitor(pressure_tx)))
}

#[cfg(target_os = "linux")]
async fn run_trigger_psi_monitor<WaitReadable, ReadinessFuture, RunPassive, PassiveFuture>(
    pressure_tx: watch::Sender<bool>,
    mut wait_readable: WaitReadable,
    run_passive: RunPassive,
) where
    WaitReadable: FnMut() -> ReadinessFuture + Send,
    ReadinessFuture: std::future::Future<Output = io::Result<()>> + Send,
    RunPassive: FnOnce(watch::Sender<bool>) -> PassiveFuture + Send,
    PassiveFuture: std::future::Future<Output = ()> + Send,
{
    let readiness_error = loop {
        let readiness = tokio::select! {
            _ = pressure_tx.closed() => break None,
            result = wait_readable() => result,
        };
        match readiness {
            Ok(()) => {
                let _ = pressure_tx.send(true);
                debug!("PSI (trigger): Memory pressure detected!");

                tokio::select! {
                    _ = pressure_tx.closed() => break None,
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                }
                let _ = pressure_tx.send(false);
                debug!("PSI (trigger): Pressure-event hold expired");
            }
            Err(error) => break Some(error),
        }
    };

    if let Some(error) = readiness_error {
        drop(wait_readable);
        warn!(
            "PSI trigger readiness failed: {}. Using passive PSI polling (50ms).",
            error
        );
        run_passive(pressure_tx).await;
    }
}

#[cfg(target_os = "linux")]
fn spawn_trigger_psi_monitor(
    async_fd: AsyncFd<File>,
    pressure_tx: watch::Sender<bool>,
) -> BackgroundTask {
    let async_fd = Arc::new(async_fd);
    BackgroundTask::new(tokio::spawn(run_trigger_psi_monitor(
        pressure_tx,
        move || {
            let async_fd = Arc::clone(&async_fd);
            async move {
                let mut guard = async_fd.readable().await?;
                guard.clear_ready();
                Ok(())
            }
        },
        run_passive_psi_monitor,
    )))
}

#[cfg(target_os = "linux")]
fn spawn_registered_psi_monitor_with<Register, SpawnTrigger, SpawnPassive>(
    file: File,
    pressure_tx: watch::Sender<bool>,
    register: Register,
    spawn_trigger: SpawnTrigger,
    spawn_passive: SpawnPassive,
) -> BackgroundTask
where
    Register: FnOnce(File) -> io::Result<AsyncFd<File>>,
    SpawnTrigger: FnOnce(AsyncFd<File>, watch::Sender<bool>) -> BackgroundTask,
    SpawnPassive: FnOnce(watch::Sender<bool>) -> BackgroundTask,
{
    match register(file) {
        Ok(async_fd) => {
            info!("PSI memory pressure monitoring enabled via triggers (threshold: 200ms/2s)");
            spawn_trigger(async_fd, pressure_tx)
        }
        Err(error) => {
            warn!(
                "Failed to register PSI trigger: {}. Using passive PSI polling (50ms).",
                error
            );
            spawn_passive(pressure_tx)
        }
    }
}

fn start_psi_monitor() -> Result<(watch::Receiver<bool>, Option<BackgroundTask>)> {
    let (pressure_tx, pressure_rx) = watch::channel(false);

    #[cfg(target_os = "linux")]
    {
        // Tier 1: Try PSI triggers (requires root)
        let (file, passive_only) = match select_psi_file(
            || {
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open("/proc/pressure/memory")
            },
            || File::open("/proc/pressure/memory"),
        ) {
            Ok(result) => result,
            Err(e) => {
                warn!(
                    "PSI not available (kernel < 5.2?): {}. No PSI background monitoring.",
                    e
                );
                return Ok((pressure_rx, None));
            }
        };

        // Try to configure trigger: 200ms of memory stall within 2 second window
        let trigger = b"some 200000 2000000";
        let mut file_mut = file;
        let trigger_error = if passive_only {
            None
        } else {
            file_mut.write_all(trigger).err()
        };
        if passive_only || trigger_error.is_some() {
            // Tier 2: PSI triggers failed, use passive polling instead
            if passive_only {
                info!("PSI triggers unavailable. Using passive PSI polling (50ms).");
            } else if trigger_error
                .as_ref()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::InvalidInput)
            {
                info!(
                    "PSI triggers unavailable (requires root). Using passive PSI polling (50ms)."
                );
            } else {
                warn!(
                    "Failed to set PSI trigger: {}. Using passive PSI polling.",
                    trigger_error.expect("trigger error was checked")
                );
            }

            return Ok((pressure_rx, Some(spawn_passive_psi_monitor(pressure_tx))));
        }

        let task = spawn_registered_psi_monitor_with(
            file_mut,
            pressure_tx,
            AsyncFd::new,
            spawn_trigger_psi_monitor,
            spawn_passive_psi_monitor,
        );
        return Ok((pressure_rx, Some(task)));
    }

    #[cfg(not(target_os = "linux"))]
    {
        warn!("PSI monitoring only available on Linux.");
        return Ok((pressure_rx, None));
    }
}

#[derive(Debug, Clone, Copy)]
struct PressureControllerSample {
    level: PressureLevel,
    healthy_clear_sample: bool,
}

fn read_pressure_controller_sample(pressure: bool) -> PressureControllerSample {
    #[cfg(target_os = "linux")]
    let metrics = PsiMetrics::read().ok();
    #[cfg(not(target_os = "linux"))]
    let metrics: Option<PsiMetrics> = None;

    let mem_info = ResourceManager::read_system_memory().ok();
    let soft = pressure
        || metrics
            .as_ref()
            .is_some_and(PsiMetrics::has_memory_pressure);
    let critical = metrics
        .as_ref()
        .is_some_and(|m| m.has_critical_pressure_with_context(mem_info));
    let (ram_floor, _) = check_ram_floor_critical(mem_info);

    PressureControllerSample {
        level: PressureLevel::from_conditions(soft, critical, ram_floor),
        healthy_clear_sample: !pressure
            && !ram_floor
            && mem_info.is_some()
            && metrics.as_ref().is_some_and(PsiMetrics::is_no_pressure),
    }
}

async fn run_pressure_controller_with_sampler<Sample>(
    mgr_weak: std::sync::Weak<ResourceManager>,
    mut rx: watch::Receiver<bool>,
    recovery_interval: Duration,
    mut sample_pressure: Sample,
) where
    Sample: FnMut(bool) -> PressureControllerSample + Send,
{
    let mut poll_pressure_state = false;
    let mut last_level = PressureLevel::Clear;

    loop {
        if poll_pressure_state {
            tokio::select! {
                biased;
                changed = rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                _ = tokio::time::sleep(recovery_interval) => {}
            }
        } else if rx.changed().await.is_err() {
            break;
        }

        let Some(mgr) = mgr_weak.upgrade() else {
            break;
        };
        let pressure = *rx.borrow();
        let sample = sample_pressure(pressure);
        let pressure_transition = sample.level != last_level;

        let mut state = mgr.state.lock().await;
        let old_limit = state.effective_cpu_slots();
        let configured_limit = state.configured_cpu_slots;
        let new_limit = pressure_controller_limit(
            old_limit,
            configured_limit,
            sample.level,
            sample.healthy_clear_sample,
            pressure_transition,
        );
        let recovered = sample.level == PressureLevel::Clear
            && sample.healthy_clear_sample
            && new_limit >= configured_limit;
        if sample.level.is_pressure() || (state.pressure_cpu_slots.is_some() && !recovered) {
            state.pressure_cpu_slots = Some(new_limit);
        } else {
            state.pressure_cpu_slots = None;
        }
        let effective_limit = state.effective_cpu_slots();

        if effective_limit < old_limit {
            if sample.level.is_hard() {
                warn!(
                    "PSI controller: {:?} pressure; lowering the recovery concurrency limit {} -> {}",
                    sample.level, old_limit, effective_limit
                );
            } else {
                debug!(
                    "PSI controller: soft pressure; lowering the recovery concurrency limit {} -> {}",
                    old_limit, effective_limit
                );
            }
        } else if effective_limit > old_limit {
            debug!(
                "PSI controller: healthy clear sample; increasing the recovery concurrency limit {} -> {}",
                old_limit, effective_limit
            );
        }
        poll_pressure_state = state.pressure_cpu_slots.is_some();
        last_level = sample.level;
    }
}

async fn run_pressure_controller(
    mgr_weak: std::sync::Weak<ResourceManager>,
    rx: watch::Receiver<bool>,
) {
    run_pressure_controller_with_sampler(
        mgr_weak,
        rx,
        PRESSURE_RECOVERY_INTERVAL,
        read_pressure_controller_sample,
    )
    .await;
}

fn allocation_has_critical_pressure() -> bool {
    #[cfg(target_os = "linux")]
    if let Ok(metrics) = PsiMetrics::read() {
        let mem_info = ResourceManager::read_system_memory().ok();
        if metrics.has_critical_pressure_with_context(mem_info) {
            debug!(
                "PSI critical during allocation (full60={:.2}% full300={:.2}%) - waiting",
                metrics.full_avg60 * 100.0,
                metrics.full_avg300 * 100.0
            );
            return true;
        }
    }

    false
}

impl ResourceManager {
    fn read_memory_sample_with<ReadMemory>(
        &self,
        read_memory: &mut ReadMemory,
    ) -> Result<MemorySample>
    where
        ReadMemory: FnMut() -> Result<(usize, usize)>,
    {
        let generation = self
            .memory_sample_generation
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                current.checked_add(1)
            })
            .map(|previous| previous + 1)
            .map_err(|_| anyhow::anyhow!("memory sample generation exhausted"))?;
        let started_at = Instant::now();
        let (total_memory_mb, available_memory_mb) = read_memory()?;
        Ok(MemorySample {
            total_memory_mb,
            available_memory_mb,
            generation,
            started_at,
        })
    }

    /// Create a new resource manager with automatic system detection.
    pub fn new() -> Result<Arc<Self>> {
        let cpu_count = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);

        let (total_memory_mb, available_memory_mb) = Self::read_system_memory()?;

        info!(
            "ResourceManager initialized: {}MB total, {}MB available, {} CPUs",
            total_memory_mb, available_memory_mb, cpu_count
        );

        let (pressure_rx, pressure_monitor_task) = start_psi_monitor()?;
        let mgr = Arc::new(Self {
            state: Arc::new(Mutex::new(ResourceState {
                memory_ceiling_mb: total_memory_mb,
                total_memory_mb,
                available_memory_mb,
                memory_sample_generation: 0,
                allocated_memory_mb: 0,
                cache_memory_mb: 0,
                configured_cpu_slots: cpu_count,
                pressure_cpu_slots: None,
                allocated_cpu_slots: 0,
                active_loading_tasks: 0,
                active_building_tasks: 0,
                last_memory_check: Instant::now(),
                memory_check_interval: Duration::from_secs(5),
                peak_memory_mb: 0,
                peak_cache_mb: 0,
                peak_loading_tasks: 0,
                peak_building_tasks: 0,
                total_allocations: 0,
                // Reserve ~10% of total memory, at least 1024MB, at most 8192MB
                reserve_memory_mb: (total_memory_mb / 10).clamp(1024, 8192),
            })),
            cpu_semaphore: Arc::new(Semaphore::new(cpu_count)),
            pressure_rx,
            hardware_cpu_slots: cpu_count,
            memory_mode: MemoryMode::System,
            memory_sample_generation: AtomicU64::new(0),
            _pressure_monitor_task: pressure_monitor_task,
            _pressure_controller_task: StdMutex::new(None),
        });

        let controller = BackgroundTask::new(tokio::spawn(run_pressure_controller(
            Arc::downgrade(&mgr),
            mgr.pressure_rx.clone(),
        )));
        *mgr._pressure_controller_task
            .lock()
            .expect("pressure controller task mutex poisoned") = Some(controller);

        Ok(mgr)
    }

    /// Create a resource manager with exact, host-independent limits.
    ///
    /// PSI pressure monitoring remains active, but host capacity samples do
    /// not alter the explicit memory ceiling or available-memory accounting.
    pub fn with_limits(max_memory_mb: usize, max_cpu_slots: usize) -> Arc<Self> {
        let max_cpu_slots = max_cpu_slots.max(1);
        info!(
            "ResourceManager initialized with explicit limits: {}MB memory, {} CPUs",
            max_memory_mb, max_cpu_slots
        );

        let (pressure_rx, pressure_monitor_task) = start_psi_monitor().unwrap_or_else(|_| {
            let (_, rx) = watch::channel(false);
            (rx, None)
        });

        Arc::new(Self {
            state: Arc::new(Mutex::new(ResourceState {
                memory_ceiling_mb: max_memory_mb,
                total_memory_mb: max_memory_mb,
                available_memory_mb: max_memory_mb,
                memory_sample_generation: 0,
                allocated_memory_mb: 0,
                cache_memory_mb: 0,
                configured_cpu_slots: max_cpu_slots,
                pressure_cpu_slots: None,
                allocated_cpu_slots: 0,
                active_loading_tasks: 0,
                active_building_tasks: 0,
                last_memory_check: Instant::now(),
                memory_check_interval: Duration::from_secs(5),
                peak_memory_mb: 0,
                peak_cache_mb: 0,
                peak_loading_tasks: 0,
                peak_building_tasks: 0,
                total_allocations: 0,
                reserve_memory_mb: 0,
            })),
            cpu_semaphore: Arc::new(Semaphore::new(max_cpu_slots)),
            pressure_rx,
            hardware_cpu_slots: max_cpu_slots,
            memory_mode: MemoryMode::Explicit,
            memory_sample_generation: AtomicU64::new(0),
            _pressure_monitor_task: pressure_monitor_task,
            _pressure_controller_task: StdMutex::new(None),
        })
    }

    /// Fast synchronous check for whether the PSI memory-pressure
    /// channel has flagged the system as under pressure.
    ///
    /// Designed as a cheap gate for hot-path callers (e.g. every
    /// `parse_build_file_cached` invocation) that want to skip the
    /// full `acquire`/`release` round-trip when the system is idle.
    /// Reads a `watch::Receiver` — just a borrow, no syscall, no
    /// tokio park. Passive mode normally refreshes the channel on a 50 ms
    /// cadence, plus scheduler and read latency. Trigger mode updates it from
    /// kernel events and holds each event for 500 ms. A failed passive read
    /// retains the last successfully published state until reads recover.
    pub fn is_under_pressure(&self) -> bool {
        *self.pressure_rx.borrow()
    }

    /// Fixed CPU semaphore capacity detected when the manager was created.
    pub fn hardware_cpu_slots(&self) -> usize {
        self.hardware_cpu_slots
    }

    /// Acquire resources for a build action.
    ///
    /// Blocks until sufficient resources are available. Rejects literal
    /// requests with zero memory, zero CPU slots, or more CPU slots than the
    /// manager's hardware capacity.
    pub async fn acquire(self: &Arc<Self>, request: ResourceRequest) -> Result<ResourceHandle> {
        self.acquire_with_hooks(
            request,
            allocation_has_critical_pressure,
            Self::read_system_memory,
            || {},
            || {},
        )
        .await
    }

    async fn acquire_with_hooks<CriticalPressure, ReadMemory, BeforeCpuWait, BeforeFinalPressure>(
        self: &Arc<Self>,
        request: ResourceRequest,
        mut critical_pressure: CriticalPressure,
        mut read_memory: ReadMemory,
        mut before_cpu_wait: BeforeCpuWait,
        mut before_final_pressure: BeforeFinalPressure,
    ) -> Result<ResourceHandle>
    where
        CriticalPressure: FnMut() -> bool + Send,
        ReadMemory: FnMut() -> Result<(usize, usize)> + Send,
        BeforeCpuWait: FnMut() + Send,
        BeforeFinalPressure: FnMut() + Send,
    {
        let (request, cpu_slots) = request.validate_for_manager(self.hardware_cpu_slots)?;
        let admission_start = Instant::now();
        let mut admission = ResourceAdmissionStats::default();

        // Wait for memory to be available WITHOUT holding CPU permit
        loop {
            // Check the monitor's effective soft/critical pressure state.
            let under_pressure = *self.pressure_rx.borrow();

            if critical_pressure() {
                admission.pressure_retries += 1;
                // Wait longer under critical pressure, let current jobs complete
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }

            if under_pressure {
                admission.pressure_retries += 1;
                debug!("PSI: Memory pressure detected, waiting before allocation");
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }

            // Check memory availability
            let memory_available = {
                let mut state = self.state.lock().await;

                // Periodically refresh capacity only for system-managed limits.
                if self.memory_mode == MemoryMode::System
                    && state.last_memory_check.elapsed() > state.memory_check_interval
                {
                    if let Ok(sample) = self.read_memory_sample_with(&mut read_memory) {
                        if state.apply_memory_sample(sample) {
                            debug!(
                                "Memory refresh: {} MB available",
                                state.available_memory_mb
                            );
                        }
                    }
                }

                // Check if we have enough memory budget, keeping a safety reserve
                // Include cache memory (GlobalBzlCache, etc.) in the budget calculation
                let memory_budget = state.memory_budget_mb();
                request.memory_mb <= memory_budget
            };

            if !memory_available {
                admission.memory_retries += 1;
                let state = self.state.lock().await;
                let memory_budget = state.memory_budget_mb();
                debug!("Waiting for memory: requested {} MB, budget {} MB (allocated: {} MB, cache: {} MB / available: {} MB)",
                       request.memory_mb, memory_budget, state.allocated_memory_mb, state.cache_memory_mb, state.available_memory_mb);
                drop(state);

                // Wait before retrying (without holding CPU permit)
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }

            // The semaphore already enforces the normal hardware limit and
            // wakes waiters immediately when permits are released. Poll only
            // while the configured or pressure limit is below that capacity;
            // polling at the hardware limit inserted a 50 ms refill bubble
            // between compiler waves.
            let dynamic_limit_reached = {
                let state = self.state.lock().await;
                state.effective_cpu_slots() < self.hardware_cpu_slots
                    && !state.cpu_capacity_available(request.cpu_slots)
            };
            if dynamic_limit_reached {
                admission.cpu_limit_retries += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }

            // Memory is available, now acquire CPU slots.
            before_cpu_wait();
            let cpu_wait_start = Instant::now();
            let permit = Arc::clone(&self.cpu_semaphore)
                .acquire_many_owned(cpu_slots)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to acquire CPU slot: {}", e))?;
            admission.cpu_permit_wait += cpu_wait_start.elapsed();

            // Re-read direct PSI after a potentially long semaphore wait rather
            // than relying on a possibly stale background-monitor sample.
            if critical_pressure() {
                admission.pressure_retries += 1;
                drop(permit);
                debug!("PSI became critical while waiting for CPU; retrying allocation");
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }

            // Refresh host capacity after the wait in system mode. Explicit
            // managers revalidate their fixed accounting without host I/O.
            let memory_sample = if self.memory_mode == MemoryMode::System {
                match self.read_memory_sample_with(&mut read_memory) {
                    Ok(sample) => Some(sample),
                    Err(error) => {
                        admission.memory_refresh_retries += 1;
                        drop(permit);
                        debug!(
                            "Failed to refresh memory after waiting for CPU; retrying allocation: {}",
                            error
                        );
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                }
            } else {
                None
            };

            // Revalidate every admission gate against current accounting.
            // Pressure and adaptive limits may also have changed while queued.
            let admission_block = {
                let mut state = self.state.lock().await;
                if let Some(memory_sample) = memory_sample {
                    if !state.apply_memory_sample(memory_sample) {
                        debug!(
                            "Ignored stale memory sample generation {} (current {})",
                            memory_sample.generation, state.memory_sample_generation
                        );
                    }
                }
                let under_pressure = *self.pressure_rx.borrow();
                let mut block = state.admission_block(request, under_pressure);
                if block.is_none() {
                    before_final_pressure();
                    if critical_pressure() || *self.pressure_rx.borrow() {
                        block = Some(AdmissionBlock::Pressure);
                    }
                }
                if block.is_none() {
                    state.record_allocation(request);
                    debug!(
                        "Allocated: {} MB, {} CPU (total: {} MB, {} CPU, Loading: {}, Building: {})",
                        request.memory_mb,
                        request.cpu_slots,
                        state.allocated_memory_mb,
                        state.allocated_cpu_slots,
                        state.active_loading_tasks,
                        state.active_building_tasks
                    );
                }
                block
            };

            if admission_block.is_none() {
                admission.wait = admission_start.elapsed();
                return Ok(ResourceHandle {
                    manager: Arc::clone(self),
                    allocated: request,
                    permit: Some(permit),
                    admission,
                });
            }

            drop(permit);
            match admission_block.expect("blocked admission was checked") {
                AdmissionBlock::Pressure => {
                    admission.pressure_retries += 1;
                    debug!("PSI pressure began while waiting for CPU; retrying allocation");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                AdmissionBlock::CpuLimit => {
                    admission.cpu_limit_retries += 1;
                    debug!("CPU recovery limit changed while waiting; retrying allocation");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                AdmissionBlock::Memory { memory_budget_mb } => {
                    admission.memory_retries += 1;
                    debug!(
                        "Memory changed while waiting for CPU: requested {} MB, budget {} MB; retrying",
                        request.memory_mb, memory_budget_mb
                    );
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    /// Try to acquire resources without blocking. Applies the same request
    /// validation as [`Self::acquire`].
    pub fn try_acquire(
        self: &Arc<Self>,
        request: ResourceRequest,
    ) -> Result<Option<ResourceHandle>> {
        self.try_acquire_with_hooks(
            request,
            allocation_has_critical_pressure,
            Self::read_system_memory,
        )
    }

    #[cfg(test)]
    fn try_acquire_with_critical_pressure<CriticalPressure>(
        self: &Arc<Self>,
        request: ResourceRequest,
        critical_pressure: CriticalPressure,
    ) -> Result<Option<ResourceHandle>>
    where
        CriticalPressure: FnMut() -> bool,
    {
        self.try_acquire_with_hooks(request, critical_pressure, Self::read_system_memory)
    }

    fn try_acquire_with_hooks<CriticalPressure, ReadMemory>(
        self: &Arc<Self>,
        request: ResourceRequest,
        mut critical_pressure: CriticalPressure,
        mut read_memory: ReadMemory,
    ) -> Result<Option<ResourceHandle>>
    where
        CriticalPressure: FnMut() -> bool,
        ReadMemory: FnMut() -> Result<(usize, usize)>,
    {
        let (request, cpu_slots) = request.validate_for_manager(self.hardware_cpu_slots)?;

        if *self.pressure_rx.borrow() || critical_pressure() {
            return Ok(None);
        }

        let permit = match self
            .cpu_semaphore
            .clone()
            .try_acquire_many_owned(cpu_slots)
        {
            Ok(p) => p,
            Err(_) => return Ok(None),
        };

        let refresh_due = match self.state.try_lock() {
            Ok(state) => {
                self.memory_mode == MemoryMode::System
                    && state.last_memory_check.elapsed() > state.memory_check_interval
            }
            Err(_) => {
                drop(permit);
                return Ok(None);
            }
        };

        let memory_sample = if refresh_due {
            match self.read_memory_sample_with(&mut read_memory) {
                Ok(sample) => Some(sample),
                Err(_) => {
                    drop(permit);
                    return Ok(None);
                }
            }
        } else {
            None
        };

        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(_) => {
                drop(permit);
                return Ok(None);
            }
        };

        if let Some(sample) = memory_sample {
            state.apply_memory_sample(sample);
        }

        let under_pressure = *self.pressure_rx.borrow();
        if state.admission_block(request, under_pressure).is_some() {
            drop(permit);
            return Ok(None);
        }

        if *self.pressure_rx.borrow() {
            drop(permit);
            return Ok(None);
        }

        // Keep the authoritative direct predicate adjacent to accounting so
        // mutex acquisition and memory refresh cannot open an unchecked gap.
        if critical_pressure() {
            drop(permit);
            return Ok(None);
        }

        state.record_allocation(request);
        Ok(Some(ResourceHandle {
            manager: Arc::clone(self),
            allocated: request,
            permit: Some(permit),
            admission: ResourceAdmissionStats::default(),
        }))
    }

    /// Release resources asynchronously.
    async fn release_async(&self, allocated: ResourceRequest) {
        debug_log!("[TRACE resource] About to acquire state lock in release_async");
        let mut state = self.state.lock().await;
        debug_log!("[TRACE resource] Acquired state lock in release_async");
        state.allocated_memory_mb = state
            .allocated_memory_mb
            .saturating_sub(allocated.memory_mb);
        state.allocated_cpu_slots = state
            .allocated_cpu_slots
            .saturating_sub(allocated.cpu_slots);

        // Decrement task counters
        match allocated.task_type {
            TaskType::Loading => {
                state.active_loading_tasks = state.active_loading_tasks.saturating_sub(1);
            }
            TaskType::Building => {
                state.active_building_tasks = state.active_building_tasks.saturating_sub(1);
            }
        }

        debug!(
            "Released: {} MB, {} CPU (remaining: {} MB, {} CPU, Loading: {}, Building: {})",
            allocated.memory_mb,
            allocated.cpu_slots,
            state.allocated_memory_mb,
            state.allocated_cpu_slots,
            state.active_loading_tasks,
            state.active_building_tasks
        );
        debug_log!("[TRACE resource] Completed release_async");
    }

    /// Get current resource statistics (async version).
    pub async fn stats(&self) -> ResourceStats {
        let state = self.state.lock().await;
        ResourceStats {
            total_memory_mb: state.total_memory_mb,
            available_memory_mb: state.available_memory_mb,
            allocated_memory_mb: state.allocated_memory_mb,
            cache_memory_mb: state.cache_memory_mb,
            peak_memory_mb: state.peak_memory_mb,
            peak_cache_mb: state.peak_cache_mb,
            max_cpu_slots: state.effective_cpu_slots(),
            allocated_cpu_slots: state.allocated_cpu_slots,
            active_loading_tasks: state.active_loading_tasks,
            active_building_tasks: state.active_building_tasks,
            peak_loading_tasks: state.peak_loading_tasks,
            peak_building_tasks: state.peak_building_tasks,
            total_allocations: state.total_allocations,
        }
    }

    /// Get current resource statistics (sync version for non-async contexts).
    ///
    /// Note: This will panic if called from an async context. Use `stats()` instead.
    pub fn stats_sync(&self) -> ResourceStats {
        let state = self.state.blocking_lock();
        ResourceStats {
            total_memory_mb: state.total_memory_mb,
            available_memory_mb: state.available_memory_mb,
            allocated_memory_mb: state.allocated_memory_mb,
            cache_memory_mb: state.cache_memory_mb,
            peak_memory_mb: state.peak_memory_mb,
            peak_cache_mb: state.peak_cache_mb,
            max_cpu_slots: state.effective_cpu_slots(),
            allocated_cpu_slots: state.allocated_cpu_slots,
            active_loading_tasks: state.active_loading_tasks,
            active_building_tasks: state.active_building_tasks,
            peak_loading_tasks: state.peak_loading_tasks,
            peak_building_tasks: state.peak_building_tasks,
            total_allocations: state.total_allocations,
        }
    }

    /// Update cache memory usage (async version).
    ///
    /// External caches (GlobalBzlCache, etc.) should periodically report their memory usage.
    pub async fn update_cache_memory(&self, cache_memory_mb: usize) {
        let mut state = self.state.lock().await;
        state.cache_memory_mb = cache_memory_mb;
        if cache_memory_mb > state.peak_cache_mb {
            state.peak_cache_mb = cache_memory_mb;
        }
        debug!(
            "Cache memory updated: {} MB (peak: {} MB)",
            cache_memory_mb, state.peak_cache_mb
        );
    }

    /// Update cache memory usage (sync version).
    pub fn update_cache_memory_sync(&self, cache_memory_mb: usize) {
        match self.state.try_lock() {
            Ok(mut state) => {
                state.cache_memory_mb = cache_memory_mb;
                if cache_memory_mb > state.peak_cache_mb {
                    state.peak_cache_mb = cache_memory_mb;
                }
                debug!(
                    "Cache memory updated: {} MB (peak: {} MB)",
                    cache_memory_mb, state.peak_cache_mb
                );
            }
            Err(_) => {
                // Async fallback
                let state_arc = self.state.clone();
                tokio::spawn(async move {
                    let mut state = state_arc.lock().await;
                    state.cache_memory_mb = cache_memory_mb;
                    if cache_memory_mb > state.peak_cache_mb {
                        state.peak_cache_mb = cache_memory_mb;
                    }
                    debug!(
                        "Cache memory updated (async): {} MB (peak: {} MB)",
                        cache_memory_mb, state.peak_cache_mb
                    );
                });
            }
        }
    }

    /// Dynamically adjust the maximum CPU slots (without freezing memory stats) - async.
    pub async fn set_max_cpu_slots(&self, new_limit: usize) {
        let mut state = self.state.lock().await;
        let new_limit = std::cmp::min(new_limit.max(1), self.hardware_cpu_slots);
        let old_configured = state.configured_cpu_slots;
        let old_effective = state.effective_cpu_slots();
        state.configured_cpu_slots = new_limit;
        debug!(
            "set_max_cpu_slots: configured {} -> {}, effective {} -> {} (async)",
            old_configured,
            new_limit,
            old_effective,
            state.effective_cpu_slots()
        );
    }

    /// Dynamically adjust the maximum CPU slots (without freezing memory stats) - sync.
    pub fn set_max_cpu_slots_sync(&self, new_limit: usize) {
        let new_limit = std::cmp::min(new_limit.max(1), self.hardware_cpu_slots);
        // Prefer non-blocking update if mutex is free
        #[allow(clippy::match_like_matches_macro)]
        match self.state.try_lock() {
            Ok(mut state) => {
                let old_configured = state.configured_cpu_slots;
                let old_effective = state.effective_cpu_slots();
                state.configured_cpu_slots = new_limit;
                debug!(
                    "set_max_cpu_slots: configured {} -> {}, effective {} -> {} (sync, try_lock)",
                    old_configured,
                    new_limit,
                    old_effective,
                    state.effective_cpu_slots()
                );
            }
            Err(_) => {
                // We are likely in an async runtime or lock is held; avoid blocking.
                // Apply asynchronously without blocking the caller.
                let state_arc = self.state.clone();
                tokio::spawn(async move {
                    let mut state = state_arc.lock().await;
                    let old_configured = state.configured_cpu_slots;
                    let old_effective = state.effective_cpu_slots();
                    state.configured_cpu_slots = new_limit;
                    debug!(
                        "set_max_cpu_slots: configured {} -> {}, effective {} -> {} (async fallback)",
                        old_configured,
                        new_limit,
                        old_effective,
                        state.effective_cpu_slots()
                    );
                });
            }
        }
    }

    /// Read system memory from /proc/meminfo (Linux).
    pub fn read_system_memory() -> Result<(usize, usize)> {
        #[cfg(target_os = "linux")]
        {
            let contents = std::fs::read_to_string("/proc/meminfo")?;

            let mut total_mb = 0;
            let mut available_mb = 0;

            for line in contents.lines() {
                if line.starts_with("MemTotal:") {
                    if let Some(kb_str) = line.split_whitespace().nth(1) {
                        if let Ok(kb) = kb_str.parse::<usize>() {
                            total_mb = kb / 1024;
                        }
                    }
                } else if line.starts_with("MemAvailable:") {
                    if let Some(kb_str) = line.split_whitespace().nth(1) {
                        if let Ok(kb) = kb_str.parse::<usize>() {
                            available_mb = kb / 1024;
                        }
                    }
                    break; // MemAvailable is usually after MemTotal
                }
            }

            if total_mb == 0 || available_mb == 0 {
                bail!("Failed to parse /proc/meminfo");
            }

            Ok((total_mb, available_mb))
        }

        #[cfg(not(target_os = "linux"))]
        {
            bail!("Memory detection only supported on Linux");
        }
    }
}

/// Resource usage statistics.
#[derive(Debug, Clone, Copy)]
pub struct ResourceStats {
    pub total_memory_mb: usize,
    pub available_memory_mb: usize,
    pub allocated_memory_mb: usize,
    pub cache_memory_mb: usize,
    pub peak_memory_mb: usize,
    pub peak_cache_mb: usize,
    pub max_cpu_slots: usize,
    pub allocated_cpu_slots: usize,
    pub active_loading_tasks: usize,
    pub active_building_tasks: usize,
    pub peak_loading_tasks: usize,
    pub peak_building_tasks: usize,
    pub total_allocations: usize,
}

impl ResourceStats {
    /// Format stats as a human-readable string.
    pub fn format(&self) -> String {
        format!(
            "Memory: {}/{} MB allocated (peak: {} MB), Cache: {} MB (peak: {} MB), CPU: {}/{} slots, {} total allocations",
            self.allocated_memory_mb,
            self.available_memory_mb,
            self.peak_memory_mb,
            self.cache_memory_mb,
            self.peak_cache_mb,
            self.allocated_cpu_slots,
            self.max_cpu_slots,
            self.total_allocations
        )
    }

    /// Format parallelization efficiency metrics.
    pub fn format_parallelization(&self) -> String {
        let total_active = self.active_loading_tasks + self.active_building_tasks;

        if total_active == 0 {
            return "Parallelization: idle (0 tasks)".to_string();
        }

        // Calculate efficiency: active tasks / available CPU cores
        let efficiency_pct = if self.max_cpu_slots > 0 {
            (total_active as f64 / self.max_cpu_slots as f64 * 100.0).min(100.0)
        } else {
            0.0
        };

        // Suggest optimal allocation (e.g., 2 loading, 6 building for 8 cores)
        let suggested_loading = (self.max_cpu_slots as f64 * 0.25).ceil() as usize;
        let suggested_building = self.max_cpu_slots.saturating_sub(suggested_loading);

        format!(
            "Parallelization: {} loading, {} building ({}/{} cores, {:.1}% efficiency) | Suggested: {}L/{}B",
            self.active_loading_tasks,
            self.active_building_tasks,
            total_active,
            self.max_cpu_slots,
            efficiency_pct,
            suggested_loading,
            suggested_building
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::Barrier;
    use tokio::sync::Notify;

    fn test_manager(
        total_memory_mb: usize,
        hardware_cpu_slots: usize,
        configured_cpu_slots: usize,
        pressure_rx: watch::Receiver<bool>,
    ) -> Arc<ResourceManager> {
        test_manager_with_mode(
            total_memory_mb,
            hardware_cpu_slots,
            configured_cpu_slots,
            pressure_rx,
            MemoryMode::System,
        )
    }

    fn test_manager_with_mode(
        total_memory_mb: usize,
        hardware_cpu_slots: usize,
        configured_cpu_slots: usize,
        pressure_rx: watch::Receiver<bool>,
        memory_mode: MemoryMode,
    ) -> Arc<ResourceManager> {
        let hardware_cpu_slots = hardware_cpu_slots.max(1);
        let configured_cpu_slots = configured_cpu_slots.clamp(1, hardware_cpu_slots);
        Arc::new(ResourceManager {
            state: Arc::new(Mutex::new(ResourceState {
                memory_ceiling_mb: total_memory_mb,
                total_memory_mb,
                available_memory_mb: total_memory_mb,
                memory_sample_generation: 0,
                allocated_memory_mb: 0,
                cache_memory_mb: 0,
                configured_cpu_slots,
                pressure_cpu_slots: None,
                allocated_cpu_slots: 0,
                active_loading_tasks: 0,
                active_building_tasks: 0,
                last_memory_check: Instant::now(),
                memory_check_interval: Duration::from_secs(3600),
                peak_memory_mb: 0,
                peak_cache_mb: 0,
                peak_loading_tasks: 0,
                peak_building_tasks: 0,
                total_allocations: 0,
                reserve_memory_mb: 0,
            })),
            cpu_semaphore: Arc::new(Semaphore::new(hardware_cpu_slots)),
            pressure_rx,
            hardware_cpu_slots,
            memory_mode,
            memory_sample_generation: AtomicU64::new(0),
            _pressure_monitor_task: None,
            _pressure_controller_task: StdMutex::new(None),
        })
    }

    async fn wait_for_effective_limit(manager: &ResourceManager, expected: usize) {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if manager.state.lock().await.effective_cpu_slots() == expected {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("effective CPU limit did not converge");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn psi_file_falls_back_to_read_only_monitoring() {
        let (file, passive_only) = select_psi_file(
            || Err(io::Error::from(io::ErrorKind::PermissionDenied)),
            || File::open("/dev/null"),
        )
        .expect("read-only PSI fallback");

        assert!(passive_only);
        assert!(file.metadata().is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn psi_file_preserves_trigger_capability_when_writable() {
        let (file, passive_only) = select_psi_file(
            || OpenOptions::new().read(true).write(true).open("/dev/null"),
            || panic!("read-only fallback must not run"),
        )
        .expect("read-write PSI path");

        assert!(!passive_only);
        assert!(file.metadata().is_ok());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn psi_file_reports_both_open_failures() {
        let error = select_psi_file(
            || Err(io::Error::from(io::ErrorKind::PermissionDenied)),
            || Err(io::Error::from(io::ErrorKind::NotFound)),
        )
        .expect_err("unavailable PSI must fail");

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.to_string().contains("read-write open failed"));
        assert!(error.to_string().contains("read-only open failed"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn trigger_readability_failure_switches_to_passive_monitor() {
        let (pressure_tx, pressure_rx) = watch::channel(false);
        let passive_calls = Arc::new(AtomicUsize::new(0));
        let passive_calls_for_fallback = Arc::clone(&passive_calls);

        run_trigger_psi_monitor(
            pressure_tx,
            || async {
                Err::<(), io::Error>(io::Error::new(
                    io::ErrorKind::Other,
                    "injected readiness failure",
                ))
            },
            move |pressure_tx| async move {
                passive_calls_for_fallback.fetch_add(1, Ordering::SeqCst);
                let _ = pressure_tx.send(true);
            },
        )
        .await;

        assert_eq!(passive_calls.load(Ordering::SeqCst), 1);
        assert!(*pressure_rx.borrow());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn async_fd_new_failure_uses_passive_monitor() {
        let file = File::open("/dev/null").expect("open registration input");
        let (pressure_tx, pressure_rx) = watch::channel(false);
        let registration_calls = Arc::new(AtomicUsize::new(0));
        let registration_calls_for_hook = Arc::clone(&registration_calls);
        let passive_calls = Arc::new(AtomicUsize::new(0));
        let passive_calls_for_hook = Arc::clone(&passive_calls);

        let task = spawn_registered_psi_monitor_with(
            file,
            pressure_tx,
            move |_| {
                registration_calls_for_hook.fetch_add(1, Ordering::SeqCst);
                Err::<AsyncFd<File>, io::Error>(io::Error::new(
                    io::ErrorKind::Other,
                    "injected AsyncFd registration failure",
                ))
            },
            |_, _| panic!("trigger monitor must not start after registration failure"),
            move |pressure_tx| {
                passive_calls_for_hook.fetch_add(1, Ordering::SeqCst);
                let _ = pressure_tx.send(true);
                BackgroundTask::new(tokio::spawn(async {}))
            },
        );

        assert_eq!(registration_calls.load(Ordering::SeqCst), 1);
        assert_eq!(passive_calls.load(Ordering::SeqCst), 1);
        assert!(*pressure_rx.borrow());
        drop(task);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn post_wait_memory_samples_apply_in_start_order() {
        let (_pressure_tx, pressure_rx) = watch::channel(false);
        let manager = test_manager(8_192, 2, 2, pressure_rx);
        let held_first = manager
            .cpu_semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("hold first CPU slot");
        let held_second = manager
            .cpu_semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("hold second CPU slot");

        let request = ResourceRequest::new(128, 1, TaskType::Building);
        let (older_waiting_tx, older_waiting_rx) = tokio::sync::oneshot::channel();
        let (older_read_started_tx, older_read_started_rx) =
            tokio::sync::oneshot::channel();
        let (release_older_read_tx, release_older_read_rx) = mpsc::channel();
        let manager_for_older_read = Arc::clone(&manager);
        let older_acquire = tokio::spawn(async move {
            let mut older_waiting_tx = Some(older_waiting_tx);
            let mut older_read_started_tx = Some(older_read_started_tx);
            manager_for_older_read
                .acquire_with_hooks(
                    request,
                    || false,
                    move || {
                        older_read_started_tx
                            .take()
                            .expect("older memory read runs once")
                            .send(())
                            .expect("signal older memory read");
                        release_older_read_rx
                            .recv()
                            .expect("release older memory read");
                        Ok((8_192, 8_000))
                    },
                    move || {
                        older_waiting_tx
                            .take()
                            .expect("older waiter queues once")
                            .send(())
                            .expect("signal older CPU wait");
                    },
                    || {},
                )
                .await
        });

        older_waiting_rx.await.expect("older waiter reached CPU wait");
        drop(held_first);
        older_read_started_rx
            .await
            .expect("older memory read started");

        let (newer_waiting_tx, newer_waiting_rx) = tokio::sync::oneshot::channel();
        let (newer_read_started_tx, newer_read_started_rx) =
            tokio::sync::oneshot::channel();
        let manager_for_newer_read = Arc::clone(&manager);
        let newer_acquire = tokio::spawn(async move {
            let mut newer_waiting_tx = Some(newer_waiting_tx);
            let mut newer_read_started_tx = Some(newer_read_started_tx);
            manager_for_newer_read
                .acquire_with_hooks(
                    request,
                    || false,
                    move || {
                        newer_read_started_tx
                            .take()
                            .expect("newer memory read runs once")
                            .send(())
                            .expect("signal newer memory read");
                        Ok((8_192, 64))
                    },
                    move || {
                        newer_waiting_tx
                            .take()
                            .expect("newer waiter queues once")
                            .send(())
                            .expect("signal newer CPU wait");
                    },
                    || {},
                )
                .await
        });

        newer_waiting_rx.await.expect("newer waiter reached CPU wait");
        drop(held_second);
        newer_read_started_rx
            .await
            .expect("newer memory read started");

        let newer_last_memory_check = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let state = manager.state.lock().await;
                if state.memory_sample_generation == 2 && state.available_memory_mb == 64 {
                    break state.last_memory_check;
                }
                drop(state);
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("newer low memory sample was not applied");

        release_older_read_tx
            .send(())
            .expect("release older memory read");

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if manager.cpu_semaphore.available_permits() == 2
                    || older_acquire.is_finished()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("older memory sample did not finish admission");

        let state = manager.state.lock().await;
        let available_memory_mb = state.available_memory_mb;
        let memory_sample_generation = state.memory_sample_generation;
        let last_memory_check = state.last_memory_check;
        let allocated_memory_mb = state.allocated_memory_mb;
        let total_allocations = state.total_allocations;
        drop(state);
        let available_permits = manager.cpu_semaphore.available_permits();

        older_acquire.abort();
        newer_acquire.abort();
        let _ = older_acquire.await;
        let _ = newer_acquire.await;

        assert_eq!(available_memory_mb, 64);
        assert_eq!(memory_sample_generation, 2);
        assert_eq!(last_memory_check, newer_last_memory_check);
        assert_eq!(allocated_memory_mb, 0);
        assert_eq!(total_allocations, 0);
        assert_eq!(available_permits, 2);
    }

    #[test]
    fn pressure_level_prioritizes_hard_conditions_and_clears() {
        assert_eq!(
            PressureLevel::from_conditions(false, false, false),
            PressureLevel::Clear
        );
        assert_eq!(
            PressureLevel::from_conditions(true, false, false),
            PressureLevel::Soft
        );
        assert_eq!(
            PressureLevel::from_conditions(true, true, false),
            PressureLevel::Critical
        );
        assert_eq!(
            PressureLevel::from_conditions(true, true, true),
            PressureLevel::RamFloor
        );
        assert_eq!(
            PressureLevel::from_conditions(false, true, true),
            PressureLevel::RamFloor
        );
        assert_eq!(
            PressureLevel::from_conditions(false, false, false),
            PressureLevel::Clear
        );
    }

    #[test]
    fn pressure_level_notifies_every_distinct_transition() {
        let (tx, mut rx) = watch::channel(false);
        let mut last_level = PressureLevel::Clear;

        assert!(publish_pressure_level(
            &tx,
            &mut last_level,
            PressureLevel::Soft
        ));
        assert!(rx.has_changed().expect("sender remains open"));
        assert!(*rx.borrow_and_update());
        assert!(!publish_pressure_level(
            &tx,
            &mut last_level,
            PressureLevel::Soft
        ));
        assert!(!rx.has_changed().expect("sender remains open"));

        for (level, expected_pressure) in [
            (PressureLevel::Critical, true),
            (PressureLevel::RamFloor, true),
            (PressureLevel::Critical, true),
            (PressureLevel::Soft, true),
            (PressureLevel::Clear, false),
        ] {
            assert!(publish_pressure_level(&tx, &mut last_level, level));
            assert!(rx.has_changed().expect("sender remains open"));
            assert_eq!(*rx.borrow_and_update(), expected_pressure);
        }

        assert!(!publish_pressure_level(
            &tx,
            &mut last_level,
            PressureLevel::Clear
        ));
        assert!(!rx.has_changed().expect("sender remains open"));
    }

    #[test]
    fn pressure_conditions_do_not_falsely_clear_overlapped_pressure() {
        let critical = PressureConditions {
            soft: true,
            critical: true,
            ram_floor: false,
        };
        let overlapped = PressureConditions {
            ram_floor: true,
            ..critical
        };
        assert_eq!(overlapped.level(), PressureLevel::RamFloor);
        assert!(!overlapped.critical_cleared_since(critical));
        assert!(!overlapped.ram_floor_cleared_since(critical));

        let floor_cleared = PressureConditions {
            ram_floor: false,
            ..overlapped
        };
        assert_eq!(floor_cleared.level(), PressureLevel::Critical);
        assert!(floor_cleared.ram_floor_cleared_since(overlapped));
        assert!(!floor_cleared.critical_cleared_since(overlapped));
    }

    #[test]
    fn pressure_controller_recovers_fully_without_new_channel_events() {
        let configured_limit = 16;
        let mut limit = pressure_controller_limit(
            configured_limit,
            configured_limit,
            PressureLevel::Critical,
            false,
            true,
        );
        assert_eq!(limit, 1);

        let mut transition = true;
        while limit < configured_limit {
            limit = pressure_controller_limit(
                limit,
                configured_limit,
                PressureLevel::Clear,
                true,
                transition,
            );
            transition = false;
        }
        assert_eq!(limit, configured_limit);
    }

    #[test]
    fn renewed_pressure_interrupts_recovery_and_missing_samples_hold() {
        let configured_limit = 16;
        let mut limit = 1;
        for _ in 0..3 {
            limit = pressure_controller_limit(
                limit,
                configured_limit,
                PressureLevel::Clear,
                true,
                false,
            );
        }
        assert_eq!(limit, 4);

        limit = pressure_controller_limit(
            limit,
            configured_limit,
            PressureLevel::Soft,
            false,
            true,
        );
        assert_eq!(limit, 2);
        assert_eq!(
            pressure_controller_limit(
                limit,
                configured_limit,
                PressureLevel::Soft,
                false,
                false,
            ),
            limit
        );
        assert_eq!(
            pressure_controller_limit(
                limit,
                configured_limit,
                PressureLevel::Clear,
                false,
                true,
            ),
            limit
        );
    }

    #[tokio::test]
    async fn pressure_controller_timer_recovers_to_configured_ceiling() {
        let (pressure_tx, pressure_rx) = watch::channel(false);
        let manager = test_manager(16_384, 8, 3, pressure_rx.clone());
        let sample_count = Arc::new(AtomicUsize::new(0));
        let sample_count_for_controller = Arc::clone(&sample_count);
        let controller = tokio::spawn(run_pressure_controller_with_sampler(
            Arc::downgrade(&manager),
            pressure_rx,
            Duration::from_millis(20),
            move |pressure| {
                sample_count_for_controller.fetch_add(1, Ordering::Relaxed);
                if pressure {
                    PressureControllerSample {
                        level: PressureLevel::Critical,
                        healthy_clear_sample: false,
                    }
                } else {
                    PressureControllerSample {
                        level: PressureLevel::Clear,
                        healthy_clear_sample: true,
                    }
                }
            },
        ));

        pressure_tx.send(true).expect("controller receiver");
        wait_for_effective_limit(&manager, 1).await;

        pressure_tx.send(false).expect("controller receiver");
        wait_for_effective_limit(&manager, 3).await;
        let samples_at_recovery = sample_count.load(Ordering::Relaxed);
        assert!(
            samples_at_recovery >= 3,
            "recovery must include a timer sample after the clear event"
        );

        tokio::time::sleep(Duration::from_millis(60)).await;
        let state = manager.state.lock().await;
        assert_eq!(state.configured_cpu_slots, 3);
        assert_eq!(state.effective_cpu_slots(), 3);
        assert!(state.pressure_cpu_slots.is_none());
        drop(state);
        assert_eq!(sample_count.load(Ordering::Relaxed), samples_at_recovery);

        drop(pressure_tx);
        controller.await.expect("pressure controller task");
    }

    #[test]
    fn psi_read_logging_limits_consecutive_and_flapping_failures() {
        let start = Instant::now();
        let mut state = PsiReadLogState::default();

        assert!(state.record_failure(start));
        assert!(!state.record_failure(start + Duration::from_secs(1)));
        assert!(state.record_success());

        assert!(!state.record_failure(start + Duration::from_secs(2)));
        assert!(!state.record_success());

        assert!(state.record_failure(start + PSI_READ_LOG_INTERVAL));
        assert!(state.record_success());
    }

    #[test]
    fn psi_diagnostic_rate_limit_has_one_concurrent_winner() {
        const THREADS: usize = 16;
        let last_diagnostic = Arc::new(AtomicU64::new(0));
        let barrier = Arc::new(Barrier::new(THREADS));
        let mut workers = Vec::with_capacity(THREADS);

        for _ in 0..THREADS {
            let last_diagnostic = Arc::clone(&last_diagnostic);
            let barrier = Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                claim_psi_diagnostic_slot(&last_diagnostic, 100, 30)
            }));
        }

        let winners = workers
            .into_iter()
            .map(|worker| worker.join().expect("diagnostic claimant"))
            .filter(|won| *won)
            .count();
        assert_eq!(winners, 1);
        assert!(!claim_psi_diagnostic_slot(&last_diagnostic, 129, 30));
        assert!(claim_psi_diagnostic_slot(&last_diagnostic, 130, 30));
        assert!(claim_psi_diagnostic_slot(&last_diagnostic, 130, 0));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn manager_drop_cancels_owned_pressure_tasks() {
        let manager = ResourceManager::new().expect("resource manager");
        let monitor = manager
            ._pressure_monitor_task
            .as_ref()
            .map(BackgroundTask::abort_handle);
        let controller = manager
            ._pressure_controller_task
            .lock()
            .expect("pressure controller task mutex")
            .as_ref()
            .map(BackgroundTask::abort_handle)
            .expect("controller task");

        drop(manager);

        for task in monitor.into_iter().chain(std::iter::once(controller)) {
            tokio::time::timeout(Duration::from_secs(1), async {
                while !task.is_finished() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("owned pressure task must terminate after manager drop");
        }
    }

    #[tokio::test]
    async fn acquire_revalidates_pressure_after_semaphore_wake() {
        let (pressure_tx, pressure_rx) = watch::channel(false);
        let manager = test_manager(8_192, 1, 1, pressure_rx);
        let held_permit = Arc::clone(&manager.cpu_semaphore)
            .acquire_owned()
            .await
            .expect("held CPU permit");
        let reached_cpu_wait = Arc::new(Notify::new());
        let reached_cpu_wait_for_task = Arc::clone(&reached_cpu_wait);
        let manager_for_task = Arc::clone(&manager);
        let mut waiter = tokio::spawn(async move {
            manager_for_task
                .acquire_with_hooks(
                    ResourceRequest::new(128, 1, TaskType::Building),
                    || false,
                    || Ok((8_192, 8_192)),
                    move || reached_cpu_wait_for_task.notify_one(),
                    || {},
                )
                .await
        });

        tokio::time::timeout(Duration::from_secs(1), reached_cpu_wait.notified())
            .await
            .expect("allocation reached semaphore wait");
        pressure_tx.send(true).expect("pressure receiver");
        drop(held_permit);

        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut waiter)
                .await
                .is_err(),
            "queued allocation must not pass after pressure begins"
        );
        assert_eq!(manager.state.lock().await.allocated_cpu_slots, 0);

        pressure_tx.send(false).expect("pressure receiver");
        let handle = tokio::time::timeout(Duration::from_secs(1), waiter)
            .await
            .expect("allocation did not resume")
            .expect("allocation task")
            .expect("resource acquisition");
        assert_eq!(manager.state.lock().await.allocated_cpu_slots, 1);
        handle.release_async().await;
        assert_eq!(manager.state.lock().await.allocated_cpu_slots, 0);
    }

    #[tokio::test]
    async fn concurrent_waiters_do_not_overshoot_reduced_limit() {
        const WAITERS: usize = 4;
        let (_pressure_tx, pressure_rx) = watch::channel(false);
        let manager = test_manager(8_192, WAITERS, WAITERS, pressure_rx);
        let held_permits = Arc::clone(&manager.cpu_semaphore)
            .acquire_many_owned(WAITERS as u32)
            .await
            .expect("held CPU permits");
        let reached_cpu_wait = Arc::new(AtomicUsize::new(0));
        let mut waiters = Vec::with_capacity(WAITERS);

        for _ in 0..WAITERS {
            let manager_for_task = Arc::clone(&manager);
            let reached_cpu_wait = Arc::clone(&reached_cpu_wait);
            waiters.push(tokio::spawn(async move {
                manager_for_task
                    .acquire_with_hooks(
                        ResourceRequest::new(128, 1, TaskType::Building),
                        || false,
                        || Ok((8_192, 8_192)),
                        move || {
                            reached_cpu_wait.fetch_add(1, Ordering::Relaxed);
                        },
                        || {},
                    )
                    .await
            }));
        }

        tokio::time::timeout(Duration::from_secs(1), async {
            while reached_cpu_wait.load(Ordering::Relaxed) < WAITERS {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("allocations did not reach semaphore wait");
        manager.state.lock().await.pressure_cpu_slots = Some(1);
        drop(held_permits);

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if manager.state.lock().await.allocated_cpu_slots == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("one allocation was not admitted");
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(manager.state.lock().await.allocated_cpu_slots, 1);
        assert_eq!(waiters.iter().filter(|waiter| waiter.is_finished()).count(), 1);

        let mut admitted = None;
        for waiter in waiters {
            if waiter.is_finished() {
                assert!(admitted.is_none());
                admitted = Some(
                    waiter
                        .await
                        .expect("allocation task")
                        .expect("resource acquisition"),
                );
            } else {
                waiter.abort();
                match waiter.await {
                    Err(error) => assert!(error.is_cancelled()),
                    Ok(_) => panic!("waiter completed after cancellation"),
                }
            }
        }
        admitted
            .expect("one admitted allocation")
            .release_async()
            .await;
        assert_eq!(manager.state.lock().await.allocated_cpu_slots, 0);
    }

    #[tokio::test]
    async fn acquire_rechecks_direct_pressure_after_semaphore_wake() {
        let (_pressure_tx, pressure_rx) = watch::channel(false);
        let manager = test_manager(8_192, 1, 1, pressure_rx);
        let held_permit = Arc::clone(&manager.cpu_semaphore)
            .acquire_owned()
            .await
            .expect("held CPU permit");
        let reached_cpu_wait = Arc::new(Notify::new());
        let reached_cpu_wait_for_task = Arc::clone(&reached_cpu_wait);
        let critical_checks = Arc::new(AtomicUsize::new(0));
        let critical_checks_for_task = Arc::clone(&critical_checks);
        let manager_for_task = Arc::clone(&manager);
        let mut waiter = tokio::spawn(async move {
            manager_for_task
                .acquire_with_hooks(
                    ResourceRequest::new(128, 1, TaskType::Building),
                    move || critical_checks_for_task.fetch_add(1, Ordering::SeqCst) > 0,
                    || Ok((8_192, 8_192)),
                    move || reached_cpu_wait_for_task.notify_one(),
                    || {},
                )
                .await
        });

        tokio::time::timeout(Duration::from_secs(1), reached_cpu_wait.notified())
            .await
            .expect("allocation reached semaphore wait");
        assert_eq!(critical_checks.load(Ordering::SeqCst), 1);
        drop(held_permit);

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if critical_checks.load(Ordering::SeqCst) >= 2
                    && manager.cpu_semaphore.available_permits() == 1
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("post-wake pressure check did not release the permit");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut waiter)
                .await
                .is_err(),
            "direct critical pressure must prevent admission after the wait"
        );
        assert_eq!(manager.state.lock().await.allocated_cpu_slots, 0);

        waiter.abort();
        match waiter.await {
            Err(error) => assert!(error.is_cancelled()),
            Ok(_) => panic!("allocation completed after cancellation"),
        }
    }

    #[tokio::test]
    async fn acquire_refreshes_memory_after_semaphore_wake() {
        let (_pressure_tx, pressure_rx) = watch::channel(false);
        let manager = test_manager(8_192, 1, 1, pressure_rx);
        let held_permit = Arc::clone(&manager.cpu_semaphore)
            .acquire_owned()
            .await
            .expect("held CPU permit");
        let reached_cpu_wait = Arc::new(Notify::new());
        let reached_cpu_wait_for_task = Arc::clone(&reached_cpu_wait);
        let memory_reads = Arc::new(AtomicUsize::new(0));
        let memory_reads_for_task = Arc::clone(&memory_reads);
        let manager_for_task = Arc::clone(&manager);
        let mut waiter = tokio::spawn(async move {
            manager_for_task
                .acquire_with_hooks(
                    ResourceRequest::new(128, 1, TaskType::Building),
                    || false,
                    move || {
                        memory_reads_for_task.fetch_add(1, Ordering::SeqCst);
                        Ok((8_192, 64))
                    },
                    move || reached_cpu_wait_for_task.notify_one(),
                    || {},
                )
                .await
        });

        tokio::time::timeout(Duration::from_secs(1), reached_cpu_wait.notified())
            .await
            .expect("allocation reached semaphore wait");
        assert_eq!(memory_reads.load(Ordering::SeqCst), 0);
        drop(held_permit);

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let state = manager.state.lock().await;
                let refreshed = state.available_memory_mb == 64;
                drop(state);
                if refreshed && manager.cpu_semaphore.available_permits() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("post-wake memory refresh did not reject stale admission");
        assert!(memory_reads.load(Ordering::SeqCst) >= 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut waiter)
                .await
                .is_err(),
            "fresh low-memory state must prevent admission after the wait"
        );
        assert_eq!(manager.state.lock().await.allocated_memory_mb, 0);

        waiter.abort();
        match waiter.await {
            Err(error) => assert!(error.is_cancelled()),
            Ok(_) => panic!("allocation completed after cancellation"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn acquire_rechecks_direct_pressure_at_accounting_boundary() {
        let (_pressure_tx, pressure_rx) = watch::channel(false);
        let manager = test_manager(8_192, 1, 1, pressure_rx);
        let critical = Arc::new(AtomicBool::new(false));
        let critical_for_task = Arc::clone(&critical);
        let critical_checks = Arc::new(AtomicUsize::new(0));
        let critical_checks_for_task = Arc::clone(&critical_checks);
        let boundary_phase = Arc::new(AtomicUsize::new(0));
        let boundary_phase_for_predicate = Arc::clone(&boundary_phase);
        let boundary_phase_for_hook = Arc::clone(&boundary_phase);
        let (boundary_entered_tx, boundary_entered_rx) = tokio::sync::oneshot::channel();
        let (release_boundary_tx, release_boundary_rx) = mpsc::channel();
        let (final_predicate_tx, final_predicate_rx) = tokio::sync::oneshot::channel();
        let manager_for_task = Arc::clone(&manager);
        let waiter = tokio::spawn(async move {
            let mut boundary_entered_tx = Some(boundary_entered_tx);
            let mut final_predicate_tx = Some(final_predicate_tx);
            manager_for_task
                .acquire_with_hooks(
                    ResourceRequest::new(128, 1, TaskType::Building),
                    move || {
                        let call = critical_checks_for_task.fetch_add(1, Ordering::SeqCst);
                        let is_critical = critical_for_task.load(Ordering::SeqCst);
                        if call == 2 {
                            assert!(is_critical, "critical pressure must be visible at admission");
                            assert_eq!(
                                boundary_phase_for_predicate.compare_exchange(
                                    1,
                                    2,
                                    Ordering::SeqCst,
                                    Ordering::SeqCst,
                                ),
                                Ok(1),
                                "final direct predicate must follow the accounting hook",
                            );
                            final_predicate_tx
                                .take()
                                .expect("final direct predicate runs once")
                                .send(())
                                .expect("signal final direct predicate");
                        }
                        is_critical
                    },
                    || Ok((8_192, 8_192)),
                    || {},
                    move || {
                        assert_eq!(
                            boundary_phase_for_hook.compare_exchange(
                                0,
                                1,
                                Ordering::SeqCst,
                                Ordering::SeqCst,
                            ),
                            Ok(0),
                            "accounting boundary must be entered once",
                        );
                        boundary_entered_tx
                            .take()
                            .expect("accounting boundary is entered once")
                            .send(())
                            .expect("signal accounting boundary");
                        release_boundary_rx
                            .recv()
                            .expect("release accounting boundary");
                    },
                )
                .await
        });

        tokio::time::timeout(Duration::from_secs(1), boundary_entered_rx)
            .await
            .expect("allocation did not reach accounting boundary")
            .expect("accounting boundary sender dropped");
        assert_eq!(boundary_phase.load(Ordering::SeqCst), 1);
        assert_eq!(critical_checks.load(Ordering::SeqCst), 2);
        assert_eq!(manager.cpu_semaphore.available_permits(), 0);

        critical.store(true, Ordering::SeqCst);
        release_boundary_tx
            .send(())
            .expect("release accounting boundary");
        tokio::time::timeout(Duration::from_secs(1), final_predicate_rx)
            .await
            .expect("final direct pressure check did not finish")
            .expect("final direct pressure sender dropped");
        let returned_permit = tokio::time::timeout(
            Duration::from_secs(1),
            manager.cpu_semaphore.clone().acquire_owned(),
        )
        .await
        .expect("allocation permit was not returned")
        .expect("resource semaphore closed");

        assert_eq!(boundary_phase.load(Ordering::SeqCst), 2);
        assert!(critical_checks.load(Ordering::SeqCst) >= 3);
        assert!(!waiter.is_finished());
        assert_eq!(manager.cpu_semaphore.available_permits(), 0);
        let state = manager.state.lock().await;
        assert_eq!(state.allocated_memory_mb, 0);
        assert_eq!(state.allocated_cpu_slots, 0);
        assert_eq!(state.total_allocations, 0);
        drop(state);
        drop(returned_permit);
        assert_eq!(manager.cpu_semaphore.available_permits(), 1);

        waiter.abort();
        match waiter.await {
            Err(error) => assert!(error.is_cancelled()),
            Ok(_) => panic!("allocation completed after critical pressure began"),
        }
    }

    #[tokio::test]
    async fn system_memory_samples_cannot_raise_the_creation_ceiling() {
        let (_pressure_tx, pressure_rx) = watch::channel(false);
        let manager = test_manager(1_024, 2, 2, pressure_rx);
        let memory_reads = AtomicUsize::new(0);
        let handle = manager
            .acquire_with_hooks(
                ResourceRequest::new(128, 1, TaskType::Building),
                || false,
                || {
                    memory_reads.fetch_add(1, Ordering::SeqCst);
                    Ok((65_536, 65_536))
                },
                || {},
                || {},
            )
            .await
            .expect("capped allocation");

        let state = manager.state.lock().await;
        assert_eq!(state.memory_ceiling_mb, 1_024);
        assert_eq!(state.total_memory_mb, 1_024);
        assert_eq!(state.available_memory_mb, 1_024);
        assert_eq!(state.allocated_memory_mb, 128);
        drop(state);
        assert_eq!(memory_reads.load(Ordering::SeqCst), 1);

        handle.release_async().await;
        assert_eq!(manager.state.lock().await.allocated_memory_mb, 0);
    }

    #[tokio::test]
    async fn explicit_memory_mode_ignores_host_reader_failures() {
        let (_pressure_tx, pressure_rx) = watch::channel(false);
        let manager =
            test_manager_with_mode(4_096, 2, 2, pressure_rx, MemoryMode::Explicit);
        {
            let mut state = manager.state.lock().await;
            state.last_memory_check = Instant::now()
                .checked_sub(Duration::from_secs(2))
                .expect("earlier memory timestamp");
            state.memory_check_interval = Duration::from_secs(1);
        }
        let memory_reads = AtomicUsize::new(0);
        let request = ResourceRequest::new(128, 1, TaskType::Building);

        let handle = manager
            .acquire_with_hooks(
                request,
                || false,
                || {
                    memory_reads.fetch_add(1, Ordering::SeqCst);
                    Err(anyhow::anyhow!("injected unavailable memory reader"))
                },
                || {},
                || {},
            )
            .await
            .expect("explicit async allocation");
        assert_eq!(memory_reads.load(Ordering::SeqCst), 0);
        handle.release_async().await;

        let handle = manager
            .try_acquire_with_hooks(
                request,
                || false,
                || {
                    memory_reads.fetch_add(1, Ordering::SeqCst);
                    Err(anyhow::anyhow!("injected unavailable memory reader"))
                },
            )
            .expect("explicit try allocation")
            .expect("explicit resources available");
        assert_eq!(memory_reads.load(Ordering::SeqCst), 0);
        handle.release_async().await;

        let state = manager.state.lock().await;
        assert_eq!(state.memory_ceiling_mb, 4_096);
        assert_eq!(state.total_memory_mb, 4_096);
        assert_eq!(state.available_memory_mb, 4_096);
        assert_eq!(state.allocated_memory_mb, 0);
        assert_eq!(state.allocated_cpu_slots, 0);
    }

    #[tokio::test]
    async fn async_acquire_tracks_and_releases_multiple_cpu_slots() {
        let (_pressure_tx, pressure_rx) = watch::channel(false);
        let manager =
            test_manager_with_mode(8_192, 4, 4, pressure_rx, MemoryMode::Explicit);
        let request = ResourceRequest::new(512, 3, TaskType::Building);

        let handle = manager
            .acquire_with_hooks(
                request,
                || false,
                || panic!("explicit memory mode must not sample host capacity"),
                || {},
                || {},
            )
            .await
            .expect("three-slot allocation");

        let state = manager.state.lock().await;
        assert_eq!(state.allocated_memory_mb, 512);
        assert_eq!(state.allocated_cpu_slots, 3);
        assert_eq!(state.active_building_tasks, 1);
        drop(state);
        assert_eq!(manager.cpu_semaphore.available_permits(), 1);

        handle.release_async().await;
        let state = manager.state.lock().await;
        assert_eq!(state.allocated_memory_mb, 0);
        assert_eq!(state.allocated_cpu_slots, 0);
        assert_eq!(state.active_building_tasks, 0);
        drop(state);
        assert_eq!(manager.cpu_semaphore.available_permits(), 4);
    }

    #[tokio::test]
    async fn literal_zero_requests_are_rejected_by_both_entry_points() {
        let (_pressure_tx, pressure_rx) = watch::channel(false);
        let manager = test_manager(8_192, 2, 2, pressure_rx);
        let normalized = ResourceRequest::new(0, 0, TaskType::Building);
        assert_eq!(normalized.memory_mb, 1);
        assert_eq!(normalized.cpu_slots, 1);
        let invalid_requests = [
            ResourceRequest {
                memory_mb: 0,
                cpu_slots: 1,
                task_type: TaskType::Building,
            },
            ResourceRequest {
                memory_mb: 1,
                cpu_slots: 0,
                task_type: TaskType::Building,
            },
        ];

        for request in invalid_requests {
            let acquire_result = tokio::time::timeout(
                Duration::from_millis(100),
                manager.acquire(request),
            )
            .await
            .expect("invalid request validation must not block");
            assert!(acquire_result.is_err());
            assert!(manager.try_acquire(request).is_err());
        }

        assert_eq!(manager.cpu_semaphore.available_permits(), 2);
        let state = manager.state.lock().await;
        assert_eq!(state.allocated_memory_mb, 0);
        assert_eq!(state.allocated_cpu_slots, 0);
        assert_eq!(state.total_allocations, 0);
    }

    #[tokio::test]
    async fn oversized_request_errors_before_queue_and_does_not_block_following_work() {
        let (_pressure_tx, pressure_rx) = watch::channel(false);
        let manager = test_manager(8_192, 2, 2, pressure_rx);
        let oversized = ResourceRequest::new(128, 3, TaskType::Building);

        let acquire_result = tokio::time::timeout(
            Duration::from_millis(100),
            manager.acquire(oversized),
        )
        .await
        .expect("oversized validation must not enter the semaphore queue");
        let acquire_error = match acquire_result {
            Err(error) => error,
            Ok(_) => panic!("oversized request must fail"),
        };
        assert!(acquire_error.to_string().contains("hardware capacity"));
        assert!(manager.try_acquire(oversized).is_err());

        let valid = ResourceRequest::new(128, 1, TaskType::Building);
        let handle = tokio::time::timeout(
            Duration::from_secs(1),
            manager.acquire_with_hooks(
                valid,
                || false,
                || Ok((8_192, 8_192)),
                || {},
                || {},
            ),
        )
        .await
        .expect("valid request was blocked behind oversized request")
        .expect("valid request");
        handle.release_async().await;
        assert_eq!(manager.cpu_semaphore.available_permits(), 2);
    }

    #[test]
    fn try_acquire_rechecks_direct_critical_pressure_after_permit() {
        let (_pressure_tx, pressure_rx) = watch::channel(false);
        let manager = test_manager(8_192, 2, 2, pressure_rx);
        let request = ResourceRequest::new(128, 1, TaskType::Building);
        let critical_checks = AtomicUsize::new(0);

        assert!(manager
            .try_acquire_with_critical_pressure(request, || {
                critical_checks.fetch_add(1, Ordering::SeqCst) > 0
            })
            .expect("try acquire")
            .is_none());
        assert_eq!(critical_checks.load(Ordering::SeqCst), 2);
        assert_eq!(manager.cpu_semaphore.available_permits(), 2);
        let state = manager.state.blocking_lock();
        assert_eq!(state.allocated_memory_mb, 0);
        assert_eq!(state.allocated_cpu_slots, 0);
        assert_eq!(state.total_allocations, 0);
    }

    #[test]
    fn try_acquire_fails_closed_when_due_memory_refresh_fails() {
        let (_pressure_tx, pressure_rx) = watch::channel(false);
        let manager = test_manager(8_192, 2, 2, pressure_rx);
        {
            let mut state = manager.state.blocking_lock();
            state.last_memory_check = Instant::now()
                .checked_sub(Duration::from_secs(2))
                .expect("earlier memory timestamp");
            state.memory_check_interval = Duration::from_secs(1);
        }
        let memory_reads = AtomicUsize::new(0);

        let result = manager
            .try_acquire_with_hooks(
                ResourceRequest::new(128, 1, TaskType::Building),
                || false,
                || {
                    memory_reads.fetch_add(1, Ordering::SeqCst);
                    Err(anyhow::anyhow!("injected memory read failure"))
                },
            )
            .expect("try acquire");

        assert!(result.is_none());
        assert_eq!(memory_reads.load(Ordering::SeqCst), 1);
        assert_eq!(manager.cpu_semaphore.available_permits(), 2);
        let state = manager.state.blocking_lock();
        assert_eq!(state.allocated_memory_mb, 0);
        assert_eq!(state.allocated_cpu_slots, 0);
        assert_eq!(state.total_allocations, 0);
    }

    #[tokio::test]
    async fn try_acquire_returns_none_on_state_lock_contention() {
        let (_pressure_tx, pressure_rx) = watch::channel(false);
        let manager = test_manager(8_192, 2, 2, pressure_rx);
        let state_guard = manager.state.lock().await;
        let memory_reads = AtomicUsize::new(0);

        let result = manager
            .try_acquire_with_hooks(
                ResourceRequest::new(128, 1, TaskType::Building),
                || false,
                || {
                    memory_reads.fetch_add(1, Ordering::SeqCst);
                    Ok((8_192, 8_192))
                },
            )
            .expect("try acquire");

        assert!(result.is_none());
        assert_eq!(memory_reads.load(Ordering::SeqCst), 0);
        assert_eq!(manager.cpu_semaphore.available_permits(), 2);
        drop(state_guard);
        let state = manager.state.lock().await;
        assert_eq!(state.allocated_memory_mb, 0);
        assert_eq!(state.allocated_cpu_slots, 0);
        assert_eq!(state.total_allocations, 0);
    }

    #[tokio::test]
    async fn try_acquire_is_safe_inside_async_runtime() {
        let (_pressure_tx, pressure_rx) = watch::channel(false);
        let manager = test_manager(8_192, 2, 2, pressure_rx);
        let handle = manager
            .try_acquire_with_hooks(
                ResourceRequest::new(128, 1, TaskType::Building),
                || false,
                || Ok((8_192, 8_192)),
            )
            .expect("try acquire")
            .expect("available resource");

        assert_eq!(manager.state.lock().await.allocated_cpu_slots, 1);
        handle.release_async().await;
        assert_eq!(manager.state.lock().await.allocated_cpu_slots, 0);
        assert_eq!(manager.cpu_semaphore.available_permits(), 2);
    }

    #[test]
    fn try_acquire_enforces_every_gate_and_exact_permits() {
        let (pressure_tx, pressure_rx) = watch::channel(false);
        let manager = test_manager(1_000, 4, 4, pressure_rx);
        let one_slot = ResourceRequest::new(100, 1, TaskType::Building);

        let oversized = ResourceRequest::new(100, 5, TaskType::Building);
        assert!(manager
            .try_acquire_with_critical_pressure(oversized, || false)
            .is_err());
        assert_eq!(manager.cpu_semaphore.available_permits(), 4);

        pressure_tx.send(true).expect("pressure receiver");
        assert!(manager
            .try_acquire_with_critical_pressure(one_slot, || false)
            .expect("try acquire")
            .is_none());
        assert_eq!(manager.cpu_semaphore.available_permits(), 4);

        pressure_tx.send(false).expect("pressure receiver");
        manager.state.blocking_lock().pressure_cpu_slots = Some(1);
        let two_slots = ResourceRequest::new(100, 2, TaskType::Loading);
        assert!(manager
            .try_acquire_with_critical_pressure(two_slots, || false)
            .expect("try acquire")
            .is_none());
        assert_eq!(manager.cpu_semaphore.available_permits(), 4);

        manager.state.blocking_lock().pressure_cpu_slots = None;
        let too_much_memory = ResourceRequest::new(1_001, 2, TaskType::Loading);
        assert!(manager
            .try_acquire_with_critical_pressure(too_much_memory, || false)
            .expect("try acquire")
            .is_none());
        assert_eq!(manager.cpu_semaphore.available_permits(), 4);

        let mut handle = manager
            .try_acquire_with_critical_pressure(two_slots, || false)
            .expect("try acquire")
            .expect("two-slot allocation");
        assert_eq!(manager.cpu_semaphore.available_permits(), 2);
        let state = manager.state.blocking_lock();
        assert_eq!(state.allocated_cpu_slots, 2);
        assert_eq!(state.active_loading_tasks, 1);
        assert_eq!(state.total_allocations, 1);
        drop(state);
        drop(handle.permit.take());
        assert_eq!(manager.cpu_semaphore.available_permits(), 4);
    }

    #[tokio::test]
    async fn configured_limit_updates_preserve_pressure_throttle() {
        let (_pressure_tx, pressure_rx) = watch::channel(false);
        let manager = test_manager(8_192, 8, 8, pressure_rx);
        manager.state.lock().await.pressure_cpu_slots = Some(2);

        manager.set_max_cpu_slots_sync(6);
        let state = manager.state.lock().await;
        assert_eq!(state.configured_cpu_slots, 6);
        assert_eq!(state.effective_cpu_slots(), 2);
        drop(state);

        manager.set_max_cpu_slots(1).await;
        let state = manager.state.lock().await;
        assert_eq!(state.configured_cpu_slots, 1);
        assert_eq!(state.effective_cpu_slots(), 1);
        drop(state);

        manager.set_max_cpu_slots(4).await;
        let state = manager.state.lock().await;
        assert_eq!(state.configured_cpu_slots, 4);
        assert_eq!(state.effective_cpu_slots(), 2);
    }

    // Phase 5.C.4 commit 21: ResourceClass classification +
    // default-request tests. Pro Q5: "Add ResourceClass now.
    // Do not tune per-mnemonic limits yet."

    #[test]
    fn resource_class_classifies_cpp_compile() {
        assert_eq!(
            ResourceClass::from_mnemonic("CppCompile"),
            ResourceClass::CppCompile
        );
        assert_eq!(
            ResourceClass::from_mnemonic("CCompile"),
            ResourceClass::CppCompile
        );
    }

    #[test]
    fn resource_class_classifies_kernel_actions() {
        assert_eq!(
            ResourceClass::from_mnemonic("KernelCompile"),
            ResourceClass::KernelCompile
        );
        assert_eq!(
            ResourceClass::from_mnemonic("KernelLink"),
            ResourceClass::KernelLink
        );
        assert_eq!(
            ResourceClass::from_mnemonic("KernelCodegen"),
            ResourceClass::KernelCodegen
        );
    }

    #[test]
    fn resource_class_classifies_protoc() {
        assert_eq!(
            ResourceClass::from_mnemonic("ProtocCompile"),
            ResourceClass::CodegenProtoc
        );
    }

    #[test]
    fn resource_class_classifies_tblgen() {
        assert_eq!(
            ResourceClass::from_mnemonic("MlirTblgen"),
            ResourceClass::CodegenTblgen
        );
        assert_eq!(
            ResourceClass::from_mnemonic("LlvmTblgen"),
            ResourceClass::CodegenTblgen
        );
    }

    #[test]
    fn resource_class_classifies_archive_and_link() {
        assert_eq!(
            ResourceClass::from_mnemonic("CcArchive"),
            ResourceClass::Archive
        );
        assert_eq!(
            ResourceClass::from_mnemonic("CcLink"),
            ResourceClass::Link
        );
        assert_eq!(
            ResourceClass::from_mnemonic("Link"),
            ResourceClass::Link
        );
        assert_eq!(
            ResourceClass::from_mnemonic("CppLink"),
            ResourceClass::Link
        );
    }

    #[test]
    fn resource_class_unknown_mnemonic_falls_back_to_other() {
        assert_eq!(
            ResourceClass::from_mnemonic("UnrecognizedMnemonic"),
            ResourceClass::Other
        );
        assert_eq!(ResourceClass::from_mnemonic(""), ResourceClass::Other);
    }

    #[test]
    fn resource_class_default_requests_match_existing_pool() {
        // Identity-mapping invariant: commit 21 introduces the
        // seam without changing limits. CppCompile → cpp_compile,
        // Link → cpp_link, everything else → minimal.
        assert_eq!(
            ResourceClass::CppCompile.default_request().memory_mb,
            ResourceRequest::cpp_compile().memory_mb
        );
        assert_eq!(
            ResourceClass::Link.default_request().memory_mb,
            ResourceRequest::cpp_link().memory_mb
        );
        assert_eq!(
            ResourceClass::KernelCompile.default_request().memory_mb,
            ResourceRequest::minimal().memory_mb
        );
        assert_eq!(
            ResourceClass::KernelLink.default_request().memory_mb,
            ResourceRequest::cpp_link().memory_mb
        );
        for class in [
            ResourceClass::KernelCodegen,
            ResourceClass::CodegenProtoc,
            ResourceClass::CodegenTblgen,
            ResourceClass::Archive,
            ResourceClass::Other,
        ] {
            assert_eq!(
                class.default_request().memory_mb,
                ResourceRequest::minimal().memory_mb,
                "{:?} should route to minimal request today",
                class
            );
        }
    }

    #[tokio::test]
    async fn admission_stats_measure_cpu_permit_waits() {
        let manager = ResourceManager::with_limits(1_024, 1);
        let request = ResourceRequest::new(128, 1, TaskType::Building);
        let first = manager.acquire(request).await.unwrap();
        let waiting_manager = Arc::clone(&manager);
        let waiter = tokio::spawn(async move { waiting_manager.acquire(request).await.unwrap() });

        tokio::time::sleep(Duration::from_millis(30)).await;
        first.release_async().await;
        let second = waiter.await.unwrap();
        let stats = second.admission_stats();

        assert!(stats.cpu_permit_wait >= Duration::from_millis(20));
        assert_eq!(stats.pressure_retries, 0);
        assert_eq!(stats.memory_retries, 0);
        assert_eq!(stats.cpu_limit_retries, 0);
        assert_eq!(stats.memory_refresh_retries, 0);
        second.release_async().await;
    }

    #[tokio::test]
    async fn test_resource_allocation() {
        let mgr = ResourceManager::with_limits(10_000, 4); // 10GB, 4 CPUs

        let req = ResourceRequest::new(2_000, 1, TaskType::Building); // 2GB
        let handle = mgr.acquire(req).await.unwrap();

        let stats = mgr.stats().await;
        assert_eq!(stats.allocated_memory_mb, 2_000);
        assert_eq!(stats.allocated_cpu_slots, 1);

        handle.release_async().await;

        let stats = mgr.stats().await;
        assert_eq!(stats.allocated_memory_mb, 0);
        assert_eq!(stats.allocated_cpu_slots, 0);
    }

    #[tokio::test]
    async fn test_resource_blocking() {
        let mgr = ResourceManager::with_limits(1_000, 2); // 1GB, 2 CPUs

        let req = ResourceRequest::new(600, 1, TaskType::Building);
        let _handle1 = mgr.acquire(req).await.unwrap();

        // Second allocation should succeed (600 + 400 = 1000)
        let req2 = ResourceRequest::new(400, 1, TaskType::Building);
        let _handle2 = mgr.acquire(req2).await.unwrap();

        let stats = mgr.stats().await;
        assert_eq!(stats.allocated_memory_mb, 1_000);

        // Third allocation should be blocked (we can't test blocking easily in sync test)
        // Just verify try_acquire fails
        let req3 = ResourceRequest::new(100, 1, TaskType::Building);
        let result = mgr.try_acquire(req3).unwrap();
        assert!(result.is_none());
    }
}
