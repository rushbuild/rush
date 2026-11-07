#!/usr/bin/env bash
# Rush — CI scheduler-gate profile: TSL proto pilot ladder
# (phase 5.C.4 commit 262; Batch BU)
#
# Wraps tools/ci/rush_scheduler_gate.sh with thresholds
# tuned for the cumulative TSL non-colliding proto_library
# pilot pool established across batches BL/BS/BT.
#
# This profile demonstrates the BQ-introduced thresholds
# end-to-end on a concrete pilot set:
#   - 9 non-colliding TSL proto_library targets verified
#     across status_proto, histogram_proto,
#     bfc_memory_map_proto, dnn_proto, test_log_proto,
#     rpc_options_proto, xplane_proto,
#     profiler_options_proto, trace_events_proto
#   - Plus the single colliding error_codes_proto_impl
#     case which is expected to quarantine under
#     OutputPathCollision.
#
# THRESHOLDS:
#   - RUSH_CI_MAX_BLOCKER_VERIFIER_MISMATCH=0      (strict; correctness)
#   - RUSH_CI_MAX_BLOCKER_OUTPUT_VALIDATION_FAILURE=0 (strict; correctness)
#   - RUSH_CI_MAX_FAILED_CANDIDATES=0              (strict; correctness)
#   - RUSH_CI_MAX_BLOCKER_OUTPUT_PATH_COLLISIONS=1 (BD-1 case allowed)
#   - RUSH_CI_MAX_BLOCKER_MISSING_VENDORED_DEP=    (unbounded;
#     proto pilots don't transit ml_dtypes, but we leave
#     this open so the profile doesn't fail if a closure
#     change introduces an Eigen-blocked dep)
#   - RUSH_CI_MIN_SUCCESSFUL_ACTION_BEARING_PILOTS=9 (assert progress)
#
# USAGE:
#   tools/ci/profiles/tsl_proto_pilot_ladder.sh \
#     <dry-run-report.json> [rush-binary]
#
# The dry-run-report.json is the output of the candidate
# scanner's --legacy-deletion-dry-run-json mode run
# against the target pattern below. The scanner is run
# upstream; this profile only ASSERTS thresholds.
#
# EXIT CODES (inherited from rush_scheduler_gate.sh):
#   0 — all thresholds satisfied
#   1 — at least one threshold exceeded
#   2 — environment problem
#
# WHAT WOULD FAIL THE GATE:
#   - A new verifier_mismatch fires on ANY proto pilot
#     (regression in proto-rule analyze).
#   - failed_candidates > 0 (a previously-passing pilot
#     now fails at runtime).
#   - output_path_collisions > 1 (a SECOND BD-1-class
#     collision appears — the count budget is exactly 1
#     for the known error_codes_proto_impl case).
#   - successful_pilots < 9 (the cumulative pool
#     established in BL+BS+BT has regressed).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GATE_SCRIPT="$SCRIPT_DIR/../rush_scheduler_gate.sh"

if [[ $# -lt 1 || $# -gt 2 ]]; then
    cat >&2 <<USAGE
usage: $0 <dry-run-report.json> [rush-binary]

This profile asserts thresholds against an EXISTING
dry-run report JSON produced by the candidate scanner.
It does not drive the scan itself.

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

# The proto pilot target pattern (informational; the
# actual probe set is fed via the dry-run JSON).
TARGET_PATTERN="//examples/hello_cpp:hello"

echo "→ Running TSL proto pilot ladder profile" >&2
echo "  dry-run report: $DRY_RUN_JSON" >&2
echo "  thresholds: see top of $(basename "${BASH_SOURCE[0]}")" >&2

RUSH_CI_DRY_RUN_REPORT_JSON="$DRY_RUN_JSON" \
RUSH_CI_MAX_UNSUPPORTED_SHAPES=0 \
RUSH_CI_MAX_MISSING_TOOLS=0 \
RUSH_CI_MAX_ACTION_GRAPH_INCOMPLETE=0 \
RUSH_CI_MAX_LEGACY_REQUIRED_TARGETS=0 \
RUSH_CI_MAX_EXTERNAL_SOURCE_MISSING=0 \
RUSH_CI_MAX_BLOCKER_VERIFIER_MISMATCH=0 \
RUSH_CI_MAX_BLOCKER_OUTPUT_VALIDATION_FAILURE=0 \
RUSH_CI_MAX_FAILED_CANDIDATES=0 \
RUSH_CI_MAX_OUTPUT_PATH_COLLISIONS=1 \
RUSH_CI_MIN_SUCCESSFUL_ACTION_BEARING_PILOTS=9 \
RUSH_CI_ALLOW_NON_REMOTE_CACHEABLE=1 \
    exec "$GATE_SCRIPT" --dry-run "$TARGET_PATTERN" "$RUSH_BIN"
