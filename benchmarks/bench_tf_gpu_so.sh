#!/usr/bin/env bash
# rush-vs-Bazel head-to-head on //tensorflow:libtensorflow_cc.so (the GPU-enabled
# full TF C++ runtime shared lib). Three metrics, median of N runs each:
#   CLEAN       — wiped outputs, full-throughput build of the .so (headline)
#   INCREMENTAL — touch one .cc in the .so closure, rebuild
#   NOOP        — rebuild, nothing changed (warm)
#
# Both tools build the SAME target with the SAME --define set; deps stay warm
# OUTSIDE the source tree so we measure build-graph EXECUTION, not one-time fetch.
# rush uses its persistent daemon by default. Bazel uses its own server + repo cache.
set -uo pipefail

TF="${TF:-/data/tensorflow-pristine}"
TARGET_RUSH="//tensorflow:tensorflow_cc"
TARGET_BZL="//tensorflow:libtensorflow_cc.so"
RUNS="${RUNS:-3}"
JOBS="${JOBS:-32}"
RUSH="${RUSH:-/data/rush/target/release/rush}"
BENCH_ROOT="${BENCH_ROOT:-/data/rush-bench-so}"
LOGS="$BENCH_ROOT/logs"
INCR_FILE="${INCR_FILE:-$TF/tensorflow/core/framework/attr_value_util.cc}"
mkdir -p "$LOGS"

DEFS="--define framework_shared_object=true --define tflite_with_xnnpack=false \
--define tflite_kernel_use_xnnpack=false --define tensorflow_mkldnn_contraction_kernel=0 \
--define no_gcp_support=true --define __TENSORFLOW_NO_LGPL_DEPS__=1 \
--define no_aws_support=true --define no_hdfs_support=true --define no_nccl_support=true"

export RUSH_GLOBAL_CLONE_CACHE=/data/rush-cache-217/external
export RUSH_ENABLE_TF217_WORKSPACE_FACADES=1
export RUSH_ENABLE_ROCM=1
export RUSH_GPU_GCC_INSTALL_DIR=/usr/lib/gcc/x86_64-linux-gnu/12
export PATH=/data/rush-cache-217/external/flatbuffers_flatc:$PATH
export CPLUS_INCLUDE_PATH=/data/rush-cache-217/external/flatbuffers_flatc/include
export RUSH_LINKER=mold

BZL_STARTUP="--nohome_rc --nosystem_rc --output_base=$BENCH_ROOT/bazel-output-base"
BZL_BUILD="build --remote_cache= --remote_upload_local_results=false \
--repository_cache=$BENCH_ROOT/bazel-repo-cache --jobs=$JOBS --local_cpu_resources=$JOBS $DEFS"

stats() { awk '{a[NR]=$1; s+=$1} END{n=NR; if(!n){print "n=0"; exit} asort(a);
  m=(n%2)?a[(n+1)/2]:(a[n/2]+a[n/2+1])/2; mean=s/n; for(i=1;i<=n;i++)v+=(a[i]-mean)^2;
  printf "median=%.1fs min=%.1fs max=%.1fs stddev=%.1f n=%d\n", m,a[1],a[n],sqrt(v/n),n}'; }

walltime() { /usr/bin/time -v -o "$LOGS/$1.time" "${@:2}" >"$LOGS/$1.log" 2>&1
  awk '/Elapsed .wall/{n=split($NF,a,":"); print (n==3)?a[1]*3600+a[2]*60+a[3]:a[1]*60+a[2]}' "$LOGS/$1.time"; }

rush_build() { ( cd "$TF" && "$RUSH" build $DEFS "$TARGET_RUSH" ); }
bzl_build()  { ( cd "$TF" && bazel $BZL_STARTUP $BZL_BUILD "$TARGET_BZL" ); }

run_rush() { local mode=$1 s=() t
  case $mode in
    clean) "$RUSH" clean >/dev/null 2>&1; rm -rf "$TF/rush-out" 2>/dev/null;;
    noop|incr) rush_build >/dev/null 2>&1;;   # prime
  esac
  [ "$mode" = incr ] && cp "$INCR_FILE" "$INCR_FILE.bak"
  for i in $(seq 1 "$RUNS"); do
    [ "$mode" = clean ] && { "$RUSH" clean >/dev/null 2>&1; rm -rf "$TF/rush-out" 2>/dev/null; }
    [ "$mode" = incr ] && printf '\n// rush-bench %d\n' "$i" >> "$INCR_FILE"
    t=$(cd "$TF" && walltime "rush.$mode.$i" "$RUSH" build $DEFS "$TARGET_RUSH")
    echo "  rush $mode run$i: ${t}s"; s+=("$t")
  done
  [ "$mode" = incr ] && mv "$INCR_FILE.bak" "$INCR_FILE"
  printf '%s\n' "${s[@]}" | stats | sed "s/^/  rush $mode  /"
}

run_bzl() { local mode=$1 s=() t
  case $mode in
    clean) ( cd "$TF" && bazel $BZL_STARTUP clean >/dev/null 2>&1 );;
    noop|incr) bzl_build >/dev/null 2>&1;;
  esac
  [ "$mode" = incr ] && cp "$INCR_FILE" "$INCR_FILE.bak"
  for i in $(seq 1 "$RUNS"); do
    [ "$mode" = clean ] && ( cd "$TF" && bazel $BZL_STARTUP clean >/dev/null 2>&1 )
    [ "$mode" = incr ] && printf '\n// rush-bench %d\n' "$i" >> "$INCR_FILE"
    t=$(cd "$TF" && walltime "bzl.$mode.$i" bazel $BZL_STARTUP $BZL_BUILD "$TARGET_BZL")
    echo "  bazel $mode run$i: ${t}s"; s+=("$t")
  done
  [ "$mode" = incr ] && mv "$INCR_FILE.bak" "$INCR_FILE"
  printf '%s\n' "${s[@]}" | stats | sed "s/^/  bazel $mode  /"
}

echo "### .so head-to-head  target=$TARGET_RUSH  runs=$RUNS  jobs=$JOBS  tree=$TF"
case "${1:-all}" in
  rush-clean) run_rush clean;; rush-incr) run_rush incr;; rush-noop) run_rush noop;;
  bzl-clean) run_bzl clean;;  bzl-incr) run_bzl incr;;  bzl-noop) run_bzl noop;;
  *) echo usage: $0 'rush-clean|rush-incr|rush-noop|bzl-clean|bzl-incr|bzl-noop'; exit 1;;
esac
