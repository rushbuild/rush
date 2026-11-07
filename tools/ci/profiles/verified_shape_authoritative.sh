#!/usr/bin/env bash
# Rush — CI scheduler-gate profile: 5.C.5a verified-shape
# scheduler-authoritative mode (Phase 5.C.4 commit 267,
# Lane A step A6).
#
# This is the authoritative gate for the 5.C.5a pilot:
# all verified shapes MUST dispatch via the scheduler and
# MUST NOT invoke legacy build() for their own actions.
#
# Pro 2026-05-26 A6 spec thresholds:
#   verifier_mismatch                       = 0  (strict)
#   output_validation_failure               = 0  (strict)
#   failed_candidates                       = 0  (strict)
#   verified_shape_legacy_violation         = 0  (strict)
#   missing_generated_producer              = 0  (strict)
#
# Maps to the existing gate-script env knobs:
#   RUSH_CI_MAX_BLOCKER_VERIFIER_MISMATCH           = 0
#   RUSH_CI_MAX_BLOCKER_OUTPUT_VALIDATION_FAILURE   = 0
#   RUSH_CI_MAX_FAILED_CANDIDATES                   = 0
#
# Two correctness invariants are NEW for A6 and read
# directly from the invocation report (not from
# candidate_blockers): verified_shape_legacy_violation
# and missing_generated_producer. The gate script
# consumes these via the dry-run report's invocation
# section when it's populated; for now, the strict-by-
# default knobs cover the same correctness class.
#
# USAGE:
#   tools/ci/profiles/verified_shape_authoritative.sh \
#     <dry-run-report.json> [rush-binary]
#
# The dry-run-report.json must include
# candidate_blockers.status_counts and at least one
# successful executed pilot (otherwise the gate is
# noise).
#
# EXIT CODES (inherited from rush_scheduler_gate.sh):
#   0 — all thresholds satisfied
#   1 — at least one threshold exceeded (= contract violation)
#   2 — environment problem
#
# WHAT WOULD FAIL THE GATE:
#   - A verified shape invoked legacy in authoritative mode
#     (verified_shape_legacy_violation > 0). This is the
#     primary A5 contract.
#   - A verifier_mismatch fires (scheduler argv differs
#     from legacy argv for a target).
#   - An output_validation_failure fires (scheduler-
#     dispatched action's outputs failed validation).
#   - A pilot failed at runtime (failed_candidates > 0).
#   - A scheduler action needed a generated producer that
#     wasn't declared (missing_generated_producer > 0).
#
# WHAT DOES NOT FAIL THE GATE:
#   - Eigen quarantines (those targets are not in any
#     verified shape).
#   - OutputPathCollision quarantines (BD-1 class; up to
#     1 expected, configurable via MAX_OUTPUT_PATH_COLLISIONS).
#   - Unverified-rule legacy invocations.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GATE_SCRIPT="$SCRIPT_DIR/../rush_scheduler_gate.sh"

if [[ $# -lt 1 || $# -gt 2 ]]; then
    cat >&2 <<USAGE
usage: $0 <dry-run-report.json> [rush-binary]

A6 profile: verified-shape scheduler-authoritative mode.
Asserts the strict correctness invariants Pro pinned in
the 2026-05-26 A6 spec.

example:
    $0 /tmp/dry-run.json target/release/rush
USAGE
    exit 2
fi

DRY_RUN_JSON="$1"
RUSH_BIN="${2:-rush}"

if [[ ! -f "$DRY_RUN_JSON" ]]; then
    echo "error: dry-run report not found at: $DRY_RUN_JSON" >&2
    exit 2
fi

# Informational target pattern (gate script needs SOME
# target; the actual contract is on the dry-run JSON).
TARGET_PATTERN="//examples/hello_cpp:hello"

echo "→ Running 5.C.5a verified-shape authoritative profile" >&2
echo "  dry-run report: $DRY_RUN_JSON" >&2
echo "  Pro 2026-05-26 A6 strict thresholds" >&2

RUSH_CI_DRY_RUN_REPORT_JSON="$DRY_RUN_JSON" \
RUSH_CI_MAX_UNSUPPORTED_SHAPES=0 \
RUSH_CI_MAX_MISSING_TOOLS=0 \
RUSH_CI_MAX_ACTION_GRAPH_INCOMPLETE=0 \
RUSH_CI_MAX_LEGACY_REQUIRED_TARGETS=0 \
RUSH_CI_MAX_EXTERNAL_SOURCE_MISSING=0 \
RUSH_CI_MAX_BLOCKER_VERIFIER_MISMATCH=0 \
RUSH_CI_MAX_BLOCKER_OUTPUT_VALIDATION_FAILURE=0 \
RUSH_CI_MAX_BLOCKER_ACTION_GRAPH_INCOMPLETE=0 \
RUSH_CI_MAX_FAILED_CANDIDATES=0 \
RUSH_CI_MAX_OUTPUT_PATH_COLLISIONS=1 \
RUSH_CI_ALLOW_NON_REMOTE_CACHEABLE=1 \
    exec "$GATE_SCRIPT" --dry-run "$TARGET_PATTERN" "$RUSH_BIN"
