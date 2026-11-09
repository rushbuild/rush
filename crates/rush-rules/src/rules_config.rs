use crate::context::BuildContext;
use crate::providers::ProviderSet;
use crate::{
    extract_string, extract_string_list, Artifact, BuildRule, RuleInstance,
    Value,
};
use anyhow::Result;
use async_trait::async_trait;
use std::collections::HashMap;

// ============================================================================
// config_setting - Configuration matching metadata
// ============================================================================

pub struct ConfigSettingRule;

impl BuildRule for ConfigSettingRule {
    fn name(&self) -> &str {
        "config_setting"
    }

    fn parse_attributes(
        &self,
        attrs: &HashMap<String, crate::Value>,
    ) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let constraint_values = extract_string_list(attrs, "constraint_values")?;
        let mut unsupported_native_options = match attrs.get("values") {
            None => Vec::new(),
            Some(Value::Dict(values)) => values
                .keys()
                .filter(|key| !rush_command_core::native_options::supported_native_option(key))
                .cloned()
                .collect::<Vec<_>>(),
            Some(_) => anyhow::bail!("values must be a dictionary"),
        };
        unsupported_native_options.sort();
        unsupported_native_options.dedup();
        let has_predicate = attrs
            .get("define_values")
            .and_then(|value| match value {
                Value::Dict(values) => Some(!values.is_empty()),
                _ => None,
            })
            .unwrap_or(false)
            || attrs
                .get("values")
                .and_then(|value| match value {
                    Value::Dict(values) => Some(!values.is_empty()),
                    _ => None,
                })
                .unwrap_or(false)
            || attrs
                .get("constraint_values")
                .and_then(|value| match value {
                    Value::List(values) => Some(!values.is_empty()),
                    _ => None,
                })
                .unwrap_or(false)
            || attrs
                .get("flag_values")
                .and_then(|value| match value {
                    Value::Dict(values) => Some(!values.is_empty()),
                    _ => None,
                })
                .unwrap_or(false);
        let invalid_native_value = match attrs.get("values") {
            Some(Value::Dict(values)) => {
                let mut entries: Vec<_> = values.iter().collect();
                entries.sort_by_key(|(name, _)| *name);
                entries.into_iter().find_map(|(name, value)| {
                    if !rush_command_core::native_options::supported_native_option(name) { return None; }
                    match value {
                        Value::String(value) => rush_command_core::native_options::NativeConfiguration::default()
                            .matches(name, value).err(),
                        _ => Some(format!("native option {name:?} requires a string predicate")),
                    }
                })
            }
            _ => None,
        };
        let configuration_error = if !has_predicate {
            Some(
                "Either values, define_values, flag_values or constraint_values must be specified and non-empty"
                    .to_string(),
            )
        } else if !unsupported_native_options.is_empty() {
            Some(format!(
                "unsupported native configuration option(s): {}",
                unsupported_native_options.join(", ")
            ))
        } else if invalid_native_value.is_some() {
            invalid_native_value
        } else if matches!(attrs.get("flag_values"), Some(Value::Dict(values)) if !values.is_empty()) {
            // The predicate registry can retain typed defaults, but a complete
            // declared-setting/alias/type resolver is not implemented at this
            // configured-rule boundary. Preserve the error as metadata so an
            // unrelated sibling stays lazy; selecting/requesting this condition
            // must not silently turn an unresolved flag into a false predicate.
            Some("Rush does not yet support non-empty config_setting.flag_values".to_string())
        } else {
            None
        };

        Ok(Box::new(ConfigSettingInstance {
            name,
            constraint_values,
            configuration_error,
        }))
    }
}

pub struct ConfigSettingInstance {
    name: String,
    constraint_values: Vec<String>,
    configuration_error: Option<String>,
}

#[async_trait]
impl RuleInstance for ConfigSettingInstance {
    fn rule_type(&self) -> &str {
        // Was the trait default "unknown". The prune planner's admission
        // and its census gate on this string; an unnamed WRITER hiding in
        // the "unknown" bucket would ride along with any future
        // admission of that bucket. Named so gating can be per-kind.
        "config_setting"
    }

    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        // constraint_values are graph metadata, not build-order/provider edges.
        Vec::new()
    }

    fn configuration_constraint_values(&self) -> &[String] {
        &self.constraint_values
    }

    fn configuration_error(&self) -> Option<&str> {
        self.configuration_error.as_deref()
    }

    async fn build(&self, _context: &BuildContext) -> Result<Vec<Artifact>> {
        // config_setting is a Bazel configuration matching rule used for select() statements.
        // It does NOT produce build artifacts - it only provides configuration constraints.
        // This is correct behavior, not a stub implementation.
        Ok(Vec::new())
    }

    fn providers(&self) -> ProviderSet {
        ProviderSet::empty()
    }

    fn is_configuration_condition(&self) -> bool {
        true
    }
}

