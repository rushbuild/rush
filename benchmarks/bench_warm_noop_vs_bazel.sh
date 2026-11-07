#!/usr/bin/env bash
# Head-to-head TF-scale WARM NO-OP benchmark: rush persistent daemon vs Bazel.
#
# Measures the metric that matters for an incremental developer loop — the time
# to confirm "nothing to build" on an already-built tree — for both build
# systems on the SAME target and tree:
#
#   * rush: spawn/reuse the resident `rushd`, do one cold build to populate the
#     daemon's analyzed-graph snapshot + Phase-2 action-freshness ledger, then
#     time N fresh-CLI no-op rebuilds that hit the daemon (reuse + ledger
#     fast-path: skip .bzl/BUILD/PASS-A AND skip the PASS-B wavefront).
#   * Bazel: warm up the resident server once, then time N no-op rebuilds.
#
# This is the harness behind the "rush beats Bazel's TF-scale warm no-op"
# result. It writes ONLY to the daemon socket dir and /tmp logs — never the TF
# source tree or the rush external cache.
#
# Env knobs (all optional, with TF-2.17 defaults matching the project setup):
#   RUSH_BIN       path to the rush binary           (default: rush on PATH)
#   TF_DIR         workspace root to build in         (default: /data/tensorflow-2.17)
#   TARGET         target pattern                      (default: //tensorflow/core:framework)
#   REPS           number of warm no-op reps           (default: 3)
#   RUSH_DEFINES   extra --define flags for rush       (default: the TF-2.17 set)
#   SKIP_BAZEL=1   skip the Bazel half (rush only)
set -u

RUSH=${RUSH_BIN:-rush}
TF=${TF_DIR:-/data/tensorflow-2.17}
TARGET=${TARGET:-//tensorflow/core:framework}
REPS=${REPS:-3}
DEFINES=${RUSH_DEFINES:-"--define framework_shared_object=false --define tflite_with_xnnpack=false --define tensorflow_mkldnn_contraction_kernel=0 --define no_gcp_support=true --define __TENSORFLOW_NO_LGPL_DEPS__=1 --define no_aws_support=true --define no_hdfs_support=true --define no_nccl_support=true"}

cd "$TF" || { echo "no such workspace: $TF" >&2; exit 1; }
echo "=== warm-no-op head-to-head: $TARGET in $TF (REPS=$REPS) ==="

timeit() { # label cmd...
  local label="$1"; shift
  local t0 t1 rc dt
  t0=$(date +%s.%N); "$@" >/tmp/bench_noop_cmd.log 2>&1; rc=$?; t1=$(date +%s.%N)
  dt=$(awk "BEGIN{printf \"%.3f\", $t1-$t0}")
  echo "[$label] ${dt}s rc=$rc" >&2
  echo "$dt"
}
median() { printf '%s\n' "$@" | sort -g | awk '{a[NR]=$0} END{print (NR%2)? a[int(NR/2)+1] : (a[NR/2]+a[NR/2+1])/2}'; }

# ── rush ────────────────────────────────────────────────────────────────────
pkill -f 'rush daemon' 2>/dev/null
rm -f /run/user/$(id -u)/rush-daemon/*/rushd.sock 2>/dev/null
sleep 1
echo "--- rush COLD via daemon (populate snapshot + freshness ledger) ---"
timeout 7200 $RUSH build $TARGET $DEFINES --daemon=auto >/tmp/bench_rush_cold.log 2>&1
echo "rush cold rc=$?"
echo "--- rush WARM no-op x$REPS (fresh process, daemon reuse + ledger fast-path) ---"
RW=()
for i in $(seq 1 "$REPS"); do
  RW+=("$(timeit "rush-warm-$i" timeout 600 $RUSH build $TARGET $DEFINES --daemon=auto)")
done
RMED=$(median "${RW[@]}")
echo "rush warm no-op: ${RW[*]}  MEDIAN=${RMED}s"

# ── Bazel ─────────────────────────────────────────────────────────────────────
if [ "${SKIP_BAZEL:-0}" = "1" ]; then echo "RESULT rush=${RMED}s (Bazel skipped)"; exit 0; fi
echo "--- bazel warm-up build (once) ---"
timeout 7200 bazel build $TARGET >/tmp/bench_bazel_warmup.log 2>&1
brc=$?
echo "bazel warmup rc=$brc"
if [ $brc -ne 0 ]; then
  echo "Bazel could not build this tree; reporting rush only."
  echo "RESULT rush=${RMED}s (Bazel build failed)"
  exit 0
fi
echo "--- bazel no-op rebuild x$REPS ---"
BW=()
for i in $(seq 1 "$REPS"); do
  BW+=("$(timeit "bazel-noop-$i" timeout 600 bazel build $TARGET)")
done
BMED=$(median "${BW[@]}")
echo "bazel no-op: ${BW[*]}  MEDIAN=${BMED}s"

echo "============================================================"
echo "RESULT  rush warm no-op median = ${RMED}s   bazel no-op median = ${BMED}s"
awk "BEGIN{r=$RMED; b=$BMED; if(r<b) printf \"rush BEATS Bazel by %.2fx\\n\", b/r; else if(r>b) printf \"rush is %.2fx slower than Bazel\\n\", r/b; else print \"parity\"}"
