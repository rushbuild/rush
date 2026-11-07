use crate::platform::PlatformConfig;
use crate::{ConfigurableOp, Value};
use std::collections::HashMap;

fn value_kind(value: &Value) -> &'static str {
    match value {
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Bool(_) => "bool",
        Value::String(_) => "string",
        Value::List(_) => "list",
        Value::Dict(_) => "dict",
        Value::None => "None",
        Value::Select(_) => "select",
        Value::Configurable { .. } => "configurable expression",
        _ => "unsupported value",
    }
}

fn apply_configurable_op(op: ConfigurableOp, left: Value, right: Value) -> anyhow::Result<Value> {
    match op {
        ConfigurableOp::Add => match (left, right) {
            (Value::Int(left), Value::Int(right)) => Ok(Value::Int(left + right)),
            (Value::String(left), Value::String(right)) => {
                Ok(Value::String(left + &right))
            }
            (Value::List(mut left), Value::List(right)) => {
                left.extend(right);
                Ok(Value::List(left))
            }
            (Value::None, Value::List(right)) => Ok(Value::List(right)),
            (Value::List(left), Value::None) => Ok(Value::List(left)),
            (Value::None, Value::None) => Ok(Value::None),
            (left, right) => anyhow::bail!(
                "cannot apply configurable + to {} and {}",
                value_kind(&left),
                value_kind(&right)
            ),
        },
        ConfigurableOp::BitOr => match (left, right) {
            (Value::Dict(mut left), Value::Dict(right)) => {
                left.extend(right);
                Ok(Value::Dict(left))
            }
            (Value::List(left), Value::List(right)) => {
                let mut merged = Vec::new();
                for value in left.into_iter().chain(right) {
                    if !merged.contains(&value) {
                        merged.push(value);
                    }
                }
                Ok(Value::List(merged))
            }
            (Value::Int(left), Value::Int(right)) => Ok(Value::Int(left | right)),
            (left, right) => anyhow::bail!(
                "cannot apply configurable | to {} and {}",
                value_kind(&left),
                value_kind(&right)
            ),
        },
    }
}

/// Evaluates select() expressions based on platform configuration
pub struct SelectEvaluator {
    platform: PlatformConfig,
}

impl SelectEvaluator {
    /// Create a new select evaluator with auto-detected platform
    pub fn new() -> Self {
        Self {
            platform: PlatformConfig::new(),
        }
    }

    /// Create an evaluator permanently bound to one workspace configuration
    /// registry. This remains correct when evaluation resumes on a different
    /// worker thread after an async boundary.
    pub fn for_configuration_scope(scope: impl Into<String>) -> Self {
        Self {
            platform: PlatformConfig::for_configuration_scope(scope),
        }
    }

    /// Create with custom platform configuration
    pub fn with_platform(platform: PlatformConfig) -> Self {
        Self { platform }
    }

    /// Evaluate a select() expression to a concrete value
    ///
    /// Selects the unique matching condition, or "//conditions:default" if none match.
    /// Returns error if no match found and no default provided.
    pub fn evaluate(&self, select_map: &HashMap<String, Value>) -> anyhow::Result<Value> {
        // Collect every match. Taking the first HashMap entry is
        // nondeterministic and cannot implement Bazel's specialization rule.
        let mut matches = Vec::new();
        for (condition, value) in select_map {
            if condition == "//conditions:default" {
                continue;
            }
            if self.platform.matches(condition) {
                matches.push((condition, value));
            }
        }

        let mut maximal = matches
            .iter()
            .enumerate()
            .filter(|(candidate_index, (candidate, _))| {
                !matches.iter().enumerate().any(|(other_index, (other, _))| {
                    candidate_index != &other_index
                        && self.platform.strictly_specializes(other, candidate)
                })
            })
            .map(|(_, matched)| *matched)
            .collect::<Vec<_>>();
        maximal.sort_by(|(left, _), (right, _)| left.cmp(right));

        if maximal.len() > 1
            && !maximal
                .iter()
                .skip(1)
                .all(|(_, value)| *value == maximal[0].1)
        {
            let labels = maximal
                .iter()
                .map(|(condition, _)| condition.as_str())
                .collect::<Vec<_>>();
            anyhow::bail!(
                "Illegal ambiguous match in select(): incomparable matching conditions resolve to different values: {}",
                labels.join(", ")
            );
        }
        if let Some((_, value)) = maximal.first() {
            return self.evaluate_value(value);
        }

        // No match found - look for default
        if let Some(default_value) = select_map.get("//conditions:default") {
            return self.evaluate_value(default_value);
        }

        anyhow::bail!("No matching condition in select() and no default provided")
    }

