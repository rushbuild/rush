// Rush Build - Action Batching
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Action batching for reducing overhead from small tasks.
//!
//! Combines multiple small compilation units or actions into batched
//! executions, dramatically reducing process spawn overhead and improving
//! throughput for large numbers of small files.

use anyhow::Result;
use rush_action::Action;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Action batching strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchStrategy {
    /// Batch by compilation command.
    ByCommand,

    /// Batch by target directory.
    ByDirectory,

    /// Batch by file size (small files together).
    BySize,

    /// Adaptive batching based on system load.
    Adaptive,
}

/// Action batcher for combining small tasks.
pub struct ActionBatcher {
    /// Batching strategy.
    strategy: BatchStrategy,

    /// Maximum batch size (number of actions).
    max_batch_size: usize,

    /// Maximum batch duration before forcing execution.
    max_batch_duration: Duration,

    /// Pending batches.
    pending: HashMap<String, Batch>,

    /// Statistics.
    stats: BatchStats,
}

/// A batch of actions.
struct Batch {
    /// Actions in this batch.
    actions: Vec<Action>,

    /// Time when batch was created.
    created_at: Instant,

    /// Estimated total duration.
    estimated_duration_ms: u64,
}

/// Batching statistics.
#[derive(Debug, Clone, Default)]
pub struct BatchStats {
    /// Total actions processed.
    pub total_actions: usize,

    /// Number of batches created.
    pub batches_created: usize,

    /// Actions executed individually (too large to batch).
    pub unbatched_actions: usize,

    /// Average batch size.
    pub avg_batch_size: f64,

    /// Time saved by batching (estimated).
    pub time_saved_ms: u64,
}

impl ActionBatcher {
    /// Create a new action batcher.
    pub fn new(strategy: BatchStrategy) -> Self {
        ActionBatcher {
            strategy,
            max_batch_size: 50,
            max_batch_duration: Duration::from_millis(100),
            pending: HashMap::new(),
            stats: BatchStats::default(),
        }
    }

    /// Set maximum batch size.
    pub fn with_max_batch_size(mut self, size: usize) -> Self {
        self.max_batch_size = size;
        self
    }

    /// Set maximum batch duration.
    pub fn with_max_batch_duration(mut self, duration: Duration) -> Self {
        self.max_batch_duration = duration;
        self
    }

    /// Add an action to be batched.
    pub fn add(&mut self, action: Action) -> Option<Vec<Action>> {
        self.stats.total_actions += 1;

        // Determine batch key based on strategy
        let batch_key = match self.strategy {
            BatchStrategy::ByCommand => self.get_command_key(&action),
            BatchStrategy::ByDirectory => self.get_directory_key(&action),
            BatchStrategy::BySize => self.get_size_key(&action),
            BatchStrategy::Adaptive => self.get_adaptive_key(&action),
        };

        // Get or create batch
        let batch = self
            .pending
            .entry(batch_key.clone())
            .or_insert_with(|| Batch {
                actions: Vec::new(),
                created_at: Instant::now(),
                estimated_duration_ms: 0,
            });

        batch.actions.push(action);
        batch.estimated_duration_ms += 100; // Rough estimate

        // Check if batch should be flushed
        if batch.actions.len() >= self.max_batch_size
            || batch.created_at.elapsed() >= self.max_batch_duration
        {
            return Some(self.flush_batch(&batch_key));
        }

        None
    }

    /// Flush a specific batch.
    fn flush_batch(&mut self, key: &str) -> Vec<Action> {
        if let Some(batch) = self.pending.remove(key) {
            self.stats.batches_created += 1;

            let batch_size = batch.actions.len();
            if batch_size > 1 {
                // Estimate time saved: avoid (n-1) process spawns
                // Assume each spawn costs ~10ms
                self.stats.time_saved_ms += (batch_size as u64 - 1) * 10;
            }

            // Update average batch size
            let total_batched = self.stats.batches_created;
            self.stats.avg_batch_size = (self.stats.avg_batch_size * (total_batched - 1) as f64
                + batch_size as f64)
                / total_batched as f64;

            return batch.actions;
        }

        Vec::new()
    }

    /// Flush all pending batches.
    pub fn flush_all(&mut self) -> Vec<Vec<Action>> {
        let keys: Vec<_> = self.pending.keys().cloned().collect();
        keys.iter().map(|key| self.flush_batch(key)).collect()
    }

    /// Get batch key based on command.
    fn get_command_key(&self, action: &Action) -> String {
        format!("{}", action.command.executable.display())
    }

    /// Get batch key based on directory.
    fn get_directory_key(&self, action: &Action) -> String {
        if let Some(first_input) = action.inputs.first() {
            let path = match first_input {
                rush_action::Artifact::Source(p) => p,
                rush_action::Artifact::Derived { output_path, .. } => output_path,
            };
            if let Some(parent) = path.parent() {
                return parent.display().to_string();
            }
        }
        "unknown".to_string()
    }

    /// Get batch key based on size.
    fn get_size_key(&self, action: &Action) -> String {
        let total_size: u64 = action
            .inputs
            .iter()
            .filter_map(|a| {
                let path = match a {
                    rush_action::Artifact::Source(p) => p,
                    rush_action::Artifact::Derived { output_path, .. } => output_path,
                };
                std::fs::metadata(path).ok()
            })
            .map(|m| m.len())
            .sum();

        if total_size < 10_000 {
            "small".to_string()
        } else if total_size < 100_000 {
            "medium".to_string()
        } else {
            "large".to_string()
        }
    }

