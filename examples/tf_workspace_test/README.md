# TensorFlow WORKSPACE Loading Test Suite

This directory contains incremental test cases that isolate specific TensorFlow WORKSPACE patterns that caused Rush to hang.

## Test Progression

Each test builds on the previous one, isolating different aspects:

1. **test1_nested_workspace**: Simple local_repository with nested WORKSPACE
2. **test2_tf_vendored**: tf_vendored() custom repository rule (minimal @local_tsl pattern)
3. **test3_workspace_cascade**: Workspace function cascade (workspace3→workspace2→workspace1)
4. **test4_external_bzl_load**: Loading .bzl files from external repos (@repo//path:file.bzl)

## Expected Behavior

- **test1**: Should register external repo and process its nested WORKSPACE ✓
- **test2**: Should execute tf_vendored() and create symlink repo ✓
- **test3**: Should call workspace functions in sequence without deadlock ✓
- **test4**: Should load .bzl files from registered external repos ✓

## Running Tests

```bash
cd examples/tf_workspace_test/test1_nested_workspace
rush build //:main

cd ../test2_tf_vendored
rush build //:main

cd ../test3_workspace_cascade
rush build //:main

cd ../test4_external_bzl_load
rush build //:main
```

## TensorFlow Patterns Tested

Based on actual TensorFlow source code:
- `tensorflow/workspace3.bzl:9-10` - tf_vendored(@local_tsl)
- `third_party/repo.bzl:143-160` - tf_vendored implementation
- WORKSPACE cascading macro calls
