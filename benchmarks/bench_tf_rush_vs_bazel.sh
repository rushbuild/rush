#!/usr/bin/env bash
# Reproducible rush-vs-Bazel TF build benchmark — a stable baseline for projections.
#
# Methodology follows the bazel-bench standard + Bazel's build-performance docs + Codex 019eebc6:
#   - Scenarios: CLEAN (build from wiped outputs, deps cached), NULL (no-op rebuild),
#     INCREMENTAL (one real 1-line source edit, rebuilt). These are the canonical build-perf metrics.
#   - N>=7 runs per scenario; report median + min/max + stddev (one outlier shouldn't move the number).
#   - Dedicated machine, no other load. wall + CPU + peak-RSS via /usr/bin/time -v.
#   - Equalize dep state: externals live OUTSIDE the source tree for BOTH tools, so we measure
#     build-graph EXECUTION, not the one-time fetch/bootstrap (which is each tool's separate concern).
#   - Bazel: --config=monolithic (= framework_shared_object=false, the STATIC closure — matches what
#     rush builds; the default dynamic :framework is NOT comparable). --remote_cache= disables TF's
#     public Google cache so Bazel actually BUILDS locally instead of downloading prebuilt artifacts.
#   - rush: --define framework_shared_object=false + --disk-cache false for the clean scenario.
#   - Incremental uses a real content EDIT (not `touch`) — Bazel hashes content, touch won't rebuild.
#
# GATE (run FIRST): action-shape parity. If Bazel and rush don't build a comparable closure
#   (CppCompile/CppLink action counts in the same ballpark), the timings are meaningless — STOP.
#
# Target: //tensorflow/cc:framework_gradients_test — a single tf_cc_test that forces compile+link of
#   a real binary over a large closure (all_kernels, framework, protos_all_cc, testlib, cc/ API),
#   avoiding the framework_shared_object asymmetry. Override with $TARGET.
#
# Usage:  TF=/path/to/tensorflow ./bench_tf_rush_vs_bazel.sh [gate|clean|null|incr|all]
set -uo pipefail

TARGET="${TARGET:-//tensorflow/cc:framework_gradients_test}"
TF="${TF:-/data/tensorflow}"
JOBS="${JOBS:-32}"
RUNS="${RUNS:-7}"
RUSH="${RUSH:-/data/rush/target/release/rush}"
BENCH_ROOT="${BENCH_ROOT:-/data/rush-bench}"
LOGS="${LOGS:-$BENCH_ROOT/logs}"
INCR_FILE="${INCR_FILE:-$TF/tensorflow/core/framework/attr_value_util.cc}"
mkdir -p "$LOGS"

# externals OUT of the tree for both tools (deps stay warm across `clean`)
BZL_STARTUP="--nohome_rc --nosystem_rc --output_base=$BENCH_ROOT/bazel-output-base"
BZL_BUILD="build --config=monolithic --remote_cache= --remote_upload_local_results=false \
  --repository_cache=$BENCH_ROOT/bazel-repo-cache --jobs=$JOBS --local_cpu_resources=$JOBS"
export RUSH_GLOBAL_CLONE_CACHE="${RUSH_GLOBAL_CLONE_CACHE:-/data/rush-cache/external}"
RUSH_DEF="--define framework_shared_object=false --define tflite_with_xnnpack=false \
  --define tensorflow_mkldnn_contraction_kernel=0 --define no_gcp_support=true \
  --define no_aws_support=true --define no_hdfs_support=true --define no_nccl_support=true"

# median/min/max/stddev of whitespace-separated seconds on stdin
stats() { awk '{a[NR]=$1; s+=$1} END{n=NR; if(!n){print "n=0"; exit} asort(a);
  m=(n%2)?a[(n+1)/2]:(a[n/2]+a[n/2+1])/2; mean=s/n; for(i=1;i<=n;i++)v+=(a[i]-mean)^2;
  printf "median=%.1f min=%.1f max=%.1f stddev=%.1f n=%d\n", m, a[1], a[n], sqrt(v/n), n}'; }

