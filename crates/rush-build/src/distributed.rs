// Rush Build - Distributed Build Coordination
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Distributed build coordination for scaling across multiple machines.
//!
//! Coordinates build execution across a cluster of workers, handling task
//! distribution, load balancing, and fault tolerance.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Distributed build coordinator.
pub struct DistributedCoordinator {
    /// Worker pool.
    workers: Arc<RwLock<WorkerPool>>,

    /// Task queue.
    task_queue: Arc<RwLock<TaskQueue>>,

    /// Coordinator configuration.
    config: CoordinatorConfig,

    /// Statistics.
    stats: Arc<DistributedStats>,
}

/// Coordinator configuration.
#[derive(Debug, Clone)]
pub struct CoordinatorConfig {
    /// Heartbeat interval.
    pub heartbeat_interval: Duration,

    /// Worker timeout.
    pub worker_timeout: Duration,

    /// Maximum retries per task.
    pub max_retries: usize,

    /// Enable task stealing.
    pub enable_task_stealing: bool,

    /// Load balancing strategy.
    pub load_balance_strategy: LoadBalanceStrategy,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        CoordinatorConfig {
            heartbeat_interval: Duration::from_secs(5),
            worker_timeout: Duration::from_secs(30),
            max_retries: 3,
            enable_task_stealing: true,
            load_balance_strategy: LoadBalanceStrategy::LeastLoaded,
        }
    }
}

/// Load balancing strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadBalanceStrategy {
    /// Round-robin assignment.
    RoundRobin,

    /// Assign to least loaded worker.
    LeastLoaded,

    /// Assign based on worker capabilities.
    CapabilityBased,

    /// Random assignment.
    Random,
}

/// Worker pool.
struct WorkerPool {
    /// Active workers.
    workers: HashMap<String, Worker>,

    /// Next worker index for round-robin.
    next_worker_idx: usize,
}

/// Worker information.
#[derive(Debug, Clone)]
struct Worker {
    /// Worker ID.
    id: String,

    /// Worker address.
    _address: String,

    /// Worker capabilities.
    capabilities: WorkerCapabilities,

    /// Current load (number of active tasks).
    current_load: usize,

    /// Maximum concurrent tasks.
    max_concurrent: usize,

    /// Last heartbeat time.
    last_heartbeat: Instant,

    /// Worker status.
    status: WorkerStatus,
}

/// Worker capabilities.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerCapabilities {
    /// CPU count.
    pub cpu_count: usize,

    /// Available memory (bytes).
    pub memory_bytes: u64,

    /// Supported platforms.
    pub platforms: Vec<String>,

    /// Installed toolchains.
    pub toolchains: Vec<String>,
}

/// Worker status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerStatus {
    /// Worker is idle.
    Idle,

    /// Worker is busy.
    Busy,

    /// Worker is offline.
    Offline,
}

/// Task queue.
struct TaskQueue {
    /// Pending tasks.
    pending: VecDeque<BuildTask>,

    /// In-progress tasks.
    in_progress: HashMap<String, TaskExecution>,

    /// Completed tasks.
    completed: HashSet<String>,

    /// Failed tasks.
    failed: HashMap<String, TaskFailure>,
}

/// Build task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildTask {
    /// Task ID.
    pub id: String,

    /// Task action.
    pub action: String,

    /// Required capabilities.
    pub required_capabilities: Option<WorkerCapabilities>,

    /// Estimated duration (milliseconds).
    pub estimated_duration_ms: u64,

    /// Priority (higher = more urgent).
    pub priority: u32,

    /// Dependencies (must complete first).
    pub dependencies: Vec<String>,
}

/// Task execution state.
struct TaskExecution {
    /// Task being executed.
    task: BuildTask,

    /// Worker executing the task.
    worker_id: String,

    /// Start time.
    started_at: Instant,

    /// Retry count.
    retries: usize,
}

/// Task failure information.
#[derive(Debug, Clone)]
struct TaskFailure {
    /// Task that failed.
    _task: BuildTask,

    /// Error message.
    _error: String,

    /// Number of attempts.
    _attempts: usize,
}

/// Distributed build statistics.
#[derive(Debug, Default)]
pub struct DistributedStats {
    /// Total tasks submitted.
    pub tasks_submitted: AtomicU64,

    /// Tasks completed.
    pub tasks_completed: AtomicU64,

    /// Tasks failed.
    pub tasks_failed: AtomicU64,

