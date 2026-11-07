// Test alias() functionality
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use rush_parse::{parse_build_file, Value};
use std::fs;
use tempfile::TempDir;

#[test]
fn test_alias_basic() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create BUILD file with alias
    let build_content = r#"
alias(
    name = "has_gpu",
    actual = "@mojo_gpu_toolchains//:has_gpu",
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 1);
    let rule = &build_file.rules[0];
    assert_eq!(rule.name, "has_gpu");
    assert_eq!(rule.rule_type, "alias");

    // Check actual attribute
    if let Some(Value::String(actual)) = rule.attributes.get("actual") {
        assert_eq!(actual, "@mojo_gpu_toolchains//:has_gpu");
    } else {
        panic!("Expected actual to be a string");
    }

    Ok(())
}

#[test]
fn test_alias_with_select() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create BUILD file with alias using select()
    let build_content = r#"
alias(
    name = "uv",
    actual = select({
        "//:linux_aarch64": "@uv_linux_aarch64//:uv",
        "//:linux_x86_64": "@uv_linux_x86_64//:uv",
        "@platforms//os:macos": "@uv_darwin_aarch64//:uv",
    }),
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 1);
    let rule = &build_file.rules[0];
    assert_eq!(rule.name, "uv");
    assert_eq!(rule.rule_type, "alias");

    // Check actual is a Select
    if let Some(Value::Select(conditions)) = rule.attributes.get("actual") {
        assert_eq!(conditions.len(), 3);
        assert!(conditions.contains_key("//:linux_aarch64"));
        assert!(conditions.contains_key("//:linux_x86_64"));
        assert!(conditions.contains_key("@platforms//os:macos"));
    } else {
        panic!("Expected actual to be a Select");
    }

    Ok(())
}

#[test]
fn test_alias_with_visibility() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create BUILD file with alias with visibility
    let build_content = r#"
alias(
    name = "public_alias",
    actual = "//internal:target",
    visibility = ["//visibility:public"],
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 1);
    let rule = &build_file.rules[0];
    assert_eq!(rule.name, "public_alias");
    assert_eq!(rule.rule_type, "alias");

    // Check visibility attribute
    if let Some(Value::List(visibility)) = rule.attributes.get("visibility") {
        assert_eq!(visibility.len(), 1);
        if let Value::String(s) = &visibility[0] {
            assert_eq!(s, "//visibility:public");
        }
    } else {
        panic!("Expected visibility to be a list");
    }

    Ok(())
}

#[test]
fn test_multiple_aliases() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create BUILD file with multiple aliases
    let build_content = r#"
alias(
    name = "alias1",
    actual = "//target1",
)

alias(
    name = "alias2",
    actual = "//target2",
)

alias(
    name = "alias3",
    actual = "@external//:target",
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 3);

    assert_eq!(build_file.rules[0].name, "alias1");
    assert_eq!(build_file.rules[0].rule_type, "alias");

    assert_eq!(build_file.rules[1].name, "alias2");
    assert_eq!(build_file.rules[1].rule_type, "alias");

    assert_eq!(build_file.rules[2].name, "alias3");
    assert_eq!(build_file.rules[2].rule_type, "alias");

    Ok(())
}

#[test]
fn test_alias_with_tags() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create BUILD file with alias with tags
    let build_content = r#"
alias(
    name = "gpu_alias",
    actual = "@toolchains//:gpu",
    tags = ["manual", "gpu"],
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 1);
    let rule = &build_file.rules[0];

    // Check tags attribute
    if let Some(Value::List(tags)) = rule.attributes.get("tags") {
        assert_eq!(tags.len(), 2);
        if let Value::String(s) = &tags[0] {
            assert_eq!(s, "manual");
        }
        if let Value::String(s) = &tags[1] {
            assert_eq!(s, "gpu");
        }
    } else {
        panic!("Expected tags to be a list");
    }

    Ok(())
}
