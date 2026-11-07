// Rush Build - Remote Execution API
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! gRPC-based remote execution for distributed builds.
//!
//! Implements Bazel Remote Execution API v2 protocol for distributed action
//! execution. Actions are scheduled on remote workers, with inputs/outputs
//! managed through content-addressable storage (CAS).

use anyhow::{anyhow, Context, Result};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, Semaphore};

/// Content digest for content-addressable storage.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ActionDigest {
    /// BLAKE3 hash of the content.
    pub hash: String,

    /// Size in bytes.
    pub size_bytes: u64,
}

impl ActionDigest {
    /// Compute digest from bytes.
    pub fn from_bytes(data: &[u8]) -> Self {
        let hash = blake3::hash(data);
        ActionDigest {
            hash: hash.to_hex().to_string(),
            size_bytes: data.len() as u64,
        }
    }

    /// Compute digest from file.
    pub async fn from_file(path: &Path) -> Result<Self> {
        let data = tokio::fs::read(path).await?;
        Ok(Self::from_bytes(&data))
    }
}

impl std::fmt::Display for ActionDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.hash, self.size_bytes)
    }
}

/// Platform requirements for execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Platform {
    /// Operating system (linux, darwin, windows).
    pub os: String,

    /// CPU architecture (x86_64, aarch64).
    pub arch: String,

    /// Minimum memory in MB.
    pub min_memory_mb: u64,

    /// Minimum CPU cores.
    pub min_cores: u32,

    /// Custom properties (e.g., GPU, specific tools).
    pub properties: HashMap<String, String>,
}

impl Default for Platform {
    fn default() -> Self {
        Platform {
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            min_memory_mb: 2048,
            min_cores: 1,
            properties: HashMap::new(),
        }
    }
}

impl Platform {
    /// Check if a worker satisfies these platform requirements.
    pub fn matches(&self, worker_platform: &Platform) -> bool {
        self.os == worker_platform.os
            && self.arch == worker_platform.arch
            && self.min_memory_mb <= worker_platform.min_memory_mb
            && self.min_cores <= worker_platform.min_cores
    }
}

/// Input file for remote execution.
#[derive(Debug, Clone)]
pub struct InputFile {
    /// Relative path in execution directory.
    pub path: PathBuf,

    /// Content digest.
    pub digest: ActionDigest,

    /// Whether file is executable.
    pub is_executable: bool,
}

/// Output file from remote execution.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OutputFile {
    /// Relative path in execution directory.
    pub path: PathBuf,

    /// Content digest.
    pub digest: ActionDigest,
}

/// Remote action to execute.
#[derive(Debug, Clone)]
pub struct RemoteAction {
    /// Command to execute.
    pub command: Vec<String>,

    /// Environment variables.
    pub environment: HashMap<String, String>,

    /// Working directory (relative to execution root).
    pub working_directory: PathBuf,

    /// Input files.
    pub inputs: Vec<InputFile>,

    /// Expected output file paths.
    pub output_paths: Vec<PathBuf>,

    /// Platform requirements.
    pub platform: Platform,

    /// Execution timeout.
    pub timeout: Duration,

    /// Whether to skip cache lookup.
    pub do_not_cache: bool,
}

impl RemoteAction {
    /// Compute action digest for caching.
    pub fn compute_digest(&self) -> ActionDigest {
        let mut hasher = blake3::Hasher::new();

        // Hash command
        for arg in &self.command {
            hasher.update(arg.as_bytes());
            hasher.update(&[0]);
        }

        // Hash environment (sorted for determinism)
        let mut env_vec: Vec<_> = self.environment.iter().collect();
        env_vec.sort_by_key(|(k, _)| *k);
        for (k, v) in env_vec {
            hasher.update(k.as_bytes());
            hasher.update(b"=");
            hasher.update(v.as_bytes());
            hasher.update(&[0]);
        }

        // Hash inputs
        for input in &self.inputs {
            hasher.update(input.path.to_string_lossy().as_bytes());
            hasher.update(b":");
            hasher.update(input.digest.hash.as_bytes());
            hasher.update(&[0]);
        }

        // Hash platform
        hasher.update(self.platform.os.as_bytes());
        hasher.update(self.platform.arch.as_bytes());

        let hash = hasher.finalize();
        ActionDigest {
            hash: hash.to_hex().to_string(),
            size_bytes: 0, // Action digest doesn't track size
        }
    }
}

