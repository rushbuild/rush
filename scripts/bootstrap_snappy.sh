#!/usr/bin/env bash
# Build TensorFlow's pinned Snappy with the legacy iovec API TF 2.15 uses.

set -euo pipefail

SNAPPY_COMMIT="${SNAPPY_COMMIT:-984b191f0fefdeb17050b42a90b7625999c13b8d}"
SNAPPY_SHA256="${SNAPPY_SHA256:-2e458b7017cd58dcf1469ab315389e85e7f445bd035188f2983f81fb19ecfb29}"
SNAPPY_URL="https://github.com/google/snappy/archive/${SNAPPY_COMMIT}.tar.gz"
SNAPPY_MIRROR_URL="https://storage.googleapis.com/mirror.tensorflow.org/github.com/google/snappy/archive/${SNAPPY_COMMIT}.tar.gz"
CACHE_ROOT="${RUSH_GLOBAL_CLONE_CACHE:-/data/rush-cache/external}"
DEST="${CACHE_ROOT}/snappy"
STAMP="${DEST}/.rush-bootstrap-commit"
STAMP_VALUE="${SNAPPY_COMMIT}-rtti"
JOBS="${RUSH_SNAPPY_BUILD_JOBS:-$(nproc)}"

MODE=""
FORCE=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run) MODE="dry-run"; shift ;;
        --apply) MODE="apply"; shift ;;
        --force) FORCE=1; shift ;;
        -h|--help)
            sed -n '2,45p' "$0"
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
=== bootstrap_snappy plan ===
commit      : ${SNAPPY_COMMIT}
cache root  : ${CACHE_ROOT}
destination : ${DEST}
build jobs  : ${JOBS}
EOF

if [[ "$MODE" == "dry-run" ]]; then
    exit 0
fi

for tool in cmake curl sha256sum tar; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "error: required tool not on PATH: $tool" >&2
        exit 2
    }
done

if [[ -f "$STAMP" ]] \
    && [[ "$(cat "$STAMP")" == "$STAMP_VALUE" ]] \
    && [[ -f "$DEST/build/libsnappy.a" ]] \
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

mkdir -p "$CACHE_ROOT" "$DEST"
tarball="$(mktemp "${CACHE_ROOT}/.snappy.XXXXXX.tar.gz")"
trap 'rm -f "$tarball"' EXIT

curl --fail --silent --show-error --location --output "$tarball" \
    "$SNAPPY_MIRROR_URL" \
    || curl --fail --silent --show-error --location --output "$tarball" \
        "$SNAPPY_URL"
actual_sha="$(sha256sum "$tarball" | awk '{print $1}')"
[[ "$actual_sha" == "$SNAPPY_SHA256" ]] || {
    echo "error: Snappy sha256 mismatch" >&2
    echo "  expected: $SNAPPY_SHA256" >&2
    echo "  actual:   $actual_sha" >&2
    exit 1
}
tar -xzf "$tarball" -C "$DEST" --strip-components=1

grep -q 'RawCompressFromIOVec' "$DEST/snappy.h" || {
    echo "error: pinned Snappy source lacks RawCompressFromIOVec" >&2
    exit 1
}

# The pinned CMake build disables RTTI, but TensorFlow's Bazel target does not.
# Riegeli's Snappy streams consequently reference typeinfo for snappy::Sink and
# snappy::Source. Keep the bootstrapped archive ABI-compatible with Bazel.
grep -q 'CMAKE_CXX_FLAGS} -fno-rtti' "$DEST/CMakeLists.txt" || {
    echo "error: pinned Snappy CMake RTTI setting changed" >&2
    exit 1
}
sed -i 's/CMAKE_CXX_FLAGS} -fno-rtti/CMAKE_CXX_FLAGS} -frtti/' \
    "$DEST/CMakeLists.txt"

cmake -S "$DEST" -B "$DEST/build" \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_POSITION_INDEPENDENT_CODE=ON \
    -DBUILD_SHARED_LIBS=OFF \
    -DSNAPPY_BUILD_TESTS=OFF \
    -DSNAPPY_BUILD_BENCHMARKS=OFF
cmake --build "$DEST/build" --parallel "$JOBS"

[[ -f "$DEST/build/libsnappy.a" ]] || {
    echo "error: expected archive not produced: $DEST/build/libsnappy.a" >&2
    exit 1
}
[[ -f "$DEST/build/snappy-stubs-public.h" ]] || {
    echo "error: generated public header not produced" >&2
    exit 1
}
printf '%s\n' "$STAMP_VALUE" > "$STAMP"
echo "pinned Snappy ready: $DEST/build/libsnappy.a"
