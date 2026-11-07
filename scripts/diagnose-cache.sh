#!/bin/bash
# Diagnose whether GlobalBzlCache is actually working

set -euo pipefail

RUSH_DIR="$(dirname "$0")/.."
cd "$RUSH_DIR"

LOG_DIR="${RUSH_LOG_DIR:-/tmp}"
mkdir -p "$LOG_DIR"
LOG_FILE="$LOG_DIR/cache-diagnostic.log"

echo "=== Rebuilding Rush with bzl-debug feature ==="
cargo build --release --package rush-starlark --features bzl-debug 2>&1 | tail -5
echo "Building main rush binary..."
cargo build --release 2>&1 | tail -5

TF_PATH="${TF_PATH:-${RUSH_DIR}/../tensorflow}"

if [ ! -d "$TF_PATH" ]; then
    echo "ERROR: TensorFlow repo not found at $TF_PATH"
    echo "Set TF_PATH environment variable or clone to ../tensorflow"
    exit 1
fi

echo ""
echo "=== Running TensorFlow build with memory limit and cache debugging ==="
echo "Memory limit: 8 GB"
echo "Timeout: 60 seconds (early abort)"
echo "TensorFlow path: $TF_PATH"
echo ""

cd "$TF_PATH"

RUST_BACKTRACE=1 RUSH_MEMORY_LIMIT=8G timeout 60s \
    "${RUSH_DIR}/target/release/rush" build //tensorflow/core:platform_base \
    2>&1 | tee "$LOG_FILE" || true

echo ""
echo "=== Cache Activity Summary ==="
echo ""
echo "Cache hits:"
grep -c "CACHE HIT" "$LOG_FILE" || echo "0"

echo ""
echo "Cache misses:"
grep -c "CACHE MISS" "$LOG_FILE" || echo "0"

echo ""
echo "Cache stores:"
grep -c "CACHE STORE" "$LOG_FILE" || echo "0"

echo ""
echo "Cache evictions:"
grep -c "CACHE EVICT" "$LOG_FILE" || echo "0"

echo ""
echo "=== Memory Warnings ==="
grep "Memory usage:" "$LOG_FILE" | tail -5 || echo "None found"

echo ""
echo "=== Cache Statistics (if printed) ==="
grep -A 5 "GlobalBzlCache Statistics" "$LOG_FILE" || echo "Not printed"

echo ""
echo "=== Exit Status ==="
if grep -q "killed" "$LOG_FILE"; then
    echo "❌ Process was killed (likely OOM)"
elif tail -1 "$LOG_FILE" | grep -q "error"; then
    echo "⚠️  Build failed (check log)"
else
    echo "✅ Build completed or timed out gracefully"
fi

echo ""
echo "Full log: $LOG_FILE"
