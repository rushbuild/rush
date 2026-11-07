// Rush Build - Watch Mode
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Watch mode for continuous builds on file changes.

use anyhow::Result;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use owo_colors::OwoColorize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

/// File watcher for continuous builds.
pub struct FileWatcher {
    /// Root directory to watch.
    workspace_root: PathBuf,

    /// Targets to rebuild when files change.
    targets: Vec<String>,

    /// File patterns to watch (*.cc, *.rs, BUILD, etc.).
    watch_patterns: Vec<String>,

    /// Debounce duration (group rapid changes).
    debounce_duration: Duration,

    /// Files to ignore.
    ignore_patterns: Vec<String>,
}

impl FileWatcher {
    /// Create a new file watcher.
    pub fn new(workspace_root: PathBuf, targets: Vec<String>) -> Self {
        FileWatcher {
            workspace_root,
            targets,
            watch_patterns: vec![
                "*.cc".to_string(),
                "*.cpp".to_string(),
                "*.h".to_string(),
                "*.hpp".to_string(),
                "*.rs".to_string(),
                "*.py".to_string(),
                "BUILD".to_string(),
                "BUILD.bazel".to_string(),
                "Cargo.toml".to_string(),
            ],
            debounce_duration: Duration::from_millis(500),
            ignore_patterns: vec![
                "rush-out".to_string(),
                "target".to_string(),
                ".git".to_string(),
                "*.o".to_string(),
                "*.rlib".to_string(),
                "*.pyc".to_string(),
            ],
        }
    }

    /// Start watching for file changes.
    pub fn watch<F>(&self, on_change: F) -> Result<()>
    where
        F: FnMut(&[PathBuf]) -> Result<()>,
    {
        let (tx, rx) = channel();

        let mut watcher = RecommendedWatcher::new(
            move |res: Result<Event, notify::Error>| {
                if let Ok(event) = res {
                    let _ = tx.send(event);
                }
            },
            notify::Config::default(),
        )?;

        // Watch workspace root recursively
        watcher.watch(&self.workspace_root, RecursiveMode::Recursive)?;

        eprintln!("\n{}", "👁️  Watch mode started".bold().green());
        eprintln!(
            "   {} {}",
            "Watching:".bright_black(),
            self.workspace_root.display()
        );
        eprintln!(
            "   {} {}",
            "Targets:".bright_black(),
            self.targets.join(", ").cyan()
        );
        eprintln!("   {}\n", "Press Ctrl+C to stop...".bright_black());

        self.event_loop(rx, on_change)?;

        Ok(())
    }

    /// Event loop for processing file changes.
    fn event_loop<F>(&self, rx: Receiver<Event>, mut on_change: F) -> Result<()>
    where
        F: FnMut(&[PathBuf]) -> Result<()>,
    {
        let mut pending_changes: HashSet<PathBuf> = HashSet::new();
        let mut last_trigger = Instant::now();

        loop {
            // Collect events with debouncing
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(event) => {
                    if let EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_) =
                        event.kind
                    {
                        for path in event.paths {
                            if self.should_watch(&path) {
                                pending_changes.insert(path);
                            }
                        }
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    // Check if we should trigger a rebuild
                    if !pending_changes.is_empty()
                        && last_trigger.elapsed() >= self.debounce_duration
                    {
                        let changed_files: Vec<PathBuf> = pending_changes.drain().collect();

                        eprintln!("\n{}", "📝 Files changed:".bold().yellow());
                        for file in &changed_files {
                            if let Ok(rel_path) = file.strip_prefix(&self.workspace_root) {
                                eprintln!(
                                    "   {} {}",
                                    "•".yellow(),
                                    rel_path.display().to_string().bright_white()
                                );
                            }
                        }

                        eprintln!("\n{}", "🔨 Rebuilding...".bold().cyan());
                        let rebuild_start = Instant::now();

                        match on_change(&changed_files) {
                            Ok(()) => {
                                let duration = rebuild_start.elapsed();
                                eprintln!(
                                    "\n{} {}\n",
                                    "✅ Build succeeded in".green().bold(),
                                    format!("{:.2}s", duration.as_secs_f64()).green().bold()
                                );
                            }
                            Err(e) => {
                                eprintln!(
                                    "\n{} {}\n",
                                    "❌ Build failed:".red().bold(),
                                    e.to_string().red()
                                );
                            }
                        }

                        last_trigger = Instant::now();
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    break;
                }
            }
        }

        Ok(())
    }

