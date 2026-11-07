#!/bin/bash
# CI-friendly benchmark runner
# Exits with error code if performance regresses

set -e

REGRESSION_THRESHOLD=10  # Fail if >10% slower
LOG_DIR="${RUSH_LOG_DIR:-/tmp}"
mkdir -p "$LOG_DIR"
LOG_FILE="$LOG_DIR/bench-ci.log"

echo "Running CI benchmarks..."
cargo bench --no-fail-fast 2>&1 | tee "$LOG_FILE"

# Check for regressions
if grep -q "Performance has regressed" "$LOG_FILE"; then
    echo "❌ PERFORMANCE REGRESSION DETECTED!"

    # Extract regression percentages
    grep "Performance has regressed" -B 1 "$LOG_FILE"

    exit 1
else
    echo "✅ No performance regressions detected"
    exit 0
fi
