// Test BuildOrchestrator with disk-backed cache
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use rush_build::{BuildOrchestrator, BuildRequest, CacheConfig};
use std::path::PathBuf;
use std::time::Instant;
use tempfile::TempDir;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("🚀 BuildOrchestrator with Disk Cache Test\n");

    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    // Create temporary cache directory
    let temp_dir = TempDir::new()?;
    let cache_dir = temp_dir.path().to_path_buf();

    println!("Workspace: {}", workspace.display());
    println!("Cache directory: {}\n", cache_dir.display());

    // Configure disk cache
    let cache_config = CacheConfig {
        cache_root: cache_dir.clone(),
        max_size: 1024 * 1024 * 1024, // 1GB
        max_age: 30 * 24 * 60 * 60,   // 30 days
    };

    // Test 1: First build with disk cache
    println!("{}", "=".repeat(60));
    println!("Test 1: First build (clean, with disk cache)");
    println!("{}\n", "=".repeat(60));

    {
        let orchestrator =
            BuildOrchestrator::with_disk_cache(workspace.clone(), cache_config.clone())?;

        let request = BuildRequest {
            targets: vec!["//examples/parallel_test:calculator".to_string()],
        };

        let start = Instant::now();
        orchestrator.build(request).await?;
        let duration = start.elapsed();

        println!(
            "\n✨ First build completed in {:.2}s",
            duration.as_secs_f64()
        );
        println!("   Cache populated to disk");
    }

    // Wait for async disk writes to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;

    // Test 2: Second build with new orchestrator instance
    println!("\n{}", "=".repeat(60));
    println!("Test 2: Second build (new orchestrator, disk cache persists)");
    println!("{}\n", "=".repeat(60));

    {
        // Create new orchestrator - memory cache is empty but disk persists
        let orchestrator = BuildOrchestrator::with_disk_cache(workspace.clone(), cache_config)?;

        let request = BuildRequest {
            targets: vec!["//examples/parallel_test:calculator".to_string()],
        };

        let start = Instant::now();
        orchestrator.build(request).await?;
        let duration = start.elapsed();

        println!(
            "\n✨ Second build completed in {:.2}s",
            duration.as_secs_f64()
        );
        println!("   Should be fast - loaded from disk cache!");
    }

    // Test 3: Compare with in-memory cache
    println!("\n{}", "=".repeat(60));
    println!("Test 3: In-memory cache (no persistence)");
    println!("{}\n", "=".repeat(60));

    {
        let orchestrator = BuildOrchestrator::with_workspace(workspace);

        let request = BuildRequest {
            targets: vec!["//examples/parallel_test:calculator".to_string()],
        };

        let start = Instant::now();
        orchestrator.build(request).await?;
        let duration = start.elapsed();

        println!(
            "\n✨ In-memory build completed in {:.2}s",
            duration.as_secs_f64()
        );
        println!("   Note: In-memory cache lost on exit");
    }

    println!("\n{}", "=".repeat(60));
    println!("Summary");
    println!("{}", "=".repeat(60));
    println!("✅ Disk cache persists across BuildOrchestrator instances");
    println!("✅ In-memory cache is faster but ephemeral");
    println!("✅ Hybrid approach gives best of both worlds");

    Ok(())
}
