// Test macro expansion functionality
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use rush_parse::{parse_build_file, Value};
use std::fs;
use tempfile::TempDir;

#[test]
fn test_simple_macro_expansion() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create a .bzl file with a simple macro
    let bzl_content = r#"
def custom_library(name, srcs):
    cc_library(
        name = name,
        srcs = srcs,
    )
"#;
    fs::write(workspace_path.join("macros.bzl"), bzl_content)?;

    // Create BUILD file that uses the macro
    let build_content = r#"
load(":macros.bzl", "custom_library")

custom_library(
    name = "mylib",
    srcs = ["foo.cc", "bar.cc"],
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    // The macro should expand to a cc_library rule
    assert_eq!(build_file.rules.len(), 1);
    let rule = &build_file.rules[0];
    assert_eq!(rule.name, "mylib");
    assert_eq!(rule.rule_type, "cc_library");

    // Check that srcs attribute is preserved
    if let Some(Value::List(srcs)) = rule.attributes.get("srcs") {
        assert_eq!(srcs.len(), 2);
    } else {
        panic!("Expected srcs to be a list");
    }

    Ok(())
}

#[test]
fn test_macro_with_multiple_rules() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create a .bzl file with a macro that generates multiple rules
    let bzl_content = r#"
def lib_and_test(name, srcs):
    cc_library(
        name = name,
        srcs = srcs,
    )
    cc_test(
        name = name + "_test",
        srcs = srcs,
    )
"#;
    fs::write(workspace_path.join("defs.bzl"), bzl_content)?;

    // Create BUILD file that uses the macro
    let build_content = r#"
load(":defs.bzl", "lib_and_test")

lib_and_test(
    name = "foo",
    srcs = ["foo.cc"],
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    // The macro should expand to both library and test
    assert!(
        !build_file.rules.is_empty(),
        "Expected at least 1 rule from macro expansion"
    );

    // Find the library rule
    let lib_rule = build_file
        .rules
        .iter()
        .find(|r| r.rule_type == "cc_library");
    assert!(lib_rule.is_some(), "Expected cc_library rule");
    assert_eq!(lib_rule.unwrap().name, "foo");

    Ok(())
}

#[test]
fn test_regular_rule_with_loaded_macro() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create a .bzl file with a macro (that won't be used)
    let bzl_content = r#"
def unused_macro(name):
    pass
"#;
    fs::write(workspace_path.join("defs.bzl"), bzl_content)?;

    // Create BUILD file with both load and regular rule
    let build_content = r#"
load(":defs.bzl", "unused_macro")

cc_library(
    name = "regular",
    srcs = ["regular.cc"],
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    // Should have the regular rule
    assert_eq!(build_file.rules.len(), 1);
    assert_eq!(build_file.rules[0].name, "regular");
    assert_eq!(build_file.rules[0].rule_type, "cc_library");

    Ok(())
}
