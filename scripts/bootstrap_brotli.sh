#!/usr/bin/env bash
# Build TensorFlow's pinned Brotli as PIC static archives for Rush.

set -euo pipefail

BROTLI_COMMIT="${BROTLI_COMMIT:-3914999fcc1fda92e750ef9190aa6db9bf7bdb07}"
BROTLI_SHA256="${BROTLI_SHA256:-84a9a68ada813a59db94d83ea10c54155f1d34399baf377842ff3ab9b3b3256e}"
BROTLI_URL="https://github.com/google/brotli/archive/${BROTLI_COMMIT}.zip"
BROTLI_MIRROR_URL="https://storage.googleapis.com/mirror.tensorflow.org/github.com/google/brotli/archive/${BROTLI_COMMIT}.zip"
CACHE_ROOT="${RUSH_GLOBAL_CLONE_CACHE:-/data/rush-cache/external}"
DEST="${CACHE_ROOT}/org_brotli"
STAMP="${DEST}/.rush-bootstrap-commit"
JOBS="${RUSH_BOOTSTRAP_JOBS:-$(nproc)}"

MODE=""
FORCE=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run) MODE="dry-run"; shift ;;
        --apply) MODE="apply"; shift ;;
        --force) FORCE=1; shift ;;
        -h|--help)
            sed -n '2,50p' "$0"
            exit 0
            ;;
        *) echo "error: unknown arg: $1" >&2; exit 2 ;;
    esac
done
[[ -n "$MODE" ]] || {
    echo "error: must specify --dry-run or --apply" >&2
    exit 2
}

cat <<EOF
=== bootstrap_brotli plan ===
commit      : ${BROTLI_COMMIT}
cache root  : ${CACHE_ROOT}
destination : ${DEST}
parallelism : ${JOBS}
EOF

if [[ "$MODE" == "dry-run" ]]; then
    exit 0
fi

for tool in cmake curl nproc python3 sha256sum; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "error: required tool not on PATH: $tool" >&2
        exit 2
    }
done

if [[ -f "$STAMP" ]] \
    && [[ "$(cat "$STAMP")" == "$BROTLI_COMMIT" ]] \
    && [[ -f "$DEST/libbrotlicommon.a" ]] \
    && [[ -f "$DEST/libbrotlidec.a" ]] \
    && [[ -f "$DEST/libbrotlienc.a" ]] \
    && [[ -f "$DEST/src/c/include/brotli/shared_dictionary.h" ]] \
    && [[ $FORCE -eq 0 ]]; then
    echo "already bootstrapped: $DEST"
    exit 0
fi

if [[ -d "$DEST" && -n "$(ls -A "$DEST" 2>/dev/null)" ]]; then
    [[ $FORCE -eq 1 ]] || {
        echo "error: $DEST is non-empty; use --force to overwrite" >&2
        exit 1
    }
    rm -rf "$DEST"
fi

mkdir -p "$CACHE_ROOT"
archive="$(mktemp "${CACHE_ROOT}/.brotli.XXXXXX.zip")"
stage="$(mktemp -d "${CACHE_ROOT}/.org_brotli.XXXXXX")"
trap 'rm -f "$archive"; rm -rf "$stage"' EXIT

curl --fail --silent --show-error --location --output "$archive" \
    "$BROTLI_MIRROR_URL" \
    || curl --fail --silent --show-error --location --output "$archive" \
        "$BROTLI_URL"
actual_sha="$(sha256sum "$archive" | awk '{print $1}')"
[[ "$actual_sha" == "$BROTLI_SHA256" ]] || {
    echo "error: Brotli sha256 mismatch" >&2
    echo "  expected: $BROTLI_SHA256" >&2
    echo "  actual:   $actual_sha" >&2
    exit 1
}

mkdir -p "$stage/src"
python3 - "$archive" "$stage/src" "brotli-${BROTLI_COMMIT}/" <<'PY'
import pathlib
import sys
import zipfile

archive = pathlib.Path(sys.argv[1])
destination = pathlib.Path(sys.argv[2])
prefix = sys.argv[3]

with zipfile.ZipFile(archive) as source:
    for member in source.infolist():
        if not member.filename.startswith(prefix):
            raise SystemExit(
                f"error: unexpected Brotli archive path: {member.filename}"
            )
        relative = member.filename[len(prefix):]
        if not relative:
            continue
        target = destination / relative
        if target.resolve() != destination.resolve() \
                and destination.resolve() not in target.resolve().parents:
            raise SystemExit(
                f"error: unsafe Brotli archive path: {member.filename}"
            )
        if member.is_dir():
            target.mkdir(parents=True, exist_ok=True)
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        with source.open(member) as input_file, target.open("wb") as output_file:
            output_file.write(input_file.read())
PY

[[ -f "$stage/src/c/include/brotli/shared_dictionary.h" ]] || {
    echo "error: pinned Brotli source lacks shared_dictionary.h" >&2
    exit 1
}

cmake -S "$stage/src" -B "$stage/build" \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_POSITION_INDEPENDENT_CODE=ON \
    -DBROTLI_DISABLE_TESTS=ON \
    -DBUILD_SHARED_LIBS=OFF
cmake --build "$stage/build" --parallel "$JOBS"

for library in common dec enc; do
    source_archive="$stage/build/libbrotli${library}-static.a"
    [[ -f "$source_archive" ]] || {
        echo "error: Brotli build did not produce $source_archive" >&2
        exit 1
    }
    cp "$source_archive" "$stage/libbrotli${library}.a"
done

printf '%s\n' "${DEST}/src/c/include" > "$stage/INCLUDE_ROOT"
printf '%s\n' "$BROTLI_COMMIT" > "$stage/.rush-bootstrap-commit"
mv "$stage" "$DEST"
trap 'rm -f "$archive"' EXIT
echo "pinned Brotli ready: $DEST"
