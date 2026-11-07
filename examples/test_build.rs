// Test Rush build execution
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use rush_build::{BuildOrchestrator, BuildRequest};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("🚀 Rush Build Execution Test\n");

    // Create build orchestrator
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let orchestrator = BuildOrchestrator::with_workspace(workspace);

    println!("Workspace: {}\n", orchestrator.workspace_root().display());

    // Create build request for hello_cpp binary (which depends on hello_lib)
    let request = BuildRequest {
        targets: vec!["//examples/hello_cpp:hello".to_string()],
    };

    // Execute build
    orchestrator.build(request.clone()).await?;

    println!("\n{}", "=".repeat(60));
    println!("🔄 Running second build to test caching...");
    println!("{}\n", "=".repeat(60));

    // Run again to test caching
    orchestrator.build(request).await?;

    Ok(())
}
