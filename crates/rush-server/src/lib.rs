// Rush Server - Build server
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Build dashboard server with web UI.
//!
//! Provides a web-based dashboard for monitoring build progress, viewing
//! statistics, and visualizing dependency graphs.

#![warn(missing_docs)]

use anyhow::Result;
use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Json, Response},
    routing::get,
    Router,
};
use rush_build::{BuildOrchestrator, RemoteExecutorStats};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_http::trace::TraceLayer;

/// Build server state shared across handlers.
#[derive(Clone)]
struct ServerState {
    orchestrator: Arc<RwLock<BuildOrchestrator>>,
}

/// Build status response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildStatus {
    /// Whether a build is currently running.
    pub running: bool,

    /// Current workspace root.
    pub workspace_root: String,

    /// Number of tracked files.
    pub tracked_files: usize,

    /// Number of tracked targets.
    pub tracked_targets: usize,
}

/// Statistics response combining all metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsResponse {
    /// Build metrics.
    pub metrics: serde_json::Value,

    /// Remote executor statistics (if enabled).
    pub remote_stats: Option<RemoteExecutorStats>,

    /// Worker pool statistics.
    pub worker_stats: std::collections::HashMap<String, usize>,
}

/// Build server daemon with web dashboard.
///
/// Provides HTTP API and web UI for build monitoring.
pub struct BuildServer {
    state: ServerState,
    port: u16,
}

impl BuildServer {
    /// Create a new build server on default port (8080).
    pub fn new() -> Result<Self> {
        Self::with_port(8080)
    }

    /// Create a build server on a specific port.
    pub fn with_port(port: u16) -> Result<Self> {
        let orchestrator = BuildOrchestrator::new()?;
        let state = ServerState {
            orchestrator: Arc::new(RwLock::new(orchestrator)),
        };

        Ok(BuildServer { state, port })
    }

    /// Start the dashboard server.
    ///
    /// Serves HTTP API and web UI on the configured port.
    pub async fn start(&self) -> Result<()> {
        let app = Router::new()
            .route("/", get(dashboard_page))
            .route("/api/status", get(status_handler))
            .route("/api/stats", get(stats_handler))
            .route("/api/metrics", get(metrics_handler))
            .route("/api/graph", get(graph_handler))
            .layer(TraceLayer::new_for_http())
            .with_state(self.state.clone());

        let addr = format!("0.0.0.0:{}", self.port);
        println!("🌐 Rush Build Dashboard starting on http://{}", addr);
        println!("   API available at http://{}⁣/api/status", addr);
        println!("   Dashboard at http://{}", addr);

        let listener = tokio::net::TcpListener::bind(&addr).await?;
        axum::serve(listener, app).await?;

        Ok(())
    }

    /// Get the server port.
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Default for BuildServer {
    fn default() -> Self {
        Self::new().expect("Failed to create build server")
    }
}

/// Handler for dashboard HTML page.
async fn dashboard_page() -> impl IntoResponse {
    Html(include_str!("dashboard.html"))
}

/// Handler for /api/status endpoint.
async fn status_handler(State(state): State<ServerState>) -> impl IntoResponse {
    let orch = state.orchestrator.read().await;

    let stats =
        orch.incremental_stats()
            .await
            .unwrap_or(rush_build::incremental::ChangeDetectorStats {
                tracked_files: 0,
                tracked_targets: 0,
            });

    let status = BuildStatus {
        running: false, // TODO: Track active builds
        workspace_root: orch.workspace_root().display().to_string(),
        tracked_files: stats.tracked_files,
        tracked_targets: stats.tracked_targets,
    };

    Json(status)
}

/// Handler for /api/stats endpoint.
async fn stats_handler(State(state): State<ServerState>) -> impl IntoResponse {
    let orch = state.orchestrator.read().await;

    let metrics = orch.metrics().to_json();
    let remote_stats = orch.remote_executor_stats().await;
    let worker_stats = orch.worker_pool_stats().await;

    let response = StatsResponse {
        metrics,
        remote_stats,
        worker_stats,
    };

    Json(response)
}

/// Handler for /api/metrics endpoint.
async fn metrics_handler(State(state): State<ServerState>) -> impl IntoResponse {
    let orch = state.orchestrator.read().await;
    Json(orch.metrics().to_json())
}

/// Handler for /api/graph endpoint.
async fn graph_handler(State(state): State<ServerState>) -> Result<impl IntoResponse, AppError> {
    let orch = state.orchestrator.read().await;

    match orch.generate_dot_graph(None) {
        Ok(dot) => Ok(Json(serde_json::json!({
            "format": "dot",
            "graph": dot,
        }))),
        Err(e) => Err(AppError(e)),
    }
}

/// Error wrapper for axum handlers.
struct AppError(anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Internal error: {}", self.0),
        )
            .into_response()
    }
}

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}
