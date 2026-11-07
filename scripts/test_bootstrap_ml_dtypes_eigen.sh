#!/usr/bin/env bash
# Rush — self-tests for bootstrap_ml_dtypes_eigen.sh
# Phase 5.C.4 commit 268 (Lane C step C2).
#
# Per Pro's pattern (gate-script tests): hermetic where
# possible, network-touching where unavoidable. The
# dry-run mode tests don't touch the network; the apply
# tests would, so they're guarded behind
# RUSH_BOOTSTRAP_TEST_NETWORK=1.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BOOTSTRAP="$SCRIPT_DIR/bootstrap_ml_dtypes_eigen.sh"

PASS=0
FAIL=0
pass() { PASS=$((PASS + 1)); echo "  ok  $1"; }
fail() { FAIL=$((FAIL + 1)); echo "  FAIL  $1"; }

# ---------------------------------------------------------
# Test 1 (Pro C2 spec): dry-run prints expected commands
# ---------------------------------------------------------
echo "test: dry_run_prints_expected_commands"
OUT="$(bash "$BOOTSTRAP" --dry-run 2>&1)"
RC=$?
if [[ $RC -eq 0 ]] \
    && echo "$OUT" | grep -q "Would fetch and verify @eigen_archive" \
    && echo "$OUT" | grep -q "Would fetch and verify @ml_dtypes" \
    && echo "$OUT" | grep -q "https://gitlab.com/libeigen/eigen" \
    && echo "$OUT" | grep -q "https://github.com/jax-ml/ml_dtypes" \
    && echo "$OUT" | grep -q "66e8f38891841bf88ee976a316c0c78a52f0cee5" \
    && echo "$OUT" | grep -q "2ca30a2b3c0744625ae3d6988f5596740080bbd0" \
    && echo "$OUT" | grep -q "Expected include roots"; then
    pass "dry_run_prints_expected_commands"
else
    fail "dry_run_prints_expected_commands (rc=$RC)"
fi

# ---------------------------------------------------------
# Test 2 (Pro C2 spec): apply refuses existing dir without
# --force
# ---------------------------------------------------------
echo "test: apply_refuses_existing_dir_without_force"
TMP_CACHE="$(mktemp -d)"
mkdir -p "$TMP_CACHE/eigen_archive"
echo "existing content" > "$TMP_CACHE/eigen_archive/old_file.txt"
OUT="$(RUSH_GLOBAL_CLONE_CACHE="$TMP_CACHE" \
    bash "$BOOTSTRAP" --apply 2>&1)"
RC=$?
rm -rf "$TMP_CACHE"
if [[ $RC -eq 1 ]] \
    && echo "$OUT" | grep -q "non-empty; use --force to overwrite"; then
    pass "apply_refuses_existing_dir_without_force"
else
    fail "apply_refuses_existing_dir_without_force (rc=$RC, expected 1 + refuse msg)"
fi

# ---------------------------------------------------------
# Test 3 (Pro C2 spec): metadata missing fails cleanly.
# Use a tmpdir without curl as the entire PATH — the
# script's require_tool check exits 2 cleanly.
# ---------------------------------------------------------
echo "test: metadata_missing_fails_cleanly"
# Build a tmpdir with bash but NO curl. The script's
# `require_tool curl` will fire exit 2 with a clean
# "required tool not on PATH" message.
TMP_NOCURL="$(mktemp -d)"
trap "rm -rf '$TMP_NOCURL'" EXIT
# Symlink minimal shell tools but NOT curl.
for cmd in bash sh mkdir mktemp dirname awk sed grep cat \
           true false test command rm ls; do
    src="$(command -v "$cmd" 2>/dev/null || true)"
    [[ -n "$src" ]] && ln -sf "$src" "$TMP_NOCURL/$cmd"
done
OUT="$(PATH="$TMP_NOCURL" "$TMP_NOCURL/bash" "$BOOTSTRAP" --apply 2>&1)"
RC=$?
if [[ $RC -eq 2 ]] && echo "$OUT" | grep -q "required tool not on PATH: curl"; then
    pass "metadata_missing_fails_cleanly"
else
    fail "metadata_missing_fails_cleanly (rc=$RC; expected 2 with 'required tool not on PATH: curl')"
fi

# ---------------------------------------------------------
# Test 4: bad mode arg fails with exit 2
# ---------------------------------------------------------
echo "test: bad_mode_arg_fails"
OUT="$(bash "$BOOTSTRAP" 2>&1)"
RC=$?
if [[ $RC -eq 2 ]] && echo "$OUT" | grep -q "must specify --dry-run or --apply"; then
    pass "bad_mode_arg_fails"
else
    fail "bad_mode_arg_fails (rc=$RC)"
fi

# ---------------------------------------------------------
# Optional network test: actual fetch + sha256 verify.
# Only runs when RUSH_BOOTSTRAP_TEST_NETWORK=1. Skipped
# in CI by default to avoid network flake.
# ---------------------------------------------------------
if [[ "${RUSH_BOOTSTRAP_TEST_NETWORK:-0}" == "1" ]]; then
    echo "test: apply_fetches_and_verifies_sha256 (network)"
    TMP_CACHE="$(mktemp -d)"
    if RUSH_GLOBAL_CLONE_CACHE="$TMP_CACHE" \
        bash "$BOOTSTRAP" --apply >/dev/null 2>&1; then
        if [[ -f "$TMP_CACHE/eigen_archive/Eigen/Core" ]] \
            && [[ -f "$TMP_CACHE/ml_dtypes/include/float8.h" ]]; then
            pass "apply_fetches_and_verifies_sha256"
        else
            fail "apply_fetches_and_verifies_sha256 (expected files not found)"
        fi
    else
        fail "apply_fetches_and_verifies_sha256 (apply failed)"
    fi
    rm -rf "$TMP_CACHE"
fi

# ---------------------------------------------------------
# Summary
# ---------------------------------------------------------

echo ""
echo "summary: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]] && exit 0 || exit 1
