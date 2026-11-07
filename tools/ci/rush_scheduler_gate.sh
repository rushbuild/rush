#!/usr/bin/env bash
# Rush — CI scheduler gate (phase 5.C.4 commit 76; --dry-run
# added in commit 78)
#
# Operator-facing sample script: runs rush's scheduler
# preflight + legacy breakdown + (optionally) action-key
# explain against a target pattern, then applies
# thresholds from env vars.
#
# Per Pro's commit-76 directive:
#   - Sample/operator script, NOT enforced project CI policy
#   - Uses report-specific CLI flags (commit 71), not env-var
#     folklore alone
#   - Exits non-zero when any threshold is exceeded
#   - Prints jq install hint when jq is missing
#
# USAGE:
#   tools/ci/rush_scheduler_gate.sh [--dry-run] <target-pattern> [rush-binary]
#
# EXAMPLES:
#   tools/ci/rush_scheduler_gate.sh //... target/release/rush
#   tools/ci/rush_scheduler_gate.sh --dry-run //pkg:lib
#
#   RUSH_CI_MAX_UNSUPPORTED_SHAPES=0 \
#   RUSH_CI_MAX_MISSING_TOOLS=0 \
#   tools/ci/rush_scheduler_gate.sh //... target/release/rush
#
# THRESHOLDS (override via env var):
#   RUSH_CI_MAX_UNSUPPORTED_SHAPES        default: unlimited
#   RUSH_CI_MAX_MISSING_TOOLS             default: unlimited
#   RUSH_CI_MAX_ACTION_GRAPH_INCOMPLETE   default: unlimited
#   RUSH_CI_MAX_LEGACY_REQUIRED_TARGETS   default: unlimited
#   RUSH_CI_MAX_EXTERNAL_SOURCE_MISSING   default: unlimited (Batch AE)
#   RUSH_CI_ALLOW_NON_REMOTE_CACHEABLE    default: 1 (allow)
#
# CANDIDATE-BLOCKER TAXONOMY THRESHOLDS (Phase 5.C.4 Batch BI):
# These thresholds gate on the candidate scanner's blocker
# taxonomy as surfaced in the 5.C.5 dry-run report JSON. They
# only fire when the dry-run report (--legacy-deletion-dry-run-json
# or RUSH_5C5_DRY_RUN_JSON=1) is available AND its
# candidate_blockers section is populated by an upstream candidate
# scan. When the section is empty (current live build path
# default), the thresholds are recorded as 0 and pass trivially.
#
#   RUSH_CI_DRY_RUN_REPORT_JSON     path to dry-run JSON file
#                                   (when set, taxonomy gates run)
#   RUSH_CI_MAX_BLOCKER_MISSING_VENDORED_DEP    default: unlimited
#   RUSH_CI_MAX_BLOCKER_MISSING_HOST_PACKAGE    default: unlimited
#   RUSH_CI_MAX_BLOCKER_MISSING_TOOL            default: unlimited
#   RUSH_CI_MAX_BLOCKER_UNSUPPORTED_SHAPE       default: unlimited
#   RUSH_CI_MAX_BLOCKER_EXTERNAL_SOURCE_MISSING default: unlimited
#   RUSH_CI_MAX_BLOCKER_VERIFIER_MISMATCH       default: 0 (strict)
#   RUSH_CI_MAX_BLOCKER_OUTPUT_VALIDATION_FAILURE default: 0 (strict)
#   RUSH_CI_MAX_BLOCKER_ACTION_GRAPH_INCOMPLETE default: unlimited
#   RUSH_CI_MAX_BLOCKER_TOO_BROAD               default: unlimited
#   RUSH_CI_MAX_QUARANTINED_CANDIDATES          default: unlimited
#   RUSH_CI_MAX_FAILED_CANDIDATES               default: 0 (strict)
#
# CANDIDATE-BLOCKER TAXONOMY THRESHOLDS (Phase 5.C.4 Batch BQ):
#   RUSH_CI_MAX_OUTPUT_PATH_COLLISIONS          default: unlimited
#     New blocker kind for BD-1-class duplicate output
#     paths. See docs/analysis/output-path-collision-audit.md.
#   RUSH_CI_MAX_NON_STUB_MISSING_SOURCES        default: 0 (strict)
#     Reuses preflight's external_repo_source_missing
#     bucket but enforces a strict-by-default contract:
#     non-stub missing sources are never OK in CI. Stub
#     sources are admitted via the AJ2 ExternalSystemStub
#     pathway and excluded from this count.
#   RUSH_CI_MIN_SUCCESSFUL_ACTION_BEARING_PILOTS default: unset
#     When set, the gate FAILS if status_counts.executed
#     is BELOW the threshold. Inverse direction from the
#     other thresholds. Used by pilot-ladder profiles to
#     assert progress.
#   RUSH_CI_MAX_MISSING_VENDORED_DEPENDENCIES default: unlimited
#     Lane C alias for RUSH_CI_MAX_BLOCKER_MISSING_VENDORED_DEP,
#     named for the Eigen materialization workflow. When
#     both are set, the more-specific
#     MAX_BLOCKER_MISSING_VENDORED_DEP wins.
#
# --dry-run:
#   Use `--analyze-only --use-scheduler --scheduler-preflight=json`
#   so the gate runs against the SAME preflight surface a real
#   build would produce, but WITHOUT executing actions.
#   Operators iterating on threshold values pay only the analyze
#   pass cost. Phase 5.C.4 commit 78.
#
# EXIT CODES:
#   0 — all thresholds satisfied
#   1 — at least one threshold exceeded
#   2 — environment problem (jq missing, rush binary not
#       found, invalid args, etc.)

