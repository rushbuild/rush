#!/bin/bash
# Rush vs Bazel Performance Comparison Script
# Copyright © 2025 Luis Chamberlain. All rights reserved.
# SPDX-License-Identifier: MIT

set -euo pipefail

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Configuration
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
RESULTS_DIR="${PROJECT_ROOT}/benchmark-results"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
RESULTS_FILE="${RESULTS_DIR}/comparison_${TIMESTAMP}.md"

# Performance targets
TARGET_SPEEDUP=2.0  # We want to be 2-3x faster than Bazel

# Test projects
declare -a PROJECTS=(
    "examples/bench_project_small"
    "examples/bench_project_medium"
    "examples/bench_project_large"
    "examples/bench_project_complex"
)

# Metrics to track
declare -A RUSH_CLEAN_BUILD_TIMES
declare -A BAZEL_CLEAN_BUILD_TIMES
declare -A RUSH_INCREMENTAL_BUILD_TIMES
declare -A BAZEL_INCREMENTAL_BUILD_TIMES
declare -A RUSH_CACHE_HIT_RATES
declare -A BAZEL_CACHE_HIT_RATES
declare -A RUSH_MEMORY_USAGE
declare -A BAZEL_MEMORY_USAGE
declare -A RUSH_CPU_USAGE
declare -A BAZEL_CPU_USAGE

# Helper functions
log_info() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

log_success() {
    echo -e "${GREEN}[SUCCESS]${NC} $1"
}

