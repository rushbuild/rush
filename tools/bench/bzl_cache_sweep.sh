#!/usr/bin/env bash
# Rush — GlobalBzlCache capacity sweep (phase 5.C.4 commit 98)
#
# Pro's commit-98 directive: "Run the same target with
# several cache configurations: current default,
# entries=250, entries=500, entries=1000, optionally
# entries=2000, optionally bytes=4GiB if safe. Three to
# five configs is enough. This is not a PhD thesis."
#
# Commit 96 finding: TF macros analyze-pass shows
# byte-bound eviction (337 byte-limit evictions, 0
# entry-limit evictions). This sweep tests BOTH dimensions
# to either confirm "byte is the cap" or surface a
# different signal.
#
# USAGE:
#   tools/bench/bzl_cache_sweep.sh <target> [rush-binary]
#
# EXAMPLES:
#   tools/bench/bzl_cache_sweep.sh //tensorflow/core/platform:macros
#   tools/bench/bzl_cache_sweep.sh //... target/release/rush
#
# OUTPUT:
#   TSV on stdout with columns:
#     config_label  entries_cap  bytes_cap  wall_secs
#     hits  misses  evictions  cold_misses  repeat_misses
#     by_entry_limit  by_byte_limit  hit_rate_pct
#     peak_entries  peak_memory_mb  unique_files_seen
#
# EXIT CODES:
#   0 — sweep completed
#   2 — environment problem (jq/rush missing, bad args,
#       target not given)

set -euo pipefail

