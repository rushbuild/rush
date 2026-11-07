#!/usr/bin/env bash
# Daemon migration gauge — runtime untracked-read measurer.
#
# The source-line lint (lint_untracked_reads.sh) counts raw fs/env reads in the
# analysis crates but cannot tell an analysis read from an executor/setup read.
# The TRUE remaining-work surface for safe daemon reuse is a RUNTIME measure:
#
#     UNTRACKED = (every file the build process actually read, via strace)
#               - (every path the tracked-IO manifest recorded)
#
# Any path in that difference is a dependency the analysis consumed but did NOT
# record — a future wrong-build hole if the daemon reuses a retained graph.
#
# This script:
#   1. primes the target (untraced warm build),
#   2. runs the build once under strace AND with the tracked-IO path dump on
#      (RUSH_TRACKED_IO_AUDIT=1 + RUSH_TRACKED_IO_AUDIT_PATHS=<file>),
#   3. diffs strace-reads MINUS manifest-paths,
#   4. categorizes the untracked set by prefix and prints the gauge.
#
# Categories:
#   workspace-source  — under $TF_DIR but NOT rush-out/  (GENUINE migration targets)
#   external          — under $EXT_DIR                   (external-repo analysis reads)
#   output-tree       — rush-out/                        (daemon's own writes; EXCLUDE)
#   system            — /usr /lib /lib64 /proc /sys /etc /dev /run
#   noise             — the rush binary / cargo / rustup / toolchain / home dotdirs
#
# Usage:
#   RUSH_BIN=... TF_DIR=... EXT_DIR=... untracked_read_gauge.sh <target> [-- <defines>]
#
# See docs/design/persistent-daemon-skyframe.md.

set -uo pipefail

TARGET="${1:?usage: untracked_read_gauge.sh <target> [-- <defines>]}"; shift || true
[ "${1:-}" = "--" ] && shift
DEFS=("$@")

RUSH_BIN="${RUSH_BIN:-rush}"
TF_DIR="${TF_DIR:-$PWD}"
EXT_DIR="${EXT_DIR:-${RUSH_GLOBAL_CLONE_CACHE:-/data/rush-cache-217/external}}"
OUTDIR="${OUTDIR:-/tmp/rush_gauge}"
TRACE="$OUTDIR/strace.log"
MANIFEST="$OUTDIR/manifest_paths.txt"
STRACE_READS="$OUTDIR/strace_reads.txt"
MANIFEST_PATHS="$OUTDIR/manifest_paths_only.txt"
UNTRACKED="$OUTDIR/untracked.txt"

mkdir -p "$OUTDIR"
cd "$TF_DIR" || exit 1

echo "== priming (untraced warm build) =="
"$RUSH_BIN" build "$TARGET" "${DEFS[@]}" >/dev/null 2>&1

echo "== tracing build under strace + tracked-IO path dump =="
RUSH_TRACKED_IO_AUDIT=1 RUSH_TRACKED_IO_AUDIT_PATHS="$MANIFEST" \
  strace -f -y -e trace=%file,execve -o "$TRACE" \
    "$RUSH_BIN" build "$TARGET" "${DEFS[@]}" >/dev/null 2>"$OUTDIR/build.stderr"

if [ ! -s "$MANIFEST" ]; then
  echo "!! manifest dump empty/missing ($MANIFEST) — did the audit-dump build land?" >&2
  echo "   build.stderr tail:" >&2
  tail -5 "$OUTDIR/build.stderr" >&2
  exit 1
fi

# --- strace-observed reads: every absolute path opened/statted/listed/readlinked,
#     INCLUDING ENOENT negative probes (those are existence/glob/package-boundary
#     analysis deps and must be tracked). -------------------------------------------
grep -aE '(openat?|stat|lstat|newfstatat|readlink|getdents)\(' "$TRACE" \
  | grep -aoE '"(/[^"]+)"' | tr -d '"' \
  | sort -u > "$STRACE_READS"

# --- manifest paths (strip the "Kind<TAB>" prefix and any trailing "(exists=..)"). --
sed -E 's/^[A-Za-z]+\t//; s/\t\(.*\)$//' "$MANIFEST" | sort -u > "$MANIFEST_PATHS"

# --- the untracked set: strace MINUS manifest. -----------------------------------
comm -23 "$STRACE_READS" "$MANIFEST_PATHS" > "$UNTRACKED"

ws="$(cd "$TF_DIR" && pwd)"
total=$(wc -l < "$UNTRACKED")

# Categorize. Order matters: rush-out before workspace-source so output-tree wins.
cat_workspace="$OUTDIR/cat_workspace.txt"
cat_external="$OUTDIR/cat_external.txt"
cat_output="$OUTDIR/cat_output.txt"
cat_system="$OUTDIR/cat_system.txt"
cat_noise="$OUTDIR/cat_noise.txt"
: > "$cat_workspace"; : > "$cat_external"; : > "$cat_output"
: > "$cat_system"; : > "$cat_noise"

while IFS= read -r p; do
  case "$p" in
    /usr/*|/lib/*|/lib64/*|/proc/*|/sys/*|/etc/*|/dev/*|/run/*|/bin/*|/sbin/*)
      echo "$p" >> "$cat_system" ;;
    "$RUSH_BIN"|*/target/release/*|*/.cargo/*|*/.rustup/*|*/rustlib/*)
      echo "$p" >> "$cat_noise" ;;
    "$EXT_DIR"/*)
      echo "$p" >> "$cat_external" ;;
    *)
      # rush-out (output tree) anywhere under the workspace
      if printf '%s' "$p" | grep -qE '(^|/)rush-out/'; then
        echo "$p" >> "$cat_output"
      elif [ "${p#$ws/}" != "$p" ]; then
        echo "$p" >> "$cat_workspace"
      else
        echo "$p" >> "$cat_noise"
      fi ;;
  esac
done < "$UNTRACKED"

cnt() { wc -l < "$1" | tr -d ' '; }

echo
echo "================= UNTRACKED-READ GAUGE ================="
echo "target:            $TARGET"
echo "strace reads:      $(cnt "$STRACE_READS")"
echo "manifest paths:    $(cnt "$MANIFEST_PATHS")"
echo "UNTRACKED total:   $total"
echo "-------------------------------------------------------"
printf "  workspace-source (MIGRATE): %s\n" "$(cnt "$cat_workspace")"
printf "  external         (MIGRATE): %s\n" "$(cnt "$cat_external")"
printf "  output-tree      (exclude): %s\n" "$(cnt "$cat_output")"
printf "  system           (ignore):  %s\n" "$(cnt "$cat_system")"
printf "  noise/toolchain  (ignore):  %s\n" "$(cnt "$cat_noise")"
echo "-------------------------------------------------------"
echo "  REAL remaining analysis-migration surface ="
echo "    workspace-source + external = $(( $(cnt "$cat_workspace") + $(cnt "$cat_external") ))"
echo "======================================================="
echo
echo "== top workspace-source untracked (genuine migration targets) =="
sort "$cat_workspace" | uniq | head -30
echo
echo "== top external untracked =="
sort "$cat_external" | uniq | head -30
echo
echo "artifacts in $OUTDIR/ (strace.log, manifest_paths.txt, untracked.txt, cat_*.txt)"
