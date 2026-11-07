//! Work-First Build Orchestrator with Core Utilization Monitoring
//!
//! This orchestrator implements the "cocktail party" strategy for lazy .bzl loading:
//! - Start compilation immediately with available work
//! - Only load .bzl files when cores are idle OR dependencies require them
//! - Keep cores saturated throughout the build
//!
//! ## Key Principle
//!
//! **Prefer execution over preparation**: Don't load what you don't need yet.
//! If cores are busy compiling, don't waste memory loading more .bzl files.

use crate::lazy_evaluator::{ImmediateAction, LazyBuildAnalysis};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Core utilization monitor
///
/// Tracks how many CPU cores are currently busy executing actions.
/// The orchestrator uses this to decide when to load more .bzl files.
#[derive(Debug, Clone)]
pub struct CoreMonitor {
    /// Total number of CPU cores available
    num_cores: usize,

    /// Number of cores currently busy (atomic for thread-safe updates)
    currently_busy: Arc<AtomicUsize>,
}

impl CoreMonitor {
    /// Create a new core monitor
    ///
    /// Automatically detects the number of CPU cores.
    pub fn new() -> Self {
        Self {
            num_cores: num_cpus::get(),
            currently_busy: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Create a core monitor with a specific core count (for testing)
    pub fn with_cores(num_cores: usize) -> Self {
        Self {
            num_cores,
            currently_busy: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Get current core utilization as a fraction (0.0 to 1.0)
    ///
    /// - 0.0 = all cores idle
    /// - 1.0 = all cores busy
    pub fn utilization(&self) -> f64 {
        let busy = self.currently_busy.load(Ordering::Relaxed);
        busy as f64 / self.num_cores as f64
    }

    /// Get total number of cores
    pub fn num_cores(&self) -> usize {
        self.num_cores
    }

    /// Get number of currently busy cores
    pub fn busy_cores(&self) -> usize {
        self.currently_busy.load(Ordering::Relaxed)
    }

    /// Check if there's capacity to spawn more actions
    pub fn has_capacity(&self) -> bool {
        self.busy_cores() < self.num_cores
    }

    /// Mark a core as busy
    pub fn mark_busy(&self) {
        self.currently_busy.fetch_add(1, Ordering::Relaxed);
    }

    /// Mark a core as idle
    pub fn mark_idle(&self) {
        self.currently_busy.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Default for CoreMonitor {
    fn default() -> Self {
        Self::new()
    }
}

/// Priority level for .bzl loading
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LoadPriority {
    /// Core is required by current target (load immediately)
    Required,

    /// Nice to have for future work (load only if cores idle)
    Prefetch,

    /// Low priority background loading
    Background,
}

/// A .bzl file that might need to be loaded
#[derive(Debug, Clone)]
pub struct PendingBzlLoad {
    /// Path to the .bzl file
    pub path: PathBuf,

    /// Symbols needed from this file
    pub symbols: Vec<String>,

    /// Priority for loading this file
    pub priority: LoadPriority,
}

/// Work-first build orchestrator
///
/// Coordinates action execution with lazy .bzl loading based on core utilization.
pub struct WorkFirstOrchestrator {
    /// Monitor for core utilization
    core_monitor: CoreMonitor,

    /// Actions ready to execute immediately (source files to compile)
    ready_queue: VecDeque<ImmediateAction>,

    /// Actions waiting for dependencies to complete
    pending_queue: Vec<ImmediateAction>,

    /// .bzl files that might need to be loaded
    /// Sorted by priority (Required first, then Prefetch, then Background)
    bzl_load_queue: Vec<PendingBzlLoad>,

    /// Cache of already-loaded .bzl files
    /// Maps path → symbols loaded from that file
    loaded_bzl_files: HashMap<PathBuf, Vec<String>>,

    /// Configuration: minimum core utilization before loading more .bzl files
    /// Default: 0.8 (only load .bzl if <80% cores busy)
    min_utilization_for_bzl_load: f64,
}

impl WorkFirstOrchestrator {
    /// Create a new work-first orchestrator
    pub fn new() -> Self {
        Self {
            core_monitor: CoreMonitor::new(),
            ready_queue: VecDeque::new(),
            pending_queue: Vec::new(),
            bzl_load_queue: Vec::new(),
            loaded_bzl_files: HashMap::new(),
            min_utilization_for_bzl_load: 0.8,
        }
    }

    /// Create orchestrator with custom core count (for testing)
    pub fn with_cores(num_cores: usize) -> Self {
        Self {
            core_monitor: CoreMonitor::with_cores(num_cores),
            ready_queue: VecDeque::new(),
            pending_queue: Vec::new(),
            bzl_load_queue: Vec::new(),
            loaded_bzl_files: HashMap::new(),
            min_utilization_for_bzl_load: 0.8,
        }
    }

    /// Set minimum core utilization threshold for .bzl loading
    ///
    /// Lower values = more aggressive .bzl loading (may waste memory)
    /// Higher values = more conservative loading (may leave cores idle)
    ///
    /// Default: 0.8 (80%)
    pub fn set_min_utilization(&mut self, min: f64) {
        self.min_utilization_for_bzl_load = min.clamp(0.0, 1.0);
    }

    /// Initialize orchestrator with lazy build analysis
    ///
    /// This queues all immediate work and notes which .bzl files might be needed.
    pub fn initialize(&mut self, analysis: LazyBuildAnalysis) {
        // Queue all immediate actions (source files we can compile right now)
        for action in analysis.immediate_actions {
            self.ready_queue.push_back(action);
        }

        // Queue .bzl files that might be needed
        for (bzl_path, symbols) in analysis.required_bzl_symbols {
            self.bzl_load_queue.push(PendingBzlLoad {
                path: bzl_path,
                symbols,
                priority: LoadPriority::Prefetch,
            });
        }

        // Sort by priority (Required first)
        self.bzl_load_queue
            .sort_by_key(|load| std::cmp::Reverse(load.priority));
    }

    /// Check if we should load more .bzl files
    ///
    /// Returns true if:
    /// - Core utilization is below threshold (cores are idle)
    /// - There are .bzl files waiting to be loaded
    pub fn should_load_bzl(&self) -> bool {
        if self.bzl_load_queue.is_empty() {
            return false;
        }

        // Always load Required priority immediately
        if let Some(next) = self.bzl_load_queue.first() {
            if next.priority == LoadPriority::Required {
                return true;
            }
        }

        // For Prefetch/Background: only load if cores are idle
        self.core_monitor.utilization() < self.min_utilization_for_bzl_load
    }

    /// Get the next .bzl file to load (if any)
    pub fn next_bzl_to_load(&mut self) -> Option<PendingBzlLoad> {
        if self.should_load_bzl() {
            self.bzl_load_queue.pop()
        } else {
            None
        }
    }

    /// Mark a .bzl file as loaded
    pub fn mark_bzl_loaded(&mut self, path: PathBuf, symbols: Vec<String>) {
        self.loaded_bzl_files.insert(path, symbols);
    }

    /// Check if a .bzl file has already been loaded
    pub fn is_bzl_loaded(&self, path: &PathBuf) -> bool {
        self.loaded_bzl_files.contains_key(path)
    }

    /// Get the next ready action to execute
    ///
    /// Returns None if:
    /// - No actions are ready
    /// - All cores are busy
    pub fn next_ready_action(&mut self) -> Option<ImmediateAction> {
        if !self.core_monitor.has_capacity() {
            return None;
        }

        self.ready_queue.pop_front()
    }

    /// Queue a new action as ready
    pub fn queue_action(&mut self, action: ImmediateAction) {
        self.ready_queue.push_back(action);
    }

    /// Get current core utilization statistics
    pub fn core_stats(&self) -> CoreStats {
        CoreStats {
            total_cores: self.core_monitor.num_cores(),
            busy_cores: self.core_monitor.busy_cores(),
            utilization: self.core_monitor.utilization(),
        }
    }

    /// Get orchestrator statistics
    pub fn stats(&self) -> OrchestratorStats {
        OrchestratorStats {
            ready_actions: self.ready_queue.len(),
            pending_actions: self.pending_queue.len(),
            pending_bzl_loads: self.bzl_load_queue.len(),
            loaded_bzl_files: self.loaded_bzl_files.len(),
            core_stats: self.core_stats(),
        }
    }

    /// Get a reference to the core monitor
    pub fn core_monitor(&self) -> &CoreMonitor {
        &self.core_monitor
    }
}

impl Default for WorkFirstOrchestrator {
    fn default() -> Self {
        Self::new()
    }
}

/// Core utilization statistics
#[derive(Debug, Clone, Copy)]
pub struct CoreStats {
    pub total_cores: usize,
    pub busy_cores: usize,
    pub utilization: f64,
}

/// Orchestrator statistics
#[derive(Debug, Clone)]
pub struct OrchestratorStats {
    pub ready_actions: usize,
    pub pending_actions: usize,
    pub pending_bzl_loads: usize,
    pub loaded_bzl_files: usize,
    pub core_stats: CoreStats,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_core_monitor_basic() {
        let monitor = CoreMonitor::with_cores(4);

        assert_eq!(monitor.num_cores(), 4);
        assert_eq!(monitor.busy_cores(), 0);
        assert_eq!(monitor.utilization(), 0.0);

        monitor.mark_busy();
        assert_eq!(monitor.busy_cores(), 1);
        assert_eq!(monitor.utilization(), 0.25);

        monitor.mark_busy();
        monitor.mark_busy();
        assert_eq!(monitor.busy_cores(), 3);
        assert_eq!(monitor.utilization(), 0.75);

        monitor.mark_idle();
        assert_eq!(monitor.busy_cores(), 2);
        assert_eq!(monitor.utilization(), 0.5);
    }

    #[test]
    fn test_orchestrator_bzl_loading_threshold() {
        let mut orch = WorkFirstOrchestrator::with_cores(4);
        orch.set_min_utilization(0.8);

        // Queue a .bzl file to load
        orch.bzl_load_queue.push(PendingBzlLoad {
            path: PathBuf::from("test.bzl"),
            symbols: vec!["foo".to_string()],
            priority: LoadPriority::Prefetch,
        });

        // With 0 cores busy (0% utilization), should load
        assert!(orch.should_load_bzl());

        // Mark 2 cores busy (50% utilization), still should load
        orch.core_monitor.mark_busy();
        orch.core_monitor.mark_busy();
        assert!(orch.should_load_bzl());

        // Mark 1 more busy (75% utilization), still should load
        orch.core_monitor.mark_busy();
        assert!(orch.should_load_bzl());

        // Mark 1 more busy (100% utilization), should NOT load
        orch.core_monitor.mark_busy();
        assert!(!orch.should_load_bzl());
    }

    #[test]
    fn test_orchestrator_required_priority_always_loads() {
        let mut orch = WorkFirstOrchestrator::with_cores(4);

        // Queue a REQUIRED .bzl file
        orch.bzl_load_queue.push(PendingBzlLoad {
            path: PathBuf::from("required.bzl"),
            symbols: vec!["important".to_string()],
            priority: LoadPriority::Required,
        });

        // Even with all cores busy, should load Required priority
        for _ in 0..4 {
            orch.core_monitor.mark_busy();
        }
        assert_eq!(orch.core_monitor.utilization(), 1.0);
        assert!(orch.should_load_bzl());
    }

    #[test]
    fn test_action_queueing() {
        let mut orch = WorkFirstOrchestrator::with_cores(2);

        let action1 = ImmediateAction {
            action_type: crate::lazy_evaluator::ActionType::Compile,
            inputs: vec![PathBuf::from("a.cc")],
            output: PathBuf::from("a.o"),
            options: HashMap::new(),
        };

        let action2 = ImmediateAction {
            action_type: crate::lazy_evaluator::ActionType::Compile,
            inputs: vec![PathBuf::from("b.cc")],
            output: PathBuf::from("b.o"),
            options: HashMap::new(),
        };

        orch.queue_action(action1.clone());
        orch.queue_action(action2.clone());

        assert_eq!(orch.ready_queue.len(), 2);

        // Can pop first action
        let got = orch.next_ready_action();
        assert!(got.is_some());
        assert_eq!(got.unwrap().inputs[0], PathBuf::from("a.cc"));

        // Mark core busy
        orch.core_monitor.mark_busy();

        // Can still pop second action (1 core available)
        let got = orch.next_ready_action();
        assert!(got.is_some());

        // Mark second core busy
        orch.core_monitor.mark_busy();

        // Now no capacity
        assert!(orch.next_ready_action().is_none());
    }
}
