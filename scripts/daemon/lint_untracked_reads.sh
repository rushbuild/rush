#!/usr/bin/env bash
# Daemon migration ratchet — untracked-read scanner.
#
# Scans the ANALYSIS crate sources (rush-starlark, rush-build) for raw, UNTRACKED
# filesystem / env / subprocess reads — the sites that the persistent daemon must
# eventually route through crates/rush-incremental's TrackedIo capability so that a
# retained analysis graph can be safely reused. Every match here is a future
# wrong-build hole: a dependency the analysis consumed but did not record.
#
# This script does TWO jobs:
#   1. INVENTORY  (default mode) — print the per-crate / per-file / per-pattern
#      breakdown; the top files are the migration work-list.
#   2. RATCHET    (--check)     — compare current counts to the recorded baseline
#      (scripts/daemon/untracked_reads_baseline.txt) and FAIL if the total, or any
#      single file's count, has GROWN. Counts may only shrink as analysis migrates
#      onto TrackedIo. This is line-shift-proof: it tracks counts, not line numbers.
#
# Modes:
#   (none)            print the inventory report to stdout
#   --check           exit non-zero if any count grew beyond baseline; else exit 0
#   --update-baseline re-record the baseline (use AFTER a migration legitimately
#                     reduces counts; never use to paper over a NEW raw read)
#
# This is TOOLING ONLY. It changes no build behavior and migrates nothing. It is
# meant to be wired into CI and/or a pre-commit hook in a LATER brick — this brick
# does not install any hook.
#
# See docs/design/persistent-daemon-skyframe.md  "MIGRATION ENFORCEMENT".

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BASELINE="${BASELINE:-$REPO_ROOT/scripts/daemon/untracked_reads_baseline.txt}"
REPORT="${REPORT:-$REPO_ROOT/docs/design/untracked-reads-inventory.md}"

# Analysis crates to scan.
SCAN_DIRS=(
  "$REPO_ROOT/crates/rush-starlark/src"
  "$REPO_ROOT/crates/rush-build/src"
)

# ---------------------------------------------------------------------------
# Pattern definitions. Each entry is "label|<extended-regex>".
# Kept deliberately simple (line-based grep) per the brick spec: ratchet
# correctness matters more than perfect classification.
# ---------------------------------------------------------------------------
PATTERNS=(
  'std::fs|std::fs::'
  'tokio::fs|tokio::fs::'
  'env::var|(std::)?env::var\b|env::vars\b'
  'Path::exists/is_file/is_dir|\.(exists|is_file|is_dir)\('
  'metadata/read_dir/read_link|\.(metadata|symlink_metadata|read_dir|read_link)\('
  'glob|glob::glob|glob!\(|::glob\('
  'Command::new|Command::new|process::Command'
)

# ---------------------------------------------------------------------------
# emit_counts: print "<count>\t<relpath>" lines (per-file totals across all
# patterns) plus a trailing "<total>\t__TOTAL__" line, and (to fd 3 if open)
# the per-pattern tallies. Strips test code, comments, and best-effort string
# literals before matching.
# ---------------------------------------------------------------------------

# Build a single combined regex for the "any untracked read" per-file count.
combined_regex() {
  local parts=()
  local p
  for p in "${PATTERNS[@]}"; do
    parts+=("${p#*|}")
  done
  local IFS='|'
  echo "(${parts[*]})"
}

