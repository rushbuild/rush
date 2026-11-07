// Rush Build - Work-Stealing Scheduler
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Work-stealing scheduler for optimal CPU utilization.
//!
//! Implements a work-stealing algorithm where idle workers steal tasks from
//! busy workers, ensuring better load balancing and CPU utilization than
//! traditional thread pools.

use anyhow::Result;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;

/// Type alias for task queue
type TaskQueue = Arc<Mutex<VecDeque<Box<dyn Task>>>>;

/// Work-stealing task.
pub trait Task: Send + Sync {
    /// Execute the task.
    fn execute(&self) -> Result<()>;

    /// Task priority (higher = more important).
    fn priority(&self) -> u32 {
        0
    }

    /// Estimated task duration in milliseconds.
    fn estimated_duration_ms(&self) -> u64 {
        100
    }
}

/// Work-stealing scheduler.
pub struct WorkStealingScheduler {
    /// Per-worker task queues.
    worker_queues: Vec<TaskQueue>,

    /// Global overflow queue for load balancing.
    global_queue: TaskQueue,

    /// Worker threads.
    workers: Vec<WorkerHandle>,

    /// Number of workers.
    num_workers: usize,

    /// Shutdown flag.
    shutdown: Arc<AtomicBool>,

    /// Active task counter.
    active_tasks: Arc<AtomicUsize>,

    /// Notification for new work.
    notify: Arc<Notify>,

    /// Statistics.
    stats: Arc<WorkStealingStats>,
}

/// Worker thread handle.
struct WorkerHandle {
    /// Worker ID.
    _id: usize,

    /// Join handle.
    handle: Option<std::thread::JoinHandle<()>>,
}

/// Context for worker thread execution.
struct WorkerContext {
    /// Worker ID.
    worker_id: usize,
    /// Local queue for this worker.
    local_queue: TaskQueue,
    /// Global overflow queue.
    global_queue: TaskQueue,
    /// All worker queues for work stealing.
    all_queues: Vec<TaskQueue>,
    /// Shutdown flag.
    shutdown: Arc<AtomicBool>,
    /// Active task counter.
    active_tasks: Arc<AtomicUsize>,
    /// Notification for new work.
    notify: Arc<Notify>,
    /// Statistics.
    stats: Arc<WorkStealingStats>,
}

/// Work-stealing statistics.
#[derive(Debug, Default)]
pub struct WorkStealingStats {
    /// Total tasks executed.
    pub tasks_executed: AtomicUsize,

    /// Successful steals.
    pub steals_succeeded: AtomicUsize,

    /// Failed steal attempts.
    pub steals_failed: AtomicUsize,

    /// Tasks executed from local queue.
    pub local_executions: AtomicUsize,

    /// Tasks executed from global queue.
    pub global_executions: AtomicUsize,

    /// Total steal attempts.
    pub steal_attempts: AtomicUsize,
}

impl WorkStealingScheduler {
    /// Create a new work-stealing scheduler.
    pub fn new(num_workers: usize) -> Self {
        let num_workers = num_workers.max(1);

        let mut worker_queues = Vec::new();
        for _ in 0..num_workers {
            worker_queues.push(Arc::new(Mutex::new(VecDeque::new())));
        }

        WorkStealingScheduler {
            worker_queues,
            global_queue: Arc::new(Mutex::new(VecDeque::new())),
            workers: Vec::new(),
            num_workers,
            shutdown: Arc::new(AtomicBool::new(false)),
            active_tasks: Arc::new(AtomicUsize::new(0)),
            notify: Arc::new(Notify::new()),
            stats: Arc::new(WorkStealingStats::default()),
        }
    }

    /// Start the scheduler.
    pub fn start(&mut self) {
        for worker_id in 0..self.num_workers {
            let ctx = WorkerContext {
                worker_id,
                local_queue: Arc::clone(&self.worker_queues[worker_id]),
                global_queue: Arc::clone(&self.global_queue),
                all_queues: self.worker_queues.to_vec(),
                shutdown: Arc::clone(&self.shutdown),
                active_tasks: Arc::clone(&self.active_tasks),
                notify: Arc::clone(&self.notify),
                stats: Arc::clone(&self.stats),
            };

            let handle = std::thread::spawn(move || {
                Self::worker_loop(ctx);
            });

            self.workers.push(WorkerHandle {
                _id: worker_id,
                handle: Some(handle),
            });
        }
    }

    /// Submit a task to the scheduler.
    pub fn submit<T: Task + 'static>(&self, task: T) {
        let boxed_task: Box<dyn Task> = Box::new(task);

