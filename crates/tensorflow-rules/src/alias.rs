use anyhow::Result;
use async_trait::async_trait;
use rush_rules::{
    context::BuildContext, extract_string, extract_string_opt, providers::ProviderSet, Artifact,
    BuildRule, RuleInstance, Value,
};
use std::collections::HashMap;

/// alias() rule - creates a redirect to another target
///
/// Example:
/// ```python
/// alias(
///     name = "reader_base",
///     actual = "//tensorflow/core/framework:reader_base",
///     visibility = ["//visibility:public"],
/// )
/// ```
pub struct AliasRule;

#[async_trait]
impl BuildRule for AliasRule {
    fn name(&self) -> &str {
        "alias"
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let actual = extract_string(attrs, "actual")?;
        let visibility = extract_string_opt(attrs, "visibility");

        Ok(Box::new(AliasInstance {
            name,
            actual,
            visibility,
        }))
    }

    fn documentation(&self) -> &str {
        "Creates a redirect to another target"
    }
}

struct AliasInstance {
    name: String,
    actual: String,
    #[allow(dead_code)]
    visibility: Option<String>,
}

#[async_trait]
impl RuleInstance for AliasInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        // The alias depends on the actual target
        vec![self.actual.clone()]
    }

    async fn build(&self, _context: &BuildContext) -> Result<Vec<Artifact>> {
        // This is correct behavior: aliases produce nothing of
        // their own. Patch set 2b: the orchestrator detects
        // rule_type == "alias" and stores
        // built_artifacts[alias_label] = Arc::clone(built_artifacts[
        // actual_target]) so consumers querying the alias get the
        // actual target's NestedSet identity (direct AND transitive).
        Ok(Vec::new())
    }

    fn providers(&self) -> ProviderSet {
        // Alias forwards providers from the actual target. The
        // orchestrator detects rule_type == "alias" and merges the
        // aliased target's ProviderSet into ours via Arc-clone.
        ProviderSet::empty()
    }

    fn is_configuration_alias(&self) -> bool {
        true
    }

    fn rule_type(&self) -> &str {
        "alias"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_alias() {
        let mut attrs = HashMap::new();
        attrs.insert("name".to_string(), Value::String("my_alias".to_string()));
        attrs.insert("actual".to_string(), Value::String("//foo:bar".to_string()));

        let rule = AliasRule;
        let instance = rule.parse_attributes(&attrs).unwrap();

        assert_eq!(instance.target_name(), "my_alias");

        let deps = instance.dependencies();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0], "//foo:bar");
    }

    #[test]
    fn test_alias_with_visibility() {
        let mut attrs = HashMap::new();
        attrs.insert(
            "name".to_string(),
            Value::String("public_alias".to_string()),
        );
        attrs.insert(
            "actual".to_string(),
            Value::String(":private_target".to_string()),
        );
        attrs.insert(
            "visibility".to_string(),
            Value::String("//visibility:public".to_string()),
        );

        let rule = AliasRule;
        let instance = rule.parse_attributes(&attrs).unwrap();

        assert_eq!(instance.target_name(), "public_alias");
        assert_eq!(instance.dependencies()[0], ":private_target");
    }

    #[test]
    fn test_alias_missing_actual() {
        let mut attrs = HashMap::new();
        attrs.insert(
            "name".to_string(),
            Value::String("broken_alias".to_string()),
        );

        let rule = AliasRule;
        let result = rule.parse_attributes(&attrs);

        assert!(result.is_err());
    }
}
