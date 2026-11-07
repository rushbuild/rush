#!/bin/bash
# Generate a test case that demonstrates LRU cache thrashing
# Creates more .bzl files than the symbol_cache can hold (100 entries)

set -e

TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$TEST_DIR"

echo "Generating cache thrashing test with 150 interdependent .bzl files..."
echo "Symbol cache size: 100 entries (from bzl_loader.rs:71)"
echo "This will cause thrashing when all files need to be cached simultaneously"

# Clean previous test
rm -rf workspace
mkdir -p workspace

# Create 150 .bzl files, each loading the previous one
# This creates a dependency chain: file_150 -> file_149 -> ... -> file_1 -> base.bzl
# When loading file_150, ALL 150 files must be in cache to avoid re-parsing

# Create base.bzl
cat > workspace/base.bzl << 'EOF'
def base_function():
    return "base"

BASE_VALUE = "base_value"
EOF

# Create file_1.bzl through file_150.bzl
for i in $(seq 1 150); do
    if [ $i -eq 1 ]; then
        prev="base"
    else
        prev="file_$((i-1))"
    fi

    cat > workspace/file_${i}.bzl << EOF
load(":${prev}.bzl", "${prev}_function", "${prev^^}_VALUE")

def file_${i}_function():
    return "${prev}_function() + file_${i}"

FILE_${i}_VALUE = "${prev^^}_VALUE + _file_${i}"
EOF
done

# Create a BUILD file that loads the final file in the chain
cat > workspace/BUILD << 'EOF'
load(":file_150.bzl", "file_150_function", "FILE_150_VALUE")

# This BUILD file depends on all 150 .bzl files being loaded
# With symbol_cache limited to 100 entries, files will be evicted and re-parsed
print("Loaded file_150_function successfully")
print("FILE_150_VALUE =", FILE_150_VALUE)
EOF

echo "Generated 150 interdependent .bzl files in workspace/"
echo ""
echo "To run the test:"
echo "  cd $TEST_DIR/workspace"
echo "  rush build //:all 2>&1 | tee ../build.log"
echo ""
echo "Expected behavior:"
echo "  - With cache size < 150, files will be evicted during the load chain"
echo "  - When a file is needed again, it will be re-parsed"
echo "  - Look for repeated 'Parsing .bzl file' messages in build.log"
echo ""
echo "To check for thrashing:"
echo "  grep 'Parsing .bzl file' build.log | sort | uniq -c | sort -rn"
