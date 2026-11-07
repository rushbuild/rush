#!/usr/bin/env bash
# Bootstrap the REAL @absl_py (abseil-py) Python source.
#
# TensorFlow's @absl_py is a tf_http_archive of abseil-py v1.0.0
# (tensorflow/workspace2.bzl). py_strict_binary tools used by genrules — e.g.
# //tensorflow/compiler/mlir/quantization/tensorflow:gen_quantized_function_library
# — `from absl import app/flags` and call app.run()/flags.DEFINE_*. rush had only
# an empty BUILD-file stub for @absl_py (external_deps.rs), so the real absl
# Python package was absent and the tools could not run. This materializes the
# genuine abseil-py source (verified by SHA256) so those tools execute for real.
# NO STUBS: this is the actual upstream source TF pins, not a placeholder.
set -euo pipefail

VERSION="1.0.0"
URL="https://github.com/abseil/abseil-py/archive/refs/tags/v${VERSION}.tar.gz"
SHA256="a7c51b2a0aa6357a9cbb2d9437e8cd787200531867dc02565218930b6a32166e"
STRIP_PREFIX="abseil-py-${VERSION}"

OUT="${RUSH_ABSL_PY_OUT:-${RUSH_GLOBAL_CLONE_CACHE:-/data/rush-cache/external}/absl_py}"
echo "abseil-py version: ${VERSION}"
echo "output dir:        ${OUT}"

if [ -f "${OUT}/absl/app.py" ]; then
    echo "OK: ${OUT}/absl/app.py already present — nothing to do"
    exit 0
fi

TMP="$(mktemp -d)"
trap 'rm -rf "${TMP}"' EXIT

TARBALL="${TMP}/absl_py.tar.gz"
echo "fetching ${URL}"
if command -v curl >/dev/null 2>&1; then
    curl -fsSL "${URL}" -o "${TARBALL}"
else
    wget -qO "${TARBALL}" "${URL}"
fi

echo "verifying SHA256"
ACTUAL="$(sha256sum "${TARBALL}" | cut -d' ' -f1)"
if [ "${ACTUAL}" != "${SHA256}" ]; then
    echo "ERROR: SHA256 mismatch: expected ${SHA256}, got ${ACTUAL}" >&2
    exit 1
fi
echo "OK: SHA256 verified"

tar -xzf "${TARBALL}" -C "${TMP}"
SRC="${TMP}/${STRIP_PREFIX}"
[ -f "${SRC}/absl/app.py" ] || { echo "ERROR: ${SRC}/absl/app.py not in tarball" >&2; exit 1; }

mkdir -p "${OUT}"
# Copy the real absl/ package tree (the importable Python source).
rm -rf "${OUT}/absl"
cp -a "${SRC}/absl" "${OUT}/absl"

if [ ! -f "${OUT}/absl/app.py" ] || [ ! -f "${OUT}/absl/flags/__init__.py" ]; then
    echo "ERROR: materialized @absl_py missing app.py / flags/__init__.py" >&2
    exit 1
fi
echo "built: ${OUT}/absl ($(find "${OUT}/absl" -name '*.py' | wc -l) .py files)"
echo "OK: real abseil-py source materialized at ${OUT}"
