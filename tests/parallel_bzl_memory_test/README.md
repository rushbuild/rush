# Parallel .bzl Loading Memory Test

## Purpose

This test reproduces and validates fixes for excessive memory consumption during parallel .bzl file loading.

## Background

### Original Issue

During TensorFlow build, Rush consumed **16.78 GB** in 1 second while loading 51 .bzl files, triggering memory protection:

```
Memory guard starting: effective_abort=10.50 GB (70% of 15.00 GB max)
Memory usage: 16.78 GB / 10.50 GB (effective abort threshold)
ABORTING IMMEDIATELY TO PROTECT SYSTEM
```

### Root Causes

1. **Unbounded Parallel Loading**: When evaluating 51 consecutive load() statements, Rush spawned 51 concurrent tokio tasks simultaneously
2. **Dependency Graph Building**: Before parallel loading, Rush parsed ALL .bzl files recursively to build a dependency graph, accumulating ASTs in memory

## Test Setup

This test simulates the issue with 50 simple .bzl files (lib1.bzl through lib50.bzl), each defining:
- A rule function
- A macro function
- Constants and data structures

The BUILD file loads all 50 in parallel:
```python
load(":lib1.bzl", "lib1_rule")
load(":lib2.bzl", "lib2_rule")
...
load(":lib50.bzl", "lib50_rule")

filegroup(name = "test", srcs = [])
```

## Results

### Before Fix
**Peak memory: 6656 MB (6.5 GB)** for 50 simple .bzl files

With TensorFlow's 51 complex .bzl files: **16.78 GB**

### After Fix #1: Semaphore-Limited Concurrency
```rust
let max_concurrent = 12;
let semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrent));
```

**Peak memory: 4096 MB (4.0 GB)** - **38% reduction (2.6 GB saved)**

### After Fix #2: Skip Dependency Graph Building
```rust
let skip_dep_graph = std::env::var("RUSH_SKIP_BZL_DEP_GRAPH")
    .unwrap_or("true".to_string())
    .parse()
    .unwrap_or(true);
```

**Peak memory: 4096 MB (4.0 GB)** - maintained improvement

With TensorFlow: Still hits PSI critical pressure threshold (6.15% > 5%) before completing

## Fixes Implemented

### 1. Limit Concurrent Tasks (evaluator.rs:506)

```rust
// OLD: Spawn all tasks at once
for (file, symbols, _priority) in level {
    let task = tokio::task::spawn(async move {
        load_bzl_from_evaluator(...)
    });
    tasks.push(task);
}

// NEW: Use semaphore to limit concurrency
let max_concurrent = 12;
let semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrent));

for (file, symbols, _priority) in level {
    let task = tokio::task::spawn(async move {
        let _permit = semaphore.acquire().await.unwrap();
        load_bzl_from_evaluator(...)
        // Permit dropped, next task can proceed
    });
    tasks.push(task);
}
```

**Impact**: Limits memory by ensuring only 12 .bzl files are being evaluated simultaneously

### 2. Skip Dependency Graph Building (evaluator.rs:445)

```rust
// OLD: Always build dependency graph (parses all files recursively)
let dep_graph = self.build_load_dependency_graph(loads, &loader)?;

// NEW: Skip dependency graph by default to save memory
let skip_dep_graph = std::env::var("RUSH_SKIP_BZL_DEP_GRAPH")
    .unwrap_or("true".to_string())
    .parse()
    .unwrap_or(true);

let dep_graph = if skip_dep_graph {
    HashMap::new() // Empty graph, all files loaded in parallel with semaphore
} else {
    self.build_load_dependency_graph(loads, &loader)?
};
```

**Impact**: Avoids parsing all .bzl files + dependencies upfront. Trade-off: Can't detect circular dependencies, but they'll just fail during loading.

## Running the Test

```bash
cd tests/parallel_bzl_memory_test
bash run_test.sh
```

Expected output:
```
Peak memory: 4096 MB (8% of 48498 MB available)
Memory guard peak: 0.00 GB
```

## Configuration

Set memory thresholds via environment variables:

```bash
export RUSH_MEMORY_THRESHOLD_GB=15          # Max threshold (default: 60% of total RAM)
export RUSH_MEMORY_EARLY_ABORT_PCT=70       # Early abort at 70% of threshold
export RUSH_SKIP_BZL_DEP_GRAPH=true         # Skip dependency graph (default: true)
```

## Remaining Issues

### TensorFlow Still Fails

With both fixes, TensorFlow build still triggers memory protection, but via different mechanism:

**PSI Critical Pressure** (not memory threshold):
```
PSI CRITICAL MEMORY PRESSURE - ABORTING BUILD
PSI Metrics:
  some10=6.15%  some60=1.11%  some300=0.23%
  full10=6.15%  full60=1.11%  full300=0.23%
```

Current threshold: `full_avg10 > 0.05` (5%)

**Possible Solutions**:
1. Increase PSI threshold to 10-15% for build workloads
2. Further reduce memory consumption in .bzl evaluation
3. Stream/discard ASTs more aggressively after extracting symbols
4. Add memory pooling/arena allocation for temporary AST storage

## Files

- `BUILD` - Loads 50 .bzl files in parallel
- `lib{1..50}.bzl` - 50 generated library files
- `run_test.sh` - Test script with memory monitoring
- `WORKSPACE` - Minimal workspace
- `test_output.log` - Captured test output

## Related

- Memory guard implementation: `crates/rush-build/src/memory_guard.rs`
- PSI detection: `crates/rush-resource/src/lib.rs`
- Parallel loading: `crates/rush-starlark/src/evaluator.rs`
- Original TensorFlow test logs: `tests/memory_test_tsl_bzl/`
