// Integration test: Build hello_cpp example end-to-end
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use anyhow::{anyhow, Result};
use rush_build::{cache_paths, BuildOrchestrator};
use std::process::Command;

#[tokio::test]
async fn test_build_hello_cpp_example() -> Result<()> {
    println!("\n╔════════════════════════════════════════════════════════════╗");
    println!("║     Integration Test: Build hello_cpp with Rush           ║");
    println!("╚════════════════════════════════════════════════════════════╝\n");

    // Create BuildOrchestrator (will find workspace root automatically)
    println!("🔧 Creating BuildOrchestrator...");
    let orchestrator = BuildOrchestrator::new()?;

    let workspace_root = orchestrator.workspace_root();
    println!("📂 Workspace: {}", workspace_root.display());

    // Verify hello_cpp example exists
    let hello_cpp_path = workspace_root.join("examples").join("hello_cpp");
    if !hello_cpp_path.exists() {
        return Err(anyhow!(
            "hello_cpp example not found at: {}",
            hello_cpp_path.display()
        ));
    }

    let build_path = hello_cpp_path.join("BUILD");
    if !build_path.exists() {
        return Err(anyhow!("BUILD file not found at: {}", build_path.display()));
    }

    println!(
        "✅ Found hello_cpp example at: {}",
        hello_cpp_path.display()
    );
    println!();

    // Clean previous build outputs (in workspace root, not package dir)
    let output_dir = cache_paths::workspace_output_root(&workspace_root);
    if output_dir.exists() {
        println!("🧹 Cleaning previous build outputs...");
        std::fs::remove_dir_all(&output_dir)?;
    }

    // Build the hello target with dependencies
    println!("\n🔨 Building //examples/hello_cpp:hello\n");

    let target_patterns = vec!["//examples/hello_cpp:hello".to_string()];

    match orchestrator.build_with_dependencies(target_patterns).await {
        Ok(()) => {
            println!("\n✅ Build completed successfully!\n");
        }
        Err(e) => {
            eprintln!("\n❌ Build failed: {}\n", e);
            return Err(e);
        }
    }

    // Verify artifacts were created
    println!("🔍 Verifying build artifacts...\n");

    // Check for library artifact
    let lib_path = output_dir.join("lib").join("libhello_lib.a");
    if lib_path.exists() {
        println!("✅ Library artifact: {}", lib_path.display());
    } else {
        eprintln!("❌ Missing library: {}", lib_path.display());
        return Err(anyhow!("Library artifact not created"));
    }

    // Check for binary artifact
    let bin_path = output_dir.join("bin").join("hello");
    if bin_path.exists() {
        println!("✅ Binary artifact: {}", bin_path.display());
    } else {
        eprintln!("❌ Missing binary: {}", bin_path.display());
        return Err(anyhow!("Binary artifact not created"));
    }

    // Check for object files
    let obj_dir = output_dir.join("obj");
    if obj_dir.exists() {
        let obj_count = std::fs::read_dir(&obj_dir)?.count();
        println!(
            "✅ Object files: {} created in {}",
            obj_count,
            obj_dir.display()
        );
    }

    println!();

    // Execute the binary to verify it works
    println!("🚀 Running the compiled binary...\n");

    let output = Command::new(&bin_path)
        .current_dir(&hello_cpp_path)
        .output()?;

    if !output.status.success() {
        eprintln!("❌ Binary execution failed with status: {}", output.status);
        eprintln!("stderr: {}", String::from_utf8_lossy(&output.stderr));
        return Err(anyhow!("Binary execution failed"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    println!("📤 Output: {}", stdout.trim());

    // Verify expected output
    if stdout.contains("Hello from Rush!") {
        println!("\n✅ Binary produced expected output!");
    } else {
        eprintln!(
            "\n❌ Unexpected output. Expected 'Hello from Rush!', got: {}",
            stdout
        );
        return Err(anyhow!("Binary produced unexpected output"));
    }

    println!("\n╔════════════════════════════════════════════════════════════╗");
    println!("║          🎉 END-TO-END BUILD SUCCESSFUL! 🎉                ║");
    println!("╚════════════════════════════════════════════════════════════╝");
    println!("\n📊 What This Proves:");
    println!("  ✅ BUILD file parsing works");
    println!("  ✅ Rule instantiation works");
    println!("  ✅ Dependency resolution works");
    println!("  ✅ Topological sort works");
    println!("  ✅ C++ compilation works");
    println!("  ✅ Library archiving works");
    println!("  ✅ Binary linking works");
    println!("  ✅ Artifact passing between targets works");
    println!("  ✅ Generated binary executes correctly");
    println!("\n🎯 Rush is a REAL, WORKING build system!\n");

    Ok(())
}
