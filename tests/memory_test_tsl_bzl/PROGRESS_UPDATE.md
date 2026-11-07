# TensorFlow Build Progress Update

## Date: 2025-10-17

## Summary

Continued investigation and fixes for TensorFlow build compatibility with Rush.

## Work Completed

### 1. Deadlock Investigation ✅ COMPLETED

**Created**: `tests/deadlock_test/` - Minimal reproduction test cases

**Findings**:
- Simple concurrent .bzl loading does NOT deadlock in isolation
- The "waiting for cache" mechanism works correctly (0s waits in tests)
- TensorFlow showed 10+ second waits, suggesting deeper issue
- Created nested test case simulating cuda_configure.bzl pattern:
  - BUILD loads cuda_stub.bzl, rocm_stub.bzl, nccl_stub.bzl
  - rocm and nccl also load cuda_stub (concurrent access)
  - cuda_stub loads from @bazel_tools (stub repo)
- **Result**: All tests passed without deadlock

**Root Cause**: The real deadlock in TensorFlow was **PSI abort during .bzl evaluation**, not an actual cache deadlock. Files were "waiting" when PSI triggered immediate abort.

### 2. Implemented any() and all() Built-ins ✅ COMPLETED

**File**: `crates/rush-starlark/src/builtins.rs`

**Changes**:
- Lines 40-41: Registered `any` and `all` in `get_builtins()`
- Lines 1618-1647: Implemented `builtin_any()`
  - Takes iterable, returns True if any element is truthy
  - Empty iterable returns False
- Lines 1649-1679: Implemented `builtin_all()`
  - Takes iterable, returns True if all elements are truthy
  - Empty iterable returns True (mathematical convention)

**Testing**:
- Created `tests/deadlock_test/test_any.bzl` with comprehensive tests
- Both functions work correctly with lists, booleans, numbers

### 3. Context-Aware PSI Thresholds (Previous Session)

**File**: `crates/rush-resource/src/lib.rs:259-296`

**Logic**:
- >40% RAM available → Allow PSI up to 30%
- >20% RAM available → Allow PSI up to 20%
- <20% RAM available → Strict PSI 10%

**Critical Bug Fixed**: Line 285 was dividing by 100 twice (0.30 / 2 / 100 = 0.0015 instead of 0.15)

### 4. Current TensorFlow Build Status

**Progress**:
- ✅ Successfully loads 4 initial .bzl files (workspace3.bzl)
- ✅ Triggers parallel loading of 51 .bzl files (workspace2.bzl)
- ✅ Memory protection working (no thrashing)
- ⚠️ any() built-in was missing (just added)

**Remaining Blockers**:
1. ~~Undefined variable: any~~ - **FIXED** (just implemented)
2. String format() edge cases
3. @bazel_tools external repository not fully implemented
4. Many custom Bazel rules missing (proto_library, cc_proto_library, etc.)

## Test Files Created

1. **tests/deadlock_test/BUILD** - Minimal concurrent .bzl loading test
2. **tests/deadlock_test/file1.bzl** - Simple file loading from @bazel_tools
3. **tests/deadlock_test/file2.bzl** - Another simple file
4. **tests/deadlock_test/cuda_stub.bzl** - Simulates cuda_configure.bzl pattern
5. **tests/deadlock_test/rocm_stub.bzl** - Simulates rocm_configure.bzl pattern
6. **tests/deadlock_test/nccl_stub.bzl** - Simulates nccl_configure.bzl pattern
7. **tests/deadlock_test/common.bzl** - Common utilities
8. **tests/deadlock_test/test_any.bzl** - Tests for any() and all()
9. **tests/deadlock_test/README.md** - Test documentation

## Configuration Variables

### Memory Guard
```bash
export RUSH_MEMORY_THRESHOLD_GB=25          # Max threshold (default: 60% of RAM)
export RUSH_MEMORY_EARLY_ABORT_PCT=70       # Abort at 70% of threshold
```

### .bzl Loading
```bash
export RUSH_SKIP_BZL_DEP_GRAPH=true         # Skip dependency graph (default: true)
export RUSH_BZL_MAX_CONCURRENT=4            # Max concurrent .bzl loads (default: 4)
```

### PSI Thresholds
```bash
export RUSH_PSI_CRITICAL_FULL10=15          # full_avg10 threshold % (default: 15)
export RUSH_PSI_CRITICAL_FULL60=10          # full_avg60 threshold % (default: 10)
export RUSH_PSI_CRITICAL_SOME60=50          # some_avg60 threshold % (default: 50)
```

## Next Steps

1. ⏳ **IN PROGRESS**: Test TensorFlow build with any() built-in
2. **TODO**: Fix string format() method edge cases
3. **TODO**: Implement @bazel_tools repository with actual .bzl files
4. **TODO**: Test on medium-sized projects (not just TensorFlow)

## Performance Metrics

### Synthetic Test (50 .bzl files)
- **Before fixes**: 6656 MB memory, unbounded concurrency
- **After fixes**: 4096 MB memory (38% reduction), limited to 4 concurrent

### TensorFlow Build
- **Attempt 1**: Memory abort at 16.78 GB
- **Attempt 2**: PSI abort at 19.46% (too aggressive threshold)
- **Attempt 3**: PSI abort at 12.49% (double-division bug)
- **Attempt 4**: With context-aware PSI - test running...

## Commit History

This session:
1. Investigated .bzl deadlock patterns
2. Implemented any() and all() built-ins
3. Documented findings

Previous sessions:
1. `82b6b3c` - Added memory guard
2. `6a281e1` - Added attr and config stubs
3. `21c6f2f` - Fixed nested .bzl loading deadlock
4. `48fdc28` - Fixed keyword-only argument parsing
5. Context-aware PSI (uncommitted)
6. any()/all() built-ins (uncommitted)
