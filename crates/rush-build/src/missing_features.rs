/// Missing Bazel Features Tracker
///
/// This module tracks missing Bazel features (repositories, providers, symbols)
/// encountered during TensorFlow builds and outputs them in a structured format
/// for systematic implementation.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

/// Types of missing Bazel features
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MissingFeatureKind {
    /// External repository (e.g., @rules_python)
    ExternalRepository,
    /// Provider (e.g., CcInfo, PyInfo)
    Provider,
    /// Bazel built-in function (e.g., rule(), provider())
    BuiltinFunction,
    /// Namespace (e.g., cc_common, proto_common_do_not_use)
    Namespace,
    /// Symbol from .bzl file (e.g., function, variable)
    Symbol,
}

/// Information about a missing feature
#[derive(Debug, Clone)]
pub struct MissingFeature {
    pub kind: MissingFeatureKind,
    pub name: String,
    pub context: String,  // Where it was referenced (file path, error message)
    pub count: usize,     // How many times encountered
}

/// Global tracker for missing features
#[derive(Debug, Clone)]
pub struct MissingFeaturesTracker {
    features: Arc<Mutex<HashMap<(MissingFeatureKind, String), MissingFeature>>>,
}

impl Default for MissingFeaturesTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl MissingFeaturesTracker {
    /// Create a new tracker
    pub fn new() -> Self {
        Self {
            features: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Record a missing feature
    pub fn record(
        &self,
        kind: MissingFeatureKind,
        name: String,
        context: String,
    ) {
        let mut features = self.features.lock().unwrap();
        let key = (kind.clone(), name.clone());

        features.entry(key)
            .and_modify(|f| {
                f.count += 1;
                // Keep the first context for simplicity
            })
            .or_insert(MissingFeature {
                kind,
                name,
                context,
                count: 1,
            });
    }

    /// Get all missing features grouped by kind
    pub fn get_grouped(&self) -> HashMap<MissingFeatureKind, Vec<MissingFeature>> {
        let features = self.features.lock().unwrap();
        let mut grouped: HashMap<MissingFeatureKind, Vec<MissingFeature>> = HashMap::new();

        for feature in features.values() {
            grouped
                .entry(feature.kind.clone())
                .or_default()
                .push(feature.clone());
        }

        // Sort each group by count (descending) then name
        for features_list in grouped.values_mut() {
            features_list.sort_by(|a, b| {
                b.count.cmp(&a.count)
                    .then_with(|| a.name.cmp(&b.name))
            });
        }

        grouped
    }

    /// Get all missing external repositories
    pub fn get_missing_repos(&self) -> Vec<String> {
        let features = self.features.lock().unwrap();
        let mut repos: Vec<String> = features
            .values()
            .filter(|f| f.kind == MissingFeatureKind::ExternalRepository)
            .map(|f| f.name.clone())
            .collect();
        repos.sort();
        repos.dedup();
        repos
    }

    /// Get all missing providers
    pub fn get_missing_providers(&self) -> Vec<String> {
        let features = self.features.lock().unwrap();
        let mut providers: Vec<String> = features
            .values()
            .filter(|f| f.kind == MissingFeatureKind::Provider)
            .map(|f| f.name.clone())
            .collect();
        providers.sort();
        providers.dedup();
        providers
    }

    /// Clear all tracked features
    pub fn clear(&self) {
        let mut features = self.features.lock().unwrap();
        features.clear();
    }

    /// Get total count of missing features
    pub fn count(&self) -> usize {
        let features = self.features.lock().unwrap();
        features.len()
    }

    /// Generate a markdown report of missing features
    pub fn generate_report(&self) -> String {
        let grouped = self.get_grouped();
        let mut report = String::new();

        report.push_str("# Missing Bazel Features Report\n\n");
        report.push_str("This report lists all missing Bazel features encountered during the TensorFlow build.\n\n");

        // Summary
        report.push_str("## Summary\n\n");
        report.push_str(&format!("**Total unique missing features**: {}\n\n", self.count()));

        for (kind, features) in &grouped {
            report.push_str(&format!("- **{:?}**: {} items\n", kind, features.len()));
        }
        report.push_str("\n---\n\n");

        // Detailed listings
        let order = vec![
            MissingFeatureKind::ExternalRepository,
            MissingFeatureKind::Provider,
            MissingFeatureKind::Namespace,
            MissingFeatureKind::BuiltinFunction,
            MissingFeatureKind::Symbol,
        ];

        for kind in order {
            if let Some(features) = grouped.get(&kind) {
                report.push_str(&format!("## {:?}\n\n", kind));
                report.push_str("| Name | Count | First Seen In |\n");
                report.push_str("|------|-------|---------------|\n");

                for feature in features {
                    // Truncate context if too long
                    let context = if feature.context.len() > 80 {
                        format!("{}...", &feature.context[..77])
                    } else {
                        feature.context.clone()
                    };
                    report.push_str(&format!(
                        "| `{}` | {} | {} |\n",
                        feature.name, feature.count, context
                    ));
                }
                report.push_str("\n");
            }
        }

        report.push_str("---\n\n");
        report.push_str("## Implementation Priority\n\n");
        report.push_str("Features are sorted by occurrence count (most frequently needed first).\n\n");

        // Get top 10 most frequently encountered
        let features = self.features.lock().unwrap();
        let mut all_features: Vec<_> = features.values().collect();
        all_features.sort_by(|a, b| b.count.cmp(&a.count));

        report.push_str("### Top 10 Most Needed\n\n");
        for (i, feature) in all_features.iter().take(10).enumerate() {
            report.push_str(&format!(
                "{}. **`{}`** ({:?}) - encountered {} times\n",
                i + 1, feature.name, feature.kind, feature.count
            ));
        }

        report
    }

    /// Output the report to a file
    pub fn write_report(&self, path: &std::path::Path) -> std::io::Result<()> {
        use std::io::Write;
        let report = self.generate_report();
        let mut file = std::fs::File::create(path)?;
        file.write_all(report.as_bytes())?;
        Ok(())
    }
}

/// Extract missing repository name from error message
pub fn extract_missing_repo(error_msg: &str) -> Option<String> {
    // Pattern: "External repository 'repo_name' not found"
    if let Some(start) = error_msg.find("External repository '") {
        let rest = &error_msg[start + 21..];
        if let Some(end) = rest.find('\'') {
            return Some(rest[..end].to_string());
        }
    }
    None
}

/// Extract undefined variable name from error message
pub fn extract_undefined_var(error_msg: &str) -> Option<String> {
    // Pattern: "Undefined variable: var_name"
    if let Some(start) = error_msg.find("Undefined variable: ") {
        let rest = &error_msg[start + 20..];
        // Take until first whitespace or end of string
        let var_name = rest.split_whitespace().next()
            .unwrap_or(rest)
            .trim_end_matches(|c: char| !c.is_alphanumeric() && c != '_');
        return Some(var_name.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_missing_repo() {
        let msg = "External repository 'rules_python' not found";
        assert_eq!(extract_missing_repo(msg), Some("rules_python".to_string()));
    }

    #[test]
    fn test_extract_undefined_var() {
        let msg = "Undefined variable: CcInfo";
        assert_eq!(extract_undefined_var(msg), Some("CcInfo".to_string()));
    }

    #[test]
    fn test_tracker() {
        let tracker = MissingFeaturesTracker::new();

        tracker.record(
            MissingFeatureKind::ExternalRepository,
            "rules_python".to_string(),
            "BUILD file".to_string(),
        );

        tracker.record(
            MissingFeatureKind::Provider,
            "CcInfo".to_string(),
            "cc/defs.bzl".to_string(),
        );

        assert_eq!(tracker.count(), 2);
        assert_eq!(tracker.get_missing_repos(), vec!["rules_python"]);
        assert_eq!(tracker.get_missing_providers(), vec!["CcInfo"]);
    }
}