set -euo pipefail

# ---------------------------------------------------------
# Arg parsing
# ---------------------------------------------------------

DRY_RUN=0
POSITIONAL=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        -h|--help)
            sed -n '2,40p' "$0"
            exit 0
            ;;
        --)
            shift
            while [[ $# -gt 0 ]]; do
                POSITIONAL+=("$1")
                shift
            done
            break
            ;;
        -*)
            echo "error: unknown flag: $1" >&2
            exit 2
            ;;
        *)
            POSITIONAL+=("$1")
            shift
            ;;
    esac
done

if [[ ${#POSITIONAL[@]} -lt 1 || ${#POSITIONAL[@]} -gt 2 ]]; then
    cat >&2 <<USAGE
usage: $0 [--dry-run] <target-pattern> [rush-binary]

examples:
    $0 //... target/release/rush
    $0 //pkg:lib
    $0 --dry-run //... target/release/rush

env-var thresholds (default: unlimited unless noted):
    RUSH_CI_MAX_UNSUPPORTED_SHAPES
    RUSH_CI_MAX_MISSING_TOOLS
    RUSH_CI_MAX_ACTION_GRAPH_INCOMPLETE
    RUSH_CI_MAX_LEGACY_REQUIRED_TARGETS
    RUSH_CI_MAX_EXTERNAL_SOURCE_MISSING  (Phase 5.C.4 Batch AE)
    RUSH_CI_ALLOW_NON_REMOTE_CACHEABLE   (default: 1)
USAGE
    exit 2
fi

TARGET="${POSITIONAL[0]}"
RUSH_BIN="${POSITIONAL[1]:-rush}"

# ---------------------------------------------------------
# Prerequisites
# ---------------------------------------------------------

if ! command -v jq >/dev/null 2>&1; then
    cat >&2 <<JQHINT
error: jq not found on PATH

This script needs jq to parse rush's JSON report output.

install:
    debian/ubuntu: sudo apt install jq
    rhel/fedora:   sudo dnf install jq
    macos:         brew install jq
    nix:           nix-shell -p jq

JQHINT
    exit 2
fi

if ! command -v "$RUSH_BIN" >/dev/null 2>&1; then
    if [[ ! -x "$RUSH_BIN" ]]; then
        echo "error: rush binary not found at: $RUSH_BIN" >&2
        echo "       build first: cargo build --release" >&2
        exit 2
    fi
fi

# ---------------------------------------------------------
# Thresholds (unset → unlimited)
# ---------------------------------------------------------

MAX_UNSUPPORTED_SHAPES="${RUSH_CI_MAX_UNSUPPORTED_SHAPES:-}"
MAX_MISSING_TOOLS="${RUSH_CI_MAX_MISSING_TOOLS:-}"
MAX_AG_INCOMPLETE="${RUSH_CI_MAX_ACTION_GRAPH_INCOMPLETE:-}"
MAX_LEGACY_REQUIRED="${RUSH_CI_MAX_LEGACY_REQUIRED_TARGETS:-}"
# Phase 5.C.4 Batch AE (commit 220): threshold knob for the
# ExternalRepoSourceMissing bucket added in ABR-2. Unset →
# no threshold check (the count is still parsed and printed
# for diagnostic visibility). When set, the gate fails if
# the total count across all (rule_kind, reason) entries in
# the external_repo_source_missing bucket exceeds the limit.
MAX_EXTERNAL_SOURCE_MISSING="${RUSH_CI_MAX_EXTERNAL_SOURCE_MISSING:-}"
ALLOW_NON_REMOTE_CACHEABLE="${RUSH_CI_ALLOW_NON_REMOTE_CACHEABLE:-1}"
# Phase 5.C.4 commit 79: threshold knob for
# non-remote-cacheable action counts. Replaces / extends
# the older boolean ALLOW knob. Resolution order:
#   1. If MAX_NON_REMOTE_CACHEABLE_ACTIONS is set: use it.
#   2. Else if ALLOW=0: equivalent to MAX=0 (strict).
#   3. Else: unlimited (no check).
# CLI flag wins over env (CLI flag for this knob lands in
# a future commit if needed; today the env-only path is
# enough).
MAX_NON_REMOTE_CACHEABLE="${RUSH_CI_MAX_NON_REMOTE_CACHEABLE_ACTIONS:-}"

# Phase 5.C.4 Batch BI: candidate-blocker taxonomy thresholds.
# These gate against the dry-run report's candidate_blockers
# section (Batch BH). They only fire when an upstream candidate
# scan has populated CandidateReportEntry data and the dry-run
# JSON is supplied via RUSH_CI_DRY_RUN_REPORT_JSON.
#
# Defaults: most are unlimited (operator opts in). Two are
# strict-by-default (=0) because they represent never-OK
# conditions that, if observed, indicate the scheduler-vs-legacy
# verifier and/or output validation pipeline is producing real
# divergences that must not pass CI:
#   - blocker_verifier_mismatch: any non-zero means there's a
#     scheduler-vs-legacy command divergence. If observed in CI,
#     the candidate must be quarantined OR a Class 1 fix applied.
#   - blocker_output_validation_failure: any non-zero means a
#     scheduler-dispatched action's outputs failed validation,
#     which is a correctness-class failure.
#   - failed_candidates: any non-zero means a candidate
#     executed but its action failed at runtime. Either a real
#     bug or a flaky test — both worth a CI gate.
DRY_RUN_REPORT_JSON_PATH="${RUSH_CI_DRY_RUN_REPORT_JSON:-}"
MAX_B_MISSING_VENDORED="${RUSH_CI_MAX_BLOCKER_MISSING_VENDORED_DEP:-}"
MAX_B_MISSING_HOST="${RUSH_CI_MAX_BLOCKER_MISSING_HOST_PACKAGE:-}"
MAX_B_MISSING_TOOL="${RUSH_CI_MAX_BLOCKER_MISSING_TOOL:-}"
MAX_B_UNSUPPORTED_SHAPE="${RUSH_CI_MAX_BLOCKER_UNSUPPORTED_SHAPE:-}"
MAX_B_EXTERNAL_SRC="${RUSH_CI_MAX_BLOCKER_EXTERNAL_SOURCE_MISSING:-}"
MAX_B_VERIFIER_MISMATCH="${RUSH_CI_MAX_BLOCKER_VERIFIER_MISMATCH:-0}"
MAX_B_OUTPUT_VAL="${RUSH_CI_MAX_BLOCKER_OUTPUT_VALIDATION_FAILURE:-0}"
MAX_B_AG_INCOMPLETE="${RUSH_CI_MAX_BLOCKER_ACTION_GRAPH_INCOMPLETE:-}"
MAX_B_TOO_BROAD="${RUSH_CI_MAX_BLOCKER_TOO_BROAD:-}"
MAX_QUARANTINED="${RUSH_CI_MAX_QUARANTINED_CANDIDATES:-}"
MAX_FAILED="${RUSH_CI_MAX_FAILED_CANDIDATES:-0}"
# Phase 5.C.4 Batch BQ thresholds (2026-05-25): additional
# knobs for the BN-introduced OutputPathCollision blocker
# kind, plus the BO-aligned "non-stub missing sources" knob,
# and an optional minimum-successful-pilots assertion for
# pilot-ladder profiles.
MAX_B_OUTPUT_PATH_COLLISION="${RUSH_CI_MAX_OUTPUT_PATH_COLLISIONS:-}"
MAX_NON_STUB_MISSING_SOURCES="${RUSH_CI_MAX_NON_STUB_MISSING_SOURCES:-0}"
MIN_SUCCESSFUL_PILOTS="${RUSH_CI_MIN_SUCCESSFUL_ACTION_BEARING_PILOTS:-}"
# Phase 5.C.4 Lane C step C5 (2026-05-26): Eigen-specific
# threshold knob. Functionally an alias for
# RUSH_CI_MAX_BLOCKER_MISSING_VENDORED_DEP but documented
# as the named ml_dtypes_eigen / missing-vendored-deps
# limit operators reach for when wiring up the Eigen
# materialization lane. When unset, falls back to the
# generic missing_vendored_dependency threshold.
MAX_MISSING_VENDORED_DEPS="${RUSH_CI_MAX_MISSING_VENDORED_DEPENDENCIES:-}"
if [[ -n "$MAX_MISSING_VENDORED_DEPS" && -z "$MAX_B_MISSING_VENDORED" ]]; then
    MAX_B_MISSING_VENDORED="$MAX_MISSING_VENDORED_DEPS"
fi

# ---------------------------------------------------------
# Run scheduler preflight (JSON)
# ---------------------------------------------------------

if [[ "$DRY_RUN" -eq 1 ]]; then
    echo "→ Running scheduler preflight on $TARGET (dry-run, analyze-only) ..." >&2
else
    echo "→ Running scheduler preflight on $TARGET ..." >&2
fi

# Phase 5.C.4 commit 71/78: use the report-specific CLI flag.
# stderr captures rush's human progress; stdout is the JSON
# we parse.
#
# --dry-run: add --analyze-only so rush stops after the
# analyze pass / scheduler-eligibility computation. The
# orchestrator's analyze-only path emits the same
# SchedulerPreflightReport JSON (commit 78) so threshold
# semantics are identical to the execution-capable path.
DRY_RUN_FLAGS=()
if [[ "$DRY_RUN" -eq 1 ]]; then
    DRY_RUN_FLAGS+=(--analyze-only)
fi

PREFLIGHT_JSON="$(
    "$RUSH_BIN" build \
        --use-scheduler \
        --scheduler-preflight=json \
        "${DRY_RUN_FLAGS[@]}" \
        "$TARGET" 2>/dev/null
)" || {
    echo "error: rush preflight invocation failed" >&2
    exit 2
}

if [[ -z "$PREFLIGHT_JSON" ]]; then
    echo "error: scheduler preflight produced no JSON output" >&2
    exit 2
fi

# Sanity-check schema version
SCHEMA="$(echo "$PREFLIGHT_JSON" | jq -r '.schema_version')"
if [[ "$SCHEMA" != "1" ]]; then
    echo "error: unexpected preflight schema_version: $SCHEMA (expected 1)" >&2
    echo "       this script targets schema_version=1" >&2
    exit 2
fi

# Extract counts via jq. Each bucket is a Vec; sum counts.
UNSUPPORTED_TOTAL="$(
    echo "$PREFLIGHT_JSON" | jq '[.unsupported_shape[].count] | add // 0'
)"
MISSING_TOOL_TOTAL="$(
    echo "$PREFLIGHT_JSON" | jq '[.missing_tool[].count] | add // 0'
)"
AG_INCOMPLETE_TOTAL="$(
    echo "$PREFLIGHT_JSON" | jq '[.action_graph_incomplete[].count] | add // 0'
)"
RULE_KIND_UNVERIFIED_TOTAL="$(
    echo "$PREFLIGHT_JSON" | jq '[.rule_kind_unverified[].count] | add // 0'
)"
# Phase 5.C.4 Batch AE (commit 220): parse the new
# external_repo_source_missing bucket added in ABR-2. The
# bucket may be absent in old JSON output (additive schema —
# we accept that and default to 0); fail-mode parsing handles
# the case so the gate continues to work against older Rush
# binaries.
EXTERNAL_SOURCE_MISSING_TOTAL="$(
    echo "$PREFLIGHT_JSON" | jq '[.external_repo_source_missing[]?.count] | add // 0'
)"
DISPATCHED="$(echo "$PREFLIGHT_JSON" | jq -r '.dispatched')"
TOTAL="$(echo "$PREFLIGHT_JSON" | jq -r '.total_targets')"

