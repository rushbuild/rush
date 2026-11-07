// Rush Build - Custom Rule Framework
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Custom rule framework for extensibility.
//!
//! Allows users to define custom build rules with their own semantics,
//! attributes, and action generation logic. Rules can be composed and
//! extended to create powerful build abstractions.

use anyhow::{Context, Result};
use rush_action::{Action, Artifact, Command};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Type alias for action generator function
type ActionGeneratorFn = Box<
    dyn Fn(&Target, &HashMap<String, AttributeValue>, &RuleContext) -> Result<Vec<Action>>
        + Send
        + Sync,
>;

/// Type alias for macro expansion result
type MacroExpansion = Result<Vec<(String, String, HashMap<String, AttributeValue>)>>;

/// Custom rule definition trait.
pub trait CustomRule: Send + Sync {
    /// Rule name (e.g., "my_custom_library").
    fn name(&self) -> &str;

    /// Rule attributes schema.
    fn attributes(&self) -> Vec<AttributeSchema>;

    /// Generate actions for this rule.
    fn generate_actions(
        &self,
        target: &Target,
        attrs: &HashMap<String, AttributeValue>,
        ctx: &RuleContext,
    ) -> Result<Vec<Action>>;

    /// Validate attributes.
    fn validate_attrs(&self, attrs: &HashMap<String, AttributeValue>) -> Result<()> {
        for schema in self.attributes() {
            if schema.required && !attrs.contains_key(&schema.name) {
                anyhow::bail!("Required attribute '{}' not found", schema.name);
            }

            if let Some(value) = attrs.get(&schema.name) {
                if !value.matches_type(&schema.attr_type) {
                    anyhow::bail!(
                        "Attribute '{}' has wrong type (expected {:?})",
                        schema.name,
                        schema.attr_type
                    );
                }
            }
        }

        Ok(())
    }

    /// Rule dependencies (other rules this extends/composes).
    fn dependencies(&self) -> Vec<String> {
        vec![]
    }

    /// Rule outputs (what this rule produces).
    fn outputs(
        &self,
        _target: &Target,
        _attrs: &HashMap<String, AttributeValue>,
    ) -> Result<Vec<Artifact>> {
        Ok(vec![])
    }
}

/// Attribute schema definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttributeSchema {
    /// Attribute name.
    pub name: String,

    /// Attribute type.
    pub attr_type: AttributeType,

    /// Is this attribute required?
    pub required: bool,

    /// Default value.
    pub default: Option<AttributeValue>,

    /// Documentation.
    pub doc: Option<String>,
}

/// Attribute type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AttributeType {
    /// String value.
    String,

    /// Boolean value.
    Bool,

    /// Integer value.
    Int,

    /// List of strings.
    StringList,

    /// Label (target reference).
    Label,

    /// List of labels.
    LabelList,

    /// File path.
    Path,

    /// List of file paths.
    PathList,

    /// Dictionary/map.
    Dict,

    /// Custom type.
    Custom(String),
}

/// Attribute value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AttributeValue {
    /// String value.
    String(String),

    /// Boolean value.
    Bool(bool),

    /// Integer value.
    Int(i64),

    /// List of strings.
    StringList(Vec<String>),

    /// Label reference.
    Label(String),

    /// List of labels.
    LabelList(Vec<String>),

    /// File path.
    Path(PathBuf),

    /// List of paths.
    PathList(Vec<PathBuf>),

    /// Dictionary.
    Dict(HashMap<String, AttributeValue>),
}

impl AttributeValue {
    /// Check if value matches expected type.
    pub fn matches_type(&self, attr_type: &AttributeType) -> bool {
        matches!(
            (self, attr_type),
            (AttributeValue::String(_), AttributeType::String)
                | (AttributeValue::Bool(_), AttributeType::Bool)
                | (AttributeValue::Int(_), AttributeType::Int)
                | (AttributeValue::StringList(_), AttributeType::StringList)
                | (AttributeValue::Label(_), AttributeType::Label)
                | (AttributeValue::LabelList(_), AttributeType::LabelList)
                | (AttributeValue::Path(_), AttributeType::Path)
                | (AttributeValue::PathList(_), AttributeType::PathList)
                | (AttributeValue::Dict(_), AttributeType::Dict)
        )
    }

