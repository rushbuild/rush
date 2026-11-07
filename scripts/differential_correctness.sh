#!/usr/bin/env bash
# differential_correctness.sh — roadmap P1 mutation-matrix harness for rush.
#
# Proves rush's INCREMENTAL correctness (not just clean-build correctness):
# captures the rush action graph (--aquery / RUSH_AQUERY_OUT) and the built
# output digests, applies a mutation, rebuilds, and asserts the right things
# changed and nothing else. "463/463 passing" only proves clean builds; this
# probes the actual Bazel contract.
#
# Usage: differential_correctness.sh <workspace_dir> <target> <mutate_src> <bin_rel>
#   workspace_dir : abs path to a rush workspace (has BUILD/WORKSPACE)
#   target        : label to build, e.g. //:hello
#   mutate_src    : a source file under workspace to edit, e.g. hello_lib.cc
#   bin_rel       : built binary path rel to workspace, e.g. rush-out/hello
#
# Env: RUSH=/data/rush/target/release/rush (override as needed)
set -u
RUSH="${RUSH:-/data/rush/target/release/rush}"
WS="$1"; TARGET="$2"; MUT="$3"; BIN="$4"
TMP="$(mktemp -d)"; PASS=0; FAIL=0
# Unique-per-run edit marker so an edit's content NEVER collides with a prior
# run's content-keyed cache entry (rush's action cache keys on source content;
# a fixed marker made repeat runs hit the previous run's cached object and
# falsely look like a stale-cache under-build).
MARK="DIFFTEST_$$_${RANDOM}"
cd "$WS" || exit 2

# Normalize: strip the absolute workspace prefix so the graph is comparable
# across checkouts / output roots.
norm() { sed "s#${WS}/##g; s#${WS}##g"; }
aquery() { RUSH_AQUERY_OUT="$TMP/$1" "$RUSH" build "$TARGET" --analyze-only >/dev/null 2>&1; norm < "$TMP/$1" > "$TMP/$1.norm"; }
digest() { [ -f "$BIN" ] && md5sum "$BIN" | cut -d' ' -f1 || echo MISSING; }
check() { if [ "$2" = "$3" ]; then echo "  PASS: $1"; PASS=$((PASS+1)); else echo "  FAIL: $1 ($2 != $3)"; FAIL=$((FAIL+1)); fi; }
checkne() { if [ "$2" != "$3" ]; then echo "  PASS: $1"; PASS=$((PASS+1)); else echo "  FAIL: $1 (both $2)"; FAIL=$((FAIL+1)); fi; }

cp "$MUT" "$TMP/mut.orig"
echo "=== differential_correctness: $TARGET (mutate $MUT) ==="

# --- M0: clean build + baseline graph/digest ---
rm -rf rush-out 2>/dev/null
"$RUSH" build "$TARGET" >/dev/null 2>&1
D_CLEAN="$(digest)"; aquery g_clean
echo "clean: digest=$D_CLEAN, $(grep -c '"mnemonic"' "$TMP/g_clean.norm") actions"

# --- M0a: no-op aquery determinism (graph byte-identical) ---
aquery g_noop
check "no-op: action graph byte-identical" "$(md5sum < "$TMP/g_clean.norm")" "$(md5sum < "$TMP/g_noop.norm")"

# --- M0b: no-op rebuild output-digest stability ---
"$RUSH" build "$TARGET" >/dev/null 2>&1
check "no-op: output digest stable" "$D_CLEAN" "$(digest)"

# --- M1: edit one source (unique marker per run; see MARK) ---
# Capture the per-action HIT/MISS trace so we can assert MINIMALITY: editing one
# source must re-execute only the dependent actions; unrelated actions must HIT.
sed -i "s/Hello/$MARK/g" "$MUT" 2>/dev/null
RUSH_ACTION_TRACE="$TMP/trace" "$RUSH" build "$TARGET" >/dev/null 2>&1
D_EDIT="$(digest)"; aquery g_edit
T_HIT=$(grep -c '^HIT ' "$TMP/trace" 2>/dev/null || echo 0)
T_MISS=$(grep -c '^MISS ' "$TMP/trace" 2>/dev/null || echo 0)
echo "edit action trace: $T_MISS executed (MISS), $T_HIT cached (HIT)"
checkne "minimality: NOT a full rebuild (>=1 action cache-HIT)" "$T_HIT" "0"
checkne "minimality: the edit DID rebuild (>=1 action MISS)" "$T_MISS" "0"
checkne "edit: output digest changed (rebuild reflects edit)" "$D_CLEAN" "$D_EDIT"
check "edit: action SET unchanged (same mnemonics+outputs)" \
  "$(python3 -c "import json,sys;d=json.load(open(sys.argv[1]));print(sorted((a['mnemonic'],tuple(a['outputs'])) for a in d['actions']))" "$TMP/g_clean")" \
  "$(python3 -c "import json,sys;d=json.load(open(sys.argv[1]));print(sorted((a['mnemonic'],tuple(a['outputs'])) for a in d['actions']))" "$TMP/g_edit")"

# --- M1b: incremental(edit) output == clean rebuild from scratch (correctness) ---
D_INC="$(digest)"
rm -rf rush-out 2>/dev/null
"$RUSH" build "$TARGET" >/dev/null 2>&1
check "edit: incremental output == clean-from-scratch output" "$D_INC" "$(digest)"

# restore
cp "$TMP/mut.orig" "$MUT"; rm -rf rush-out 2>/dev/null; "$RUSH" build "$TARGET" >/dev/null 2>&1
check "restore: output digest back to baseline" "$D_CLEAN" "$(digest)"

echo "=== RESULT: $PASS pass / $FAIL fail ==="
rm -rf "$TMP"
exit $([ "$FAIL" -eq 0 ] && echo 0 || echo 1)
