// Rush Build - Lock-Free Task Queue
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Lock-free task queues for minimal contention.
//!
//! Implements lock-free data structures for task queues, eliminating
//! lock contention and enabling true parallel task submission and stealing.

use anyhow::Result;
use crossbeam::queue::{ArrayQueue, SegQueue};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Lock-free MPMC (multi-producer, multi-consumer) task queue.
///
/// Uses Crossbeam's SegQueue for unbounded lock-free operations.
pub struct LockFreeQueue<T> {
    /// The underlying lock-free queue.
    queue: SegQueue<T>,

    /// Statistics.
    stats: Arc<QueueStats>,
}

/// Lock-free bounded MPMC queue.
///
/// Uses Crossbeam's ArrayQueue for bounded operations with better cache locality.
pub struct BoundedLockFreeQueue<T> {
    /// The underlying bounded lock-free queue.
    queue: ArrayQueue<T>,

    /// Queue capacity.
    capacity: usize,

    /// Statistics.
    stats: Arc<QueueStats>,
}

/// Queue statistics.
#[derive(Debug, Default)]
pub struct QueueStats {
    /// Total push operations.
    pub pushes: AtomicU64,

    /// Total pop operations.
    pub pops: AtomicU64,

    /// Failed push attempts (bounded queue only).
    pub push_failures: AtomicU64,

    /// Failed pop attempts (empty queue).
    pub pop_failures: AtomicU64,

    /// Current queue size estimate.
    pub size_estimate: AtomicUsize,
}

impl<T> LockFreeQueue<T> {
    /// Create a new unbounded lock-free queue.
    pub fn new() -> Self {
        LockFreeQueue {
            queue: SegQueue::new(),
            stats: Arc::new(QueueStats::default()),
        }
    }

    /// Push an item to the queue.
    ///
    /// Never fails (unbounded).
    pub fn push(&self, item: T) {
        self.queue.push(item);
        self.stats.pushes.fetch_add(1, Ordering::Relaxed);
        self.stats.size_estimate.fetch_add(1, Ordering::Relaxed);
    }

