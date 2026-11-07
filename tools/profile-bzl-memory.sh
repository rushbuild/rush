#!/bin/bash
# Binary search profiling for .bzl memory leak
# Finds which .bzl file causes OOM by loading incrementally

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUSH_DIR="${SCRIPT_DIR}/.."
RUSH="${RUSH_BIN:-${RUSH_DIR}/target/release/rush}"
TF_DIR="${TF_PATH:-${RUSH_DIR}/../tensorflow}"
TARGET="//tensorflow/c:c_api"
MEMORY_LIMIT="20G"
LOG_DIR="${RUSH_LOG_DIR:-/tmp}"

if [ ! -d "$TF_DIR" ]; then
    echo "ERROR: TensorFlow repo not found at $TF_DIR"
    echo "Set TF_PATH environment variable or clone to ../tensorflow"
    exit 1
fi

# Ensure log directory exists
mkdir -p "$LOG_DIR"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

cd "$TF_DIR"

echo -e "${GREEN}╔════════════════════════════════════════════════════════════════╗${NC}"
echo -e "${GREEN}║ Binary Search Profiling for .bzl Memory Leak                 ║${NC}"
echo -e "${GREEN}╚════════════════════════════════════════════════════════════════╝${NC}"
echo ""
echo "Target: $TARGET"
echo "Memory cap: $MEMORY_LIMIT"
echo ""

# Function to test a specific limit
test_limit() {
    local limit=$1
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${YELLOW}Testing RUSH_PROFILE_BZL_LIMIT=${limit}${NC}"
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

    # Clean before each test
    "$RUSH" clean >/dev/null 2>&1 || true

    # Run with memory cap and profiling limit
    export RUSH_PROFILE_BZL_LIMIT=$limit
    export RUSH_SKIP_BZL_DEP_GRAPH=true
    export RUSH_ALLOW_MANY_BZL=1

    local output_file="$LOG_DIR/rush-profile-$limit.log"

    # Run with systemd cgroup memory limit
    if systemd-run --user --scope \
        --slice=rush-profile \
        --property=MemoryMax="$MEMORY_LIMIT" \
        --property=MemoryHigh=$((19 * 1024 * 1024 * 1024)) \
        --property=MemorySwapMax=0 \
        --unit=rush-profile-$$ \
        "$RUSH" build "$TARGET" > "$output_file" 2>&1; then
        echo -e "${GREEN}✓ SUCCESS: Loaded $limit .bzl files without OOM${NC}"

        # Show which files were loaded
        echo -e "\n${BLUE}Files loaded:${NC}"
        grep "\[PROFILE\] Files to load:" "$output_file" | head -3

        return 0  # Success
    else
        local exit_code=$?
        if [ $exit_code -eq 137 ]; then
            echo -e "${RED}✗ OOM KILLED: Limit $limit caused out-of-memory (exit 137)${NC}"

            # Show last loaded file
            echo -e "\n${BLUE}Last activity before OOM:${NC}"
            grep -E "\[PROFILE\]|\[DEBUG Evaluator\] Level.*Successfully loaded|Memory reserved" "$output_file" | tail -10
        else
            echo -e "${YELLOW}⚠ OTHER FAILURE: exit code $exit_code${NC}"
            tail -20 "$output_file"
        fi
        return 1  # Failure
    fi
}

# Phase 1: Binary search with powers of 2
echo -e "${GREEN}Phase 1: Binary search with powers of 2${NC}"
echo ""

last_good=0
first_bad=0

for power in 0 1 2 3 4 5 6 7 8 9 10; do
    limit=$((2**power))

    if test_limit $limit; then
        last_good=$limit
        echo -e "${GREEN}Last good: $last_good${NC}\n"
    else
        first_bad=$limit
        echo -e "${RED}First bad: $first_bad${NC}\n"
        break
    fi

    sleep 2
done

# Phase 2: Linear search if we found a range
if [ $first_bad -gt 0 ] && [ $last_good -gt 0 ] && [ $((first_bad - last_good)) -gt 1 ]; then
    echo -e "${GREEN}╔════════════════════════════════════════════════════════════════╗${NC}"
    echo -e "${GREEN}║ Phase 2: Linear search between $last_good and $first_bad${NC}"
    echo -e "${GREEN}╚════════════════════════════════════════════════════════════════╝${NC}"
    echo ""

    for limit in $(seq $((last_good + 1)) $((first_bad - 1))); do
        if test_limit $limit; then
            last_good=$limit
            echo -e "${GREEN}Last good: $last_good${NC}\n"
        else
            first_bad=$limit
            echo -e "${RED}First bad: $first_bad${NC}\n"
            break
        fi

        sleep 2
    done
fi

# Final report
echo -e "${GREEN}╔════════════════════════════════════════════════════════════════╗${NC}"
echo -e "${GREEN}║ FINAL RESULTS                                                  ║${NC}"
echo -e "${GREEN}╚════════════════════════════════════════════════════════════════╝${NC}"
echo ""

if [ $first_bad -gt 0 ]; then
    echo -e "${GREEN}✓ Last successful limit: $last_good files${NC}"
    echo -e "${RED}✗ First failing limit: $first_bad files${NC}"
    echo ""
    echo -e "${YELLOW}The problematic .bzl file is at position $first_bad${NC}"
    echo ""
    echo -e "${BLUE}To identify the exact file:${NC}"
    echo "  RUSH_PROFILE_BZL_LIMIT=$last_good $RUSH build $TARGET 2>&1 | grep '\\[PROFILE\\] Files to load'"
    echo "  RUSH_PROFILE_BZL_LIMIT=$first_bad $RUSH build $TARGET 2>&1 | grep '\\[PROFILE\\] Files to load'"
    echo ""
    echo -e "${BLUE}Compare the outputs to find which file is at position $first_bad${NC}"
else
    echo -e "${GREEN}✓ All tested limits succeeded (no OOM found within tested range)${NC}"
    echo -e "${YELLOW}You may need to test higher limits or the issue is elsewhere${NC}"
fi

echo ""
echo -e "${BLUE}Logs saved in: $LOG_DIR/rush-profile-*.log${NC}"
