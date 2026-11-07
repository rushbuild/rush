// Test Rush parallel compilation
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use rush_build::{BuildOrchestrator, BuildRequest};
use std::path::PathBuf;
use std::time::Instant;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("🚀 Rush Parallel Compilation Test\n");

    // Create build orchestrator
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let orchestrator = BuildOrchestrator::with_workspace(workspace);

    println!("Workspace: {}\n", orchestrator.workspace_root().display());

    // Create build request for parallel_test project
    let request = BuildRequest {
        targets: vec!["//examples/parallel_test:calculator".to_string()],
    };

    println!("{}", "=".repeat(60));
    println!("Clean build (should compile 5 files in parallel)");
    println!("{}\n", "=".repeat(60));

    let start = Instant::now();
    orchestrator.build(request.clone()).await?;
    let duration = start.elapsed();

    println!("\n✨ Build completed in {:.2}s", duration.as_secs_f64());

    println!("\n{}", "=".repeat(60));
    println!("Incremental build (all cached)");
    println!("{}\n", "=".repeat(60));

    let start = Instant::now();
    orchestrator.build(request).await?;
    let duration = start.elapsed();

    println!(
        "\n✨ Incremental build completed in {:.2}s",
        duration.as_secs_f64()
    );

    Ok(())
}
