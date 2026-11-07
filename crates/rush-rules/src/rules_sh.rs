use crate::context::BuildContext;
use crate::providers::ProviderSet;
use crate::{
    extract_string, extract_string_list, Artifact, ArtifactType, BuildRule, RuleInstance, Value,
};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;

// ============================================================================
// sh_binary - Shell script executable (stub)
// ============================================================================

pub struct ShBinaryRule;

impl BuildRule for ShBinaryRule {
    fn name(&self) -> &str {
        "sh_binary"
    }

    fn parse_attributes(
        &self,
        attrs: &HashMap<String, crate::Value>,
    ) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let srcs = extract_string_list(attrs, "srcs").unwrap_or_default();
        let deps = extract_string_list(attrs, "deps").unwrap_or_default();
        let data = extract_string_list(attrs, "data").unwrap_or_default();

        Ok(Box::new(ShBinaryInstance {
            name,
            srcs,
            deps,
            data,
        }))
    }
}

pub struct ShBinaryInstance {
    name: String,
    srcs: Vec<String>,
    deps: Vec<String>,
    data: Vec<String>,
}

#[async_trait]
impl RuleInstance for ShBinaryInstance {
    fn rule_type(&self) -> &str {
        // Was the trait default "unknown". The prune planner's admission
        // and its census gate on this string; an unnamed WRITER hiding in
        // the "unknown" bucket would ride along with any future
        // admission of that bucket. Named so gating can be per-kind.
        "sh_binary"
    }

    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        let mut all_deps = self.deps.clone();
        all_deps.extend(self.data.clone());
        all_deps
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        let mut artifacts = Vec::new();

        // Track source files (the script itself)
        for src in &self.srcs {
            let src_path = context.source_path(src);
            artifacts.push(Artifact {
                path: src_path,
                artifact_type: ArtifactType::Source,
            });
        }

        // In a real implementation, we might copy the script to the output directory
        // and make it executable. For now, we just track it.

        Ok(artifacts)
    }

    fn providers(&self) -> ProviderSet {
        ProviderSet::empty()
    }
}

// ============================================================================
// sh_test - Shell script test (stub)
// ============================================================================

pub struct ShTestRule;

impl BuildRule for ShTestRule {
    fn name(&self) -> &str {
        "sh_test"
    }

    fn parse_attributes(
        &self,
        attrs: &HashMap<String, crate::Value>,
    ) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let srcs = extract_string_list(attrs, "srcs").unwrap_or_default();
        let deps = extract_string_list(attrs, "deps").unwrap_or_default();
        let data = extract_string_list(attrs, "data").unwrap_or_default();

        Ok(Box::new(ShTestInstance {
            name,
            srcs,
            deps,
            data,
        }))
    }
}

pub struct ShTestInstance {
    name: String,
    srcs: Vec<String>,
    deps: Vec<String>,
    data: Vec<String>,
}

#[async_trait]
impl RuleInstance for ShTestInstance {
    fn rule_type(&self) -> &str {
        // Was the trait default "unknown". The prune planner's admission
        // and its census gate on this string; an unnamed WRITER hiding in
        // the "unknown" bucket would ride along with any future
        // admission of that bucket. Named so gating can be per-kind.
        "sh_test"
    }

    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        let mut all_deps = self.deps.clone();
        all_deps.extend(self.data.clone());
        all_deps
    }

    async fn build(&self, context: &BuildContext) -> Result<Vec<Artifact>> {
        let mut artifacts = Vec::new();

        // Track source files
        for src in &self.srcs {
            let src_path = context.source_path(src);
            artifacts.push(Artifact {
                path: src_path,
                artifact_type: ArtifactType::Source,
            });
        }

        Ok(artifacts)
    }

    fn providers(&self) -> ProviderSet {
        ProviderSet::empty()
    }
}
