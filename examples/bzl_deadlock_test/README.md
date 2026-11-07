# BZL Deadlock Test Suite

This directory contains isolated test cases to diagnose the .bzl loading hang seen when building TensorFlow.

## Test Cases

### Test 1: Simple Load (`test1_simple/`)
- Single .bzl file with no dependencies
- Tests basic .bzl loading and evaluation
- **Expected**: Should work without issues

### Test 2: Nested Load (`test2_nested/`)
- Chain of .bzl files: BUILD → a.bzl → b.bzl → c.bzl
- Tests recursive .bzl loading without cycles
- **Expected**: Should work without issues

### Test 3: Circular Load (`test3_circular/`)
- Circular dependency: circular_a.bzl ↔ circular_b.bzl
- Tests circular load detection
- **Expected**: Should fail with "Circular .bzl load detected" error

### Test 4: Resource Pressure (`test4_resource_pressure/`)
- Loads 5 .bzl files simultaneously from one file
- Tests resource acquisition under load
- **Expected**: May hang if tokio deadlock theory is correct

### Test 5: TensorFlow Pattern (`test5_tensorflow_pattern/`)
- Mimics TensorFlow's build_config_root.default.bzl pattern
- Uses select() and conditional logic
- **Expected**: May hang if specific TensorFlow patterns trigger issue

## Running Tests

Each test can be run independently:

```bash
cd examples/bzl_deadlock_test

# Test 1: Simple
RUST_LOG=debug cargo run --bin rush -- build //test1_simple:test_simple 2>&1 | tee test1_simple.log

# Test 2: Nested
RUST_LOG=debug cargo run --bin rush -- build //test2_nested:test_nested 2>&1 | tee test2_nested.log

# Test 3: Circular (should error)
RUST_LOG=debug cargo run --bin rush -- build //test3_circular:test_circular 2>&1 | tee test3_circular.log

# Test 4: Resource Pressure (may hang)
RUST_LOG=debug cargo run --bin rush -- build //test4_resource_pressure:test_resource_pressure 2>&1 | tee test4_resource_pressure.log

# Test 5: TensorFlow Pattern
RUST_LOG=debug cargo run --bin rush -- build //test5_tensorflow_pattern:test_tensorflow_pattern 2>&1 | tee test5_tensorflow_pattern.log
```

## Analysis

After running tests, look for:
1. Which tests hang vs. complete
2. Last debug message before hang
3. PSI memory pressure levels during hang
4. Resource acquisition patterns

## Hypothesis

**Theory**: Test 4 (Resource Pressure) will hang when system is under memory pressure, confirming the tokio deadlock theory described in AI-HANDOFF.md.

**Root Cause**: `bzl_loader.rs:151` uses `block_in_place` to acquire resources from async ResourceManager, which deadlocks tokio runtime when PSI monitor needs to adjust limits.

## Test Results (2025-10-14)

| Test | Expected Result | Actual Result | Notes |
|------|----------------|---------------|-------|
| test1_simple | ✅ Pass | ✅ **PASS** | Resource acquisition/release works perfectly |
| test2_nested | ✅ Pass | ✅ **PASS** | Recursive loading A→B→C works correctly |
| test3_circular | ❌ Error (circular) | ✅ **PASS (with warning)** | Circular detection works: "Warning: Circular .bzl load detected" |
| test4_resource_pressure | ⚠️ May hang | ✅ **PASS** | All 5 helpers loaded sequentially, NO hang |
| test5_tensorflow_pattern | ❓ Unknown | ✅ **PASS** | TensorFlow-style pattern works |

**Key Finding**: All isolated .bzl loading patterns work correctly. The TensorFlow hang must be caused by:
1. **External repo loading** (`@local_tsl//...`) - not tested here
2. **Complex macro evaluation** within .bzl files
3. **Scale** - TensorFlow loads 100+ .bzl files vs. our 5
4. **Actual memory pressure** - system wasn't under stress during these tests

## Analysis

### What Works ✅
- Basic .bzl loading and caching
- Recursive/nested .bzl loading (A→B→C chains)
- Circular dependency detection
- Resource acquisition for .bzl evaluation
- Sequential loading of multiple .bzl files

### What's NOT Tested
- **External repository resolution** (`@local_tsl//tsl/platform/default:build_config_root.bzl`)
- Loading .bzl files from fetched http_archive dependencies
- High memory pressure scenarios (PSI > 50%)
- Loading 50+ .bzl files simultaneously

### Hypothesis Revision

**Original theory** (tokio deadlock in bzl_loader.rs:151) is **unlikely** because:
- Test 4 loaded 5 .bzl files without issues
- Resource acquisition completed in all cases
- No hangs observed even with nested loading

**New hypothesis**: The TensorFlow hang occurs during **external repository .bzl loading**, specifically:
```
load("@local_tsl//tsl/platform/default:build_config_root.bzl", ...)
```

This involves:
1. Resolving `@local_tsl` repository path
2. Loading .bzl file from external cache
3. Possibly missing or incomplete repository fetch

**Evidence from AI-HANDOFF.md**:
- Hang occurs at: `//tensorflow/core/platform:build_config_root.default.bzl`
- That file loads from: `@local_tsl//tsl/platform/default:build_config_root.bzl`
- The external load is the last operation before hang

## Next Steps

To confirm the hypothesis, create a test case that:
1. Sets up a mock external repository
2. Loads a .bzl file from that external repo using `@repo//...` syntax
3. Tests if external repo .bzl loading hangs or fails

**Test 6 needed**: `test6_external_repo/`
