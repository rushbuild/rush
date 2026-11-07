#!/usr/bin/env bash
# Rush — CI scheduler gate self-test (phase 5.C.4 commit 76)
#
# Static fixture + smoke-invocation tests for
# tools/ci/rush_scheduler_gate.sh.
#
# Per Pro's commit-76 directive: "If shell-script tests are
# annoying, use static fixture tests plus a smoke invocation.
# Do not turn this into a CI framework."
#
# USAGE:
#   tools/ci/test_rush_scheduler_gate.sh [rush-binary]
#
# Exits 0 on all tests passing, 1 on any failure.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
GATE_SCRIPT="$SCRIPT_DIR/rush_scheduler_gate.sh"
RUSH_BIN="${1:-rush}"

# Find workspace root for the smoke invocation.
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
HELLO_CPP="$WORKSPACE_ROOT/examples/hello_cpp"
if [[ -x "$RUSH_BIN" ]]; then
    # Resolve to absolute path so subshells changing into
    # examples/hello_cpp can still find it.
    RUSH_BIN="$(realpath "$RUSH_BIN" 2>/dev/null || readlink -f "$RUSH_BIN")"
elif command -v "$RUSH_BIN" >/dev/null 2>&1; then
    RUSH_BIN="$(command -v "$RUSH_BIN")"
elif [[ -x "$WORKSPACE_ROOT/target/release/rush" ]]; then
    RUSH_BIN="$WORKSPACE_ROOT/target/release/rush"
fi

PASS=0
FAIL=0

pass() {
    PASS=$((PASS + 1))
    echo "  ok  $1"
}
fail() {
    FAIL=$((FAIL + 1))
    echo "  FAIL  $1"
}

# ---------------------------------------------------------
# Test 1: script_fails_when_jq_missing_or_mocked
# ---------------------------------------------------------
# Run with PATH set to a directory containing no `jq`.
# Script should exit 2 and print install hint.
echo "test: script_fails_when_jq_missing"
TMP_NOJQ_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_NOJQ_DIR"' EXIT
# Symlink common shell tools but NOT jq into the sandbox so
# the script can still launch (bash, sh, command, mkdir, etc.)
# while jq's absence triggers the gate's install hint.
for cmd in bash sh command mkdir rm cat echo grep; do
    src="$(command -v "$cmd" 2>/dev/null || true)"
    [[ -n "$src" ]] && ln -sf "$src" "$TMP_NOJQ_DIR/$cmd"
done
OUT="$(PATH="$TMP_NOJQ_DIR" "$TMP_NOJQ_DIR/bash" "$GATE_SCRIPT" //... "$RUSH_BIN" 2>&1)"
RC=$?
if [[ $RC -eq 2 ]] && echo "$OUT" | grep -q "jq not found"; then
    pass "script_fails_when_jq_missing"
else
    fail "script_fails_when_jq_missing (rc=$RC, out=${OUT:0:120}...)"
fi

# ---------------------------------------------------------
# Test 2: script_parses_sample_preflight_json
# ---------------------------------------------------------
# Verify jq queries against a known JSON fixture extract the
# expected counts.
echo "test: script_parses_sample_preflight_json"
FIXTURE='{
  "schema_version": 1,
  "dispatched": 3,
  "rule_kind_unverified": [{"rule_kind": "genrule", "count": 7}],
  "unsupported_shape": [{"rule_kind": "tf_proto_library", "reason": "grpc", "count": 2}],
  "missing_tool": [{"tool": "protoc", "count": 4}],
  "action_graph_incomplete": [],
  "total_targets": 16
}'
SCHEMA="$(echo "$FIXTURE" | jq -r '.schema_version')"
UNSUPPORTED="$(echo "$FIXTURE" | jq '[.unsupported_shape[].count] | add // 0')"
MISSING="$(echo "$FIXTURE" | jq '[.missing_tool[].count] | add // 0')"
RKU="$(echo "$FIXTURE" | jq '[.rule_kind_unverified[].count] | add // 0')"
AG="$(echo "$FIXTURE" | jq '[.action_graph_incomplete[].count] | add // 0')"
if [[ "$SCHEMA" == "1" && "$UNSUPPORTED" == "2" && "$MISSING" == "4" \
    && "$RKU" == "7" && "$AG" == "0" ]]; then
    pass "script_parses_sample_preflight_json"
else
    fail "script_parses_sample_preflight_json \
         (schema=$SCHEMA unsupported=$UNSUPPORTED \
          missing=$MISSING rku=$RKU ag=$AG)"
fi

# ---------------------------------------------------------
# Smoke invocation tests (require rush + hello_cpp).
# ---------------------------------------------------------
if [[ ! -x "$RUSH_BIN" ]] && ! command -v "$RUSH_BIN" >/dev/null 2>&1; then
    echo "skip: smoke tests (rush binary not found at $RUSH_BIN)"
    echo ""
    echo "summary: $PASS passed, $FAIL failed (smoke tests skipped)"
    [[ $FAIL -eq 0 ]] && exit 0 || exit 1
