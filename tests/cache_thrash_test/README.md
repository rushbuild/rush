# Cache Thrashing Test

## Problem

The TensorFlow build crashes due to LRU cache thrashing in `bzl_loader.rs`.

### Root Cause

From `crates/rush-starlark/src/bzl_loader.rs`:
- Line 70: `program_cache: LruCache::new(NonZeroUsize::new(200).unwrap())`
- Line 71: `symbol_cache: LruCache::new(NonZeroUsize::new(100).unwrap())`

**The Issue:**
- TensorFlow has 428 .bzl files
- Symbol cache limited to 100 entries
- Program cache limited to 200 entries
- When loading deep dependency chains or parallel batches, caches evict files that are needed again
- This causes repeated parsing and evaluation → thrashing → crash/hang

### Evidence from TensorFlow Build

```bash
$ grep "Parsing .bzl file:" tf_lru_build.log | sort | uniq -c | sort -rn | head -5
      3 [DEBUG Evaluator] Parsing .bzl file: .../bazel_skylib/lib/new_sets.bzl
      3 [DEBUG Evaluator] Parsing .bzl file: .../local_config_cuda/cuda/build_defs.bzl
      3 [DEBUG Evaluator] Parsing .bzl file: .../local_config_rocm/rocm/build_defs.bzl
      3 [DEBUG Evaluator] Parsing .bzl file: .../third_party/compute_library/build_defs.bzl
      3 [DEBUG Evaluator] Parsing .bzl file: .../third_party/mkl_dnn/build_defs.bzl
```

Files are being parsed **3 times** because they're evicted from cache and needed again.

## Test Cases

### Test 1: Deep Dependency Chain (`workspace/`)

**Pattern:** Linear dependency chain longer than cache size

```
file_150.bzl → file_149.bzl → ... → file_1.bzl → base.bzl
```

**Why it thrashes:**
- 150 files in chain
- Symbol cache holds 100 entries
- When loading file_150, files 1-50 get evicted
- If any evicted file is needed again, must re-parse

**Run:**
```bash
./generate_test.sh
cd workspace
rush build //:all 2>&1 | tee ../build.log
grep 'Parsing .bzl file' ../build.log | sort | uniq -c | sort -rn
```

### Test 2: Parallel Loading with Shared Dependencies (`workspace_parallel/`)

**Pattern:** Many files loading shared dependencies (mimics TensorFlow)

```
user_1.bzl ─┐
user_2.bzl ─┤
user_3.bzl ─┼─→ shared_1.bzl, shared_2.bzl, ..., shared_10.bzl
...         │
user_120.bzl┘
```

**Why it thrashes:**
- 120 user files + 10 shared files = 130 total
- Symbol cache holds 100 entries
- Parallel batch loading triggers for all 120 user files
- Shared dependencies get evicted as user files are cached
- Shared files must be re-parsed multiple times

**Run:**
```bash
./generate_parallel_test.sh
cd workspace_parallel
rush build //:all 2>&1 | tee ../parallel_build.log
grep 'Parsing .bzl file.*shared_' ../parallel_build.log | sort | uniq -c | sort -rn
```

## Expected Results

**Without fix:** Files parsed multiple times (>1 in uniq -c output)
**With fix:** Each file parsed exactly once

## Solution

Increase cache sizes in `bzl_loader.rs`:

```rust
// Old (causes thrashing):
program_cache: LruCache::new(NonZeroUsize::new(200).unwrap()),
symbol_cache: LruCache::new(NonZeroUsize::new(100).unwrap()),

// New (handles large projects):
program_cache: LruCache::new(NonZeroUsize::new(2000).unwrap()),  // 10x increase
symbol_cache: LruCache::new(NonZeroUsize::new(1000).unwrap()),   // 10x increase
```

Or make it configurable based on available memory.

## Files

- `generate_test.sh` - Generates deep dependency chain test
- `generate_parallel_test.sh` - Generates parallel loading test
- `workspace/` - Deep chain test (150 files)
- `workspace_parallel/` - Parallel test (130 files)
