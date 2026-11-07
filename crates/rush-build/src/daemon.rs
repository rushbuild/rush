// Rush Build - Build Daemon
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Build daemon for persistent process acceleration.
//!
//! Keeps the build system running as a background daemon to avoid startup
//! costs, maintain warm caches, and enable incremental builds. Similar to
//! Bazel's server but with better memory management and faster response times.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, RwLock};

/// Build daemon server.
pub struct BuildDaemon {
    /// Daemon configuration.
    config: DaemonConfig,

    /// Workspace root.
    workspace_root: PathBuf,

    /// Active build sessions.
    sessions: Arc<RwLock<Vec<BuildSession>>>,

    /// Daemon state.
    state: Arc<RwLock<DaemonState>>,

    /// Shutdown signal.
    shutdown_tx: Option<mpsc::Sender<()>>,
}

/// Daemon configuration.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Socket path for IPC.
    pub socket_path: PathBuf,

    /// Idle timeout before shutdown.
    pub idle_timeout: Duration,

    /// Maximum memory usage (bytes).
    pub max_memory_bytes: u64,

    /// Enable JIT compilation caching.
    pub enable_jit_cache: bool,

    /// Maximum concurrent builds.
    pub max_concurrent_builds: usize,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        DaemonConfig {
            socket_path: PathBuf::from("/tmp/rush-daemon.sock"),
            idle_timeout: Duration::from_secs(3 * 3600), // 3 hours
            max_memory_bytes: 4 * 1024 * 1024 * 1024,    // 4GB
            enable_jit_cache: true,
            max_concurrent_builds: 4,
        }
    }
}

/// Build session.
#[derive(Debug, Clone)]
struct BuildSession {
    /// Session ID.
    id: String,

    /// Session start time.
    _start_time: Instant,

    /// Client PID.
    _client_pid: u32,

    /// Build targets.
    _targets: Vec<String>,
}

/// Daemon state.
#[derive(Debug, Clone)]
struct DaemonState {
    /// Daemon start time.
    start_time: SystemTime,

    /// Last activity time.
    last_activity: Instant,

    /// Total builds executed.
    builds_executed: u64,

    /// Current memory usage (bytes).
    memory_usage: u64,

    /// Active build count.
    active_builds: usize,
}

/// Client request to daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DaemonRequest {
    /// Execute a build.
    Build {
        targets: Vec<String>,
        flags: Vec<String>,
        client_pid: u32,
    },

    /// Query daemon status.
    Status,

    /// Shutdown the daemon.
    Shutdown,

    /// Ping (keep-alive).
    Ping,

    /// Clean build artifacts.
    Clean { targets: Vec<String> },
}

/// Daemon response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DaemonResponse {
    /// Build completed successfully.
    BuildSuccess {
        duration_ms: u64,
        actions_executed: usize,
        cache_hits: usize,
    },

    /// Build failed.
    BuildFailure { error: String },

    /// Daemon status.
    Status {
        uptime_secs: u64,
        builds_executed: u64,
        memory_usage_mb: f64,
        active_builds: usize,
    },

    /// Command acknowledged.
    Ack,

    /// Error response.
    Error { message: String },
}

impl BuildDaemon {
    /// Create a new build daemon.
    pub fn new<P: AsRef<Path>>(workspace_root: P, config: DaemonConfig) -> Self {
        let state = DaemonState {
            start_time: SystemTime::now(),
            last_activity: Instant::now(),
            builds_executed: 0,
            memory_usage: 0,
            active_builds: 0,
        };

        BuildDaemon {
            config,
            workspace_root: workspace_root.as_ref().to_path_buf(),
            sessions: Arc::new(RwLock::new(Vec::new())),
            state: Arc::new(RwLock::new(state)),
            shutdown_tx: None,
        }
    }