/// Execution status streaming.
#[derive(Debug, Clone)]
pub enum ExecutionStatus {
    /// Action queued for execution.
    Queued {
        /// Position in queue.
        position: usize,
    },

    /// Worker assigned, uploading inputs.
    UploadingInputs {
        /// Bytes uploaded so far.
        bytes_uploaded: u64,
        /// Total bytes to upload.
        total_bytes: u64,
    },

    /// Executing on worker.
    Executing {
        /// Worker ID.
        worker_id: String,
    },

    /// Execution complete, downloading outputs.
    DownloadingOutputs {
        /// Bytes downloaded so far.
        bytes_downloaded: u64,
        /// Total bytes to download.
        total_bytes: u64,
    },

    /// Completed successfully.
    Completed,

    /// Failed with error.
    Failed {
        /// Error message.
        error: String,
    },
}

/// Remote execution response.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExecutionResponse {
    /// Exit code.
    pub exit_code: i32,

    /// Standard output.
    pub stdout: Vec<u8>,

    /// Standard error.
    pub stderr: Vec<u8>,

    /// Output files produced.
    pub outputs: Vec<OutputFile>,

    /// Whether result was cached.
    pub cached: bool,

    /// Execution duration.
    pub execution_duration: Duration,

    /// Worker ID that executed the action.
    pub worker_id: Option<String>,
}

/// Remote worker connection.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RemoteWorker {
    /// Worker ID.
    pub id: String,

    /// gRPC endpoint.
    pub endpoint: String,

    /// Worker platform capabilities.
    pub platform: Platform,

    /// Current load (0.0 - 1.0).
    pub load: f64,

    /// Last heartbeat timestamp.
    pub last_heartbeat: SystemTime,

    /// Whether worker is healthy.
    pub healthy: bool,
}

impl RemoteWorker {
    /// Check if this worker can execute the action.
    pub fn can_execute(&self, action: &RemoteAction) -> bool {
        self.healthy && action.platform.matches(&self.platform) && self.load < 0.9
    }
}

/// Content-addressable storage client.
struct CasClient {
    endpoint: String,
    http_client: reqwest::Client,
}

impl CasClient {
    fn new(endpoint: String) -> Self {
        CasClient {
            endpoint,
            http_client: reqwest::Client::new(),
        }
    }

    /// Upload blob to CAS.
    async fn upload_blob(&self, digest: &ActionDigest, data: &[u8]) -> Result<()> {
        let url = format!("{}/cas/{}", self.endpoint, digest.hash);

        self.http_client
            .put(&url)
            .body(data.to_vec())
            .send()
            .await
            .context("Failed to upload blob")?
            .error_for_status()
            .context("Upload failed")?;

        Ok(())
    }

    /// Download blob from CAS.
    async fn download_blob(&self, digest: &ActionDigest) -> Result<Vec<u8>> {
        let url = format!("{}/cas/{}", self.endpoint, digest.hash);

        let response = self
            .http_client
            .get(&url)
            .send()
            .await
            .context("Failed to download blob")?
            .error_for_status()
            .context("Download failed")?;

        let data = response.bytes().await?.to_vec();

        // Verify digest
        let computed = ActionDigest::from_bytes(&data);
        if computed.hash != digest.hash {
            return Err(anyhow!(
                "Digest mismatch: expected {}, got {}",
                digest.hash,
                computed.hash
            ));
        }

        Ok(data)
    }

    /// Check if blob exists in CAS.
    async fn has_blob(&self, digest: &ActionDigest) -> Result<bool> {
        let url = format!("{}/cas/{}", self.endpoint, digest.hash);

        let response = self
            .http_client
            .head(&url)
            .send()
            .await
            .context("Failed to check blob")?;

        Ok(response.status().is_success())
    }
}

/// Remote execution cache.
struct RemoteCache {
    cas: Arc<CasClient>,
    memory_cache: Arc<RwLock<HashMap<ActionDigest, ExecutionResponse>>>,
}

