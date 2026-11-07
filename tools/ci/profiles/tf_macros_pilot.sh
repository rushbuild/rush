#!/usr/bin/env bash
# Rush — CI scheduler-gate profile: 5.C.5 first-target pilot
# (phase 5.C.4 commit 209; Batch AA).
#
# Wraps tools/ci/rush_scheduler_gate.sh with the threshold
# values Pro pinned for //tensorflow/core/platform:macros.
# This is the pilot's go/no-go gate; failing one of these
# thresholds means the pilot has regressed and needs
# investigation BEFORE expanding the target subset.
#
# Pro's commit-209 thresholds:
#   unsupported_shapes      = 0
#   missing_tools           = 0
#   action_graph_incomplete = 0
#   legacy_required         = allowed only if documented
#   non_remote_cacheable    = allowed for now
#
# Phase 5.C.4 Batch AF (commit 221) added:
#   external_source_missing = 0
#     (pilots must NOT have unmaterialized external
#     repos in their transitive closure; cf. AB-2
#     hard stop on @snappy)
#
# USAGE:
#   tools/ci/profiles/tf_macros_pilot.sh [--dry-run] [rush-binary]
#
# EXAMPLES:
#   tools/ci/profiles/tf_macros_pilot.sh --dry-run
#   tools/ci/profiles/tf_macros_pilot.sh --dry-run target/release/rush
#
# EXIT CODES (inherited from rush_scheduler_gate.sh):
#   0 — all thresholds satisfied
#   1 — at least one threshold exceeded
#   2 — environment problem
#
# WHAT WOULD FAIL THE GATE:
#   - A new UnsupportedShape gate fires for the chosen
#     target (would mean a new shape entered scope without
#     verifier-zero-diff coverage).
#   - A new MissingTool fires (would mean the host lost a
#     tool the target needs — protoc, g++, ar, ld, bazel —
#     since the pilot succeeded last).
#   - ActionGraphIncomplete > 0 (would mean a target in
#     scope is BOTH allowlisted AND has incomplete actions,
#     which contradicts the commit 206 NoDeclaredActions
#     classification).
#   - Legacy-required count > 0 without documentation in
#     docs/item-5-5c5-first-target-plan.md.
#
# WHY non_remote_cacheable IS STILL ALLOWED:
#   The pilot target is header-only — there are NO actions
#   to cache. The non-remote-cacheable count is irrelevant
#   for this target. Once expansion adds action-bearing
#   targets, the next-target candidate report (commit 210)
#   will revisit whether this allowance should tighten.
#
# WHY ZERO-ACTION TARGETS ARE NOT REPORTED AS LEGACY-REQUIRED:
#   Commit 206 added the NoDeclaredActions eligibility
#   variant. Zero-action allowlisted targets are now
#   classified as scheduler-dispatched (with action_count=0),
#   NOT as ActionGraphIncomplete legacy fallbacks. The pilot
#   target's own actions don't appear in the legacy_required
#   bucket; only the synthesized source-header nodes
#   (system_cc_library, RuleKindUnverified) do, and those
#   are off-allowlist by design.
#
# IF THIS GATE FAILS:
#   Per Pro's "If the pilot fails" stop rules:
#     - no-action handling failure → fix only no-action
#       scheduler handling.
#     - output validation failure → fix only the
#       zero-action / scheduler-ready policy path.
#     - unsupported shape → improve gating, do NOT
#       migrate grpc/gentbl.
#     - missing tool → document and stop.
#     - reporting/warmup failure → fix reporting only.
#     - DO NOT broaden target scope.
#     - DO NOT jump to platform_base.
#     - DO NOT start full 5.C.5 deletion.

set -euo pipefail

# Default rush binary location relative to the repo root.
DEFAULT_RUSH_BIN="target/release/rush"

# Resolve the repo root so the profile is invocable from
# anywhere within the workspace.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"

# The pilot target is fixed; this profile is specific to
# //tensorflow/core/platform:macros. Pro's directive: one
# target per profile, no fan-out.
PILOT_TARGET="//tensorflow/core/platform:macros"

# Thresholds for the pilot:
#   - unsupported_shapes / missing_tools / action_graph_incomplete = 0
#   - legacy_required allowed only if documented (we
#     allow 2 for the synthesized headers; widening
#     requires a doc commit alongside the gate change)
#   - non_remote_cacheable allowed (unset = unlimited)
export RUSH_CI_MAX_UNSUPPORTED_SHAPES=0
export RUSH_CI_MAX_MISSING_TOOLS=0
export RUSH_CI_MAX_ACTION_GRAPH_INCOMPLETE=0
export RUSH_CI_MAX_LEGACY_REQUIRED_TARGETS="${RUSH_CI_MAX_LEGACY_REQUIRED_TARGETS:-2}"
# Phase 5.C.4 Batch AF (commit 221): the
# ExternalRepoSourceMissing class (ABR-2 commit 215)
# must be ZERO for any scheduler pilot. Missing external
# repo sources mean the verifier cannot prove dispatch
# correctness — a non-zero bucket here is the same
# correctness gap that fired in Batch AB-2. The macros
# pilot's transitive closure is header-only (no .cc
# sources from any external repo) so the expected count
# is 0; the threshold pins that invariant.
export RUSH_CI_MAX_EXTERNAL_SOURCE_MISSING=0
# non_remote_cacheable allowance is the default-on of
# the underlying gate; no override needed here.

# Forward all args + the pilot target to the underlying gate.
# The user can pass `--dry-run` and/or a custom rush binary path.
cd "${REPO_ROOT}/tensorflow"
exec "${REPO_ROOT}/tools/ci/rush_scheduler_gate.sh" "$@" \
    "${PILOT_TARGET}" \
    "${REPO_ROOT}/${DEFAULT_RUSH_BIN}"