echo "  dispatched:                  $DISPATCHED / $TOTAL" >&2
echo "  unsupported_shape:           $UNSUPPORTED_TOTAL" >&2
echo "  missing_tool:                $MISSING_TOOL_TOTAL" >&2
echo "  action_graph_incomplete:     $AG_INCOMPLETE_TOTAL" >&2
echo "  rule_kind_unverified:        $RULE_KIND_UNVERIFIED_TOTAL" >&2
echo "  external_repo_source_missing: $EXTERNAL_SOURCE_MISSING_TOTAL" >&2

# ---------------------------------------------------------
# Apply thresholds
# ---------------------------------------------------------

FAILURES=0
fail() {
    echo "FAIL: $*" >&2
    FAILURES=$((FAILURES + 1))
}

threshold_check() {
    local name="$1"
    local count="$2"
    local limit="$3"
    if [[ -z "$limit" ]]; then
        return 0
    fi
    if [[ "$count" -gt "$limit" ]]; then
        fail "$name = $count exceeds threshold $limit"
    fi
}

threshold_check "unsupported_shape" "$UNSUPPORTED_TOTAL" "$MAX_UNSUPPORTED_SHAPES"
threshold_check "missing_tool" "$MISSING_TOOL_TOTAL" "$MAX_MISSING_TOOLS"
threshold_check "action_graph_incomplete" "$AG_INCOMPLETE_TOTAL" "$MAX_AG_INCOMPLETE"
threshold_check "rule_kind_unverified" "$RULE_KIND_UNVERIFIED_TOTAL" "$MAX_LEGACY_REQUIRED"
# Phase 5.C.4 Batch AE (commit 220): apply the new
# external_repo_source_missing threshold if the operator
# set RUSH_CI_MAX_EXTERNAL_SOURCE_MISSING. Unset → the
# count is reported but not gated (useful for analyze-only
# observation runs). For pilot profiles where scheduler
# dispatch must not have unmaterialized external repos,
# the profile sets RUSH_CI_MAX_EXTERNAL_SOURCE_MISSING=0.
threshold_check "external_repo_source_missing" "$EXTERNAL_SOURCE_MISSING_TOTAL" "$MAX_EXTERNAL_SOURCE_MISSING"