fi
if [[ ! -d "$HELLO_CPP" ]]; then
    echo "skip: smoke tests (hello_cpp not found at $HELLO_CPP)"
    echo ""
    echo "summary: $PASS passed, $FAIL failed (smoke tests skipped)"
    [[ $FAIL -eq 0 ]] && exit 0 || exit 1
fi

# ---------------------------------------------------------
# Test 3: script_passes_with_threshold_allowance
# ---------------------------------------------------------
echo "test: script_passes_with_threshold_allowance"
rm -rf "$HELLO_CPP/rush-out"
(
    cd "$HELLO_CPP" && \
    RUSH_CI_MAX_UNSUPPORTED_SHAPES=100 \
    RUSH_CI_MAX_MISSING_TOOLS=100 \
    RUSH_CI_MAX_LEGACY_REQUIRED_TARGETS=100 \
    "$GATE_SCRIPT" //... "$RUSH_BIN" >/dev/null 2>&1
)
RC=$?
if [[ $RC -eq 0 ]]; then
    pass "script_passes_with_threshold_allowance"
else
    fail "script_passes_with_threshold_allowance (rc=$RC)"
fi

# ---------------------------------------------------------
# Test 4: script_fails_on_unsupported_shape_threshold
# ---------------------------------------------------------
# Phase 5.C.4 BK refresh: prior fixture relied on hello_cpp
# having at least one rule_kind_unverified target. After
# BE (commit 252) added system_cc_library to the scheduler
# allowlist, hello_cpp's //... is 2/2 dispatched with empty
# unsupported_shape and rule_kind_unverified buckets. The
# previous test premise no longer holds.
#
# Refresh: use a tiny fake-rush wrapper that emits a known
# preflight JSON with rule_kind_unverified=7. The gate
# parses it just like a real rush invocation. This makes
# the test hermetic — it verifies the threshold-trip logic
# without depending on any specific target's preflight
# bucket counts.
echo "test: script_fails_on_unsupported_shape_threshold"
FAKE_RUSH_FAIL="$(mktemp)"
cat > "$FAKE_RUSH_FAIL" <<'EOF'
#!/usr/bin/env bash
cat <<'JSON'
{
  "schema_version": 1,
  "dispatched": 3,
  "rule_kind_unverified": [{"rule_kind": "genrule", "count": 7}],
  "unsupported_shape": [],
  "missing_tool": [],
  "action_graph_incomplete": [],
  "external_repo_source_missing": [],
  "total_targets": 10
}
JSON
EOF
chmod +x "$FAKE_RUSH_FAIL"
RUSH_CI_MAX_LEGACY_REQUIRED_TARGETS=0 \
    "$GATE_SCRIPT" //... "$FAKE_RUSH_FAIL" >/dev/null 2>&1
RC=$?
rm -f "$FAKE_RUSH_FAIL"
if [[ $RC -eq 1 ]]; then
    pass "script_fails_on_unsupported_shape_threshold"
else
    fail "script_fails_on_unsupported_shape_threshold (rc=$RC, expected 1)"
fi

# ---------------------------------------------------------
# Summary
# ---------------------------------------------------------

# ---------------------------------------------------------
# Test 5 (commit 78): dry_run_passes_on_hello_cpp_with_loose_thresholds
# ---------------------------------------------------------
echo "test: dry_run_passes_on_hello_cpp_with_loose_thresholds"
rm -rf "$HELLO_CPP/rush-out"
(
    cd "$HELLO_CPP" && \
    RUSH_CI_MAX_UNSUPPORTED_SHAPES=100 \
    RUSH_CI_MAX_MISSING_TOOLS=100 \
    RUSH_CI_MAX_LEGACY_REQUIRED_TARGETS=100 \
    "$GATE_SCRIPT" --dry-run //... "$RUSH_BIN" >/dev/null 2>&1
)
RC=$?
if [[ $RC -eq 0 ]]; then
    pass "dry_run_passes_on_hello_cpp_with_loose_thresholds"
else
    fail "dry_run_passes_on_hello_cpp_with_loose_thresholds (rc=$RC)"
fi