if [[ $# -lt 1 || $# -gt 2 ]]; then
    cat >&2 <<USAGE
usage: $0 <target> [rush-binary]

example:
    $0 //tensorflow/core/platform:macros target/release/rush

The sweep runs the same target N times with different
GlobalBzlCache caps. Outputs TSV to stdout.
USAGE
    exit 2
fi

TARGET="$1"
RUSH_BIN="${2:-rush}"

if ! command -v jq >/dev/null 2>&1; then
    echo "error: jq required for parsing JSON output. install: sudo apt install jq" >&2
    exit 2
fi

if ! command -v "$RUSH_BIN" >/dev/null 2>&1 && [[ ! -x "$RUSH_BIN" ]]; then
    echo "error: rush binary not found at: $RUSH_BIN" >&2
    exit 2
fi

# Resolve to absolute path so cd-into-workspace doesn't lose it.
if [[ -x "$RUSH_BIN" ]]; then
    RUSH_BIN="$(realpath "$RUSH_BIN" 2>/dev/null || readlink -f "$RUSH_BIN")"
fi

# ---------------------------------------------------------
# Configurations to sweep
# ---------------------------------------------------------
#
# Format: "label|entries|bytes"
# Empty entries / bytes means "use rush default".
#
# Commit 96 data showed byte-bound eviction, so the
# byte-dimension sweep is the load-bearing one. The entry
# sweep is included to falsify "entries are actually the
# limit" — if entries=1000 doesn't help, that confirms.

CONFIGS=(
    "default-512M||"
    "entries-250|250|"
    "entries-500|500|"
    "entries-1000|1000|"
    "bytes-1G||1G"
    "bytes-2G||2G"
    "bytes-4G||4G"
)

# ---------------------------------------------------------
# Print TSV header
# ---------------------------------------------------------

printf 'config\tentries_cap\tbytes_cap\twall_secs\thits\tmisses\tevictions\tcold_misses\trepeat_misses\tby_entry_limit\tby_byte_limit\thit_rate_pct\tpeak_entries\tpeak_memory_mb\tunique_files_seen\n'

# ---------------------------------------------------------
# Run sweep
# ---------------------------------------------------------

WORKSPACE_DIR=""
# Determine workspace dir based on target prefix.
if [[ "$TARGET" == //tensorflow/* ]]; then
    SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
    WORKSPACE_DIR="$REPO_ROOT/tensorflow"
fi

run_one() {
    local label="$1"
    local entries="$2"
    local bytes="$3"

    local env_args=()
    if [[ -n "$entries" ]]; then
        env_args+=("RUSH_BZL_CACHE_MAX_ENTRIES=$entries")
    fi
    if [[ -n "$bytes" ]]; then
        env_args+=("RUSH_BZL_CACHE_MAX_BYTES=$bytes")
    fi
    env_args+=(
        "RUSH_ANALYZE_TIMING=1"
        "RUSH_ANALYZE_TIMING_JSON=1"
        "RUSH_ANALYZE_CACHE_COUNTERS=1"
        "RUSH_ANALYZE_CACHE_COUNTERS_JSON=1"
    )

    local cwd_arg=()
    if [[ -n "$WORKSPACE_DIR" ]]; then
        cwd_arg=("-C" "$WORKSPACE_DIR")
    fi

    # Capture wall time + stdout JSON. rush-out must be
    # cleaned between runs so cache warm-up doesn't skew.
    if [[ -n "$WORKSPACE_DIR" ]]; then
        rm -rf "$WORKSPACE_DIR/rush-out"
    fi

    local stdout_tmp
    stdout_tmp="$(mktemp)"
    local start_ns end_ns
    start_ns="$(date +%s%N)"
    if [[ -n "$WORKSPACE_DIR" ]]; then
        ( cd "$WORKSPACE_DIR" && env "${env_args[@]}" \
            "$RUSH_BIN" build --analyze-only --use-scheduler \
            --scheduler-preflight=json \
            "$TARGET" ) > "$stdout_tmp" 2>/dev/null || true
    else
        env "${env_args[@]}" \
            "$RUSH_BIN" build --analyze-only --use-scheduler \
            --scheduler-preflight=json \
            "$TARGET" > "$stdout_tmp" 2>/dev/null || true
    fi
    end_ns="$(date +%s%N)"

    local wall_ms=$(( (end_ns - start_ns) / 1000000 ))
    local wall_secs="$(awk -v ms="$wall_ms" 'BEGIN{printf "%.1f", ms/1000}')"

    # Extract cache-counter JSON line (it has
    # build_file_lookups). Filter for lines that start
    # with `{` (skip non-JSON noise like [BAZEL FETCH]
    # banner output that leaks to stdout pre-commit-70
    # cleanup boundaries), then pick the line containing
    # the build_file_lookups marker.
    local counters_json
    counters_json="$(
        grep -E '^\{.*build_file_lookups' "$stdout_tmp" 2>/dev/null \
            | head -n 1 || true
    )"

    rm -f "$stdout_tmp"

    if [[ -z "$counters_json" ]]; then
        printf '%s\t%s\t%s\t%s\t-\t-\t-\t-\t-\t-\t-\t-\t-\t-\t-\n' \
            "$label" "${entries:-default}" "${bytes:-default}" "$wall_secs"
        return
    fi

    local hits misses evictions cold repeat by_entry by_byte hit_rate peak_entries peak_mb unique
    hits="$(echo "$counters_json" | jq -r '.bzl_cache_hits // 0')"
    misses="$(echo "$counters_json" | jq -r '.bzl_cache_misses // 0')"
    evictions="$(echo "$counters_json" | jq -r '.bzl_cache_evictions // 0')"
    cold="$(echo "$counters_json" | jq -r '.bzl_cold_misses // 0')"
    repeat="$(echo "$counters_json" | jq -r '.bzl_repeat_misses // 0')"
    by_entry="$(echo "$counters_json" | jq -r '.bzl_evictions_by_entry_limit // 0')"
    by_byte="$(echo "$counters_json" | jq -r '.bzl_evictions_by_byte_limit // 0')"
    hit_rate="$(echo "$counters_json" | jq -r '.bzl_cache_hit_rate_percent // "-"')"
    peak_entries="$(echo "$counters_json" | jq -r '.bzl_peak_entry_count // 0')"
    peak_mb="$(echo "$counters_json" | jq -r '.bzl_peak_memory_mb // 0')"
    unique="$(echo "$counters_json" | jq -r '.bzl_unique_files_seen // 0')"

    # Format hit_rate to 1 decimal place if numeric.
    if [[ "$hit_rate" != "-" ]]; then
        hit_rate="$(awk -v r="$hit_rate" 'BEGIN{printf "%.1f", r}')"
    fi

    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$label" "${entries:-default}" "${bytes:-default}" "$wall_secs" \
        "$hits" "$misses" "$evictions" "$cold" "$repeat" \
        "$by_entry" "$by_byte" "$hit_rate" \
        "$peak_entries" "$peak_mb" "$unique"
}

echo "→ Sweeping ${#CONFIGS[@]} configs against $TARGET ..." >&2

for cfg in "${CONFIGS[@]}"; do
    IFS='|' read -r label entries bytes <<< "$cfg"
    echo "  config: $label (entries=${entries:-default}, bytes=${bytes:-default}) ..." >&2
    run_one "$label" "$entries" "$bytes"
done

echo "→ Sweep complete." >&2
