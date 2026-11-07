// Rush Build - Build Optimization Suggestions
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Automated build optimization suggestions.
//!
//! Analyzes build patterns and provides actionable recommendations to improve
//! build performance, reduce cache misses, and optimize dependency graphs.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Optimization analyzer.
pub struct OptimizationAnalyzer {
    /// Build metrics to analyze.
    metrics: BuildAnalysisMetrics,

    /// Generated suggestions.
    suggestions: Vec<OptimizationSuggestion>,
}

/// Build metrics for analysis.
#[derive(Debug, Clone, Default)]
pub struct BuildAnalysisMetrics {
    /// Total build actions.
    pub total_actions: usize,

    /// Cache hit rate.
    pub cache_hit_rate: f64,

    /// Average action duration (ms).
    pub avg_action_duration_ms: f64,

    /// Parallelism factor.
    pub parallelism_factor: f64,

    /// Actions per target.
    pub actions_per_target: HashMap<String, usize>,

    /// Target dependencies.
    pub target_dependencies: HashMap<String, Vec<String>>,

    /// Slow actions (>1 second).
    pub slow_actions: Vec<String>,

    /// Frequently rebuilt targets.
    pub frequent_rebuilds: Vec<String>,
}

/// Optimization suggestion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptimizationSuggestion {
    /// Suggestion category.
    pub category: SuggestionCategory,

    /// Severity level.
    pub severity: SeverityLevel,

    /// Title.
    pub title: String,

    /// Detailed description.
    pub description: String,

    /// Suggested action.
    pub action: String,

    /// Estimated impact.
    pub estimated_impact: ImpactEstimate,

    /// Affected targets.
    pub affected_targets: Vec<String>,
}

/// Suggestion category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SuggestionCategory {
    /// Caching optimization.
    Caching,

    /// Parallelism improvement.
    Parallelism,

    /// Dependency optimization.
    Dependencies,

    /// Action batching.
    Batching,

    /// Resource utilization.
    Resources,

    /// Build graph structure.
    Structure,
}

/// Severity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SeverityLevel {
    /// High impact - should address immediately.
    High,

    /// Medium impact - should address soon.
    Medium,

    /// Low impact - nice to have.
    Low,

    /// Informational only.
    Info,
}

/// Impact estimate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactEstimate {
    /// Estimated time savings (milliseconds).
    pub time_savings_ms: u64,

    /// Confidence level (0.0 - 1.0).
    pub confidence: f64,

    /// Affected action count.
    pub affected_actions: usize,
}

impl OptimizationAnalyzer {
    /// Create a new optimization analyzer.
    pub fn new(metrics: BuildAnalysisMetrics) -> Self {
        OptimizationAnalyzer {
            metrics,
            suggestions: Vec::new(),
        }
    }

    /// Analyze and generate suggestions.
    pub fn analyze(&mut self) -> Result<()> {
        self.analyze_cache_effectiveness();
        self.analyze_parallelism();
        self.analyze_dependencies();
        self.analyze_slow_actions();
        self.analyze_frequent_rebuilds();
        self.analyze_batching_opportunities();

        // Sort by severity
        self.suggestions.sort_by(|a, b| {
            let severity_order = |s: SeverityLevel| match s {
                SeverityLevel::High => 0,
                SeverityLevel::Medium => 1,
                SeverityLevel::Low => 2,
                SeverityLevel::Info => 3,
            };
            severity_order(a.severity).cmp(&severity_order(b.severity))
        });

        Ok(())
    }

