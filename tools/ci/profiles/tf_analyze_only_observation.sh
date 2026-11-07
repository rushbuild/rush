#!/usr/bin/env bash
# Rush — CI scheduler-gate profile: TF analyze-only observation
# (phase 5.C.4 Batch AF commit 221).
#
# Wraps tools/ci/rush_scheduler_gate.sh with thresholds suited
# to OBSERVATION runs over TF targets — the dry-run / readiness-
# report mode, NOT a pilot gate.
#
# This profile exists because Pro's AB-revised directive
# explicitly contemplated two modes for the
# ExternalRepoSourceMissing threshold:
#
#   - Scheduler pilots (tf_macros_pilot.sh):
#     external_source_missing = 0 (strict)
#   - Analyze-only / dry-run reports (this profile):
#     external_source_missing = unlimited (observe-only)
#
# The first mode catches a regression — a pilot crossing
# unmaterialized external repos must FAIL the gate. The
# second mode is for enumerating the materialization gap
# without failing on it: the operator running the dry-run
# wants the count, not a rejection.
#
# USAGE:
#   tools/ci/profiles/tf_analyze_only_observation.sh [--dry-run] <target> [rush-binary]
#
# EXAMPLES:
#   tools/ci/profiles/tf_analyze_only_observation.sh --dry-run //tensorflow/core/platform:cpu_feature_guard
#   tools/ci/profiles/tf_analyze_only_observation.sh --dry-run //tensorflow/core/platform:tensor_coding target/release/rush
#
# EXIT CODES (inherited from rush_scheduler_gate.sh):
#   0 — observation succeeded (counts printed; no strict
#       gating fired)
#   1 — at least one threshold exceeded (rare — the
#       observation profile keeps most thresholds unset;
#       a fail here indicates a stricter knob the user
#       set in their shell)
#   2 — environment problem
#
# WHAT THIS PROFILE PINS:
#   - unsupported_shapes      = 0  (these are still real
#                                   correctness gaps,
#                                   should never grow in
#                                   analyze-only either)
#   - missing_tools           = 0  (same reasoning)
#   - action_graph_incomplete = 0  (same reasoning)
#   - legacy_required         = unlimited (observation
#                                   accepts arbitrary
#                                   legacy debt — that's
#                                   the data being
#                                   observed)
#   - external_source_missing = unlimited (the whole
#                                   point of observation
#                                   runs is to enumerate
#                                   this number)
#   - non_remote_cacheable    = allowed (default-on of
#                                   the underlying gate)
#
# WHAT THIS PROFILE DOES NOT DO:
#   - It does not run any actions; observation is
#     analyze-only via the underlying script's --dry-run
#     flag.
#   - It does not modify external repos.
#   - It does not pre-set RUSH_5C5_DRY_RUN=1; that
#     environment variable wires the rust-side dry-run
#     report emission (AD-revised). Operators wanting the
#     full aggregate report should set it explicitly
#     alongside this profile.
#
# RELATED:
#   - tools/ci/profiles/tf_macros_pilot.sh
#     (strict pilot gate; counterpart to this profile)
#   - docs/item-5-5c5-pilot-pivot-decision.md
#     (ABR-5 reasoning for why observation runs matter
#     while the action-bearing pilot ladder is paused)

set -euo pipefail

# Default rush binary location relative to the repo root.
DEFAULT_RUSH_BIN="target/release/rush"

# Resolve the repo root so the profile is invocable from
# anywhere within the workspace.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"

# Thresholds for observation runs.
export RUSH_CI_MAX_UNSUPPORTED_SHAPES=0
export RUSH_CI_MAX_MISSING_TOOLS=0
export RUSH_CI_MAX_ACTION_GRAPH_INCOMPLETE=0
# legacy_required and external_source_missing intentionally
# left unset — the observation profile counts them; it
# doesn't gate on them.

# Forward all args to the underlying gate. The user passes
# the target pattern and (optionally) the rush binary path.
cd "${REPO_ROOT}/tensorflow"
exec "${REPO_ROOT}/tools/ci/rush_scheduler_gate.sh" "$@" \
    "${REPO_ROOT}/${DEFAULT_RUSH_BIN}"