// ============================================================================
// Configuration-noun rules: platform, toolchain, toolchain_type,
// constraint_setting, constraint_value.
//
// Like config_setting these declare configuration metadata, not build
// actions: Bazel consumes them during toolchain resolution and platform
// matching. Their attribute edges (constraint_values, exec_compatible_with,
// the toolchain label) are configuration references, not build deps.
// ============================================================================

pub struct ConfigNounRule {
    pub rule_type: &'static str,
}

impl BuildRule for ConfigNounRule {
    fn name(&self) -> &str {
        self.rule_type
    }

    fn parse_attributes(
        &self,
        attrs: &HashMap<String, crate::Value>,
    ) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let constraint_setting = if self.rule_type == "constraint_value" {
            Some(extract_string(attrs, "constraint_setting")?)
        } else {
            None
        };
        Ok(Box::new(ConfigNounInstance {
            name,
            rule_type: self.rule_type,
            constraint_setting,
        }))
    }
}

pub struct ConfigNounInstance {
    name: String,
    rule_type: &'static str,
    constraint_setting: Option<String>,
}

#[async_trait]
impl RuleInstance for ConfigNounInstance {
    fn rule_type(&self) -> &str {
        self.rule_type
    }

    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        Vec::new()
    }

    fn configuration_constraint_setting(&self) -> Option<&str> {
        self.constraint_setting.as_deref()
    }

    async fn build(&self, _context: &BuildContext) -> Result<Vec<Artifact>> {
        Ok(Vec::new())
    }

    fn providers(&self) -> ProviderSet {
        ProviderSet::empty()
    }

    fn is_configuration_condition(&self) -> bool {
        self.rule_type == "constraint_value"
    }
}

// ============================================================================
// Incompatible targets: target_compatible_with resolved to
// @platforms//:incompatible. Bazel registers such targets but never
// configures or builds them (and never evaluates their other attributes'
// selects); with --keep_going they are reported as skipped.
// ============================================================================

pub struct IncompatibleTargetInstance {
    name: String,
}

impl IncompatibleTargetInstance {
    pub fn new(name: String) -> Self {
        Self { name }
    }
}

#[async_trait]
impl RuleInstance for IncompatibleTargetInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        Vec::new()
    }

    async fn build(&self, _context: &BuildContext) -> Result<Vec<Artifact>> {
        Ok(Vec::new())
    }

    fn providers(&self) -> ProviderSet {
        ProviderSet::empty()
    }

    fn suppresses_configuration_condition_visibility(&self) -> bool {
        true
    }
}

// ============================================================================
// Deferred-configuration errors: Bazel evaluates a target's attribute
// selects at CONFIGURATION time, so a target with an unresolvable select
// (e.g. a CUDA-only pip pin under the default config) loads fine and only
// errors if actually requested. Rush instantiates eagerly, so a failed
// parse becomes an instance that reproduces the error when the target enters
// the requested configured-target closure (before analysis or execution).
// ============================================================================

pub struct UnconfigurableTargetInstance {
    name: String,
    error: String,
    suppress_condition_visibility: bool,
}

impl UnconfigurableTargetInstance {
    pub fn new(name: String, error: String) -> Self {
        Self {
            name,
            error,
            suppress_condition_visibility: false,
        }
    }

    /// Construct an error produced while resolving configurable attributes.
    /// Bazel reports this before RuleContext visibility validation.
    pub fn from_configurable_error(name: String, error: String) -> Self {
        Self {
            name,
            error,
            suppress_condition_visibility: true,
        }
    }
}

#[async_trait]
impl RuleInstance for UnconfigurableTargetInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        Vec::new()
    }

    fn configuration_error(&self) -> Option<&str> {
        Some(&self.error)
    }

    fn suppresses_configuration_condition_visibility(&self) -> bool {
        self.suppress_condition_visibility
    }

    async fn build(&self, _context: &BuildContext) -> Result<Vec<Artifact>> {
        Err(anyhow::anyhow!(
            "target '{}' cannot be configured: {}",
            self.name,
            self.error
        ))
    }

    fn providers(&self) -> ProviderSet {
        ProviderSet::empty()
    }
}

