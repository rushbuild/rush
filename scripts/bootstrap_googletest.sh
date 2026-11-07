#!/usr/bin/env bash
# Materialize TensorFlow's pinned GoogleTest source for Rush.
#
# TensorFlow 2.15 pins GoogleTest release 1.12.1. Rush evaluates the real
# upstream BUILD.bazel so gtest and gmock headers, sources, and include roots
# flow through the normal CcInfo graph instead of relying on host packages.
set -euo pipefail

GTEST_VERSION="${GTEST_VERSION:-1.12.1}"
GTEST_SHA256="${GTEST_SHA256:-81964fe578e9bd7c94dfdb09c8e4d6e6759e19967e397dbea48d1c10e45d0df2}"
GTEST_URL="https://github.com/google/googletest/archive/refs/tags/release-${GTEST_VERSION}.tar.gz"
GTEST_MIRROR_URL="https://storage.googleapis.com/mirror.tensorflow.org/github.com/google/googletest/archive/refs/tags/release-${GTEST_VERSION}.tar.gz"
CACHE_ROOT="${RUSH_GLOBAL_CLONE_CACHE:-/data/rush-cache/external}"
DEST="${CACHE_ROOT}/com_google_googletest"
STAMP="${DEST}/.rush-bootstrap-version"

MODE=""
FORCE=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run) MODE="dry-run"; shift ;;
        --apply) MODE="apply"; shift ;;
        --force) FORCE=1; shift ;;
        -h|--help)
            sed -n '2,32p' "$0"
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
=== bootstrap_googletest plan ===
version     : ${GTEST_VERSION}
cache root  : ${CACHE_ROOT}
destination : ${DEST}
EOF

if [[ "$MODE" == "dry-run" ]]; then
    exit 0
fi

for tool in curl sha256sum tar; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "error: required tool not on PATH: $tool" >&2
        exit 2
    }
done

if [[ -f "$STAMP" ]] \
    && [[ "$(cat "$STAMP")" == "$GTEST_VERSION" ]] \
    && [[ -f "$DEST/googletest/include/gtest/gtest.h" ]] \
    && [[ -f "$DEST/BUILD.bazel" ]] \
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
tarball="$(mktemp "${CACHE_ROOT}/.googletest.XXXXXX.tar.gz")"
trap 'rm -f "$tarball"' EXIT

curl --fail --silent --show-error --location --output "$tarball" \
    "$GTEST_MIRROR_URL" \
    || curl --fail --silent --show-error --location --output "$tarball" \
        "$GTEST_URL"
actual_sha="$(sha256sum "$tarball" | awk '{print $1}')"
[[ "$actual_sha" == "$GTEST_SHA256" ]] || {
    echo "error: GoogleTest sha256 mismatch" >&2
    echo "  expected: $GTEST_SHA256" >&2
    echo "  actual:   $actual_sha" >&2
    exit 1
}

tar -xzf "$tarball" -C "$DEST" --strip-components=1
[[ -f "$DEST/googletest/include/gtest/gtest.h" ]] || {
    echo "error: expected gtest header not produced" >&2
    exit 1
}
[[ -f "$DEST/BUILD.bazel" ]] || {
    echo "error: expected BUILD.bazel not produced: $DEST/BUILD.bazel" >&2
    exit 1
}
printf '%s\n' "$GTEST_VERSION" > "$STAMP"
echo "pinned GoogleTest source ready: $DEST"
