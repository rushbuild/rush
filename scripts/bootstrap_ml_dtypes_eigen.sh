#!/usr/bin/env bash
# Rush — bootstrap the TF-pinned ml_dtypes + eigen_archive
# clones for Lane C (Eigen materialization).
#
# Phase 5.C.4 commit 268 (Lane C step C2). Per Pro
# 2026-05-26 directive: explicit, non-magical
# materialization of the two repos `@ml_dtypes` and
# `@eigen_archive` depend on. Does NOT auto-run during
# `rush build`. Operators invoke this script once on a
# fresh host.
#
# Pro's C2 spec:
#   - reads pinned metadata from a checked-in config
#   - fetches/clones only ml_dtypes_eigen
#   - writes into the expected external path
#   - refuses to overwrite non-empty dir unless --force
#   - prints exact include root
#   - does not run automatically during rush build
#
# USAGE:
#   scripts/bootstrap_ml_dtypes_eigen.sh --dry-run
#   scripts/bootstrap_ml_dtypes_eigen.sh --apply
#   scripts/bootstrap_ml_dtypes_eigen.sh --apply --force
#
# Env knobs:
#   RUSH_GLOBAL_CLONE_CACHE   (default: /data/rush-cache/external)
#
# EXIT CODES:
#   0 — success (apply) or plan printed (dry-run)
#   1 — overwrite refused, sha256 mismatch, or fetch
#       failed
#   2 — environment problem (missing tools, invalid args)

set -euo pipefail

# ---------------------------------------------------------
# TF-pinned metadata. Extracted from
# tensorflow/third_party/{eigen3,py/ml_dtypes}/workspace.bzl.
# Bumping these requires re-extracting from TF's pin files
# AND updating docs/analysis/ml_dtypes-eigen-materialization-plan.md.
# ---------------------------------------------------------

# Commit + sha256 are env-overridable so the same bootstrap serves multiple TF
# pins (e.g. the A1 TF-2.17 climb: ML_DTYPES_COMMIT=24084d9e... which renamed
# int4.h -> intn.h, a header the 2.15 pin 2ca30a2b lacks). Defaults = TF 2.15.
ML_DTYPES_COMMIT="${ML_DTYPES_COMMIT:-2ca30a2b3c0744625ae3d6988f5596740080bbd0}"
ML_DTYPES_SHA256="${ML_DTYPES_SHA256:-5ea2b9ab133ddd522bcdf2f9e77ac277bb19d3fa4442bd77ee024fc225f0b5ab}"
ML_DTYPES_URL="https://github.com/jax-ml/ml_dtypes/archive/${ML_DTYPES_COMMIT}/ml_dtypes-${ML_DTYPES_COMMIT}.tar.gz"
# Strip prefix per TF's workspace.bzl: "ml_dtypes-<commit>/ml_dtypes".
# The trailing /ml_dtypes is what makes the clone root contain
# include/, _src/, etc. directly (rather than wrapping them in
# another ml_dtypes/ directory).
ML_DTYPES_STRIP="ml_dtypes-${ML_DTYPES_COMMIT}/ml_dtypes"

EIGEN_COMMIT="${EIGEN_COMMIT:-66e8f38891841bf88ee976a316c0c78a52f0cee5}"
EIGEN_SHA256="${EIGEN_SHA256:-01fcd68409c038bbcfd16394274c2bf71e2bb6dda89a2319e23fc59a2da17210}"
EIGEN_URL="https://gitlab.com/libeigen/eigen/-/archive/${EIGEN_COMMIT}/eigen-${EIGEN_COMMIT}.tar.gz"
EIGEN_STRIP="eigen-${EIGEN_COMMIT}"

# ---------------------------------------------------------
# Arg parsing
# ---------------------------------------------------------

MODE=""
FORCE=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run) MODE="dry-run"; shift ;;
        --apply)   MODE="apply"; shift ;;
        --force)   FORCE=1; shift ;;
        -h|--help)
            sed -n '2,32p' "$0"
            exit 0
            ;;
        *)
            echo "error: unknown arg: $1" >&2
            exit 2
            ;;
    esac
done

if [[ -z "$MODE" ]]; then
    echo "error: must specify --dry-run or --apply" >&2
    exit 2
fi

# ---------------------------------------------------------
# Resolve clone cache root.
# ---------------------------------------------------------

CACHE_ROOT="${RUSH_GLOBAL_CLONE_CACHE:-/data/rush-cache/external}"

EIGEN_DEST="${CACHE_ROOT}/eigen_archive"
ML_DTYPES_DEST="${CACHE_ROOT}/ml_dtypes"

# ---------------------------------------------------------
# Tool checks
# ---------------------------------------------------------

require_tool() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "error: required tool not on PATH: $1" >&2
        exit 2
    fi
}
require_tool curl
require_tool tar
require_tool sha256sum
require_tool mkdir

# ---------------------------------------------------------
# Fetch helper
# ---------------------------------------------------------

