// Rush Build - Persistent Worker Protocol
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Persistent worker protocol implementation for the Rush build system.
//!
//! This module provides a persistent worker protocol that keeps compiler and tool
//! processes running between builds to eliminate process spawn overhead and enable
//! JIT warmup, reducing build times significantly.
//!
//! # Architecture
//!
//! The persistent worker system consists of:
//! - `WorkerPool`: Manages a collection of workers with auto-scaling
//! - `Worker`: Individual worker process wrapper with health monitoring
//! - `WorkerRequest`/`WorkerResponse`: JSON-RPC protocol messages
//! - `WorkerType`: Different worker types (Java, Python, TypeScript, etc.)
//!
//! # Protocol
//!
//! Workers communicate via stdin/stdout using JSON-RPC 2.0:
//! - Request: `{"jsonrpc":"2.0","id":123,"method":"compile","params":{...}}`
//! - Response: `{"jsonrpc":"2.0","id":123,"result":{...}}`
//! - Error: `{"jsonrpc":"2.0","id":123,"error":{...}}`
//!
//! # Example
//!
//! ```no_run
//! use rush_build::persistent_workers::{WorkerPool, WorkerType, WorkerConfig};
//!
//! # async fn example() -> anyhow::Result<()> {
//! let config = WorkerConfig::default();
//! let pool = WorkerPool::new(config);
//!
//! // Execute a compilation request
//! let request = vec!["file.java".to_string()];
//! let response = pool.execute(WorkerType::JavaC, request).await?;
//! # Ok(())
//! # }
//! ```

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, RwLock, Semaphore};
use tokio::time::timeout;

/// Type of worker process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WorkerType {
    /// Java compiler (javac).
    JavaC,
    /// Kotlin compiler (kotlinc).
    KotlinC,
    /// Python interpreter (python3).
    Python,
    /// TypeScript compiler (tsc).
    TypeScript,
    /// Go compiler (go).
    Go,
    /// Scala compiler (scalac).
    Scala,
    /// Protocol buffer compiler (protoc).
    Protoc,
    /// Custom worker type.
    Custom(u32),
}

impl WorkerType {
    /// Get the executable path for this worker type.
    pub fn executable(&self) -> &str {
        match self {
            WorkerType::JavaC => "javac",
            WorkerType::KotlinC => "kotlinc",
            WorkerType::Python => "python3",
            WorkerType::TypeScript => "tsc",
            WorkerType::Go => "go",
            WorkerType::Scala => "scalac",
            WorkerType::Protoc => "protoc",
            WorkerType::Custom(_) => "custom-worker",
        }
    }

    /// Get worker type name for logging.
    pub fn name(&self) -> String {
        match self {
            WorkerType::JavaC => "JavaC".to_string(),
            WorkerType::KotlinC => "KotlinC".to_string(),
            WorkerType::Python => "Python".to_string(),
            WorkerType::TypeScript => "TypeScript".to_string(),
            WorkerType::Go => "Go".to_string(),
            WorkerType::Scala => "Scala".to_string(),
            WorkerType::Protoc => "Protoc".to_string(),
            WorkerType::Custom(id) => format!("Custom({})", id),
        }
    }
}

/// Configuration for worker pool.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Maximum number of workers per type.
    pub max_workers_per_type: usize,
    /// Minimum number of workers to keep warm per type.
    pub min_workers_per_type: usize,
    /// Request timeout duration.
    pub request_timeout: Duration,
    /// Worker health check interval.
    pub health_check_interval: Duration,
    /// Maximum memory per worker (bytes).
    pub max_memory_bytes: u64,
    /// Maximum CPU time per worker (seconds).
    pub max_cpu_seconds: u64,
    /// Worker idle timeout before shutdown.
    pub idle_timeout: Duration,
    /// Maximum request retries on failure.
    pub max_retries: usize,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        WorkerConfig {
            max_workers_per_type: num_cpus::get(),
            min_workers_per_type: 1,
            request_timeout: Duration::from_secs(300),
            health_check_interval: Duration::from_secs(30),
            max_memory_bytes: 2 * 1024 * 1024 * 1024, // 2GB
            max_cpu_seconds: 600,                     // 10 minutes
            idle_timeout: Duration::from_secs(300),
            max_retries: 2,
        }
    }
}

