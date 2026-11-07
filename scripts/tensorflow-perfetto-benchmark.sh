#!/usr/bin/env bash
# Reproducible Rush-vs-Bazel TensorFlow clean-build profiles for Perfetto.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUSH_REPO="$(cd "$SCRIPT_DIR/.." && pwd)"
ROOT="${RUSH_TF_BENCH_ROOT:-/data/rush-tf-clean}"
TF_ROOT="${TF_ROOT:-$ROOT/tensorflow}"
RUSH_BIN="${RUSH_BIN:-$ROOT/bin/rush}"
BAZEL_BIN="${BAZEL_BIN:-$ROOT/bin/bazel}"
RESULTS_ROOT="${RESULTS_ROOT:-$ROOT/results/perfetto}"
TARGET="${TARGET:-//tensorflow/cc:cc_ops}"
REPETITIONS="${REPETITIONS:-7}"
WARMUPS="${WARMUPS:-1}"
JOBS="${JOBS:-96}"
CPUSET="${CPUSET:-0-95}"
MODE="${MODE:-both}"
CXX="${CXX:-$ROOT/wrappers/clang22-v604/c++}"
RUN_LABEL="${RUN_LABEL:-baseline}"
PYTHON="${PYTHON:-python3}"
PROFILE_TOOL="$SCRIPT_DIR/perfetto-build-profile.py"

usage() {
    cat <<EOF
Usage: $0 [options]

Capture interleaved clean-output Rush and Bazel TensorFlow builds as Chrome
trace JSON that opens directly in https://ui.perfetto.dev/.

Options:
  --rush-bin PATH       Rush executable (default: $RUSH_BIN)
  --bazel-bin PATH      Bazel executable (default: $BAZEL_BIN)
  --rush-repo PATH      Rush repository used for metadata
  --tensorflow PATH     TensorFlow checkout (default: $TF_ROOT)
  --results-root PATH   Parent results directory
  --target LABEL        Build label (default: $TARGET)
  --repetitions N       Measured runs per tool (default: $REPETITIONS)
  --warmups N           Unmeasured warmups per tool (default: $WARMUPS)
  --jobs N              Build parallelism (default: $JOBS)
  --cpuset LIST         taskset CPU list (default: $CPUSET)
  --mode MODE           both, rush, or bazel (default: $MODE)
  --cxx PATH            Rush compiler wrapper (default: $CXX)
  --label NAME          Result label (default: $RUN_LABEL)
  -h, --help            Show this help

The harness removes build outputs and action caches, but intentionally retains
repository/bootstrap downloads and the operating-system page cache. It records
that policy as "clean-output, warm-page-cache"; cold-page-cache testing must be
run separately with administrator-controlled cache isolation.
EOF
}

while (($#)); do
    case "$1" in
        --rush-bin) RUSH_BIN=$2; shift 2 ;;
        --bazel-bin) BAZEL_BIN=$2; shift 2 ;;
        --rush-repo) RUSH_REPO=$2; shift 2 ;;
        --tensorflow) TF_ROOT=$2; shift 2 ;;
        --results-root) RESULTS_ROOT=$2; shift 2 ;;
        --target) TARGET=$2; shift 2 ;;
        --repetitions) REPETITIONS=$2; shift 2 ;;
        --warmups) WARMUPS=$2; shift 2 ;;
        --jobs) JOBS=$2; shift 2 ;;
        --cpuset) CPUSET=$2; shift 2 ;;
        --mode) MODE=$2; shift 2 ;;
        --cxx) CXX=$2; shift 2 ;;
        --label) RUN_LABEL=$2; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done

case "$MODE" in
    both|rush|bazel) ;;
    *) echo "--mode must be both, rush, or bazel" >&2; exit 2 ;;
esac
[[ "$REPETITIONS" =~ ^[1-9][0-9]*$ ]] || {
    echo "--repetitions must be positive" >&2
    exit 2
}
[[ "$WARMUPS" =~ ^[0-9]+$ ]] || {
    echo "--warmups must be non-negative" >&2
    exit 2
}

for command in "$PYTHON" jq taskset /usr/bin/time sha256sum git; do
    command -v "$command" >/dev/null || {
        echo "Missing required command: $command" >&2
        exit 1
    }