    /// Analyze cache effectiveness.
    fn analyze_cache_effectiveness(&mut self) {
        if self.metrics.cache_hit_rate < 50.0 {
            self.suggestions.push(OptimizationSuggestion {
                category: SuggestionCategory::Caching,
                severity: SeverityLevel::High,
                title: "Low cache hit rate detected".to_string(),
                description: format!(
                    "Cache hit rate is {:.1}%, indicating many actions are being re-executed unnecessarily.",
                    self.metrics.cache_hit_rate
                ),
                action: "Review action inputs for non-determinism (timestamps, absolute paths). Enable remote caching if not already enabled.".to_string(),
                estimated_impact: ImpactEstimate {
                    time_savings_ms: ((100.0 - self.metrics.cache_hit_rate) / 100.0
                        * self.metrics.total_actions as f64
                        * self.metrics.avg_action_duration_ms) as u64,
                    confidence: 0.8,
                    affected_actions: (self.metrics.total_actions as f64
                        * (100.0 - self.metrics.cache_hit_rate)
                        / 100.0) as usize,
                },
                affected_targets: vec![],
            });
        }

        if self.metrics.cache_hit_rate >= 50.0 && self.metrics.cache_hit_rate < 80.0 {
            self.suggestions.push(OptimizationSuggestion {
                category: SuggestionCategory::Caching,
                severity: SeverityLevel::Medium,
                title: "Cache hit rate could be improved".to_string(),
                description: format!(
                    "Cache hit rate is {:.1}%. There's room for improvement.",
                    self.metrics.cache_hit_rate
                ),
                action: "Check for sources of non-determinism. Consider using content-addressable storage for better cache keys.".to_string(),
                estimated_impact: ImpactEstimate {
                    time_savings_ms: ((80.0 - self.metrics.cache_hit_rate) / 100.0
                        * self.metrics.total_actions as f64
                        * self.metrics.avg_action_duration_ms) as u64,
                    confidence: 0.6,
                    affected_actions: (self.metrics.total_actions as f64
                        * (80.0 - self.metrics.cache_hit_rate)
                        / 100.0) as usize,
                },
                affected_targets: vec![],
            });
        }
    }

    /// Analyze parallelism.
    fn analyze_parallelism(&mut self) {
        if self.metrics.parallelism_factor < 2.0 {
            self.suggestions.push(OptimizationSuggestion {
                category: SuggestionCategory::Parallelism,
                severity: SeverityLevel::High,
                title: "Low parallelism detected".to_string(),
                description: format!(
                    "Parallelism factor is {:.1}x, indicating builds are mostly sequential.",
                    self.metrics.parallelism_factor
                ),
                action: "Review dependency graph for unnecessary dependencies that serialize execution. Consider splitting large targets.".to_string(),
                estimated_impact: ImpactEstimate {
                    time_savings_ms: (self.metrics.total_actions as f64
                        * self.metrics.avg_action_duration_ms
                        * 0.5) as u64,
                    confidence: 0.7,
                    affected_actions: self.metrics.total_actions,
                },
                affected_targets: vec![],
            });
        }

        if self.metrics.parallelism_factor >= 2.0 && self.metrics.parallelism_factor < 4.0 {
            self.suggestions.push(OptimizationSuggestion {
                category: SuggestionCategory::Parallelism,
                severity: SeverityLevel::Medium,
                title: "Moderate parallelism - room for improvement".to_string(),
                description: format!(
                    "Parallelism factor is {:.1}x. Modern systems can achieve 8-16x with good graph structure.",
                    self.metrics.parallelism_factor
                ),
                action: "Analyze critical path and consider breaking up bottleneck targets.".to_string(),
                estimated_impact: ImpactEstimate {
                    time_savings_ms: (self.metrics.total_actions as f64
                        * self.metrics.avg_action_duration_ms
                        * 0.2) as u64,
                    confidence: 0.5,
                    affected_actions: self.metrics.total_actions / 2,
                },
                affected_targets: vec![],
            });
        }
    }

    /// Analyze dependencies.
    fn analyze_dependencies(&mut self) {
        // Find targets with excessive dependencies
        for (target, deps) in &self.metrics.target_dependencies {
            if deps.len() > 50 {
                self.suggestions.push(OptimizationSuggestion {
                    category: SuggestionCategory::Dependencies,
                    severity: SeverityLevel::Medium,
                    title: format!("Target {} has {} dependencies", target, deps.len()),
                    description: "Large number of dependencies can slow down builds and reduce cache effectiveness.".to_string(),
                    action: "Consider splitting this target into smaller, more focused targets.".to_string(),
                    estimated_impact: ImpactEstimate {
                        time_savings_ms: (deps.len() as f64 * 100.0) as u64,
                        confidence: 0.6,
                        affected_actions: deps.len(),
                    },
                    affected_targets: vec![target.clone()],
                });
            }
        }

        // Detect circular dependencies (simplified)
        let circular = self.detect_circular_dependencies();
        if !circular.is_empty() {
            self.suggestions.push(OptimizationSuggestion {
                category: SuggestionCategory::Structure,
                severity: SeverityLevel::High,
                title: "Circular dependencies detected".to_string(),
                description: format!(
                    "Found {} potential circular dependency chains.",
                    circular.len()
                ),
                action: "Break circular dependencies to enable better parallelism and caching."
                    .to_string(),
                estimated_impact: ImpactEstimate {
                    time_savings_ms: 0,
                    confidence: 1.0,
                    affected_actions: circular.len(),
                },
                affected_targets: circular,
            });
        }
    }

