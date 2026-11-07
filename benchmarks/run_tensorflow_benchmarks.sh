#!/bin/bash
# TensorFlow Rush Benchmark Suite

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUSH_DIR="${SCRIPT_DIR}/.."
RUSH="${RUSH_BIN:-${RUSH_DIR}/target/release/rush}"
TF_DIR="${TF_PATH:-${RUSH_DIR}/../tensorflow}"
RESULTS_FILE="${RUSH_DIR}/benchmarks/rush_tensorflow_results.md"

if [ ! -d "$TF_DIR" ]; then
    echo "ERROR: TensorFlow repo not found at $TF_DIR"
    echo "Set TF_PATH environment variable or clone to ../tensorflow"
    exit 1
fi

echo "Using TensorFlow at: $TF_DIR"
echo "Using Rush at: $RUSH"
echo ""

cd "$TF_DIR"

echo "# TensorFlow Build Benchmarks - Rush" > "$RESULTS_FILE"
echo "" >> "$RESULTS_FILE"
echo "**Date**: $(date)" >> "$RESULTS_FILE"
echo "**TensorFlow Version**: v2.15" >> "$RESULTS_FILE"
echo "**Rush Version**: 0.1.0 ($(git -C \"$RUSH_DIR\" rev-parse --short HEAD))" >> "$RESULTS_FILE"
echo "**System**: $(uname -s) $(uname -m)" >> $RESULTS_FILE
echo "**CPU**: $(lscpu | grep 'Model name' | cut -d: -f2 | xargs)" >> $RESULTS_FILE
echo "**Cores**: $(nproc)" >> $RESULTS_FILE
echo "" >> $RESULTS_FILE

echo "## Benchmark Results" >> $RESULTS_FILE
echo "" >> $RESULTS_FILE

# Benchmark 1: Simple header-only library
echo "=== Test 1: Header-Only Library (//tensorflow/c:c_api) ==="
echo "### 1. Header-Only Library (//tensorflow/c:c_api)" >> "$RESULTS_FILE"
"$RUSH" clean 2>&1 | head -2
echo "\`\`\`" >> "$RESULTS_FILE"
{ time "$RUSH" build //tensorflow/c:c_api 2>&1; } 2>&1 | tee -a "$RESULTS_FILE"
echo "\`\`\`" >> "$RESULTS_FILE"
echo "" >> "$RESULTS_FILE"

# Benchmark 2: No-op rebuild
echo "=== Test 2: No-Op Rebuild (cached) ==="
echo "### 2. No-Op Rebuild (//tensorflow/c:c_api)" >> "$RESULTS_FILE"
echo "\`\`\`" >> "$RESULTS_FILE"
{ time "$RUSH" build //tensorflow/c:c_api 2>&1; } 2>&1 | tee -a "$RESULTS_FILE"
echo "\`\`\`" >> "$RESULTS_FILE"
echo "" >> "$RESULTS_FILE"

# Benchmark 3: Simple binary with actual compilation
echo "=== Test 3: Simple Binary with Compilation (//rush_test:hello) ==="
echo "### 3. Simple Binary (//rush_test:hello)" >> "$RESULTS_FILE"
"$RUSH" clean 2>&1 | head -2
echo "\`\`\`" >> "$RESULTS_FILE"
{ time "$RUSH" build //rush_test:hello 2>&1; } 2>&1 | tee -a "$RESULTS_FILE"
echo "\`\`\`" >> "$RESULTS_FILE"
echo "" >> "$RESULTS_FILE"

# Benchmark 4: No-op on simple binary
echo "=== Test 4: No-Op on Simple Binary ==="
echo "### 4. No-Op on Simple Binary (//rush_test:hello)" >> "$RESULTS_FILE"
echo "\`\`\`" >> "$RESULTS_FILE"
{ time "$RUSH" build //rush_test:hello 2>&1; } 2>&1 | tee -a "$RESULTS_FILE"
echo "\`\`\`" >> "$RESULTS_FILE"
echo "" >> "$RESULTS_FILE"

# Benchmark 5: Rush example baseline
echo "=== Test 5: Rush Example (Baseline) ==="
echo "### 5. Rush hello_cpp Example (Baseline)" >> "$RESULTS_FILE"
cd "${RUSH_DIR}/examples/hello_cpp"
"$RUSH" clean 2>&1 | head -2
echo "\`\`\`" >> "$RESULTS_FILE"
{ time "$RUSH" build //:hello 2>&1; } 2>&1 | tee -a "$RESULTS_FILE"
echo "\`\`\`" >> "$RESULTS_FILE"
echo "" >> "$RESULTS_FILE"

echo "" >> "$RESULTS_FILE"
echo "Benchmark complete! Results saved to: $RESULTS_FILE"
