// Test select() functionality
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use rush_parse::{parse_build_file, Value};
use std::fs;
use tempfile::TempDir;

#[test]
fn test_select_basic() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create BUILD file with select()
    let build_content = r#"
cc_library(
    name = "mylib",
    srcs = select({
        "@platforms//os:linux": ["linux.cc"],
        "@platforms//os:osx": ["macos.cc"],
        "//conditions:default": [],
    }),
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 1);

    let rule = &build_file.rules[0];
    assert_eq!(rule.name, "mylib");
    assert_eq!(rule.rule_type, "cc_library");

    // Check srcs is a Select
    if let Some(Value::Select(conditions)) = rule.attributes.get("srcs") {
        assert_eq!(conditions.len(), 3);

        // Check linux condition
        if let Some(Value::List(linux_srcs)) = conditions.get("@platforms//os:linux") {
            assert_eq!(linux_srcs.len(), 1);
            if let Value::String(s) = &linux_srcs[0] {
                assert_eq!(s, "linux.cc");
            } else {
                panic!("Expected String in linux condition");
            }
        } else {
            panic!("Expected linux condition to be a List");
        }

        // Check macos condition
        if let Some(Value::List(macos_srcs)) = conditions.get("@platforms//os:osx") {
            assert_eq!(macos_srcs.len(), 1);
            if let Value::String(s) = &macos_srcs[0] {
                assert_eq!(s, "macos.cc");
            } else {
                panic!("Expected String in macos condition");
            }
        } else {
            panic!("Expected macos condition to be a List");
        }

        // Check default condition
        if let Some(Value::List(default_srcs)) = conditions.get("//conditions:default") {
            assert_eq!(default_srcs.len(), 0);
        } else {
            panic!("Expected default condition to be a List");
        }
    } else {
        panic!("Expected srcs to be a Select");
    }

    Ok(())
}

#[test]
fn test_select_with_strings() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create BUILD file with select() returning strings
    let build_content = r#"
config_setting(name = "debug", define_values = {"mode": "debug"})
config_setting(name = "release", define_values = {"mode": "release"})
cc_library(
    name = "mylib",
    defines = select({
        ":debug": ["DEBUG"],
        ":release": ["NDEBUG"],
    }),
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 3);

    let rule = build_file
        .rules
        .iter()
        .find(|rule| rule.name == "mylib")
        .expect("mylib rule");

    // Check defines is a Select
    if let Some(Value::Select(conditions)) = rule.attributes.get("defines") {
        assert_eq!(conditions.len(), 2);
        assert!(conditions.contains_key(":debug"));
        assert!(conditions.contains_key(":release"));
    } else {
        panic!("Expected defines to be a Select");
    }

    Ok(())
}

#[test]
fn test_select_nested_in_list() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create BUILD file with select() nested in list concatenation
    // Note: For now we just test that select() parses, not list concat
    let build_content = r#"
config_setting(name = "opt", define_values = {"mode": "opt"})
config_setting(name = "dbg", define_values = {"mode": "dbg"})
cc_library(
    name = "mylib",
    copts = select({
        ":opt": ["-O3"],
        ":dbg": ["-g"],
    }),
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 3);

    let rule = build_file
        .rules
        .iter()
        .find(|rule| rule.name == "mylib")
        .expect("mylib rule");

    // Check copts is a Select
    if let Some(Value::Select(conditions)) = rule.attributes.get("copts") {
        assert_eq!(conditions.len(), 2);

        if let Some(Value::List(opts)) = conditions.get(":opt") {
            assert_eq!(opts.len(), 1);
            if let Value::String(s) = &opts[0] {
                assert_eq!(s, "-O3");
            }
        }
    } else {
        panic!("Expected copts to be a Select");
    }

    Ok(())
}
