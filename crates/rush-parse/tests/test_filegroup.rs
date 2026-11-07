// Test filegroup() functionality
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use rush_parse::{parse_build_file, Value};
use std::fs;
use tempfile::TempDir;

#[test]
fn test_filegroup_basic() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create BUILD file with filegroup
    let build_content = r#"
filegroup(
    name = "kernel_sources",
    srcs = glob(["kernels/*.mojo"]),
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 1);
    let rule = &build_file.rules[0];
    assert_eq!(rule.name, "kernel_sources");
    assert_eq!(rule.rule_type, "filegroup");

    Ok(())
}

#[test]
fn test_filegroup_with_data() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create BUILD file with filegroup including data
    let build_content = r#"
filegroup(
    name = "test_data",
    srcs = ["data.json", "config.yaml"],
    data = ["extra_file.txt"],
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 1);
    let rule = &build_file.rules[0];
    assert_eq!(rule.name, "test_data");
    assert_eq!(rule.rule_type, "filegroup");

    // Check srcs attribute
    if let Some(Value::List(srcs)) = rule.attributes.get("srcs") {
        assert_eq!(srcs.len(), 2);
    } else {
        panic!("Expected srcs to be a list");
    }

    // Check data attribute
    if let Some(Value::List(data)) = rule.attributes.get("data") {
        assert_eq!(data.len(), 1);
    } else {
        panic!("Expected data to be a list");
    }

    Ok(())
}

#[test]
fn test_filegroup_with_visibility() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create BUILD file with filegroup with visibility
    let build_content = r#"
filegroup(
    name = "public_files",
    srcs = ["file1.txt", "file2.txt"],
    visibility = ["//visibility:public"],
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 1);
    let rule = &build_file.rules[0];
    assert_eq!(rule.name, "public_files");

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
