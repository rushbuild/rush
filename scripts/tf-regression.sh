#!/usr/bin/env bash
#
# rush TF-corpus regression runner (see docs/ci/tf-regression-strategy.md).
#
#   scripts/tf-regression.sh --tier t0      # cargo test + 1 smoke TF cc_test
#   scripts/tf-regression.sh --tier t1      # sentinel suite (one multi-root PASS-A)
#   scripts/tf-regression.sh --tier t2      # full corpus sweep + ratchet diff
#
# Env:
#   RUSH        rush binary           (default: target/release/rush)
#   TF_WS       TensorFlow workspace  (default: /data/tensorflow)
#   RUSH_REPO   rush checkout         (default: dir containing this script's ..)
#
# T1 is the per-merge gate: every sentinel in ci/sentinel_manifest.toml is a
# witness for a structural subsystem (one multi-root build = one PASS-A, the
# c321 win); a regression in any covered subsystem flips its witness red. All
# sentinels are in ci/expected_pass.txt, so T1 passes iff none regressed.
set -uo pipefail

TIER=""
while [ $# -gt 0 ]; do
  case "$1" in
    --tier) TIER="$2"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done
[ -n "$TIER" ] || { echo "usage: $0 --tier {t0,t1,t2}" >&2; exit 2; }

REPO="${RUSH_REPO:-$(cd "$(dirname "$0")/.." && pwd)}"
RUSH="${RUSH:-$REPO/target/release/rush}"
TF_WS="${TF_WS:-/data/tensorflow}"
MANIFEST="$REPO/ci/sentinel_manifest.toml"
EXPECTED="$REPO/ci/expected_pass.txt"
RUN_TO="${RUN_TO:-300}"          # honor Bazel timeout=moderate, not 120
# Heavy sentinels (context_test's full eager closure) need a generous build
# budget on a COLD closure. The design (docs/ci/tf-regression-strategy.md)
# runs CI on the canonical WARM checkout where the closure is already built,
# so this ceiling is only hit on a cold first run.
BUILD_TO="${BUILD_TO:-5400}"

# bazel test env (runfiles) — mirrors rscale_batch.sh
mkdir -p /tmp/rush_runfiles /tmp/rush_test_tmp
ln -sfn "$TF_WS" /tmp/rush_runfiles/org_tensorflow
export TEST_WORKSPACE=org_tensorflow TEST_SRCDIR=/tmp/rush_runfiles \
       RUNFILES_DIR=/tmp/rush_runfiles TEST_TMPDIR=/tmp/rush_test_tmp
# tensorflow_mkldnn_contraction_kernel=0: rush has no oneDNN (@onednn//:mkl_dnn),
# so the supported no-oneDNN config flips the Eigen contraction select() to the
# no_mkl branch (no TENSORFLOW_USE_MKLDNN_CONTRACTION_KERNEL define -> no dnnl_*
# undefined). Required now that transitive `defines` propagate into consumer
# compiles. RUSH_DEFINES splits on ',' so this survives the unquoted $RB.
RB="env RUSH_DEFINES=tflite_with_xnnpack=false,tensorflow_mkldnn_contraction_kernel=0 RUSH_LINKER=mold"

sentinel_labels() { grep -E '^label[[:space:]]*=' "$MANIFEST" | sed -E 's/.*"([^"]+)".*/\1/'; }

# Build a space-separated label list in ONE rush invocation (single PASS-A),
# then run each produced binary. Echoes "<label> <PASS|FAIL|NOBIN>" per line.
build_and_run() {
  local labels="$*" rc=0
  ( cd "$TF_WS" && timeout "$BUILD_TO" $RB "$RUSH" build $labels --include-tests ) >/tmp/tfreg_build.log 2>&1 || rc=$?
  for L in $labels; do
    local t="${L##*:}" bin="$TF_WS/rush-out/${L##*:}"
    if [ -x "$bin" ]; then
      if ( cd "$TF_WS" && timeout "$RUN_TO" "$bin" >/dev/null 2>&1 </dev/null ); then
        echo "$L PASS"
      else
        echo "$L FAIL"
      fi
    else
      echo "$L NOBIN"
    fi
  done
}

case "$TIER" in
  t0)
    echo "== T0: cargo test (analysis layer, lock-free) =="
    ( cd "$REPO" && cargo test --workspace --quiet ) || exit 1
    echo "== T0: smoke tf_cc_test =="
    r=$(build_and_run "//tensorflow/core/platform:enable_tf2_utils_test")
    echo "$r"; echo "$r" | grep -q " PASS$" || { echo "T0 FAIL"; exit 1; }
    echo "T0 PASS"
    ;;
  t1)
    echo "== T1: sentinel suite (one multi-root PASS-A) =="
    mapfile -t LBLS < <(sentinel_labels)
    [ "${#LBLS[@]}" -gt 0 ] || { echo "no sentinels parsed from $MANIFEST" >&2; exit 2; }
    res=$(build_and_run "${LBLS[@]}")
    echo "$res"
    fails=0
    while read -r L st; do
      if [ "$st" != "PASS" ]; then
        # only a regression if the label is in the green baseline
        if grep -qxF "$L" "$EXPECTED"; then
          echo "REGRESSION: $L ($st) was green in ci/expected_pass.txt" >&2
          fails=$((fails+1))
        fi
      fi
    done <<< "$res"
    [ "$fails" -eq 0 ] && { echo "T1 PASS (${#LBLS[@]} sentinels green)"; } || { echo "T1 FAIL ($fails regressions)"; exit 1; }
    ;;
  t2)
    echo "== T2: full corpus sweep + ratchet diff vs ci/expected_pass.txt =="
    echo "T2 drives the full 466-label sweep (rscale_batch.sh lineage), then:"
    echo "  comm -13 <(sort expected_pass) <(sort new_pass)  -> NEW GREEN (promote after 3/3)"
    echo "  comm -23 <(sort expected_pass) <(sort new_pass)  -> REGRESSIONS (alarm)"
    echo "Run the sweep harness (/data/rscale/rscale_batch.sh), then diff its"
    echo "passing set against $EXPECTED. Wired as the nightly cron job."
    exit 0
    ;;
  *) echo "unknown tier: $TIER" >&2; exit 2 ;;
esac
