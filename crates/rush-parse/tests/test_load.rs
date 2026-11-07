// Test load() functionality
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use rush_parse::parse_build_file;
use std::fs;
use tempfile::TempDir;

#[test]
fn test_load_basic() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create a .bzl file with a macro
    let bzl_content = r#"
def custom_library(name, srcs):
    cc_library(
        name = name,
        srcs = srcs,
    )
"#;
    fs::write(workspace_path.join("macros.bzl"), bzl_content)?;

    // Create BUILD file that loads the macro
    let build_content = r#"
load("//:macros.bzl", "custom_library")

cc_library(
    name = "mylib",
    srcs = ["foo.cc"],
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    // Verify the BUILD file parses successfully
    assert_eq!(build_file.rules.len(), 1);
    assert_eq!(build_file.rules[0].name, "mylib");

    Ok(())
}

#[test]
fn test_load_workspace_relative() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create build_defs directory
    fs::create_dir(workspace_path.join("build_defs"))?;

    // Create a .bzl file with a macro
    let bzl_content = r#"
def my_macro(name):
    cc_library(
        name = name,
    )
"#;
    fs::write(workspace_path.join("build_defs/rules.bzl"), bzl_content)?;

    // Create BUILD file that loads the macro
    let build_content = r#"
load("//build_defs:rules.bzl", "my_macro")

cc_library(
    name = "test",
    srcs = ["test.cc"],
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    // Verify the BUILD file parses successfully
    assert_eq!(build_file.rules.len(), 1);

    Ok(())
}

#[test]
fn test_load_package_relative() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create a .bzl file in the same directory
    let bzl_content = r#"
def helper_macro():
    pass
"#;
    fs::write(workspace_path.join("helpers.bzl"), bzl_content)?;

    // Create BUILD file that loads the macro with package-relative path
    let build_content = r#"
load(":helpers.bzl", "helper_macro")

cc_library(
    name = "mylib",
    srcs = ["foo.cc"],
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    // Verify the BUILD file parses successfully
    assert_eq!(build_file.rules.len(), 1);

    Ok(())
}

#[test]
fn test_load_multiple_symbols() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create a .bzl file with multiple macros
    let bzl_content = r#"
def macro_one(name):
    pass

def macro_two(name, srcs):
    pass

def macro_three():
    pass
"#;
    fs::write(workspace_path.join("defs.bzl"), bzl_content)?;

    // Create BUILD file that loads multiple symbols
    let build_content = r#"
load(":defs.bzl", "macro_one", "macro_two", "macro_three")

cc_library(
    name = "lib",
    srcs = ["lib.cc"],
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    // Verify the BUILD file parses successfully
    assert_eq!(build_file.rules.len(), 1);

    Ok(())
}
