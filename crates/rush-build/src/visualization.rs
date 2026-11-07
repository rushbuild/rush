// Rush Build - Build Visualization and Analytics
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Build visualization and analytics for understanding build performance.
//!
//! Generates visual representations of build graphs, timing analysis, and
//! performance metrics to help developers optimize build times and understand
//! dependencies.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Build graph visualization.
#[derive(Debug, Clone)]
pub struct GraphVisualizer {
    /// Nodes in the graph.
    nodes: Vec<GraphNode>,

    /// Edges between nodes.
    edges: Vec<GraphEdge>,
}

/// Graph node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphNode {
    /// Node ID.
    pub id: String,

    /// Node label (display name).
    pub label: String,

    /// Node type.
    pub node_type: String,

    /// Execution time (milliseconds).
    pub duration_ms: Option<u64>,

    /// Cache hit status.
    pub cache_hit: Option<bool>,

    /// Size in bytes (for outputs).
    pub size_bytes: Option<u64>,
}

/// Graph edge (dependency).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEdge {
    /// Source node ID.
    pub from: String,

    /// Target node ID.
    pub to: String,

    /// Edge type (e.g., "depends", "generates").
    pub edge_type: String,
}

impl GraphVisualizer {
    /// Create a new graph visualizer.
    pub fn new() -> Self {
        GraphVisualizer {
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }

    /// Add a node to the graph.
    pub fn add_node(&mut self, node: GraphNode) {
        self.nodes.push(node);
    }

    /// Add an edge to the graph.
    pub fn add_edge(&mut self, edge: GraphEdge) {
        self.edges.push(edge);
    }

    /// Export graph as DOT format (Graphviz).
    pub fn to_dot(&self) -> String {
        let mut dot = String::from("digraph BuildGraph {\n");
        dot.push_str("  rankdir=LR;\n");
        dot.push_str("  node [shape=box];\n\n");

        // Add nodes
        for node in &self.nodes {
            let color = match node.cache_hit {
                Some(true) => "green",
                Some(false) => "red",
                None => "gray",
            };

            let label = if let Some(duration) = node.duration_ms {
                format!("{}\\n{}ms", node.label, duration)
            } else {
                node.label.clone()
            };

            dot.push_str(&format!(
                "  \"{}\" [label=\"{}\", color=\"{}\"];\n",
                node.id, label, color
            ));
        }

        dot.push('\n');

        // Add edges
        for edge in &self.edges {
            dot.push_str(&format!("  \"{}\" -> \"{}\";\n", edge.from, edge.to));
        }

        dot.push_str("}\n");
        dot
    }

    /// Export graph as JSON.
    pub fn to_json(&self) -> Result<String> {
        #[derive(Serialize)]
        struct JsonGraph<'a> {
            nodes: &'a [GraphNode],
            edges: &'a [GraphEdge],
        }

        let graph = JsonGraph {
            nodes: &self.nodes,
            edges: &self.edges,
        };

        serde_json::to_string_pretty(&graph).context("Failed to serialize graph to JSON")
    }

    /// Export graph as Mermaid diagram.
    pub fn to_mermaid(&self) -> String {
        let mut mermaid = String::from("graph LR\n");

        for edge in &self.edges {
            let from_node = self.nodes.iter().find(|n| n.id == edge.from);
            let to_node = self.nodes.iter().find(|n| n.id == edge.to);

            let from_label = from_node.map(|n| &n.label).unwrap_or(&edge.from);
            let to_label = to_node.map(|n| &n.label).unwrap_or(&edge.to);

            mermaid.push_str(&format!(
                "  {}[{}] --> {}[{}]\n",
                self.sanitize_mermaid_id(&edge.from),
                from_label,
                self.sanitize_mermaid_id(&edge.to),
                to_label
            ));
        }

        mermaid
    }

    /// Sanitize ID for Mermaid syntax.
    fn sanitize_mermaid_id(&self, id: &str) -> String {
        id.replace(['/', ':', '.'], "_")
    }

    /// Get node count.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Get edge count.
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }
}