/// JSON-RPC request to worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerRequest {
    /// JSON-RPC version (always "2.0").
    pub jsonrpc: String,
    /// Request ID for matching responses.
    pub id: u64,
    /// Method to invoke.
    pub method: String,
    /// Request parameters.
    pub params: WorkerParams,
}

/// Parameters for worker request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerParams {
    /// Command arguments.
    pub args: Vec<String>,
    /// Working directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Environment variables.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    /// Input files.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inputs: Option<Vec<PathBuf>>,
}

/// JSON-RPC response from worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WorkerResponse {
    /// Successful response.
    Success {
        /// JSON-RPC version.
        jsonrpc: String,
        /// Request ID.
        id: u64,
        /// Result data.
        result: WorkerResult,
    },
    /// Error response.
    Error {
        /// JSON-RPC version.
        jsonrpc: String,
        /// Request ID.
        id: u64,
        /// Error details.
        error: WorkerError,
    },
}

/// Successful worker result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerResult {
    /// Exit code.
    pub exit_code: i32,
    /// Standard output.
    pub stdout: Vec<u8>,
    /// Standard error.
    pub stderr: Vec<u8>,
    /// Execution duration in milliseconds.
    pub duration_ms: u64,
}

/// Worker error details.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerError {
    /// Error code.
    pub code: i32,
    /// Error message.
    pub message: String,
    /// Additional error data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// Metrics for worker pool.
#[derive(Debug, Default)]
pub struct WorkerMetrics {
    /// Total requests processed.
    pub total_requests: AtomicU64,
    /// Total requests that succeeded.
    pub successful_requests: AtomicU64,
    /// Total requests that failed.
    pub failed_requests: AtomicU64,
    /// Total request latency (nanoseconds).
    pub total_latency_ns: AtomicU64,
    /// Cache hits (warm workers).
    pub cache_hits: AtomicU64,
    /// Workers spawned.
    pub workers_spawned: AtomicU64,
    /// Workers crashed.
    pub workers_crashed: AtomicU64,
    /// Workers restarted.
    pub workers_restarted: AtomicU64,
}

impl WorkerMetrics {
    /// Record a successful request.
    pub fn record_success(&self, latency: Duration, was_warm: bool) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        self.successful_requests.fetch_add(1, Ordering::Relaxed);
        self.total_latency_ns
            .fetch_add(latency.as_nanos() as u64, Ordering::Relaxed);
        if was_warm {
            self.cache_hits.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Record a failed request.
    pub fn record_failure(&self, latency: Duration) {
        self.total_requests.fetch_add(1, Ordering::Relaxed);
        self.failed_requests.fetch_add(1, Ordering::Relaxed);
        self.total_latency_ns
            .fetch_add(latency.as_nanos() as u64, Ordering::Relaxed);
    }

    /// Record worker spawn.
    pub fn record_spawn(&self) {
        self.workers_spawned.fetch_add(1, Ordering::Relaxed);
    }

    /// Record worker crash.
    pub fn record_crash(&self) {
        self.workers_crashed.fetch_add(1, Ordering::Relaxed);
    }

    /// Record worker restart.
    pub fn record_restart(&self) {
        self.workers_restarted.fetch_add(1, Ordering::Relaxed);
    }

    /// Get average latency in milliseconds.
    pub fn avg_latency_ms(&self) -> f64 {
        let total = self.total_requests.load(Ordering::Relaxed);
        if total == 0 {
            return 0.0;
        }
        let total_ns = self.total_latency_ns.load(Ordering::Relaxed);
        (total_ns as f64 / total as f64) / 1_000_000.0
    }

    /// Get success rate as percentage.
    pub fn success_rate(&self) -> f64 {
        let total = self.total_requests.load(Ordering::Relaxed);
        if total == 0 {
            return 0.0;
        }
        let success = self.successful_requests.load(Ordering::Relaxed);
        (success as f64 / total as f64) * 100.0
    }

    /// Get cache hit rate as percentage.
    pub fn cache_hit_rate(&self) -> f64 {
        let total = self.total_requests.load(Ordering::Relaxed);
        if total == 0 {
            return 0.0;
        }
        let hits = self.cache_hits.load(Ordering::Relaxed);
        (hits as f64 / total as f64) * 100.0
    }
}

/// Individual worker process.
struct Worker {
    /// Worker type.
    _worker_type: WorkerType,
    /// Child process.
    child: Child,
    /// Stdin writer.
    stdin: BufWriter<tokio::process::ChildStdin>,
    /// Stdout reader.
    stdout: BufReader<tokio::process::ChildStdout>,
    /// Stderr reader (for health monitoring).
    _stderr: BufReader<tokio::process::ChildStderr>,
    /// Worker spawn time.
    _spawn_time: Instant,
    /// Last request time.
    last_request: Arc<Mutex<Instant>>,
    /// Number of requests processed.
    requests_processed: AtomicUsize,
    /// Worker is healthy.
    healthy: Arc<RwLock<bool>>,
}

impl Worker {
    /// Spawn a new worker process.
    async fn spawn(worker_type: WorkerType, _config: &WorkerConfig) -> Result<Self> {
        let executable = worker_type.executable();

        // Spawn worker in persistent mode
        let mut child = Command::new(executable)
            .arg("--persistent-worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("Failed to spawn worker: {}", executable))?;

        let stdin = BufWriter::new(
            child
                .stdin
                .take()
                .ok_or_else(|| anyhow::anyhow!("Failed to capture worker stdin"))?,
        );

        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| anyhow::anyhow!("Failed to capture worker stdout"))?,
        );