    /// Tasks retried.
    pub tasks_retried: AtomicU64,

    /// Active workers.
    pub active_workers: AtomicUsize,

    /// Total execution time (microseconds).
    pub total_execution_time_us: AtomicU64,
}

impl DistributedCoordinator {
    /// Create a new distributed coordinator.
    pub fn new(config: CoordinatorConfig) -> Self {
        DistributedCoordinator {
            workers: Arc::new(RwLock::new(WorkerPool {
                workers: HashMap::new(),
                next_worker_idx: 0,
            })),
            task_queue: Arc::new(RwLock::new(TaskQueue {
                pending: VecDeque::new(),
                in_progress: HashMap::new(),
                completed: HashSet::new(),
                failed: HashMap::new(),
            })),
            config,
            stats: Arc::new(DistributedStats::default()),
        }
    }

    /// Register a worker.
    pub async fn register_worker(
        &self,
        id: String,
        address: String,
        capabilities: WorkerCapabilities,
        max_concurrent: usize,
    ) -> Result<()> {
        let mut pool = self.workers.write().await;

        let worker = Worker {
            id: id.clone(),
            _address: address,
            capabilities,
            current_load: 0,
            max_concurrent,
            last_heartbeat: Instant::now(),
            status: WorkerStatus::Idle,
        };

        pool.workers.insert(id, worker);
        self.stats.active_workers.fetch_add(1, Ordering::Relaxed);

        Ok(())
    }

    /// Unregister a worker.
    pub async fn unregister_worker(&self, id: &str) -> Result<()> {
        let mut pool = self.workers.write().await;

        if pool.workers.remove(id).is_some() {
            self.stats.active_workers.fetch_sub(1, Ordering::Relaxed);
        }

        Ok(())
    }

