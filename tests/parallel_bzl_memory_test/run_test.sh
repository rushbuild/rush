#!/bin/bash
# Test parallel .bzl loading memory consumption
set -e

TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUSH_BIN="${TEST_DIR}/../../target/release/rush"

echo "════════════════════════════════════════════════════════════════"
echo "Parallel .bzl Loading Memory Test"
echo "════════════════════════════════════════════════════════════════"
echo ""
echo "This test loads 50 .bzl files in parallel to measure memory behavior."
echo "Expected: Memory should stay reasonable (< 2GB for this simple test)"
echo ""

# Check if rush binary exists
if [ ! -f "$RUSH_BIN" ]; then
    echo "ERROR: rush binary not found at $RUSH_BIN"
    echo "Build it with: cargo build --release"
    exit 1
fi

echo "Initial memory state:"
free -h
echo ""

cd "$TEST_DIR"

# Set conservative thresholds
export RUSH_MEMORY_THRESHOLD_GB=5
export RUSH_MEMORY_EARLY_ABORT_PCT=70

echo "Memory thresholds:"
echo "  RUSH_MEMORY_THRESHOLD_GB=$RUSH_MEMORY_THRESHOLD_GB"
echo "  RUSH_MEMORY_EARLY_ABORT_PCT=$RUSH_MEMORY_EARLY_ABORT_PCT"
echo "  Effective abort point: $(echo "$RUSH_MEMORY_THRESHOLD_GB * 0.7" | bc) GB"
echo ""

START_TIME=$(date +%s)
START_MEM=$(free -m | awk 'NR==2 {print $3}')

echo "════════════════════════════════════════════════════════════════"
echo "RUSH BUILD OUTPUT:"
echo "════════════════════════════════════════════════════════════════"

set +e
"$RUSH_BIN" build //:test 2>&1 | tee test_output.log
EXIT_CODE=$?
set -e

END_TIME=$(date +%s)
END_MEM=$(free -m | awk 'NR==2 {print $3}')
DURATION=$((END_TIME - START_TIME))
MEM_DELTA=$((END_MEM - START_MEM))

echo ""
echo "════════════════════════════════════════════════════════════════"
echo "TEST RESULTS:"
echo "════════════════════════════════════════════════════════════════"
echo "Exit code: $EXIT_CODE"
if [ $EXIT_CODE -eq 137 ]; then
    echo "  → Memory protection triggered (memory consumption too high)"
elif [ $EXIT_CODE -eq 0 ]; then
    echo "  → Build completed successfully"
else
    echo "  → Build failed with error"
fi
echo ""
echo "Duration: ${DURATION}s"
echo "Memory delta: ${MEM_DELTA} MB"
echo ""
echo "Final memory state:"
free -h
echo ""

echo "Full output saved to: test_output.log"
echo ""

# Check peak memory from log
PEAK_MEM=$(grep -E "Peak memory:|Memory guard peak:" test_output.log | tail -1)
echo "From Rush logs: $PEAK_MEM"
