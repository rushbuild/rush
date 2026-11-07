// Test config_setting() functionality
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use rush_parse::{parse_build_file, Value};
use std::fs;
use tempfile::TempDir;

#[test]
fn test_config_setting_basic() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create BUILD file with config_setting
    let build_content = r#"
config_setting(
    name = "linux_x86_64",
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
    assert_eq!(rule.name, "linux_x86_64");
    assert_eq!(rule.rule_type, "config_setting");

    // Check constraint_values attribute
    if let Some(Value::List(values)) = rule.attributes.get("constraint_values") {
        assert_eq!(values.len(), 2);
    } else {
        panic!("Expected constraint_values to be a list");
    }

    Ok(())
}

#[test]
fn test_config_setting_with_flag_values() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create BUILD file with config_setting using flag_values
    let build_content = r#"
config_setting(
    name = "asan_linux",
    constraint_values = ["@platforms//os:linux"],
    flag_values = {"//:modular_config": "asan"},
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 1);
    let rule = &build_file.rules[0];
    assert_eq!(rule.name, "asan_linux");
    assert_eq!(rule.rule_type, "config_setting");

    Ok(())
}

#[test]
fn test_config_setting_with_values() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create BUILD file with config_setting using values
    let build_content = r#"
config_setting(
    name = "production_build",
    values = {"features": "UNREACHABLE"},
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 1);
    let rule = &build_file.rules[0];
    assert_eq!(rule.name, "production_build");

    Ok(())
}

#[test]
fn test_multiple_config_settings() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create BUILD file with multiple config_settings
    let build_content = r#"
config_setting(
    name = "linux_x86_64",
    constraint_values = [
        "@platforms//cpu:x86_64",
        "@platforms//os:linux",
    ],
)

config_setting(
    name = "linux_aarch64",
    constraint_values = [
        "@platforms//cpu:aarch64",
        "@platforms//os:linux",
    ],
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 2);

    let names: Vec<&str> = build_file.rules.iter().map(|r| r.name.as_str()).collect();
    assert!(names.contains(&"linux_x86_64"));
    assert!(names.contains(&"linux_aarch64"));

    // Both should be config_setting rules
    for rule in &build_file.rules {
        assert_eq!(rule.rule_type, "config_setting");
    }

    Ok(())
}