done
[[ -d "$TF_ROOT" ]] || { echo "Missing TensorFlow checkout: $TF_ROOT" >&2; exit 1; }
[[ -f "$PROFILE_TOOL" ]] || { echo "Missing profile utility: $PROFILE_TOOL" >&2; exit 1; }
if [[ "$MODE" != bazel ]]; then
    [[ -x "$RUSH_BIN" ]] || { echo "Rush is not executable: $RUSH_BIN" >&2; exit 1; }
fi
if [[ "$MODE" != rush ]]; then
    [[ -x "$BAZEL_BIN" ]] || { echo "Bazel is not executable: $BAZEL_BIN" >&2; exit 1; }
fi

safe_target="$(printf '%s' "$TARGET" | tr -cs 'A-Za-z0-9._-' '-')"
timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
RUN_ROOT="$RESULTS_ROOT/${timestamp}-${RUN_LABEL}-${safe_target}"
STATUS="$RUN_ROOT/status"
mkdir -p "$RUN_ROOT"
printf 'running\n' > "$STATUS"

finish() {
    rc=$?
    if ((rc == 0)); then
        printf 'finished rc=0\n' > "$STATUS"
    else
        printf 'finished rc=%d\n' "$rc" > "$STATUS"
    fi
}
trap finish EXIT

git_sha_or_unknown() {
    git -C "$1" rev-parse HEAD 2>/dev/null || printf 'unknown\n'
}

binary_sha_or_unknown() {
    if [[ -f "$1" ]]; then
        sha256sum "$1" | awk '{print $1}'
    else
        printf 'unavailable\n'
    fi
}

rush_sha="$(git_sha_or_unknown "$RUSH_REPO")"
tf_sha="$(git_sha_or_unknown "$TF_ROOT")"
rush_bin_sha="$(binary_sha_or_unknown "$RUSH_BIN")"
bazel_bin_sha="$(binary_sha_or_unknown "$BAZEL_BIN")"
bazel_version="$("$BAZEL_BIN" --version 2>/dev/null || true)"

uname -a > "$RUN_ROOT/uname.txt"
lscpu -J > "$RUN_ROOT/lscpu.json"
free -b > "$RUN_ROOT/memory.txt"
findmnt -T "$TF_ROOT" -J > "$RUN_ROOT/tensorflow-mount.json"
ps -eo pid,ppid,psr,etimes,pcpu,pmem,rss,args --sort=-pcpu \
    > "$RUN_ROOT/processes-start.txt"

jq -n \
    --arg created_utc "$timestamp" \
    --arg run_label "$RUN_LABEL" \
    --arg target "$TARGET" \
    --arg clean_policy "clean-output, warm-page-cache" \
    --arg mode "$MODE" \
    --arg rush_repo "$RUSH_REPO" \
    --arg rush_sha "$rush_sha" \
    --arg rush_bin "$RUSH_BIN" \
    --arg rush_bin_sha256 "$rush_bin_sha" \
    --arg bazel_bin "$BAZEL_BIN" \
    --arg bazel_bin_sha256 "$bazel_bin_sha" \
    --arg bazel_version "$bazel_version" \
    --arg tensorflow "$TF_ROOT" \
    --arg tensorflow_sha "$tf_sha" \
    --arg cxx "$CXX" \
    --arg cpuset "$CPUSET" \
    --argjson jobs "$JOBS" \
    --argjson repetitions "$REPETITIONS" \
    --argjson warmups "$WARMUPS" \
    '{
        schema_version: 1,
        created_utc: $created_utc,
        label: $run_label,
        target: $target,
        clean_policy: $clean_policy,
        mode: $mode,
        jobs: $jobs,
        cpuset: $cpuset,
        repetitions: $repetitions,
        warmups: $warmups,
        rush: {
            repo: $rush_repo,
            commit: $rush_sha,
            binary: $rush_bin,
            binary_sha256: $rush_bin_sha256,
            cxx: $cxx
        },
        bazel: {
            binary: $bazel_bin,
            binary_sha256: $bazel_bin_sha256,
            version: $bazel_version
        },
        tensorflow: {
            checkout: $tensorflow,
            commit: $tensorflow_sha
        }
    }' > "$RUN_ROOT/manifest.json"