    /// Check if a file should be watched.
    fn should_watch(&self, path: &Path) -> bool {
        // Check ignore patterns
        for ignore in &self.ignore_patterns {
            if let Some(name) = path.file_name() {
                if let Some(name_str) = name.to_str() {
                    if ignore.contains('*') {
                        // Simple glob matching
                        let pattern = ignore.trim_start_matches('*');
                        if name_str.ends_with(pattern) {
                            return false;
                        }
                    } else if name_str == ignore {
                        return false;
                    }
                }
            }

            // Check path components
            if path.components().any(|c| c.as_os_str() == ignore.as_str()) {
                return false;
            }
        }

        // Check watch patterns
        if let Some(name) = path.file_name() {
            if let Some(name_str) = name.to_str() {
                for pattern in &self.watch_patterns {
                    if pattern.contains('*') {
                        let ext = pattern.trim_start_matches('*');
                        if name_str.ends_with(ext) {
                            return true;
                        }
                    } else if name_str == pattern {
                        return true;
                    }
                }
            }
        }

        false
    }

    /// Get affected targets based on changed files.
    pub fn get_affected_targets(&self, changed_files: &[PathBuf]) -> Vec<String> {
        // Map files to their packages
        let mut affected_packages = HashSet::new();

        for file in changed_files {
            if let Ok(rel_path) = file.strip_prefix(&self.workspace_root) {
                // Extract package from path
                if let Some(parent) = rel_path.parent() {
                    affected_packages.insert(parent.to_string_lossy().to_string());
                }
            }
        }

        // If BUILD files changed, rebuild all targets in that package
        let has_build_changes = changed_files.iter().any(|f| {
            f.file_name()
                .and_then(|n| n.to_str())
                .map(|s| s == "BUILD" || s == "BUILD.bazel")
                .unwrap_or(false)
        });

        if has_build_changes || affected_packages.is_empty() {
            // Rebuild all specified targets
            self.targets.clone()
        } else {
            // Filter targets to affected packages
            self.targets
                .iter()
                .filter(|target| affected_packages.iter().any(|pkg| target.contains(pkg)))
                .cloned()
                .collect()
        }
    }

    /// Add a file pattern to watch.
    pub fn add_pattern(&mut self, pattern: String) {
        self.watch_patterns.push(pattern);
    }

    /// Add an ignore pattern.
    pub fn add_ignore(&mut self, pattern: String) {
        self.ignore_patterns.push(pattern);
    }

    /// Set debounce duration.
    pub fn set_debounce(&mut self, duration: Duration) {
        self.debounce_duration = duration;
    }
}

/// Change summary for displaying to user.
#[derive(Debug)]
pub struct ChangeSummary {
    /// Files added.
    pub added: Vec<PathBuf>,
    /// Files modified.
    pub modified: Vec<PathBuf>,
    /// Files removed.
    pub removed: Vec<PathBuf>,
}

impl ChangeSummary {
    /// Create from a list of changed files.
    pub fn from_paths(paths: &[PathBuf]) -> Self {
        ChangeSummary {
            added: Vec::new(),
            modified: paths.to_vec(),
            removed: Vec::new(),
        }
    }

    /// Print a summary of changes.
    pub fn print(&self) {
        if !self.added.is_empty() {
            eprintln!("   {} {} files", "Added:".green(), self.added.len());
        }
        if !self.modified.is_empty() {
            eprintln!("   {} {} files", "Modified:".yellow(), self.modified.len());
        }
        if !self.removed.is_empty() {
            eprintln!("   {} {} files", "Removed:".red(), self.removed.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_watch() {
        let watcher = FileWatcher::new(
            PathBuf::from("/workspace"),
            vec!["//pkg:target".to_string()],
        );

        // Should watch source files
        assert!(watcher.should_watch(Path::new("src/main.rs")));
        assert!(watcher.should_watch(Path::new("src/lib.cc")));
        assert!(watcher.should_watch(Path::new("BUILD")));

        // Should ignore build outputs
        assert!(!watcher.should_watch(Path::new("rush-out/main.o")));
        assert!(!watcher.should_watch(Path::new("target/debug/lib.rlib")));
        assert!(!watcher.should_watch(Path::new(".git/config")));
    }

    #[test]
    fn test_get_affected_targets() {
        let watcher = FileWatcher::new(
            PathBuf::from("/workspace"),
            vec!["//pkg1:target1".to_string(), "//pkg2:target2".to_string()],
        );

        let changed = vec![PathBuf::from("/workspace/pkg1/main.rs")];
        let affected = watcher.get_affected_targets(&changed);

        // Should include targets from pkg1
        assert!(affected.iter().any(|t| t.contains("pkg1")));
    }
}
