#!/bin/bash
# Test runner for .bzl timeout investigation
# Runs tests with increasing complexity to identify performance bottlenecks

set -e

RUSH_BIN="$(pwd)/../../target/release/rush"
RESULTS_FILE="test_results.md"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

echo "# .bzl Timeout Investigation Test Results" > $RESULTS_FILE
echo "" >> $RESULTS_FILE
echo "**Date**: $(date)" >> $RESULTS_FILE
echo "**Rush Binary**: $RUSH_BIN" >> $RESULTS_FILE
echo "" >> $RESULTS_FILE

# Build Rush first
echo -e "${YELLOW}Building Rush...${NC}"
cd ../..
cargo build --release --bin rush
cd tests/bzl_timeout_investigation

run_test() {
    local test_dir=$1
    local test_name=$2
    local target=$3
    local description=$4

    echo -e "\n${YELLOW}Running Test: $test_name${NC}"
    echo "  Directory: $test_dir"
    echo "  Target: $target"
    echo "  Description: $description"

    # Record test info
    echo "" >> $RESULTS_FILE
    echo "## Test: $test_name" >> $RESULTS_FILE
    echo "" >> $RESULTS_FILE
    echo "- **Directory**: \`$test_dir\`" >> $RESULTS_FILE
    echo "- **Target**: \`$target\`" >> $RESULTS_FILE
    echo "- **Description**: $description" >> $RESULTS_FILE
    echo "" >> $RESULTS_FILE

    cd $test_dir

    # Measure time and run
    local start_time=$(date +%s.%N)

    if timeout 120s $RUSH_BIN build $target > build.log 2>&1; then
        local end_time=$(date +%s.%N)
        local elapsed=$(echo "$end_time - $start_time" | bc)

        echo -e "  ${GREEN}✓ PASSED${NC} (${elapsed}s)"
        echo "- **Status**: ✅ PASSED" >> $RESULTS_FILE
        echo "- **Time**: ${elapsed}s" >> $RESULTS_FILE

        # Extract key metrics from log
        if grep -q "Build Summary" build.log; then
            local actions=$(grep "Actions:" build.log | head -1)
            echo "- **Metrics**: $actions" >> $RESULTS_FILE
        fi
    else
        local end_time=$(date +%s.%N)
        local elapsed=$(echo "$end_time - $start_time" | bc)

        echo -e "  ${RED}✗ FAILED${NC} (${elapsed}s)"
        echo "- **Status**: ❌ FAILED" >> $RESULTS_FILE
        echo "- **Time**: ${elapsed}s (timeout or error)" >> $RESULTS_FILE

        # Extract error from log
        if grep -q "TIMEOUT" build.log; then
            local timeout_file=$(grep "TIMEOUT evaluating" build.log | tail -1)
            echo "- **Error**: $timeout_file" >> $RESULTS_FILE
        elif grep -q "Error:" build.log; then
            local error=$(grep "Error:" build.log | tail -1)
            echo "- **Error**: $error" >> $RESULTS_FILE
        fi
    fi

    echo "" >> $RESULTS_FILE
    echo "\`\`\`" >> $RESULTS_FILE
    tail -50 build.log >> $RESULTS_FILE
    echo "\`\`\`" >> $RESULTS_FILE

    cd ..
}

echo ""
echo "========================================"
echo "  .bzl Timeout Investigation Test Suite"
echo "========================================"
echo ""

run_test "test1" "Test 1: Minimal" \
    "//:test_target" \
    "No loads, just function definitions"

run_test "test2" "Test 2: Single Load" \
    "//:test_lib" \
    "Load from stub tensorflow.bzl (no recursive loads)"

run_test "test3" "Test 3: Nested Loads" \
    "//:nested_test" \
    "3-level deep loading chain (BUILD -> strict -> tensorflow -> platform)"

run_test "test4" "Test 4: Multiple Loads" \
    "//:multiple_loads_test" \
    "Multiple parallel dependency chains (3 .bzl files)"

echo ""
echo "========================================"
echo "  Test Results Summary"
echo "========================================"
echo ""
echo "Results saved to: $RESULTS_FILE"
echo ""

# Print summary
echo "Summary:" >> $RESULTS_FILE
echo "" >> $RESULTS_FILE

cat $RESULTS_FILE | grep "Status" | while read line; do
    echo "$line"
done

echo ""
echo "View full results: cat $RESULTS_FILE"