log_warning() {
    echo -e "${YELLOW}[WARNING]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

# Check prerequisites
check_prerequisites() {
    log_info "Checking prerequisites..."

    if ! command -v bazel &> /dev/null; then
        log_warning "Bazel not found. Install it for comparison benchmarks."
        log_warning "Continuing with Rush-only benchmarks."
        BAZEL_AVAILABLE=false
    else
        BAZEL_AVAILABLE=true
        log_success "Bazel found: $(bazel version | head -n1)"
    fi

    if ! command -v cargo &> /dev/null; then
        log_error "Cargo not found. Please install Rust."
        exit 1
    fi

    if ! command -v time &> /dev/null; then
        log_error "GNU time not found. Please install it."
        exit 1
    fi

    # Check for perf tool (Linux only)
    if command -v perf &> /dev/null; then
        PERF_AVAILABLE=true
    else
        PERF_AVAILABLE=false
        log_warning "perf tool not available. CPU/memory metrics will be approximate."
    fi
}

# Build Rush from source
build_rush() {
    log_info "Building Rush in release mode..."
    cd "$PROJECT_ROOT"
    cargo build --release --bin rush
    log_success "Rush built successfully"
}

# Measure build time with detailed metrics
measure_build_time() {
    local build_tool=$1
    local project_dir=$2
    local build_type=$3  # clean or incremental
    local project_name=$(basename "$project_dir")

    log_info "Measuring ${build_type} build time for ${project_name} with ${build_tool}..."

    # Clean build artifacts if clean build
    if [ "$build_type" == "clean" ]; then
        if [ "$build_tool" == "rush" ]; then
            rm -rf "${project_dir}/rush-out" "${project_dir}/.rush-cache" 2>/dev/null || true
        else
            (cd "$project_dir" && bazel clean --expunge 2>/dev/null || true)
        fi
    fi

    # Prepare for measurement
    local time_file=$(mktemp)
    local mem_file=$(mktemp)

    # Run build with timing
    local start_time=$(date +%s.%N)

    if [ "$build_tool" == "rush" ]; then
        if [ "$PERF_AVAILABLE" == "true" ]; then
            /usr/bin/time -v "${PROJECT_ROOT}/target/release/rush" build //... \
                --project-dir "$project_dir" 2> "$time_file" || true
        else
            /usr/bin/time -p "${PROJECT_ROOT}/target/release/rush" build //... \
                --project-dir "$project_dir" 2> "$time_file" || true
        fi
    else
        if [ "$PERF_AVAILABLE" == "true" ]; then
            (cd "$project_dir" && /usr/bin/time -v bazel build //... 2> "$time_file") || true
        else
            (cd "$project_dir" && /usr/bin/time -p bazel build //... 2> "$time_file") || true
        fi
    fi

    local end_time=$(date +%s.%N)
    local elapsed=$(echo "$end_time - $start_time" | bc)

    # Extract metrics
    local memory_kb=0
    local cpu_percent=0

    if [ "$PERF_AVAILABLE" == "true" ]; then
        memory_kb=$(grep "Maximum resident set size" "$time_file" | awk '{print $6}')
        cpu_percent=$(grep "Percent of CPU" "$time_file" | awk '{print $7}' | tr -d '%')
    fi

    # Store results
    if [ "$build_type" == "clean" ]; then
        if [ "$build_tool" == "rush" ]; then
            RUSH_CLEAN_BUILD_TIMES["$project_name"]=$elapsed
            RUSH_MEMORY_USAGE["$project_name"]=$memory_kb
            RUSH_CPU_USAGE["$project_name"]=$cpu_percent
        else
            BAZEL_CLEAN_BUILD_TIMES["$project_name"]=$elapsed
            BAZEL_MEMORY_USAGE["$project_name"]=$memory_kb
            BAZEL_CPU_USAGE["$project_name"]=$cpu_percent
        fi
    else
        if [ "$build_tool" == "rush" ]; then
            RUSH_INCREMENTAL_BUILD_TIMES["$project_name"]=$elapsed
        else
            BAZEL_INCREMENTAL_BUILD_TIMES["$project_name"]=$elapsed
        fi
    fi

    # Cleanup
    rm -f "$time_file" "$mem_file"

    log_success "${build_tool} ${build_type} build completed in ${elapsed}s"
}

# Measure cache hit rate
measure_cache_hit_rate() {
    local build_tool=$1
    local project_dir=$2
    local project_name=$(basename "$project_dir")

    log_info "Measuring cache hit rate for ${project_name} with ${build_tool}..."

    # Build once to populate cache
    if [ "$build_tool" == "rush" ]; then
        "${PROJECT_ROOT}/target/release/rush" build //... --project-dir "$project_dir" &>/dev/null || true
    else
        (cd "$project_dir" && bazel build //... &>/dev/null) || true
    fi

    # Clean outputs but keep cache
    if [ "$build_tool" == "rush" ]; then
        rm -rf "${project_dir}/rush-out" 2>/dev/null || true
    else
        (cd "$project_dir" && bazel clean 2>/dev/null) || true
    fi

    # Rebuild and measure cache hits
    local cache_output=$(mktemp)

    if [ "$build_tool" == "rush" ]; then
        "${PROJECT_ROOT}/target/release/rush" build //... --project-dir "$project_dir" \
            --verbose 2>&1 | tee "$cache_output" || true
        # Parse cache hit rate from Rush output
        local cache_hits=$(grep -c "cache hit" "$cache_output" || echo "0")
        local total_actions=$(grep -c "action" "$cache_output" || echo "1")
        local hit_rate=$(echo "scale=2; $cache_hits * 100 / $total_actions" | bc)
        RUSH_CACHE_HIT_RATES["$project_name"]=$hit_rate
    else
        (cd "$project_dir" && bazel build //... 2>&1 | tee "$cache_output") || true
        # Parse cache statistics from Bazel
        local cache_hits=$(grep -c "remote cache hit" "$cache_output" || echo "0")
        local total_actions=$(grep -c "action" "$cache_output" || echo "1")
        local hit_rate=$(echo "scale=2; $cache_hits * 100 / $total_actions" | bc)
        BAZEL_CACHE_HIT_RATES["$project_name"]=$hit_rate
    fi

    rm -f "$cache_output"
    log_success "Cache hit rate: ${hit_rate}%"
}

# Run benchmarks for all projects
run_benchmarks() {
    log_info "Running benchmarks for all projects..."

    for project in "${PROJECTS[@]}"; do
        local project_path="${PROJECT_ROOT}/${project}"

        if [ ! -d "$project_path" ]; then
            log_warning "Project not found: $project_path (skipping)"
            continue
        fi

        log_info "Benchmarking project: $project"

        # Rush benchmarks
        measure_build_time "rush" "$project_path" "clean"
        measure_build_time "rush" "$project_path" "incremental"
        measure_cache_hit_rate "rush" "$project_path"

        # Bazel benchmarks (if available)
        if [ "$BAZEL_AVAILABLE" == "true" ]; then
            measure_build_time "bazel" "$project_path" "clean"
            measure_build_time "bazel" "$project_path" "incremental"
            measure_cache_hit_rate "bazel" "$project_path"
        fi
    done
}

# Generate comparison report
generate_report() {
    log_info "Generating comparison report..."

    mkdir -p "$RESULTS_DIR"

    cat > "$RESULTS_FILE" <<EOF
# Rush vs Bazel Performance Comparison

**Date:** $(date)
**Rush Version:** $(cd "$PROJECT_ROOT" && cargo pkgid | cut -d'#' -f2)
EOF

    if [ "$BAZEL_AVAILABLE" == "true" ]; then
        cat >> "$RESULTS_FILE" <<EOF
**Bazel Version:** $(bazel version | head -n1)

## Performance Target

Rush aims to be **${TARGET_SPEEDUP}x faster** than Bazel.

## Results Summary

| Project | Metric | Rush | Bazel | Speedup | Target Met |
|---------|--------|------|-------|---------|------------|
EOF

        for project in "${PROJECTS[@]}"; do
            local project_name=$(basename "$project")

            # Clean build comparison
            local rush_time=${RUSH_CLEAN_BUILD_TIMES[$project_name]:-0}
            local bazel_time=${BAZEL_CLEAN_BUILD_TIMES[$project_name]:-0}

            if (( $(echo "$bazel_time > 0" | bc -l) )); then
                local speedup=$(echo "scale=2; $bazel_time / $rush_time" | bc)
                local target_met="✅"
                if (( $(echo "$speedup < $TARGET_SPEEDUP" | bc -l) )); then
                    target_met="❌"
                fi

                cat >> "$RESULTS_FILE" <<EOF
| $project_name | Clean Build | ${rush_time}s | ${bazel_time}s | ${speedup}x | $target_met |
EOF
            fi

            # Incremental build comparison
            rush_time=${RUSH_INCREMENTAL_BUILD_TIMES[$project_name]:-0}
            bazel_time=${BAZEL_INCREMENTAL_BUILD_TIMES[$project_name]:-0}

            if (( $(echo "$bazel_time > 0" | bc -l) )); then
                local speedup=$(echo "scale=2; $bazel_time / $rush_time" | bc)
                local target_met="✅"
                if (( $(echo "$speedup < $TARGET_SPEEDUP" | bc -l) )); then
                    target_met="❌"
                fi

                cat >> "$RESULTS_FILE" <<EOF
| $project_name | Incremental | ${rush_time}s | ${bazel_time}s | ${speedup}x | $target_met |
EOF
            fi

            # Cache hit rate comparison
            local rush_cache=${RUSH_CACHE_HIT_RATES[$project_name]:-0}
            local bazel_cache=${BAZEL_CACHE_HIT_RATES[$project_name]:-0}

            cat >> "$RESULTS_FILE" <<EOF
| $project_name | Cache Hit % | ${rush_cache}% | ${bazel_cache}% | N/A | N/A |
EOF

            # Memory usage comparison
            if [ "$PERF_AVAILABLE" == "true" ]; then
                local rush_mem=${RUSH_MEMORY_USAGE[$project_name]:-0}
                local bazel_mem=${BAZEL_MEMORY_USAGE[$project_name]:-0}
                local rush_mem_mb=$(echo "scale=2; $rush_mem / 1024" | bc)
                local bazel_mem_mb=$(echo "scale=2; $bazel_mem / 1024" | bc)

                cat >> "$RESULTS_FILE" <<EOF
| $project_name | Memory (MB) | ${rush_mem_mb} | ${bazel_mem_mb} | N/A | N/A |
EOF
            fi
        done
    else
        cat >> "$RESULTS_FILE" <<EOF

**Note:** Bazel not available. Showing Rush benchmarks only.

## Rush Performance Results

| Project | Clean Build | Incremental Build | Cache Hit % |
|---------|-------------|-------------------|-------------|
EOF

        for project in "${PROJECTS[@]}"; do
            local project_name=$(basename "$project")
            local clean_time=${RUSH_CLEAN_BUILD_TIMES[$project_name]:-0}
            local incr_time=${RUSH_INCREMENTAL_BUILD_TIMES[$project_name]:-0}
            local cache_rate=${RUSH_CACHE_HIT_RATES[$project_name]:-0}

            cat >> "$RESULTS_FILE" <<EOF
| $project_name | ${clean_time}s | ${incr_time}s | ${cache_rate}% |
EOF
        done
    fi

    cat >> "$RESULTS_FILE" <<EOF

## Detailed Metrics

### Clean Build Times
EOF

    for project in "${PROJECTS[@]}"; do
        local project_name=$(basename "$project")
        cat >> "$RESULTS_FILE" <<EOF

**$project_name:**
- Rush: ${RUSH_CLEAN_BUILD_TIMES[$project_name]:-N/A}s
- Bazel: ${BAZEL_CLEAN_BUILD_TIMES[$project_name]:-N/A}s
EOF
    done

    cat >> "$RESULTS_FILE" <<EOF

## Conclusions

EOF

    if [ "$BAZEL_AVAILABLE" == "true" ]; then
        cat >> "$RESULTS_FILE" <<EOF
Rush is designed to achieve ${TARGET_SPEEDUP}x speedup over Bazel by:
1. Eliminating JVM overhead
2. Using Rust's zero-cost abstractions
3. Optimizing parallel execution
4. Implementing efficient caching strategies

See detailed results above for current performance status.
EOF
    else
        cat >> "$RESULTS_FILE" <<EOF
Install Bazel to enable comparative benchmarks.

Rush baseline performance metrics have been recorded.
EOF
    fi

    log_success "Report generated: $RESULTS_FILE"
}

# Main execution
main() {
    log_info "Rush vs Bazel Performance Benchmark Suite"
    log_info "==========================================="

    check_prerequisites
    build_rush
    run_benchmarks
    generate_report

    log_success "Benchmarking complete!"
    log_info "Results: $RESULTS_FILE"
}

main "$@"
