#!/usr/bin/env bash
# Rush — CI scheduler-gate profile: Lane C (Eigen
# materialization) unblock check.
#
# Phase 5.C.4 commit 269 (Lane C step C5). Asserts that
# after operators have run scripts/bootstrap_ml_dtypes_eigen.sh
# --apply, the previously-quarantined ~22 Eigen Class 2
# candidates are no longer blocked by
# MissingVendoredDependency(ml_dtypes_eigen).
#
# Pro 2026-05-26 C5 spec thresholds:
#   RUSH_CI_MAX_MISSING_VENDORED_DEPENDENCIES = 0
#   verifier_mismatch                          = 0  (strict)
#   output_validation_failure                  = 0  (strict)
#   failed_candidates                          = 0  (strict)
#
# USAGE:
#   tools/ci/profiles/ml_dtypes_eigen_unblocked.sh \
#     <dry-run-report.json> [rush-binary]
#
# The dry-run report should be produced AFTER running
# the Eigen-quarantined candidates with the bootstrap
# applied. A passing run means the materialization
# successfully unblocked those candidates.
#
# EXIT CODES (inherited from rush_scheduler_gate.sh):
#   0 — Eigen unblock confirmed
#   1 — threshold violation (still blocked or correctness
#       regression)
#   2 — environment problem

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GATE_SCRIPT="$SCRIPT_DIR/../rush_scheduler_gate.sh"

if [[ $# -lt 1 || $# -gt 2 ]]; then
    cat >&2 <<USAGE
usage: $0 <dry-run-report.json> [rush-binary]

C5 profile: Eigen materialization unblock check.
Asserts that after bootstrap_ml_dtypes_eigen.sh --apply,
the ~22 Eigen-quarantined candidates from BO audit are
no longer blocked.

example:
    scripts/bootstrap_ml_dtypes_eigen.sh --apply
    # ... run quarantined candidates, produce dry-run JSON
    $0 /tmp/post-bootstrap-dry-run.json target/release/rush
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

echo "→ Running Lane C Eigen-unblock profile" >&2
echo "  dry-run report: $DRY_RUN_JSON" >&2

RUSH_CI_DRY_RUN_REPORT_JSON="$DRY_RUN_JSON" \
RUSH_CI_MAX_UNSUPPORTED_SHAPES=100 \
RUSH_CI_MAX_MISSING_TOOLS=0 \
RUSH_CI_MAX_ACTION_GRAPH_INCOMPLETE=0 \
RUSH_CI_MAX_LEGACY_REQUIRED_TARGETS=100 \
RUSH_CI_MAX_BLOCKER_VERIFIER_MISMATCH=0 \
RUSH_CI_MAX_BLOCKER_OUTPUT_VALIDATION_FAILURE=0 \
RUSH_CI_MAX_FAILED_CANDIDATES=0 \
RUSH_CI_MAX_OUTPUT_PATH_COLLISIONS=1 \
RUSH_CI_MAX_MISSING_VENDORED_DEPENDENCIES=0 \
RUSH_CI_ALLOW_NON_REMOTE_CACHEABLE=1 \
    exec "$GATE_SCRIPT" --dry-run "$TARGET_PATTERN" "$RUSH_BIN"
