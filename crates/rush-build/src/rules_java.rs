// Rush Build - Java rule graph scaffolding
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Graph-only handling for Java build rules.
//!
//! TensorFlow BUILD files declare Java targets next to host C++ targets that
//! rush has to acknowledge so BUILD evaluation completes. Rush does not
//! produce Java artifacts — this is C++-only scope — so those declarations
//! remain real graph nodes but fail configuration if selected.
//!
//! This is not a placeholder for a future Java builder: the deliberate
//! design is that Java targets can coexist with C++ siblings without an
//! empty-artifact success being mistaken for Java support. See
//! UNIMPLEMENTED.md "Java output artifacts". Tracked in issue #184.

use anyhow::Result;
use async_trait::async_trait;
use rush_rules::context::BuildContext;
use rush_rules::{extract_string, Artifact, BuildRule, RuleInstance, Value};
use std::collections::HashMap;

fn extract_strict_label_list(
    attrs: &HashMap<String, Value>,
    key: &str,
    rule_type: &str,
) -> Result<Vec<String>> {
    let Some(value) = attrs.get(key) else {
        return Ok(Vec::new());
    };
    let Value::List(items) = value else {
        anyhow::bail!("{} requires '{}' to be a list of labels", rule_type, key);
    };
    items
        .iter()
        .enumerate()
        .map(|(index, item)| match item {
            Value::String(label) => Ok(label.clone()),
            _ => anyhow::bail!(
                "{} requires element {} of '{}' to be a label string",
                rule_type,
                index,
                key
            ),
        })
        .collect()
}

/// A Java rule whose declaration and direct `deps` are known, but whose
/// configured-target semantics Rush cannot truthfully produce yet.
///
/// Keeping a real instance lets unrelated C++ siblings load normally.  If the
/// Java target enters the requested closure, `configuration_error()` makes
/// both analysis-only and normal builds fail before any empty-artifact result
/// could be mistaken for success.
pub struct UnsupportedJavaRule {
    rule_type: &'static str,
    reason: &'static str,
}

impl UnsupportedJavaRule {
    pub const fn new(rule_type: &'static str, reason: &'static str) -> Self {
        Self { rule_type, reason }
    }

    pub const fn java_lite_proto_library() -> Self {
        Self::new(
            "java_lite_proto_library",
            "missing Bazel's ProtoInfo aspect, protoc --java_out=lite, Javac, and JavaInfo",
        )
    }

    pub const fn java_proto_library() -> Self {
        Self::new(
            "java_proto_library",
            "missing Bazel's ProtoInfo aspect, protoc --java_out, Javac, and JavaInfo",
        )
    }

    pub const fn java_library() -> Self {
        Self::new(
            "java_library",
            "missing Javac action and JavaInfo semantics",
        )
    }

    pub const fn java_binary() -> Self {
        Self::new(
            "java_binary",
            "missing Java toolchain, Javac, JavaInfo, launcher, and runfiles semantics",
        )
    }

    pub const fn java_import() -> Self {
        Self::new(
            "java_import",
            "missing imported-jar validation and JavaInfo semantics",
        )
    }

    pub const fn java_plugin() -> Self {
        Self::new(
            "java_plugin",
            "missing annotation-processor toolchain and JavaPluginInfo semantics",
        )
    }

    pub const fn java_test() -> Self {
        Self::new(
            "java_test",
            "missing Java toolchain, Javac, JavaInfo, test runner, and runfiles semantics",
        )
    }

    pub const fn all() -> [Self; 7] {
        [
            Self::java_proto_library(),
            Self::java_lite_proto_library(),
            Self::java_library(),
            Self::java_binary(),
            Self::java_import(),
            Self::java_plugin(),
            Self::java_test(),
        ]
    }
}

impl BuildRule for UnsupportedJavaRule {
    fn name(&self) -> &str {
        self.rule_type
    }

    fn parse_attributes(&self, attrs: &HashMap<String, Value>) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let deps = extract_strict_label_list(attrs, "deps", self.rule_type)?;

