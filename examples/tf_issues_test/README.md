# TensorFlow Build Issues Test Suite

This directory contains isolated test cases to replicate and fix issues
discovered when attempting to build TensorFlow with Rush.

## Test Cases

### 1. tar_extraction_test
Tests HTTP archive extraction with various tar formats that trigger
"Invalid number of elements" errors seen in TensorFlow dependencies.

### 2. vendored_repos_test
Tests loading of vendored repositories (local_tsl, local_xla) that are
referenced via local_repository() in WORKSPACE files.

### 3. bzl_loading_hang_test
Tests complex .bzl file loading chains that cause hangs or incomplete
execution during the analysis phase.

### 4. missing_repos_test
Tests behavior when .bzl files reference external repositories that
don't exist (local_config_cuda, local_config_rocm, etc.).

## Running Tests

```bash
cd examples/tf_issues_test
./run_all_tests.sh
```

Or run individual tests:
```bash
cd tar_extraction_test && rush build //:test
cd vendored_repos_test && rush build //:test
cd bzl_loading_hang_test && rush build //:test
cd missing_repos_test && rush build //:test
```
