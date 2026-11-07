#!/usr/bin/env bash
# Daemon Phase 0 — analysis read-auditor.
#
# Records every file/exec syscall a (warm, no-op) `rush build` makes via strace, then categorizes
# them. This is the GROUND-TRUTH dependency manifest the persistent daemon must invalidate on, and
# the empirical check for Codex's P0 risk #2 (untracked filesystem probes that shape the graph but
# aren't recorded as analysis dependencies). See docs/design/persistent-daemon-skyframe.md.
#
# NOTE: strace sees file/exec syscalls but NOT in-process getenv() — env-read tracking needs a
# separate mechanism (LD_PRELOAD or code instrumentation). This audits the FS/exec surface.
#
# Usage: RUSH_BIN=... TF_DIR=... audit_analysis_reads.sh <target> [-- <build defines...>]
set -uo pipefail

TARGET="${1:?usage: audit_analysis_reads.sh <target> [-- <defines>]}"; shift || true
[ "${1:-}" = "--" ] && shift
DEFS=("$@")

RUSH_BIN="${RUSH_BIN:-rush}"
TF_DIR="${TF_DIR:-$PWD}"
TRACE="${TRACE:-/tmp/rush_analysis_strace.log}"
OUT="${OUT:-/tmp/rush_analysis_manifest.txt}"

cd "$TF_DIR" || exit 1

echo "== priming (untraced) =="
"$RUSH_BIN" build "$TARGET" "${DEFS[@]}" >/dev/null 2>&1

echo "== tracing no-op build under strace =="
strace -f -y -e trace=%file,execve -o "$TRACE" \
    "$RUSH_BIN" build "$TARGET" "${DEFS[@]}" >/dev/null 2>&1

# Extract successfully-opened/statted absolute paths (drop ENOENT negative probes into their own
# bucket — those are package-boundary / glob / existence checks, which are ALSO analysis deps).
ws="$(pwd)"
: > "$OUT"
{
  echo "# rush analysis read manifest — target=$TARGET  ($(wc -l < "$TRACE") syscalls traced)"
  echo

  echo "## subprocesses exec'd (should be ZERO bazel after the warmup-skip fix):"
  grep -aE 'execve\(' "$TRACE" | sed -E 's/.*execve\("([^"]+)".*/\1/' | sort | uniq -c | sort -rn | head -20
  echo

  echo "## file reads by category (successful opens/stats):"
  # paths that resolved (no '= -1' error) — accept open/openat/stat/lstat/newfstatat
  reads="$(grep -aE '(openat?|stat|lstat|newfstatat|readlink)\(' "$TRACE" \
            | grep -avE '= -1' \
            | grep -aoE '"(/[^"]+)"' | tr -d '"' | sort -u)"
  total=$(printf '%s\n' "$reads" | grep -c .)
  echo "  TOTAL distinct paths read: $total"
  printf '%s\n' "$reads" | grep -cE '/BUILD(\.bazel)?$'     | xargs echo "  BUILD files:        "
  printf '%s\n' "$reads" | grep -cE '\.bzl$'                | xargs echo "  .bzl files:         "
  printf '%s\n' "$reads" | grep -cE '\.(cc|cpp|cxx|h|hpp|c)$' | xargs echo "  C/C++ sources/hdrs: "
  printf '%s\n' "$reads" | grep -cE 'external/'             | xargs echo "  under external/:    "
  printf '%s\n' "$reads" | grep -vcE '^(/usr|/lib|/proc|/sys|/etc|/dev)' | xargs echo "  non-system paths:   "
  echo

  echo "## negative existence probes (ENOENT — glob/package-boundary/existence deps, MUST be tracked):"
  neg="$(grep -aE '(openat?|stat|lstat|newfstatat)\(' "$TRACE" | grep -aE '= -1 ENOENT' \
          | grep -aoE '"(/[^"]+)"' | tr -d '"' | sort -u | grep -vE '^(/usr|/lib|/proc|/sys|/etc|/dev)')"
  echo "  TOTAL workspace-relevant ENOENT probes: $(printf '%s\n' "$neg" | grep -c .)"
  printf '%s\n' "$neg" | grep -E "$ws|external" | head -25 | sed 's/^/    /'
  echo

  echo "## getdents (directory listings — glob() deps; raw fd-resolved by strace -y):"
  grep -aE 'getdents' "$TRACE" | grep -aoE '<[^>]+>' | tr -d '<>' | sort -u \
    | grep -E "$ws|external" | head -25 | sed 's/^/    /'
} | tee "$OUT"

echo
echo "manifest written: $OUT  (full trace: $TRACE)"
