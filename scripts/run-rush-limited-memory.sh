#!/bin/bash
# Run Rush with strict memory limits using systemd-run
# This prevents Rush from consuming all system memory and thrashing the desktop

set -euo pipefail

# Default memory limit: 8 GB (adjust as needed)
MEMORY_LIMIT="${RUSH_MEMORY_LIMIT:-8G}"

# Get Rush binary path
RUSH_BIN="${RUSH_BIN:-$(dirname "$0")/../target/release/rush}"

if [ ! -x "$RUSH_BIN" ]; then
    echo "Error: Rush binary not found at: $RUSH_BIN"
    echo "Build it with: cargo build --release"
    exit 1
fi

# Check if systemd-run is available
if ! command -v systemd-run &> /dev/null; then
    echo "Error: systemd-run not found. Install systemd."
    exit 1
fi

echo "Running Rush with ${MEMORY_LIMIT} memory limit..."
echo "Command: $RUSH_BIN $@"
echo ""

# Run Rush in a systemd scope with memory limit
# --user: Run in user session (no root needed)
# --scope: Create a transient scope unit
# -p MemoryMax: Hard memory limit (OOM kill if exceeded)
# -p MemoryHigh: Throttle at 90% of limit
# -q: Quiet mode (don't print unit name)

exec systemd-run \
    --user \
    --scope \
    -p MemoryMax="${MEMORY_LIMIT}" \
    -p MemoryHigh="$((${MEMORY_LIMIT%G} * 90 / 100))G" \
    -q \
    -- \
    "$RUSH_BIN" "$@"