    /// Analyze slow actions.
    fn analyze_slow_actions(&mut self) {
        if !self.metrics.slow_actions.is_empty() {
            self.suggestions.push(OptimizationSuggestion {
                category: SuggestionCategory::Resources,
                severity: SeverityLevel::Medium,
                title: format!("{} slow actions detected", self.metrics.slow_actions.len()),
                description: "Actions taking >1 second dominate build time.".to_string(),
                action: "Consider enabling unity builds, optimizing compiler flags, or using faster linkers (mold, lld).".to_string(),
                estimated_impact: ImpactEstimate {
                    time_savings_ms: (self.metrics.slow_actions.len() as f64 * 500.0) as u64,
                    confidence: 0.5,
                    affected_actions: self.metrics.slow_actions.len(),
                },
                affected_targets: self.metrics.slow_actions.clone(),
            });
        }
    }

    /// Analyze frequent rebuilds.
    fn analyze_frequent_rebuilds(&mut self) {
        if !self.metrics.frequent_rebuilds.is_empty() {
            self.suggestions.push(OptimizationSuggestion {
                category: SuggestionCategory::Caching,
                severity: SeverityLevel::High,
                title: "Targets being rebuilt frequently".to_string(),
                description: format!(
                    "{} targets are being rebuilt more often than expected.",
                    self.metrics.frequent_rebuilds.len()
                ),
                action: "Check if these targets have inputs that change frequently (version stamps, timestamps). Consider isolating volatile inputs.".to_string(),
                estimated_impact: ImpactEstimate {
                    time_savings_ms: (self.metrics.frequent_rebuilds.len() as f64
                        * self.metrics.avg_action_duration_ms) as u64,
                    confidence: 0.7,
                    affected_actions: self.metrics.frequent_rebuilds.len(),
                },
                affected_targets: self.metrics.frequent_rebuilds.clone(),
            });
        }
    }

    /// Analyze batching opportunities.
    fn analyze_batching_opportunities(&mut self) {
        // Find targets with many small actions
        let mut small_action_targets = Vec::new();

        for (target, &action_count) in &self.metrics.actions_per_target {
            if action_count > 10 && self.metrics.avg_action_duration_ms < 100.0 {
                small_action_targets.push(target.clone());
            }
        }

        if !small_action_targets.is_empty() {
            self.suggestions.push(OptimizationSuggestion {
                category: SuggestionCategory::Batching,
                severity: SeverityLevel::Medium,
                title: "Batching opportunities detected".to_string(),
                description: format!(
                    "{} targets with many small actions could benefit from batching.",
                    small_action_targets.len()
                ),
                action: "Enable unity builds or action batching to reduce process spawn overhead."
                    .to_string(),
                estimated_impact: ImpactEstimate {
                    time_savings_ms: (small_action_targets.len() as f64 * 10.0 * 10.0) as u64,
                    confidence: 0.8,
                    affected_actions: small_action_targets
                        .iter()
                        .filter_map(|t| self.metrics.actions_per_target.get(t))
                        .sum(),
                },
                affected_targets: small_action_targets,
            });
        }
    }

    /// Detect circular dependencies (simplified).
    fn detect_circular_dependencies(&self) -> Vec<String> {
        let mut visited = HashSet::new();
        let mut rec_stack = HashSet::new();
        let mut circular = Vec::new();

        for target in self.metrics.target_dependencies.keys() {
            if self.has_cycle(target, &mut visited, &mut rec_stack) {
                circular.push(target.clone());
            }
        }

        circular
    }

    /// Check for cycles using DFS.
    fn has_cycle(
        &self,
        target: &str,
        visited: &mut HashSet<String>,
        rec_stack: &mut HashSet<String>,
    ) -> bool {
        if rec_stack.contains(target) {
            return true;
        }

        if visited.contains(target) {
            return false;
        }

        visited.insert(target.to_string());
        rec_stack.insert(target.to_string());

        if let Some(deps) = self.metrics.target_dependencies.get(target) {
            for dep in deps {
                if self.has_cycle(dep, visited, rec_stack) {
                    return true;
                }
            }
        }

        rec_stack.remove(target);
        false
    }