# Strip non-code lines from a Rust file on stdin -> stdout:
#  - drop everything inside #[cfg(test)] mod ... { ... } blocks (brace-depth tracked)
#  - drop #[test] fns (brace-depth tracked)
#  - drop /* ... */ block comments (may span lines)
#  - drop // and /// line comments (best-effort: trailing comments too)
#  - blank out "..." string-literal contents (best-effort, no escaped-quote handling)
strip_noncode() {
  awk '
    BEGIN { in_block_comment=0; skip_depth=0; pending_skip=0 }
    {
      line=$0

      # --- block comments /* ... */ (may span multiple lines) ---
      if (in_block_comment) {
        idx=index(line,"*/")
        if (idx==0) { next }
        line=substr(line, idx+2)
        in_block_comment=0
      }
      # remove any complete /* ... */ on this line, then detect an open one
      while ((s=index(line,"/*")) > 0) {
        rest=substr(line, s+2)
        e=index(rest,"*/")
        if (e>0) {
          line=substr(line,1,s-1) substr(rest,e+2)
        } else {
          line=substr(line,1,s-1)
          in_block_comment=1
          break
        }
      }

      # --- line comments // and /// (best effort; ignores // inside strings) ---
      c=index(line,"//")
      if (c>0) { line=substr(line,1,c-1) }

      # --- blank out string-literal contents: "..."  ->  "" ---
      # Drop escaped quotes first so they do not terminate a literal early, then
      # blank both raw-string and ordinary-string bodies. This keeps stray { } and
      # // inside strings from corrupting the brace-depth / comment tracking below.
      gsub(/\\"/, "", line)
      gsub(/r#*"[^"]*"#*/, "\"\"", line)
      gsub(/"[^"]*"/, "\"\"", line)
      # blank char literals that contain a brace: '{'  '}'  ->  ''
      gsub(/'[{}]'/, "''", line)

      # --- test-code suppression (brace-depth tracked) ---
      # A line carrying #[cfg(test)] or #[test] marks the NEXT braced item as skip.
      if (line ~ /#\[cfg\(test\)\]/ || line ~ /#\[test\]/) {
        pending_skip=1
        next
      }

      ob = gsub(/{/,"{",line)   # count of {
      cb = gsub(/}/,"}",line)   # count of }

      if (skip_depth > 0) {
        skip_depth += ob - cb
        if (skip_depth < 0) skip_depth=0
        next
      }
      if (pending_skip) {
        # enter skip region once we hit the opening brace of the item
        if (ob > 0) {
          skip_depth = ob - cb
          if (skip_depth < 0) skip_depth=0
          pending_skip=0
          next
        }
        # attribute line(s) before the brace: just drop them
        next
      }

      print line
    }
  '
}

is_test_file() {
  case "$(basename "$1")" in
    tests.rs|*_test.rs|*_tests.rs) return 0 ;;
    *) return 1 ;;
  esac
}

# Compute counts. Outputs (stdout):
#   per-file lines: "<count>\t<relpath>"
# And writes per-pattern tallies into the global assoc array via a temp file.
PATTERN_TALLY_FILE="${PATTERN_TALLY_FILE:-}"

compute() {
  local cre; cre="$(combined_regex)"
  # init pattern tallies
  local p label
  for p in "${PATTERNS[@]}"; do
    label="${p%%|*}"
    printf '%s\t0\n' "$label" >> "$PATTERN_TALLY_FILE"
  done

  local f rel stripped n
  while IFS= read -r f; do
    is_test_file "$f" && continue
    stripped="$(strip_noncode < "$f")"
    rel="${f#"$REPO_ROOT"/}"

    # per-file combined count
    n="$(printf '%s\n' "$stripped" | grep -aEc "$cre")"
    if [ "$n" -gt 0 ]; then
      printf '%d\t%s\n' "$n" "$rel"
    fi

    # accumulate per-pattern (recount into tally file)
    for p in "${PATTERNS[@]}"; do
      label="${p%%|*}"
      regex="${p#*|}"
      local pc
      pc="$(printf '%s\n' "$stripped" | grep -aEc "$regex")"
      if [ "$pc" -gt 0 ]; then
        # bump the tally for this label
        awk -v L="$label" -v add="$pc" 'BEGIN{FS=OFS="\t"} {if($1==L)$2=$2+add; print}' \
          "$PATTERN_TALLY_FILE" > "$PATTERN_TALLY_FILE.tmp" && mv "$PATTERN_TALLY_FILE.tmp" "$PATTERN_TALLY_FILE"
      fi
    done
  done < <(find "${SCAN_DIRS[@]}" -name '*.rs' -type f | sort)
}

# ---------------------------------------------------------------------------
# Gather counts into memory.
# ---------------------------------------------------------------------------
declare -A FILE_COUNTS
TOTAL=0