    /// Get as string.
    pub fn as_string(&self) -> Option<&str> {
        match self {
            AttributeValue::String(s) => Some(s),
            _ => None,
        }
    }

    /// Get as bool.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            AttributeValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Get as int.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            AttributeValue::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// Get as string list.
    pub fn as_string_list(&self) -> Option<&Vec<String>> {
        match self {
            AttributeValue::StringList(l) => Some(l),
            _ => None,
        }
    }

    /// Get as label list.
    pub fn as_label_list(&self) -> Option<&Vec<String>> {
        match self {
            AttributeValue::LabelList(l) => Some(l),
            _ => None,
        }
    }

    /// Get as path.
    pub fn as_path(&self) -> Option<&PathBuf> {
        match self {
            AttributeValue::Path(p) => Some(p),
            _ => None,
        }
    }

    /// Get as path list.
    pub fn as_path_list(&self) -> Option<&Vec<PathBuf>> {
        match self {
            AttributeValue::PathList(l) => Some(l),
            _ => None,
        }
    }
}

/// Target definition.
#[derive(Debug, Clone)]
pub struct Target {
    /// Target label (e.g., "//pkg:name").
    pub label: String,

    /// Target name.
    pub name: String,

    /// Target package.
    pub package: String,

    /// Rule kind.
    pub rule_kind: String,
}

/// Rule execution context.
pub struct RuleContext {
    /// Workspace root.
    pub workspace_root: PathBuf,

    /// Output directory.
    pub output_dir: PathBuf,

    /// Target configuration.
    pub config: BuildConfig,

    /// Toolchain registry.
    pub toolchains: Arc<ToolchainRegistry>,
}

/// Build configuration.
#[derive(Debug, Clone)]
pub struct BuildConfig {
    /// Compilation mode (opt, dbg, fastbuild).
    pub compilation_mode: CompilationMode,

    /// Target platform.
    pub target_platform: String,

    /// CPU architecture.
    pub cpu: String,

    /// Additional flags.
    pub flags: HashMap<String, String>,
}

/// Compilation mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CompilationMode {
    /// Optimized build.
    Opt,

    /// Debug build.
    Dbg,

    /// Fast build (minimal optimization).
    Fastbuild,
}

/// Toolchain registry for rules.
pub struct ToolchainRegistry {
    toolchains: HashMap<String, Box<dyn Toolchain>>,
}

impl ToolchainRegistry {
    /// Create new toolchain registry.
    pub fn new() -> Self {
        ToolchainRegistry {
            toolchains: HashMap::new(),
        }
    }

    /// Register a toolchain.
    pub fn register(&mut self, name: String, toolchain: Box<dyn Toolchain>) {
        self.toolchains.insert(name, toolchain);
    }

    /// Get toolchain.
    pub fn get(&self, name: &str) -> Option<&dyn Toolchain> {
        self.toolchains.get(name).map(|t| t.as_ref())
    }
}

impl Default for ToolchainRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Toolchain trait for custom rules.
pub trait Toolchain: Send + Sync {
    /// Toolchain name.
    fn name(&self) -> &str;

    /// Get compiler path.
    fn compiler(&self) -> Result<PathBuf>;

    /// Get linker path.
    fn linker(&self) -> Result<PathBuf>;

    /// Get standard library path.
    fn sysroot(&self) -> Result<PathBuf>;

    /// Get additional flags.
    fn flags(&self) -> Vec<String>;
}

/// Custom rule registry.
pub struct CustomRuleRegistry {
    /// Registered rules.
    rules: HashMap<String, Arc<dyn CustomRule>>,
}

impl CustomRuleRegistry {
    /// Create new rule registry.
    pub fn new() -> Self {
        CustomRuleRegistry {
            rules: HashMap::new(),
        }
    }

    /// Register a custom rule.
    pub fn register(&mut self, rule: Arc<dyn CustomRule>) {
        self.rules.insert(rule.name().to_string(), rule);
    }

    /// Get rule by name.
    pub fn get(&self, name: &str) -> Option<Arc<dyn CustomRule>> {
        self.rules.get(name).cloned()
    }

    /// List all registered rules.
    pub fn list_rules(&self) -> Vec<String> {
        self.rules.keys().cloned().collect()
    }

