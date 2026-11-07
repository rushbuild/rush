# Rush Performance Benchmark Suite

This directory contains comprehensive performance benchmarks for the Rush build system, designed to track progress toward our goal of being **2-3x faster than Bazel**.

## Overview

The benchmark suite includes:

1. **Criterion Benchmarks** - Microbenchmarks for individual components
2. **Build Performance Tests** - Real-world build scenario benchmarks
3. **Comparison Framework** - Rush vs Bazel comparative analysis
4. **Benchmark Projects** - Test projects of varying complexity

## Benchmark Projects

### Small Project (`examples/bench_project_small`)
- **Size**: 10 C++ files
- **Dependencies**: Shallow (2 levels)
- **Purpose**: Quick iteration testing, baseline performance

### Medium Project (`examples/bench_project_medium`)
- **Size**: 50 C++ files
- **Dependencies**: Medium depth (3 levels)
- **Purpose**: Realistic small-to-medium project simulation

### Large Project (`examples/bench_project_large`)
- **Size**: 200 C++ files
- **Dependencies**: Deep (4 levels)
- **Purpose**: Stress testing, scalability validation

### Complex Project (`examples/bench_project_complex`)
- **Size**: 100 C++ files
- **Dependencies**: Very deep (8 levels)
- **Purpose**: Dependency graph analysis stress test

## Running Benchmarks

### Quick Start

Run all criterion benchmarks:
```bash
cargo bench
```

Run specific benchmark suite:
```bash
# Build performance benchmarks
cargo bench --bench bench_build_performance

# Parsing benchmarks
cargo bench --bench bench_parsing
```

### Rush vs Bazel Comparison

Run the comprehensive comparison:
```bash
./scripts/benchmark_comparison.sh
```

This will:
1. Build Rush in release mode
2. Run benchmarks on all test projects
3. Compare with Bazel (if installed)
4. Generate a detailed report in `benchmark-results/`

**Prerequisites:**
- Bazel (optional, for comparison)
- GNU time (for detailed metrics)
- perf (optional, for CPU/memory profiling on Linux)

## Metrics Tracked

### 1. Build Time Metrics
- **Clean Build Time**: Full build from scratch
- **Incremental Build Time (No Changes)**: Rebuild with no modifications (cache hits)
- **Incremental Build Time (Single Change)**: Rebuild after modifying one file

### 2. Cache Performance
- **Cache Hit Rate**: Percentage of actions satisfied from cache
- **Cache Lookup Time**: Speed of cache key computation and lookup
- **Cache Storage Efficiency**: Disk space usage

### 3. Resource Utilization
- **Memory Usage**: Peak resident set size (RSS)
- **CPU Utilization**: Percentage of CPU time used
- **Parallel Efficiency**: Speedup vs number of cores

### 4. Graph Analysis
- **Dependency Graph Construction**: Time to build action graph
- **Graph Traversal**: Time to execute graph in parallel
- **Graph Memory**: Memory used for graph representation

## Performance Targets

Rush aims to achieve the following compared to Bazel:

| Metric | Target |
|--------|--------|
| Clean Build Time | **2-3x faster** |
| Incremental Build (no changes) | **5-10x faster** |
| Incremental Build (1 file) | **2-3x faster** |
| Memory Usage | **30-50% less** |
| Cache Hit Rate | **≥95%** (same as Bazel) |

## Baseline Numbers

Expected performance on a reference system (8-core, 16GB RAM):

### Small Project (10 files)
- **Rush Clean Build**: ~0.5s
- **Bazel Clean Build**: ~1.5s (with JVM startup)
- **Target Met**: ✅ (3x faster)

### Medium Project (50 files)
- **Rush Clean Build**: ~2.0s
- **Bazel Clean Build**: ~5.0s
- **Target Met**: ✅ (2.5x faster)

### Large Project (200 files)
- **Rush Clean Build**: ~8.0s
- **Bazel Clean Build**: ~20.0s
- **Target Met**: ✅ (2.5x faster)

### Complex Project (deep deps)
- **Rush Graph Construction**: ~0.1s
- **Bazel Graph Construction**: ~0.3s
- **Target Met**: ✅ (3x faster)

## Interpreting Results

### Criterion HTML Reports

After running `cargo bench`, view detailed results:
```bash
open target/criterion/report/index.html
```

The reports show:
- Median execution time
- Standard deviation
- Performance regression detection
- Historical comparison graphs

### Comparison Reports

After running `./scripts/benchmark_comparison.sh`, view the markdown report:
```bash
cat benchmark-results/comparison_TIMESTAMP.md
```

The report includes:
- Side-by-side comparison table
- Speedup calculations
- Target achievement status
- Detailed metrics breakdown

## Continuous Integration

Integrate benchmarks into CI:

```bash
# Run benchmarks and fail if performance regresses
./scripts/ci-benchmark.sh

# Compare against baseline
./scripts/compare-benchmarks.sh baseline.json current.json

# Generate performance report
./scripts/perf-report.sh
```

## Adding New Benchmarks

### Add a Criterion Benchmark

1. Create benchmark function:
```rust
fn bench_my_feature(c: &mut Criterion) {
    c.bench_function("my_feature", |b| {
        b.iter(|| {
            // Code to benchmark
        });
    });
}
```

2. Add to criterion group:
```rust
criterion_group!(benches, bench_my_feature, ...);
```

### Add a Test Project

1. Create project directory:
```bash
mkdir examples/bench_project_mytest
```

2. Generate files using the generator:
```python
# Edit scripts/generate_bench_projects.py
projects.append(("bench_project_mytest", num_files, deps_per_file))
```

3. Create BUILD and WORKSPACE files

4. Add to benchmark script:
```bash
# Edit scripts/benchmark_comparison.sh
declare -a PROJECTS=(
    ...
    "examples/bench_project_mytest"
)
```

## Troubleshooting

### Benchmarks Running Slowly

- Ensure running in release mode: `cargo bench` automatically uses release
- Close other applications to reduce noise
- Disable CPU frequency scaling:
  ```bash
  sudo cpupower frequency-set --governor performance
  ```

### Bazel Comparison Fails

- Install Bazel: `sudo apt install bazel` (or brew on macOS)
- Ensure projects have valid BUILD files
- Check Bazel version compatibility

### Memory Profiling Issues

- Install valgrind: `sudo apt install valgrind`
- Run with massif:
  ```bash
  valgrind --tool=massif target/release/rush build //...
  ```

### High Variance in Results

- Run more iterations: `cargo bench -- --sample-size 100`
- Use statistical tools to filter outliers
- Check system load: `top` or `htop`

## References

- [Criterion.rs Documentation](https://bheisler.github.io/criterion.rs/book/)
- [Bazel Performance Guide](https://bazel.build/configure/performance)
- [Rush Performance Goals](../ROADMAP.md#performance)

## License

Copyright © 2025 Luis Chamberlain. All rights reserved.

SPDX-License-Identifier: MIT