    /// Submit a task for execution.
    pub async fn submit_task(&self, task: BuildTask) -> Result<()> {
        let mut queue = self.task_queue.write().await;

        // Check if dependencies are satisfied
        for dep in &task.dependencies {
            if !queue.completed.contains(dep) && !queue.in_progress.contains_key(dep) {
                // Dependency not yet started - add to pending
                queue.pending.push_back(task);
                self.stats.tasks_submitted.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
        }

        // Dependencies satisfied - ready to execute
        queue.pending.push_back(task);
        self.stats.tasks_submitted.fetch_add(1, Ordering::Relaxed);

        Ok(())
    }

    /// Schedule pending tasks to workers.
    pub async fn schedule_tasks(&self) -> Result<usize> {
        let mut scheduled = 0;

        loop {
            let task = {
                let mut queue = self.task_queue.write().await;

                // Find next ready task
                let ready_task_idx = queue.pending.iter().position(|task| {
                    task.dependencies
                        .iter()
                        .all(|dep| queue.completed.contains(dep))
                });

                match ready_task_idx {
                    Some(idx) => queue.pending.remove(idx),
                    None => break,
                }
            };

            if let Some(task) = task {
                // Find suitable worker
                if let Some(worker_id) = self.select_worker(&task).await {
                    self.assign_task(worker_id, task).await?;
                    scheduled += 1;
                } else {
                    // No suitable worker available - put back in queue
                    let mut queue = self.task_queue.write().await;
                    queue.pending.push_front(task);
                    break;
                }
            }
        }

        Ok(scheduled)
    }

    /// Select a worker for a task based on load balancing strategy.
    async fn select_worker(&self, task: &BuildTask) -> Option<String> {
        let pool = self.workers.read().await;

        match self.config.load_balance_strategy {
            LoadBalanceStrategy::RoundRobin => self.select_worker_round_robin(&pool),
            LoadBalanceStrategy::LeastLoaded => self.select_worker_least_loaded(&pool),
            LoadBalanceStrategy::CapabilityBased => self.select_worker_by_capability(&pool, task),
            LoadBalanceStrategy::Random => self.select_worker_random(&pool),
        }
    }

    /// Round-robin worker selection.
    fn select_worker_round_robin(&self, pool: &WorkerPool) -> Option<String> {
        let available: Vec<_> = pool
            .workers
            .values()
            .filter(|w| w.status != WorkerStatus::Offline && w.current_load < w.max_concurrent)
            .collect();

        if available.is_empty() {
            return None;
        }

        let idx = pool.next_worker_idx % available.len();
        Some(available[idx].id.clone())
    }

    /// Least loaded worker selection.
    fn select_worker_least_loaded(&self, pool: &WorkerPool) -> Option<String> {
        pool.workers
            .values()
            .filter(|w| w.status != WorkerStatus::Offline && w.current_load < w.max_concurrent)
            .min_by_key(|w| w.current_load)
            .map(|w| w.id.clone())
    }

    /// Capability-based worker selection.
    fn select_worker_by_capability(&self, pool: &WorkerPool, task: &BuildTask) -> Option<String> {
        let required = task.required_capabilities.as_ref()?;

        pool.workers
            .values()
            .filter(|w| {
                w.status != WorkerStatus::Offline
                    && w.current_load < w.max_concurrent
                    && self.worker_has_capabilities(w, required)
            })
            .min_by_key(|w| w.current_load)
            .map(|w| w.id.clone())
    }

    /// Random worker selection.
    fn select_worker_random(&self, pool: &WorkerPool) -> Option<String> {
        use rand::seq::SliceRandom;

        let available: Vec<_> = pool
            .workers
            .values()
            .filter(|w| w.status != WorkerStatus::Offline && w.current_load < w.max_concurrent)
            .collect();

        available
            .choose(&mut rand::thread_rng())
            .map(|w| w.id.clone())
    }

    /// Check if worker has required capabilities.
    fn worker_has_capabilities(&self, worker: &Worker, required: &WorkerCapabilities) -> bool {
        worker.capabilities.cpu_count >= required.cpu_count
            && worker.capabilities.memory_bytes >= required.memory_bytes
            && required
                .platforms
                .iter()
                .all(|p| worker.capabilities.platforms.contains(p))
            && required
                .toolchains
                .iter()
                .all(|t| worker.capabilities.toolchains.contains(t))
    }

    /// Assign a task to a worker.
    async fn assign_task(&self, worker_id: String, task: BuildTask) -> Result<()> {
        let task_id = task.id.clone();

        // Update worker state
        {
            let mut pool = self.workers.write().await;
            if let Some(worker) = pool.workers.get_mut(&worker_id) {
                worker.current_load += 1;
                worker.status = WorkerStatus::Busy;
            }
        }

        // Update task queue
        {
            let mut queue = self.task_queue.write().await;
            queue.in_progress.insert(
                task_id.clone(),
                TaskExecution {
                    task,
                    worker_id,
                    started_at: Instant::now(),
                    retries: 0,
                },
            );
        }

        Ok(())
    }

    /// Mark a task as completed.
    pub async fn complete_task(&self, task_id: &str) -> Result<()> {
        let worker_id = {
            let mut queue = self.task_queue.write().await;

            if let Some(execution) = queue.in_progress.remove(task_id) {
                let duration = execution.started_at.elapsed();
                self.stats
                    .total_execution_time_us
                    .fetch_add(duration.as_micros() as u64, Ordering::Relaxed);

                queue.completed.insert(task_id.to_string());
                self.stats.tasks_completed.fetch_add(1, Ordering::Relaxed);

                Some(execution.worker_id)
            } else {
                None
            }
        };

        // Update worker state
        if let Some(worker_id) = worker_id {
            let mut pool = self.workers.write().await;
            if let Some(worker) = pool.workers.get_mut(&worker_id) {
                worker.current_load = worker.current_load.saturating_sub(1);
                if worker.current_load == 0 {
                    worker.status = WorkerStatus::Idle;
                }
            }
        }

        Ok(())
    }

    /// Mark a task as failed.
    pub async fn fail_task(&self, task_id: &str, error: String) -> Result<()> {
        let mut queue = self.task_queue.write().await;

        if let Some(mut execution) = queue.in_progress.remove(task_id) {
            execution.retries += 1;

            if execution.retries < self.config.max_retries {
                // Retry the task
                queue.pending.push_back(execution.task);
                self.stats.tasks_retried.fetch_add(1, Ordering::Relaxed);
            } else {
                // Max retries exceeded - mark as failed
                queue.failed.insert(
                    task_id.to_string(),
                    TaskFailure {
                        _task: execution.task,
                        _error: error,
                        _attempts: execution.retries,
                    },
                );
                self.stats.tasks_failed.fetch_add(1, Ordering::Relaxed);
            }

            // Update worker state
            let mut pool = self.workers.write().await;
            if let Some(worker) = pool.workers.get_mut(&execution.worker_id) {
                worker.current_load = worker.current_load.saturating_sub(1);
                if worker.current_load == 0 {
                    worker.status = WorkerStatus::Idle;
                }
            }
        }

        Ok(())
    }

    /// Handle worker heartbeat.
    pub async fn heartbeat(&self, worker_id: &str) -> Result<()> {
        let mut pool = self.workers.write().await;

        if let Some(worker) = pool.workers.get_mut(worker_id) {
            worker.last_heartbeat = Instant::now();
        }

        Ok(())
    }

    /// Check for timed-out workers and reassign their tasks.
    pub async fn check_timeouts(&self) -> Result<()> {
        let now = Instant::now();
        let timeout = self.config.worker_timeout;

        let timed_out_workers: Vec<String> = {
            let pool = self.workers.read().await;
            pool.workers
                .values()
                .filter(|w| now.duration_since(w.last_heartbeat) > timeout)
                .map(|w| w.id.clone())
                .collect()
        };

        for worker_id in timed_out_workers {
            // Mark worker as offline
            {
                let mut pool = self.workers.write().await;
                if let Some(worker) = pool.workers.get_mut(&worker_id) {
                    worker.status = WorkerStatus::Offline;
                }
            }

            // Reassign tasks from this worker
            let tasks_to_reassign: Vec<String> = {
                let queue = self.task_queue.read().await;
                queue
                    .in_progress
                    .iter()
                    .filter(|(_, exec)| exec.worker_id == worker_id)
                    .map(|(id, _)| id.clone())
                    .collect()
            };

            for task_id in tasks_to_reassign {
                self.fail_task(&task_id, "Worker timeout".to_string())
                    .await?;
            }
        }

        Ok(())
    }

    /// Get statistics.
    pub fn stats(&self) -> DistributedStatsSnapshot {
        DistributedStatsSnapshot {
            tasks_submitted: self.stats.tasks_submitted.load(Ordering::Relaxed),
            tasks_completed: self.stats.tasks_completed.load(Ordering::Relaxed),
            tasks_failed: self.stats.tasks_failed.load(Ordering::Relaxed),
            tasks_retried: self.stats.tasks_retried.load(Ordering::Relaxed),
            active_workers: self.stats.active_workers.load(Ordering::Relaxed),
            total_execution_time_us: self.stats.total_execution_time_us.load(Ordering::Relaxed),
        }
    }
}

/// Snapshot of distributed statistics.
#[derive(Debug, Clone)]
pub struct DistributedStatsSnapshot {
    /// Total tasks submitted.
    pub tasks_submitted: u64,