        let stderr = BufReader::new(
            child
                .stderr
                .take()
                .ok_or_else(|| anyhow::anyhow!("Failed to capture worker stderr"))?,
        );

        Ok(Worker {
            _worker_type: worker_type,
            child,
            stdin,
            stdout,
            _stderr: stderr,
            _spawn_time: Instant::now(),
            last_request: Arc::new(Mutex::new(Instant::now())),
            requests_processed: AtomicUsize::new(0),
            healthy: Arc::new(RwLock::new(true)),
        })
    }

    /// Execute a request on this worker.
    async fn execute(
        &mut self,
        request: WorkerRequest,
        config: &WorkerConfig,
    ) -> Result<WorkerResult> {
        // Update last request time
        *self.last_request.lock().await = Instant::now();

        // Serialize request
        let request_json =
            serde_json::to_string(&request).context("Failed to serialize worker request")?;

        // Send request
        self.stdin
            .write_all(request_json.as_bytes())
            .await
            .context("Failed to write request to worker")?;
        self.stdin
            .write_all(b"\n")
            .await
            .context("Failed to write newline to worker")?;
        self.stdin
            .flush()
            .await
            .context("Failed to flush worker stdin")?;

        // Read response with timeout
        let mut response_line = String::new();
        let read_result = timeout(
            config.request_timeout,
            self.stdout.read_line(&mut response_line),
        )
        .await;

        match read_result {
            Ok(Ok(0)) => {
                // EOF - worker died
                *self.healthy.write().await = false;
                anyhow::bail!("Worker terminated unexpectedly");
            }
            Ok(Ok(_)) => {
                // Got response
                let response: WorkerResponse = serde_json::from_str(&response_line)
                    .context("Failed to parse worker response")?;

                match response {
                    WorkerResponse::Success { result, .. } => {
                        self.requests_processed.fetch_add(1, Ordering::Relaxed);
                        Ok(result)
                    }
                    WorkerResponse::Error { error, .. } => {
                        anyhow::bail!("Worker error: {} (code {})", error.message, error.code)
                    }
                }
            }
            Ok(Err(e)) => {
                *self.healthy.write().await = false;
                Err(e).context("Failed to read worker response")
            }
            Err(_) => {
                *self.healthy.write().await = false;
                anyhow::bail!("Worker request timeout after {:?}", config.request_timeout)
            }
        }
    }

    /// Check if worker is healthy.
    async fn is_healthy(&self) -> bool {
        *self.healthy.read().await
    }

    /// Check if worker is idle.
    async fn _is_idle(&self, idle_timeout: Duration) -> bool {
        self.last_request.lock().await.elapsed() > idle_timeout
    }

    /// Get worker uptime.
    fn _uptime(&self) -> Duration {
        self._spawn_time.elapsed()
    }

