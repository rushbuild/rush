#!/bin/bash
# Shared benchmark preconditions for rush-vs-rush and rush-vs-Bazel
# measurement.
#
# Written after the 2026-08-22 external review, which found that the
# previous harness produced numbers nobody could defend:
#
#   * "cold" cleared $WORKSPACE/.cache/rush, which holds no cache
#     entries — the action cache and CAS live under $HOME. With real
#     content-addressed restoration in place a "cold" run silently
#     becomes a repeat-cold run.
#   * A/B arms appended IDENTICAL text to the touched source, so the
#     second arm hit the shared action cache for the same action id and
#     looked ~32% faster while doing strictly less work.
#   * Headline numbers were published from commit messages with no
#     receipt on disk.
#
# The follow-up review corrected one rule of the first version. "Arms
# that executed different action counts cannot be compared" is right
# for a MECHANISM benchmark and wrong for an END-TO-END one: when early
# cutoff is the thing under test, doing less work IS the result. So
# there are two benchmark classes with different gates:
#
#   MECHANISM   isolates one code path. Both arms must do the SAME
#               work; a differing MISS count invalidates the comparison.
#               bench_assert_equal_work
#
#   END-TO-END  measures what a developer waits for after a real edit.
#               Arms may legitimately execute different action counts;
#               what must match is the RESULT. Report the action
#               counts alongside the times, never hide them.
#               bench_assert_same_outcome + bench_report_work
#
# And prefer isolating each arm's CACHE over perturbing its SOURCE:
# giving two arms different mutation text to dodge cache aliasing also
# gives them different work. Same bytes, different cache root.
#
# Every function fails loudly rather than producing a number that
# cannot be trusted. Source it; do not copy it.

set -u

# ── cache location: ask rush, never reconstruct ─────────────────────────
rush_cache_root() { "$RUSH" info --path cache-root; }
rush_cas_dir()    { "$RUSH" info --path cas-dir; }

# A private cache root for one A/B arm. Lets both arms apply the
# IDENTICAL mutation without aliasing on a shared action id.
bench_arm_cache() { # arm_name -> prints a root to export as RUSH_CACHE_ROOT
  local base=${BENCH_CACHE_BASE:-${TMPDIR:-/tmp}/rush-bench-caches}
  local root="$base/$1"
  mkdir -p "$root"
  printf '%s' "$root"
}

# Make a build genuinely cold: remove the outputs AND everything rush
# would restore them from. Verifies afterwards.
bench_make_cold() { # workspace_out_dir
  local out=$1 root cas
  root=$(rush_cache_root) || { echo "FATAL: rush info failed"; return 90; }
  case "$root" in
    /|/home|/home/*/|"") echo "FATAL: refusing to clear '$root'"; return 90;;
  esac
  rm -rf "$out"
  rm -rf "${root:?}/cas"
  find "$root" -maxdepth 1 -name '*.cache' -delete 2>/dev/null
  cas=$(rush_cas_dir)
  local left
  left=$(find "$root" -maxdepth 1 -name '*.cache' 2>/dev/null | wc -l)
  if [ "$left" -ne 0 ] || [ -d "$cas" ]; then
    echo "FATAL: cache not clear after cold prep (entries=$left cas=$cas)"
    return 90
  fi
  return 0
}

bench_assert_cache() { # expected: empty|populated
  local want=$1 root n cas_n
  root=$(rush_cache_root)
  n=$(find "$root" -maxdepth 1 -name '*.cache' 2>/dev/null | wc -l)
  cas_n=$(find "$root/cas" -type f 2>/dev/null | wc -l)
  case "$want" in
    empty)
      [ "$n" -eq 0 ] && [ "$cas_n" -eq 0 ] && return 0
      echo "FATAL: expected empty cache, found ${n} entries / ${cas_n} blobs"
      return 91;;
    populated)
      [ "$n" -gt 0 ] && return 0
      echo "FATAL: expected a populated cache, found ${n} entries"
      return 91;;
  esac
}

# A daemon spawned by an earlier run keeps THAT run's environment for
# nearly every knob, so a stale one silently applies the previous
# configuration's toolchain. Observed on 2026-08-23: a sweep kept
# linking with a linker the operator had already removed from the
# command, because the daemon still had it. Never assume; verify.
bench_require_fresh_daemon() {
  pkill -f "rush daemon" >/dev/null 2>&1
  sleep 2
  if pgrep -f "rush daemon" >/dev/null 2>&1; then
    echo "FATAL: a rush daemon survived the kill; it would apply its own"
    echo "       environment to this run rather than yours"
    return 94
  fi
  return 0
}
bench_stop_daemon() { bench_require_fresh_daemon; }

# ── mutation ────────────────────────────────────────────────────────────

# The SAME bytes for every arm. Uniqueness across RUNS (so a later run
# does not hit an earlier run's cache) comes from the token argument,
# which the caller keeps identical between arms of one comparison.
bench_touch() { # file token
  printf '\n# bench %s\n' "$2" >> "$1"
}

bench_run_token() { printf 'run-%s-%s' "$(date +%s%N)" "$$"; }

# ── work accounting ─────────────────────────────────────────────────────

bench_action_counts() { # tracefile -> "HIT=n MISS=n VIRTUAL=n RESTORE=n"
  awk '{c[$1]++} END {printf "HIT=%d MISS=%d VIRTUAL=%d RESTORE=%d",
        c["HIT"], c["MISS"], c["HIT-VIRTUAL"], c["HIT-CAS-RESTORE"]}' "$1"
}

bench_miss_count() { awk '$1=="MISS"{n++} END{print n+0}' "$1"; }

# MECHANISM gate: the arms must have done the same work, or the
# comparison is measuring the workload rather than the mechanism.
bench_assert_equal_work() { # traceA traceB
  local a b
  a=$(bench_miss_count "$1")
  b=$(bench_miss_count "$2")
  if [ "$a" != "$b" ]; then
    echo "FATAL: mechanism benchmark arms did different work — MISS $a vs $b."
    echo "       Either the arms are not isolated, or this is an end-to-end"
    echo "       comparison and should use bench_assert_same_outcome."
    return 92
  fi
  return 0
}

# END-TO-END gate: arms MAY execute different action counts — that is
# often the result being measured. What must match is the artifact the
# developer ends up with.
bench_assert_same_outcome() { # fileA fileB [label]
  local a b
  a=$(sha256sum "$1" 2>/dev/null | cut -d' ' -f1)
  b=$(sha256sum "$2" 2>/dev/null | cut -d' ' -f1)
  if [ -z "$a" ] || [ "$a" != "$b" ]; then
    echo "FATAL: end-to-end arms produced different results for ${3:-output}"
    echo "       $1 -> ${a:-missing}"
    echo "       $2 -> ${b:-missing}"
    return 92
  fi
  return 0
}

# Always publish the work each arm did next to the time it took, so a
# reader can see whether a win came from speed or from skipping.
bench_report_work() { # armA_name traceA armB_name traceB
  printf '  work: %-12s %s\n' "$1" "$(bench_action_counts "$2")"
  printf '  work: %-12s %s\n' "$3" "$(bench_action_counts "$4")"
}

# ── receipts ────────────────────────────────────────────────────────────

bench_receipt() { # csv_path row...
  local csv=$1; shift
  printf '%s\n' "$*" >> "$csv"
  [ -s "$csv" ] || { echo "FATAL: receipt $csv not written"; return 93; }
}

bench_require_receipt() { # path
  [ -s "$1" ] || { echo "FATAL: refusing to report without receipt $1"; return 93; }
  echo "receipt: $1 ($(wc -l < "$1") rows)"
}
