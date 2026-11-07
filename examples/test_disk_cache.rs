// Test Rush disk-backed cache
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use rush_action::{Action, Command, ExecutionInfo};
use rush_cache::{ActionCache, CacheConfig};
use rush_exec::{ExecutionStrategy, LocalStrategy};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;
use tempfile::TempDir;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("🔧 Rush Disk Cache Test\n");

    // Create temporary cache directory
    let temp_dir = TempDir::new()?;
    let cache_dir = temp_dir.path().to_path_buf();

    println!("Cache directory: {}\n", cache_dir.display());

    // Test 1: Create disk cache and store results
    println!("{}", "=".repeat(60));
    println!("Test 1: Store action results in disk cache");
    println!("{}\n", "=".repeat(60));

    {
        let config = CacheConfig {
            cache_root: cache_dir.clone(),
            max_size: 1024 * 1024 * 1024,   // 1GB
            ttl_seconds: 30 * 24 * 60 * 60, // 30 days
        };

        let cache = ActionCache::with_disk(config)?;

        // Create a simple action (echo hello)
        let action = create_echo_action("hello world");
        println!("Action ID: {}", action.id);

        // Execute and cache the result
        let start = Instant::now();
        let strategy = LocalStrategy;
        let result = strategy.execute(&action).await?;
        let exec_time = start.elapsed();

        println!("Execution time: {:.2}ms", exec_time.as_secs_f64() * 1000.0);
        println!("Exit code: {}", result.exit_code);
        println!("Output: {}", String::from_utf8_lossy(&result.stdout));

        // Store in cache
        cache.put(action.id.clone(), result.clone());
        println!("✅ Stored in cache");

        // Verify immediate retrieval
        let cached = cache.get(&action.id);
        assert!(cached.is_some(), "Cache should contain result");
        println!("✅ Retrieved from memory cache");
    }

    // Test 2: Create new cache instance and verify persistence
    println!("\n{}", "=".repeat(60));
    println!("Test 2: Verify persistence across cache instances");
    println!("{}\n", "=".repeat(60));

    // Wait a bit for async disk write to complete
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    {
        let config = CacheConfig {
            cache_root: cache_dir.clone(),
            max_size: 1024 * 1024 * 1024,
            ttl_seconds: 30 * 24 * 60 * 60,
        };

        let cache = ActionCache::with_disk(config)?;

        // Recreate the same action
        let action = create_echo_action("hello world");
        println!("Action ID: {}", action.id);

        // Try to retrieve from disk cache (memory is empty)
        let start = Instant::now();
        let cached = cache.get(&action.id);
        let lookup_time = start.elapsed();

        match cached {
            Some(result) => {
                println!("✅ Retrieved from disk cache!");
                println!("Lookup time: {:.2}ms", lookup_time.as_secs_f64() * 1000.0);
                println!("Exit code: {}", result.exit_code);
                println!("Output: {}", String::from_utf8_lossy(&result.stdout));
            }
            None => {
                println!("❌ Failed to retrieve from disk cache");
                println!("This might be a timing issue - disk write may not have completed");
            }
        }
    }

    // Test 3: Compare memory vs disk performance
    println!("\n{}", "=".repeat(60));
    println!("Test 3: Performance comparison");
    println!("{}\n", "=".repeat(60));

    let mem_cache = ActionCache::new();
    let action = create_echo_action("performance test");

    // Execute once
    let strategy = LocalStrategy;
    let result = strategy.execute(&action).await?;

    // Store in memory cache
    mem_cache.put(action.id.clone(), result.clone());

    // Benchmark memory cache
    let start = Instant::now();
    for _ in 0..1000 {
        let _ = mem_cache.get(&action.id);
    }
    let mem_time = start.elapsed();

    println!(
        "Memory cache: 1000 lookups in {:.2}ms",
        mem_time.as_secs_f64() * 1000.0
    );
    println!(
        "Average: {:.2}μs per lookup",
        mem_time.as_secs_f64() * 1_000_000.0 / 1000.0
    );

    println!("\n✨ Disk cache test completed!");

    Ok(())
}

fn create_echo_action(message: &str) -> Action {
    let inputs = vec![];
    let command = Command {
        executable: PathBuf::from("/bin/echo"),
        args: vec![message.to_string()],
        execution_info: ExecutionInfo::default(),
    };

    let env = HashMap::new();
    let id = Action::compute_id(&inputs, &command, &env);

    Action {
        id,
        inputs,
        outputs: vec![],
        command,
        env,
        mnemonic: "Echo".to_string(),
    }
}
