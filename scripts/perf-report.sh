#!/bin/bash
# Generate performance report from latest benchmark

LATEST=$(ls -t benchmark-results/bench-*.log 2>/dev/null | head -1)

if [ -z "$LATEST" ]; then
    echo "No benchmark results found. Run ./scripts/run-benchmarks.sh first."
    exit 1
fi

echo "Performance Report"
echo "=================="
echo "Generated: $(date)"
echo "Benchmark: $LATEST"
echo ""

echo "BUILD File Parsing Performance:"
echo "-------------------------------"
grep "build_file_parsing" "$LATEST" | grep "time:" | while read line; do
    echo "$line"
done

echo ""
echo "WORKSPACE Parsing Performance:"
echo "------------------------------"
grep "bazelrc_parsing" "$LATEST" | grep "time:" | while read line; do
    echo "$line"
done

echo ""
echo "Performance Changes:"
echo "--------------------"
grep "Performance has" "$LATEST" || echo "No baseline comparison available"

echo ""
echo "Full report: $LATEST"