    /// Recursively evaluate a value (in case it contains nested selects)
    pub fn evaluate_value(&self, value: &Value) -> anyhow::Result<Value> {
        match value {
            Value::Select(select_map) => self.evaluate(select_map),
            Value::Configurable { op, left, right } => apply_configurable_op(
                *op,
                self.evaluate_value(left)?,
                self.evaluate_value(right)?,
            ),
            Value::List(items) => {
                // Evaluate each item in the list
                let evaluated_items: anyhow::Result<Vec<Value>> =
                    items.iter().map(|item| self.evaluate_value(item)).collect();
                Ok(Value::List(evaluated_items?))
            }
            // Other value types are already concrete
            other => Ok(other.clone()),
        }
    }

    /// Evaluate all select() expressions in a map of attributes
    pub fn evaluate_attributes(
        &self,
        attrs: &HashMap<String, Value>,
    ) -> anyhow::Result<HashMap<String, Value>> {
        let mut evaluated = HashMap::new();

        for (key, value) in attrs {
            evaluated.insert(key.clone(), self.evaluate_value(value)?);
        }

        Ok(evaluated)
    }
}

impl Default for SelectEvaluator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evaluator_with_explicit_host_conditions() -> SelectEvaluator {
        let mut platform = PlatformConfig::new();
        platform.set_condition("@platforms//os:linux".to_string(), cfg!(target_os = "linux"));
        platform.set_condition("@platforms//os:osx".to_string(), cfg!(target_os = "macos"));
        SelectEvaluator::with_platform(platform)
    }

    #[test]
    fn test_simple_select() {
        // Pure select resolver tests provide their condition truth values
        // explicitly. Production @platforms labels only match after the real
        // constraint_value target has been loaded and validated.
        let evaluator = evaluator_with_explicit_host_conditions();

        let mut select_map = HashMap::new();
        select_map.insert(
            "@platforms//os:linux".to_string(),
            Value::String("linux_impl.cc".to_string()),
        );
        select_map.insert(
            "@platforms//os:osx".to_string(),
            Value::String("macos_impl.cc".to_string()),
        );
        select_map.insert(
            "//conditions:default".to_string(),
            Value::String("generic_impl.cc".to_string()),
        );

        let result = evaluator.evaluate(&select_map).unwrap();

        let expected = if cfg!(target_os = "linux") {
            "linux_impl.cc"
        } else if cfg!(target_os = "macos") {
            "macos_impl.cc"
        } else {
            "generic_impl.cc"
        };
        assert_eq!(result, Value::String(expected.to_string()));
    }

    #[test]
    fn test_select_with_list() {
        let evaluator = evaluator_with_explicit_host_conditions();

        let mut select_map = HashMap::new();
        select_map.insert(
            "@platforms//os:linux".to_string(),
            Value::List(vec![
                Value::String("linux_a.cc".to_string()),
                Value::String("linux_b.cc".to_string()),
            ]),
        );
        select_map.insert(
            "//conditions:default".to_string(),
            Value::List(vec![Value::String("default.cc".to_string())]),
        );

        let result = evaluator.evaluate(&select_map).unwrap();

        match result {
            Value::List(items) => {
                let expected = if cfg!(target_os = "linux") {
                    vec![
                        Value::String("linux_a.cc".to_string()),
                        Value::String("linux_b.cc".to_string()),
                    ]
                } else {
                    vec![Value::String("default.cc".to_string())]
                };
                assert_eq!(items, expected);
            }
            _ => panic!("Expected list value"),
        }
    }

    #[test]
    fn test_evaluate_attributes() {
        let evaluator = evaluator_with_explicit_host_conditions();

        let mut attrs = HashMap::new();

        // Regular attribute (no select)
        attrs.insert("name".to_string(), Value::String("my_lib".to_string()));

        // Attribute with select
        let mut select_map = HashMap::new();
        select_map.insert(
            "@platforms//os:linux".to_string(),
            Value::List(vec![Value::String("linux.cc".to_string())]),
        );
        select_map.insert(
            "//conditions:default".to_string(),
            Value::List(vec![Value::String("default.cc".to_string())]),
        );
        attrs.insert("srcs".to_string(), Value::Select(select_map));

        let evaluated = evaluator.evaluate_attributes(&attrs).unwrap();

        assert_eq!(evaluated.len(), 2);
        assert!(matches!(evaluated.get("name"), Some(Value::String(_))));
        assert!(matches!(evaluated.get("srcs"), Some(Value::List(_))));
    }

    #[test]
    fn test_no_match_no_default() {
        let mut platform = PlatformConfig::new();
        // Set only a non-existent condition
        platform.set_condition("//conditions:gpu".to_string(), true);

        let evaluator = SelectEvaluator::with_platform(platform);

        let mut select_map = HashMap::new();
        select_map.insert(
            "//conditions:nonexistent".to_string(),
            Value::String("value".to_string()),
        );

        // Should fail because no match and no default
        let result = evaluator.evaluate(&select_map);
        assert!(result.is_err());
    }

    #[test]
    fn multiple_matching_conditions_fail_deterministically() {
        crate::config_defines::with_configuration_scope("select-ambiguity-test", || {
            crate::config_defines::begin_command(&[
                ("left".to_string(), "1".to_string()),
                ("right".to_string(), "1".to_string()),
            ]);
            crate::config_defines::consider_config_setting(
                "cfg",
                "first",
                &HashMap::from([("left".to_string(), "1".to_string())]),
            );
            crate::config_defines::consider_config_setting(
                "cfg",
                "second",
                &HashMap::from([("right".to_string(), "1".to_string())]),
            );
            let evaluator = SelectEvaluator::new();
            let select_map = HashMap::from([
                (
                    "//cfg:first".to_string(),
                    Value::String("first".to_string()),
                ),
                (
                    "//cfg:second".to_string(),
                    Value::String("second".to_string()),
                ),
            ]);

            let error = evaluator
                .evaluate(&select_map)
                .expect_err("incomparable select conditions must fail");
            assert!(error.to_string().contains("Illegal ambiguous match"));
            assert!(error.to_string().contains("//cfg:first, //cfg:second"));

            let same_value = HashMap::from([
                (
                    "//cfg:first".to_string(),
                    Value::String("shared".to_string()),
                ),
                (
                    "//cfg:second".to_string(),
                    Value::String("shared".to_string()),
                ),
            ]);
            assert_eq!(
                evaluator.evaluate(&same_value).unwrap(),
                Value::String("shared".to_string())
            );
        });
    }

    #[test]
    fn strict_predicate_superset_wins_matching_select() {
        crate::config_defines::with_configuration_scope("select-specialization-test", || {
            crate::config_defines::begin_command(&[
                ("base".to_string(), "1".to_string()),
                ("mode".to_string(), "1".to_string()),
            ]);
            crate::config_defines::consider_config_setting(
                "cfg",
                "base",
                &HashMap::from([("base".to_string(), "1".to_string())]),
            );
            crate::config_defines::consider_config_setting(
                "cfg",
                "specialized",
                &HashMap::from([
                    ("base".to_string(), "1".to_string()),
                    ("mode".to_string(), "1".to_string()),
                ]),
            );
            let result = SelectEvaluator::new()
                .evaluate(&HashMap::from([
                    (
                        "//cfg:base".to_string(),
                        Value::String("base".to_string()),
                    ),
                    (
                        "//cfg:specialized".to_string(),
                        Value::String("specialized".to_string()),
                    ),
                ]))
                .unwrap();
            assert_eq!(result, Value::String("specialized".to_string()));
        });
    }

    #[test]
    fn config_settings_outrank_constraint_values_and_aliases() {
        crate::config_defines::with_configuration_scope(
            "select-condition-kind-priority-test",
            || {
                crate::config_defines::begin_command(&[(
                    "mode".to_string(),
                    "on".to_string(),
                )]);
                crate::config_defines::consider_constraint_value(None, "cv", "linux");
                crate::config_defines::consider_constraint_value(None, "cv", "x86");
                crate::config_defines::consider_config_setting_predicates(
                    None,
                    "cfg",
                    "equal",
                    &HashMap::new(),
                    &HashMap::new(),
                    &["//cv:linux".to_string()],
                    &HashMap::new(),
                );
                crate::config_defines::consider_config_setting_predicates(
                    None,
                    "cfg",
                    "incomparable",
                    &HashMap::from([("mode".to_string(), "on".to_string())]),
                    &HashMap::new(),
                    &[],
                    &HashMap::new(),
                );
                crate::config_defines::consider_config_alias(
                    None,
                    "cfg",
                    "setting_alias",
                    "//cfg:incomparable",
                );
                crate::config_defines::consider_config_alias(
                    None,
                    "cfg",
                    "constraint_alias",
                    "//cv:linux",
                );

                let mut platform = PlatformConfig::new();
                for condition in [
                    "//cv:linux",
                    "//cv:x86",
                    "//cfg:equal",
                    "//cfg:incomparable",
                    "//cfg:setting_alias",
                    "//cfg:constraint_alias",
                ] {
                    platform.set_condition(condition.to_string(), true);
                }
                let evaluator = SelectEvaluator::with_platform(platform);
                let assert_setting_wins = |setting: &str, constraint: &str| {
                    assert_eq!(
                        evaluator
                            .evaluate(&HashMap::from([
                                (
                                    constraint.to_string(),
                                    Value::String("constraint".to_string()),
                                ),
                                (
                                    setting.to_string(),
                                    Value::String("setting".to_string()),
                                ),
                            ]))
                            .unwrap(),
                        Value::String("setting".to_string())
                    );
                };

                assert_setting_wins("//cfg:equal", "//cv:linux");
                assert_setting_wins("//cfg:incomparable", "//cv:linux");
                assert_setting_wins("//cfg:setting_alias", "//cv:linux");
                assert_setting_wins("//cfg:incomparable", "//cfg:constraint_alias");

                let error = evaluator
                    .evaluate(&HashMap::from([
                        (
                            "//cv:linux".to_string(),
                            Value::String("linux".to_string()),
                        ),
                        (
                            "//cv:x86".to_string(),
                            Value::String("x86".to_string()),
                        ),
                    ]))
                    .expect_err("two direct constraint values remain ambiguous");
                assert!(error.to_string().contains("Illegal ambiguous match"));
            },
        );
    }

    #[test]
    fn test_nested_select() {
        let evaluator = evaluator_with_explicit_host_conditions();

        // Create a nested select - select within a list
        let mut inner_select = HashMap::new();
        inner_select.insert(
            "@platforms//os:linux".to_string(),
            Value::String("linux_inner.cc".to_string()),
        );
        inner_select.insert(
            "//conditions:default".to_string(),
            Value::String("default_inner.cc".to_string()),
        );

        let list_with_select = Value::List(vec![
            Value::String("always_included.cc".to_string()),
            Value::Select(inner_select),
        ]);

        let result = evaluator.evaluate_value(&list_with_select).unwrap();

        match result {
            Value::List(items) => {
                assert_eq!(items.len(), 2);
                // First should be the always included file
                assert!(matches!(&items[0], Value::String(s) if s == "always_included.cc"));
                // Second should be evaluated select
                assert_eq!(
                    items[1],
                    Value::String(
                        if cfg!(target_os = "linux") {
                            "linux_inner.cc"
                        } else {
                            "default_inner.cc"
                        }
                        .to_string()
                    )
                );
            }
            _ => panic!("Expected list"),
        }
    }

    #[test]
    fn test_compact_configurable_add_and_union() {
        let evaluator = SelectEvaluator::new();
        let add = Value::Configurable {
            op: ConfigurableOp::Add,
            left: std::sync::Arc::new(Value::List(vec![Value::String(
                "base.cc".to_string(),
            )])),
            right: std::sync::Arc::new(Value::Select(HashMap::from([
                (
                    "//conditions:rush_select_test_never".to_string(),
                    Value::List(vec![Value::String("wrong.cc".to_string())]),
                ),
                (
                    "//conditions:default".to_string(),
                    Value::List(vec![Value::String("selected.cc".to_string())]),
                ),
            ]))),
        };
        assert_eq!(
            evaluator.evaluate_value(&add).unwrap(),
            Value::List(vec![
                Value::String("base.cc".to_string()),
                Value::String("selected.cc".to_string()),
            ])
        );

        let union = Value::Configurable {
            op: ConfigurableOp::BitOr,
            left: std::sync::Arc::new(Value::Dict(HashMap::from([(
                "left".to_string(),
                Value::String("one".to_string()),
            )]))),
            right: std::sync::Arc::new(Value::Select(HashMap::from([(
                "//conditions:default".to_string(),
                Value::Dict(HashMap::from([(
                    "right".to_string(),
                    Value::String("two".to_string()),
                )])),
            )]))),
        };
        assert_eq!(
            evaluator.evaluate_value(&union).unwrap(),
            Value::Dict(HashMap::from([
                ("left".to_string(), Value::String("one".to_string())),
                ("right".to_string(), Value::String("two".to_string())),
            ]))
        );
    }
}
