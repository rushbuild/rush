// Rush Remote - Remote Execution
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Remote execution client for distributing builds across a cluster.

use anyhow::Result;
use rush_action::Action;
use rush_exec::ActionResult;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Semaphore;

/// Remote execution request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteExecutionRequest {
    /// Action to execute.
    pub action: Action,

    /// Platform requirements (OS, architecture, etc.).
    pub platform: HashMap<String, String>,

    /// Skip cache lookup.
    pub skip_cache: bool,
}

/// Remote execution response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteExecutionResponse {
    /// Execution result.
    pub result: ActionResult,

    /// Worker that executed the action.
    pub worker_id: String,

    /// Whether result came from cache.
    pub cached: bool,

    /// Execution time in milliseconds.
    pub execution_time_ms: u64,
}

/// Remote execution client.
pub struct RemoteExecutor {
    /// Portable standard-protocol adapter, selected by native build commands.
    reapi: Option<crate::build_executor::BuildExecutor>,
    /// Endpoint URL (e.g., "http://build-cluster.example.com:8080").
    endpoint: String,

    /// HTTP client.
    client: reqwest::Client,

    /// Maximum concurrent remote executions.
    concurrency_limit: Arc<Semaphore>,

    /// Statistics.
    stats: Arc<tokio::sync::Mutex<RemoteExecutorStats>>,
}

/// Remote executor statistics.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoteExecutorStats {
    /// Total actions executed remotely.
    pub total_actions: usize,

    /// Actions served from remote cache.
    pub cache_hits: usize,

    /// Actions executed on remote workers.
    pub remote_executions: usize,

    /// Total time spent waiting for remote execution (ms).
    pub total_wait_time_ms: u64,

    /// Failed remote executions (fallback to local).
    pub failures: usize,
}

impl RemoteExecutor {
    /// Whether native execution uses the standard protocol and strict contracts.
    pub fn is_reapi(&self) -> bool {
        self.reapi.is_some()
    }

    /// Create a new remote executor.
    pub fn new(endpoint: String) -> Self {
        Self::with_concurrency(endpoint, 100)
    }