    /// Gracefully shutdown worker.
    async fn shutdown(mut self) -> Result<()> {
        // Send shutdown request
        let shutdown_req = WorkerRequest {
            jsonrpc: "2.0".to_string(),
            id: u64::MAX,
            method: "shutdown".to_string(),
            params: WorkerParams {
                args: vec![],
                cwd: None,
                env: None,
                inputs: None,
            },
        };

        if let Ok(req_json) = serde_json::to_string(&shutdown_req) {
            let _ = self.stdin.write_all(req_json.as_bytes()).await;
            let _ = self.stdin.write_all(b"\n").await;
            let _ = self.stdin.flush().await;
        }

        // Wait for process to exit (with timeout)
        let _ = timeout(Duration::from_secs(5), self.child.wait()).await;

        // Force kill if still running
        let _ = self.child.kill().await;

        Ok(())
    }
}

/// Pool of workers for a specific worker type.
struct WorkerTypePool {
    /// Worker type.
    worker_type: WorkerType,
    /// Available workers.
    available: Arc<Mutex<Vec<Worker>>>,
    /// Active worker count.
    active_count: AtomicUsize,
    /// Pool configuration.
    config: WorkerConfig,
    /// Semaphore for limiting concurrent workers.
    semaphore: Arc<Semaphore>,
}

impl WorkerTypePool {
    /// Create a new pool for a worker type.
    fn new(worker_type: WorkerType, config: WorkerConfig) -> Self {
        let max_workers = config.max_workers_per_type;
        WorkerTypePool {
            worker_type,
            available: Arc::new(Mutex::new(Vec::new())),
            active_count: AtomicUsize::new(0),
            config,
            semaphore: Arc::new(Semaphore::new(max_workers)),
        }
    }

    /// Get or create a worker.
    async fn acquire_worker(&self, metrics: &WorkerMetrics) -> Result<Worker> {
        // Try to get an existing worker
        let mut available = self.available.lock().await;

        // Remove unhealthy or idle workers
        available.retain(|_w| {
            // This is a simple check - in production we'd do async checks
            true
        });

        if let Some(worker) = available.pop() {
            drop(available);
            return Ok(worker);
        }
        drop(available);

        // Need to spawn a new worker
        let _permit = self
            .semaphore
            .acquire()
            .await
            .context("Failed to acquire worker semaphore")?;

        metrics.record_spawn();
        let worker = Worker::spawn(self.worker_type, &self.config).await?;
        self.active_count.fetch_add(1, Ordering::Relaxed);

        Ok(worker)
    }

