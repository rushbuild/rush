use anyhow::Result;
use async_trait::async_trait;
use rush_rules::{
    context::BuildContext, extract_string, extract_string_list, extract_string_opt,
    providers::ProviderSet, Artifact, ArtifactType, BuildRule, RuleInstance, Value,
};
use std::collections::HashMap;
use std::path::PathBuf;

/// filegroup() rule - groups files together
///
/// Example:
/// ```python
/// filegroup(
///     name = "core_headers",
///     srcs = ["arena.h", "bitmap.h"],
///     visibility = ["//visibility:public"],
/// )
/// ```
pub struct FilegroupRule;

#[async_trait]
impl BuildRule for FilegroupRule {
    fn name(&self) -> &str {
        "filegroup"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let srcs = extract_string_list(attrs, "srcs").unwrap_or_default();
        let visibility = extract_string_opt(attrs, "visibility");

        Ok(Box::new(FilegroupInstance {
            name,
            srcs,
            visibility,
        }))
    }

    fn documentation(&self) -> &str {
        "Groups files together for use as dependencies"
    }
}

struct FilegroupInstance {
    name: String,
    srcs: Vec<String>,
    #[allow(dead_code)]
    visibility: Option<String>,
}

#[async_trait]
impl RuleInstance for FilegroupInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        // filegroups can reference other filegroups as srcs
        // We need to extract any labels (starting with : or //)
        self.srcs
            .iter()
            .filter(|src| src.starts_with(':') || src.starts_with("//"))
            .cloned()
            .collect()
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        let mut artifacts = Vec::new();

        // Process each source file
        for src in &self.srcs {
            // Skip labels - they'll be resolved via dependencies
            if src.starts_with(':') || src.starts_with("//") || src.starts_with('@') {
                // This is a dependency reference, not a file
                // Get artifacts from the dependency
                if let Some(dep_artifacts) = context.direct_artifacts(src) {
                    artifacts.extend(dep_artifacts.iter().cloned());
                }
                continue;
            }

            // Regular file - add it as a source artifact
            let file_path = context.source_path(src);

            // Determine artifact type based on extension
            let artifact_type = if src.ends_with(".h") || src.ends_with(".hpp") {
                ArtifactType::Header
            } else if src.ends_with(".cc") || src.ends_with(".cpp") || src.ends_with(".c") {
                ArtifactType::Source
            } else {
                ArtifactType::Generated // Generic type for other files
            };

            artifacts.push(Artifact {
                path: file_path,
                artifact_type,
            });
        }

        Ok(artifacts)
    }

    fn providers(&self) -> ProviderSet {
        let files: Vec<PathBuf> = self
            .srcs
            .iter()
            .filter(|src| !src.starts_with(':') && !src.starts_with("//") && !src.starts_with('@'))
            .map(PathBuf::from)
            .collect();
        ProviderSet::empty().with_files(rush_rules::providers::FilesInfo::new(files))
    }

    /// Produce a `FilesInfo` whose member paths are ABSOLUTE (qualified by
    /// THIS filegroup's package), not the bare names `providers()` returns.
    ///
    /// Consumers expand a `filegroup` label in `srcs`/`data` via
    /// `expand_filegroup_srcs`, which only splices ABSOLUTE paths (a relative
    /// name would be wrongly re-resolved against the CONSUMER's package). With
    /// bare names a cross-package filegroup src (e.g. a tf_cc_test whose
    /// srcs=["//tensorflow/core/lib/strings:legacy_strings_proto_serialization_test"])
    /// stayed a label and reached compute_id as a literal path → "open input
    /// //...:...: No such file". `ctx.package()`/`workspace_root()` give the
    /// package context `providers()` lacks. Nested filegroup/label members are
    /// left to the caller's transitive expansion.
    fn analyze(
        &self,
        ctx: &mut rush_rules::analysis_context::AnalysisContext,
        _dep_providers: &[std::sync::Arc<ProviderSet>],
    ) -> ProviderSet {
        let ws = ctx.workspace_root().to_path_buf();
        let pkg = ctx.package().to_string();
        let files: Vec<PathBuf> = self
            .srcs
            .iter()
            .filter(|src| !src.starts_with(':') && !src.starts_with("//") && !src.starts_with('@'))
            .map(|src| {
                if pkg.is_empty() {
                    ws.join(src)
                } else {
                    ws.join(&pkg).join(src)
                }
            })
            .collect();
        ProviderSet::empty().with_files(rush_rules::providers::FilesInfo::new(files))
    }

    fn rule_type(&self) -> &str {
        "filegroup"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_filegroup() {
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("test_files".to_string()));
        attrs.insert(
            "srcs".to_string(),
            Value::List(vec![
                Value::String("test.h".to_string()),
                Value::String("test.cc".to_string()),
            ]),
        );

        let rule = FilegroupRule;
        let instance = rule.parse_attributes(&attrs).unwrap();

        assert_eq!(instance.target_name(), "test_files");
        assert_eq!(instance.dependencies().len(), 0);
    }

    #[test]
    fn test_filegroup_with_label_deps() {
        let mut attrs = HashMap::new();
        attrs.insert(
            "name".to_string(),
            Value::String("combined_files".to_string()),
        );
        attrs.insert(
            "srcs".to_string(),
            Value::List(vec![
                Value::String("local.h".to_string()),
                Value::String(":other_group".to_string()),
                Value::String("//package:remote_group".to_string()),
            ]),
        );

        let rule = FilegroupRule;
        let instance = rule.parse_attributes(&attrs).unwrap();

        let deps = instance.dependencies();
        assert_eq!(deps.len(), 2);
        assert!(deps.contains(&":other_group".to_string()));
        assert!(deps.contains(&"//package:remote_group".to_string()));
    }
}