        Ok(Box::new(UnsupportedJavaInstance {
            name,
            deps,
            rule_type: self.rule_type,
            error: format!(
                "{} is known but unsupported: {}",
                self.rule_type, self.reason
            ),
        }))
    }

    fn documentation(&self) -> &str {
        "Known Java target retained for graph fidelity; selecting it fails at configuration time"
    }
}

struct UnsupportedJavaInstance {
    name: String,
    deps: Vec<String>,
    rule_type: &'static str,
    error: String,
}

#[async_trait]
impl RuleInstance for UnsupportedJavaInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        self.deps.clone()
    }

    fn configuration_error(&self) -> Option<&str> {
        Some(&self.error)
    }

    async fn build(&self, _context: &BuildContext) -> Result<Vec<Artifact>> {
        anyhow::bail!(
            "target '{}' cannot be configured: {}",
            self.name,
            self.error
        )
    }

    fn providers(&self) -> rush_rules::providers::ProviderSet {
        rush_rules::providers::ProviderSet::empty()
    }

    fn rule_type(&self) -> &str {
        self.rule_type
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_java_rule_retains_graph_and_fails_configuration() {
        let rule = UnsupportedJavaRule::new(
            "java_lite_proto_library",
            "missing ProtoInfo aspect, protoc --java_out=lite, Javac, and JavaInfo",
        );
        let attrs = HashMap::from([
            ("name".to_string(), Value::String("lite".to_string())),
            (
                "deps".to_string(),
                Value::List(vec![Value::String(":proto".to_string())]),
            ),
        ]);
        let instance = rule.parse_attributes(&attrs).unwrap();

        assert_eq!(instance.target_name(), "lite");
        assert_eq!(instance.dependencies(), vec![":proto"]);
        assert_eq!(instance.rule_type(), "java_lite_proto_library");
        let error = instance
            .configuration_error()
            .expect("known-unsupported target must fail when selected");
        assert!(error.contains("ProtoInfo aspect"));
        assert!(error.contains("protoc --java_out=lite"));
        assert!(error.contains("JavaInfo"));
    }

    #[test]
    fn unsupported_java_rule_rejects_malformed_dependency_lists() {
        let rule = UnsupportedJavaRule::java_library();
        let attrs = HashMap::from([
            ("name".to_string(), Value::String("java".to_string())),
            ("deps".to_string(), Value::List(vec![Value::Int(7)])),
        ]);

        let error = rule
            .parse_attributes(&attrs)
            .err()
            .expect("invalid deps must fail");
        assert!(error
            .to_string()
            .contains("requires element 0 of 'deps' to be a label string"));

        for value in [
            Value::None,
            Value::Dict(HashMap::new()),
            Value::String("select({...})".to_string()),
        ] {
            let attrs = HashMap::from([
                ("name".to_string(), Value::String("java".to_string())),
                ("deps".to_string(), value),
            ]);
            let error = rule
                .parse_attributes(&attrs)
                .err()
                .expect("a non-list deps value must fail");
            assert!(error
                .to_string()
                .contains("requires 'deps' to be a list of labels"));
        }
    }

    #[tokio::test]
    async fn every_loaded_java_rule_fails_loud_if_selected() {
        let context = BuildContext::new(
            std::path::PathBuf::from("/tmp/rush-java-test-workspace"),
            std::path::PathBuf::from("/tmp/rush-java-test-output"),
        );
        for rule in UnsupportedJavaRule::all() {
            let rule_type = rule.name().to_string();
            let instance = rule
                .parse_attributes(&HashMap::from([(
                    "name".to_string(),
                    Value::String("java_target".to_string()),
                )]))
                .unwrap();
            assert_eq!(instance.rule_type(), rule_type);
            assert!(instance.configuration_error().is_some());
            assert!(instance
                .build(&context)
                .await
                .unwrap_err()
                .to_string()
                .contains("cannot be configured"));
        }
    }
}
