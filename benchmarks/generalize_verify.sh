#!/usr/bin/env bash
# Generalize-verify: prove rush's sub-second daemon WARM no-op is GENERAL across
# target kinds — a deep CPU cc_library (//tensorflow/core:framework) and the full
# GPU cc_shared_library .so (//tensorflow:tensorflow_cc) — not .so-specific.
#
# For each target: prime once via the default daemon, then time REPS fresh-CLI
# warm no-ops and grep the daemon log for the graph-reuse + action-freshness-ledger
# (+ link-plan-cache, for the .so) markers. See benchmarks/gpu_so_noop_recipe.md.
#
# Sources the per-host GPU build env; ENV_FILE overrides the default path.
set -u
source "${ENV_FILE:-/data/tensorflow-pristine/.rush_gpu_build_env.sh}"
cd "$TF" || exit 1
REPS=${REPS:-3}
DEFS="$RUSH_GPU_DEFINES"
SO_DEFS="--define framework_shared_object=true $DEFS"
DLOG_DIR=/run/user/$(id -u)/rush-daemon

median() { printf '%s\n' "$@" | sort -g | awk '{a[NR]=$0} END{print (NR%2)? a[int(NR/2)+1] : (a[NR/2]+a[NR/2+1])/2}'; }
timeit() { local t0 t1; t0=$(date +%s.%N); "$@" >/tmp/gv_cmd.log 2>&1; local rc=$?; t1=$(date +%s.%N); awk "BEGIN{printf \"%.3f\", $t1-$t0}"; echo " rc=$rc" >&2; }
# newest daemon log mtime'd within this run
newest_dlog() { ls -t $DLOG_DIR/*/rushd.log 2>/dev/null | head -1; }

run_target() {
  local name="$1" target="$2" defs="$3"
  echo "######## $name : $target ########"
  echo "--- PRIME (cold/warm build) ---"
  timeout 7200 $RUSH build $target $defs --daemon=auto >/tmp/gv_prime_$name.log 2>&1
  echo "prime rc=$? (see /tmp/gv_prime_$name.log)"
  local DLOG; DLOG=$(newest_dlog)
  local mark0=0; [ -n "$DLOG" ] && mark0=$(wc -l < "$DLOG")
  local W=()
  for i in $(seq 1 "$REPS"); do
    local t; t=$(timeit timeout 900 $RUSH build $target $defs --daemon=auto)
    echo "  $name warm$i: ${t}s"
    W+=("$t")
  done
  local M; M=$(median "${W[@]}")
  echo ">>> $name WARM NO-OP MEDIAN=${M}s  reps=(${W[*]})"
  echo "--- daemon markers (new lines since prime, $DLOG) ---"
  if [ -n "$DLOG" ]; then
    tail -n +"$((mark0+1))" "$DLOG" | grep -E "Reusing analyzed graph \(manifest Fresh\)|action-freshness ledger CLEAN|skipping PASS-B|link-plan cache HIT" | sort | uniq -c | sed 's/^/  /'
  else
    echo "  (no daemon log found)"
  fi
  echo
}

CASE=${1:-all}
case "$CASE" in
  framework) run_target framework //tensorflow/core:framework "$DEFS";;
  so) run_target so //tensorflow:tensorflow_cc "$SO_DEFS";;
  all)
    run_target framework //tensorflow/core:framework "$DEFS"
    run_target so //tensorflow:tensorflow_cc "$SO_DEFS"
    ;;
  *) echo "usage: $0 framework|so|all" >&2; exit 1;;
esac
