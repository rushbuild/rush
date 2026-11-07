#!/bin/bash
# Run all Criterion benchmarks and save timestamped results

set -e

TIMESTAMP=$(date +%Y%m%d-%H%M%S)
RESULTS_DIR="benchmark-results"
mkdir -p "$RESULTS_DIR"

echo "Running benchmarks at $TIMESTAMP..."

# Run benchmarks
cargo bench --no-fail-fast 2>&1 | tee "$RESULTS_DIR/bench-$TIMESTAMP.log"

# Copy Criterion HTML reports
if [ -d "target/criterion" ]; then
    cp -r target/criterion "$RESULTS_DIR/criterion-$TIMESTAMP"
    echo "HTML reports saved to $RESULTS_DIR/criterion-$TIMESTAMP"
fi

echo "Benchmark results saved to $RESULTS_DIR/bench-$TIMESTAMP.log"
echo "Done!"
