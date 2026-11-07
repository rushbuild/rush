#!/bin/bash
# Test runner for .bzl memory footprint analysis
#
# This script runs atomic tests for cuda_configure.bzl, rocm_configure.bzl,
# and nccl_configure.bzl to measure their individual memory footprints.

set -e

RUSH_BIN="${RUSH_BIN:-../../target/release/rush}"
TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [ ! -x "$RUSH_BIN" ]; then
    echo "ERROR: Rush binary not found at $RUSH_BIN"
    echo "Build Rush first: cargo build --release"
    exit 1
fi

echo "=== .bzl Memory Footprint Test Suite ==="
echo ""
echo "Testing individual memory consumption of TensorFlow GPU configuration files"
echo "that caused system freeze when loaded in parallel."
echo ""

run_test() {
    local test_name="$1"
    local test_dir="$TEST_DIR/$test_name"

    echo "----------------------------------------"
    echo "Testing: $test_name"
    echo "----------------------------------------"

    cd "$test_dir"

    # Clear cache to ensure fresh load
    rm -rf .cache 2>/dev/null || true

    # Run Rush and capture cache memory stats
    echo "Loading $test_name..."
    if "$RUSH_BIN" build //:test 2>&1 | tee "$test_name.log"; then
        # Extract cache memory from output
        cache_mem=$(grep "Cache memory:" "$test_name.log" | tail -1 || echo "N/A")
        peak_mem=$(grep "Peak memory:" "$test_name.log" | tail -1 || echo "N/A")

        echo "Results:"
        echo "  $cache_mem"
        echo "  $peak_mem"

        # Extract just the numbers for summary
        cache_mb=$(echo "$cache_mem" | grep -oP '\d+(?= MB)' | head -1 || echo "0")
        echo "$test_name,$cache_mb" >> "$TEST_DIR/results.csv"
    else
        echo "FAILED: $test_name build failed"
        cache_mb=0
    fi

    echo ""
    cd "$TEST_DIR"
}

# Initialize results file
echo "test_name,cache_memory_mb" > "$TEST_DIR/results.csv"

# Run individual tests
run_test "cuda_test"
run_test "rocm_test"
run_test "nccl_test"

echo "========================================="
echo "Test Summary"
echo "========================================="
echo ""

cat "$TEST_DIR/results.csv" | column -t -s ','

echo ""
echo "Detailed logs:"
echo "  - cuda_test/cuda_test.log"
echo "  - rocm_test/rocm_test.log"
echo "  - nccl_test/nccl_test.log"
echo ""
echo "Results saved to: results.csv"
