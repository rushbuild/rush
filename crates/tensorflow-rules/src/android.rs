use anyhow::{anyhow, Result};
use async_trait::async_trait;
use rush_rules::context::BuildContext;
use rush_rules::providers::ProviderSet;
use rush_rules::{extract_string, Artifact, BuildRule, RuleInstance, Value};
use std::collections::HashMap;

/// A loaded Android rule whose configured-target semantics Rush cannot honor.
///
/// TensorFlow's host-only graph contains Android targets beside ordinary C++
/// targets. Keeping the declaration as a real graph node lets unrequested
/// siblings load, while `configuration_error()` makes any selected or
/// transitively reachable Android target fail before analysis or execution.
pub struct UnsupportedAndroidRule {
    pub rule_type: &'static str,
}

impl BuildRule for UnsupportedAndroidRule {
    fn name(&self) -> &str {
        self.rule_type
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let error = format!(
            "Rush does not implement {kind}: Android toolchain selection, \
             resource and manifest processing, and Android packaging semantics \
             are unsupported",
            kind = self.rule_type,
        );
        Ok(Box::new(UnsupportedAndroidInstance {
            name,
            rule_type: self.rule_type,
            error,
        }))
    }
}

struct UnsupportedAndroidInstance {
    name: String,
    rule_type: &'static str,
    error: String,
}

#[async_trait]
impl RuleInstance for UnsupportedAndroidInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        Vec::new()
    }

    fn configuration_error(&self) -> Option<&str> {
        Some(&self.error)
    }

    async fn build(&self, _context: &BuildContext) -> Result<Vec<Artifact>> {
        Err(anyhow!(
            "target '{}' cannot be configured: {}",
            self.name,
            self.error
        ))
    }

    fn providers(&self) -> ProviderSet {
        ProviderSet::empty()
    }

    fn rule_type(&self) -> &str {
        self.rule_type
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn android_rules_are_retained_as_fail_loud_targets() {
        for rule_type in ["android_library", "android_binary", "aar_import"] {
            let rule = UnsupportedAndroidRule { rule_type };
            let instance = rule
                .parse_attributes(&HashMap::from([(
                    "name".to_string(),
                    Value::String("mobile_target".to_string()),
                )]))
                .unwrap();

            assert_eq!(instance.target_name(), "mobile_target");
            assert_eq!(instance.rule_type(), rule_type);
            let error = instance
                .configuration_error()
                .expect("Android targets must carry a deferred error");
            assert!(error.contains(rule_type), "{error}");
            assert!(error.contains("Android toolchain selection"), "{error}");

            let context = BuildContext::new(
                std::path::PathBuf::from("/tmp/rush-android-test-workspace"),
                std::path::PathBuf::from("/tmp/rush-android-test-output"),
            );
            let error = instance.build(&context).await.unwrap_err().to_string();
            assert!(error.contains("cannot be configured"), "{error}");
            assert!(error.contains(rule_type), "{error}");
        }
    }
}
