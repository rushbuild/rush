// Envoy Bazel compatibility layer
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Compatibility layer for Envoy's custom Bazel macros
//!
//! Envoy uses ~40 custom macros like envoy_cc_library, envoy_cc_test, etc.
//! This module translates them to standard Bazel rules that Rush understands.

use crate::{Rule, Value};
use std::collections::HashMap;

/// Translate Envoy macro to standard Bazel rule
pub fn translate_envoy_macro(rule: &Rule) -> Rule {
    // An opaque declaration is not an Envoy macro registration. Preserve its
    // selected-only refusal or qualified operation without reinterpretation.
    if rule.cc_projection.is_some() {
        return rule.clone();
    }
    match rule.rule_type.as_str() {
        // Library variants
        "envoy_cc_library" => to_cc_library(rule),
        "envoy_cc_linux_library" => to_cc_library(rule),
        "envoy_cc_posix_library" => to_cc_library(rule),
        "envoy_cc_win32_library" => to_cc_library(rule),
        "envoy_cc_extension" => to_cc_library(rule),
        "envoy_cc_contrib_extension" => to_cc_library(rule),

        // Test variants
        "envoy_cc_test" => to_cc_test(rule),
        "envoy_cc_fuzz_test" => to_cc_test(rule),
        "envoy_benchmark_test" => to_cc_test(rule),

        // Binary variants
        "envoy_cc_binary" => to_cc_binary(rule),

        // Proto/Starlark specific
        "envoy_proto_library" => to_proto_library(rule),

        // If not an Envoy macro, return as-is
        _ => rule.clone(),
    }
}

/// Convert envoy_cc_library to standard cc_library
fn to_cc_library(rule: &Rule) -> Rule {
    let mut new_rule = rule.clone();
    new_rule.rule_type = "cc_library".to_string();

    // Envoy adds default include paths and visibility
    // We handle this by ensuring Rush's cc_library implementation does the same

    new_rule
}

/// Convert envoy_cc_test to standard cc_test
fn to_cc_test(rule: &Rule) -> Rule {
    let mut new_rule = rule.clone();
    new_rule.rule_type = "cc_test".to_string();

    // Envoy tests often have additional test runners
    // Add googletest dependency if not present
    if !new_rule.attributes.contains_key("deps") {
        new_rule.attributes.insert(
            "deps".to_string(),
            Value::List(vec![Value::String(
                "@com_google_googletest//:gtest".to_string(),
            )]),
        );
    }

    new_rule
}

/// Convert envoy_cc_binary to standard cc_binary
fn to_cc_binary(rule: &Rule) -> Rule {
    let mut new_rule = rule.clone();
    new_rule.rule_type = "cc_binary".to_string();
    new_rule
}

/// Convert envoy_proto_library to proto_library
fn to_proto_library(rule: &Rule) -> Rule {
    let mut new_rule = rule.clone();
    new_rule.rule_type = "proto_library".to_string();
    new_rule
}

/// Handle envoy_package() macro - extracts default visibility
pub fn parse_envoy_package(attrs: &HashMap<String, Value>) -> Option<String> {
    attrs.get("default_visibility").and_then(|v| match v {
        Value::String(s) => Some(s.clone()),
        _ => None,
    })
}

/// Handle envoy select macros
pub fn handle_envoy_select(select_name: &str) -> Vec<String> {
    match select_name {
        "envoy_select_enable_http3" => vec!["http3".to_string()],
        "envoy_select_enable_yaml" => vec!["yaml".to_string()],
        "envoy_select_google_grpc" => vec!["grpc".to_string()],
        "envoy_select_wasm_v8" => vec!["wasm".to_string()],
        "envoy_select_hot_restart" => vec!["hot_restart".to_string()],
        "envoy_select_admin_functionality" => vec!["admin".to_string()],
        "envoy_select_boringssl" => vec!["boringssl".to_string()],
        _ => Vec::new(),
    }
}

/// Get Envoy-specific copts (compiler options)
pub fn get_envoy_copts() -> Vec<String> {
    vec![
        "-std=c++20".to_string(),
        "-Wall".to_string(),
        "-Wno-deprecated-declarations".to_string(),
        "-DABSL_MIN_LOG_LEVEL=4".to_string(),
    ]
}

/// Get Envoy-specific linkopts
pub fn get_envoy_linkopts() -> Vec<String> {
    vec!["-pthread".to_string(), "-lm".to_string()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_envoy_cc_library_translation() {
        let mut attrs = HashMap::new();
        attrs.insert(
            "srcs".to_string(),
            Value::List(vec![Value::String("lib.cc".to_string())]),
        );

        let envoy_rule = Rule {
            cc_projection: None,
            name: "mylib".to_string(),
            rule_type: "envoy_cc_library".to_string(),
            attributes: attrs,
        };

        let translated = translate_envoy_macro(&envoy_rule);
        assert_eq!(translated.rule_type, "cc_library");
        assert_eq!(translated.name, "mylib");
    }

    #[test]
    fn test_envoy_select_macros() {
        let deps = handle_envoy_select("envoy_select_enable_http3");
        assert_eq!(deps, vec!["http3"]);
    }
}
