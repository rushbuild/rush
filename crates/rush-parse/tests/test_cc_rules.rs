// Test C++ rules (cc_library, cc_binary)
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use rush_parse::{parse_build_file, Value};
use std::fs;
use tempfile::TempDir;

#[test]
fn test_cc_library_basic() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    let build_content = r#"
cc_library(
    name = "mylib",
    srcs = ["mylib.cc"],
    hdrs = ["mylib.h"],
    deps = ["//base:logging"],
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 1);
    let rule = &build_file.rules[0];
    assert_eq!(rule.name, "mylib");
    assert_eq!(rule.rule_type, "cc_library");

    if let Some(Value::List(srcs)) = rule.attributes.get("srcs") {
        assert_eq!(srcs.len(), 1);
    }

    if let Some(Value::List(hdrs)) = rule.attributes.get("hdrs") {
        assert_eq!(hdrs.len(), 1);
    }

    if let Some(Value::List(deps)) = rule.attributes.get("deps") {
        assert_eq!(deps.len(), 1);
    }

    Ok(())
}

#[test]
fn test_cc_binary_basic() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    let build_content = r#"
cc_binary(
    name = "main",
    srcs = ["main.cc"],
    deps = [":mylib"],
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 1);
    let rule = &build_file.rules[0];
    assert_eq!(rule.name, "main");
    assert_eq!(rule.rule_type, "cc_binary");

    Ok(())
}

#[test]
fn test_cc_library_with_select() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    let build_content = r#"
cc_library(
    name = "platform_lib",
    srcs = ["common.cc"] + select({
        "//:linux": ["linux.cc"],
        "//:macos": ["macos.cc"],
    }),
    linkopts = select({
        "@platforms//os:linux": ["-Wl,-rpath,$ORIGIN/../lib"],
        "@platforms//os:macos": ["-Wl,-rpath,@loader_path/../lib"],
    }),
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 1);
    let rule = &build_file.rules[0];
    assert_eq!(rule.name, "platform_lib");
    assert_eq!(rule.rule_type, "cc_library");

    Ok(())
}

#[test]
fn test_cc_library_with_copts() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    let build_content = r#"
cc_library(
    name = "optimized_lib",
    srcs = ["lib.cc"],
    copts = ["-O3", "-march=native"],
    defines = ["NDEBUG"],
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 1);
    let rule = &build_file.rules[0];

    if let Some(Value::List(copts)) = rule.attributes.get("copts") {
        assert_eq!(copts.len(), 2);
    }

    if let Some(Value::List(defines)) = rule.attributes.get("defines") {
        assert_eq!(defines.len(), 1);
    }

    Ok(())
}

#[test]
fn test_cc_test() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    let build_content = r#"
cc_test(
    name = "mylib_test",
    srcs = ["mylib_test.cc"],
    deps = [
        ":mylib",
        "@gtest//:gtest_main",
    ],
    size = "small",
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 1);
    let rule = &build_file.rules[0];
    assert_eq!(rule.name, "mylib_test");
    assert_eq!(rule.rule_type, "cc_test");

    if let Some(Value::String(size)) = rule.attributes.get("size") {
        assert_eq!(size, "small");
    }

    Ok(())
}

#[test]
fn test_cc_import() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    let build_content = r#"
cc_import(
    name = "prebuilt",
    shared_library = select({
        "//:linux_x86_64": "@prebuilt_libs//:lib/linux_x86_64/libfoo.so",
        "//:macos": "@prebuilt_libs//:lib/macos/libfoo.dylib",
    }),
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 1);
    let rule = &build_file.rules[0];
    assert_eq!(rule.name, "prebuilt");
    assert_eq!(rule.rule_type, "cc_import");

    if let Some(Value::Select(_)) = rule.attributes.get("shared_library") {
        // Verify it's a select
    } else {
        panic!("Expected shared_library to be a Select");
    }

    Ok(())
}
