use crate::BuildRule;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::Path;

/// Registry of build rules
pub struct RuleRegistry {
    rules: HashMap<String, Box<dyn BuildRule>>,
}

impl RuleRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self {
            rules: HashMap::new(),
        }
    }

    /// Register a build rule
    pub fn register(&mut self, rule: Box<dyn BuildRule>) {
        let name = rule.name().to_string();
        self.rules.insert(name, rule);
    }

    /// Get a rule by name
    pub fn get(&self, name: &str) -> Option<&dyn BuildRule> {
        self.rules.get(name).map(|r| r.as_ref())
    }

    /// Check if a rule exists
    pub fn has_rule(&self, name: &str) -> bool {
        self.rules.contains_key(name)
    }

    /// List all registered rule names
    pub fn list_rules(&self) -> Vec<String> {
        self.rules.keys().cloned().collect()
    }

    /// Load a plugin from a dynamic library
    ///
    /// Note: This is a placeholder for future plugin loading support.
    /// Actual implementation would use libloading or similar.
    pub fn load_plugin(&mut self, _plugin_path: &Path) -> Result<()> {
        Err(anyhow!("Plugin loading not yet implemented"))
    }
}

impl Default for RuleRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::BuildContext;
    use crate::providers::ProviderSet;
    use crate::Artifact;
    use crate::{BuildRule, RuleInstance, Value};
    use async_trait::async_trait;
    use std::collections::HashMap;

    // Mock rule for testing
    struct MockRule;

    #[async_trait]
    impl BuildRule for MockRule {
        fn name(&self) -> &str {
            "mock_rule"
        }

        fn parse_attributes(
            &self,
            _attrs: &HashMap<String, Value>,
        ) -> Result<Box<dyn RuleInstance>> {
            Ok(Box::new(MockRuleInstance))
        }
    }

    struct MockRuleInstance;

    #[async_trait]
    impl RuleInstance for MockRuleInstance {
        fn target_name(&self) -> &str {
            "mock_target"
        }

        fn dependencies(&self) -> Vec<String> {
            Vec::new()
        }

        async fn build(&self, _context: &BuildContext) -> Result<Vec<Artifact>> {
            // This is correct behavior: test mock with no real
            // build action; the rule registry exercises only
            // registration/lookup, not execution.
            Ok(Vec::new())
        }

        fn providers(&self) -> ProviderSet {
            ProviderSet::empty()
        }
    }

    #[test]
    fn test_registry_registration() {
        let mut registry = RuleRegistry::new();
        assert!(!registry.has_rule("mock_rule"));

        registry.register(Box::new(MockRule));
        assert!(registry.has_rule("mock_rule"));

        let rule = registry.get("mock_rule");
        assert!(rule.is_some());
        assert_eq!(rule.unwrap().name(), "mock_rule");
    }

    #[test]
    fn test_list_rules() {
        let mut registry = RuleRegistry::new();
        registry.register(Box::new(MockRule));

        let rules = registry.list_rules();
        assert_eq!(rules.len(), 1);
        assert!(rules.contains(&"mock_rule".to_string()));
    }

    #[test]
    fn test_get_nonexistent_rule() {
        let registry = RuleRegistry::new();
        assert!(registry.get("nonexistent").is_none());
    }
}