    /// Try to pop an item from the queue.
    pub fn pop(&self) -> Option<T> {
        match self.queue.pop() {
            Some(item) => {
                self.stats.pops.fetch_add(1, Ordering::Relaxed);
                self.stats.size_estimate.fetch_sub(1, Ordering::Relaxed);
                Some(item)
            }
            None => {
                self.stats.pop_failures.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Check if queue is empty.
    ///
    /// Note: This is a snapshot and may be stale immediately.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Get queue length estimate.
    ///
    /// Note: This is an estimate and may not be precise due to concurrent operations.
    pub fn len_estimate(&self) -> usize {
        self.stats.size_estimate.load(Ordering::Relaxed)
    }

    /// Get statistics.
    pub fn stats(&self) -> &QueueStats {
        &self.stats
    }
}

impl<T> Default for LockFreeQueue<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> BoundedLockFreeQueue<T> {
    /// Create a new bounded lock-free queue with specified capacity.
    pub fn new(capacity: usize) -> Self {
        BoundedLockFreeQueue {
            queue: ArrayQueue::new(capacity),
            capacity,
            stats: Arc::new(QueueStats::default()),
        }
    }

    /// Try to push an item to the queue.
    ///
    /// Returns the item back if the queue is full.
    pub fn push(&self, item: T) -> Result<(), T> {
        match self.queue.push(item) {
            Ok(()) => {
                self.stats.pushes.fetch_add(1, Ordering::Relaxed);
                self.stats.size_estimate.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(item) => {
                self.stats.push_failures.fetch_add(1, Ordering::Relaxed);
                Err(item)
            }
        }
    }

    /// Try to pop an item from the queue.
    pub fn pop(&self) -> Option<T> {
        match self.queue.pop() {
            Some(item) => {
                self.stats.pops.fetch_add(1, Ordering::Relaxed);
                self.stats.size_estimate.fetch_sub(1, Ordering::Relaxed);
                Some(item)
            }
            None => {
                self.stats.pop_failures.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Check if queue is full.
    pub fn is_full(&self) -> bool {
        self.queue.is_full()
    }

    /// Check if queue is empty.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Get capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Get queue length.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Get statistics.
    pub fn stats(&self) -> &QueueStats {
        &self.stats
    }
}

/// Lock-free work-stealing deque.
///
/// Optimized for LIFO local operations and FIFO stealing.
pub struct WorkStealingDeque<T> {
    /// Local LIFO stack (for the owner thread).
    ///
    /// A `SegQueue` is FIFO. Using one here made the owner pop its OLDEST
    /// item, which is the opposite of the design: the owner takes the most
    /// recently pushed work because it is still cache-hot, and thieves take
    /// the oldest. The struct claimed LIFO in three comments and delivered
    /// FIFO. A mutex is sound for this side because only the owner touches
    /// it; the steal queue stays lock-free.
    local_stack: Mutex<Vec<T>>,

    /// Steal queue (for thieves).
    steal_queue: SegQueue<T>,

    /// Balance threshold - when to move items between queues.
    balance_threshold: usize,

    /// Owner thread ID.
    _owner_thread: std::thread::ThreadId,

    /// Statistics.
    stats: Arc<DequeStats>,
}

/// Work-stealing deque statistics.
#[derive(Debug, Default)]
pub struct DequeStats {
    /// Local pushes (from owner).
    pub local_pushes: AtomicU64,

    /// Local pops (from owner).
    pub local_pops: AtomicU64,

    /// Steals (from other threads).
    pub steals: AtomicU64,

    /// Failed steal attempts.
    pub steal_failures: AtomicU64,

    /// Rebalance operations.
    pub rebalances: AtomicU64,
}

impl<T> WorkStealingDeque<T> {
    /// Create a new work-stealing deque.
    pub fn new() -> Self {
        WorkStealingDeque {
            local_stack: Mutex::new(Vec::new()),
            steal_queue: SegQueue::new(),
            balance_threshold: 32,
            _owner_thread: std::thread::current().id(),
            stats: Arc::new(DequeStats::default()),
        }
    }

    /// Push an item (from owner thread).
    ///
    /// Uses LIFO for better cache locality.
    pub fn push(&self, item: T) {
        if let Ok(mut stack) = self.local_stack.lock() {
            stack.push(item);
        }
        self.stats.local_pushes.fetch_add(1, Ordering::Relaxed);

        // Periodically rebalance to steal queue
        if self
            .stats
            .local_pushes
            .load(Ordering::Relaxed)
            .is_multiple_of(self.balance_threshold as u64)
        {
            self.rebalance();
        }
    }

    /// Pop an item (from owner thread).
    ///
    /// Tries local LIFO first, then steal queue.
    pub fn pop(&self) -> Option<T> {
        // Try local stack first (LIFO: most recently pushed is cache-hot)
        if let Some(item) = self.local_stack.lock().ok().and_then(|mut s| s.pop()) {
            self.stats.local_pops.fetch_add(1, Ordering::Relaxed);
            return Some(item);
        }

        // Fall back to steal queue
        if let Some(item) = self.steal_queue.pop() {
            self.stats.local_pops.fetch_add(1, Ordering::Relaxed);
            return Some(item);
        }

        None
    }

    /// Steal an item (from other threads).
    ///
    /// Always steals from the steal queue (FIFO for better distribution).
    pub fn steal(&self) -> Option<T> {
        match self.steal_queue.pop() {
            Some(item) => {
                self.stats.steals.fetch_add(1, Ordering::Relaxed);
                Some(item)
            }
            None => {
                self.stats.steal_failures.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Check if deque is empty.
    pub fn is_empty(&self) -> bool {
        self.local_stack.lock().map(|s| s.is_empty()).unwrap_or(true)
            && self.steal_queue.is_empty()
    }

    /// Rebalance local stack to steal queue.
    ///
    /// Moves half the items to make them available for stealing.
    fn rebalance(&self) {
        let target = self.balance_threshold / 2;

        // Hand thieves the OLDEST work (the front of the stack) and keep the
        // newest for the owner. Popping instead would give away exactly the
        // cache-hot items the owner should run next.
        let moved = match self.local_stack.lock() {
            Ok(mut stack) => {
                let take = target.min(stack.len());
                let drained: Vec<T> = stack.drain(..take).collect();
                let n = drained.len();
                for item in drained {
                    self.steal_queue.push(item);
                }
                n
            }
            Err(_) => 0,
        };

        if moved > 0 {
            self.stats.rebalances.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Get statistics.
    pub fn stats(&self) -> &DequeStats {
        &self.stats
    }
}

impl<T> Default for WorkStealingDeque<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Lock-free task pool with priority support.
///
/// Uses multiple lock-free queues for different priority levels.
pub struct PriorityTaskPool<T> {
    /// High-priority queue.
    high_priority: LockFreeQueue<T>,

    /// Normal-priority queue.
    normal_priority: LockFreeQueue<T>,

    /// Low-priority queue.
    low_priority: LockFreeQueue<T>,

    /// Total tasks.
    total_tasks: AtomicUsize,

    /// Shutdown flag.
    shutdown: AtomicBool,
}

/// Task priority level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Low = 0,
    Normal = 1,
    High = 2,
}

impl<T> PriorityTaskPool<T> {
    /// Create a new priority task pool.
    pub fn new() -> Self {
        PriorityTaskPool {
            high_priority: LockFreeQueue::new(),
            normal_priority: LockFreeQueue::new(),
            low_priority: LockFreeQueue::new(),
            total_tasks: AtomicUsize::new(0),
            shutdown: AtomicBool::new(false),
        }
    }

    /// Submit a task with specified priority.
    pub fn submit(&self, task: T, priority: Priority) {
        if self.shutdown.load(Ordering::Acquire) {
            return;
        }

        match priority {
            Priority::High => self.high_priority.push(task),
            Priority::Normal => self.normal_priority.push(task),
            Priority::Low => self.low_priority.push(task),
        }

        self.total_tasks.fetch_add(1, Ordering::Release);
    }

    /// Get next task (prioritized).
    pub fn get_next(&self) -> Option<T> {
        // Try high priority first
        if let Some(task) = self.high_priority.pop() {
            self.total_tasks.fetch_sub(1, Ordering::Release);
            return Some(task);
        }

        // Then normal priority
        if let Some(task) = self.normal_priority.pop() {
            self.total_tasks.fetch_sub(1, Ordering::Release);
            return Some(task);
        }

        // Finally low priority
        if let Some(task) = self.low_priority.pop() {
            self.total_tasks.fetch_sub(1, Ordering::Release);
            return Some(task);
        }

        None
    }

    /// Get total pending tasks.
    pub fn pending_count(&self) -> usize {
        self.total_tasks.load(Ordering::Acquire)
    }

    /// Check if pool is empty.
    pub fn is_empty(&self) -> bool {
        self.pending_count() == 0
    }

    /// Initiate shutdown.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
    }

    /// Check if shutdown.
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }
}

impl<T> Default for PriorityTaskPool<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn test_lock_free_queue() {
        let queue = Arc::new(LockFreeQueue::new());

        // Push items
        for i in 0..100 {
            queue.push(i);
        }

        // Pop items
        let mut sum = 0;
        while let Some(item) = queue.pop() {
            sum += item;
        }

        assert_eq!(sum, (0..100).sum::<i32>());

        let stats = queue.stats();
        assert_eq!(stats.pushes.load(Ordering::Relaxed), 100);
        assert_eq!(stats.pops.load(Ordering::Relaxed), 100);
    }

    #[test]
    fn test_bounded_queue() {
        let queue = BoundedLockFreeQueue::new(10);

        // Fill queue
        for i in 0..10 {
            assert!(queue.push(i).is_ok());
        }

        assert!(queue.is_full());

        // Try to push when full
        assert!(queue.push(999).is_err());

        let stats = queue.stats();
        assert_eq!(stats.push_failures.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_concurrent_queue() {
        let queue = Arc::new(LockFreeQueue::new());
        let mut handles = vec![];

        // Spawn producer threads
        for i in 0..4 {
            let q = Arc::clone(&queue);
            handles.push(thread::spawn(move || {
                for j in 0..1000 {
                    q.push(i * 1000 + j);
                }
            }));
        }

        // Wait for producers
        for handle in handles {
            handle.join().unwrap();
        }

        // Verify all items were pushed
        let stats = queue.stats();
        assert_eq!(stats.pushes.load(Ordering::Relaxed), 4000);

        // Spawn consumer threads
        let mut handles = vec![];
        for _ in 0..4 {
            let q = Arc::clone(&queue);
            handles.push(thread::spawn(move || {
                let mut count = 0;
                while q.pop().is_some() {
                    count += 1;
                }
                count
            }));
        }

        // Collect results
        let total: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();

        assert_eq!(total, 4000);
    }

    #[test]
    fn test_work_stealing_deque() {
        let deque = WorkStealingDeque::new();

        // Push items
        for i in 0..100 {
            deque.push(i);
        }

        // Pop should get them in LIFO order (from local stack)
        let first = deque.pop().unwrap();
        assert!(first >= 50); // Should be from the later half due to LIFO + rebalancing

        // Steal should get them in FIFO order (from steal queue)
        let stolen = deque.steal();
        assert!(stolen.is_some());
    }

    #[test]
    fn test_priority_pool() {
        let pool = PriorityTaskPool::new();

        // Submit tasks with different priorities
        pool.submit(1, Priority::Low);
        pool.submit(2, Priority::High);
        pool.submit(3, Priority::Normal);
        pool.submit(4, Priority::High);

        // Should get high priority tasks first
        assert_eq!(pool.get_next(), Some(2));
        assert_eq!(pool.get_next(), Some(4));

        // Then normal
        assert_eq!(pool.get_next(), Some(3));

        // Finally low
        assert_eq!(pool.get_next(), Some(1));

        assert!(pool.is_empty());
    }

    #[test]
    fn test_priority_pool_concurrent() {
        const SUBMITTED: usize = 1000;
        let pool = Arc::new(PriorityTaskPool::new());
        let producer_done = Arc::new(AtomicBool::new(false));
        let drained_total = Arc::new(AtomicUsize::new(0));
        let mut handles = vec![];

        // Producer thread
        let p = Arc::clone(&pool);
        let done_flag = Arc::clone(&producer_done);
        handles.push(thread::spawn(move || {
            for i in 0..SUBMITTED {
                let priority = match i % 3 {
                    0 => Priority::High,
                    1 => Priority::Normal,
                    _ => Priority::Low,
                };
                p.submit(i, priority);
            }
            // Release-store pairs with the consumers' acquire-load, so a
            // consumer that observes the flag also observes every submit.
            done_flag.store(true, Ordering::Release);
        }));

        // Consumer threads
        //
        // A consumer must not stop merely because the pool looks empty: the
        // producer above is still running, so an early consumer sees an
        // empty pool before the first submit. The previous version broke out
        // on that, and when all four consumers left before the producer
        // finished, the leftover items failed the final assertion perhaps
        // one run in ten. Stop on "producer finished AND pool drained"
        // instead.
        for _ in 0..4 {
            let p = Arc::clone(&pool);
            let done = Arc::clone(&producer_done);
            let drained = Arc::clone(&drained_total);
            handles.push(thread::spawn(move || {
                // Bounded wait. An item that the pool holds but will not
                // hand back leaves is_empty() false forever, and without a
                // deadline this loop would hang the suite instead of
                // reporting the defect.
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_secs(30);
                loop {
                    if p.get_next().is_some() {
                        drained.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    if done.load(Ordering::Acquire) && p.is_empty() {
                        break;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "pool still reports items after 30s: producer_done={}, \
                         drained={}",
                        done.load(Ordering::Acquire),
                        drained.load(Ordering::Relaxed)
                    );
                    std::thread::yield_now();
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // Count what came out, not just that the pool ended empty: an empty
        // pool is also what losing items looks like.
        assert_eq!(drained_total.load(Ordering::Relaxed), SUBMITTED);
        assert!(pool.is_empty());
    }
}