impl Default for GraphVisualizer {
    fn default() -> Self {
        Self::new()
    }
}

/// Build timing analysis.
#[derive(Debug, Clone)]
pub struct TimingAnalyzer {
    /// Action timings.
    timings: HashMap<String, ActionTiming>,
}

/// Action timing information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionTiming {
    /// Action ID.
    pub action_id: String,

    /// Start time (Unix timestamp).
    pub start_time: u64,

    /// Duration in milliseconds.
    pub duration_ms: u64,

    /// Cache hit status.
    pub cache_hit: bool,

    /// Worker ID that executed the action.
    pub worker_id: Option<usize>,
}

impl TimingAnalyzer {
    /// Create a new timing analyzer.
    pub fn new() -> Self {
        TimingAnalyzer {
            timings: HashMap::new(),
        }
    }

    /// Add action timing.
    pub fn add_timing(&mut self, timing: ActionTiming) {
        self.timings.insert(timing.action_id.clone(), timing);
    }

    /// Get total build time.
    pub fn total_time_ms(&self) -> u64 {
        if self.timings.is_empty() {
            return 0;
        }

        let min_start = self
            .timings
            .values()
            .map(|t| t.start_time)
            .min()
            .unwrap_or(0);

        let max_end = self
            .timings
            .values()
            .map(|t| t.start_time + t.duration_ms)
            .max()
            .unwrap_or(0);

        max_end.saturating_sub(min_start)
    }

    /// Get cumulative execution time (sum of all action durations).
    pub fn cumulative_time_ms(&self) -> u64 {
        self.timings.values().map(|t| t.duration_ms).sum()
    }

    /// Get parallelism factor (cumulative / total).
    pub fn parallelism_factor(&self) -> f64 {
        let total = self.total_time_ms();
        if total == 0 {
            return 0.0;
        }

        let cumulative = self.cumulative_time_ms();
        cumulative as f64 / total as f64
    }

    /// Get slowest actions.
    pub fn slowest_actions(&self, count: usize) -> Vec<&ActionTiming> {
        let mut timings: Vec<_> = self.timings.values().collect();
        timings.sort_by_key(|t| std::cmp::Reverse(t.duration_ms));
        timings.into_iter().take(count).collect()
    }

    /// Get cache hit rate.
    pub fn cache_hit_rate(&self) -> f64 {
        if self.timings.is_empty() {
            return 0.0;
        }

        let hits = self.timings.values().filter(|t| t.cache_hit).count();
        (hits as f64 / self.timings.len() as f64) * 100.0
    }

    /// Generate timing report.
    pub fn generate_report(&self) -> TimingReport {
        TimingReport {
            total_actions: self.timings.len(),
            total_time_ms: self.total_time_ms(),
            cumulative_time_ms: self.cumulative_time_ms(),
            parallelism_factor: self.parallelism_factor(),
            cache_hit_rate: self.cache_hit_rate(),
            slowest_actions: self
                .slowest_actions(10)
                .iter()
                .map(|&t| t.clone())
                .collect(),
        }
    }
}

impl Default for TimingAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

/// Timing analysis report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimingReport {
    /// Total number of actions.
    pub total_actions: usize,

    /// Total wall-clock time.
    pub total_time_ms: u64,

    /// Cumulative execution time.
    pub cumulative_time_ms: u64,

    /// Parallelism factor.
    pub parallelism_factor: f64,

    /// Cache hit rate.
    pub cache_hit_rate: f64,

    /// Slowest actions.
    pub slowest_actions: Vec<ActionTiming>,
}

/// Build analytics dashboard data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalyticsDashboard {
    /// Build metrics.
    pub metrics: BuildMetrics,

    /// Timing analysis.
    pub timing: TimingReport,

    /// Dependency statistics.
    pub dependencies: DependencyStats,

    /// Resource utilization.
    pub resources: ResourceStats,
}

/// Build metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildMetrics {
    /// Total targets built.
    pub total_targets: usize,

    /// Successful builds.
    pub successful: usize,

    /// Failed builds.
    pub failed: usize,

    /// Cached targets.
    pub cached: usize,

    /// Build duration.
    pub duration_ms: u64,
}