printf 'ordinal\ttool\trepetition\tmeasured\tstatus\n' > "$RUN_ROOT/progress.tsv"
BAZEL_OUTPUT_BASE="$RUN_ROOT/bazel-output-base"
BAZEL_REPOSITORY_CACHE="$ROOT/bazel-repository-cache"
mkdir -p "$BAZEL_REPOSITORY_CACHE"
ordinal=0

write_command() {
    local output=$1
    shift
    printf '%q ' "$@" > "$output"
    printf '\n' >> "$output"
}

run_one() {
    local tool=$1
    local repetition=$2
    local measured=$3
    ordinal=$((ordinal + 1))
    local run_name
    run_name="$(printf 'run-%03d-%s-r%02d' "$ordinal" "$tool" "$repetition")"
    local run_dir="$RUN_ROOT/$run_name"
    local cache_dir="$run_dir/cache"
    local time_file="$run_dir/time.tsv"
    local trace_raw="$run_dir/trace.raw.json"
    local trace_perfetto="$run_dir/trace.perfetto.json"
    local actions="$run_dir/actions.jsonl"
    local trace_summary="$run_dir/trace-summary.json"
    local action_summary="$run_dir/action-summary.json"
    mkdir -p "$run_dir" "$cache_dir"
    ps -eo pid,ppid,psr,etimes,pcpu,pmem,rss,args --sort=-pcpu \
        > "$run_dir/processes-before.txt"
    uptime > "$run_dir/load-before.txt"

    local -a command
    if [[ "$tool" == rush ]]; then
        rm -rf "$TF_ROOT/rush-out" "$cache_dir"
        mkdir -p "$cache_dir"
        command=(
            env
            "RUSTUP_HOME=$ROOT/rustup"
            "CARGO_HOME=$ROOT/cargo-home"
            "CARGO_TARGET_DIR=$ROOT/cargo-target"
            "PATH=$ROOT/bootstrap-cache/com_google_protobuf:$ROOT/bin:$ROOT/cargo-home/bin:$PATH"
            "RUSH_GLOBAL_CLONE_CACHE=$ROOT/bootstrap-cache"
            "RUSH_BUILD_JOBS=$JOBS"
            "RUSH_ACTION_AWARE_TARGET_WAVEFRONT=1"
            "RUSH_CRITICAL_PATH_TARGET_FRONTIER=1"
            "RUSH_BUILD_CHATTER=1"
            "RUSH_ACTION_TIMING=$actions"
            "RUSH_HEAVY_CPP_CPU_SLOTS=24"
            "RUSH_TF_GENERATED_CC_SHARDS=4"
            "RUSH_CXX=$CXX"
            "RUSH_TF_CRITICAL_CXX=$CXX"
            taskset -c "$CPUSET"
            "$RUSH_BIN" build
            --daemon=off
            "--jobs=$JOBS"
            -c opt
            --use-scheduler
            "--disk-cache-dir=$cache_dir"
            "--profile=$trace_raw"
            "$TARGET"
        )
    else
        set +e
        (
            cd "$TF_ROOT"
            "$BAZEL_BIN" --batch "--output_base=$BAZEL_OUTPUT_BASE" clean
        ) > "$run_dir/clean.log" 2>&1
        clean_rc=$?
        set -e
        if ((clean_rc != 0)); then
            echo "Bazel clean failed for $run_name" >&2
            return "$clean_rc"
        fi
        command=(
            taskset -c "$CPUSET"
            "$BAZEL_BIN"
            --batch
            "--output_base=$BAZEL_OUTPUT_BASE"
            build
            "--jobs=$JOBS"
            -c opt
            "--repository_cache=$BAZEL_REPOSITORY_CACHE"
            "--profile=$trace_raw"
            "$TARGET"
        )
    fi

    write_command "$run_dir/command.txt" "${command[@]}"
    printf '%s\t%s\t%s\t%s\trunning\n' \
        "$ordinal" "$tool" "$repetition" "$measured" \
        >> "$RUN_ROOT/progress.tsv"
    set +e
    (
        cd "$TF_ROOT"
        /usr/bin/time \
            -f '%e\t%U\t%S\t%M\t%P\t%x' \
            -o "$time_file" \
            "${command[@]}"
    ) > "$run_dir/build.log" 2>&1
    rc=$?
    set -e

    wall_seconds=
    user_seconds=
    system_seconds=
    max_rss_kb=
    cpu_percent=
    time_exit_code=
    if [[ -s "$time_file" ]]; then
        IFS=$'\t' read -r wall_seconds user_seconds system_seconds \
            max_rss_kb cpu_percent time_exit_code < "$time_file"
        cpu_percent="${cpu_percent%%%}"
    fi

    if ((rc == 0)) && [[ -s "$trace_raw" ]]; then
        if [[ "$tool" == rush ]]; then
            "$PYTHON" "$PROFILE_TOOL" merge-rush \
                --trace "$trace_raw" \
                --actions "$actions" \
                --output "$trace_perfetto" \
                --summary "$action_summary"
            "$PYTHON" "$PROFILE_TOOL" summarize-trace \
                --trace "$trace_perfetto" \
                --output "$trace_summary"
        else
            "$PYTHON" "$PROFILE_TOOL" summarize-trace \
                --trace "$trace_raw" \
                --output "$trace_summary"
        fi
    fi

    local relative_trace_summary=
    local relative_action_summary=
    [[ -f "$trace_summary" ]] && \
        relative_trace_summary="${trace_summary#"$RUN_ROOT"/}"
    [[ -f "$action_summary" ]] && \
        relative_action_summary="${action_summary#"$RUN_ROOT"/}"
    jq -n \
        --arg run "$run_name" \
        --arg tool "$tool" \
        --arg target "$TARGET" \
        --arg trace_summary "$relative_trace_summary" \
        --arg action_summary "$relative_action_summary" \
        --arg wall_seconds "$wall_seconds" \
        --arg user_seconds "$user_seconds" \
        --arg system_seconds "$system_seconds" \
        --arg max_rss_kb "$max_rss_kb" \
        --arg cpu_percent "$cpu_percent" \
        --argjson repetition "$repetition" \
        --argjson measured "$measured" \
        --argjson exit_code "$rc" \
        '{
            schema_version: 1,
            run: $run,
            tool: $tool,
            target: $target,
            repetition: $repetition,
            measured: $measured,
            exit_code: $exit_code,
            wall_seconds: ($wall_seconds | tonumber?),
            user_seconds: ($user_seconds | tonumber?),
            system_seconds: ($system_seconds | tonumber?),
            max_rss_kb: ($max_rss_kb | tonumber?),
            cpu_percent: ($cpu_percent | tonumber?),
            trace_summary: $trace_summary,
            action_summary: $action_summary
        }' > "$run_dir/result.json"
    uptime > "$run_dir/load-after.txt"
    printf '%s\t%s\t%s\t%s\tfinished-rc-%s\n' \
        "$ordinal" "$tool" "$repetition" "$measured" "$rc" \
        >> "$RUN_ROOT/progress.tsv"
    "$PYTHON" "$PROFILE_TOOL" summarize-suite --root "$RUN_ROOT"
    return "$rc"
}

tools=()
[[ "$MODE" == both || "$MODE" == rush ]] && tools+=(rush)
[[ "$MODE" == both || "$MODE" == bazel ]] && tools+=(bazel)

for ((warmup = 1; warmup <= WARMUPS; warmup++)); do
    for tool in "${tools[@]}"; do
        run_one "$tool" "$warmup" false
    done
done

for ((repetition = 1; repetition <= REPETITIONS; repetition++)); do
    if [[ "$MODE" == both && $((repetition % 2)) -eq 0 ]]; then
        order=(bazel rush)
    else
        order=("${tools[@]}")
    fi
    for tool in "${order[@]}"; do
        run_one "$tool" "$repetition" true
    done
done

ps -eo pid,ppid,psr,etimes,pcpu,pmem,rss,args --sort=-pcpu \
    > "$RUN_ROOT/processes-finish.txt"
"$PYTHON" "$PROFILE_TOOL" summarize-suite --root "$RUN_ROOT"
printf '%s\n' "$RUN_ROOT"