impl RemoteCache {
    fn new(cas_endpoint: String) -> Self {
        RemoteCache {
            cas: Arc::new(CasClient::new(cas_endpoint)),
            memory_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Check cache for action result.
    async fn get(&self, action_digest: &ActionDigest) -> Option<ExecutionResponse> {
        // Check memory first
        if let Some(result) = self.memory_cache.read().get(action_digest) {
            return Some(result.clone());
        }

        // Check remote cache
        let cache_key = format!("cache:{}", action_digest.hash);
        let cache_digest = ActionDigest {
            hash: cache_key,
            size_bytes: 0,
        };

        if let Ok(true) = self.cas.has_blob(&cache_digest).await {
            if let Ok(data) = self.cas.download_blob(&cache_digest).await {
                if let Ok(result) = bincode::deserialize::<ExecutionResponse>(&data) {
                    self.memory_cache
                        .write()
                        .insert(action_digest.clone(), result.clone());
                    return Some(result);
                }
            }
        }

        None
    }

    /// Store action result in cache.
    async fn put(&self, action_digest: &ActionDigest, result: &ExecutionResponse) -> Result<()> {
        // Store in memory
        self.memory_cache
            .write()
            .insert(action_digest.clone(), result.clone());

        // Store in remote cache
        let cache_key = format!("cache:{}", action_digest.hash);
        let cache_digest = ActionDigest {
            hash: cache_key,
            size_bytes: 0,
        };

        let data = bincode::serialize(result)?;
        self.cas.upload_blob(&cache_digest, &data).await?;

        Ok(())
    }
}

/// Execution metrics.
#[derive(Debug, Clone, Default)]
pub struct ExecutionMetrics {
    /// Total actions executed.
    pub total_actions: u64,

    /// Cache hits.
    pub cache_hits: u64,

    /// Cache misses.
    pub cache_misses: u64,

    /// Total execution time.
    pub total_execution_time: Duration,

    /// Total upload bytes.
    pub total_upload_bytes: u64,

    /// Total download bytes.
    pub total_download_bytes: u64,

    /// Average execution latency.
    pub avg_execution_latency: Duration,

    /// Average upload throughput (bytes/sec).
    pub avg_upload_throughput: f64,

    /// Average download throughput (bytes/sec).
    pub avg_download_throughput: f64,
}

impl ExecutionMetrics {
    fn record_execution(
        &mut self,
        response: &ExecutionResponse,
        duration: Duration,
        upload_bytes: u64,
        download_bytes: u64,
    ) {
        self.total_actions += 1;

        if response.cached {
            self.cache_hits += 1;
        } else {
            self.cache_misses += 1;
        }

        self.total_execution_time += duration;
        self.total_upload_bytes += upload_bytes;
        self.total_download_bytes += download_bytes;

        // Update averages
        self.avg_execution_latency = self.total_execution_time / self.total_actions as u32;

        if duration.as_secs_f64() > 0.0 {
            self.avg_upload_throughput = upload_bytes as f64 / duration.as_secs_f64();
            self.avg_download_throughput = download_bytes as f64 / duration.as_secs_f64();
        }
    }

    /// Get cache hit rate (0.0 - 1.0).
    pub fn cache_hit_rate(&self) -> f64 {
        if self.total_actions == 0 {
            return 0.0;
        }
        self.cache_hits as f64 / self.total_actions as f64
    }
}

/// Retry policy for transient failures.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Maximum number of retries.
    pub max_retries: u32,

    /// Initial retry delay.
    pub initial_delay: Duration,

    /// Backoff multiplier.
    pub backoff_multiplier: f64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            max_retries: 3,
            initial_delay: Duration::from_millis(100),
            backoff_multiplier: 2.0,
        }
    }
}

impl RetryPolicy {
    async fn execute_with_retry<F, Fut, T>(&self, mut f: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        let mut attempts = 0;
        let mut delay = self.initial_delay;

        loop {
            match f().await {
                Ok(result) => return Ok(result),
                Err(e) => {
                    attempts += 1;
                    if attempts > self.max_retries {
                        return Err(e);
                    }

                    tokio::time::sleep(delay).await;
                    delay = Duration::from_secs_f64(delay.as_secs_f64() * self.backoff_multiplier);
                }
            }
        }
    }
}

