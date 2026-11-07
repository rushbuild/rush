// Performance test for Modular BUILD parsing
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

use rush_parse::parse_build_file;
use std::time::Instant;

#[test]
fn perf_test_modular_parsing() -> anyhow::Result<()> {
    let modular_root = std::env::var_os("RUSH_MODULAR_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("../modular"));
    let modular_build_path = modular_root.join("BUILD.bazel");
    let modular_build = modular_build_path.as_path();

    if !modular_build.exists() {
        eprintln!("Skipping: Modular BUILD.bazel not found");
        return Ok(());
    }

    println!("\n🚀 Performance Test: Modular BUILD.bazel Parsing\n");

    // Warm-up
    for _ in 0..3 {
        let _ = parse_build_file(modular_build)?;
    }

    // Actual timing
    let iterations = 100;
    let start = Instant::now();

    for _ in 0..iterations {
        let _ = parse_build_file(modular_build)?;
    }

    let elapsed = start.elapsed();
    let avg_us = elapsed.as_micros() / iterations;
    let avg_ms = elapsed.as_millis() as f64 / iterations as f64;

    println!("Iterations: {}", iterations);
    println!("Total time: {:?}", elapsed);
    println!("Average:    {:.2} ms ({} µs)", avg_ms, avg_us);
    println!("Throughput: {:.1} parses/sec", 1000.0 / avg_ms);

    // Parse once to get stats
    let build_file = parse_build_file(modular_build)?;
    println!("\nFile stats:");
    println!("  Rules parsed: {}", build_file.rules.len());
    println!(
        "  Time per rule: {:.0} µs",
        avg_us / build_file.rules.len() as u128
    );

    // Performance targets (based on Bazel benchmarks)
    // Bazel: ~5-10ms for similar files with JVM warmup
    // Rush target: <2ms (2-5x faster)
    println!("\n⚡ Performance Analysis:");
    println!("  Bazel (typical): ~5-10 ms");
    println!("  Rush (current):  {:.2} ms", avg_ms);

    if avg_ms < 2.0 {
        println!("  Status: ✅ EXCELLENT (>2.5x faster than Bazel)");
    } else if avg_ms < 5.0 {
        println!("  Status: ✅ GOOD (meeting target)");
    } else if avg_ms < 10.0 {
        println!("  Status: ⚠️  ACCEPTABLE (on par with Bazel)");
    } else {
        println!("  Status: ❌ NEEDS OPTIMIZATION (slower than Bazel)");
    }

    Ok(())
}