timed() { # $1=tag ; rest=cmd — prints wall seconds, logs full /usr/bin/time -v
  local tag="$1"; shift
  /usr/bin/time -v -o "$LOGS/$tag.time" "$@" >"$LOGS/$tag.log" 2>&1
  awk '/Elapsed .wall/{n=split($NF,a,":"); print (n==3)?a[1]*3600+a[2]*60+a[3]:a[1]*60+a[2]}' "$LOGS/$tag.time"
}

gate() { # action-shape parity — MUST pass before trusting timings
  echo "== GATE: action-shape parity for $TARGET =="
  ( cd "$TF" && bazel $BZL_STARTUP aquery "mnemonic(\"CppCompile|CppLink\", deps($TARGET))" $BZL_BUILD 2>/dev/null \
    | grep -cE "^action " ) | sed 's/^/bazel CppCompile+CppLink actions: /'
  echo "rush: compare against its build-report target/action count (build once with --build-report)"
}

bench_clean() { local tool=$1; local t s=(); for i in $(seq 1 "$RUNS"); do
  if [ "$tool" = bazel ]; then ( cd "$TF" && bazel $BZL_STARTUP clean >/dev/null 2>&1 )
    t=$(cd "$TF" && timed "bazel.clean.$i" bazel $BZL_STARTUP $BZL_BUILD "$TARGET")
  else "$RUSH" clean >/dev/null 2>&1
    t=$(cd "$TF" && timed "rush.clean.$i" "$RUSH" build $RUSH_DEF --jobs="$JOBS" --disk-cache false "$TARGET"); fi
  echo "  $tool clean run$i: ${t}s"; s+=("$t"); done
  printf '%s\n' "${s[@]}" | stats | sed "s/^/  $tool CLEAN /"; }

bench_null() { local tool=$1; local t s=()
  if [ "$tool" = bazel ]; then ( cd "$TF" && bazel $BZL_STARTUP $BZL_BUILD "$TARGET" >/dev/null 2>&1 )
  else ( cd "$TF" && "$RUSH" build $RUSH_DEF --jobs="$JOBS" "$TARGET" >/dev/null 2>&1 ); fi   # prime
  for i in $(seq 1 "$RUNS"); do
    if [ "$tool" = bazel ]; then t=$(cd "$TF" && timed "bazel.null.$i" bazel $BZL_STARTUP $BZL_BUILD "$TARGET")
    else t=$(cd "$TF" && timed "rush.null.$i" "$RUSH" build $RUSH_DEF --jobs="$JOBS" "$TARGET"); fi
    echo "  $tool null run$i: ${t}s"; s+=("$t"); done
  printf '%s\n' "${s[@]}" | stats | sed "s/^/  $tool NULL  /"; }

bench_incr() { local tool=$1; local t s=()
  ( cd "$TF" && { [ "$tool" = bazel ] && bazel $BZL_STARTUP $BZL_BUILD "$TARGET" || "$RUSH" build $RUSH_DEF --jobs="$JOBS" "$TARGET"; } >/dev/null 2>&1 )  # prime
  cp "$INCR_FILE" "$INCR_FILE.benchbak"
  for i in $(seq 1 "$RUNS"); do
    printf '\n// rush-bench incremental edit %d\n' "$i" >> "$INCR_FILE"   # real content change
    if [ "$tool" = bazel ]; then t=$(cd "$TF" && timed "bazel.incr.$i" bazel $BZL_STARTUP $BZL_BUILD "$TARGET")
    else t=$(cd "$TF" && timed "rush.incr.$i" "$RUSH" build $RUSH_DEF --jobs="$JOBS" "$TARGET"); fi
    echo "  $tool incr run$i: ${t}s"; s+=("$t"); done
  mv "$INCR_FILE.benchbak" "$INCR_FILE"   # restore
  printf '%s\n' "${s[@]}" | stats | sed "s/^/  $tool INCR  /"; }

echo "### rush-vs-Bazel TF benchmark — target=$TARGET jobs=$JOBS runs=$RUNS tree=$TF"
case "${1:-all}" in
  gate)  gate ;;
  clean) bench_clean bazel; bench_clean rush ;;
  null)  bench_null  bazel; bench_null  rush ;;
  incr)  bench_incr  bazel; bench_incr  rush ;;
  all)   gate; for s in clean null incr; do bench_$s bazel; bench_$s rush; done ;;
  *) echo "usage: $0 [gate|clean|null|incr|all]"; exit 1 ;;
esac
