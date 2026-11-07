// Test platform() functionality
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use rush_parse::{parse_build_file, Value};
use std::fs;
use tempfile::TempDir;

#[test]
fn test_platform_basic() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create BUILD file with platform
    let build_content = r#"
platform(
    name = "linux_x86",
    constraint_values = [
        "@platforms//cpu:x86_64",
        "@platforms//os:linux",
    ],
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 1);
    let rule = &build_file.rules[0];
    assert_eq!(rule.name, "linux_x86");
    assert_eq!(rule.rule_type, "platform");

    // Check constraint_values attribute
    if let Some(Value::List(values)) = rule.attributes.get("constraint_values") {
        assert_eq!(values.len(), 2);
    } else {
        panic!("Expected constraint_values to be a list");
    }

    Ok(())
}

#[test]
fn test_platform_with_exec_properties() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create BUILD file with platform using exec_properties
    let build_content = r#"
platform(
    name = "h100_platform",
    constraint_values = [
        "@platforms//cpu:x86_64",
        "@platforms//os:linux",
    ],
    exec_properties = {
        "OSFamily": "Linux",
        "Pool": "h100-executors",
    },
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 1);
    let rule = &build_file.rules[0];
    assert_eq!(rule.name, "h100_platform");
    assert_eq!(rule.rule_type, "platform");

    Ok(())
}
