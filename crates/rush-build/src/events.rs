// Rush Build Event Stream
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Build event stream for monitoring build progress.
//!
//! Emits structured events during builds for integration with CI/CD systems.

use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Build event types.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BuildEvent {
    /// Build started.
    BuildStarted {
        /// Timestamp in milliseconds since epoch.
        timestamp: u64,
        /// Build ID.
        build_id: String,
        /// Targets to build.
        targets: Vec<String>,
    },

    /// Target started building.
    TargetStarted {
        /// Timestamp.
        timestamp: u64,
        /// Target label.
        target: String,
        /// Rule type.
        rule_type: String,
    },

    /// Target finished building.
    TargetCompleted {
        /// Timestamp.
        timestamp: u64,
        /// Target label.
        target: String,
        /// Success status.
        success: bool,
        /// Duration in milliseconds.
        duration_ms: u64,
    },

    /// Action started executing.
    ActionStarted {
        /// Timestamp.
        timestamp: u64,
        /// Action description.
        description: String,
        /// Target this action belongs to.
        target: String,
    },

    /// Action completed.
    ActionCompleted {
        /// Timestamp.
        timestamp: u64,
        /// Action description.
        description: String,
        /// Success status.
        success: bool,
        /// Whether this was a cache hit.
        cache_hit: bool,
        /// Duration in milliseconds.
        duration_ms: u64,
    },

    /// Test started running.
    TestStarted {
        /// Timestamp.
        timestamp: u64,
        /// Test label.
        test: String,
    },

    /// Test completed.
    TestCompleted {
        /// Timestamp.
        timestamp: u64,
        /// Test label.
        test: String,
        /// Pass/fail status.
        passed: bool,
        /// Duration in milliseconds.
        duration_ms: u64,
        /// Exit code.
        exit_code: i32,
    },

    /// Build finished.
    BuildFinished {
        /// Timestamp.
        timestamp: u64,
        /// Overall success status.
        success: bool,
        /// Total duration in milliseconds.
        duration_ms: u64,
        /// Number of targets built.
        targets_built: usize,
        /// Number of targets failed.
        targets_failed: usize,
    },

    /// Build progress update.
    Progress {
        /// Timestamp.
        timestamp: u64,
        /// Number of actions completed.
        completed: usize,
        /// Total number of actions.
        total: usize,
        /// Cache hit rate (0-100).
        cache_hit_rate: f64,
    },

    /// General message/log event.
    Message {
        /// Timestamp.
        timestamp: u64,
        /// Log level: info, warning, error.
        level: String,
        /// Message text.
        message: String,
    },
}

/// Build event stream emitter.
#[derive(Debug, Clone)]
pub struct BuildEventStream {
    /// File writer for events (if enabled).
    writer: Arc<Mutex<Option<BufWriter<File>>>>,

    /// Whether the stream is enabled.
    enabled: bool,

    /// If true, flush after each event write to reduce data loss on crashes.
    /// Controlled by env var `RUSH_EVENT_AUTOFLUSH` ("1", "true").
    autoflush: bool,
}

impl BuildEventStream {
    /// Create a new build event stream.
    pub fn new(enabled: bool) -> Self {
        BuildEventStream {
            writer: Arc::new(Mutex::new(None)),
            enabled,
            autoflush: std::env::var("RUSH_EVENT_AUTOFLUSH")
                .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "True"))
                .unwrap_or(false),
        }
    }

    /// Create a build event stream writing to a file.
    pub fn to_file(path: &Path) -> Result<Self, std::io::Error> {
        let file = File::create(path)?;
        let writer = BufWriter::new(file);

        Ok(BuildEventStream {
            writer: Arc::new(Mutex::new(Some(writer))),
            enabled: true,
            autoflush: std::env::var("RUSH_EVENT_AUTOFLUSH")
                .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "True"))
                .unwrap_or(false),
        })
    }

    /// Emit a build event.
    pub fn emit(&self, event: BuildEvent) {
        if !self.enabled {
            return;
        }

        let json = match serde_json::to_string(&event) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("Failed to serialize build event: {}", e);
                return;
            }
        };

        if let Ok(mut writer_guard) = self.writer.lock() {
            if let Some(writer) = writer_guard.as_mut() {
                if let Err(e) = writeln!(writer, "{}", json) {
                    eprintln!("Failed to write build event: {}", e);
                } else if self.autoflush {
                    // Best-effort flush to persist events even if process dies
                    let _ = writer.flush();
                }
            } else {
                // No file writer, print to stdout
                println!("BUILD_EVENT: {}", json);
                // Can't flush stdout here reliably; recommend piping to tee
            }
        }
    }

    /// Flush any buffered events.
    pub fn flush(&self) {
        if let Ok(mut writer_guard) = self.writer.lock() {
            if let Some(writer) = writer_guard.as_mut() {
                let _ = writer.flush();
            }
        }
    }

    /// Get current timestamp in milliseconds since epoch.
    pub fn timestamp() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    }

    /// Emit a build started event.
    pub fn build_started(&self, build_id: String, targets: Vec<String>) {
        self.emit(BuildEvent::BuildStarted {
            timestamp: Self::timestamp(),
            build_id,
            targets,
        });
    }

    /// Emit a target started event.
    pub fn target_started(&self, target: String, rule_type: String) {
        self.emit(BuildEvent::TargetStarted {
            timestamp: Self::timestamp(),
            target,
            rule_type,
        });
    }

    /// Emit a target completed event.
    pub fn target_completed(&self, target: String, success: bool, duration_ms: u64) {
        self.emit(BuildEvent::TargetCompleted {
            timestamp: Self::timestamp(),
            target,
            success,
            duration_ms,
        });
    }

    /// Emit a build finished event.
    pub fn build_finished(
        &self,
        success: bool,
        duration_ms: u64,
        targets_built: usize,
        targets_failed: usize,
    ) {
        self.emit(BuildEvent::BuildFinished {
            timestamp: Self::timestamp(),
            success,
            duration_ms,
            targets_built,
            targets_failed,
        });
    }

    /// Emit a progress update.
    pub fn progress(&self, completed: usize, total: usize, cache_hit_rate: f64) {
        self.emit(BuildEvent::Progress {
            timestamp: Self::timestamp(),
            completed,
            total,
            cache_hit_rate,
        });
    }

    /// Emit a message.
    pub fn message(&self, level: &str, message: String) {
        self.emit(BuildEvent::Message {
            timestamp: Self::timestamp(),
            level: level.to_string(),
            message,
        });
    }
}

impl Default for BuildEventStream {
    fn default() -> Self {
        Self::new(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_event_stream_disabled() {
        let stream = BuildEventStream::new(false);
        stream.build_started("test_id".to_string(), vec!["//pkg:target".to_string()]);
        // Should not panic
    }

    #[test]
    fn test_event_stream_to_file() -> Result<(), Box<dyn std::error::Error>> {
        let temp_file = NamedTempFile::new()?;
        let stream = BuildEventStream::to_file(temp_file.path())?;

        stream.build_started("test_id".to_string(), vec!["//pkg:target".to_string()]);
        stream.flush();

        // Verify file has content
        let contents = std::fs::read_to_string(temp_file.path())?;
        assert!(!contents.is_empty());
        assert!(contents.contains("build_started"));

        Ok(())
    }
}