// ============================================================================
// label_flag - Label-valued build setting
// ============================================================================

pub struct LabelFlagRule;

impl BuildRule for LabelFlagRule {
    fn name(&self) -> &str {
        "label_flag"
    }

    fn parse_attributes(
        &self,
        attrs: &HashMap<String, crate::Value>,
    ) -> Result<Box<dyn RuleInstance>> {
        let name = extract_string(attrs, "name")?;
        let actual = extract_string(attrs, "build_setting_default")?;
        Ok(Box::new(LabelFlagInstance { name, actual }))
    }
}

pub struct LabelFlagInstance {
    name: String,
    actual: String,
}

#[async_trait]
impl RuleInstance for LabelFlagInstance {
    fn target_name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> Vec<String> {
        vec![self.actual.clone()]
    }

    async fn build(&self, _context: &BuildContext) -> Result<Vec<Artifact>> {
        Ok(Vec::new())
    }

    fn providers(&self) -> ProviderSet {
        ProviderSet::empty()
    }

    fn rule_type(&self) -> &str {
        // A label_flag configured to its default forwards the selected
        // target's providers and artifacts exactly like an alias. Rush does
        // not yet expose command-line build-setting overrides, so the default
        // is the only valid configured value here.
        "alias"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_option_validation_accepts_typed_predicates_and_rejects_unknowns() {
        let parse = |values| ConfigSettingRule.parse_attributes(&HashMap::from([
            ("name".to_string(), Value::String("native".to_string())),
            ("values".to_string(), Value::Dict(values)),
        ])).unwrap();
        let supported = parse(HashMap::from([
            ("apple_platform_type".into(), Value::String("ios".into())),
            ("compilation_mode".into(), Value::String("opt".into())),
            ("stamp".into(), Value::String("1".into())),
            ("crosstool_top".into(), Value::String("@bazel_tools//tools/cpp:toolchain".into())),
            ("copt".into(), Value::String("-DVALUE".into())),
        ]));
        assert!(supported.configuration_error().is_none());
        let invalid = parse(HashMap::from([("apple_platform_type".into(), Value::String("linux".into()))]));
        assert!(invalid.configuration_error().unwrap().contains("invalid --apple_platform_type"));
        let unknown = parse(HashMap::from([("unrecognized".into(), Value::String("anything".into()))]));
        assert!(unknown.configuration_error().unwrap().contains("unsupported native configuration option"));
        for label in ["//bad/../pkg:tc", "//pkg//sub:tc", "/absolute:tc"] {
            let malformed = parse(HashMap::from([("crosstool_top".into(), Value::String(label.into()))]));
            assert!(malformed.configuration_error().unwrap().contains("invalid native label option"));
        }
    }

    #[test]
    fn config_setting_retains_constraint_metadata_without_build_edges() {
        let attrs = HashMap::from([
            ("name".to_string(), Value::String("host".to_string())),
            (
                "constraint_values".to_string(),
                Value::List(vec![
                    Value::String("@platforms//os:linux".to_string()),
                    Value::String("@platforms//cpu:x86_64".to_string()),
                ]),
            ),
        ]);
        let instance = ConfigSettingRule.parse_attributes(&attrs).unwrap();

        assert!(instance.dependencies().is_empty());
        assert_eq!(
            instance.configuration_constraint_values(),
            [
                "@platforms//os:linux".to_string(),
                "@platforms//cpu:x86_64".to_string(),
            ]
        );
    }

    #[test]
    fn config_setting_retains_unsupported_flag_value_error_for_configured_scope() {
        let attrs = HashMap::from([
            ("name".to_string(), Value::String("default_mode".to_string())),
            (
                "flag_values".to_string(),
                Value::Dict(HashMap::from([(
                    "//:mode".to_string(),
                    Value::String("default".to_string()),
                )])),
            ),
        ]);
        let instance = ConfigSettingRule.parse_attributes(&attrs).unwrap();

        assert_eq!(
            instance.configuration_error(),
            Some("Rush does not yet support non-empty config_setting.flag_values")
        );
        assert!(instance.is_configuration_condition());
        assert!(
            instance.dependencies().is_empty(),
            "unsupported metadata is not an execution edge"
        );
    }
}