# fetch_and_extract <name> <url> <sha256> <strip_prefix> <dest>
#   Downloads URL to a tmpdir, verifies sha256, extracts with
#   strip-prefix, moves into dest.
fetch_and_extract() {
    local name="$1" url="$2" sha256="$3" strip="$4" dest="$5"

    local tmpdir
    tmpdir="$(mktemp -d)"
    trap "rm -rf '$tmpdir'" RETURN

    local tarball="${tmpdir}/${name}.tar.gz"

    echo "  fetching $url" >&2
    if ! curl --fail --silent --show-error --location \
        --output "$tarball" "$url"; then
        echo "error: fetch failed for $name" >&2
        return 1
    fi

    local actual_sha
    actual_sha="$(sha256sum "$tarball" | awk '{print $1}')"
    if [[ "$actual_sha" != "$sha256" ]]; then
        echo "error: sha256 mismatch for $name" >&2
        echo "  expected: $sha256" >&2
        echo "  actual:   $actual_sha" >&2
        return 1
    fi
    echo "  sha256 OK: $actual_sha" >&2

    # Compute strip-prefix component count.
    local strip_n
    strip_n="$(echo "$strip" | awk -F/ '{print NF}')"

    local extracted_dir="${tmpdir}/extracted"
    mkdir -p "$extracted_dir"
    tar -xzf "$tarball" -C "$extracted_dir" --strip-components="$strip_n"

    # Atomic move into dest.
    mkdir -p "$(dirname "$dest")"
    if [[ -d "$dest" ]]; then
        rm -rf "$dest"
    fi
    mv "$extracted_dir" "$dest"

    echo "  wrote: $dest" >&2
}

# ---------------------------------------------------------
# Overwrite-protection check
# ---------------------------------------------------------

check_overwrite() {
    local dest="$1"
    if [[ -d "$dest" && -n "$(ls -A "$dest" 2>/dev/null)" ]]; then
        if [[ $FORCE -eq 0 ]]; then
            echo "error: $dest is non-empty; use --force to overwrite" >&2
            return 1
        fi
        echo "  --force: existing $dest will be overwritten" >&2
    fi
}

# ---------------------------------------------------------
# Dry-run vs apply
# ---------------------------------------------------------

cat <<HEADER >&2
─────────────────────────────────────────────────────────────
bootstrap_ml_dtypes_eigen.sh ($MODE)
  cache root: $CACHE_ROOT
  eigen dest: $EIGEN_DEST
  ml_dtypes dest: $ML_DTYPES_DEST
  force overwrite: $FORCE
─────────────────────────────────────────────────────────────
HEADER

if [[ "$MODE" == "dry-run" ]]; then
    cat <<DRYRUN
# Would fetch and verify @eigen_archive:
curl --fail --silent --show-error --location \\
    --output /tmp/eigen.tar.gz \\
    "$EIGEN_URL"
echo "$EIGEN_SHA256  /tmp/eigen.tar.gz" | sha256sum --check
mkdir -p "$EIGEN_DEST"
tar -xzf /tmp/eigen.tar.gz \\
    -C "$EIGEN_DEST" \\
    --strip-components=1

# Would fetch and verify @ml_dtypes:
curl --fail --silent --show-error --location \\
    --output /tmp/ml_dtypes.tar.gz \\
    "$ML_DTYPES_URL"
echo "$ML_DTYPES_SHA256  /tmp/ml_dtypes.tar.gz" | sha256sum --check
mkdir -p "$ML_DTYPES_DEST"
tar -xzf /tmp/ml_dtypes.tar.gz \\
    -C "$ML_DTYPES_DEST" \\
    --strip-components=2

# Expected include roots after extraction:
#   $EIGEN_DEST/Eigen/Core    (i.e. -I$EIGEN_DEST)
#   $ML_DTYPES_DEST/include/float8.h   (i.e. -I$ML_DTYPES_DEST)
# Rush's external_deps.rs ml_dtypes stub generator emits
# 'includes = [".", "ml_dtypes"]' which makes the consumer
# path 'ml_dtypes/float8.h' resolve to
# $ML_DTYPES_DEST/include/float8.h via include_prefix.
DRYRUN
    exit 0
fi

# Apply mode
check_overwrite "$EIGEN_DEST" || exit 1
check_overwrite "$ML_DTYPES_DEST" || exit 1

echo "→ fetching @eigen_archive..." >&2
fetch_and_extract \
    "eigen_archive" \
    "$EIGEN_URL" \
    "$EIGEN_SHA256" \
    "$EIGEN_STRIP" \
    "$EIGEN_DEST"

echo "→ fetching @ml_dtypes..." >&2
fetch_and_extract \
    "ml_dtypes" \
    "$ML_DTYPES_URL" \
    "$ML_DTYPES_SHA256" \
    "$ML_DTYPES_STRIP" \
    "$ML_DTYPES_DEST"

# Sanity-check that the expected headers landed.
if [[ ! -f "${EIGEN_DEST}/Eigen/Core" ]]; then
    echo "warning: ${EIGEN_DEST}/Eigen/Core not found — layout may differ from expected" >&2
fi
if [[ ! -f "${ML_DTYPES_DEST}/include/float8.h" ]]; then
    echo "warning: ${ML_DTYPES_DEST}/include/float8.h not found — layout may differ from expected" >&2
fi

cat <<DONE >&2
─────────────────────────────────────────────────────────────
Bootstrap complete.

Eigen include root:     $EIGEN_DEST
ml_dtypes include root: $ML_DTYPES_DEST

Rush's @eigen_archive and @ml_dtypes stub generators
will probe these paths via resolve_clone_path().
RUSH_GLOBAL_CLONE_CACHE=$CACHE_ROOT is the env knob if
the cache root needs to vary at build time.
─────────────────────────────────────────────────────────────
DONE