# ---------------------------------------------------------
# Phase 5.C.4 commit 79: non-remote-cacheable threshold
# ---------------------------------------------------------
#
# Resolve effective threshold from the two knobs:
#   RUSH_CI_MAX_NON_REMOTE_CACHEABLE_ACTIONS  (new, primary)
#   RUSH_CI_ALLOW_NON_REMOTE_CACHEABLE        (legacy, =0 → strict)
#
# Today every action reports NonCacheable due to
# InheritedHostEnv (commit 66). Setting strict mode today is
# expected to fail — that's the truthful signal.

EFFECTIVE_MAX_NRC=""
if [[ -n "$MAX_NON_REMOTE_CACHEABLE" ]]; then
    EFFECTIVE_MAX_NRC="$MAX_NON_REMOTE_CACHEABLE"
elif [[ "$ALLOW_NON_REMOTE_CACHEABLE" == "0" ]]; then
    EFFECTIVE_MAX_NRC="0"
fi

if [[ -n "$EFFECTIVE_MAX_NRC" ]]; then
    # Run --explain-action-keys-format=json and count
    # entries flagged non-remote-cacheable.
    #
    # Phase 5.C.4 commit 85: prefer the direct
    # `cacheability.class` field (commit 84) when present.
    # The `cacheability` object lands on every
    # ActionKeyExplainEntry whose orchestrator path is
    # post-commit-84. Fall back to the env_policy
    # heuristic for back-compat with older rush binaries
    # that don't emit the field yet.
    echo "→ Counting non-remote-cacheable actions (threshold=$EFFECTIVE_MAX_NRC) ..." >&2
    EXPLAIN_JSON="$(
        "$RUSH_BIN" build \
            --analyze-only \
            --explain-action-keys \
            --explain-action-keys-format=json \
            "$TARGET" 2>/dev/null
    )" || true
    NONCACHEABLE=0
    NRC_SOURCE="none"
    if [[ -n "$EXPLAIN_JSON" ]]; then
        # First check whether the rush binary emits the
        # `cacheability` field at all. If yes, use it as
        # the canonical signal. If not, fall back to
        # env_policy.
        HAS_CACHEABILITY="$(
            echo "$EXPLAIN_JSON" \
                | jq -rs 'map(.cacheability != null) | any' \
                2>/dev/null || echo "false"
        )"
        if [[ "$HAS_CACHEABILITY" == "true" ]]; then
            # Direct signal: count entries whose
            # cacheability.class is NOT "remote_cacheable".
            NONCACHEABLE="$(
                echo "$EXPLAIN_JSON" \
                    | jq -rs '[.[] | select((.cacheability // {}).class != "remote_cacheable" and (.cacheability // {}).class != null)] | length' \
                    2>/dev/null || echo 0
            )"
            NRC_SOURCE="cacheability.class"
        else
            # Fallback: env_policy proxy (1:1 with
            # cacheability under commit 66's defaults).
            NONCACHEABLE="$(
                echo "$EXPLAIN_JSON" \
                    | jq -rs '[.[] | select((.spec // {}).env_policy == "inherit_host")] | length' \
                    2>/dev/null || echo 0
            )"
            NRC_SOURCE="spec.env_policy"
        fi
    fi
    echo "  non_remote_cacheable_actions:  $NONCACHEABLE (source: $NRC_SOURCE)" >&2
    if [[ "$NONCACHEABLE" -gt "$EFFECTIVE_MAX_NRC" ]]; then
        fail "non_remote_cacheable_actions = $NONCACHEABLE exceeds threshold $EFFECTIVE_MAX_NRC"
    fi