# ---------------------------------------------------------
# Test 6 (commit 78, BK-refreshed): dry_run_fails_on_unsupported_shape_threshold
# ---------------------------------------------------------
# Same BK refresh as Test 4: hello_cpp's //... has no
# rule_kind_unverified after BE allowlisted system_cc_library,
# so we use a fake-rush wrapper. The --dry-run flag is still
# passed to confirm the threshold logic applies identically
# in dry-run mode.
echo "test: dry_run_fails_on_unsupported_shape_threshold"
FAKE_RUSH_DR_FAIL="$(mktemp)"
cat > "$FAKE_RUSH_DR_FAIL" <<'EOF'
#!/usr/bin/env bash
cat <<'JSON'
{
  "schema_version": 1,
  "dispatched": 3,
  "rule_kind_unverified": [{"rule_kind": "genrule", "count": 7}],
  "unsupported_shape": [],
  "missing_tool": [],
  "action_graph_incomplete": [],
  "external_repo_source_missing": [],
  "total_targets": 10
}
JSON
EOF
chmod +x "$FAKE_RUSH_DR_FAIL"
RUSH_CI_MAX_LEGACY_REQUIRED_TARGETS=0 \
    "$GATE_SCRIPT" --dry-run //... "$FAKE_RUSH_DR_FAIL" >/dev/null 2>&1
RC=$?
rm -f "$FAKE_RUSH_DR_FAIL"
if [[ $RC -eq 1 ]]; then
    pass "dry_run_fails_on_unsupported_shape_threshold"
else
    fail "dry_run_fails_on_unsupported_shape_threshold (rc=$RC, expected 1)"
fi

# ---------------------------------------------------------
# Test 7 (commit 79): non_remote_cacheable_default_allowed
# ---------------------------------------------------------
# With neither RUSH_CI_MAX_NON_REMOTE_CACHEABLE_ACTIONS nor
# RUSH_CI_ALLOW_NON_REMOTE_CACHEABLE=0 set, the threshold is
# unenforced — the gate passes regardless of how many
# actions are non-cacheable. Pin the default-allowed
# contract.
echo "test: non_remote_cacheable_default_allowed"
rm -rf "$HELLO_CPP/rush-out"
(
    cd "$HELLO_CPP" && \
    RUSH_CI_MAX_UNSUPPORTED_SHAPES=100 \
    RUSH_CI_MAX_MISSING_TOOLS=100 \
    RUSH_CI_MAX_LEGACY_REQUIRED_TARGETS=100 \
    "$GATE_SCRIPT" --dry-run //... "$RUSH_BIN" >/dev/null 2>&1
)
RC=$?
if [[ $RC -eq 0 ]]; then
    pass "non_remote_cacheable_default_allowed"
else
    fail "non_remote_cacheable_default_allowed (rc=$RC, default should allow)"
fi

# ---------------------------------------------------------
# Test 8 (commit 79): max_non_remote_cacheable_zero_fails_today
# ---------------------------------------------------------
# RUSH_CI_MAX_NON_REMOTE_CACHEABLE_ACTIONS=0 = strict mode.
# Today every action is non-cacheable (InheritedHostEnv) so
# this MUST fail. Pin the truthful-strict-mode-fails-today
# contract.
echo "test: max_non_remote_cacheable_zero_fails_today"
rm -rf "$HELLO_CPP/rush-out"
(
    cd "$HELLO_CPP" && \
    RUSH_CI_MAX_UNSUPPORTED_SHAPES=100 \
    RUSH_CI_MAX_MISSING_TOOLS=100 \
    RUSH_CI_MAX_LEGACY_REQUIRED_TARGETS=100 \
    RUSH_CI_MAX_NON_REMOTE_CACHEABLE_ACTIONS=0 \
    "$GATE_SCRIPT" --dry-run //... "$RUSH_BIN" >/dev/null 2>&1
)
RC=$?
if [[ $RC -eq 1 ]]; then
    pass "max_non_remote_cacheable_zero_fails_today"
else
    fail "max_non_remote_cacheable_zero_fails_today (rc=$RC, expected 1 — strict mode should fail today)"
fi

# ---------------------------------------------------------
# Test 9 (commit 79): max_non_remote_cacheable_threshold_passes_when_count_below_limit
# ---------------------------------------------------------
# Setting MAX to a large number passes regardless of count.
echo "test: max_non_remote_cacheable_threshold_passes_when_count_below_limit"
rm -rf "$HELLO_CPP/rush-out"
(
    cd "$HELLO_CPP" && \
    RUSH_CI_MAX_UNSUPPORTED_SHAPES=100 \
    RUSH_CI_MAX_MISSING_TOOLS=100 \
    RUSH_CI_MAX_LEGACY_REQUIRED_TARGETS=100 \
    RUSH_CI_MAX_NON_REMOTE_CACHEABLE_ACTIONS=1000 \
    "$GATE_SCRIPT" --dry-run //... "$RUSH_BIN" >/dev/null 2>&1
)
RC=$?
if [[ $RC -eq 0 ]]; then
    pass "max_non_remote_cacheable_threshold_passes_when_count_below_limit"
else
    fail "max_non_remote_cacheable_threshold_passes_when_count_below_limit (rc=$RC)"
fi