/// Dependency statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyStats {
    /// Total dependencies.
    pub total_deps: usize,

    /// Average dependency depth.
    pub avg_depth: f64,

    /// Maximum dependency depth.
    pub max_depth: usize,

    /// Targets with most dependencies.
    pub most_deps: Vec<(String, usize)>,
}

/// Resource utilization statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceStats {
    /// Peak memory usage (bytes).
    pub peak_memory_bytes: u64,

    /// Average CPU utilization (percentage).
    pub avg_cpu_percent: f64,

    /// Peak worker count.
    pub peak_workers: usize,

    /// Network bytes transferred.
    pub network_bytes: u64,
}

/// HTML report generator.
pub struct HtmlReportGenerator {
    /// Dashboard data.
    dashboard: AnalyticsDashboard,
}

impl HtmlReportGenerator {
    /// Create a new HTML report generator.
    pub fn new(dashboard: AnalyticsDashboard) -> Self {
        HtmlReportGenerator { dashboard }
    }

    /// Generate HTML report.
    pub fn generate(&self) -> String {
        let mut html = String::from("<!DOCTYPE html>\n");
        html.push_str("<html>\n<head>\n");
        html.push_str("<title>Rush Build Report</title>\n");
        html.push_str("<style>\n");
        html.push_str(Self::css());
        html.push_str("</style>\n");
        html.push_str("</head>\n<body>\n");

        html.push_str("<h1>Rush Build Report</h1>\n");

        // Metrics section
        html.push_str(&self.metrics_section());

        // Timing section
        html.push_str(&self.timing_section());

        // Dependencies section
        html.push_str(&self.dependencies_section());

        // Resources section
        html.push_str(&self.resources_section());

        html.push_str("</body>\n</html>");
        html
    }

