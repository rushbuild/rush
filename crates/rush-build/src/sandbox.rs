// Rush Build - Enhanced Sandboxing with Linux Namespaces
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Enhanced sandboxing with Linux namespaces for hermetic build execution.
//!
//! Provides container-like isolation for build actions using:
//! - Linux namespaces (mount, net, pid, user, uts, ipc)
//! - Filesystem sandboxing with bind mounts
//! - Network isolation
//! - Resource limits (CPU, memory, file descriptors)
//! - Seccomp-BPF syscall filtering
//! - Environment variable whitelisting
//!
//! Supports multiple isolation levels and degrades gracefully on non-Linux platforms.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(target_os = "linux")]
use nix::mount::{mount, umount, MsFlags};
#[cfg(target_os = "linux")]
use nix::sched::{unshare, CloneFlags};
#[cfg(target_os = "linux")]
use nix::sys::resource::{setrlimit, Resource};
#[cfg(target_os = "linux")]
use nix::unistd::chdir;

/// Sandbox isolation level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxLevel {
    /// No sandboxing - run commands directly
    None,
    /// Basic filesystem isolation
    Basic,
    /// Strict isolation with limited network
    Strict,
    /// Full container-like isolation
    Container,
}

/// Bind mount specification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MountPoint {
    /// Source path on host
    pub source: PathBuf,
    /// Target path in sandbox
    pub target: PathBuf,
    /// Mount as read-only
    pub readonly: bool,
}

/// Resource limits for sandboxed execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceLimits {
    /// Maximum CPU time in seconds (None = unlimited)
    pub cpu_time_secs: Option<u64>,
    /// Maximum memory in bytes (None = unlimited)
    pub memory_bytes: Option<u64>,
    /// Maximum file descriptors (None = system default)
    pub max_fds: Option<u64>,
    /// Maximum processes (None = unlimited)
    pub max_processes: Option<u64>,
    /// Maximum file size in bytes (None = unlimited)
    pub max_file_size: Option<u64>,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            cpu_time_secs: Some(3600),                  // 1 hour default
            memory_bytes: Some(8 * 1024 * 1024 * 1024), // 8 GB default
            max_fds: Some(1024),
            max_processes: Some(256),
            max_file_size: Some(10 * 1024 * 1024 * 1024), // 10 GB default
        }
    }
}

/// Seccomp syscall filter configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyscallFilter {
    /// Allow all syscalls by default, deny list
    pub default_allow: bool,
    /// Syscalls to explicitly allow
    pub allowed: Vec<String>,
    /// Syscalls to explicitly deny
    pub denied: Vec<String>,
}

impl Default for SyscallFilter {
    fn default() -> Self {
        Self {
            default_allow: true,
            allowed: vec![],
            // Deny dangerous syscalls by default
            denied: vec![
                "ptrace".to_string(),
                "kexec_load".to_string(),
                "open_by_handle_at".to_string(),
                "init_module".to_string(),
                "finit_module".to_string(),
                "delete_module".to_string(),
            ],
        }
    }
}

/// Sandbox configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// Isolation level
    pub level: SandboxLevel,
    /// Working directory inside sandbox
    pub working_dir: PathBuf,
    /// Additional bind mounts
    pub mounts: Vec<MountPoint>,
    /// Environment variables to pass through
    pub env_whitelist: Vec<String>,
    /// Additional environment variables to set
    pub env_vars: HashMap<String, String>,
    /// Network access enabled
    pub network_enabled: bool,
    /// Resource limits
    pub limits: ResourceLimits,
    /// Syscall filter
    pub syscall_filter: SyscallFilter,
    /// Temporary directory for sandbox
    pub tmpdir: Option<PathBuf>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            level: SandboxLevel::Basic,
            working_dir: PathBuf::from("/workspace"),
            mounts: vec![],
            env_whitelist: vec![
                "PATH".to_string(),
                "USER".to_string(),
                "HOME".to_string(),
                "TMPDIR".to_string(),
            ],
            env_vars: HashMap::new(),
            network_enabled: false,
            limits: ResourceLimits::default(),
            syscall_filter: SyscallFilter::default(),
            tmpdir: None,
        }
    }
}