# ---------------------------------------------------------
# Test 10 (commit 79): allow_non_remote_cacheable_zero_is_shorthand_for_max_zero
# ---------------------------------------------------------
# Legacy ALLOW=0 is shorthand for MAX=0 — should fail
# identically to test 8.
echo "test: allow_non_remote_cacheable_zero_is_shorthand_for_max_zero"
rm -rf "$HELLO_CPP/rush-out"
(
    cd "$HELLO_CPP" && \
    RUSH_CI_MAX_UNSUPPORTED_SHAPES=100 \
    RUSH_CI_MAX_MISSING_TOOLS=100 \
    RUSH_CI_MAX_LEGACY_REQUIRED_TARGETS=100 \
    RUSH_CI_ALLOW_NON_REMOTE_CACHEABLE=0 \
    "$GATE_SCRIPT" --dry-run //... "$RUSH_BIN" >/dev/null 2>&1
)
RC=$?
if [[ $RC -eq 1 ]]; then
    pass "allow_non_remote_cacheable_zero_is_shorthand_for_max_zero"
else
    fail "allow_non_remote_cacheable_zero_is_shorthand_for_max_zero (rc=$RC, expected 1)"
fi

# ---------------------------------------------------------
# Test 11 (commit 78): dry_run_does_not_execute_actions
# ---------------------------------------------------------
# After a dry-run pass, rush-out should NOT contain compiled
# artifacts (no .o files). --analyze-only stops before
# action execution.
echo "test: dry_run_does_not_execute_actions"
rm -rf "$HELLO_CPP/rush-out"
(
    cd "$HELLO_CPP" && \
    "$GATE_SCRIPT" --dry-run //... "$RUSH_BIN" >/dev/null 2>&1
) || true
OBJS_FOUND=0
if [[ -d "$HELLO_CPP/rush-out" ]]; then
    OBJS_FOUND="$(find "$HELLO_CPP/rush-out" -name '*.o' 2>/dev/null | wc -l)"
fi
if [[ "$OBJS_FOUND" -eq 0 ]]; then
    pass "dry_run_does_not_execute_actions"
else
    fail "dry_run_does_not_execute_actions (found $OBJS_FOUND .o files)"
fi

# ---------------------------------------------------------
# Test 12 (commit 85): ci_gate_counts_non_cacheable_from_cacheability_field
# ---------------------------------------------------------
# Today's rush binary emits the `cacheability` field on
# every explain entry (post commit-84). Verify the gate
# uses it as the signal source by checking the log
# message includes "(source: cacheability.class)".
echo "test: ci_gate_counts_non_cacheable_from_cacheability_field"
rm -rf "$HELLO_CPP/rush-out"
OUT="$(
    cd "$HELLO_CPP" && \
    RUSH_CI_MAX_UNSUPPORTED_SHAPES=100 \
    RUSH_CI_MAX_MISSING_TOOLS=100 \
    RUSH_CI_MAX_LEGACY_REQUIRED_TARGETS=100 \
    RUSH_CI_MAX_NON_REMOTE_CACHEABLE_ACTIONS=1000 \
    "$GATE_SCRIPT" --dry-run //... "$RUSH_BIN" 2>&1
)"
if echo "$OUT" | grep -q "source: cacheability.class"; then
    pass "ci_gate_counts_non_cacheable_from_cacheability_field"
else
    fail "ci_gate_counts_non_cacheable_from_cacheability_field (no 'cacheability.class' source in output)"
fi

# ---------------------------------------------------------
# Test 13 (commit 85): ci_gate_falls_back_to_env_policy_for_old_json
# ---------------------------------------------------------
# Verify the fallback path triggers when fed JSON that
# DOESN'T include cacheability (simulated old rush binary).
# Uses a static fixture passed through jq directly.
echo "test: ci_gate_falls_back_to_env_policy_for_old_json"
OLD_JSON='{"schema_version":1,"spec":{"env_policy":"inherit_host","mnemonic":"X"}}
{"schema_version":1,"spec":{"env_policy":"inherit_host","mnemonic":"Y"}}
{"schema_version":1,"spec":{"env_policy":"explicit_only","mnemonic":"Z"}}'
# Pre-commit-85 jq predicate (env_policy fallback).
HAS_CACHE="$(echo "$OLD_JSON" | jq -rs 'map(.cacheability != null) | any')"
if [[ "$HAS_CACHE" == "false" ]]; then
    COUNT="$(echo "$OLD_JSON" | jq -rs '[.[] | select((.spec // {}).env_policy == "inherit_host")] | length')"
    if [[ "$COUNT" == "2" ]]; then
        pass "ci_gate_falls_back_to_env_policy_for_old_json"
    else
        fail "ci_gate_falls_back_to_env_policy_for_old_json (expected 2 inherit_host entries, got $COUNT)"
    fi
else
    fail "ci_gate_falls_back_to_env_policy_for_old_json (cacheability detection said true on old JSON)"
fi