        // Try to push to least-loaded worker queue
        let mut min_size = usize::MAX;
        let mut min_idx = 0;

        for (idx, queue) in self.worker_queues.iter().enumerate() {
            let size = queue.lock().len();
            if size < min_size {
                min_size = size;
                min_idx = idx;
            }
        }

        // If all queues are too full, use global queue
        if min_size > 100 {
            self.global_queue.lock().push_back(boxed_task);
        } else {
            self.worker_queues[min_idx].lock().push_back(boxed_task);
        }

        self.active_tasks.fetch_add(1, Ordering::SeqCst);
        self.notify.notify_one();
    }

    /// Worker loop.
    fn worker_loop(ctx: WorkerContext) {
        loop {
            if ctx.shutdown.load(Ordering::Relaxed) {
                break;
            }

            // Try to get task from local queue
            let task = {
                let mut queue = ctx.local_queue.lock();
                queue.pop_front()
            };

            if let Some(task) = task {
                ctx.stats.local_executions.fetch_add(1, Ordering::Relaxed);
                Self::execute_task(task, &ctx.active_tasks, &ctx.stats);
                continue;
            }

            // Try global queue
            let task = {
                let mut queue = ctx.global_queue.lock();
                queue.pop_front()
            };

            if let Some(task) = task {
                ctx.stats.global_executions.fetch_add(1, Ordering::Relaxed);
                Self::execute_task(task, &ctx.active_tasks, &ctx.stats);
                continue;
            }

            // Try to steal from other workers
            ctx.stats.steal_attempts.fetch_add(1, Ordering::Relaxed);
            let stolen = Self::try_steal(ctx.worker_id, &ctx.all_queues, &ctx.stats);

            if let Some(task) = stolen {
                ctx.stats.steals_succeeded.fetch_add(1, Ordering::Relaxed);
                Self::execute_task(task, &ctx.active_tasks, &ctx.stats);
                continue;
            }

            ctx.stats.steals_failed.fetch_add(1, Ordering::Relaxed);

            // No work available, wait for notification
            if ctx.active_tasks.load(Ordering::Relaxed) == 0 {
                // Use async notify with timeout
                let rt = tokio::runtime::Handle::try_current();
                if let Ok(handle) = rt {
                    handle.block_on(async {
                        tokio::select! {
                            _ = ctx.notify.notified() => {}
                            _ = tokio::time::sleep(tokio::time::Duration::from_millis(10)) => {}
                        }
                    });
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }
        }
    }

    /// Try to steal a task from another worker.
    fn try_steal(
        worker_id: usize,
        all_queues: &[TaskQueue],
        _stats: &Arc<WorkStealingStats>,
    ) -> Option<Box<dyn Task>> {
        // Random stealing strategy
        let num_workers = all_queues.len();
        let mut victim = (worker_id + 1) % num_workers;

        for _ in 0..num_workers {
            if victim == worker_id {
                victim = (victim + 1) % num_workers;
                continue;
            }

            // Try to steal from victim's queue (from the back for better locality)
            let mut victim_queue = all_queues[victim].lock();
            if let Some(task) = victim_queue.pop_back() {
                return Some(task);
            }

            victim = (victim + 1) % num_workers;
        }

        None
    }

    /// Execute a task.
    fn execute_task(
        task: Box<dyn Task>,
        active_tasks: &Arc<AtomicUsize>,
        stats: &Arc<WorkStealingStats>,
    ) {
        if let Err(e) = task.execute() {
            eprintln!("Task execution failed: {}", e);
        }

        active_tasks.fetch_sub(1, Ordering::SeqCst);
        stats.tasks_executed.fetch_add(1, Ordering::Relaxed);
    }

    /// Wait for all tasks to complete.
    pub fn wait_idle(&self) {
        while self.active_tasks.load(Ordering::Relaxed) > 0 {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Shutdown the scheduler.
    pub fn shutdown(mut self) {
        self.shutdown.store(true, Ordering::Relaxed);

        // Notify all workers to wake up
        for _ in 0..self.num_workers {
            self.notify.notify_one();
        }

        // Wait for workers to finish
        for mut worker in self.workers.drain(..) {
            if let Some(handle) = worker.handle.take() {
                let _ = handle.join();
            }
        }
    }

    /// Get statistics.
    pub fn stats(&self) -> WorkStealingStatsSnapshot {
        WorkStealingStatsSnapshot {
            tasks_executed: self.stats.tasks_executed.load(Ordering::Relaxed),
            steals_succeeded: self.stats.steals_succeeded.load(Ordering::Relaxed),
            steals_failed: self.stats.steals_failed.load(Ordering::Relaxed),
            local_executions: self.stats.local_executions.load(Ordering::Relaxed),
            global_executions: self.stats.global_executions.load(Ordering::Relaxed),
            steal_attempts: self.stats.steal_attempts.load(Ordering::Relaxed),
        }
    }

    /// Get number of pending tasks.
    pub fn pending_tasks(&self) -> usize {
        self.active_tasks.load(Ordering::Relaxed)
    }
}

/// Snapshot of work-stealing statistics.
#[derive(Debug, Clone)]
pub struct WorkStealingStatsSnapshot {
    /// Total tasks executed.
    pub tasks_executed: usize,

    /// Successful steals.
    pub steals_succeeded: usize,

    /// Failed steal attempts.
    pub steals_failed: usize,

    /// Tasks executed from local queue.
    pub local_executions: usize,

    /// Tasks executed from global queue.
    pub global_executions: usize,

    /// Total steal attempts.
    pub steal_attempts: usize,
}

impl WorkStealingStatsSnapshot {
    /// Get steal success rate.
    pub fn steal_success_rate(&self) -> f64 {
        if self.steal_attempts == 0 {
            return 0.0;
        }
        (self.steals_succeeded as f64 / self.steal_attempts as f64) * 100.0
    }

    /// Get average queue utilization.
    pub fn local_execution_rate(&self) -> f64 {
        if self.tasks_executed == 0 {
            return 0.0;
        }
        (self.local_executions as f64 / self.tasks_executed as f64) * 100.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    struct TestTask {
        counter: Arc<AtomicU64>,
        sleep_ms: u64,
    }

    impl Task for TestTask {
        fn execute(&self) -> Result<()> {
            std::thread::sleep(std::time::Duration::from_millis(self.sleep_ms));
            self.counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn estimated_duration_ms(&self) -> u64 {
            self.sleep_ms
        }
    }

    #[test]
    fn test_work_stealing_scheduler() {
        let mut scheduler = WorkStealingScheduler::new(4);
        scheduler.start();

        let counter = Arc::new(AtomicU64::new(0));

        // Submit 100 tasks
        for _ in 0..100 {
            let task = TestTask {
                counter: Arc::clone(&counter),
                sleep_ms: 1,
            };
            scheduler.submit(task);
        }

        scheduler.wait_idle();

        assert_eq!(counter.load(Ordering::SeqCst), 100);

        let stats = scheduler.stats();
        assert_eq!(stats.tasks_executed, 100);

        scheduler.shutdown();
    }

    #[test]
    fn test_work_stealing() {
        let mut scheduler = WorkStealingScheduler::new(2);
        scheduler.start();

        let counter = Arc::new(AtomicU64::new(0));

        // Submit many tasks to trigger stealing
        for _ in 0..1000 {
            let task = TestTask {
                counter: Arc::clone(&counter),
                sleep_ms: 0,
            };
            scheduler.submit(task);
        }

        scheduler.wait_idle();

        // Every task must run. Do NOT assert that steals happened: submit()
        // pushes to the least-loaded queue, so a balanced run legitimately
        // steals nothing, and asserting otherwise tests the scheduler's luck
        // rather than its behaviour. try_steal is covered directly below,
        // against a deliberate imbalance.
        let stats = scheduler.stats();
        assert_eq!(
            counter.load(Ordering::Relaxed),
            1000,
            "every submitted task must execute"
        );
        assert_eq!(stats.tasks_executed, 1000);

        scheduler.shutdown();
    }

    /// An idle worker must be able to take work from a loaded one.
    ///
    /// Driven directly, because the scheduler balances well enough that a
    /// live run may never need to steal.
    #[test]
    fn try_steal_takes_work_from_a_loaded_victim() {
        let stats = Arc::new(WorkStealingStats::default());
        let counter = Arc::new(AtomicU64::new(0));

        // Two queues: worker 0 idle, worker 1 holding work.
        let queues: Vec<TaskQueue> = (0..2)
            .map(|_| Arc::new(Mutex::new(VecDeque::new())))
            .collect();
        for _ in 0..3 {
            queues[1].lock().push_back(Box::new(TestTask {
                counter: Arc::clone(&counter),
                sleep_ms: 0,
            }) as Box<dyn Task>);
        }

        let stolen = WorkStealingScheduler::try_steal(0, &queues, &stats);
        assert!(stolen.is_some(), "an idle worker must steal from a loaded one");
        assert_eq!(queues[1].lock().len(), 2, "exactly one task moves per steal");

        // Nothing left to take once the victim is drained.
        while WorkStealingScheduler::try_steal(0, &queues, &stats).is_some() {}
        assert!(queues[1].lock().is_empty());
    }
}
