#!/bin/bash

set -e

RUSH_BIN="${RUSH_BIN:-../../target/debug/rush}"
RESULTS_DIR="test_results"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)

mkdir -p "$RESULTS_DIR"

echo "========================================"
echo "TensorFlow Issues Test Suite"
echo "========================================"
echo "Rush binary: $RUSH_BIN"
echo "Timestamp: $TIMESTAMP"
echo ""

run_test() {
    local test_name=$1
    local test_dir=$2
    local target=${3:-//:test}
    local timeout=${4:-30}

    echo "----------------------------------------"
    echo "Running: $test_name"
    echo "Directory: $test_dir"
    echo "Target: $target"
    echo "----------------------------------------"

    local log_file="$RESULTS_DIR/${test_name}_${TIMESTAMP}.log"
    local status_file="$RESULTS_DIR/${test_name}_status.txt"

    cd "$test_dir"

    if timeout $timeout "../../$RUSH_BIN" build "$target" > "../../$log_file" 2>&1; then
        echo "✓ PASSED: $test_name"
        echo "PASSED" > "../../$status_file"
    else
        local exit_code=$?
        echo "✗ FAILED: $test_name (exit code: $exit_code)"
        echo "FAILED (exit: $exit_code)" > "../../$status_file"
        echo ""
        echo "Last 30 lines of output:"
        tail -30 "../../$log_file"
    fi

    cd - > /dev/null
    echo ""
}

# Test 1: Tar extraction
run_test "tar_extraction" "tar_extraction_test" "//:test" 60

# Test 2: Missing repos
run_test "missing_repos" "missing_repos_test" "//:test" 30

# Test 3: Vendored repos
run_test "vendored_repos" "vendored_repos_test" "//:test" 30

# Test 4: BZL loading hang (shorter timeout to catch hangs)
run_test "bzl_loading_hang" "bzl_loading_hang_test" "//:test" 30

# Summary
echo "========================================"
echo "Test Summary"
echo "========================================"
for status_file in "$RESULTS_DIR"/*_status.txt; do
    test_name=$(basename "$status_file" _status.txt)
    status=$(cat "$status_file")
    echo "$test_name: $status"
done
echo ""
echo "Full logs available in: $RESULTS_DIR/"
echo "========================================"