/// Remote executor client.
pub struct RemoteExecutor {
    /// gRPC endpoint.
    endpoint: String,

    /// CAS endpoint.
    cas_endpoint: String,

    /// Available workers.
    workers: Arc<RwLock<Vec<RemoteWorker>>>,

    /// Remote cache.
    cache: Arc<RemoteCache>,

    /// CAS client.
    cas: Arc<CasClient>,

    /// Execution metrics.
    metrics: Arc<RwLock<ExecutionMetrics>>,

    /// Retry policy.
    retry_policy: RetryPolicy,

    /// Concurrency limit.
    semaphore: Arc<Semaphore>,

    /// HTTP client.
    http_client: reqwest::Client,
}

impl RemoteExecutor {
    /// Create a new remote executor.
    pub fn new(endpoint: String, cas_endpoint: String, max_concurrent: usize) -> Self {
        RemoteExecutor {
            endpoint: endpoint.clone(),
            cas_endpoint: cas_endpoint.clone(),
            workers: Arc::new(RwLock::new(Vec::new())),
            cache: Arc::new(RemoteCache::new(cas_endpoint.clone())),
            cas: Arc::new(CasClient::new(cas_endpoint)),
            metrics: Arc::new(RwLock::new(ExecutionMetrics::default())),
            retry_policy: RetryPolicy::default(),
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            http_client: reqwest::Client::new(),
        }
    }

    /// Discover and register workers.
    pub async fn discover_workers(&self) -> Result<()> {
        let url = format!("{}/workers", self.endpoint);

        let response = self
            .http_client
            .get(&url)
            .send()
            .await
            .context("Failed to discover workers")?
            .error_for_status()
            .context("Worker discovery failed")?;

        let workers: Vec<RemoteWorker> = response.json().await?;

        *self.workers.write() = workers;

        Ok(())
    }

    /// Select best worker for action.
    fn select_worker(&self, action: &RemoteAction) -> Option<RemoteWorker> {
        let workers = self.workers.read();

        workers
            .iter()
            .filter(|w| w.can_execute(action))
            .min_by(|a, b| a.load.partial_cmp(&b.load).unwrap())
            .cloned()
    }

    /// Upload inputs to CAS.
    async fn upload_inputs(
        &self,
        inputs: &[InputFile],
        status_tx: &mpsc::Sender<ExecutionStatus>,
    ) -> Result<u64> {
        let mut total_bytes = 0u64;
        let mut uploaded_bytes = 0u64;

        // Calculate total size
        for input in inputs {
            total_bytes += input.digest.size_bytes;
        }

        // Upload each input
        for input in inputs {
            // Check if already in CAS
            if !self.cas.has_blob(&input.digest).await? {
                let data = tokio::fs::read(&input.path).await?;
                self.cas.upload_blob(&input.digest, &data).await?;
            }

            uploaded_bytes += input.digest.size_bytes;

            let _ = status_tx
                .send(ExecutionStatus::UploadingInputs {
                    bytes_uploaded: uploaded_bytes,
                    total_bytes,
                })
                .await;
        }

        Ok(uploaded_bytes)
    }

    /// Download outputs from CAS.
    async fn download_outputs(
        &self,
        outputs: &[OutputFile],
        output_dir: &Path,
        status_tx: &mpsc::Sender<ExecutionStatus>,
    ) -> Result<u64> {
        let mut total_bytes = 0u64;
        let mut downloaded_bytes = 0u64;

        // Calculate total size
        for output in outputs {
            total_bytes += output.digest.size_bytes;
        }

        // Download each output
        for output in outputs {
            let data = self.cas.download_blob(&output.digest).await?;

            let output_path = output_dir.join(&output.path);
            if let Some(parent) = output_path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }

            let mut file = tokio::fs::File::create(&output_path).await?;
            file.write_all(&data).await?;

            downloaded_bytes += output.digest.size_bytes;

            let _ = status_tx
                .send(ExecutionStatus::DownloadingOutputs {
                    bytes_downloaded: downloaded_bytes,
                    total_bytes,
                })
                .await;
        }

