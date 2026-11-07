#!/bin/bash
# Atomic memory test for tsl.bzl loading
set -e

TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUSH_BIN="${TEST_DIR}/../../target/release/rush"

echo "════════════════════════════════════════════════════════════════"
echo "Memory Test: TSL tsl.bzl Loading"
echo "════════════════════════════════════════════════════════════════"
echo ""
echo "This test loads tsl.bzl by building a TSL target."
echo "Tests Rush's memory handling with real TensorFlow dependencies."
echo "With aggressive protection, expect:"
echo "  - Abort if memory > 10.5GB (70% of 15GB threshold)"
echo "  - Exit code 137 if memory guard triggers"
echo "  - Immediate abort on PSI critical pressure"
echo ""
echo "Press Ctrl+C to abort, or Enter to continue..."
read

# Check if TensorFlow repo exists
# Default to ../tensorflow relative to rush repo root, or use TF_PATH env var
TF_PATH="${TF_PATH:-${TEST_DIR}/../../../tensorflow}"
TF_PATH="$(realpath "$TF_PATH" 2>/dev/null || echo "$TF_PATH")"
if [ ! -d "$TF_PATH" ]; then
    echo "ERROR: TensorFlow repo not found at $TF_PATH"
    echo "Clone it with: git clone https://github.com/tensorflow/tensorflow.git \$(dirname \"\$(pwd)\")/tensorflow"
    echo "Or set TF_PATH environment variable to point to your TensorFlow repo"
    exit 1
fi
echo "Using TensorFlow at: $TF_PATH"

# Check if TSL workspace exists
TSL_PATH="$TF_PATH/third_party/xla/third_party/tsl"
if [ ! -d "$TSL_PATH" ]; then
    echo "ERROR: TSL workspace not found at $TSL_PATH"
    exit 1
fi

# Check if rush binary exists
if [ ! -f "$RUSH_BIN" ]; then
    echo "ERROR: rush binary not found at $RUSH_BIN"
    echo "Build it with: cargo build --release"
    exit 1
fi

echo "Initial memory state:"
free -h
echo ""
cat /proc/pressure/memory
echo ""

echo "Starting test at $(date)..."
echo "Running from TensorFlow workspace: $TF_PATH"
echo ""

# Run from TensorFlow's main workspace
cd "$TF_PATH"

# Run with memory monitoring
# Set conservative thresholds for testing:
# - 15GB max threshold (to trigger on smaller systems)
# - 70% early abort = 10.5GB effective
export RUSH_MEMORY_THRESHOLD_GB=15
export RUSH_MEMORY_EARLY_ABORT_PCT=70

echo "Memory thresholds:"
echo "  RUSH_MEMORY_THRESHOLD_GB=$RUSH_MEMORY_THRESHOLD_GB"
echo "  RUSH_MEMORY_EARLY_ABORT_PCT=$RUSH_MEMORY_EARLY_ABORT_PCT"
echo "  Effective abort point: $(echo "$RUSH_MEMORY_THRESHOLD_GB * 0.7" | bc) GB"
echo ""

# Capture start time
START_TIME=$(date +%s)
START_MEM=$(free -m | awk 'NR==2 {print $3}')

echo "════════════════════════════════════════════════════════════════"
echo "RUSH BUILD OUTPUT:"
echo "════════════════════════════════════════════════════════════════"

set +e
# Clean first to force actual compilation (not just .bzl loading)
echo "Cleaning previous build state..."
"$RUSH_BIN" clean 2>&1 | head -5

# Build a simple TensorFlow target that loads @local_tsl//tsl:tsl.bzl
# This triggers both memory-intensive .bzl loading AND actual compilation
"$RUSH_BIN" build //tensorflow/core:platform_base 2>&1 | tee "$TEST_DIR/test_output.log"
EXIT_CODE=$?
set -e

# Capture end time
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
    echo "  → Memory protection triggered (expected behavior)"
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
cat /proc/pressure/memory
echo ""

echo "Full output saved to: $TEST_DIR/test_output.log"
echo ""
echo "To analyze memory behavior during test, run:"
echo "  grep -E 'Memory|PSI' $TEST_DIR/test_output.log"