# ---------------------------------------------------------
# Test 14 (commit 85): max_non_remote_cacheable_threshold_still_works
# ---------------------------------------------------------
# Regression test: with the new cacheability.class signal,
# MAX_NON_REMOTE_CACHEABLE_ACTIONS=0 still fails on
# hello_cpp (every action InheritHost / NonCacheable).
echo "test: max_non_remote_cacheable_threshold_still_works"
rm -rf "$HELLO_CPP/rush-out"
(
    cd "$HELLO_CPP" && \
    RUSH_CI_MAX_UNSUPPORTED_SHAPES=100 \
    RUSH_CI_MAX_MISSING_TOOLS=100 \
    RUSH_CI_MAX_LEGACY_REQUIRED_TARGETS=100 \
    RUSH_CI_MAX_NON_REMOTE_CACHEABLE_ACTIONS=0 \
    "$GATE_SCRIPT" --dry-run //... "$RUSH_BIN" >/dev/null 2>&1
)
RC=$?
if [[ $RC -eq 1 ]]; then
    pass "max_non_remote_cacheable_threshold_still_works"
else
    fail "max_non_remote_cacheable_threshold_still_works (rc=$RC, expected 1)"
fi

# ---------------------------------------------------------
# Phase 5.C.4 Batch AE (commit 220): external_repo_source_missing
# bucket parsing and threshold check.
# ---------------------------------------------------------

# Test: jq parses the new external_repo_source_missing bucket
# from a JSON fixture matching the ABR-2 schema.
echo "test: script_parses_external_repo_source_missing_bucket"
FIXTURE_EM='{
  "schema_version": 1,
  "dispatched": 0,
  "rule_kind_unverified": [],
  "unsupported_shape": [],
  "missing_tool": [],
  "action_graph_incomplete": [],
  "external_repo_source_missing": [
    {"rule_kind": "cc_library", "reason": "external repo source file missing", "count": 3},
    {"rule_kind": "cc_library", "reason": "external repo directory not materialized", "count": 1}
  ],
  "total_targets": 4
}'
EM_TOTAL="$(echo "$FIXTURE_EM" | jq '[.external_repo_source_missing[]?.count] | add // 0')"
if [[ "$EM_TOTAL" == "4" ]]; then
    pass "script_parses_external_repo_source_missing_bucket"
else
    fail "script_parses_external_repo_source_missing_bucket (got $EM_TOTAL, expected 4)"
fi

# Test: bucket-absence backward compatibility. The bucket may
# be missing in JSON produced by old Rush binaries; the gate
# defaults to 0.
echo "test: script_defaults_external_repo_source_missing_when_absent"
FIXTURE_NO_EM='{
  "schema_version": 1,
  "dispatched": 1,
  "rule_kind_unverified": [],
  "unsupported_shape": [],
  "missing_tool": [],
  "action_graph_incomplete": [],
  "total_targets": 1
}'
EM_DEFAULT="$(echo "$FIXTURE_NO_EM" | jq '[.external_repo_source_missing[]?.count] | add // 0')"
if [[ "$EM_DEFAULT" == "0" ]]; then
    pass "script_defaults_external_repo_source_missing_when_absent"
else
    fail "script_defaults_external_repo_source_missing_when_absent (got $EM_DEFAULT)"
fi

