// Test glob() functionality
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use rush_parse::{parse_build_file, Value};
use std::fs;
use tempfile::TempDir;

#[test]
fn test_glob_basic() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create some source files
    fs::write(workspace_path.join("foo.cc"), "// foo")?;
    fs::write(workspace_path.join("bar.cc"), "// bar")?;
    fs::write(workspace_path.join("baz.h"), "// header")?;

    // Create BUILD file with glob()
    let build_content = r#"
cc_library(
    name = "mylib",
    srcs = glob(["*.cc"]),
    hdrs = glob(["*.h"]),
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 1);

    let rule = &build_file.rules[0];
    assert_eq!(rule.name, "mylib");
    assert_eq!(rule.rule_type, "cc_library");

    // Check srcs expanded from glob
    if let Some(Value::List(srcs)) = rule.attributes.get("srcs") {
        assert_eq!(srcs.len(), 2);

        let mut src_files: Vec<String> = srcs
            .iter()
            .filter_map(|v| {
                if let Value::String(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .collect();
        src_files.sort();

        assert_eq!(src_files, vec!["bar.cc", "foo.cc"]);
    } else {
        panic!("Expected srcs to be a List");
    }

    // Check hdrs expanded from glob
    if let Some(Value::List(hdrs)) = rule.attributes.get("hdrs") {
        assert_eq!(hdrs.len(), 1);
        if let Value::String(s) = &hdrs[0] {
            assert_eq!(s, "baz.h");
        } else {
            panic!("Expected header to be a String");
        }
    } else {
        panic!("Expected hdrs to be a List");
    }

    Ok(())
}

#[test]
fn test_glob_with_exclude() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create source files including test files
    fs::write(workspace_path.join("foo.cc"), "// foo")?;
    fs::write(workspace_path.join("bar.cc"), "// bar")?;
    fs::write(workspace_path.join("foo_test.cc"), "// test")?;

    // Create BUILD file with glob() and exclude
    let build_content = r#"
cc_library(
    name = "mylib",
    srcs = glob(["*.cc"], exclude = ["*_test.cc"]),
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 1);

    let rule = &build_file.rules[0];

    // Check srcs excludes test files
    if let Some(Value::List(srcs)) = rule.attributes.get("srcs") {
        assert_eq!(srcs.len(), 2);

        let mut src_files: Vec<String> = srcs
            .iter()
            .filter_map(|v| {
                if let Value::String(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .collect();
        src_files.sort();

        // Should NOT include foo_test.cc
        assert_eq!(src_files, vec!["bar.cc", "foo.cc"]);
        assert!(!src_files.contains(&"foo_test.cc".to_string()));
    } else {
        panic!("Expected srcs to be a List");
    }

    Ok(())
}

#[test]
fn test_glob_empty() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create BUILD file with glob() that matches nothing
    let build_content = r#"
cc_library(
    name = "mylib",
    srcs = glob(["*.rs"]),
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 1);

    let rule = &build_file.rules[0];

    // Check srcs is empty list
    if let Some(Value::List(srcs)) = rule.attributes.get("srcs") {
        assert_eq!(srcs.len(), 0);
    } else {
        panic!("Expected srcs to be a List");
    }

    Ok(())
}