    /// Apply rule to target.
    pub fn apply_rule(
        &self,
        rule_name: &str,
        target: &Target,
        attrs: &HashMap<String, AttributeValue>,
        ctx: &RuleContext,
    ) -> Result<Vec<Action>> {
        let rule = self
            .get(rule_name)
            .context(format!("Rule not found: {}", rule_name))?;

        // Validate attributes
        rule.validate_attrs(attrs)?;

        // Generate actions
        rule.generate_actions(target, attrs, ctx)
    }
}

impl Default for CustomRuleRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Rule builder for easier rule creation.
pub struct RuleBuilder {
    name: String,
    attributes: Vec<AttributeSchema>,
    action_generator: Option<ActionGeneratorFn>,
}

impl RuleBuilder {
    /// Create new rule builder.
    pub fn new(name: impl Into<String>) -> Self {
        RuleBuilder {
            name: name.into(),
            attributes: Vec::new(),
            action_generator: None,
        }
    }

    /// Add string attribute.
    pub fn attr_string(mut self, name: impl Into<String>, required: bool) -> Self {
        self.attributes.push(AttributeSchema {
            name: name.into(),
            attr_type: AttributeType::String,
            required,
            default: None,
            doc: None,
        });
        self
    }

    /// Add bool attribute.
    pub fn attr_bool(mut self, name: impl Into<String>, required: bool) -> Self {
        self.attributes.push(AttributeSchema {
            name: name.into(),
            attr_type: AttributeType::Bool,
            required,
            default: None,
            doc: None,
        });
        self
    }

    /// Add string list attribute.
    pub fn attr_string_list(mut self, name: impl Into<String>, required: bool) -> Self {
        self.attributes.push(AttributeSchema {
            name: name.into(),
            attr_type: AttributeType::StringList,
            required,
            default: None,
            doc: None,
        });
        self
    }

    /// Add label list attribute.
    pub fn attr_label_list(mut self, name: impl Into<String>, required: bool) -> Self {
        self.attributes.push(AttributeSchema {
            name: name.into(),
            attr_type: AttributeType::LabelList,
            required,
            default: None,
            doc: None,
        });
        self
    }

    /// Set action generator function.
    pub fn action_generator<F>(mut self, f: F) -> Self
    where
        F: Fn(&Target, &HashMap<String, AttributeValue>, &RuleContext) -> Result<Vec<Action>>
            + Send
            + Sync
            + 'static,
    {
        self.action_generator = Some(Box::new(f));
        self
    }

    /// Build the rule.
    pub fn build(self) -> Arc<BuiltRule> {
        Arc::new(BuiltRule {
            name: self.name,
            attributes: self.attributes,
            action_generator: self.action_generator.expect("Action generator not set"),
        })
    }
}

/// Built rule from RuleBuilder.
pub struct BuiltRule {
    name: String,
    attributes: Vec<AttributeSchema>,
    action_generator: ActionGeneratorFn,
}

impl CustomRule for BuiltRule {
    fn name(&self) -> &str {
        &self.name
    }

    fn attributes(&self) -> Vec<AttributeSchema> {
        self.attributes.clone()
    }

    fn generate_actions(
        &self,
        target: &Target,
        attrs: &HashMap<String, AttributeValue>,
        ctx: &RuleContext,
    ) -> Result<Vec<Action>> {
        (self.action_generator)(target, attrs, ctx)
    }
}

/// Macro rule - generates multiple targets from a single declaration.
pub trait MacroRule: Send + Sync {
    /// Macro name.
    fn name(&self) -> &str;

    /// Expand macro into targets.
    fn expand(&self, name: &str, attrs: &HashMap<String, AttributeValue>) -> MacroExpansion;
}

/// Aspect - applies transformations to existing rules.
pub trait RuleAspect: Send + Sync {
    /// Aspect name.
    fn name(&self) -> &str;

    /// Apply aspect to rule actions.
    fn apply(&self, target: &Target, actions: &mut Vec<Action>, ctx: &RuleContext) -> Result<()>;

    /// Which rules this aspect applies to.
    fn applies_to(&self, rule_kind: &str) -> bool;
}

/// Rule composition - combine multiple rules.
pub struct CompositeRule {
    name: String,
    base_rules: Vec<Arc<dyn CustomRule>>,
}