gather() {
  FILE_COUNTS=()
  TOTAL=0
  PATTERN_TALLY_FILE="$(mktemp)"
  local line cnt rel
  while IFS=$'\t' read -r cnt rel; do
    [ -z "${rel:-}" ] && continue
    FILE_COUNTS["$rel"]="$cnt"
    TOTAL=$(( TOTAL + cnt ))
  done < <(compute)
}

crate_total() {
  local prefix="$1" sum=0 rel
  for rel in "${!FILE_COUNTS[@]}"; do
    case "$rel" in
      $prefix*) sum=$(( sum + ${FILE_COUNTS["$rel"]} )) ;;
    esac
  done
  echo "$sum"
}

# ---------------------------------------------------------------------------
# Writers
# ---------------------------------------------------------------------------
write_baseline() {
  {
    echo "# Untracked-read ratchet baseline."
    echo "# Generated by scripts/daemon/lint_untracked_reads.sh --update-baseline"
    echo "# Format: <count>\\t<relpath>  ;  final line is <total>\\t__TOTAL__"
    echo "# Counts may only SHRINK as analysis migrates onto TrackedIo."
    echo "# Do NOT raise these to admit a new raw read (NO-STUBS applies to ratchets too)."
    echo "#"
    local rel
    for rel in $(printf '%s\n' "${!FILE_COUNTS[@]}" | sort); do
      printf '%d\t%s\n' "${FILE_COUNTS["$rel"]}" "$rel"
    done
    printf '%d\t__TOTAL__\n' "$TOTAL"
  } > "$BASELINE"
  echo "baseline written: $BASELINE  (total=$TOTAL)"
}

write_report() {
  local starlark_total build_total
  starlark_total="$(crate_total "crates/rush-starlark/src")"
  build_total="$(crate_total "crates/rush-build/src")"

  {
    echo "# Untracked-Read Inventory — Daemon Migration Work-List"
    echo
    echo "_Generated by \`scripts/daemon/lint_untracked_reads.sh\` (default mode). Do not edit by hand; re-run the scanner._"
    echo
    echo "This is the inventory of raw, **untracked** filesystem / environment / subprocess"
    echo "reads in the analysis crates. The persistent daemon retains and reuses the analyzed"
    echo "build graph; that reuse is only correct if analysis records every filesystem/env"
    echo "dependency through \`crates/rush-incremental\`'s \`TrackedIo\` capability. Each site listed"
    echo "below is a read that today bypasses that tracking — a potential wrong-build hole once"
    echo "the graph is cached. This report is the starting **migration work-list**."
    echo
    echo "See \`docs/design/persistent-daemon-skyframe.md\` section \"MIGRATION ENFORCEMENT\"."
    echo
    echo "## Total"
    echo
    echo "**$TOTAL untracked-read sites** across the analysis crates."
    echo
    echo "## Per-crate breakdown"
    echo
    echo "| Crate | Untracked-read sites |"
    echo "| --- | ---: |"
    echo "| \`crates/rush-starlark/src\` | $starlark_total |"
    echo "| \`crates/rush-build/src\` | $build_total |"
    echo "| **Total** | **$TOTAL** |"
    echo
    echo "> Note: \`crates/rush-build/src\` contains BOTH the analysis logic and the executor."
    echo "> The executor legitimately runs subprocesses (\`Command::new\`) and writes/reads build"
    echo "> outputs (\`std::fs\`), so executor and output modules are EXPECTED to retain raw"
    echo "> \`Command::new\`/\`std::fs\` calls. The migration targets the ANALYSIS read sites only;"
    echo "> this report is the starting work-list, not a literal must-fix-everything list. Triage"
    echo "> each file by whether it shapes the analysis graph or merely executes/writes outputs."
    echo
    echo "## Top files by count (migration work-list)"
    echo
    echo "| Count | File |"
    echo "| ---: | --- |"
    local rel
    for rel in $(for r in "${!FILE_COUNTS[@]}"; do printf '%d\t%s\n' "${FILE_COUNTS["$r"]}" "$r"; done | sort -rn | head -25 | cut -f2-); do
      printf '| %d | `%s` |\n' "${FILE_COUNTS["$rel"]}" "$rel"
    done
    echo
    echo "## Per-pattern breakdown"
    echo
    echo "| Pattern | Matches |"
    echo "| --- | ---: |"
    sort -t$'\t' -k2 -rn "$PATTERN_TALLY_FILE" | while IFS=$'\t' read -r label cnt; do
      printf '| `%s` | %d |\n' "$label" "$cnt"
    done
    echo
    echo "_Per-pattern matches are counted independently and may exceed the per-file totals when a single line matches multiple patterns (e.g. \`std::fs::metadata\`)._"
  } > "$REPORT"
  echo "report written: $REPORT  (total=$TOTAL)"
}