    /// Start the daemon server.
    pub async fn start(&mut self) -> Result<()> {
        // Remove stale socket if exists
        if self.config.socket_path.exists() {
            std::fs::remove_file(&self.config.socket_path)?;
        }

        // Bind Unix domain socket
        let listener =
            UnixListener::bind(&self.config.socket_path).context("Failed to bind daemon socket")?;

        eprintln!(
            "Rush daemon started at {}",
            self.config.socket_path.display()
        );

        // Set up shutdown channel
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel(1);
        self.shutdown_tx = Some(shutdown_tx);

        // Clone state for idle timeout checker
        let state = Arc::clone(&self.state);
        let idle_timeout = self.config.idle_timeout;
        let shutdown_tx_clone = self.shutdown_tx.as_ref().unwrap().clone();

        // Spawn idle timeout checker
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;

                let state = state.read().await;
                if state.last_activity.elapsed() > idle_timeout && state.active_builds == 0 {
                    eprintln!("Daemon idle timeout reached, shutting down...");
                    let _ = shutdown_tx_clone.send(()).await;
                    break;
                }
            }
        });

        // Main accept loop
        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, _)) => {
                            let state = Arc::clone(&self.state);
                            let sessions = Arc::clone(&self.sessions);
                            let workspace = self.workspace_root.clone();

                            tokio::spawn(async move {
                                if let Err(e) = Self::handle_client(stream, state, sessions, workspace).await {
                                    eprintln!("Client handler error: {}", e);
                                }
                            });
                        }
                        Err(e) => {
                            eprintln!("Accept error: {}", e);
                        }
                    }
                }
                _ = shutdown_rx.recv() => {
                    eprintln!("Shutting down daemon...");
                    break;
                }
            }
        }

        // Cleanup
        let _ = std::fs::remove_file(&self.config.socket_path);

        Ok(())
    }

    /// Handle client connection.
    async fn handle_client(
        stream: UnixStream,
        state: Arc<RwLock<DaemonState>>,
        sessions: Arc<RwLock<Vec<BuildSession>>>,
        workspace: PathBuf,
    ) -> Result<()> {
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = String::new();

        loop {
            line.clear();
            let bytes_read = reader.read_line(&mut line).await?;

            if bytes_read == 0 {
                break; // EOF
            }

            let request: DaemonRequest = serde_json::from_str(&line)?;

            // Update activity time
            {
                let mut state = state.write().await;
                state.last_activity = Instant::now();
            }

            let response = Self::process_request(request, &state, &sessions, &workspace).await?;

            let response_json = serde_json::to_string(&response)?;
            writer.write_all(response_json.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
        }

        Ok(())
    }

    /// Process a client request.
    async fn process_request(
        request: DaemonRequest,
        state: &Arc<RwLock<DaemonState>>,
        sessions: &Arc<RwLock<Vec<BuildSession>>>,
        workspace: &Path,
    ) -> Result<DaemonResponse> {
        match request {
            DaemonRequest::Build {
                targets,
                flags,
                client_pid,
            } => {
                // Update state
                {
                    let mut state = state.write().await;
                    state.active_builds += 1;
                }

                // Create session
                let session_id = format!("session_{}", client_pid);
                let session = BuildSession {
                    id: session_id.clone(),
                    _start_time: Instant::now(),
                    _client_pid: client_pid,
                    _targets: targets.clone(),
                };

                sessions.write().await.push(session);

                // Execute build (simplified - would use actual build orchestrator)
                let start = Instant::now();
                let result = Self::execute_build(workspace, &targets, &flags).await;

                // Update state
                {
                    let mut state = state.write().await;
                    state.active_builds -= 1;
                    state.builds_executed += 1;
                }

                // Remove session
                sessions.write().await.retain(|s| s.id != session_id);

                match result {
                    Ok((actions_executed, cache_hits)) => Ok(DaemonResponse::BuildSuccess {
                        duration_ms: start.elapsed().as_millis() as u64,
                        actions_executed,
                        cache_hits,
                    }),
                    Err(e) => Ok(DaemonResponse::BuildFailure {
                        error: e.to_string(),
                    }),
                }
            }

            DaemonRequest::Status => {
                let state = state.read().await;
                let uptime = state
                    .start_time
                    .elapsed()
                    .unwrap_or(Duration::from_secs(0))
                    .as_secs();

                Ok(DaemonResponse::Status {
                    uptime_secs: uptime,
                    builds_executed: state.builds_executed,
                    memory_usage_mb: state.memory_usage as f64 / 1_048_576.0,
                    active_builds: state.active_builds,
                })
            }

            DaemonRequest::Shutdown => {
                // Graceful shutdown would be triggered here
                Ok(DaemonResponse::Ack)
            }

            DaemonRequest::Ping => Ok(DaemonResponse::Ack),

            DaemonRequest::Clean { targets: _ } => {
                // Clean implementation would go here
                Ok(DaemonResponse::Ack)
            }
        }
    }

    /// Execute a build (simplified).
    async fn execute_build(
        _workspace: &Path,
        targets: &[String],
        _flags: &[String],
    ) -> Result<(usize, usize)> {
        // This is a simplified version - in practice would:
        // 1. Load build graph
        // 2. Execute actions with caching
        // 3. Return results

        // Simulate build execution
        tokio::time::sleep(Duration::from_millis(100)).await;

        let actions_executed = targets.len() * 10;
        let cache_hits = actions_executed / 2;

        Ok((actions_executed, cache_hits))
    }

    /// Check if daemon is running.
    pub async fn is_running(socket_path: &Path) -> bool {
        socket_path.exists() && UnixStream::connect(socket_path).await.is_ok()
    }
}

