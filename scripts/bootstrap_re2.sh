#!/usr/bin/env bash
# Materialize TensorFlow's pinned RE2 source for Rush.
#
# TensorFlow pins RE2 commit 03da4fc0 and compiles it against TensorFlow's
# pinned Abseil. A system-RE2 substitution is not ABI-compatible when the
# distro RE2 was built against a different Abseil release. Rush registers this
# real source tree and evaluates its upstream BUILD.bazel, matching Bazel's
# source dependency rather than mixing system and vendored C++ types.
set -euo pipefail

RE2_COMMIT="${RE2_COMMIT:-03da4fc0857c285e3a26782f6bc8931c4c950df4}"
RE2_SHA256="${RE2_SHA256:-ef516fb84824a597c4d5d0d6d330daedb18363b5a99eda87d027e6bdd9cba299}"
RE2_URL="https://github.com/google/re2/archive/${RE2_COMMIT}.tar.gz"
RE2_MIRROR_URL="https://storage.googleapis.com/mirror.tensorflow.org/github.com/google/re2/archive/${RE2_COMMIT}.tar.gz"
CACHE_ROOT="${RUSH_GLOBAL_CLONE_CACHE:-/data/rush-cache/external}"
DEST="${CACHE_ROOT}/com_googlesource_code_re2"
STAMP="${DEST}/.rush-bootstrap-commit"

MODE=""
FORCE=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run) MODE="dry-run"; shift ;;
        --apply) MODE="apply"; shift ;;
        --force) FORCE=1; shift ;;
        -h|--help)
            sed -n '2,35p' "$0"
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
=== bootstrap_re2 plan ===
commit      : ${RE2_COMMIT}
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
    && [[ "$(cat "$STAMP")" == "$RE2_COMMIT" ]] \
    && [[ -f "$DEST/re2/re2.h" ]] \
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
tarball="$(mktemp "${CACHE_ROOT}/.re2.XXXXXX.tar.gz")"
trap 'rm -f "$tarball"' EXIT

curl --fail --silent --show-error --location --output "$tarball" \
    "$RE2_MIRROR_URL" \
    || curl --fail --silent --show-error --location --output "$tarball" \
        "$RE2_URL"
actual_sha="$(sha256sum "$tarball" | awk '{print $1}')"
[[ "$actual_sha" == "$RE2_SHA256" ]] || {
    echo "error: RE2 sha256 mismatch" >&2
    echo "  expected: $RE2_SHA256" >&2
    echo "  actual:   $actual_sha" >&2
    exit 1
}

tar -xzf "$tarball" -C "$DEST" --strip-components=1
[[ -f "$DEST/re2/re2.h" ]] || {
    echo "error: expected header not produced: $DEST/re2/re2.h" >&2
    exit 1
}
[[ -f "$DEST/BUILD.bazel" ]] || {
    echo "error: expected BUILD.bazel not produced: $DEST/BUILD.bazel" >&2
    exit 1
}
printf '%s\n' "$RE2_COMMIT" > "$STAMP"
echo "pinned RE2 source ready: $DEST"