    /// Get batch key using adaptive strategy.
    fn get_adaptive_key(&self, action: &Action) -> String {
        // Combine directory and size for adaptive batching
        let dir_key = self.get_directory_key(action);
        let size_key = self.get_size_key(action);
        format!("{}:{}", dir_key, size_key)
    }

    /// Get statistics.
    pub fn stats(&self) -> &BatchStats {
        &self.stats
    }

    /// Get number of pending batches.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Get number of pending actions across all batches.
    pub fn pending_actions(&self) -> usize {
        self.pending.values().map(|b| b.actions.len()).sum()
    }
}

impl Default for ActionBatcher {
    fn default() -> Self {
        Self::new(BatchStrategy::Adaptive)
    }
}

/// Unity build generator for aggressive batching.
///
/// Combines multiple source files into a single translation unit.
pub struct UnityBuildGenerator {
    /// Maximum files per unity build.
    max_files_per_unity: usize,

    /// Unity builds created.
    unity_count: usize,
}

impl UnityBuildGenerator {
    /// Create a new unity build generator.
    pub fn new() -> Self {
        UnityBuildGenerator {
            max_files_per_unity: 20,
            unity_count: 0,
        }
    }

    /// Set maximum files per unity build.
    pub fn with_max_files(mut self, max: usize) -> Self {
        self.max_files_per_unity = max;
        self
    }

    /// Generate unity builds from source files.
    pub fn generate(&mut self, sources: &[PathBuf], output_dir: &Path) -> Result<Vec<PathBuf>> {
        let mut unity_files = Vec::new();

        for chunk in sources.chunks(self.max_files_per_unity) {
            let unity_file = output_dir.join(format!("unity_{}.cc", self.unity_count));

            // Generate unity file content
            let mut content = String::new();
            content.push_str("// Unity build - auto-generated\n");

            for source in chunk {
                content.push_str(&format!("#include \"{}\"\n", source.display()));
            }

            std::fs::write(&unity_file, content)?;

            unity_files.push(unity_file);
            self.unity_count += 1;
        }

        Ok(unity_files)
    }
}

impl Default for UnityBuildGenerator {
    fn default() -> Self {
        Self::new()
    }
}

/// Link-time optimization batcher.
///
/// Groups object files for efficient LTO.
pub struct LtoBatcher {
    /// Object files to link.
    objects: Vec<PathBuf>,

    /// Minimum objects for LTO to be beneficial.
    min_objects_for_lto: usize,
}

impl LtoBatcher {
    /// Create a new LTO batcher.
    pub fn new() -> Self {
        LtoBatcher {
            objects: Vec::new(),
            min_objects_for_lto: 10,
        }
    }

    /// Add object file.
    pub fn add_object(&mut self, obj: PathBuf) {
        self.objects.push(obj);
    }

    /// Check if LTO should be enabled.
    pub fn should_enable_lto(&self) -> bool {
        self.objects.len() >= self.min_objects_for_lto
    }

    /// Get LTO flags for linker.
    pub fn get_lto_flags(&self) -> Vec<String> {
        if !self.should_enable_lto() {
            return Vec::new();
        }

        vec![
            "-flto".to_string(),
            format!("-flto={}", num_cpus::get()), // Parallel LTO
            "-fuse-linker-plugin".to_string(),
        ]
    }
}

impl Default for LtoBatcher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rush_action::Command;

    fn create_test_action(id: &str, executable: &str) -> Action {
        use std::path::PathBuf;
        let inputs = vec![];
        let command = Command {
            working_dir: None,
            executable: PathBuf::from(executable),
            args: vec![],
            execution_info: rush_action::ExecutionInfo::default(),
        };
        let env = std::collections::HashMap::new();

        Action {
            id: Action::compute_id(&inputs, &command, &env).unwrap(),
            inputs,
            outputs: vec![],
            command,
            env,
            mnemonic: id.to_string(),
        }
    }

    #[test]
    fn test_action_batching() {
        let mut batcher = ActionBatcher::new(BatchStrategy::ByCommand);
        batcher = batcher.with_max_batch_size(3);

        // Add actions
        let _ = batcher.add(create_test_action("1", "g++"));
        let _ = batcher.add(create_test_action("2", "g++"));

        // Third action should trigger flush
        let batch = batcher.add(create_test_action("3", "g++"));

        assert!(batch.is_some());
        let actions = batch.unwrap();
        assert_eq!(actions.len(), 3);

        let stats = batcher.stats();
        assert_eq!(stats.total_actions, 3);
        assert_eq!(stats.batches_created, 1);
    }

    #[test]
    fn test_batch_strategies() {
        let mut batcher = ActionBatcher::new(BatchStrategy::ByCommand);

        // Different commands should create different batches
        let _ = batcher.add(create_test_action("1", "g++"));
        let _ = batcher.add(create_test_action("2", "rustc"));

        assert_eq!(batcher.pending_count(), 2);
    }

    #[test]
    fn test_unity_build() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let mut generator = UnityBuildGenerator::new().with_max_files(2);

        let sources = vec![
            PathBuf::from("a.cc"),
            PathBuf::from("b.cc"),
            PathBuf::from("c.cc"),
        ];

        let unity_files = generator.generate(&sources, temp_dir.path()).unwrap();

        assert_eq!(unity_files.len(), 2); // 2 files + 1 file = 2 unity builds
    }
}