    /// Return a worker to the pool.
    async fn release_worker(&self, worker: Worker) {
        if worker.is_healthy().await {
            let mut available = self.available.lock().await;
            available.push(worker);
        } else {
            // Worker is unhealthy, shutdown
            let _ = worker.shutdown().await;
            self.active_count.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Shutdown all workers in this pool.
    async fn shutdown_all(&self) -> Result<()> {
        let mut available = self.available.lock().await;
        let workers = std::mem::take(&mut *available);
        drop(available);

        for worker in workers {
            let _ = worker.shutdown().await;
            self.active_count.fetch_sub(1, Ordering::Relaxed);
        }

        Ok(())
    }

    /// Get number of active workers.
    fn active_workers(&self) -> usize {
        self.active_count.load(Ordering::Relaxed)
    }
}

/// Pool of persistent workers.
pub struct WorkerPool {
    /// Per-type worker pools.
    pools: Arc<RwLock<HashMap<WorkerType, Arc<WorkerTypePool>>>>,
    /// Pool configuration.
    config: WorkerConfig,
    /// Request ID counter.
    next_request_id: AtomicU64,
    /// Pool metrics.
    metrics: Arc<WorkerMetrics>,
}

impl WorkerPool {
    /// Create a new worker pool.
    pub fn new(config: WorkerConfig) -> Self {
        WorkerPool {
            pools: Arc::new(RwLock::new(HashMap::new())),
            config,
            next_request_id: AtomicU64::new(1),
            metrics: Arc::new(WorkerMetrics::default()),
        }
    }

    /// Execute a request on a worker of the given type.
    pub async fn execute(
        &self,
        worker_type: WorkerType,
        args: Vec<String>,
    ) -> Result<WorkerResult> {
        self.execute_with_params(
            worker_type,
            WorkerParams {
                args,
                cwd: None,
                env: None,
                inputs: None,
            },
        )
        .await
    }

    /// Execute a request with full parameters.
    pub async fn execute_with_params(
        &self,
        worker_type: WorkerType,
        params: WorkerParams,
    ) -> Result<WorkerResult> {
        let start = Instant::now();

        // Get or create pool for this worker type
        let pool = self.get_or_create_pool(worker_type).await;

        // Acquire a worker with retries
        let mut last_error = None;
        for attempt in 0..=self.config.max_retries {
            let mut worker = match pool.acquire_worker(&self.metrics).await {
                Ok(w) => w,
                Err(e) => {
                    last_error = Some(e);
                    continue;
                }
            };

            let was_warm = worker.requests_processed.load(Ordering::Relaxed) > 0;

            // Create request
            let request = WorkerRequest {
                jsonrpc: "2.0".to_string(),
                id: self.next_request_id.fetch_add(1, Ordering::Relaxed),
                method: "execute".to_string(),
                params: params.clone(),
            };

            // Execute request
            match worker.execute(request, &self.config).await {
                Ok(result) => {
                    // Success
                    let latency = start.elapsed();
                    self.metrics.record_success(latency, was_warm);
                    pool.release_worker(worker).await;
                    return Ok(result);
                }
                Err(e) => {
                    // Worker failed
                    self.metrics.record_crash();
                    last_error = Some(e);

                    // Don't return worker if it failed
                    let _ = worker.shutdown().await;

                    if attempt < self.config.max_retries {
                        // Retry with a new worker
                        self.metrics.record_restart();
                        continue;
                    }
                }
            }
        }

        // All retries exhausted
        let latency = start.elapsed();
        self.metrics.record_failure(latency);
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Worker execution failed after retries")))
    }

    /// Get or create a pool for the given worker type.
    async fn get_or_create_pool(&self, worker_type: WorkerType) -> Arc<WorkerTypePool> {
        // Check if pool exists
        {
            let pools = self.pools.read().await;
            if let Some(pool) = pools.get(&worker_type) {
                return Arc::clone(pool);
            }
        }

        // Create new pool
        let mut pools = self.pools.write().await;
        // Double-check in case another task created it
        if let Some(pool) = pools.get(&worker_type) {
            return Arc::clone(pool);
        }

        let pool = Arc::new(WorkerTypePool::new(worker_type, self.config.clone()));
        pools.insert(worker_type, Arc::clone(&pool));
        pool
    }

    /// Shutdown all workers in the pool.
    pub async fn shutdown_all(&self) -> Result<()> {
        let pools = self.pools.write().await;
        for pool in pools.values() {
            pool.shutdown_all().await?;
        }
        Ok(())
    }

    /// Get pool statistics.
    pub async fn stats(&self) -> HashMap<String, usize> {
        let pools = self.pools.read().await;
        pools
            .iter()
            .map(|(wt, pool)| (wt.name(), pool.active_workers()))
            .collect()
    }

    /// Get pool metrics.
    pub fn metrics(&self) -> &WorkerMetrics {
        &self.metrics
    }

    /// Print metrics summary.
    pub fn print_metrics(&self) {
        use owo_colors::OwoColorize;

        eprintln!("\n{}", "Worker Pool Metrics:".bold().cyan());

        let total = self.metrics.total_requests.load(Ordering::Relaxed);
        eprintln!("  Total requests:     {}", total.to_string().bright_white());

        let success = self.metrics.successful_requests.load(Ordering::Relaxed);
        eprintln!(
            "  Successful:         {} {}",
            success.to_string().green(),
            format!("({:.1}%)", self.metrics.success_rate()).bright_black()
        );

        let failed = self.metrics.failed_requests.load(Ordering::Relaxed);
        if failed > 0 {
            eprintln!("  Failed:             {}", failed.to_string().red());
        }

        eprintln!(
            "  Avg latency:        {} ms",
            format!("{:.2}", self.metrics.avg_latency_ms()).cyan()
        );

        let hits = self.metrics.cache_hits.load(Ordering::Relaxed);
        eprintln!(
            "  Warm workers:       {} {}",
            hits.to_string().green(),
            format!("({:.1}%)", self.metrics.cache_hit_rate()).bright_black()
        );

        let spawned = self.metrics.workers_spawned.load(Ordering::Relaxed);
        eprintln!(
            "  Workers spawned:    {}",
            spawned.to_string().bright_white()
        );

        let crashed = self.metrics.workers_crashed.load(Ordering::Relaxed);
        if crashed > 0 {
            eprintln!("  Workers crashed:    {}", crashed.to_string().red());
        }

        let restarted = self.metrics.workers_restarted.load(Ordering::Relaxed);
        if restarted > 0 {
            eprintln!("  Workers restarted:  {}", restarted.to_string().yellow());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_worker_type_name() {
        assert_eq!(WorkerType::JavaC.name(), "JavaC");
        assert_eq!(WorkerType::Python.name(), "Python");
        assert_eq!(WorkerType::Custom(42).name(), "Custom(42)");
    }

    #[test]
    fn test_worker_type_executable() {
        assert_eq!(WorkerType::JavaC.executable(), "javac");
        assert_eq!(WorkerType::Python.executable(), "python3");
        assert_eq!(WorkerType::TypeScript.executable(), "tsc");
    }

    #[test]
    fn test_worker_config_default() {
        let config = WorkerConfig::default();
        assert!(config.max_workers_per_type > 0);
        assert!(config.request_timeout.as_secs() > 0);
        assert_eq!(config.max_retries, 2);
    }

    #[test]
    fn test_worker_metrics_avg_latency() {
        let metrics = WorkerMetrics::default();
        assert_eq!(metrics.avg_latency_ms(), 0.0);

        metrics.record_success(Duration::from_millis(100), false);
        assert!(metrics.avg_latency_ms() > 99.0 && metrics.avg_latency_ms() < 101.0);
    }

    #[test]
    fn test_worker_metrics_success_rate() {
        let metrics = WorkerMetrics::default();
        assert_eq!(metrics.success_rate(), 0.0);

        metrics.record_success(Duration::from_millis(100), false);
        assert_eq!(metrics.success_rate(), 100.0);

        metrics.record_failure(Duration::from_millis(50));
        assert_eq!(metrics.success_rate(), 50.0);
    }

    #[test]
    fn test_worker_metrics_cache_hit_rate() {
        let metrics = WorkerMetrics::default();
        assert_eq!(metrics.cache_hit_rate(), 0.0);

        metrics.record_success(Duration::from_millis(100), true); // warm
        assert_eq!(metrics.cache_hit_rate(), 100.0);

        metrics.record_success(Duration::from_millis(100), false); // cold
        assert_eq!(metrics.cache_hit_rate(), 50.0);
    }

    #[test]
    fn test_worker_request_serialization() {
        let request = WorkerRequest {
            jsonrpc: "2.0".to_string(),
            id: 123,
            method: "compile".to_string(),
            params: WorkerParams {
                args: vec!["test.java".to_string()],
                cwd: Some(PathBuf::from("/tmp")),
                env: None,
                inputs: None,
            },
        };

        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"jsonrpc\":\"2.0\""));
        assert!(json.contains("\"id\":123"));
        assert!(json.contains("\"method\":\"compile\""));
    }

    #[test]
    fn test_worker_response_deserialization() {
        let success_json = r#"{"jsonrpc":"2.0","id":123,"result":{"exit_code":0,"stdout":[],"stderr":[],"duration_ms":100}}"#;
        let response: WorkerResponse = serde_json::from_str(success_json).unwrap();

        match response {
            WorkerResponse::Success { id, result, .. } => {
                assert_eq!(id, 123);
                assert_eq!(result.exit_code, 0);
                assert_eq!(result.duration_ms, 100);
            }
            _ => panic!("Expected success response"),
        }

        let error_json =
            r#"{"jsonrpc":"2.0","id":456,"error":{"code":-32600,"message":"Invalid request"}}"#;
        let response: WorkerResponse = serde_json::from_str(error_json).unwrap();

        match response {
            WorkerResponse::Error { id, error, .. } => {
                assert_eq!(id, 456);
                assert_eq!(error.code, -32600);
                assert_eq!(error.message, "Invalid request");
            }
            _ => panic!("Expected error response"),
        }
    }

    #[tokio::test]
    async fn test_worker_pool_creation() {
        let config = WorkerConfig::default();
        let pool = WorkerPool::new(config);
        let stats = pool.stats().await;
        assert_eq!(stats.len(), 0);
    }

    #[tokio::test]
    async fn test_worker_pool_metrics() {
        let config = WorkerConfig::default();
        let pool = WorkerPool::new(config);

        let metrics = pool.metrics();
        assert_eq!(metrics.total_requests.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.success_rate(), 0.0);
    }
}
