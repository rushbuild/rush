// Rush Build - Task Scheduler
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Task scheduler with critical path and cost-based prioritization.

use std::collections::{HashMap, HashSet, VecDeque};

/// Build task with scheduling metadata.
#[derive(Debug, Clone)]
pub struct ScheduledTask {
    /// Task identifier (target label).
    pub id: String,

    /// Task dependencies (must complete before this task).
    pub deps: Vec<String>,

    /// Estimated cost (duration in milliseconds).
    pub estimated_cost: u64,

    /// Critical path length (longest chain to completion).
    pub critical_path_length: u64,

    /// Priority (higher = execute sooner).
    pub priority: i64,
}

/// Task scheduler with priority-based execution.
pub struct TaskScheduler {
    /// All tasks indexed by ID.
    tasks: HashMap<String, ScheduledTask>,

    /// Completed tasks.
    completed: HashSet<String>,

    /// Tasks currently running.
    running: HashSet<String>,

    /// Ready queue (tasks with all dependencies satisfied).
    ready_queue: VecDeque<String>,
}

impl TaskScheduler {
    /// Create a new task scheduler.
    pub fn new() -> Self {
        TaskScheduler {
            tasks: HashMap::new(),
            completed: HashSet::new(),
            running: HashSet::new(),
            ready_queue: VecDeque::new(),
        }
    }

    /// Add a task to the scheduler.
    pub fn add_task(&mut self, task: ScheduledTask) {
        let id = task.id.clone();
        self.tasks.insert(id.clone(), task);

        // If no dependencies, add to ready queue
        if self.tasks[&id].deps.is_empty() {
            self.ready_queue.push_back(id);
        }
    }

    /// Compute critical path lengths for all tasks.
    pub fn compute_critical_paths(&mut self) {
        // Build reverse dependency graph
        let mut reverse_deps: HashMap<String, Vec<String>> = HashMap::new();
        for (id, task) in &self.tasks {
            for dep in &task.deps {
                reverse_deps
                    .entry(dep.clone())
                    .or_default()
                    .push(id.clone());
            }
        }

        // Find leaf tasks (no dependents)
        let leaf_tasks: Vec<String> = self
            .tasks
            .keys()
            .filter(|id| !reverse_deps.contains_key(*id))
            .cloned()
            .collect();

        // Topological traversal from leaves backward
        let mut visited = HashSet::new();
        let mut stack = leaf_tasks.clone();

        for leaf in &leaf_tasks {
            if let Some(task) = self.tasks.get_mut(leaf) {
                task.critical_path_length = task.estimated_cost;
            }
        }

        while let Some(task_id) = stack.pop() {
            if visited.contains(&task_id) {
                continue;
            }
            visited.insert(task_id.clone());

            let (current_path_length, deps) = {
                let task = self.tasks.get(&task_id).unwrap();
                (task.critical_path_length, task.deps.clone())
            };

            // Update all dependencies
            for dep_id in &deps {
                if let Some(dep_task) = self.tasks.get_mut(dep_id) {
                    let new_length = current_path_length + dep_task.estimated_cost;
                    if new_length > dep_task.critical_path_length {
                        dep_task.critical_path_length = new_length;
                    }

                    if !visited.contains(dep_id) {
                        stack.push(dep_id.clone());
                    }
                }
            }
        }
    }

    /// Compute priorities for all tasks.
    pub fn compute_priorities(&mut self) {
        for task in self.tasks.values_mut() {
            // Priority = critical_path_length (longer paths = higher priority)
            // This ensures tasks on the critical path execute first
            task.priority = task.critical_path_length as i64;
        }
    }

    /// Get the next task to execute (highest priority from ready queue).
    pub fn get_next_task(&mut self) -> Option<String> {
        if self.ready_queue.is_empty() {
            return None;
        }

        // Find highest priority task in ready queue
        let mut best_idx = 0;
        let mut best_priority = i64::MIN;

        for (idx, task_id) in self.ready_queue.iter().enumerate() {
            if let Some(task) = self.tasks.get(task_id) {
                if task.priority > best_priority {
                    best_priority = task.priority;
                    best_idx = idx;
                }
            }
        }

        let task_id = self.ready_queue.remove(best_idx)?;
        self.running.insert(task_id.clone());
        Some(task_id)
    }

