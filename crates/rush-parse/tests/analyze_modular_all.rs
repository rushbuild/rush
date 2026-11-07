// Analyze all BUILD files in Modular project
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use rush_parse::parse_build_file;
use std::collections::HashMap;
use walkdir::WalkDir;

#[test]
fn analyze_all_modular_builds() -> anyhow::Result<()> {
    let modular_root_path = std::env::var_os("RUSH_MODULAR_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("../modular"));
    let modular_root = modular_root_path.as_path();

    if !modular_root.exists() {
        eprintln!("Skipping: Modular directory not found");
        return Ok(());
    }

    println!("\n🔍 Analyzing all BUILD files in Modular project...\n");

    let mut total = 0;
    let mut successful = 0;
    let mut failed = 0;
    let mut all_rule_types: HashMap<String, usize> = HashMap::new();
    let mut errors: Vec<(String, String)> = Vec::new();

    // Find all BUILD files
    for entry in WalkDir::new(modular_root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy();
            name == "BUILD" || name == "BUILD.bazel"
        })
    {
        total += 1;
        let path = entry.path();
        let rel_path = path
            .strip_prefix(modular_root)
            .unwrap_or(path)
            .to_string_lossy();

        print!("  Parsing {:<60} ", rel_path);

        match parse_build_file(path) {
            Ok(build_file) => {
                successful += 1;
                println!("✅ {} rules", build_file.rules.len());

                // Collect rule types
                for rule in &build_file.rules {
                    *all_rule_types.entry(rule.rule_type.clone()).or_insert(0) += 1;
                }
            }
            Err(e) => {
                failed += 1;
                println!("❌ FAILED");

                let error_msg = format!("{:?}", e);
                // Extract just the main error message
                let short_error = error_msg.lines().next().unwrap_or(&error_msg);
                errors.push((rel_path.to_string(), short_error.to_string()));
            }
        }
    }

    println!("\n{}", "=".repeat(80));
    println!("📊 SUMMARY");
    println!("{}", "=".repeat(80));
    println!("Total BUILD files: {}", total);
    println!(
        "Successfully parsed: {} ({:.1}%)",
        successful,
        (successful as f64 / total as f64) * 100.0
    );
    println!(
        "Failed to parse: {} ({:.1}%)",
        failed,
        (failed as f64 / total as f64) * 100.0
    );

    if !all_rule_types.is_empty() {
        println!("\n📋 Rule types found across all files:");
        let mut types: Vec<_> = all_rule_types.iter().collect();
        types.sort_by_key(|(_, count)| std::cmp::Reverse(**count));
        for (rule_type, count) in types {
            println!("  {:<30} {}", rule_type, count);
        }
    }

    if !errors.is_empty() {
        println!("\n❌ PARSING FAILURES ({} files):", errors.len());
        for (path, error) in &errors {
            println!("\n  {}", path);
            println!("    {}", error);
        }
    }

    println!("\n{}", "=".repeat(80));

    // Assert we can parse at least some files
    assert!(
        successful > 0,
        "Should be able to parse at least some BUILD files"
    );

    Ok(())
}