    /// Get all suggestions.
    pub fn suggestions(&self) -> &[OptimizationSuggestion] {
        &self.suggestions
    }

    /// Get high-severity suggestions only.
    pub fn high_priority_suggestions(&self) -> Vec<&OptimizationSuggestion> {
        self.suggestions
            .iter()
            .filter(|s| s.severity == SeverityLevel::High)
            .collect()
    }

    /// Generate summary report.
    pub fn generate_report(&self) -> String {
        let mut report = String::from("Build Optimization Report\n");
        report.push_str("=========================\n\n");

        let high: Vec<_> = self
            .suggestions
            .iter()
            .filter(|s| s.severity == SeverityLevel::High)
            .collect();

        let medium: Vec<_> = self
            .suggestions
            .iter()
            .filter(|s| s.severity == SeverityLevel::Medium)
            .collect();

        let low: Vec<_> = self
            .suggestions
            .iter()
            .filter(|s| s.severity == SeverityLevel::Low)
            .collect();

        report.push_str(&format!("High Priority: {} suggestions\n", high.len()));
        report.push_str(&format!("Medium Priority: {} suggestions\n", medium.len()));
        report.push_str(&format!("Low Priority: {} suggestions\n\n", low.len()));

        if !high.is_empty() {
            report.push_str("HIGH PRIORITY SUGGESTIONS:\n");
            report.push_str("--------------------------\n");
            for suggestion in high {
                report.push_str(&format!("\n{}\n", suggestion.title));
                report.push_str(&format!("  {}\n", suggestion.description));
                report.push_str(&format!("  Action: {}\n", suggestion.action));
                report.push_str(&format!(
                    "  Estimated savings: {:.1}s\n",
                    suggestion.estimated_impact.time_savings_ms as f64 / 1000.0
                ));
            }
        }

        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_low_cache_hit_rate() {
        let metrics = BuildAnalysisMetrics {
            total_actions: 1000,
            cache_hit_rate: 30.0,
            avg_action_duration_ms: 100.0,
            parallelism_factor: 4.0,
            ..Default::default()
        };

        let mut analyzer = OptimizationAnalyzer::new(metrics);
        analyzer.analyze().unwrap();

        let suggestions = analyzer.suggestions();
        assert!(!suggestions.is_empty());

        let cache_suggestions: Vec<_> = suggestions
            .iter()
            .filter(|s| s.category == SuggestionCategory::Caching)
            .collect();

        assert!(!cache_suggestions.is_empty());
        assert_eq!(cache_suggestions[0].severity, SeverityLevel::High);
    }

    #[test]
    fn test_low_parallelism() {
        let metrics = BuildAnalysisMetrics {
            total_actions: 1000,
            cache_hit_rate: 90.0,
            avg_action_duration_ms: 100.0,
            parallelism_factor: 1.5,
            ..Default::default()
        };

        let mut analyzer = OptimizationAnalyzer::new(metrics);
        analyzer.analyze().unwrap();

        let parallel_suggestions: Vec<_> = analyzer
            .suggestions()
            .iter()
            .filter(|s| s.category == SuggestionCategory::Parallelism)
            .collect();

        assert!(!parallel_suggestions.is_empty());
    }

    #[test]
    fn test_excessive_dependencies() {
        let mut deps = HashMap::new();
        deps.insert(
            "big_target".to_string(),
            (0..100).map(|i| format!("dep_{}", i)).collect(),
        );

        let metrics = BuildAnalysisMetrics {
            total_actions: 1000,
            cache_hit_rate: 90.0,
            avg_action_duration_ms: 100.0,
            parallelism_factor: 4.0,
            target_dependencies: deps,
            ..Default::default()
        };

        let mut analyzer = OptimizationAnalyzer::new(metrics);
        analyzer.analyze().unwrap();

        let dep_suggestions: Vec<_> = analyzer
            .suggestions()
            .iter()
            .filter(|s| s.category == SuggestionCategory::Dependencies)
            .collect();

        assert!(!dep_suggestions.is_empty());
    }
}