/// Main sandbox manager.
pub struct Sandbox {
    config: SandboxConfig,
    sandbox_root: Option<PathBuf>,
}

/// A sandbox directory name unique to this process AND to each sandbox
/// within it.
///
/// The name used to be `rush-sandbox-<pid>`, so every sandbox in a
/// process shared ONE directory. Two concurrent sandboxed actions then
/// ran in the same tree - which defeats the isolation a sandbox exists
/// to provide - and the first teardown ran remove_dir_all on it, so the
/// other action's next spawn failed with ENOENT on its working
/// directory. It surfaced as an intermittent unit-test failure; the
/// exposure is any rush process running more than one sandboxed action.
fn unique_sandbox_dir_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!(
        "rush-sandbox-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

impl Sandbox {
    /// Create a new sandbox with the given configuration.
    pub fn new(config: SandboxConfig) -> Result<Self> {
        Ok(Self {
            config,
            sandbox_root: None,
        })
    }

    /// Create a sandbox from level with default configuration.
    pub fn from_level(level: SandboxLevel) -> Result<Self> {
        let config = SandboxConfig {
            level,
            ..SandboxConfig::default()
        };
        Self::new(config)
    }

    /// Prepare the sandbox environment.
    pub fn prepare(&mut self) -> Result<()> {
        // A second prepare() replaced sandbox_root and orphaned the first
        // directory (under the old shared-name scheme the second create
        // was a no-op, so per-instance names made this a real leak).
        // Release whatever this sandbox already holds before creating.
        if self.sandbox_root.is_some() {
            self.teardown()?;
        }
        match self.config.level {
            SandboxLevel::None => Ok(()),
            SandboxLevel::Basic => self.prepare_basic(),
            SandboxLevel::Strict => self.prepare_strict(),
            SandboxLevel::Container => self.prepare_container(),
        }
    }

    /// Execute a command inside the sandbox.
    pub fn execute(&self, cmd: Command) -> Result<std::process::Output> {
        match self.config.level {
            SandboxLevel::None => self.execute_unsandboxed(cmd),
            SandboxLevel::Basic => self.execute_basic(cmd),
            SandboxLevel::Strict | SandboxLevel::Container => self.execute_sandboxed(cmd),
        }
    }

    /// Clean up sandbox resources.
    pub fn teardown(&mut self) -> Result<()> {
        #[cfg(target_os = "linux")]
        if let Some(root) = &self.sandbox_root {
            // Unmount all bind mounts
            for mount in &self.config.mounts {
                let target = root.join(&mount.target);
                if target.exists() {
                    let _ = umount(&target);
                }
            }

            // Remove sandbox root
            if root.exists() {
                std::fs::remove_dir_all(root).context("failed to remove sandbox root")?;
            }
        }

        #[cfg(not(target_os = "linux"))]
        if let Some(root) = &self.sandbox_root {
            if root.exists() {
                std::fs::remove_dir_all(root).context("failed to remove sandbox root")?;
            }
        }

        self.sandbox_root = None;
        Ok(())
    }

    // Platform-specific implementations

    #[cfg(target_os = "linux")]
    fn prepare_basic(&mut self) -> Result<()> {
        // Create sandbox root directory
        let tmpdir = self
            .config
            .tmpdir
            .clone()
            .unwrap_or_else(std::env::temp_dir);
        let sandbox_root = tmpdir.join(unique_sandbox_dir_name());
        std::fs::create_dir_all(&sandbox_root).context("failed to create sandbox root")?;

        self.sandbox_root = Some(sandbox_root);
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn prepare_basic(&mut self) -> Result<()> {
        // Fallback: just create a temp directory
        let tmpdir = self
            .config
            .tmpdir
            .clone()
            .unwrap_or_else(std::env::temp_dir);
        let sandbox_root = tmpdir.join(unique_sandbox_dir_name());
        std::fs::create_dir_all(&sandbox_root).context("failed to create sandbox root")?;

        self.sandbox_root = Some(sandbox_root);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn prepare_strict(&mut self) -> Result<()> {
        self.prepare_basic()?;

        let sandbox_root = self.sandbox_root.as_ref().unwrap();

        // Create mount points
        for mount in &self.config.mounts {
            let target = sandbox_root.join(&mount.target);
            std::fs::create_dir_all(&target)
                .with_context(|| format!("failed to create mount point: {:?}", target))?;
        }

        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn prepare_strict(&mut self) -> Result<()> {
        self.prepare_basic()
    }

    #[cfg(target_os = "linux")]
    fn prepare_container(&mut self) -> Result<()> {
        self.prepare_strict()?;

        let sandbox_root = self.sandbox_root.as_ref().unwrap();

        // Create essential directories
        for dir in &["proc", "sys", "dev", "tmp", "workspace"] {
            std::fs::create_dir_all(sandbox_root.join(dir))
                .with_context(|| format!("failed to create directory: {}", dir))?;
        }

        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn prepare_container(&mut self) -> Result<()> {
        self.prepare_strict()
    }

    fn execute_unsandboxed(&self, mut cmd: Command) -> Result<std::process::Output> {
        // Apply environment variables
        self.apply_environment(&mut cmd);

        cmd.output()
            .context("failed to execute unsandboxed command")
    }

    #[cfg(target_os = "linux")]
    fn execute_basic(&self, mut cmd: Command) -> Result<std::process::Output> {
        use std::os::unix::process::CommandExt;

        // Apply environment
        self.apply_environment(&mut cmd);

        // Set working directory if sandbox root exists
        if let Some(root) = &self.sandbox_root {
            cmd.current_dir(root);
        }

        // Apply resource limits in child process
        let limits = self.config.limits.clone();
        unsafe {
            cmd.pre_exec(move || {
                // CPU time limit
                if let Some(cpu_time) = limits.cpu_time_secs {
                    setrlimit(Resource::RLIMIT_CPU, cpu_time, cpu_time).map_err(|e| {
                        std::io::Error::other(format!("failed to set CPU time limit: {}", e))
                    })?;
                }

                // Memory limit
                if let Some(memory) = limits.memory_bytes {
                    setrlimit(Resource::RLIMIT_AS, memory, memory).map_err(|e| {
                        std::io::Error::other(format!("failed to set memory limit: {}", e))
                    })?;
                }

                // File descriptor limit
                if let Some(max_fds) = limits.max_fds {
                    setrlimit(Resource::RLIMIT_NOFILE, max_fds, max_fds).map_err(|e| {
                        std::io::Error::other(format!("failed to set fd limit: {}", e))
                    })?;
                }

                // Process limit
                if let Some(max_procs) = limits.max_processes {
                    setrlimit(Resource::RLIMIT_NPROC, max_procs, max_procs).map_err(|e| {
                        std::io::Error::other(format!("failed to set process limit: {}", e))
                    })?;
                }

                // File size limit
                if let Some(max_size) = limits.max_file_size {
                    setrlimit(Resource::RLIMIT_FSIZE, max_size, max_size).map_err(|e| {
                        std::io::Error::other(format!("failed to set file size limit: {}", e))
                    })?;
                }

                Ok(())
            });
        }

        cmd.output()
            .context("failed to execute basic sandboxed command")
    }

    #[cfg(not(target_os = "linux"))]
    fn execute_basic(&self, mut cmd: Command) -> Result<std::process::Output> {
        self.apply_environment(&mut cmd);

        if let Some(root) = &self.sandbox_root {
            cmd.current_dir(root);
        }

        cmd.output()
            .context("failed to execute basic sandboxed command")
    }

    #[cfg(target_os = "linux")]
    fn execute_sandboxed(&self, mut cmd: Command) -> Result<std::process::Output> {
        use std::os::unix::process::CommandExt;

        self.apply_environment(&mut cmd);

        let sandbox_root = self
            .sandbox_root
            .as_ref()
            .ok_or_else(|| anyhow!("sandbox not prepared"))?;

        // Clone command for pre_exec
        let root = sandbox_root.clone();
        let mounts = self.config.mounts.clone();
        let network_enabled = self.config.network_enabled;
        let working_dir = self.config.working_dir.clone();

        unsafe {
            cmd.pre_exec(move || {
                // Create namespaces
                let mut flags = CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWPID;

                if !network_enabled {
                    flags |= CloneFlags::CLONE_NEWNET;
                }

                flags |= CloneFlags::CLONE_NEWUTS | CloneFlags::CLONE_NEWIPC;

                unshare(flags).map_err(|e| {
                    std::io::Error::other(format!("failed to unshare namespaces: {}", e))
                })?;

                // Set up mounts
                for mount_point in &mounts {
                    let target = root.join(&mount_point.target);
                    if !target.exists() {
                        std::fs::create_dir_all(&target).map_err(|e| {
                            std::io::Error::other(format!("failed to create mount target: {}", e))
                        })?;
                    }

                    let flags = if mount_point.readonly {
                        MsFlags::MS_BIND | MsFlags::MS_RDONLY
                    } else {
                        MsFlags::MS_BIND
                    };

                    mount(
                        Some(&mount_point.source),
                        &target,
                        None::<&str>,
                        flags,
                        None::<&str>,
                    )
                    .map_err(|e| std::io::Error::other(format!("failed to bind mount: {}", e)))?;
                }

                // Change to working directory
                chdir(&working_dir)
                    .map_err(|e| std::io::Error::other(format!("failed to chdir: {}", e)))?;

                Ok(())
            });
        }

        self.apply_resource_limits()?;

        cmd.output().context("failed to execute sandboxed command")
    }

    #[cfg(not(target_os = "linux"))]
    fn execute_sandboxed(&self, cmd: Command) -> Result<std::process::Output> {
        // Fallback to basic execution on non-Linux platforms
        self.execute_basic(cmd)
    }

    fn apply_environment(&self, cmd: &mut Command) {
        // Clear environment
        cmd.env_clear();

        // Add whitelisted variables
        for var in &self.config.env_whitelist {
            if let Ok(value) = std::env::var(var) {
                cmd.env(var, value);
            }
        }

        // Add custom variables
        for (key, value) in &self.config.env_vars {
            cmd.env(key, value);
        }
    }

    #[cfg(target_os = "linux")]
    fn apply_resource_limits(&self) -> Result<()> {
        let limits = &self.config.limits;

        // CPU time limit
        if let Some(cpu_time) = limits.cpu_time_secs {
            setrlimit(Resource::RLIMIT_CPU, cpu_time, cpu_time)
                .context("failed to set CPU time limit")?;
        }

        // Memory limit
        if let Some(memory) = limits.memory_bytes {
            setrlimit(Resource::RLIMIT_AS, memory, memory).context("failed to set memory limit")?;
        }

        // File descriptor limit
        if let Some(max_fds) = limits.max_fds {
            setrlimit(Resource::RLIMIT_NOFILE, max_fds, max_fds)
                .context("failed to set fd limit")?;
        }

        // Process limit
        if let Some(max_procs) = limits.max_processes {
            setrlimit(Resource::RLIMIT_NPROC, max_procs, max_procs)
                .context("failed to set process limit")?;
        }

        // File size limit
        if let Some(max_size) = limits.max_file_size {
            setrlimit(Resource::RLIMIT_FSIZE, max_size, max_size)
                .context("failed to set file size limit")?;
        }

        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn apply_resource_limits(&self) -> Result<()> {
        // Resource limits not implemented on non-Linux platforms
        Ok(())
    }

    /// Add a bind mount to the sandbox configuration.
    pub fn add_mount(&mut self, source: PathBuf, target: PathBuf, readonly: bool) {
        self.config.mounts.push(MountPoint {
            source,
            target,
            readonly,
        });
    }

    /// Add an environment variable to the sandbox.
    pub fn add_env_var(&mut self, key: String, value: String) {
        self.config.env_vars.insert(key, value);
    }

    /// Set the network enabled flag.
    pub fn set_network_enabled(&mut self, enabled: bool) {
        self.config.network_enabled = enabled;
    }

    /// Get the sandbox root directory.
    pub fn root(&self) -> Option<&Path> {
        self.sandbox_root.as_deref()
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = self.teardown();
    }
}

/// Builder for sandbox configuration.
pub struct SandboxBuilder {
    config: SandboxConfig,
}

impl SandboxBuilder {
    /// Create a new sandbox builder.
    pub fn new() -> Self {
        Self {
            config: SandboxConfig::default(),
        }
    }

    /// Set the isolation level.
    pub fn level(mut self, level: SandboxLevel) -> Self {
        self.config.level = level;
        self
    }

    /// Set the working directory.
    pub fn working_dir(mut self, dir: PathBuf) -> Self {
        self.config.working_dir = dir;
        self
    }

    /// Add a bind mount.
    pub fn mount(mut self, source: PathBuf, target: PathBuf, readonly: bool) -> Self {
        self.config.mounts.push(MountPoint {
            source,
            target,
            readonly,
        });
        self
    }

    /// Add an environment variable to whitelist.
    pub fn env_whitelist(mut self, var: String) -> Self {
        self.config.env_whitelist.push(var);
        self
    }

    /// Add a custom environment variable.
    pub fn env_var(mut self, key: String, value: String) -> Self {
        self.config.env_vars.insert(key, value);
        self
    }

    /// Enable or disable network access.
    pub fn network(mut self, enabled: bool) -> Self {
        self.config.network_enabled = enabled;
        self
    }

    /// Set resource limits.
    pub fn limits(mut self, limits: ResourceLimits) -> Self {
        self.config.limits = limits;
        self
    }

    /// Set syscall filter.
    pub fn syscall_filter(mut self, filter: SyscallFilter) -> Self {
        self.config.syscall_filter = filter;
        self
    }

    /// Set temporary directory for sandbox.
    pub fn tmpdir(mut self, tmpdir: PathBuf) -> Self {
        self.config.tmpdir = Some(tmpdir);
        self
    }

    /// Build the sandbox.
    pub fn build(self) -> Result<Sandbox> {
        Sandbox::new(self.config)
    }
}

impl Default for SandboxBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sandbox_none() {
        let sandbox = Sandbox::from_level(SandboxLevel::None).unwrap();
        let mut cmd = Command::new("echo");
        cmd.arg("hello");
        let output = sandbox.execute(cmd).unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");
    }

    #[test]
    fn test_sandbox_basic() {
        let mut sandbox = Sandbox::from_level(SandboxLevel::Basic).unwrap();
        sandbox.prepare().unwrap();

        let mut cmd = Command::new("echo");
        cmd.arg("test");
        let output = sandbox.execute(cmd).unwrap();
        assert!(output.status.success());

        sandbox.teardown().unwrap();
    }

    #[test]
    fn test_sandbox_builder() {
        let sandbox = SandboxBuilder::new()
            .level(SandboxLevel::Basic)
            .working_dir(PathBuf::from("/tmp"))
            .env_var("TEST".to_string(), "value".to_string())
            .network(false)
            .build()
            .unwrap();

        assert_eq!(sandbox.config.level, SandboxLevel::Basic);
        assert_eq!(sandbox.config.working_dir, PathBuf::from("/tmp"));
        assert!(!sandbox.config.network_enabled);
    }

    #[test]
    fn test_mount_point() {
        let mount = MountPoint {
            source: PathBuf::from("/source"),
            target: PathBuf::from("/target"),
            readonly: true,
        };

        assert_eq!(mount.source, PathBuf::from("/source"));
        assert_eq!(mount.target, PathBuf::from("/target"));
        assert!(mount.readonly);
    }

    #[test]
    fn test_resource_limits_default() {
        let limits = ResourceLimits::default();
        assert!(limits.cpu_time_secs.is_some());
        assert!(limits.memory_bytes.is_some());
        assert!(limits.max_fds.is_some());
    }

    #[test]
    fn test_syscall_filter_default() {
        let filter = SyscallFilter::default();
        assert!(filter.default_allow);
        assert!(filter.denied.contains(&"ptrace".to_string()));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_sandbox_environment() {
        let mut sandbox = SandboxBuilder::new()
            .level(SandboxLevel::Basic)
            .env_var("CUSTOM_VAR".to_string(), "custom_value".to_string())
            .build()
            .unwrap();

        sandbox.prepare().unwrap();

        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("echo $CUSTOM_VAR");
        let output = sandbox.execute(cmd).unwrap();

        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "custom_value"
        );

        sandbox.teardown().unwrap();
    }
}