        Ok(downloaded_bytes)
    }

    /// Execute action remotely.
    pub async fn execute(
        &self,
        action: RemoteAction,
        output_dir: PathBuf,
    ) -> Result<(ExecutionResponse, mpsc::Receiver<ExecutionStatus>)> {
        let (status_tx, _status_rx) = mpsc::channel(32);

        let executor = self.clone();
        let status_tx_clone = status_tx.clone();

        tokio::spawn(async move {
            if let Err(e) = executor
                .execute_internal(action, output_dir, status_tx_clone)
                .await
            {
                eprintln!("Remote execution failed: {}", e);
            }
        });

        // Return immediately with status channel
        // Actual result will be sent through channel
        Err(anyhow!("Use execute_blocking or monitor status channel"))
    }

    /// Execute action remotely (blocking).
    pub async fn execute_blocking(
        &self,
        action: RemoteAction,
        output_dir: PathBuf,
    ) -> Result<ExecutionResponse> {
        let _permit = self.semaphore.acquire().await?;

        let start = Instant::now();
        let action_digest = action.compute_digest();

        // Check cache first
        if !action.do_not_cache {
            if let Some(cached_result) = self.cache.get(&action_digest).await {
                let mut metrics = self.metrics.write();
                metrics.cache_hits += 1;
                return Ok(cached_result);
            }
        }

        // Select worker
        let worker = self
            .select_worker(&action)
            .ok_or_else(|| anyhow!("No suitable worker available"))?;

        let (status_tx, _status_rx) = mpsc::channel(32);

        // Upload inputs
        let upload_bytes = self.upload_inputs(&action.inputs, &status_tx).await?;

        // Execute on worker
        let _ = status_tx
            .send(ExecutionStatus::Executing {
                worker_id: worker.id.clone(),
            })
            .await;

        let response = self
            .execute_on_worker(&worker, &action, &action_digest)
            .await?;

        // Download outputs
        let download_bytes = self
            .download_outputs(&response.outputs, &output_dir, &status_tx)
            .await?;

        let duration = start.elapsed();

        // Update metrics
        {
            let mut metrics = self.metrics.write();
            metrics.record_execution(&response, duration, upload_bytes, download_bytes);
        }

        // Cache result
        if !action.do_not_cache {
            let _ = self.cache.put(&action_digest, &response).await;
        }

        let _ = status_tx.send(ExecutionStatus::Completed).await;

        Ok(response)
    }

    async fn execute_internal(
        &self,
        action: RemoteAction,
        output_dir: PathBuf,
        status_tx: mpsc::Sender<ExecutionStatus>,
    ) -> Result<()> {
        let result = self.execute_blocking(action, output_dir).await;

        match result {
            Ok(_) => {
                let _ = status_tx.send(ExecutionStatus::Completed).await;
            }
            Err(e) => {
                let _ = status_tx
                    .send(ExecutionStatus::Failed {
                        error: e.to_string(),
                    })
                    .await;
            }
        }

        Ok(())
    }

    async fn execute_on_worker(
        &self,
        worker: &RemoteWorker,
        action: &RemoteAction,
        action_digest: &ActionDigest,
    ) -> Result<ExecutionResponse> {
        let url = format!("{}/execute", worker.endpoint);

        // Prepare execution request
        let request = serde_json::json!({
            "action_digest": action_digest,
            "command": action.command,
            "environment": action.environment,
            "working_directory": action.working_directory,
            "timeout": action.timeout.as_secs(),
        });

        let response = self
            .retry_policy
            .execute_with_retry(|| async {
                let resp = self
                    .http_client
                    .post(&url)
                    .json(&request)
                    .send()
                    .await
                    .map_err(anyhow::Error::from)?
                    .error_for_status()
                    .map_err(anyhow::Error::from)?;
                Ok::<_, anyhow::Error>(resp)
            })
            .await?;

        let result: ExecutionResponse = response.json().await?;

        Ok(result)
    }

    /// Get execution metrics.
    pub fn metrics(&self) -> ExecutionMetrics {
        self.metrics.read().clone()
    }

    /// Get available workers.
    pub fn workers(&self) -> Vec<RemoteWorker> {
        self.workers.read().clone()
    }
}

