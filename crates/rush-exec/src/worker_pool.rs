// Rush Exec - Persistent Worker Pool
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Persistent worker pool for fast tool execution.
//!
//! Maintains a pool of long-running worker processes to reduce the overhead
//! of spawning processes for frequently-used tools like compilers.

use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

/// A persistent worker process.
pub struct Worker {
    /// The running child process.
    child: Child,
    /// Worker type (e.g., "g++", "rustc").
    worker_type: String,
}

impl Worker {
    /// Spawn a new worker process.
    pub async fn spawn(executable: &str, worker_type: String) -> Result<Self> {
        let child = Command::new(executable)
            .arg("--worker-mode") // Hypothetical flag for worker mode
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        Ok(Worker { child, worker_type })
    }

    /// Execute a command in this worker.
    pub async fn execute(&mut self, args: &[String]) -> Result<WorkerResult> {
        let stdin = self
            .child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Worker stdin not available"))?;

        // Send command to worker
        let command = serde_json::json!({
            "args": args,
        });
        stdin.write_all(command.to_string().as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;

        // Read response from worker
        let stdout = self
            .child
            .stdout
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Worker stdout not available"))?;
        let mut reader = BufReader::new(stdout);
        let mut response = String::new();
        reader.read_line(&mut response).await?;

        let result: WorkerResult = serde_json::from_str(&response)?;
        Ok(result)
    }

    /// Shutdown this worker gracefully.
    pub async fn shutdown(mut self) -> Result<()> {
        if let Some(mut stdin) = self.child.stdin.take() {
            // Send shutdown command
            stdin.write_all(b"{\"shutdown\":true}\n").await?;
            stdin.flush().await?;
        }

        // Wait for process to exit
        let _ = self.child.wait().await?;
        Ok(())
    }
}

/// Result from a worker execution.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct WorkerResult {
    /// Exit code.
    pub exit_code: i32,
    /// Standard output.
    pub stdout: Vec<u8>,
    /// Standard error.
    pub stderr: Vec<u8>,
}

/// Pool of persistent workers.
pub struct WorkerPool {
    /// Active workers, keyed by worker type.
    workers: Arc<Mutex<HashMap<String, Vec<Worker>>>>,
    /// Maximum workers per type.
    max_workers_per_type: usize,
}

impl WorkerPool {
    /// Create a new worker pool.
    pub fn new(max_workers_per_type: usize) -> Self {
        WorkerPool {
            workers: Arc::new(Mutex::new(HashMap::new())),
            max_workers_per_type,
        }
    }

    /// Get or create a worker for the given executable.
    pub async fn get_worker(&self, executable: &str) -> Result<Worker> {
        let worker_type = PathBuf::from(executable)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(executable)
            .to_string();

        let mut workers = self.workers.lock().await;
        let workers_list = workers.entry(worker_type.clone()).or_insert_with(Vec::new);

        // Try to reuse an existing worker
        if let Some(worker) = workers_list.pop() {
            return Ok(worker);
        }

        // Spawn a new worker if under the limit
        if workers_list.len() < self.max_workers_per_type {
            return Worker::spawn(executable, worker_type).await;
        }

        // Pool is full, spawn a temporary worker
        Worker::spawn(executable, worker_type).await
    }

    /// Return a worker to the pool for reuse.
    pub async fn return_worker(&self, worker: Worker) {
        let worker_type = worker.worker_type.clone();
        let mut workers = self.workers.lock().await;
        let workers_list = workers.entry(worker_type).or_insert_with(Vec::new);

        if workers_list.len() < self.max_workers_per_type {
            workers_list.push(worker);
        } else {
            // Pool is full, shutdown the worker
            let _ = worker.shutdown().await;
        }
    }

    /// Shutdown all workers in the pool.
    pub async fn shutdown_all(&self) -> Result<()> {
        let mut workers = self.workers.lock().await;
        for (_, workers_list) in workers.drain() {
            for worker in workers_list {
                let _ = worker.shutdown().await;
            }
        }
        Ok(())
    }

    /// Get pool statistics.
    pub async fn stats(&self) -> HashMap<String, usize> {
        let workers = self.workers.lock().await;
        workers.iter().map(|(k, v)| (k.clone(), v.len())).collect()
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        // Note: Can't do async shutdown in Drop, workers will be killed
        // Users should call shutdown_all() explicitly before dropping
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_worker_pool_creation() {
        let pool = WorkerPool::new(4);
        let stats = pool.stats().await;
        assert_eq!(stats.len(), 0);
    }

    #[tokio::test]
    async fn test_worker_pool_stats() {
        let pool = WorkerPool::new(4);
        let stats = pool.stats().await;
        assert_eq!(stats.len(), 0);
    }
}
