// Test cross-package dependency resolution
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use rush_build::{BuildOrchestrator, BuildRequest};
use std::path::PathBuf;
use std::time::Instant;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("🔗 Rush Cross-Package Dependency Test\n");

    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let orchestrator = BuildOrchestrator::with_workspace(workspace);

    println!("Workspace: {}\n", orchestrator.workspace_root().display());

    // Build app that depends on library from different package
    let request = BuildRequest {
        targets: vec!["//examples/app_package:app".to_string()],
    };

    println!("{}", "=".repeat(60));
    println!("Building cross-package dependency tree:");
    println!("  //examples/app_package:app");
    println!("    └── //examples/lib_package:math_utils");
    println!("{}\n", "=".repeat(60));

    let start = Instant::now();
    orchestrator.build(request).await?;
    let duration = start.elapsed();

    println!("\n✨ Build completed in {:.2}s", duration.as_secs_f64());

    // Test the binary
    println!("\n{}", "=".repeat(60));
    println!("Running the built application:");
    println!("{}\n", "=".repeat(60));

    let output = std::process::Command::new("./app").output()?;

    println!("{}", String::from_utf8_lossy(&output.stdout));

    if !output.status.success() {
        eprintln!("stderr: {}", String::from_utf8_lossy(&output.stderr));
        anyhow::bail!(
            "Application failed with exit code {:?}",
            output.status.code()
        );
    }

    println!("✅ Cross-package dependencies working!");

    Ok(())
}
