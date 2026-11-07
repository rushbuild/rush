// Test list comprehension functionality
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use rush_parse::parse_build_file;
use std::fs;
use tempfile::TempDir;

#[test]
fn test_list_comp_basic() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create BUILD file with simple list comprehension
    let build_content = r#"
[
    alias(
        name = "{}_target".format(item),
        actual = "//:{}".format(item),
    )
    for item in ["foo", "bar", "baz"]
]
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    // Should generate 3 alias rules
    assert_eq!(build_file.rules.len(), 3);

    // Check generated names
    let names: Vec<&str> = build_file.rules.iter().map(|r| r.name.as_str()).collect();
    assert!(names.contains(&"foo_target"));
    assert!(names.contains(&"bar_target"));
    assert!(names.contains(&"baz_target"));

    // All should be alias rules
    for rule in &build_file.rules {
        assert_eq!(rule.rule_type, "alias");
    }

    Ok(())
}

#[test]
fn test_list_comp_with_cc_library() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create BUILD file with cc_library list comprehension
    let build_content = r#"
[
    cc_library(
        name = "{}_lib".format(name),
        srcs = ["{}.cc".format(name)],
    )
    for name in ["foo", "bar"]
]
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    // Should generate 2 cc_library rules
    assert_eq!(build_file.rules.len(), 2);

    let names: Vec<&str> = build_file.rules.iter().map(|r| r.name.as_str()).collect();
    assert!(names.contains(&"foo_lib"));
    assert!(names.contains(&"bar_lib"));

    Ok(())
}

#[test]
fn test_list_comp_single_item() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create BUILD file with single item list comprehension
    let build_content = r#"
[
    alias(
        name = "only_one",
        actual = "//target",
    )
    for item in ["single"]
]
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 1);
    assert_eq!(build_file.rules[0].name, "only_one");

    Ok(())
}
