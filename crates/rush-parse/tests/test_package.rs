// Test package() functionality
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use rush_parse::{parse_build_file_with_starlark, Value};
use std::fs;
use tempfile::TempDir;

#[test]
fn test_package_default_visibility() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    // Create BUILD file with package statement
    let build_content = r#"
package(default_visibility = ["//visibility:public"])

cc_library(
    name = "lib",
    srcs = ["lib.cc"],
)

cc_library(
    name = "private_lib",
    visibility = ["//visibility:private"],
)

cc_library(
    name = "none_lib",
    visibility = None,
)

cc_library(
    name = "empty_lib",
    visibility = [],
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = rush_incremental::tracked::test_scope(workspace_path, || {
        parse_build_file_with_starlark(&workspace_path.join("BUILD"))
    })?;

    let lib = build_file
        .rules
        .iter()
        .find(|rule| rule.name == "lib")
        .expect("lib rule");
    assert!(matches!(
        lib.attributes.get("visibility"),
        Some(Value::List(values))
            if matches!(values.as_slice(), [Value::String(value)]
                if value == "//visibility:public")
    ));

    let private_lib = build_file
        .rules
        .iter()
        .find(|rule| rule.name == "private_lib")
        .expect("private_lib rule");
    assert!(matches!(
        private_lib.attributes.get("visibility"),
        Some(Value::List(values))
            if matches!(values.as_slice(), [Value::String(value)]
                if value == "//visibility:private")
    ));

    let none_lib = build_file
        .rules
        .iter()
        .find(|rule| rule.name == "none_lib")
        .expect("None visibility rule");
    assert!(matches!(
        none_lib.attributes.get("visibility"),
        Some(Value::List(values))
            if matches!(values.as_slice(), [Value::String(value)]
                if value == "//visibility:public")
    ));

    let empty_lib = build_file
        .rules
        .iter()
        .find(|rule| rule.name == "empty_lib")
        .expect("empty visibility rule");
    assert!(matches!(
        empty_lib.attributes.get("visibility"),
        Some(Value::List(values)) if values.is_empty()
    ));

    Ok(())
}
