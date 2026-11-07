# Memory Test: TSL tsl.bzl Loading

## Purpose

Test Rush's memory handling when loading TensorFlow's `tsl.bzl` file, which triggers
parallel loading of 8+ dependencies and can cause memory spikes.

## Background

During TensorFlow build:
- Loading `tsl.bzl` triggered 8 parallel dependency loads
- Memory spiked from 26 GB → 40 GB in ~8 seconds
- System thrashed, requiring manual intervention
- Memory guard now protects against this

## Test Approach

Due to Rush's current limitation with nested `local_repository()` paths, this test
runs directly from the TSL workspace (TensorFlow's vendored TSL directory) rather
than using a separate test workspace.

The test builds a simple TSL target that forces `tsl.bzl` to load, allowing us to
measure memory behavior under controlled thresholds.

## Running the Test

```bash
./run_test.sh
```

Or with custom TensorFlow path:
```bash
TF_PATH=/path/to/tensorflow ./run_test.sh
```

With conservative thresholds (15GB max, 10.5GB effective abort), the build should:
1. Abort if Rush memory consumption exceeds 10.5GB
2. Abort if available system memory drops below 4GB
3. Abort immediately on PSI critical memory pressure
4. Exit with code 137 when memory protection triggers

## Memory Protection Implementation

The following protections are now active:

1. **Percentage-based thresholds**: Default 60% of total RAM (auto-calculated)
2. **Early abort**: 70% of threshold (default), so effective abort at 42% of total RAM
3. **Available memory check**: < 2GB available triggers abort
4. **PSI critical pressure**: Immediate abort on memory thrashing
5. **1-second polling**: Aggressive monitoring interval

## Files

- `WORKSPACE` - Points to TensorFlow TSL directory (requires local_repository support)
- `BUILD` - Single target that would load tsl.bzl (if external repos worked)
- `run_test.sh` - Test script with memory monitoring
- `test_output.log` - Shows fallback behavior due to missing external repo support
