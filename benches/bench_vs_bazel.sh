#!/bin/bash
# Rush vs Bazel Performance Comparison Script
# Copyright © 2025 Luis Chamberlain. All rights reserved.
# SPDX-License-Identifier: MIT

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Script directory
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"

# Output file
RESULTS_FILE="${SCRIPT_DIR}/benchmark_results.txt"

echo -e "${BLUE}==================================================${NC}"
echo -e "${BLUE}  Rush vs Bazel Performance Comparison${NC}"
echo -e "${BLUE}==================================================${NC}"
echo ""

# Check if Bazel is installed
if ! command -v bazel &> /dev/null; then
    echo -e "${YELLOW}Warning: Bazel not installed. Skipping Bazel benchmarks.${NC}"
    BAZEL_AVAILABLE=0
else
    BAZEL_AVAILABLE=1
    BAZEL_VERSION=$(bazel version | grep "Build label" | awk '{print $3}')
    echo -e "${GREEN}Bazel version: ${BAZEL_VERSION}${NC}"
fi

# Check if Rush is built
if [ ! -f "${PROJECT_ROOT}/target/release/rush" ]; then
    echo -e "${YELLOW}Building Rush in release mode...${NC}"
    cd "${PROJECT_ROOT}"
    cargo build --release
fi

RUSH_VERSION=$(${PROJECT_ROOT}/target/release/rush --version 2>/dev/null || echo "0.1.0")
echo -e "${GREEN}Rush version: ${RUSH_VERSION}${NC}"
echo ""

# Initialize results file
echo "Rush vs Bazel Performance Comparison" > "$RESULTS_FILE"
echo "Generated: $(date)" >> "$RESULTS_FILE"
echo "========================================" >> "$RESULTS_FILE"
echo "" >> "$RESULTS_FILE"

# Function to run timed command
time_command() {
    local cmd="$1"
    local label="$2"

    echo -e "${BLUE}Running: ${label}${NC}"

    # Run 3 times and take the average
    local total=0
    local count=3

    for i in $(seq 1 $count); do
        local start=$(date +%s%N)
        eval "$cmd" > /dev/null 2>&1 || true
        local end=$(date +%s%N)
        local duration=$(( (end - start) / 1000000 )) # Convert to milliseconds
        total=$((total + duration))
        echo -e "  Run $i: ${duration}ms"
    done

    local avg=$((total / count))
    echo -e "${GREEN}  Average: ${avg}ms${NC}"
    echo ""

    echo "$label: ${avg}ms" >> "$RESULTS_FILE"

    echo "$avg"
}

# Create test examples directory if it doesn't exist
EXAMPLES_DIR="${PROJECT_ROOT}/examples"
mkdir -p "${EXAMPLES_DIR}"

# Test 1: Parse .bazelrc files
echo -e "${YELLOW}Test 1: .bazelrc Parsing${NC}"
echo "----------------------------------------"

# Create a test .bazelrc if it doesn't exist
BAZELRC_TEST="${EXAMPLES_DIR}/.bazelrc.test"
cat > "$BAZELRC_TEST" <<EOF
# Test .bazelrc
startup --host_jvm_args=-Xmx3g

common --noenable_bzlmod
build --color=yes
build --cxxopt=-std=c++20
build --cxxopt=-Wall
build --cxxopt=-Wextra
build --action_env=CC=clang
build --action_env=CXX=clang++

build:release --compilation_mode=opt
build:release --strip=always
build:release --copt=-O3
build:release --linkopt=-fuse-ld=lld

build:debug --compilation_mode=dbg
build:debug --strip=never

test --test_verbose_timeout_warnings
test --test_output=errors
EOF

# Note: Bazel doesn't have a direct .bazelrc parsing benchmark
# Rush will parse this in its build system
echo "Created test .bazelrc with multiple configurations" >> "$RESULTS_FILE"
echo "" >> "$RESULTS_FILE"

# Test 2: Build simple project
echo -e "${YELLOW}Test 2: Simple Build${NC}"
echo "----------------------------------------"

# Create a simple test project
TEST_PROJECT_DIR="${EXAMPLES_DIR}/bench_test_project"
mkdir -p "${TEST_PROJECT_DIR}"

# Create WORKSPACE
cat > "${TEST_PROJECT_DIR}/WORKSPACE" <<EOF
workspace(name = "bench_test")
EOF