    /// Mark a task as completed and update ready queue.
    pub fn complete_task(&mut self, task_id: &str) {
        self.running.remove(task_id);
        self.completed.insert(task_id.to_string());

        // Find tasks that are now ready (all dependencies completed)
        let all_task_ids: Vec<String> = self.tasks.keys().cloned().collect();

        for id in all_task_ids {
            if self.completed.contains(&id) || self.running.contains(&id) {
                continue;
            }

            if self.ready_queue.iter().any(|ready_id| ready_id == &id) {
                continue;
            }

            let task = &self.tasks[&id];
            let all_deps_complete = task.deps.iter().all(|dep| self.completed.contains(dep));

            if all_deps_complete {
                self.ready_queue.push_back(id);
            }
        }
    }

    /// Get number of tasks ready to execute.
    pub fn ready_count(&self) -> usize {
        self.ready_queue.len()
    }

    /// Get number of tasks currently running.
    pub fn running_count(&self) -> usize {
        self.running.len()
    }

    /// Get number of completed tasks.
    pub fn completed_count(&self) -> usize {
        self.completed.len()
    }

    /// Check if all tasks are completed.
    pub fn is_complete(&self) -> bool {
        self.completed.len() == self.tasks.len()
    }

    /// Estimate task cost based on rule type and size.
    pub fn estimate_cost(rule_type: &str, file_count: usize) -> u64 {
        let base_cost = match rule_type {
            "cc_library" | "cc_binary" | "cc_test" => 500, // C++ compilation is slow
            "rust_library" | "rust_binary" | "rust_test" => 1000, // Rust is slower
            "py_library" | "py_binary" | "py_test" => 50,  // Python is fast (no compilation)
            _ => 200,
        };

        // Scale by number of files
        base_cost * file_count.max(1) as u64
    }
}

impl Default for TaskScheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_scheduling() {
        let mut scheduler = TaskScheduler::new();

        scheduler.add_task(ScheduledTask {
            id: "task1".to_string(),
            deps: vec![],
            estimated_cost: 100,
            critical_path_length: 0,
            priority: 0,
        });

        scheduler.add_task(ScheduledTask {
            id: "task2".to_string(),
            deps: vec!["task1".to_string()],
            estimated_cost: 200,
            critical_path_length: 0,
            priority: 0,
        });

        scheduler.compute_critical_paths();
        scheduler.compute_priorities();

        assert_eq!(scheduler.ready_count(), 1);

        let first = scheduler.get_next_task();
        assert_eq!(first, Some("task1".to_string()));

        scheduler.complete_task("task1");
        assert_eq!(scheduler.ready_count(), 1);

        let second = scheduler.get_next_task();
        assert_eq!(second, Some("task2".to_string()));
    }

    #[test]
    fn test_critical_path_priority() {
        let mut scheduler = TaskScheduler::new();

        // Task A (short, on critical path)
        scheduler.add_task(ScheduledTask {
            id: "A".to_string(),
            deps: vec![],
            estimated_cost: 100,
            critical_path_length: 0,
            priority: 0,
        });

        // Task B (long, on critical path, depends on A)
        scheduler.add_task(ScheduledTask {
            id: "B".to_string(),
            deps: vec!["A".to_string()],
            estimated_cost: 1000,
            critical_path_length: 0,
            priority: 0,
        });

        // Task C (independent, short)
        scheduler.add_task(ScheduledTask {
            id: "C".to_string(),
            deps: vec![],
            estimated_cost: 50,
            critical_path_length: 0,
            priority: 0,
        });

        scheduler.compute_critical_paths();
        scheduler.compute_priorities();

        // A should have higher priority (on critical path)
        let task_a = &scheduler.tasks["A"];
        let task_c = &scheduler.tasks["C"];
        assert!(task_a.priority > task_c.priority);
    }
}
