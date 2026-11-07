#!/bin/bash
# Safe TensorFlow build test with memory limits

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUSH_BIN="${RUSH_BIN:-${SCRIPT_DIR}/../target/release/rush}"
TF_PATH="${TF_PATH:-${SCRIPT_DIR}/../../tensorflow}"

LOG_DIR="${RUSH_LOG_DIR:-/tmp}"
mkdir -p "$LOG_DIR"
LOG_FILE="$LOG_DIR/tf-safe-build.log"

echo "=== Safe TensorFlow Build Test ==="
echo "Memory limit: 10GB (will abort at 6GB)"
echo "Timeout: 30 seconds"
echo "TensorFlow path: $TF_PATH"
echo "Rush binary: $RUSH_BIN"
echo ""

if [ ! -d "$TF_PATH" ]; then
    echo "ERROR: TensorFlow repo not found at $TF_PATH"
    echo "Set TF_PATH environment variable or clone to ../tensorflow"
    exit 1
fi

cd "$TF_PATH"

echo "Starting build..."
export RUSH_MEMORY_THRESHOLD_GB=10
timeout 30s "$RUSH_BIN" build //tensorflow/core:platform_base 2>&1 | tee "$LOG_FILE"

echo ""
echo "=== Results ==="
echo ""
echo "Singleton GlobalBzlCache initialized:"
grep -c "Initializing singleton GlobalBzlCache" "$LOG_FILE" || echo "0"

echo ""
echo "BzlLoaders with global cache (local caches disabled):"
grep -c "Global cache present - disabling local caches" "$LOG_FILE" || echo "0"

echo ""
echo "Memory warnings:"
grep -c "MEMORY USAGE HIGH" "$LOG_FILE" || echo "0"

echo ""
echo "Peak memory usage:"
grep "Peak memory:" "$LOG_FILE" | tail -1 || echo "Not recorded"

echo ""
echo "Full log: $LOG_FILE"
