#!/usr/bin/env bash
# Materialize TensorFlow's pinned Gemmlowp source for Rush external resolution.

set -euo pipefail

GEMMLOWP_COMMIT="${GEMMLOWP_COMMIT:-e844ffd17118c1e17d94e1ba4354c075a4577b88}"
GEMMLOWP_SHA256="${GEMMLOWP_SHA256:-522b7a82d920ebd0c4408a5365866a40b81d1c0d60b2369011d315cca03c6476}"
GEMMLOWP_URL="https://github.com/google/gemmlowp/archive/${GEMMLOWP_COMMIT}.zip"
GEMMLOWP_MIRROR_URL="https://storage.googleapis.com/mirror.tensorflow.org/github.com/google/gemmlowp/archive/${GEMMLOWP_COMMIT}.zip"
CACHE_ROOT="${RUSH_GLOBAL_CLONE_CACHE:-/data/rush-cache/external}"
DEST="${CACHE_ROOT}/gemmlowp"
STAMP="${DEST}/.rush-bootstrap-commit"

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
=== bootstrap_gemmlowp plan ===
commit      : ${GEMMLOWP_COMMIT}
cache root  : ${CACHE_ROOT}
destination : ${DEST}
EOF

if [[ "$MODE" == "dry-run" ]]; then
    exit 0
fi

for tool in curl python3 sha256sum; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "error: required tool not on PATH: $tool" >&2
        exit 2
    }
done

if [[ -f "$STAMP" ]] \
    && [[ "$(cat "$STAMP")" == "$GEMMLOWP_COMMIT" ]] \
    && [[ -f "$DEST/public/gemmlowp.h" ]] \
    && [[ -f "$DEST/fixedpoint/fixedpoint.h" ]] \
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
archive="$(mktemp "${CACHE_ROOT}/.gemmlowp.XXXXXX.zip")"
extract_root="$(mktemp -d "${CACHE_ROOT}/.gemmlowp.XXXXXX")"
trap 'rm -f "$archive"; rm -rf "$extract_root"' EXIT

curl --fail --silent --show-error --location --output "$archive" \
    "$GEMMLOWP_MIRROR_URL" \
    || curl --fail --silent --show-error --location --output "$archive" \
        "$GEMMLOWP_URL"
actual_sha="$(sha256sum "$archive" | awk '{print $1}')"
[[ "$actual_sha" == "$GEMMLOWP_SHA256" ]] || {
    echo "error: Gemmlowp sha256 mismatch" >&2
    echo "  expected: $GEMMLOWP_SHA256" >&2
    echo "  actual:   $actual_sha" >&2
    exit 1
}

python3 - "$archive" "$extract_root" \
    "gemmlowp-${GEMMLOWP_COMMIT}/" <<'PY'
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
                f"error: unexpected Gemmlowp archive path: {member.filename}"
            )
        relative = member.filename[len(prefix):]
        if not relative:
            continue
        target = destination / relative
        if target.resolve() != destination.resolve() \
                and destination.resolve() not in target.resolve().parents:
            raise SystemExit(
                f"error: unsafe Gemmlowp archive path: {member.filename}"
            )
        if member.is_dir():
            target.mkdir(parents=True, exist_ok=True)
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        with source.open(member) as input_file, target.open("wb") as output_file:
            output_file.write(input_file.read())
PY

[[ -f "$extract_root/public/gemmlowp.h" ]] || {
    echo "error: pinned Gemmlowp source lacks public/gemmlowp.h" >&2
    exit 1
}
[[ -f "$extract_root/fixedpoint/fixedpoint.h" ]] || {
    echo "error: pinned Gemmlowp source lacks fixedpoint/fixedpoint.h" >&2
    exit 1
}
[[ -f "$extract_root/BUILD" || -f "$extract_root/BUILD.bazel" ]] || {
    echo "error: pinned Gemmlowp source lacks an upstream BUILD file" >&2
    exit 1
}

mkdir -p "$DEST"
cp -a "$extract_root"/. "$DEST"/
printf '%s\n' "$GEMMLOWP_COMMIT" > "$STAMP"
echo "pinned Gemmlowp ready: $DEST"