# Test: dry-run with RUSH_CI_MAX_EXTERNAL_SOURCE_MISSING=0
# passes on a clean target (hello_cpp has no external repo
# source dependencies; the bucket is empty).
echo "test: dry_run_passes_max_external_source_missing_zero_on_clean_target"
( cd "$HELLO_CPP" && \
    RUSH_CI_ALLOW_NON_REMOTE_CACHEABLE=1 \
    RUSH_CI_MAX_EXTERNAL_SOURCE_MISSING=0 \
    "$GATE_SCRIPT" --dry-run //:hello "$RUSH_BIN" >/dev/null 2>&1 )
RC=$?
if [[ $RC -eq 0 ]]; then
    pass "dry_run_passes_max_external_source_missing_zero_on_clean_target"
else
    fail "dry_run_passes_max_external_source_missing_zero_on_clean_target (rc=$RC, expected 0)"
fi

# ---------------------------------------------------------
# Phase 5.C.4 Batch BI: candidate-blocker taxonomy thresholds
# ---------------------------------------------------------
# Build a synthetic dry-run report JSON fixture matching the
# BH schema and verify the gate honors the new thresholds.

echo "test: bi_dry_run_report_zero_blockers_passes"
TMP_DR_OK="$(mktemp)"
cat > "$TMP_DR_OK" <<'EOF'
{
  "schema_version": 1,
  "scheduler_preflight": {
    "schema_version": 1,
    "dispatched": 0,
    "rule_kind_unverified": [],
    "unsupported_shape": [],
    "missing_tool": [],
    "action_graph_incomplete": [],
    "external_repo_source_missing": [],
    "total_targets": 0
  },
  "legacy_deletion_entries": [],
  "total_legacy_complete": 0,
  "total_legacy_incomplete": 0,
  "by_rule_kind": [],
  "by_reason": [],
  "materialization": {
    "total_examined": 0,
    "blocked_targets": 0,
    "blockers_by_repo": {}
  },
  "output_validation": {
    "declared_actions": 0,
    "incomplete_actions": 0,
    "missing_input_producers": 0,
    "missing_provider_producers": 0
  },
  "producer_readiness": {
    "total_generated_inputs": 0,
    "with_producer": 0,
    "without_producer": 0
  },
  "per_target_blockers": [],
  "candidate_blockers": {
    "status_counts": {
      "ready": 1,
      "blocked": 0,
      "quarantined": 0,
      "executed": 0,
      "failed": 0
    },
    "blockers_by_kind": {},
    "total_candidates": 1
  }
}
EOF
( cd "$HELLO_CPP" && \
    RUSH_CI_ALLOW_NON_REMOTE_CACHEABLE=1 \
    RUSH_CI_MAX_UNSUPPORTED_SHAPES=100 \
    RUSH_CI_MAX_MISSING_TOOLS=100 \
    RUSH_CI_MAX_LEGACY_REQUIRED_TARGETS=100 \
    RUSH_CI_DRY_RUN_REPORT_JSON="$TMP_DR_OK" \
    "$GATE_SCRIPT" --dry-run //:hello "$RUSH_BIN" >/dev/null 2>&1 )
RC=$?
rm -f "$TMP_DR_OK"
if [[ $RC -eq 0 ]]; then
    pass "bi_dry_run_report_zero_blockers_passes"
else
    fail "bi_dry_run_report_zero_blockers_passes (rc=$RC)"
fi

echo "test: bi_dry_run_report_verifier_mismatch_default_strict_fails"
TMP_DR_VM="$(mktemp)"
cat > "$TMP_DR_VM" <<'EOF'
{
  "schema_version": 1,
  "scheduler_preflight": {
    "schema_version": 1,
    "dispatched": 0,
    "rule_kind_unverified": [],
    "unsupported_shape": [],
    "missing_tool": [],
    "action_graph_incomplete": [],
    "external_repo_source_missing": [],
    "total_targets": 0
  },
  "legacy_deletion_entries": [],
  "total_legacy_complete": 0,
  "total_legacy_incomplete": 0,
  "by_rule_kind": [],
  "by_reason": [],
  "materialization": {
    "total_examined": 0,
    "blocked_targets": 0,
    "blockers_by_repo": {}
  },
  "output_validation": {
    "declared_actions": 0,
    "incomplete_actions": 0,
    "missing_input_producers": 0,
    "missing_provider_producers": 0
  },
  "producer_readiness": {
    "total_generated_inputs": 0,
    "with_producer": 0,
    "without_producer": 0
  },
  "per_target_blockers": [],
  "candidate_blockers": {
    "status_counts": {
      "ready": 0,
      "blocked": 0,
      "quarantined": 1,
      "executed": 0,
      "failed": 0
    },
    "blockers_by_kind": {
      "verifier_mismatch": 1
    },
    "total_candidates": 1
  }
}
EOF
( cd "$HELLO_CPP" && \
    RUSH_CI_ALLOW_NON_REMOTE_CACHEABLE=1 \
    RUSH_CI_MAX_UNSUPPORTED_SHAPES=100 \
    RUSH_CI_MAX_MISSING_TOOLS=100 \
    RUSH_CI_MAX_LEGACY_REQUIRED_TARGETS=100 \
    RUSH_CI_DRY_RUN_REPORT_JSON="$TMP_DR_VM" \
    "$GATE_SCRIPT" --dry-run //:hello "$RUSH_BIN" >/dev/null 2>&1 )
RC=$?
rm -f "$TMP_DR_VM"
if [[ $RC -eq 1 ]]; then
    pass "bi_dry_run_report_verifier_mismatch_default_strict_fails"
else
    fail "bi_dry_run_report_verifier_mismatch_default_strict_fails (rc=$RC, expected 1 — verifier_mismatch defaults to strict)"
fi

echo "test: bi_dry_run_report_verifier_mismatch_relaxed_passes"
TMP_DR_VM2="$(mktemp)"
cat > "$TMP_DR_VM2" <<'EOF'
{
  "schema_version": 1,
  "scheduler_preflight": {
    "schema_version": 1,
    "dispatched": 0,
    "rule_kind_unverified": [],
    "unsupported_shape": [],
    "missing_tool": [],
    "action_graph_incomplete": [],
    "external_repo_source_missing": [],
    "total_targets": 0
  },
  "legacy_deletion_entries": [],
  "total_legacy_complete": 0,
  "total_legacy_incomplete": 0,
  "by_rule_kind": [],
  "by_reason": [],
  "materialization": {
    "total_examined": 0,
    "blocked_targets": 0,
    "blockers_by_repo": {}
  },
  "output_validation": {
    "declared_actions": 0,
    "incomplete_actions": 0,
    "missing_input_producers": 0,
    "missing_provider_producers": 0
  },
  "producer_readiness": {
    "total_generated_inputs": 0,
    "with_producer": 0,
    "without_producer": 0
  },
  "per_target_blockers": [],
  "candidate_blockers": {
    "status_counts": {
      "ready": 0,
      "blocked": 0,
      "quarantined": 1,
      "executed": 0,
      "failed": 0
    },
    "blockers_by_kind": {
      "verifier_mismatch": 1,
      "missing_vendored_dependency": 5,
      "too_broad": 2
    },
    "total_candidates": 1
  }
}
EOF
( cd "$HELLO_CPP" && \
    RUSH_CI_ALLOW_NON_REMOTE_CACHEABLE=1 \
    RUSH_CI_MAX_UNSUPPORTED_SHAPES=100 \
    RUSH_CI_MAX_MISSING_TOOLS=100 \
    RUSH_CI_MAX_LEGACY_REQUIRED_TARGETS=100 \
    RUSH_CI_DRY_RUN_REPORT_JSON="$TMP_DR_VM2" \
    RUSH_CI_MAX_BLOCKER_VERIFIER_MISMATCH=5 \
    "$GATE_SCRIPT" --dry-run //:hello "$RUSH_BIN" >/dev/null 2>&1 )
RC=$?
rm -f "$TMP_DR_VM2"
if [[ $RC -eq 0 ]]; then
    pass "bi_dry_run_report_verifier_mismatch_relaxed_passes"
else
    fail "bi_dry_run_report_verifier_mismatch_relaxed_passes (rc=$RC)"
fi

echo "test: bi_dry_run_report_failed_candidates_default_strict_fails"
TMP_DR_FC="$(mktemp)"
cat > "$TMP_DR_FC" <<'EOF'
{
  "schema_version": 1,
  "scheduler_preflight": {
    "schema_version": 1,
    "dispatched": 0,
    "rule_kind_unverified": [],
    "unsupported_shape": [],
    "missing_tool": [],
    "action_graph_incomplete": [],
    "external_repo_source_missing": [],
    "total_targets": 0
  },
  "legacy_deletion_entries": [],
  "total_legacy_complete": 0,
  "total_legacy_incomplete": 0,
  "by_rule_kind": [],
  "by_reason": [],
  "materialization": {
    "total_examined": 0,
    "blocked_targets": 0,
    "blockers_by_repo": {}
  },
  "output_validation": {
    "declared_actions": 0,
    "incomplete_actions": 0,
    "missing_input_producers": 0,
    "missing_provider_producers": 0
  },
  "producer_readiness": {
    "total_generated_inputs": 0,
    "with_producer": 0,
    "without_producer": 0
  },
  "per_target_blockers": [],
  "candidate_blockers": {
    "status_counts": {
      "ready": 0,
      "blocked": 0,
      "quarantined": 0,
      "executed": 0,
      "failed": 1
    },
    "blockers_by_kind": {},
    "total_candidates": 1
  }
}
EOF
( cd "$HELLO_CPP" && \
    RUSH_CI_ALLOW_NON_REMOTE_CACHEABLE=1 \
    RUSH_CI_MAX_UNSUPPORTED_SHAPES=100 \
    RUSH_CI_MAX_MISSING_TOOLS=100 \
    RUSH_CI_MAX_LEGACY_REQUIRED_TARGETS=100 \
    RUSH_CI_DRY_RUN_REPORT_JSON="$TMP_DR_FC" \
    "$GATE_SCRIPT" --dry-run //:hello "$RUSH_BIN" >/dev/null 2>&1 )
RC=$?
rm -f "$TMP_DR_FC"
if [[ $RC -eq 1 ]]; then
    pass "bi_dry_run_report_failed_candidates_default_strict_fails"
else
    fail "bi_dry_run_report_failed_candidates_default_strict_fails (rc=$RC, expected 1)"
fi

echo "test: bq_dry_run_report_output_path_collision_threshold"
TMP_DR_OPC="$(mktemp)"
cat > "$TMP_DR_OPC" <<'EOF'
{
  "schema_version": 1,
  "scheduler_preflight": {
    "schema_version": 1,
    "dispatched": 0,
    "rule_kind_unverified": [],
    "unsupported_shape": [],
    "missing_tool": [],
    "action_graph_incomplete": [],
    "external_repo_source_missing": [],
    "total_targets": 0
  },
  "legacy_deletion_entries": [],
  "total_legacy_complete": 0,
  "total_legacy_incomplete": 0,
  "by_rule_kind": [],
  "by_reason": [],
  "materialization": {
    "total_examined": 0,
    "blocked_targets": 0,
    "blockers_by_repo": {}
  },
  "output_validation": {
    "declared_actions": 0,
    "incomplete_actions": 0,
    "missing_input_producers": 0,
    "missing_provider_producers": 0
  },
  "producer_readiness": {
    "total_generated_inputs": 0,
    "with_producer": 0,
    "without_producer": 0
  },
  "per_target_blockers": [],
  "candidate_blockers": {
    "status_counts": {
      "ready": 0,
      "blocked": 0,
      "quarantined": 1,
      "executed": 0,
      "failed": 0
    },
    "blockers_by_kind": {
      "output_path_collision": 2
    },
    "total_candidates": 1
  }
}
EOF
( cd "$HELLO_CPP" && \
    RUSH_CI_ALLOW_NON_REMOTE_CACHEABLE=1 \
    RUSH_CI_MAX_UNSUPPORTED_SHAPES=100 \
    RUSH_CI_MAX_MISSING_TOOLS=100 \
    RUSH_CI_MAX_LEGACY_REQUIRED_TARGETS=100 \
    RUSH_CI_DRY_RUN_REPORT_JSON="$TMP_DR_OPC" \
    RUSH_CI_MAX_OUTPUT_PATH_COLLISIONS=1 \
    "$GATE_SCRIPT" --dry-run //:hello "$RUSH_BIN" >/dev/null 2>&1 )
RC=$?
rm -f "$TMP_DR_OPC"
if [[ $RC -eq 1 ]]; then
    pass "bq_dry_run_report_output_path_collision_threshold"
else
    fail "bq_dry_run_report_output_path_collision_threshold (rc=$RC, expected 1)"
fi

echo "test: bq_dry_run_report_min_successful_pilots_assertion"
TMP_DR_MIN="$(mktemp)"
cat > "$TMP_DR_MIN" <<'EOF'
{
  "schema_version": 1,
  "scheduler_preflight": {
    "schema_version": 1,
    "dispatched": 0,
    "rule_kind_unverified": [],
    "unsupported_shape": [],
    "missing_tool": [],
    "action_graph_incomplete": [],
    "external_repo_source_missing": [],
    "total_targets": 0
  },
  "legacy_deletion_entries": [],
  "total_legacy_complete": 0,
  "total_legacy_incomplete": 0,
  "by_rule_kind": [],
  "by_reason": [],
  "materialization": {
    "total_examined": 0,
    "blocked_targets": 0,
    "blockers_by_repo": {}
  },
  "output_validation": {
    "declared_actions": 0,
    "incomplete_actions": 0,
    "missing_input_producers": 0,
    "missing_provider_producers": 0
  },
  "producer_readiness": {
    "total_generated_inputs": 0,
    "with_producer": 0,
    "without_producer": 0
  },
  "per_target_blockers": [],
  "candidate_blockers": {
    "status_counts": {
      "ready": 0,
      "blocked": 0,
      "quarantined": 0,
      "executed": 2,
      "failed": 0
    },
    "blockers_by_kind": {},
    "total_candidates": 2
  }
}
EOF
( cd "$HELLO_CPP" && \
    RUSH_CI_ALLOW_NON_REMOTE_CACHEABLE=1 \
    RUSH_CI_MAX_UNSUPPORTED_SHAPES=100 \
    RUSH_CI_MAX_MISSING_TOOLS=100 \
    RUSH_CI_MAX_LEGACY_REQUIRED_TARGETS=100 \
    RUSH_CI_DRY_RUN_REPORT_JSON="$TMP_DR_MIN" \
    RUSH_CI_MIN_SUCCESSFUL_ACTION_BEARING_PILOTS=5 \
    "$GATE_SCRIPT" --dry-run //:hello "$RUSH_BIN" >/dev/null 2>&1 )
RC=$?
rm -f "$TMP_DR_MIN"
if [[ $RC -eq 1 ]]; then
    pass "bq_dry_run_report_min_successful_pilots_assertion"
else
    fail "bq_dry_run_report_min_successful_pilots_assertion (rc=$RC, expected 1 — 2 executed below min=5)"
fi

echo "test: bi_dry_run_report_missing_path_fails"
( cd "$HELLO_CPP" && \
    RUSH_CI_ALLOW_NON_REMOTE_CACHEABLE=1 \
    RUSH_CI_MAX_UNSUPPORTED_SHAPES=100 \
    RUSH_CI_MAX_MISSING_TOOLS=100 \
    RUSH_CI_MAX_LEGACY_REQUIRED_TARGETS=100 \
    RUSH_CI_DRY_RUN_REPORT_JSON=/nonexistent/dry_run.json \
    "$GATE_SCRIPT" --dry-run //:hello "$RUSH_BIN" >/dev/null 2>&1 )
RC=$?
if [[ $RC -eq 2 ]]; then
    pass "bi_dry_run_report_missing_path_fails"
else
    fail "bi_dry_run_report_missing_path_fails (rc=$RC, expected 2)"
fi

echo ""
echo "summary: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]] && exit 0 || exit 1
