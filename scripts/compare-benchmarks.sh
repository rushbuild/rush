#!/bin/bash
# Compare two benchmark runs and show differences

if [ $# -ne 2 ]; then
    echo "Usage: $0 <baseline-log> <current-log>"
    echo "Example: $0 benchmark-results/bench-20251003-120000.log benchmark-results/bench-20251003-130000.log"
    exit 1
fi

BASELINE=$1
CURRENT=$2
TMP_DIR="${RUSH_LOG_DIR:-/tmp}/rush-tmp"
mkdir -p "$TMP_DIR"

echo "Comparing benchmarks:"
echo "  Baseline: $BASELINE"
echo "  Current:  $CURRENT"
echo ""

# Extract timing data and compare
echo "=== Performance Changes ==="
grep -E "time:\s+\[" "$BASELINE" > "$TMP_DIR/baseline.txt"
grep -E "time:\s+\[" "$CURRENT" > "$TMP_DIR/current.txt"

# Show side-by-side comparison
paste "$TMP_DIR/baseline.txt" "$TMP_DIR/current.txt" | column -t

# Extract improvement/regression percentages
echo ""
echo "=== Change Summary ==="
grep -E "change:\s+\[" "$CURRENT" || echo "No change data available"

rm -f "$TMP_DIR/baseline.txt" "$TMP_DIR/current.txt"
