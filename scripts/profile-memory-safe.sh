#!/bin/bash
# Safe memory profiling with hard cgroup limits
# Uses systemd-run to contain memory usage

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MEMORY_LIMIT_GB=${1:-5}  # Default 5GB hard limit
TIMEOUT_SEC=${2:-60}     # Default 60 second timeout
RUSH_BIN="${RUSH_BIN:-${SCRIPT_DIR}/../target/release/rush}"
TF_DIR="${TF_PATH:-${SCRIPT_DIR}/../../tensorflow}"
TARGET="//tensorflow/core:platform_base"
LOG_DIR="${RUSH_LOG_DIR:-/tmp}"

if [ ! -d "$TF_DIR" ]; then
    echo "ERROR: TensorFlow repo not found at $TF_DIR"
    echo "Set TF_PATH environment variable or clone to ../tensorflow"
    exit 1
fi

echo "=== Safe Memory Profiling Test ==="
echo "Memory hard limit: ${MEMORY_LIMIT_GB}GB (kernel will OOM-kill if exceeded)"
echo "Timeout: ${TIMEOUT_SEC}s"
echo "Target: ${TARGET}"
echo "TensorFlow path: $TF_DIR"
echo "Rush binary: $RUSH_BIN"
echo ""

# Create output directory
mkdir -p "$LOG_DIR"
PROFILE_DIR="$LOG_DIR"

TIMESTAMP=$(date +%Y%m%d-%H%M%S)
LOG_FILE="$PROFILE_DIR/profile-${TIMESTAMP}.log"

echo "Logs will be saved to: $LOG_FILE"
echo ""

# Kill any existing Rush processes first
pkill -9 rush || true
sleep 2

echo "Starting profiled build in isolated cgroup..."
echo "If memory exceeds ${MEMORY_LIMIT_GB}GB, kernel will kill the process (desktop stays safe)"
echo ""

# Use systemd-run to create isolated scope with hard memory limit
# --scope: Creates a transient scope unit
# --user: Run in user slice (not system)
# -p MemoryMax: Hard limit (kernel OOM kills if exceeded)
# -p MemorySwapMax=0: Disable swap for this cgroup
# --collect: Automatically clean up the scope when done

systemd-run --user --scope \
    -p MemoryMax=${MEMORY_LIMIT_GB}G \
    -p MemorySwapMax=0 \
    --collect \
    bash -c "cd \"$TF_DIR\" && timeout ${TIMEOUT_SEC}s env RUSH_MEMORY_THRESHOLD_GB=100 \"$RUSH_BIN\" build $TARGET" \
    2>&1 | tee "$LOG_FILE" || true

EXIT_CODE=$?

echo ""
echo "=== Results ==="
echo "Exit code: $EXIT_CODE"
if [ $EXIT_CODE -eq 137 ]; then
    echo "Status: Process was OOM-killed by kernel (exceeded ${MEMORY_LIMIT_GB}GB)"
elif [ $EXIT_CODE -eq 124 ]; then
    echo "Status: Timeout after ${TIMEOUT_SEC}s"
elif [ $EXIT_CODE -eq 0 ]; then
    echo "Status: Completed successfully"
else
    echo "Status: Failed with exit code $EXIT_CODE"
fi

echo ""
echo "Memory usage statistics:"
grep -E "Peak memory:|Memory Context:|MEMORY USAGE HIGH" "$LOG_FILE" | tail -10 || echo "No memory stats found"

echo ""
echo "Lines of output: $(wc -l < "$LOG_FILE")"
echo "Full log: $LOG_FILE"