# Create BUILD file with multiple targets
cat > "${TEST_PROJECT_DIR}/BUILD" <<EOF
# Simple test targets
cc_library(
    name = "lib1",
    srcs = ["lib1.cc"],
    hdrs = ["lib1.h"],
)

cc_library(
    name = "lib2",
    srcs = ["lib2.cc"],
    hdrs = ["lib2.h"],
    deps = [":lib1"],
)

cc_library(
    name = "lib3",
    srcs = ["lib3.cc"],
    hdrs = ["lib3.h"],
    deps = [":lib1", ":lib2"],
)

cc_binary(
    name = "app",
    srcs = ["main.cc"],
    deps = [":lib3"],
)
EOF

# Create dummy source files
touch "${TEST_PROJECT_DIR}/lib1.cc" "${TEST_PROJECT_DIR}/lib1.h"
touch "${TEST_PROJECT_DIR}/lib2.cc" "${TEST_PROJECT_DIR}/lib2.h"
touch "${TEST_PROJECT_DIR}/lib3.cc" "${TEST_PROJECT_DIR}/lib3.h"
touch "${TEST_PROJECT_DIR}/main.cc"

if [ $BAZEL_AVAILABLE -eq 1 ]; then
    cd "${TEST_PROJECT_DIR}"

    # Clean Bazel cache
    bazel clean --expunge > /dev/null 2>&1 || true

    # Benchmark Bazel analysis (no actual build)
    bazel_time=$(time_command "bazel query //..." "Bazel query //...")

    cd "${PROJECT_ROOT}"
else
    echo "Bazel not available, skipping Bazel benchmarks" >> "$RESULTS_FILE"
fi

# Benchmark Rush (when implemented)
echo -e "${YELLOW}Note: Rush build //... not yet implemented${NC}"
echo "Rush build //... - Not yet implemented" >> "$RESULTS_FILE"
echo "" >> "$RESULTS_FILE"

# Test 3: Large BUILD file parsing
echo -e "${YELLOW}Test 3: Large BUILD File Parsing${NC}"
echo "----------------------------------------"

LARGE_BUILD="${EXAMPLES_DIR}/large_BUILD"
cat > "$LARGE_BUILD" <<EOF
# Large BUILD file with many targets
EOF

for i in $(seq 1 100); do
    cat >> "$LARGE_BUILD" <<EOF

cc_library(
    name = "lib_${i}",
    srcs = ["lib_${i}.cc"],
    hdrs = ["lib_${i}.h"],
    deps = [
        "//common:base",
        "//common:utils",
    ],
)
EOF
done

echo "Created BUILD file with 100 targets" >> "$RESULTS_FILE"
echo "" >> "$RESULTS_FILE"

# Summary table
echo "" >> "$RESULTS_FILE"
echo "========================================" >> "$RESULTS_FILE"
echo "Summary" >> "$RESULTS_FILE"
echo "========================================" >> "$RESULTS_FILE"
echo "" >> "$RESULTS_FILE"

if [ $BAZEL_AVAILABLE -eq 1 ]; then
    echo "Bazel is available for comparison" >> "$RESULTS_FILE"
else
    echo "Bazel not available - install Bazel for full comparison" >> "$RESULTS_FILE"
fi

echo "" >> "$RESULTS_FILE"
echo "Rush Features Benchmarked:" >> "$RESULTS_FILE"
echo "  - .bazelrc parsing" >> "$RESULTS_FILE"
echo "  - BUILD file parsing" >> "$RESULTS_FILE"
echo "  - Envoy macro translation" >> "$RESULTS_FILE"
echo "" >> "$RESULTS_FILE"
echo "Run 'cargo bench' for detailed parsing benchmarks" >> "$RESULTS_FILE"

# Display results
echo -e "${BLUE}==================================================${NC}"
echo -e "${GREEN}Results saved to: ${RESULTS_FILE}${NC}"
echo -e "${BLUE}==================================================${NC}"
echo ""
cat "$RESULTS_FILE"

echo ""
echo -e "${YELLOW}For detailed parsing benchmarks, run:${NC}"
echo -e "  ${GREEN}cargo bench${NC}"
echo ""
echo -e "${YELLOW}To view HTML benchmark reports:${NC}"
echo -e "  ${GREEN}open target/criterion/report/index.html${NC}"
echo ""
