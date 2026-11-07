#!/bin/bash
# Memory profiling with heaptrack for detailed allocation traces
# Also uses cgroup limits for safety

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MEMORY_LIMIT_GB=${1:-5}  # Default 5GB hard limit
RUSH_BIN="${RUSH_BIN:-${SCRIPT_DIR}/../target/release/rush}"
TF_DIR="${TF_PATH:-${SCRIPT_DIR}/../../tensorflow}"
TARGET="//tensorflow/core:platform_base"
LOG_DIR="${RUSH_LOG_DIR:-/tmp}"

if [ ! -d "$TF_DIR" ]; then
    echo "ERROR: TensorFlow repo not found at $TF_DIR"
    echo "Set TF_PATH environment variable or clone to ../tensorflow"
    exit 1
fi

echo "=== Heaptrack Memory Profiling (Safe Mode) ==="
echo "Memory hard limit: ${MEMORY_LIMIT_GB}GB"
echo "TensorFlow path: $TF_DIR"
echo "Rush binary: $RUSH_BIN"
echo ""

# Check if heaptrack is installed
if ! command -v heaptrack &> /dev/null; then
    echo "ERROR: heaptrack not installed"
    echo "Install with: sudo apt install heaptrack heaptrack-gui"
    exit 1
fi

mkdir -p "$LOG_DIR"
PROFILE_DIR="$LOG_DIR"

TIMESTAMP=$(date +%Y%m%d-%H%M%S)
HEAPTRACK_FILE="$PROFILE_DIR/heaptrack-${TIMESTAMP}.gz"

echo "Output file: $HEAPTRACK_FILE"
echo ""
echo "Starting profiled build..."
echo "Press Ctrl+C to stop early"
echo ""

# Run with heaptrack inside systemd cgroup
systemd-run --user --scope \
    -p MemoryMax=${MEMORY_LIMIT_GB}G \
    -p MemorySwapMax=0 \
    --collect \
    bash -c "cd \"$TF_DIR\" && heaptrack --output \"$HEAPTRACK_FILE\" timeout 60s env RUSH_MEMORY_THRESHOLD_GB=100 \"$RUSH_BIN\" build $TARGET" \
    2>&1 || true

EXIT_CODE=$?

echo ""
echo "=== Profiling Complete ==="
echo "Exit code: $EXIT_CODE"

if [ -f "$HEAPTRACK_FILE" ]; then
    echo ""
    echo "Analyzing memory profile..."
    heaptrack --analyze "$HEAPTRACK_FILE" | head -100

    echo ""
    echo "=== Summary ==="
    echo "Full report: $HEAPTRACK_FILE"
    echo ""
    echo "To view detailed GUI analysis:"
    echo "  heaptrack_gui $HEAPTRACK_FILE"
    echo ""
    echo "To view text summary:"
    echo "  heaptrack --analyze $HEAPTRACK_FILE | less"
else
    echo "ERROR: No heaptrack output file generated"
fi
