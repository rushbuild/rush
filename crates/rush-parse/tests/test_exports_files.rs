// Test exports_files() functionality
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use rush_parse::{parse_build_file_with_loader, parse_build_file_with_starlark, BuildFile, Value};
use rush_starlark::BzlLoader;
use std::fs;
use tempfile::TempDir;

fn source_visibility(build_file: &BuildFile, name: &str) -> Option<Vec<String>> {
    let source = build_file
        .rules
        .iter()
        .find(|rule| rule.rule_type == "source_file" && rule.name == name)?;
    let Value::List(specifications) = source.attributes.get("visibility")? else {
        return None;
    };
    specifications
        .iter()
        .map(|specification| match specification {
            Value::String(value) => Some(value.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn test_exports_files_basic() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create BUILD file with exports_files
    let build_content = r#"
package(default_visibility = ["//visibility:private"])

exports_files([
    "load_bazel_tools.py",
    "lsan-suppressions.txt",
])
native.exports_files(["explicit-none.txt"], visibility = None)

cc_library(
    name = "lib",
    srcs = ["lib.cc"],
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = rush_incremental::tracked::test_scope(workspace_path, || {
        parse_build_file_with_starlark(&workspace_path.join("BUILD"))
    })?;

    let source_files: Vec<_> = build_file
        .rules
        .iter()
        .filter(|rule| rule.rule_type == "source_file")
        .map(|rule| rule.name.as_str())
        .collect();
    assert_eq!(
        source_files,
        vec![
            "load_bazel_tools.py",
            "lsan-suppressions.txt",
            "explicit-none.txt"
        ]
    );
    for name in [
        "load_bazel_tools.py",
        "lsan-suppressions.txt",
        "explicit-none.txt",
    ] {
        assert_eq!(
            source_visibility(&build_file, name),
            Some(vec!["//visibility:public".to_string()]),
            "{name} must use exports_files public default"
        );
    }
    assert!(build_file
        .rules
        .iter()
        .any(|rule| rule.rule_type == "cc_library" && rule.name == "lib"));

    Ok(())
}

#[test]
fn test_exports_files_srcs_keyword() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    let build_content = r#"
exports_files(
    srcs = [
        "allocator.h",
        "allocator_registry.h",
        "tensor_interface.h",
    ],
    visibility = ["//visibility:public"],
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    let build_file = rush_incremental::tracked::test_scope(workspace_path, || {
        parse_build_file_with_starlark(&workspace_path.join("BUILD"))
    })?;

    let source_files: Vec<_> = build_file
        .rules
        .iter()
        .filter(|rule| rule.rule_type == "source_file")
        .map(|rule| rule.name.as_str())
        .collect();
    assert_eq!(
        source_files,
        vec!["allocator.h", "allocator_registry.h", "tensor_interface.h"]
    );
    for name in source_files {
        assert_eq!(
            source_visibility(&build_file, name),
            Some(vec!["//visibility:public".to_string()])
        );
    }

    Ok(())
}

#[test]
fn test_exports_files_lazy_loader_path_collects_source_file_rules() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;
    fs::write(
        workspace_path.join("defs.bzl"),
        r#"
def helper():
    native.filegroup(name = "helper_group", srcs = [])
"#,
    )?;

    let build_content = r#"
load("//:defs.bzl", "helper")

exports_files(["a.h"])
native.exports_files(srcs = ["b.h"], visibility = [])

helper()
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    let mut loader = BzlLoader::new(workspace_path.to_path_buf());
    loader.set_main_workspace_name(Some("test".to_string()));
    let build_file = rush_incremental::tracked::test_scope(workspace_path, || {
        parse_build_file_with_loader(&workspace_path.join("BUILD"), loader)
    })?;

    let mut source_files: Vec<_> = build_file
        .rules
        .iter()
        .filter(|rule| rule.rule_type == "source_file")
        .map(|rule| rule.name.as_str())
        .collect();
    source_files.sort_unstable();
    assert_eq!(source_files, vec!["a.h", "b.h"]);
    assert_eq!(
        source_visibility(&build_file, "a.h"),
        Some(vec!["//visibility:public".to_string()])
    );
    assert_eq!(source_visibility(&build_file, "b.h"), Some(vec![]));

    assert!(build_file
        .rules
        .iter()
        .any(|rule| rule.rule_type == "filegroup" && rule.name == "helper_group"));

    Ok(())
}

#[test]
fn test_exports_files_explicit_visibility_survives_parse_bridge() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();
    fs::write(workspace_path.join("WORKSPACE"), "")?;
    fs::write(
        workspace_path.join("BUILD"),
        r#"
exports_files(["empty.txt"], visibility = [])
exports_files(["private.txt"], visibility = ["//visibility:private"])
exports_files(["granted.txt"], visibility = ["//consumer:__pkg__"])
"#,
    )?;

    let build_file = rush_incremental::tracked::test_scope(workspace_path, || {
        parse_build_file_with_starlark(&workspace_path.join("BUILD"))
    })?;
    assert_eq!(source_visibility(&build_file, "empty.txt"), Some(vec![]));
    assert_eq!(
        source_visibility(&build_file, "private.txt"),
        Some(vec!["//visibility:private".to_string()])
    );
    assert_eq!(
        source_visibility(&build_file, "granted.txt"),
        Some(vec!["//consumer:__pkg__".to_string()])
    );
    Ok(())
}

#[test]
fn test_exports_files_conflicting_duplicate_visibility_fails() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();
    fs::write(workspace_path.join("WORKSPACE"), "")?;
    fs::write(
        workspace_path.join("BUILD"),
        r#"
exports_files(["x.txt"], visibility = ["//visibility:public"])
exports_files(["x.txt"], visibility = ["//visibility:private"])
"#,
    )?;

    let error = rush_incremental::tracked::test_scope(workspace_path, || {
        parse_build_file_with_starlark(&workspace_path.join("BUILD"))
    })
    .expect_err("conflicting exports_files visibility must fail");
    assert!(
        format!("{error:#}").contains("visibility for exported file 'x.txt' declared twice"),
        "unexpected error: {error:#}"
    );
    Ok(())
}
