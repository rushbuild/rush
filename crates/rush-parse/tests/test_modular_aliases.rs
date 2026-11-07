// Test alias() patterns from Modular project
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use rush_parse::parse_build_file;
use std::fs;
use tempfile::TempDir;

#[test]
fn test_modular_style_aliases() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    // Create WORKSPACE file
    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"modular\")\n",
    )?;

    // Create BUILD file with Modular-style aliases (simplified)
    let build_content = r#"
alias(
    name = "has_gpu",
    actual = "@mojo_gpu_toolchains//:has_gpu",
)

alias(
    name = "has_multi_gpu",
    actual = "@mojo_gpu_toolchains//:has_multi_gpu",
)

alias(
    name = "nvidia_gpu",
    actual = "@mojo_gpu_toolchains//:nvidia_gpu",
)

alias(
    name = "lint",
    actual = "//bazel/lint",
)

alias(
    name = "format",
    actual = "//bazel/lint:fix",
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    // Parse the BUILD file
    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    // Should parse 5 alias rules
    assert_eq!(build_file.rules.len(), 5);

    // Verify all are alias rules
    for rule in &build_file.rules {
        assert_eq!(rule.rule_type, "alias");
    }

    // Verify names
    let names: Vec<&str> = build_file.rules.iter().map(|r| r.name.as_str()).collect();
    assert!(names.contains(&"has_gpu"));
    assert!(names.contains(&"has_multi_gpu"));
    assert!(names.contains(&"nvidia_gpu"));
    assert!(names.contains(&"lint"));
    assert!(names.contains(&"format"));

    Ok(())
}
