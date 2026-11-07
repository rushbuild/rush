#!/usr/bin/env bash
# Rush — CI scheduler-gate profile: action-bearing pilot
# ladder (Phase 5.C.4 commit 283, Lane B step B4).
#
# Asserts the full action-bearing pilot ladder's strict
# correctness invariants. This is the broadest 5.C.5a
# gate: it covers cc_binary link, cc_library
# compile+archive, cc_test link, and proto_library
# compile shapes that have live authoritative-mode
# execution evidence (the ~28-pilot ladder).
#
# Pro 2026-05-28 B4 spec thresholds:
#   verifier mismatch                = 0
#   output validation failure        = 0
#   failed candidate                 = 0
#   authoritative legacy violation   = 0
#   missing generated producer       = 0
#   output path collision            = configurable (default 1; BD-1)
#   missing vendored dependency      = configurable (default 0;
#                                      assumes bootstrap applied)
#
# USAGE:
#   tools/ci/profiles/action_bearing_pilot_ladder.sh \
#     <dry-run-report.json> [rush-binary]
#
# The dry-run report should be produced from a run with
# RUSH_GLOBAL_CLONE_CACHE pointed at the bootstrapped
# Eigen/ml_dtypes clones, so the Eigen blocker is absent.
#
# EXIT CODES (inherited from rush_scheduler_gate.sh):
#   0 — all thresholds satisfied
#   1 — threshold violation
#   2 — environment problem

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GATE_SCRIPT="$SCRIPT_DIR/../rush_scheduler_gate.sh"

if [[ $# -lt 1 || $# -gt 2 ]]; then
    cat >&2 <<USAGE
usage: $0 <dry-run-report.json> [rush-binary]

B4 profile: action-bearing pilot ladder. Asserts strict
correctness invariants across the full ~28-pilot ladder.

example:
    $0 /tmp/ladder-dry-run.json target/release/rush
USAGE
    exit 2
fi

DRY_RUN_JSON="$1"
RUSH_BIN="${2:-rush}"

if [[ ! -f "$DRY_RUN_JSON" ]]; then
    echo "error: dry-run report not found at: $DRY_RUN_JSON" >&2
    exit 2
fi

TARGET_PATTERN="//examples/hello_cpp:hello"

echo "→ Running action-bearing pilot ladder profile" >&2
echo "  dry-run report: $DRY_RUN_JSON" >&2

RUSH_CI_DRY_RUN_REPORT_JSON="$DRY_RUN_JSON" \
RUSH_CI_MAX_UNSUPPORTED_SHAPES=0 \
RUSH_CI_MAX_MISSING_TOOLS=0 \
RUSH_CI_MAX_ACTION_GRAPH_INCOMPLETE=0 \
RUSH_CI_MAX_BLOCKER_VERIFIER_MISMATCH=0 \
RUSH_CI_MAX_BLOCKER_OUTPUT_VALIDATION_FAILURE=0 \
RUSH_CI_MAX_FAILED_CANDIDATES=0 \
RUSH_CI_MAX_OUTPUT_PATH_COLLISIONS="${RUSH_CI_MAX_OUTPUT_PATH_COLLISIONS:-1}" \
RUSH_CI_MAX_MISSING_VENDORED_DEPENDENCIES="${RUSH_CI_MAX_MISSING_VENDORED_DEPENDENCIES:-0}" \
RUSH_CI_ALLOW_NON_REMOTE_CACHEABLE=1 \
    exec "$GATE_SCRIPT" --dry-run "$TARGET_PATTERN" "$RUSH_BIN"