/// Daemon client for communicating with the daemon.
pub struct DaemonClient {
    /// Socket path.
    socket_path: PathBuf,
}

impl DaemonClient {
    /// Create a new daemon client.
    pub fn new(socket_path: PathBuf) -> Self {
        DaemonClient { socket_path }
    }

    /// Send request to daemon.
    pub async fn send_request(&self, request: DaemonRequest) -> Result<DaemonResponse> {
        let mut stream = UnixStream::connect(&self.socket_path)
            .await
            .context("Failed to connect to daemon")?;

        // Send request
        let request_json = serde_json::to_string(&request)?;
        stream.write_all(request_json.as_bytes()).await?;
        stream.write_all(b"\n").await?;
        stream.flush().await?;

        // Read response
        let mut reader = BufReader::new(stream);
        let mut response_line = String::new();
        reader.read_line(&mut response_line).await?;

        let response: DaemonResponse = serde_json::from_str(&response_line)?;
        Ok(response)
    }

    /// Execute a build via daemon.
    pub async fn build(&self, targets: Vec<String>, flags: Vec<String>) -> Result<DaemonResponse> {
        let client_pid = std::process::id();

        self.send_request(DaemonRequest::Build {
            targets,
            flags,
            client_pid,
        })
        .await
    }

    /// Get daemon status.
    pub async fn status(&self) -> Result<DaemonResponse> {
        self.send_request(DaemonRequest::Status).await
    }

    /// Shutdown daemon.
    pub async fn shutdown(&self) -> Result<DaemonResponse> {
        self.send_request(DaemonRequest::Shutdown).await
    }

    /// Ping daemon.
    pub async fn ping(&self) -> Result<DaemonResponse> {
        self.send_request(DaemonRequest::Ping).await
    }
}

/// Daemon manager for starting/stopping daemon.
pub struct DaemonManager {
    /// Daemon configuration.
    config: DaemonConfig,

    /// Workspace root.
    workspace_root: PathBuf,
}

impl DaemonManager {
    /// Create a new daemon manager.
    pub fn new<P: AsRef<Path>>(workspace_root: P, config: DaemonConfig) -> Self {
        DaemonManager {
            config,
            workspace_root: workspace_root.as_ref().to_path_buf(),
        }
    }

    /// Start daemon in background.
    pub async fn start_daemon(&self) -> Result<()> {
        if BuildDaemon::is_running(&self.config.socket_path).await {
            eprintln!("Daemon already running");
            return Ok(());
        }

        // Fork and daemonize (simplified - in practice would use proper daemonization)
        let config = self.config.clone();
        let workspace = self.workspace_root.clone();

        tokio::spawn(async move {
            let mut daemon = BuildDaemon::new(workspace, config);
            if let Err(e) = daemon.start().await {
                eprintln!("Daemon error: {}", e);
            }
        });

        // Wait for daemon to be ready
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if BuildDaemon::is_running(&self.config.socket_path).await {
                eprintln!("Daemon started successfully");
                return Ok(());
            }
        }

        anyhow::bail!("Failed to start daemon")
    }

    /// Stop daemon.
    pub async fn stop_daemon(&self) -> Result<()> {
        let client = DaemonClient::new(self.config.socket_path.clone());
        client.shutdown().await?;

        // Wait for daemon to stop
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if !BuildDaemon::is_running(&self.config.socket_path).await {
                eprintln!("Daemon stopped");
                return Ok(());
            }
        }

        anyhow::bail!("Daemon did not stop")
    }

    /// Restart daemon.
    pub async fn restart_daemon(&self) -> Result<()> {
        let _ = self.stop_daemon().await; // Ignore error if not running
        self.start_daemon().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_daemon_client_server() {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let socket_path = temp_dir.path().join("test-daemon.sock");

        let config = DaemonConfig {
            socket_path: socket_path.clone(),
            idle_timeout: Duration::from_secs(60),
            ..Default::default()
        };

        // Start daemon in background
        let workspace = temp_dir.path().to_path_buf();
        let config_clone = config.clone();
        tokio::spawn(async move {
            let mut daemon = BuildDaemon::new(workspace, config_clone);
            let _ = daemon.start().await;
        });

        // Wait for daemon to start
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Create client
        let client = DaemonClient::new(socket_path);

        // Test ping
        let response = client.ping().await.unwrap();
        assert!(matches!(response, DaemonResponse::Ack));

        // Test status
        let response = client.status().await.unwrap();
        if let DaemonResponse::Status {
            builds_executed, ..
        } = response
        {
            assert_eq!(builds_executed, 0);
        } else {
            panic!("Expected status response");
        }
    }
}