    /// CSS styles.
    fn css() -> &'static str {
        r#"
        body { font-family: Arial, sans-serif; margin: 20px; background: #f5f5f5; }
        h1 { color: #333; }
        h2 { color: #666; margin-top: 30px; }
        .section { background: white; padding: 20px; margin: 20px 0; border-radius: 5px; box-shadow: 0 2px 4px rgba(0,0,0,0.1); }
        .metric { display: inline-block; margin: 10px 20px; }
        .metric-label { font-weight: bold; color: #666; }
        .metric-value { font-size: 24px; color: #333; }
        table { border-collapse: collapse; width: 100%; margin-top: 10px; }
        th, td { padding: 10px; text-align: left; border-bottom: 1px solid #ddd; }
        th { background-color: #4CAF50; color: white; }
        .success { color: #4CAF50; }
        .failure { color: #f44336; }
        .warning { color: #ff9800; }
        "#
    }

    /// Generate metrics section.
    fn metrics_section(&self) -> String {
        let m = &self.dashboard.metrics;
        format!(
            r#"<div class="section">
<h2>Build Metrics</h2>
<div class="metric">
  <div class="metric-label">Total Targets</div>
  <div class="metric-value">{}</div>
</div>
<div class="metric">
  <div class="metric-label">Successful</div>
  <div class="metric-value success">{}</div>
</div>
<div class="metric">
  <div class="metric-label">Failed</div>
  <div class="metric-value failure">{}</div>
</div>
<div class="metric">
  <div class="metric-label">Cached</div>
  <div class="metric-value warning">{}</div>
</div>
<div class="metric">
  <div class="metric-label">Duration</div>
  <div class="metric-value">{:.2}s</div>
</div>
</div>"#,
            m.total_targets,
            m.successful,
            m.failed,
            m.cached,
            m.duration_ms as f64 / 1000.0
        )
    }

    /// Generate timing section.
    fn timing_section(&self) -> String {
        let t = &self.dashboard.timing;
        let mut html = String::from(r#"<div class="section"><h2>Timing Analysis</h2>"#);

        html.push_str(&format!(
            r#"<div class="metric">
  <div class="metric-label">Parallelism Factor</div>
  <div class="metric-value">{:.2}x</div>
</div>
<div class="metric">
  <div class="metric-label">Cache Hit Rate</div>
  <div class="metric-value">{:.1}%</div>
</div>"#,
            t.parallelism_factor, t.cache_hit_rate
        ));

        html.push_str("<h3>Slowest Actions</h3>");
        html.push_str("<table><tr><th>Action</th><th>Duration (ms)</th><th>Cached</th></tr>");

        for action in &t.slowest_actions {
            html.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{}</td></tr>",
                action.action_id,
                action.duration_ms,
                if action.cache_hit { "Yes" } else { "No" }
            ));
        }

        html.push_str("</table></div>");
        html
    }

    /// Generate dependencies section.
    fn dependencies_section(&self) -> String {
        let d = &self.dashboard.dependencies;
        format!(
            r#"<div class="section">
<h2>Dependencies</h2>
<div class="metric">
  <div class="metric-label">Total Dependencies</div>
  <div class="metric-value">{}</div>
</div>
<div class="metric">
  <div class="metric-label">Average Depth</div>
  <div class="metric-value">{:.1}</div>
</div>
<div class="metric">
  <div class="metric-label">Max Depth</div>
  <div class="metric-value">{}</div>
</div>
</div>"#,
            d.total_deps, d.avg_depth, d.max_depth
        )
    }

    /// Generate resources section.
    fn resources_section(&self) -> String {
        let r = &self.dashboard.resources;
        format!(
            r#"<div class="section">
<h2>Resource Utilization</h2>
<div class="metric">
  <div class="metric-label">Peak Memory</div>
  <div class="metric-value">{:.1} MB</div>
</div>
<div class="metric">
  <div class="metric-label">Avg CPU</div>
  <div class="metric-value">{:.1}%</div>
</div>
<div class="metric">
  <div class="metric-label">Peak Workers</div>
  <div class="metric-value">{}</div>
</div>
<div class="metric">
  <div class="metric-label">Network Transfer</div>
  <div class="metric-value">{:.1} MB</div>
</div>
</div>"#,
            r.peak_memory_bytes as f64 / 1_048_576.0,
            r.avg_cpu_percent,
            r.peak_workers,
            r.network_bytes as f64 / 1_048_576.0
        )
    }

    /// Write report to file.
    pub fn write_to_file<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let html = self.generate();
        std::fs::write(path.as_ref(), html).context("Failed to write HTML report")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_graph_visualizer() {
        let mut viz = GraphVisualizer::new();

        viz.add_node(GraphNode {
            id: "a".to_string(),
            label: "Node A".to_string(),
            node_type: "compile".to_string(),
            duration_ms: Some(100),
            cache_hit: Some(false),
            size_bytes: None,
        });

        viz.add_node(GraphNode {
            id: "b".to_string(),
            label: "Node B".to_string(),
            node_type: "link".to_string(),
            duration_ms: Some(50),
            cache_hit: Some(true),
            size_bytes: None,
        });

        viz.add_edge(GraphEdge {
            from: "a".to_string(),
            to: "b".to_string(),
            edge_type: "depends".to_string(),
        });

        assert_eq!(viz.node_count(), 2);
        assert_eq!(viz.edge_count(), 1);

        let dot = viz.to_dot();
        assert!(dot.contains("digraph BuildGraph"));
        assert!(dot.contains("Node A"));
    }

    #[test]
    fn test_timing_analyzer() {
        let mut analyzer = TimingAnalyzer::new();

        analyzer.add_timing(ActionTiming {
            action_id: "action1".to_string(),
            start_time: 1000,
            duration_ms: 100,
            cache_hit: false,
            worker_id: Some(0),
        });

        analyzer.add_timing(ActionTiming {
            action_id: "action2".to_string(),
            start_time: 1050,
            duration_ms: 200,
            cache_hit: true,
            worker_id: Some(1),
        });

        assert_eq!(analyzer.cumulative_time_ms(), 300);
        assert_eq!(analyzer.cache_hit_rate(), 50.0);

        let slowest = analyzer.slowest_actions(1);
        assert_eq!(slowest[0].action_id, "action2");
    }
}