fi

# ---------------------------------------------------------
# Phase 5.C.4 Batch BI: candidate-blocker taxonomy gate
# ---------------------------------------------------------
#
# Reads the dry-run report JSON (if supplied) and applies
# thresholds against its candidate_blockers section. The
# section is populated by an upstream candidate scan; when
# absent or empty, every blocker count is 0 and gates pass.
#
# The dry-run JSON path is provided by the operator via
# RUSH_CI_DRY_RUN_REPORT_JSON. This decouples the gate from
# the candidate-scan workflow: the gate parses the artifact,
# it doesn't drive the scan.

if [[ -n "$DRY_RUN_REPORT_JSON_PATH" ]]; then
    if [[ ! -f "$DRY_RUN_REPORT_JSON_PATH" ]]; then
        echo "error: RUSH_CI_DRY_RUN_REPORT_JSON not found: $DRY_RUN_REPORT_JSON_PATH" >&2
        exit 2
    fi
    DRY_RUN_JSON="$(cat "$DRY_RUN_REPORT_JSON_PATH")"
    DR_SCHEMA="$(echo "$DRY_RUN_JSON" | jq -r '.schema_version // 0')"
    if [[ "$DR_SCHEMA" != "1" ]]; then
        echo "error: unexpected dry-run schema_version: $DR_SCHEMA (expected 1)" >&2
        exit 2
    fi

    echo "→ Applying candidate-blocker taxonomy thresholds ..." >&2

    # Per-kind counts from candidate_blockers.blockers_by_kind.
    # Use jq's // 0 fallback so missing keys read as 0.
    blocker_count() {
        echo "$DRY_RUN_JSON" \
            | jq -r --arg k "$1" \
                '.candidate_blockers.blockers_by_kind[$k] // 0'
    }
    B_MISSING_VENDORED="$(blocker_count missing_vendored_dependency)"
    B_MISSING_HOST="$(blocker_count missing_host_package)"
    B_MISSING_TOOL="$(blocker_count missing_tool)"
    B_UNSUPPORTED_SHAPE="$(blocker_count unsupported_shape)"
    B_EXTERNAL_SRC="$(blocker_count external_source_missing)"
    B_VERIFIER_MISMATCH="$(blocker_count verifier_mismatch)"
    B_OUTPUT_VAL="$(blocker_count output_validation_failure)"
    B_AG_INCOMPLETE="$(blocker_count action_graph_incomplete)"
    B_TOO_BROAD="$(blocker_count too_broad)"
    B_OUTPUT_PATH_COLLISION="$(blocker_count output_path_collision)"
    QUARANTINED="$(
        echo "$DRY_RUN_JSON" \
            | jq -r '.candidate_blockers.status_counts.quarantined // 0'
    )"
    FAILED_CAND="$(
        echo "$DRY_RUN_JSON" \
            | jq -r '.candidate_blockers.status_counts.failed // 0'
    )"
    TOTAL_CAND="$(
        echo "$DRY_RUN_JSON" \
            | jq -r '.candidate_blockers.total_candidates // 0'
    )"
    echo "  candidates (total / quarantined / failed): \
$TOTAL_CAND / $QUARANTINED / $FAILED_CAND" >&2
    echo "  blockers: missing_vendored=$B_MISSING_VENDORED, \
missing_host=$B_MISSING_HOST, missing_tool=$B_MISSING_TOOL, \
unsupported_shape=$B_UNSUPPORTED_SHAPE, \
external_src=$B_EXTERNAL_SRC, verifier_mismatch=$B_VERIFIER_MISMATCH, \
output_validation_failure=$B_OUTPUT_VAL, \
action_graph_incomplete=$B_AG_INCOMPLETE, too_broad=$B_TOO_BROAD, \
output_path_collision=$B_OUTPUT_PATH_COLLISION" >&2

    threshold_check "blocker_missing_vendored_dependency" \
        "$B_MISSING_VENDORED" "$MAX_B_MISSING_VENDORED"
    threshold_check "blocker_missing_host_package" \
        "$B_MISSING_HOST" "$MAX_B_MISSING_HOST"
    threshold_check "blocker_missing_tool" \
        "$B_MISSING_TOOL" "$MAX_B_MISSING_TOOL"
    threshold_check "blocker_unsupported_shape" \
        "$B_UNSUPPORTED_SHAPE" "$MAX_B_UNSUPPORTED_SHAPE"
    threshold_check "blocker_external_source_missing" \
        "$B_EXTERNAL_SRC" "$MAX_B_EXTERNAL_SRC"
    threshold_check "blocker_verifier_mismatch" \
        "$B_VERIFIER_MISMATCH" "$MAX_B_VERIFIER_MISMATCH"
    threshold_check "blocker_output_validation_failure" \
        "$B_OUTPUT_VAL" "$MAX_B_OUTPUT_VAL"
    threshold_check "blocker_action_graph_incomplete" \
        "$B_AG_INCOMPLETE" "$MAX_B_AG_INCOMPLETE"
    threshold_check "blocker_too_broad" \
        "$B_TOO_BROAD" "$MAX_B_TOO_BROAD"
    threshold_check "blocker_output_path_collision" \
        "$B_OUTPUT_PATH_COLLISION" "$MAX_B_OUTPUT_PATH_COLLISION"
    threshold_check "quarantined_candidates" \
        "$QUARANTINED" "$MAX_QUARANTINED"
    threshold_check "failed_candidates" \
        "$FAILED_CAND" "$MAX_FAILED"

    # BQ: minimum-successful-pilots assertion. When set,
    # the gate FAILS if status_counts.executed is BELOW
    # the threshold. This is the inverse direction from
    # the "max" thresholds — used by pilot-ladder profiles
    # that want to assert progress.
    if [[ -n "$MIN_SUCCESSFUL_PILOTS" ]]; then
        EXECUTED="$(
            echo "$DRY_RUN_JSON" \
                | jq -r '.candidate_blockers.status_counts.executed // 0'
        )"
        if [[ "$EXECUTED" -lt "$MIN_SUCCESSFUL_PILOTS" ]]; then
            fail "successful_pilots = $EXECUTED is below minimum $MIN_SUCCESSFUL_PILOTS"
        fi
    fi
fi

# ---------------------------------------------------------
# Result
# ---------------------------------------------------------

if [[ "$FAILURES" -gt 0 ]]; then
    echo "" >&2
    echo "GATE FAILED: $FAILURES threshold violation(s)" >&2
    exit 1
fi

echo "" >&2
echo "GATE PASSED" >&2
exit 0
