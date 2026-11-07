# TensorFlow BUILD File .bzl Loading Test Plan

**Purpose**: Isolate which .bzl loading pattern causes Rush to hang when parsing TensorFlow BUILD files

**Status**: Plan ready for implementation

---

## Patterns Identified from TensorFlow BUILD

From `tensorflow/core/BUILD`, lines 65-114:

```python
# Pattern 1: Load from missing external repo
load("@local_config_cuda//cuda:build_defs.bzl", "if_cuda")

# Pattern 2: Load many symbols from local .bzl
load(
    "//tensorflow:tensorflow.bzl",
    "if_android", "if_cuda_or_rocm", "if_google", "if_ios", "if_libtpu",
    "if_mobile", "if_nccl", "tf_cc_test", "tf_cc_test_mkl", "tf_cc_tests",
    "tf_copts", "tf_cuda_library", ...  # 14+ symbols
)

# Pattern 3: Chain of loads from same directory
load("//tensorflow/core/platform:build_config.bzl", ...)
load("//tensorflow/core/platform:build_config_root.bzl", ...)
load("//tensorflow/core/platform:rules_cc.bzl", ...)

# Pattern 4: Nested .bzl loads
# tensorflow.bzl itself loads more .bzl files
# build_config.bzl loads build_config_root.bzl
# Which loads @local_tsl//... files

# Pattern 5: Multiple missing external repos
load("@local_config_cuda//cuda:build_defs.bzl", "if_cuda")
load("@local_config_tensorrt//:build_defs.bzl", "if_tensorrt")
```

---

## Test Suite: test5-test9

### test5: Simple .bzl with no nested loads

**What it tests**: Basic BUILD → .bzl loading

```
BUILD
  load("//:simple.bzl", "my_func")
  my_func()

simple.bzl
  def my_func():
      native.genrule(...)
```

**Expected**: Should work (already tested in earlier suite)

---

### test6: .bzl file that loads another .bzl

**What it tests**: .bzl → .bzl nested loading

```
BUILD
  load("//:level1.bzl", "func1")
  func1()

level1.bzl
  load("//:level2.bzl", "func2")
  def func1():
      func2()

level2.bzl
  def func2():
      native.genrule(...)
```

**Expected**: Should work OR reveal if .bzl-from-.bzl causes hang

---

### test7: Chain of .bzl loads (A→B→C→D)

**What it tests**: Deep .bzl dependency chain

```
BUILD
  load("//:a.bzl", "a")

a.bzl
  load("//:b.bzl", "b")

b.bzl
  load("//:c.bzl", "c")

c.bzl
  load("//:d.bzl", "d")

d.bzl
  def d():
      return "done"
```

**Expected**: Should work OR reveal if depth causes hang

---

### test8: Multiple .bzl loads from single BUILD

**What it tests**: Many parallel .bzl loads (TensorFlow has 8+ loads)

```
BUILD
  load("//:bzl1.bzl", "func1")
  load("//:bzl2.bzl", "func2")
  load("//:bzl3.bzl", "func3")
  load("//:bzl4.bzl", "func4")
  load("//:bzl5.bzl", "func5")
  load("//:bzl6.bzl", "func6")
  load("//:bzl7.bzl", "func7")
  load("//:bzl8.bzl", "func8")

  genrule(...)
```

**Expected**: Should work OR reveal if many parallel loads cause hang

---

### test9: Missing external repo .bzl load

**What it tests**: TensorFlow's exact failure pattern

```
BUILD
  load("@missing_repo//path:file.bzl", "func")
  load("//:local.bzl", "local_func")

  genrule(...)
```

**Expected**: Warnings for missing repo, but should NOT hang

This is CRITICAL - TensorFlow loads from:
- `@local_config_cuda` (missing)
- `@local_config_rocm` (missing)
- `@local_config_tensorrt` (missing)

Then continues with local .bzl loads. If Rush hangs here, that's the bug.

---

### test10: Complex nested with missing repos

**What it tests**: Combination pattern (real TensorFlow scenario)

```
BUILD
  load("@missing_cuda//cuda:build_defs.bzl", "if_cuda")  # Missing - should warn
  load("//local:config.bzl", "config")  # Local
    config.bzl loads more .bzl files
      Which load from @local_tsl  # Exists
        Which load from @local_config_rocm  # Missing - should warn

  genrule(...)
```

**Expected**: Multiple warnings, but completes parsing

---

## Implementation Priority

1. **test9 FIRST** - Missing external repo is most likely cause
2. test8 - Many parallel loads
3. test6 - Simple .bzl nesting
4. test7 - Deep .bzl chain
5. test10 - Full combination (if needed)

---

## Quick Implementation

Since we're at token limit, here's the fastest approach:

```bash
cd examples/tf_workspace_test

# Test 9 - Missing repo (MOST LIKELY CULPRIT)
mkdir -p test9_missing_repo
cd test9_missing_repo

# WORKSPACE
echo 'workspace(name = "test9")' > WORKSPACE

# BUILD - Load from missing repo then local
cat > BUILD << 'EOF'
load("@missing_cuda//cuda:defs.bzl", "if_cuda")
load("//:local.bzl", "local_func")

genrule(
    name = "main",
    outs = ["out.txt"],
    cmd = "echo 'test9' > $@",
)
EOF

# local.bzl
cat > local.bzl << 'EOF'
def local_func():
    pass
EOF

# Run test
rush build //:main 2>&1 | tee test9.log

# Check result
if timeout 10 rush build //:main; then
    echo "✅ test9 PASSED - no hang"
else
    echo "❌ test9 FAILED - HANGS (this is the bug!)"
fi
```

---

## Hypothesis

Based on TensorFlow output, the hang occurs after:
1. Successfully loading several .bzl files
2. Hitting missing external repos (`@local_config_cuda`, etc.)
3. Continuing to load more .bzl files

**Theory**: Rush's fallback when missing external .bzl fails creates an infinite loop or blocks forever.

**Code location**: `crates/rush-starlark/src/bzl_loader.rs` around line 377-384 where it resolves external repos.

---

## Next Steps

1. Implement test9 (5 minutes)
2. Run and observe if it hangs
3. If hangs: Add debug tracing to `bzl_loader.rs` resolve_external_bzl()
4. Fix the fallback logic to not hang
5. Verify TensorFlow build progresses further

**Estimated time to root cause**: 30 minutes with test9