print_stdout_summary() {
  echo "== Untracked-read inventory =="
  echo "TOTAL sites: $TOTAL"
  echo "  crates/rush-starlark/src: $(crate_total "crates/rush-starlark/src")"
  echo "  crates/rush-build/src:    $(crate_total "crates/rush-build/src")"
  echo
  echo "Top 10 files:"
  for r in "${!FILE_COUNTS[@]}"; do printf '%d\t%s\n' "${FILE_COUNTS["$r"]}" "$r"; done \
    | sort -rn | head -10 | awk '{printf "  %5d  %s\n",$1,$2}'
  echo
  echo "Per-pattern:"
  sort -t$'\t' -k2 -rn "$PATTERN_TALLY_FILE" | awk -F'\t' '{printf "  %5d  %s\n",$2,$1}'
}

# ---------------------------------------------------------------------------
# --check: compare against baseline, fail on growth.
# ---------------------------------------------------------------------------
do_check() {
  if [ ! -f "$BASELINE" ]; then
    echo "ERROR: baseline not found: $BASELINE" >&2
    echo "Run: $0 --update-baseline" >&2
    return 2
  fi

  # Load baseline into assoc array.
  declare -A BASE
  local base_total=0 cnt rel
  while IFS=$'\t' read -r cnt rel; do
    case "$rel" in ''|'#'*) continue ;; esac
    [[ "$cnt" =~ ^[0-9]+$ ]] || continue
    if [ "$rel" = "__TOTAL__" ]; then base_total="$cnt"; continue; fi
    BASE["$rel"]="$cnt"
  done < "$BASELINE"

  local failed=0

  # Per-file growth.
  for rel in "${!FILE_COUNTS[@]}"; do
    local cur="${FILE_COUNTS["$rel"]}"
    local b="${BASE["$rel"]:-0}"
    if [ "$cur" -gt "$b" ]; then
      echo "FAIL: $rel grew $b -> $cur untracked-read sites (a new raw read was added)" >&2
      failed=1
    fi
  done

  # Total growth (catches additions in brand-new files too).
  if [ "$TOTAL" -gt "$base_total" ]; then
    echo "FAIL: total untracked-read sites grew $base_total -> $TOTAL" >&2
    failed=1
  fi

  if [ "$failed" -ne 0 ]; then
    echo >&2
    echo "The untracked-read ratchet blocked this change. Analysis must route reads through" >&2
    echo "crates/rush-incremental TrackedIo, not raw std::fs/std::env/glob/Command. If you" >&2
    echo "legitimately REDUCED counts, run: $0 --update-baseline" >&2
    return 1
  fi

  echo "OK: untracked-read counts within baseline (current total=$TOTAL, baseline total=$base_total)"
  return 0
}

# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------
MODE="${1:-report}"

gather

rc=0
case "$MODE" in
  --check)
    do_check; rc=$?
    ;;
  --update-baseline)
    write_baseline
    write_report
    ;;
  report|"")
    write_report
    print_stdout_summary
    ;;
  *)
    echo "usage: $0 [--check | --update-baseline]" >&2
    rc=2
    ;;
esac

[ -n "${PATTERN_TALLY_FILE:-}" ] && rm -f "$PATTERN_TALLY_FILE" "$PATTERN_TALLY_FILE.tmp" 2>/dev/null
exit $rc