    /// Tasks completed.
    pub tasks_completed: u64,

    /// Tasks failed.
    pub tasks_failed: u64,

    /// Tasks retried.
    pub tasks_retried: u64,

    /// Active workers.
    pub active_workers: usize,

    /// Total execution time.
    pub total_execution_time_us: u64,
}

impl DistributedStatsSnapshot {
    /// Get completion rate.
    pub fn completion_rate(&self) -> f64 {
        if self.tasks_submitted == 0 {
            return 0.0;
        }
        (self.tasks_completed as f64 / self.tasks_submitted as f64) * 100.0
    }

    /// Get failure rate.
    pub fn failure_rate(&self) -> f64 {
        if self.tasks_submitted == 0 {
            return 0.0;
        }
        (self.tasks_failed as f64 / self.tasks_submitted as f64) * 100.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_worker_registration() {
        let coordinator = DistributedCoordinator::new(CoordinatorConfig::default());

        let caps = WorkerCapabilities {
            cpu_count: 4,
            memory_bytes: 8 * 1024 * 1024 * 1024,
            platforms: vec!["linux".to_string()],
            toolchains: vec!["rustc".to_string()],
        };

        coordinator
            .register_worker("worker1".to_string(), "localhost:8080".to_string(), caps, 4)
            .await
            .unwrap();

        let stats = coordinator.stats();
        assert_eq!(stats.active_workers, 1);
    }

    #[tokio::test]
    async fn test_task_submission() {
        let coordinator = DistributedCoordinator::new(CoordinatorConfig::default());

        let task = BuildTask {
            id: "task1".to_string(),
            action: "compile".to_string(),
            required_capabilities: None,
            estimated_duration_ms: 1000,
            priority: 1,
            dependencies: vec![],
        };

        coordinator.submit_task(task).await.unwrap();

        let stats = coordinator.stats();
        assert_eq!(stats.tasks_submitted, 1);
    }
}
