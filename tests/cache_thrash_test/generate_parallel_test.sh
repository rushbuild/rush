#!/bin/bash
# Generate a test that demonstrates cache thrashing with parallel .bzl loading
# Mimics TensorFlow's pattern: many files loading shared dependencies in parallel

set -e

TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$TEST_DIR"

echo "Generating parallel cache thrashing test..."
echo "Pattern: 120 files each loading 10 shared dependencies"
echo "Total unique files: 130 (120 + 10 shared)"
echo "Symbol cache size: 100 entries"
echo "When parallel loading triggers, cache will thrash the shared dependencies"

# Clean previous test
rm -rf workspace_parallel
mkdir -p workspace_parallel

# Create 10 shared dependency files
for i in $(seq 1 10); do
    cat > workspace_parallel/shared_${i}.bzl << EOF
def shared_${i}_function():
    return "shared_${i}"

SHARED_${i}_VALUE = "shared_${i}_value"
EOF
done

# Create 120 files that each load all 10 shared dependencies
for i in $(seq 1 120); do
    echo 'load(":shared_1.bzl", "shared_1_function", "SHARED_1_VALUE")' > workspace_parallel/user_${i}.bzl
    echo 'load(":shared_2.bzl", "shared_2_function", "SHARED_2_VALUE")' >> workspace_parallel/user_${i}.bzl
    echo 'load(":shared_3.bzl", "shared_3_function", "SHARED_3_VALUE")' >> workspace_parallel/user_${i}.bzl
    echo 'load(":shared_4.bzl", "shared_4_function", "SHARED_4_VALUE")' >> workspace_parallel/user_${i}.bzl
    echo 'load(":shared_5.bzl", "shared_5_function", "SHARED_5_VALUE")' >> workspace_parallel/user_${i}.bzl
    echo 'load(":shared_6.bzl", "shared_6_function", "SHARED_6_VALUE")' >> workspace_parallel/user_${i}.bzl
    echo 'load(":shared_7.bzl", "shared_7_function", "SHARED_7_VALUE")' >> workspace_parallel/user_${i}.bzl
    echo 'load(":shared_8.bzl", "shared_8_function", "SHARED_8_VALUE")' >> workspace_parallel/user_${i}.bzl
    echo 'load(":shared_9.bzl", "shared_9_function", "SHARED_9_VALUE")' >> workspace_parallel/user_${i}.bzl
    echo 'load(":shared_10.bzl", "shared_10_function", "SHARED_10_VALUE")' >> workspace_parallel/user_${i}.bzl
    echo "" >> workspace_parallel/user_${i}.bzl
    cat >> workspace_parallel/user_${i}.bzl << EOF
def user_${i}_function():
    return "user_${i}: " + shared_1_function() + shared_2_function()

USER_${i}_VALUE = SHARED_1_VALUE + "_user_${i}"
EOF
done

# Create a BUILD file that loads all 120 files simultaneously (triggers parallel loading)
cat > workspace_parallel/BUILD << 'EOF'
# Load all 120 files - this should trigger parallel batch loading
EOF

for i in $(seq 1 120); do
    echo "load(\":user_${i}.bzl\", \"user_${i}_function\")" >> workspace_parallel/BUILD
done

cat >> workspace_parallel/BUILD << 'EOF'

print("Loaded all 120 user files successfully")
EOF

echo "Generated parallel cache thrashing test in workspace_parallel/"
echo ""
echo "Pattern:"
echo "  - 10 shared .bzl files (shared_1.bzl through shared_10.bzl)"
echo "  - 120 user .bzl files, each loading all 10 shared files"
echo "  - BUILD file loads all 120 user files simultaneously"
echo ""
echo "When parallel loading kicks in:"
echo "  1. Rush detects 120 consecutive loads"
echo "  2. Triggers parallel batch loading"
echo "  3. All 120 files try to load shared dependencies simultaneously"
echo "  4. Cache (100 entries) cannot hold all 130 files (120 + 10)"
echo "  5. Shared dependencies get evicted and re-parsed repeatedly"
echo ""
echo "To run:"
echo "  cd $TEST_DIR/workspace_parallel"
echo "  rush build //:all 2>&1 | tee ../parallel_build.log"
echo ""
echo "To check for thrashing (shared files parsed multiple times):"
echo "  grep 'Parsing .bzl file.*shared_' ../parallel_build.log | sort | uniq -c | sort -rn"
