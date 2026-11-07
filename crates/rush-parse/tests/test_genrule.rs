// Test genrule() functionality
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use rush_parse::{parse_build_file, Value};
use std::fs;
use tempfile::TempDir;

#[test]
fn test_genrule_basic() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    let build_content = r#"
genrule(
    name = "gen_version",
    outs = ["version.h"],
    cmd = "echo '#define VERSION \"1.0.0\"' > $@",
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 1);
    let rule = &build_file.rules[0];
    assert_eq!(rule.name, "gen_version");
    assert_eq!(rule.rule_type, "genrule");

    if let Some(Value::List(outs)) = rule.attributes.get("outs") {
        assert_eq!(outs.len(), 1);
    } else {
        panic!("Expected outs to be a list");
    }

    if let Some(Value::String(cmd)) = rule.attributes.get("cmd") {
        assert!(cmd.contains("VERSION"));
    } else {
        panic!("Expected cmd to be a string");
    }

    Ok(())
}

#[test]
fn test_genrule_with_srcs() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    let build_content = r#"
genrule(
    name = "process_data",
    srcs = ["input.txt"],
    outs = ["output.txt"],
    cmd = "cat $(SRCS) | tr 'a-z' 'A-Z' > $@",
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 1);
    let rule = &build_file.rules[0];
    assert_eq!(rule.name, "process_data");

    if let Some(Value::List(srcs)) = rule.attributes.get("srcs") {
        assert_eq!(srcs.len(), 1);
    }

    Ok(())
}

#[test]
fn test_genrule_multiline_cmd() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    let build_content = r#"
genrule(
    name = "gen_lit_cfg",
    outs = ["lit.cfg.py"],
    cmd = """
cat > $(OUTS) <<EOF
import os
config.name = os.environ["TEST_TARGET"]
config.suffixes = [".mojo"]
EOF
    """,
    testonly = True,
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 1);
    let rule = &build_file.rules[0];
    assert_eq!(rule.name, "gen_lit_cfg");

    if let Some(Value::String(cmd)) = rule.attributes.get("cmd") {
        assert!(cmd.contains("config.name"));
    }

    if let Some(Value::Bool(testonly)) = rule.attributes.get("testonly") {
        assert!(*testonly);
    }

    Ok(())
}

#[test]
fn test_genrule_with_tools() -> anyhow::Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_path = temp_dir.path();

    fs::write(
        workspace_path.join("WORKSPACE"),
        "workspace(name = \"test\")\n",
    )?;

    let build_content = r#"
genrule(
    name = "compile_proto",
    srcs = ["schema.proto"],
    outs = ["schema.pb.cc", "schema.pb.h"],
    cmd = "$(location //tools:protoc) $(SRCS) --cpp_out=$(GENDIR)",
    tools = ["//tools:protoc"],
)
"#;
    fs::write(workspace_path.join("BUILD"), build_content)?;

    let build_file = parse_build_file(&workspace_path.join("BUILD"))?;

    assert_eq!(build_file.rules.len(), 1);
    let rule = &build_file.rules[0];
    assert_eq!(rule.name, "compile_proto");

    if let Some(Value::List(outs)) = rule.attributes.get("outs") {
        assert_eq!(outs.len(), 2);
    }

    if let Some(Value::List(tools)) = rule.attributes.get("tools") {
        assert_eq!(tools.len(), 1);
    }

    Ok(())
}
