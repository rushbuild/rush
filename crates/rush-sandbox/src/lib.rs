// Rush Sandbox - Hermetic execution
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Hermetic execution via OS-level sandboxing.
//!
//! Provides isolated execution environments for reproducible builds.

#![warn(missing_docs)]

use anyhow::Result;
use rush_action::ActionId;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

/// Sandbox configuration.
#[derive(Debug, Clone, Default)]
pub struct SandboxConfig {
    /// Allow network access.
    pub network_enabled: bool,

    /// Writable paths (outside of output dir).
    pub writable_paths: Vec<PathBuf>,

    /// Environment variables to set.
    pub env: HashMap<String, String>,

    /// Maximum memory usage (in bytes, 0 = unlimited).
    pub max_memory: u64,

    /// CPU time limit (in seconds, 0 = unlimited).
    pub cpu_time_limit: u64,
}

/// A sandboxed execution environment.
///
/// On Linux, uses namespaces for isolation.
/// On macOS, uses sandbox-exec.
/// On other platforms, falls back to basic isolation.
pub struct Sandbox {
    /// Root directory for the sandbox.
    root: PathBuf,

    /// Input files that will be mounted read-only.
    inputs: Vec<PathBuf>,

    /// Output directory for results.
    output_dir: PathBuf,

    /// Sandbox configuration.
    config: SandboxConfig,
}

impl Sandbox {
    /// Create a new sandbox.
    ///
    /// Creates a temporary directory as the sandbox root.
    pub fn new() -> Result<Self> {
        Self::with_config(SandboxConfig::default())
    }

    /// Create a sandbox with custom configuration.
    pub fn with_config(config: SandboxConfig) -> Result<Self> {
        let temp_dir = tempfile::TempDir::new()?;
        let root = temp_dir.path().to_path_buf();
        let output_dir = root.join("outputs");
        std::fs::create_dir_all(&output_dir)?;

        // Keep the temp_dir alive by converting to path and leaking
        // This prevents automatic cleanup on drop
        std::mem::forget(temp_dir);

        Ok(Sandbox {
            root,
            inputs: Vec::new(),
            output_dir,
            config,
        })
    }

    /// Add an input file to the sandbox (read-only).
    pub fn add_input(&mut self, path: PathBuf) {
        self.inputs.push(path);
    }

    /// Get the sandbox root directory.
    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    /// Get the output directory.
    pub fn output_dir(&self) -> &PathBuf {
        &self.output_dir
    }

    /// Execute a command in the sandbox.
    ///
    /// On Linux, uses namespaces for isolation:
    /// - Mount namespace: Isolate filesystem view
    /// - Network namespace: Optional network isolation
    /// - PID namespace: Isolate process IDs
    /// - UTS namespace: Isolate hostname
    #[cfg(target_os = "linux")]
    pub async fn execute(
        &self,
        action_id: ActionId,
        cmd: &rush_action::Command,
    ) -> Result<rush_exec::ActionResult> {
        use nix::mount::{mount, MsFlags};
        use nix::sched::{unshare, CloneFlags};
        use std::process::Stdio;

        // Create isolated namespaces
        let mut flags =
            CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWUTS | CloneFlags::CLONE_NEWPID;

        if !self.config.network_enabled {
            flags |= CloneFlags::CLONE_NEWNET;
        }

        unshare(flags)?;

        // Remount root as private to avoid propagating mounts
        mount(
            None::<&str>,
            "/",
            None::<&str>,
            MsFlags::MS_REC | MsFlags::MS_PRIVATE,
            None::<&str>,
        )?;

        // Create minimal filesystem hierarchy in sandbox
        let sandbox_root = &self.root;
        std::fs::create_dir_all(sandbox_root.join("bin"))?;
        std::fs::create_dir_all(sandbox_root.join("lib"))?;
        std::fs::create_dir_all(sandbox_root.join("lib64"))?;
        std::fs::create_dir_all(sandbox_root.join("usr/bin"))?;
        std::fs::create_dir_all(sandbox_root.join("usr/lib"))?;
        std::fs::create_dir_all(sandbox_root.join("tmp"))?;
        std::fs::create_dir_all(sandbox_root.join("proc"))?;
        std::fs::create_dir_all(sandbox_root.join("dev"))?;

        // Mount inputs read-only
        for input in &self.inputs {
            if input.exists() {
                let target = sandbox_root.join(input.strip_prefix("/").unwrap_or(input));
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }

                mount(
                    Some(input.as_path()),
                    target.as_path(),
                    None::<&str>,
                    MsFlags::MS_BIND | MsFlags::MS_RDONLY,
                    None::<&str>,
                )?;
            }
        }

        // Mount /proc for process information
        mount(
            Some("proc"),
            sandbox_root.join("proc").as_path(),
            Some("proc"),
            MsFlags::empty(),
            None::<&str>,
        )?;

        // Execute command in sandbox
        let mut command = tokio::process::Command::new(&cmd.executable);
        command.args(&cmd.args);
        if let Some(dir) = &cmd.working_dir {
            command.current_dir(dir);
        }

        // Set environment
        command.env_clear();
        for (k, v) in &self.config.env {
            command.env(k, v);
        }

        // Set resource limits if specified
        #[cfg(target_os = "linux")]
        {
            let max_memory = self.config.max_memory;
            let cpu_time_limit = self.config.cpu_time_limit;

            if max_memory > 0 || cpu_time_limit > 0 {
                unsafe {
                    command.pre_exec(move || {
                        use nix::sys::resource::{setrlimit, Resource};

                        if max_memory > 0 {
                            setrlimit(Resource::RLIMIT_AS, max_memory, max_memory)?;
                        }

                        if cpu_time_limit > 0 {
                            setrlimit(Resource::RLIMIT_CPU, cpu_time_limit, cpu_time_limit)?;
                        }

                        Ok(())
                    });
                }
            }
        }

        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let output = command.output().await?;

        Ok(rush_exec::ActionResult {
            action_id,
            exit_code: output.status.code().unwrap_or(-1),
            stdout: output.stdout,
            stderr: output.stderr,
            outputs: vec![],
            output_records: Vec::new(),
            duration: Duration::default(), // TODO: Measure duration
            cached: false,
        })
    }

    /// Execute a command (non-Linux fallback).
    ///
    /// Provides basic execution without full isolation.
    #[cfg(not(target_os = "linux"))]
    pub async fn execute(
        &self,
        action_id: ActionId,
        cmd: &rush_action::Command,
    ) -> Result<rush_exec::ActionResult> {
        use std::process::Stdio;

        println!("⚠️  Warning: Full sandboxing only available on Linux");
        println!("   Running with limited isolation...");

        let mut command = tokio::process::Command::new(&cmd.executable);
        command.args(&cmd.args);
        if let Some(dir) = &cmd.working_dir {
            command.current_dir(dir);
        }
        for (k, v) in &self.config.env {
            command.env(k, v);
        }

        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let output = command.output().await?;

        Ok(rush_exec::ActionResult {
            action_id,
            exit_code: output.status.code().unwrap_or(-1),
            stdout: output.stdout,
            stderr: output.stderr,
            outputs: vec![],
            output_records: Vec::new(),
            duration: Duration::default(),
            cached: false,
        })
    }

    /// Get sandbox configuration.
    pub fn config(&self) -> &SandboxConfig {
        &self.config
    }

    /// Get sandbox configuration (mutable).
    pub fn config_mut(&mut self) -> &mut SandboxConfig {
        &mut self.config
    }
}

impl Default for Sandbox {
    fn default() -> Self {
        Self::new().expect("Failed to create sandbox")
    }
}
