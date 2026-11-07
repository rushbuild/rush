#!/usr/bin/env bash
# Materialize TensorFlow's pinned DUCC source and threading overlay.

set -euo pipefail

DUCC_COMMIT="${DUCC_COMMIT:-3d28aadfd8bb0219e3df188613dbbcdfffccc3cd}"
DUCC_SHA256="${DUCC_SHA256:-eb044dd11374ed894d67081109d4aa7ed55c29fe3286b116f13db70da6af336c}"
DUCC_URL="https://gitlab.mpcdf.mpg.de/mtr/ducc/-/archive/${DUCC_COMMIT}/ducc-${DUCC_COMMIT}.tar.gz"
DUCC_MIRROR_URL="https://storage.googleapis.com/mirror.tensorflow.org/gitlab.mpcdf.mpg.de/mtr/ducc/-/archive/${DUCC_COMMIT}/ducc-${DUCC_COMMIT}.tar.gz"
SCRIPT_ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
TF_WORKSPACE="${RUSH_TF_WORKSPACE:-${SCRIPT_ROOT}/tensorflow}"
OVERLAY="${TF_WORKSPACE}/third_party/ducc"
CACHE_ROOT="${RUSH_GLOBAL_CLONE_CACHE:-/data/rush-cache/external}"
DEST="${CACHE_ROOT}/ducc"
STAMP="${DEST}/.rush-bootstrap-commit"

MODE=""
FORCE=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run) MODE="dry-run"; shift ;;
        --apply) MODE="apply"; shift ;;
        --force) FORCE=1; shift ;;
        -h|--help)
            sed -n '2,55p' "$0"
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
=== bootstrap_ducc plan ===
commit      : ${DUCC_COMMIT}
TF workspace: ${TF_WORKSPACE}
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

for file in threading.h threading.cc ducc0_custom_lowlevel_threading.h; do
    [[ -f "${OVERLAY}/${file}" ]] || {
        echo "error: TensorFlow DUCC overlay is missing: ${OVERLAY}/${file}" >&2
        exit 1
    }
done
[[ -f "${OVERLAY}/ducc.BUILD" ]] || {
    echo "error: TensorFlow DUCC BUILD overlay is missing" >&2
    exit 1
}

if [[ -f "$STAMP" ]] \
    && [[ "$(cat "$STAMP")" == "$DUCC_COMMIT" ]] \
    && [[ -f "$DEST/google/threading.h" ]] \
    && [[ -f "$DEST/google/threading.cc" ]] \
    && [[ -f "$DEST/google/ducc0_custom_lowlevel_threading.h" ]] \
    && [[ -f "$DEST/src/ducc0/infra/threading.h" ]] \
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
archive="$(mktemp "${CACHE_ROOT}/.ducc.XXXXXX.tar.gz")"
extract_root="$(mktemp -d "${CACHE_ROOT}/.ducc.XXXXXX")"
trap 'rm -f "$archive"; rm -rf "$extract_root"' EXIT

curl --fail --silent --show-error --location --output "$archive" \
    "$DUCC_MIRROR_URL" \
    || curl --fail --silent --show-error --location --output "$archive" \
        "$DUCC_URL"
actual_sha="$(sha256sum "$archive" | awk '{print $1}')"
[[ "$actual_sha" == "$DUCC_SHA256" ]] || {
    echo "error: DUCC sha256 mismatch" >&2
    echo "  expected: $DUCC_SHA256" >&2
    echo "  actual:   $actual_sha" >&2
    exit 1
}

tar -xzf "$archive" -C "$extract_root" --strip-components=1
[[ -f "$extract_root/src/ducc0/infra/threading.h" ]] || {
    echo "error: pinned DUCC source lacks its threading header" >&2
    exit 1
}
[[ -f "$extract_root/src/ducc0/infra/threading.cc" ]] || {
    echo "error: pinned DUCC source lacks its threading implementation" >&2
    exit 1
}

mkdir -p "$extract_root/google"
cp -f "$OVERLAY/threading.h" "$extract_root/google/threading.h"
cp -f "$OVERLAY/threading.cc" "$extract_root/google/threading.cc"
cp -f "$OVERLAY/ducc0_custom_lowlevel_threading.h" \
    "$extract_root/google/ducc0_custom_lowlevel_threading.h"
cp -f "$OVERLAY/ducc.BUILD" "$extract_root/BUILD.bazel"

mkdir -p "$DEST"
cp -a "$extract_root"/. "$DEST"/
printf '%s\n' "$DUCC_COMMIT" > "$STAMP"
echo "pinned DUCC ready: $DEST"
