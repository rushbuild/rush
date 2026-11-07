#!/bin/bash
# Check multi-AI review consensus for a commit
#
# Usage: ./scripts/check-consensus.sh <commit-sha>
#
# Exits 0 if consensus reached (ready to merge)
# Exits 1 if changes needed
# Exits 2 if rejected

set -euo pipefail

COMMIT_SHA="${1:-HEAD}"
TRACES_DIR=".ai-traces"

# Find all review files for this commit
REVIEW_FILES=$(find "$TRACES_DIR" -name "*-review-*.md" -type f 2>/dev/null || true)

if [ -z "$REVIEW_FILES" ]; then
    echo "❌ No review files found in $TRACES_DIR/"
    echo "Expected files: *-review-gemini.md, *-review-claude.md"
    exit 1
fi

# Count verdicts
APPROVE_COUNT=0
REQUEST_CHANGES_COUNT=0
REJECT_COUNT=0
TOTAL_REVIEWS=0

echo "Checking consensus for commit $COMMIT_SHA"
echo "=============================================="
echo

for review_file in $REVIEW_FILES; do
    # Extract reviewer name from filename
    REVIEWER=$(basename "$review_file" | sed -n 's/.*-review-\(.*\)\.md/\1/p')

    # Check if file mentions this commit
    if ! grep -q "$COMMIT_SHA" "$review_file" 2>/dev/null; then
        continue
    fi

    TOTAL_REVIEWS=$((TOTAL_REVIEWS + 1))

    # Extract verdict
    if grep -q "^\*\*APPROVE\*\*" "$review_file" || grep -q "^APPROVE" "$review_file"; then
        VERDICT="✅ APPROVE"
        APPROVE_COUNT=$((APPROVE_COUNT + 1))
    elif grep -q "^\*\*REQUEST_CHANGES\*\*" "$review_file" || grep -q "^REQUEST_CHANGES" "$review_file"; then
        VERDICT="⚠️  REQUEST_CHANGES"
        REQUEST_CHANGES_COUNT=$((REQUEST_CHANGES_COUNT + 1))
    elif grep -q "^\*\*REJECT\*\*" "$review_file" || grep -q "^REJECT" "$review_file"; then
        VERDICT="❌ REJECT"
        REJECT_COUNT=$((REJECT_COUNT + 1))
    else
        VERDICT="❓ UNKNOWN"
    fi

    echo "$REVIEWER: $VERDICT"
done

echo
echo "Summary:"
echo "--------"
echo "Total reviews: $TOTAL_REVIEWS"
echo "Approve: $APPROVE_COUNT"
echo "Request changes: $REQUEST_CHANGES_COUNT"
echo "Reject: $REJECT_COUNT"
echo

# Determine criticality (default to critical if not specified)
CRITICAL="${CRITICAL:-true}"

if [ "$REJECT_COUNT" -gt 0 ]; then
    echo "❌ REJECTED - At least one AI rejected the changes"
    exit 2
fi

if [ "$CRITICAL" = "true" ]; then
    # Critical changes need unanimous approval (all must approve)
    if [ "$APPROVE_COUNT" -eq "$TOTAL_REVIEWS" ] && [ "$TOTAL_REVIEWS" -ge 2 ]; then
        echo "✅ READY_TO_MERGE - Unanimous approval for critical change"
        exit 0
    else
        echo "⚠️  NEEDS_CHANGES - Critical changes require unanimous approval"
        exit 1
    fi
else
    # Non-critical changes need 2/3 approval
    if [ "$APPROVE_COUNT" -ge 2 ]; then
        echo "✅ READY_TO_MERGE - 2/3 approval threshold met"
        exit 0
    else
        echo "⚠️  NEEDS_CHANGES - Need at least 2 approvals"
        exit 1
    fi
fi