    /// Create with custom concurrency limit.
    pub fn with_concurrency(endpoint: String, max_concurrent: usize) -> Self {
        RemoteExecutor {
            reapi: None,
            endpoint,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(600)) // 10 minute timeout
                .build()
                .expect("Failed to create HTTP client"),
            concurrency_limit: Arc::new(Semaphore::new(max_concurrent)),
            stats: Arc::new(tokio::sync::Mutex::new(RemoteExecutorStats::default())),
        }
    }

    /// Configure a native build with a pinned REAPI worker/toolchain.
    pub fn with_reapi(
        endpoint: String,
        root: std::path::PathBuf,
        instance: String,
        platform: std::collections::BTreeMap<String, String>,
    ) -> Result<Self> {
        let mut executor = Self::new(endpoint.clone());
        executor.reapi = Some(crate::build_executor::BuildExecutor::new(
            root, endpoint, instance, platform,
        )?);
        Ok(executor)
    }

    /// Execute a portable native action without falling back to local tools.
    pub async fn execute_native(
        &self,
        executable: &std::path::Path,
        args: &[String],
        environment: Vec<(String, String)>,
        cwd: Option<&std::path::Path>,
        inputs: &[std::path::PathBuf],
        outputs: &[std::path::PathBuf],
        timeout: std::time::Duration,
    ) -> Result<crate::reapi::Outcome> {
        let _permit = self.concurrency_limit.acquire().await?;
        let start = std::time::Instant::now();
        let result = self
            .reapi
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("native execution requires REAPI configuration"))?
            .execute(executable, args, environment, cwd, inputs, outputs, timeout)
            .await;
        let mut stats = self.stats.lock().await;
        stats.total_wait_time_ms += start.elapsed().as_millis() as u64;
        match &result {
            Ok(outcome) => {
                stats.total_actions += 1;
                if outcome.cached {
                    stats.cache_hits += 1;
                } else {
                    stats.remote_executions += 1;
                }
            }
            Err(_) => stats.failures += 1,
        }
        result
    }

    /// Execute an action remotely.
    pub async fn execute(&self, action: &Action) -> Result<RemoteExecutionResponse> {
        if self.reapi.is_some() {
            let path = |artifact: &rush_action::Artifact| match artifact {
                rush_action::Artifact::Source(path) => path.clone(),
                rush_action::Artifact::Derived { output_path, .. } => output_path.clone(),
            };
            let start = std::time::Instant::now();
            let outcome = self
                .execute_native(
                    &action.command.executable,
                    &action.command.args,
                    action
                        .env
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                    action.command.working_dir.as_deref(),
                    &action.inputs.iter().map(path).collect::<Vec<_>>(),
                    &action.outputs.iter().map(path).collect::<Vec<_>>(),
                    action.command.execution_info.timeout,
                )
                .await?;
            return Ok(RemoteExecutionResponse {
                worker_id: outcome.worker,
                cached: outcome.cached,
                execution_time_ms: start.elapsed().as_millis() as u64,
                result: ActionResult {
                    action_id: action.id.clone(),
                    exit_code: outcome.exit_code,
                    stdout: outcome.stdout,
                    stderr: outcome.stderr,
                    outputs: action.outputs.clone(),
                    output_records: Vec::new(),
                    duration: start.elapsed(),
                    cached: outcome.cached,
                },
            });
        }
        // Acquire semaphore permit to limit concurrency
        let _permit = self.concurrency_limit.acquire().await?;

        let start = std::time::Instant::now();

        // Build request
        let request = RemoteExecutionRequest {
            action: action.clone(),
            platform: self.detect_platform(),
            skip_cache: false,
        };

        // Send to remote executor
        let url = format!("{}/execute", self.endpoint);
        let response = self.client.post(&url).json(&request).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Remote execution failed with status {}: {}", status, body);
        }

        let exec_response: RemoteExecutionResponse = response.json().await?;

        // Update statistics
        let mut stats = self.stats.lock().await;
        stats.total_actions += 1;
        if exec_response.cached {
            stats.cache_hits += 1;
        } else {
            stats.remote_executions += 1;
        }
        stats.total_wait_time_ms += start.elapsed().as_millis() as u64;

        Ok(exec_response)
    }

    /// Execute with fallback to local on failure.
    pub async fn execute_with_fallback(
        &self,
        action: &Action,
        local_fallback: impl std::future::Future<Output = Result<ActionResult>>,
    ) -> Result<ActionResult> {
        if self.reapi.is_some() {
            return Ok(self.execute(action).await?.result);
        }
        match self.execute(action).await {
            Ok(response) => Ok(response.result),
            Err(e) => {
                eprintln!("⚠️  Remote execution failed: {}", e);
                eprintln!("   Falling back to local execution...");

                // Update failure stats
                let mut stats = self.stats.lock().await;
                stats.failures += 1;
                drop(stats);

                local_fallback.await
            }
        }
    }

    /// Upload input files to remote cache.
    pub async fn upload_inputs(&self, _paths: &[std::path::PathBuf]) -> Result<Vec<String>> {
        anyhow::bail!("use REAPI execution to snapshot and upload declared inputs")
    }

    /// Download output files from remote cache.
    pub async fn download_outputs(&self, _digests: &[String]) -> Result<Vec<Vec<u8>>> {
        anyhow::bail!("use REAPI execution to verify and materialize declared outputs")
    }

    /// Get executor statistics.
    pub async fn stats(&self) -> RemoteExecutorStats {
        self.stats.lock().await.clone()
    }

    /// Check if remote executor is healthy.
    pub async fn health_check(&self) -> Result<bool> {
        if let Some(reapi) = &self.reapi {
            return reapi.health_check().await;
        }
        let url = format!("{}/health", self.endpoint);
        let response = self
            .client
            .get(&url)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await?;

        Ok(response.status().is_success())
    }

    /// Detect platform requirements.
    fn detect_platform(&self) -> HashMap<String, String> {
        let mut platform = HashMap::new();
        platform.insert("os".to_string(), std::env::consts::OS.to_string());
        platform.insert("arch".to_string(), std::env::consts::ARCH.to_string());
        platform
    }

    /// Get endpoint URL.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

/// Remote execution strategy (implements ExecutionStrategy).
pub struct RemoteStrategy {
    /// Remote executor.
    executor: RemoteExecutor,
}

/// Share a strict standard-protocol executor with the native action manager.
pub struct ReapiStrategy(pub Arc<RemoteExecutor>);

#[async_trait::async_trait]
impl rush_exec::ExecutionStrategy for ReapiStrategy {
    async fn execute(&self, action: &Action) -> Result<ActionResult> {
        Ok(self.0.execute(action).await?.result)
    }
    fn can_execute(&self, _action: &Action) -> bool {
        true
    }
    fn name(&self) -> &str {
        "reapi"
    }
}

impl RemoteStrategy {
    /// Create a new remote strategy.
    pub fn new(endpoint: String) -> Self {
        RemoteStrategy {
            executor: RemoteExecutor::new(endpoint),
        }
    }

    /// Get the underlying executor.
    pub fn executor(&self) -> &RemoteExecutor {
        &self.executor
    }
}

#[async_trait::async_trait]
impl rush_exec::ExecutionStrategy for RemoteStrategy {
    async fn execute(&self, action: &Action) -> Result<ActionResult> {
        let response = self.executor.execute(action).await?;
        Ok(response.result)
    }

    fn can_execute(&self, _action: &Action) -> bool {
        // Remote executor can execute any action
        true
    }

    fn name(&self) -> &str {
        "remote"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_detection() {
        let executor = RemoteExecutor::new("http://localhost:8080".to_string());
        assert!(!executor.is_reapi());
        let platform = executor.detect_platform();

        assert!(platform.contains_key("os"));
        assert!(platform.contains_key("arch"));
    }

    #[tokio::test]
    async fn test_stats() {
        let executor = RemoteExecutor::new("http://localhost:8080".to_string());
        let stats = executor.stats().await;

        assert_eq!(stats.total_actions, 0);
        assert_eq!(stats.cache_hits, 0);
        assert_eq!(stats.remote_executions, 0);
    }
}
