/// Bridge between Starlark evaluation and Rush build rules
///
/// This module provides conversion from Starlark values to rule attribute values,
/// enabling BUILD file evaluation results to be consumed by the rule system.
///
/// Note: RuleValue has the same structure as rush_rules::Value but is defined
/// here to avoid circular dependencies. The rush-build crate will handle the
/// final conversion.
use crate::evaluator::{ConfigurableOp, RuleCall, Value as StarlarkValue};
use anyhow::{anyhow, Result};
use std::collections::HashMap;

/// Rule attribute value (simplified for rule system)
#[derive(Debug, Clone, PartialEq)]
pub enum RuleValue {
    String(String),
    Int(i64),
    Bool(bool),
    List(Vec<RuleValue>),
    Dict(HashMap<String, RuleValue>),
    None,
    Select(HashMap<String, RuleValue>),
    Configurable {
        op: ConfigurableOp,
        left: Box<RuleValue>,
        right: Box<RuleValue>,
    },
}

impl RuleValue {
    pub fn as_string(&self) -> Option<&str> {
        match self {
            RuleValue::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            RuleValue::Int(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            RuleValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&Vec<RuleValue>> {
        match self {
            RuleValue::List(list) => Some(list),
            _ => None,
        }
    }
}

fn starlark_to_configurable_rule_value(value: &StarlarkValue) -> Result<RuleValue> {
    match value {
        StarlarkValue::Float(_) => Err(anyhow!(
            "configurable float operands are not supported by the rule bridge"
        )),
        StarlarkValue::List(items) | StarlarkValue::Tuple(items) => Ok(RuleValue::List(
            items
                .iter()
                .map(starlark_to_configurable_rule_value)
                .collect::<Result<Vec<_>>>()?,
        )),
        StarlarkValue::Dict(items) => Ok(RuleValue::Dict(
            items
                .iter()
                .map(|(key, value)| {
                    Ok((
                        key.clone(),
                        starlark_to_configurable_rule_value(value)?,
                    ))
                })
                .collect::<Result<HashMap<_, _>>>()?,
        )),
        StarlarkValue::Select(items) => Ok(RuleValue::Select(
            items
                .iter()
                .map(|(condition, value)| {
                    Ok((
                        condition.clone(),
                        starlark_to_configurable_rule_value(value)?,
                    ))
                })
                .collect::<Result<HashMap<_, _>>>()?,
        )),
        StarlarkValue::Configurable { op, left, right } => Ok(RuleValue::Configurable {
            op: *op,
            left: Box::new(starlark_to_configurable_rule_value(left)?),
            right: Box::new(starlark_to_configurable_rule_value(right)?),
        }),
        other => starlark_to_rule_value(other),
    }
}

/// Convert Starlark Value to RuleValue
///
/// This conversion loses some information (functions, namespaces, providers)
/// since rule attributes only support basic types.
pub fn starlark_to_rule_value(value: &StarlarkValue) -> Result<RuleValue> {
    match value {
        StarlarkValue::String(s) => Ok(RuleValue::String(s.clone())),
        StarlarkValue::Int(i) => Ok(RuleValue::Int(*i)),
        StarlarkValue::Float(f) => Ok(RuleValue::Int(*f as i64)), // Lossy conversion
        StarlarkValue::Bool(b) => Ok(RuleValue::Bool(*b)),
        StarlarkValue::None => Ok(RuleValue::None),
        StarlarkValue::List(items) | StarlarkValue::Tuple(items) => {
            let converted: Result<Vec<_>> = items.iter().map(starlark_to_rule_value).collect();
            Ok(RuleValue::List(converted?))
        }
        StarlarkValue::Dict(map) => {
            let mut converted = HashMap::new();
            for (k, v) in map {
                converted.insert(k.clone(), starlark_to_rule_value(v)?);
            }
            Ok(RuleValue::Dict(converted))
        }
        StarlarkValue::Function { .. } => Err(anyhow!("Cannot convert function to rule attribute")),
        StarlarkValue::Namespace(_) | StarlarkValue::WorkspaceNativeNamespace { .. } => {
            Err(anyhow!("Cannot convert namespace to rule attribute"))
        }
        StarlarkValue::RuleFunction(name) => Err(anyhow!(
            "Cannot convert rule function '{}' to rule attribute",
            name
        )),
        StarlarkValue::NativeFunction(name) => Err(anyhow!(
            "Cannot convert native function '{}' to rule attribute",
            name
        )),
        StarlarkValue::Provider { .. } => Err(anyhow!("Cannot convert provider to rule attribute")),
        StarlarkValue::ProviderInstance { .. } => Err(anyhow!(
            "Cannot convert provider instance to rule attribute"
        )),
        StarlarkValue::Select(conditions) => {
            let mut converted = HashMap::new();
            for (k, v) in conditions {
                converted.insert(k.clone(), starlark_to_rule_value(v)?);
            }
            Ok(RuleValue::Select(converted))
        }
        StarlarkValue::Configurable { .. } => starlark_to_configurable_rule_value(value),
        StarlarkValue::BoundMethod { .. } => {
            Err(anyhow!("Cannot convert bound method to rule attribute"))
        }
        StarlarkValue::Label { label, .. } => {
            // Convert Label to its string representation for rule attributes
            Ok(RuleValue::String(label.clone()))
        }
        StarlarkValue::LazyLoad { bzl_label, symbol_name } => {
            Err(anyhow!(
                "Internal error: LazyLoad value for symbol '{}' from '{}' should have been resolved before use",
                symbol_name, bzl_label
            ))
        }
        StarlarkValue::Lambda { .. } => Err(anyhow!("Cannot convert lambda to rule attribute")),
    }
}

/// Convert RuleCall to attribute map suitable for rule parsing
pub fn rule_call_to_attrs(rule_call: &RuleCall) -> Result<HashMap<String, RuleValue>> {
    let mut attrs = HashMap::new();

    // Convert keyword arguments
    for (key, value) in &rule_call.kwargs {
        attrs.insert(key.clone(), starlark_to_rule_value(value)?);
    }

    // Note: Positional arguments are rare in BUILD files (most rules use kwargs only)
    // If needed, we could add special handling for positional args here

    Ok(attrs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_string() {
        let starlark = StarlarkValue::String("hello".to_string());
        let rule = starlark_to_rule_value(&starlark).unwrap();
        assert_eq!(rule, RuleValue::String("hello".to_string()));
    }

    #[test]
    fn test_convert_int() {
        let starlark = StarlarkValue::Int(42);
        let rule = starlark_to_rule_value(&starlark).unwrap();
        assert_eq!(rule, RuleValue::Int(42));
    }

    #[test]
    fn test_convert_bool() {
        let starlark = StarlarkValue::Bool(true);
        let rule = starlark_to_rule_value(&starlark).unwrap();
        assert_eq!(rule, RuleValue::Bool(true));
    }

    #[test]
    fn tuple_attributes_keep_existing_sequence_conversion() {
        let items = vec![
            StarlarkValue::String("a.cc".to_string()),
            StarlarkValue::Tuple(vec![StarlarkValue::String("nested.h".to_string())]),
        ];
        let tuple = StarlarkValue::Tuple(items.clone());
        let list = StarlarkValue::List(items);
        assert_eq!(
            starlark_to_rule_value(&tuple).unwrap(),
            starlark_to_rule_value(&list).unwrap()
        );
        assert_eq!(
            starlark_to_rule_value(&tuple).unwrap(),
            RuleValue::List(vec![
                RuleValue::String("a.cc".to_string()),
                RuleValue::List(vec![RuleValue::String("nested.h".to_string())]),
            ])
        );
        assert_eq!(
            starlark_to_configurable_rule_value(&StarlarkValue::Tuple(vec![
                StarlarkValue::Int(7),
                StarlarkValue::Tuple(vec![StarlarkValue::Bool(true)]),
            ]))
            .unwrap(),
            RuleValue::List(vec![
                RuleValue::Int(7),
                RuleValue::List(vec![RuleValue::Bool(true)])
            ])
        );
    }

    #[test]
    fn test_convert_list() {
        let starlark = StarlarkValue::List(vec![
            StarlarkValue::String("a.cc".to_string()),
            StarlarkValue::String("b.cc".to_string()),
        ]);
        let rule = starlark_to_rule_value(&starlark).unwrap();
        match rule {
            RuleValue::List(items) => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0], RuleValue::String("a.cc".to_string()));
                assert_eq!(items[1], RuleValue::String("b.cc".to_string()));
            }
            _ => panic!("Expected List"),
        }
    }

    #[test]
    fn test_convert_dict() {
        let mut map = HashMap::new();
        map.insert(
            "key".to_string(),
            StarlarkValue::String("value".to_string()),
        );

        let starlark = StarlarkValue::Dict(map);
        let rule = starlark_to_rule_value(&starlark).unwrap();

        match rule {
            RuleValue::Dict(dict) => {
                assert_eq!(
                    dict.get("key"),
                    Some(&RuleValue::String("value".to_string()))
                );
            }
            _ => panic!("Expected Dict"),
        }
    }

    #[test]
    fn configurable_float_conversion_is_rejected_instead_of_truncated() {
        let value = StarlarkValue::Configurable {
            op: ConfigurableOp::Add,
            left: std::sync::Arc::new(StarlarkValue::Select(HashMap::from([(
                "//conditions:default".to_string(),
                StarlarkValue::Float(1.9),
            )]))),
            right: std::sync::Arc::new(StarlarkValue::Float(1.9)),
        };

        let error = starlark_to_rule_value(&value).unwrap_err();
        assert!(format!("{error:#}").contains("configurable float operands"));
    }

    #[test]
    fn test_rule_call_conversion() {
        let mut kwargs = HashMap::new();
        kwargs.insert(
            "name".to_string(),
            StarlarkValue::String("test".to_string()),
        );
        kwargs.insert(
            "srcs".to_string(),
            StarlarkValue::List(vec![StarlarkValue::String("test.cc".to_string())]),
        );

        let rule_call = RuleCall {
            rule_type: "cc_library".to_string(),
            args: vec![],
            kwargs,
        };

        let attrs = rule_call_to_attrs(&rule_call).unwrap();
        assert_eq!(
            attrs.get("name"),
            Some(&RuleValue::String("test".to_string()))
        );
        assert!(matches!(attrs.get("srcs"), Some(RuleValue::List(_))));
    }
}
