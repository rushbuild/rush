// Rush Build - Cross-process build coordination
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Build lock for cross-process PSI coordination.
//!
//! Prevents multiple Rush processes from building simultaneously, which can
//! cause system-wide memory thrashing. Each process monitors PSI independently,
//! but only one build can run at a time.

use crate::cache_paths::workspace_cache_root;
use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info, warn};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

/// Build lock to prevent concurrent builds across processes
pub struct BuildLock {
    lock_file: PathBuf,
    lock_handle: Option<File>,
    pid: u32,
}

impl BuildLock {
    /// Create a new build lock for the given workspace
    pub fn new(workspace_root: &Path) -> Result<Self> {
        let lock_dir = if let Ok(override_dir) = std::env::var("RUSH_LOCK_DIR") {
            PathBuf::from(override_dir)
        } else {
            workspace_cache_root(workspace_root)
        };
        std::fs::create_dir_all(&lock_dir).context("Failed to create lock directory")?;

        let lock_file = lock_dir.join(".build.lock");
        let pid = std::process::id();

        Ok(Self {
            lock_file,
            lock_handle: None,
            pid,
        })
    }

    /// Try to acquire the build lock
    ///
    /// Returns Ok(true) if lock acquired, Ok(false) if another build is running.
    /// Stale locks (>1 hour old) are automatically cleaned up.
    pub fn try_acquire(&mut self) -> Result<bool> {
        // Check if lock file exists and is stale
        if self.lock_file.exists() {
            if let Ok(mut file) = File::open(&self.lock_file) {
                let mut contents = String::new();
                if file.read_to_string(&mut contents).is_ok() {
                    if let Some((lock_pid, lock_time)) = Self::parse_lock_contents(&contents) {
                        // Check if lock is stale (>1 hour old)
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs();

                        if now - lock_time > 3600 {
                            warn!(
                                "Removing stale build lock (PID {}, {} seconds old)",
                                lock_pid,
                                now - lock_time
                            );
                            let _ = std::fs::remove_file(&self.lock_file);
                        } else {
                            // Check if process still exists (Unix only)
                            #[cfg(unix)]
                            {
                                use std::process::Command;
                                if let Ok(output) = Command::new("ps")
                                    .arg("-p")
                                    .arg(lock_pid.to_string())
                                    .output()
                                {
                                    if !output.status.success() {
                                        warn!("Removing lock from dead process (PID {})", lock_pid);
                                        let _ = std::fs::remove_file(&self.lock_file);
                                    } else {
                                        debug!(
                                            "Build lock held by active process (PID {})",
                                            lock_pid
                                        );
                                        return Ok(false);
                                    }
                                } else {
                                    // Can't check process, assume lock is valid
                                    return Ok(false);
                                }
                            }

                            #[cfg(not(unix))]
                            {
                                // On non-Unix, trust the lock file
                                return Ok(false);
                            }
                        }
                    }
                }
            }
        }

        // Try to create lock file exclusively
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);

        #[cfg(unix)]
        options.mode(0o644);

        match options.open(&self.lock_file) {
            Ok(mut file) => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();

                let lock_contents = format!("{}:{}", self.pid, now);
                file.write_all(lock_contents.as_bytes())
                    .context("Failed to write lock file")?;

                info!("✓ Acquired build lock (PID {})", self.pid);
                self.lock_handle = Some(file);
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Another process beat us to it
                debug!("Build lock already held by another process");
                Ok(false)
            }
            Err(e) => Err(e).context("Failed to create lock file"),
        }
    }

    /// Wait to acquire the build lock
    ///
    /// Waits up to max_wait_secs for the lock to become available.
    /// Returns Ok(true) if acquired, Ok(false) if timeout.
    pub fn wait_acquire(&mut self, max_wait_secs: u64) -> Result<bool> {
        use std::thread;
        use std::time::Duration;

        let start = std::time::Instant::now();
        let mut logged_waiting = false;

        loop {
            if self.try_acquire()? {
                return Ok(true);
            }

            if start.elapsed().as_secs() >= max_wait_secs {
                return Ok(false);
            }

            if !logged_waiting {
                info!("⏳ Waiting for build lock (another Rush process is building)...");
                logged_waiting = true;
            }

            thread::sleep(Duration::from_secs(1));
        }
    }

    /// Release the build lock
    pub fn release(&mut self) -> Result<()> {
        if self.lock_handle.is_some() {
            drop(self.lock_handle.take());
            if self.lock_file.exists() {
                std::fs::remove_file(&self.lock_file).context("Failed to remove lock file")?;
            }
            debug!("Released build lock (PID {})", self.pid);
        }
        Ok(())
    }

    /// Parse lock file contents (format: "pid:timestamp")
    fn parse_lock_contents(contents: &str) -> Option<(u32, u64)> {
        let parts: Vec<&str> = contents.trim().split(':').collect();
        if parts.len() == 2 {
            let pid = parts[0].parse::<u32>().ok()?;
            let time = parts[1].parse::<u64>().ok()?;
            Some((pid, time))
        } else {
            None
        }
    }
}

impl Drop for BuildLock {
    fn drop(&mut self) {
        if let Err(e) = self.release() {
            warn!("Failed to release build lock: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_acquire_release() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();

        let mut lock1 = BuildLock::new(workspace).unwrap();
        assert!(lock1.try_acquire().unwrap());

        // Second lock should fail
        let mut lock2 = BuildLock::new(workspace).unwrap();
        assert!(!lock2.try_acquire().unwrap());

        // Release first lock
        lock1.release().unwrap();

        // Now second lock should succeed
        assert!(lock2.try_acquire().unwrap());
    }

    #[test]
    fn test_stale_lock_cleanup() {
        let temp_dir = TempDir::new().unwrap();
        let workspace = temp_dir.path();

        let lock_dir = workspace.join(".cache").join("rush");
        std::fs::create_dir_all(&lock_dir).unwrap();
        let lock_file = lock_dir.join(".build.lock");

        // Create a stale lock (1 hour old)
        let one_hour_ago = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 3601;
        std::fs::write(&lock_file, format!("99999:{}", one_hour_ago)).unwrap();

        // Should clean up stale lock and acquire
        let mut lock = BuildLock::new(workspace).unwrap();
        assert!(lock.try_acquire().unwrap());
    }
}
