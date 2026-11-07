// Rush Example: Simple Action Execution
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

//! Example showing Rush components working together.

use rush_action::{Action, Artifact, Command, ExecutionInfo};
use rush_cache::ActionCache;
use rush_exec::{ExecutionStrategy, LocalStrategy};
use std::collections::HashMap;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("Rush Example: Simple Action Execution\n");

    // 1. Create an action
    println!("Creating action...");
    let inputs = vec![Artifact::Source(PathBuf::from("main.c"))];
    let command = Command {
        executable: PathBuf::from("/bin/echo"),
        args: vec!["Hello from Rush!".to_string()],
        execution_info: ExecutionInfo::default(),
    };
    let env = HashMap::new();

    let action_id = Action::compute_id(&inputs, &command, &env);
    println!("Action ID: {}", action_id);

    let action = Action {
        id: action_id.clone(),
        inputs,
        outputs: vec![],
        command,
        env,
        mnemonic: "Echo".to_string(),
    };

    // 2. Check cache
    println!("\nChecking cache...");
    let cache = ActionCache::new();
    if let Some(cached_result) = cache.get(&action_id) {
        println!("Cache hit! Exit code: {}", cached_result.exit_code);
        return Ok(());
    }
    println!("Cache miss, executing action...");

    // 3. Execute action
    println!("\nExecuting with LocalStrategy...");
    let strategy = LocalStrategy;
    let result = strategy.execute(&action).await?;

    println!("Exit code: {}", result.exit_code);
    println!("Stdout: {}", String::from_utf8_lossy(&result.stdout));

    if !result.stderr.is_empty() {
        println!("Stderr: {}", String::from_utf8_lossy(&result.stderr));
    }

    // 4. Cache the result
    println!("\nCaching result...");
    cache.put(action_id.clone(), result);
    println!("Cached! Cache size: {}", cache.size());

    // 5. Verify cache hit on second attempt
    println!("\nSecond execution (should hit cache)...");
    if let Some(cached_result) = cache.get(&action_id) {
        println!("Cache hit! Exit code: {}", cached_result.exit_code);
    }

    println!("\n✓ Example complete!");
    Ok(())
}
