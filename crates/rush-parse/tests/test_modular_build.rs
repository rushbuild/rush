// Test parsing real Modular BUILD.bazel file
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use rush_parse::parse_build_file;

#[test]
fn test_parse_modular_root_build() -> anyhow::Result<()> {
    let modular_root = std::env::var_os("RUSH_MODULAR_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("../modular"));
    let modular_build_path = modular_root.join("BUILD.bazel");
    let modular_build = modular_build_path.as_path();

    if !modular_build.exists() {
        eprintln!("Skipping: Modular BUILD.bazel not found");
        return Ok(());
    }

    println!("Parsing Modular BUILD.bazel...");

    let build_file = parse_build_file(modular_build)?;

    println!("✅ Successfully parsed BUILD.bazel!");
    println!("Rules found: {}", build_file.rules.len());

    // Group by rule type
    let mut rule_counts = std::collections::HashMap::new();
    for rule in &build_file.rules {
        *rule_counts.entry(&rule.rule_type).or_insert(0) += 1;
    }

    println!("\nRule types:");
    let mut types: Vec<_> = rule_counts.iter().collect();
    types.sort_by_key(|(_, count)| std::cmp::Reverse(**count));
    for (rule_type, count) in types {
        println!("  {}: {}", rule_type, count);
    }

    println!("\nFirst 10 rules:");
    for (i, rule) in build_file.rules.iter().take(10).enumerate() {
        println!("  {}. {} ({})", i + 1, rule.name, rule.rule_type);
    }

    // Expect at least some rules
    assert!(
        !build_file.rules.is_empty(),
        "Should have parsed some rules"
    );

    Ok(())
}