impl Clone for RemoteExecutor {
    fn clone(&self) -> Self {
        RemoteExecutor {
            endpoint: self.endpoint.clone(),
            cas_endpoint: self.cas_endpoint.clone(),
            workers: Arc::clone(&self.workers),
            cache: Arc::clone(&self.cache),
            cas: Arc::clone(&self.cas),
            metrics: Arc::clone(&self.metrics),
            retry_policy: self.retry_policy.clone(),
            semaphore: Arc::clone(&self.semaphore),
            http_client: self.http_client.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_action_digest_from_bytes() {
        let data = b"test data";
        let digest = ActionDigest::from_bytes(data);

        assert_eq!(digest.size_bytes, 9);
        assert!(!digest.hash.is_empty());
    }

    #[test]
    fn test_platform_matching() {
        let requirement = Platform {
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            min_memory_mb: 2048,
            min_cores: 2,
            properties: HashMap::new(),
        };

        let worker = Platform {
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            min_memory_mb: 4096,
            min_cores: 4,
            properties: HashMap::new(),
        };

        assert!(requirement.matches(&worker));

        let insufficient_worker = Platform {
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            min_memory_mb: 1024,
            min_cores: 1,
            properties: HashMap::new(),
        };

        assert!(!requirement.matches(&insufficient_worker));
    }

    #[test]
    fn test_remote_action_compute_digest() {
        let action1 = RemoteAction {
            command: vec!["gcc".to_string(), "-c".to_string(), "main.c".to_string()],
            environment: HashMap::new(),
            working_directory: PathBuf::from("/tmp"),
            inputs: vec![],
            output_paths: vec![],
            platform: Platform::default(),
            timeout: Duration::from_secs(300),
            do_not_cache: false,
        };

        let digest1 = action1.compute_digest();
        let digest2 = action1.compute_digest();

        assert_eq!(digest1, digest2);

        let mut action2 = action1.clone();
        action2.command.push("extra".to_string());
        let digest3 = action2.compute_digest();

        assert_ne!(digest1, digest3);
    }

    #[test]
    fn test_execution_metrics() {
        let mut metrics = ExecutionMetrics::default();

        let response = ExecutionResponse {
            exit_code: 0,
            stdout: vec![],
            stderr: vec![],
            outputs: vec![],
            cached: false,
            execution_duration: Duration::from_secs(1),
            worker_id: Some("worker1".to_string()),
        };

        metrics.record_execution(&response, Duration::from_secs(2), 1024, 2048);

        assert_eq!(metrics.total_actions, 1);
        assert_eq!(metrics.cache_misses, 1);
        assert_eq!(metrics.total_upload_bytes, 1024);
        assert_eq!(metrics.total_download_bytes, 2048);
    }

    #[test]
    fn test_cache_hit_rate() {
        let mut metrics = ExecutionMetrics::default();

        assert_eq!(metrics.cache_hit_rate(), 0.0);

        metrics.total_actions = 10;
        metrics.cache_hits = 7;

        assert_eq!(metrics.cache_hit_rate(), 0.7);
    }

    #[test]
    fn test_retry_policy_defaults() {
        let policy = RetryPolicy::default();

        assert_eq!(policy.max_retries, 3);
        assert_eq!(policy.initial_delay, Duration::from_millis(100));
        assert_eq!(policy.backoff_multiplier, 2.0);
    }

    #[test]
    fn test_worker_can_execute() {
        let worker = RemoteWorker {
            id: "worker1".to_string(),
            endpoint: "http://localhost:8080".to_string(),
            platform: Platform::default(),
            load: 0.5,
            last_heartbeat: SystemTime::now(),
            healthy: true,
        };

        let action = RemoteAction {
            command: vec!["gcc".to_string()],
            environment: HashMap::new(),
            working_directory: PathBuf::from("/tmp"),
            inputs: vec![],
            output_paths: vec![],
            platform: Platform::default(),
            timeout: Duration::from_secs(300),
            do_not_cache: false,
        };

        assert!(worker.can_execute(&action));

        let overloaded_worker = RemoteWorker {
            load: 0.95,
            ..worker
        };

        assert!(!overloaded_worker.can_execute(&action));
    }
}
