# .bzl Timeout Investigation

Testing why `//tensorflow:strict.default.bzl` times out during evaluation.

## File Structure

**strict.default.bzl** (16 lines):
- Loads 1 symbol from `//tensorflow:tensorflow.bzl`
- Defines 3 simple wrapper functions

**tensorflow.bzl** (massive file):
- Has 13+ load() statements at the top
- Each loads multiple symbols from other .bzl files
- Creates deep dependency chain

## Test Cases

### Test 1: Minimal - Just the wrapper functions
- `test1/` - strict.default.bzl WITHOUT the load()
- Tests: Basic function definitions work

### Test 2: Single load from stub
- `test2/` - strict.default.bzl WITH load from stub tensorflow.bzl
- stub tensorflow.bzl has NO loads, just exports dummy _py_test
- Tests: Single load() works

### Test 3: tensorflow.bzl with 1 load
- `test3/` - tensorflow.bzl with just first load() statement
- Tests: One deep load chain

### Test 4: tensorflow.bzl with 3 loads
- `test4/` - tensorflow.bzl with first 3 load() statements
- Tests: Multiple load chains

### Test 5: tensorflow.bzl with all loads
- `test5/` - Full tensorflow.bzl (first 150 lines)
- Tests: Full complexity (should reproduce timeout)

### Test 6: Just @local_tsl//tsl:tsl.bzl
- `test6/` - Isolated test of tsl.bzl (known to have circular deps)
- Tests: Specific problematic file

## Running Tests

```bash
# Test 1: Should be instant
cargo run --bin rush -- build //test1:target

# Test 2: Should be fast (<1s)
cargo run --bin rush -- build //test2:target

# Test 3-5: Progressively slower
# Test 6: May timeout or show circular dependency
```