impl CompositeRule {
    /// Create new composite rule.
    pub fn new(name: impl Into<String>, base_rules: Vec<Arc<dyn CustomRule>>) -> Self {
        CompositeRule {
            name: name.into(),
            base_rules,
        }
    }
}

impl CustomRule for CompositeRule {
    fn name(&self) -> &str {
        &self.name
    }

    fn attributes(&self) -> Vec<AttributeSchema> {
        // Merge attributes from all base rules
        let mut attrs = Vec::new();
        for rule in &self.base_rules {
            attrs.extend(rule.attributes());
        }
        attrs
    }

    fn generate_actions(
        &self,
        target: &Target,
        attrs: &HashMap<String, AttributeValue>,
        ctx: &RuleContext,
    ) -> Result<Vec<Action>> {
        // Combine actions from all base rules
        let mut all_actions = Vec::new();
        for rule in &self.base_rules {
            let actions = rule.generate_actions(target, attrs, ctx)?;
            all_actions.extend(actions);
        }
        Ok(all_actions)
    }
}

/// Example: Custom protobuf rule.
pub struct ProtobufRule;

impl CustomRule for ProtobufRule {
    fn name(&self) -> &str {
        "proto_library"
    }

    fn attributes(&self) -> Vec<AttributeSchema> {
        vec![
            AttributeSchema {
                name: "srcs".to_string(),
                attr_type: AttributeType::PathList,
                required: true,
                default: None,
                doc: Some("Protocol buffer source files".to_string()),
            },
            AttributeSchema {
                name: "deps".to_string(),
                attr_type: AttributeType::LabelList,
                required: false,
                default: None,
                doc: Some("Dependencies".to_string()),
            },
        ]
    }

    fn generate_actions(
        &self,
        _target: &Target,
        attrs: &HashMap<String, AttributeValue>,
        ctx: &RuleContext,
    ) -> Result<Vec<Action>> {
        let srcs = attrs
            .get("srcs")
            .and_then(|v| v.as_path_list())
            .context("srcs attribute required")?;

        let mut actions = Vec::new();

        for src in srcs {
            let output = ctx.output_dir.join(format!(
                "{}.pb.h",
                src.file_stem().unwrap().to_str().unwrap()
            ));

            let cmd = Command {
                executable: rush_rules::rules_system::protobuf_protoc_path(),
                args: vec![
                    // proto3 optional needs this on protoc < 3.15; harmless on newer (Bazel always passes it).
                    "--experimental_allow_proto3_optional".to_string(),
                    format!("--cpp_out={}", ctx.output_dir.display()),
                    src.display().to_string(),
                ],
                working_dir: None,
                execution_info: rush_action::ExecutionInfo::default(),
            };

            let inputs = vec![Artifact::Source(src.clone())];
            let env = HashMap::new();
            let action_id = Action::compute_id(&inputs, &cmd, &env)?;

            actions.push(Action {
                id: action_id,
                inputs,
                outputs: vec![Artifact::Source(output)],
                command: cmd,
                env,
                mnemonic: "Protoc".to_string(),
            });
        }

        Ok(actions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_attribute_validation() {
        let schema = AttributeSchema {
            name: "test".to_string(),
            attr_type: AttributeType::String,
            required: true,
            default: None,
            doc: None,
        };

        let value = AttributeValue::String("hello".to_string());
        assert!(value.matches_type(&schema.attr_type));

        let wrong_value = AttributeValue::Bool(true);
        assert!(!wrong_value.matches_type(&schema.attr_type));
    }

    #[test]
    fn test_rule_registry() {
        let mut registry = CustomRuleRegistry::new();
        registry.register(Arc::new(ProtobufRule));

        assert!(registry.get("proto_library").is_some());
        assert_eq!(registry.list_rules(), vec!["proto_library"]);
    }

    #[test]
    fn test_rule_builder() {
        let rule = RuleBuilder::new("my_rule")
            .attr_string("name", true)
            .attr_string_list("srcs", true)
            .action_generator(|_target, _attrs, _ctx| Ok(vec![]))
            .build();

        assert_eq!(rule.name(), "my_rule");
        assert_eq!(rule.attributes().len(), 2);
    }
}
