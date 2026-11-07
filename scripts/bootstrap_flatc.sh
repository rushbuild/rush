#!/bin/bash
# Bootstrap the REAL TF-pinned flatc (flatbuffers compiler).
#
# rush's flatbuffer_cc_library expander runs `flatc` from PATH (evaluator.rs:
# "@flatbuffers//:flatc is stubbed so $(location) resolution doesn't work").
# The distro flatc (Debian flatbuffers-compiler) is v23.5.26, but TF 2.17 pins
# flatbuffers at commit e6463926 (v24): the generated *_generated.h then fails a
# `static_assert(FLATBUFFERS_VERSION_MAJOR == ...)` against the v24 headers
# ("Non-compatible flatbuffers version included"). Build the matching v24 flatc
# from the pinned source and install it under the clone cache; prepend its dir to
# PATH for the rush build so genrules use the version-matched compiler. NO STUBS:
# this is the genuine TF-pinned flatc compiled from source.
set -euo pipefail

FLATBUFFERS_COMMIT="${FLATBUFFERS_COMMIT:-e6463926479bd6b330cbcf673f7e917803fd5831}"
CACHE_ROOT="${RUSH_GLOBAL_CLONE_CACHE:-/data/rush-cache/external}"
OUT="${CACHE_ROOT}/flatbuffers_flatc"
URL="https://github.com/google/flatbuffers/archive/${FLATBUFFERS_COMMIT}.tar.gz"

if [ -x "${OUT}/flatc" ]; then
    echo "OK: ${OUT}/flatc already present:"
    "${OUT}/flatc" --version
    exit 0
fi

command -v cmake >/dev/null || { echo "ERROR: cmake required to build flatc" >&2; exit 1; }
command -v g++   >/dev/null || { echo "ERROR: g++ required to build flatc" >&2; exit 1; }

TMP="$(mktemp -d)"
trap 'rm -rf "${TMP}"' EXIT
echo "==> fetching flatbuffers ${FLATBUFFERS_COMMIT}"
curl -fsSL "${URL}" -o "${TMP}/fb.tar.gz"
tar -xzf "${TMP}/fb.tar.gz" -C "${TMP}"
SRC="${TMP}/flatbuffers-${FLATBUFFERS_COMMIT}"
[ -f "${SRC}/CMakeLists.txt" ] || { echo "ERROR: flatbuffers CMakeLists.txt not in tarball" >&2; exit 1; }

echo "==> building flatc (cmake)"
cmake -S "${SRC}" -B "${SRC}/build" \
    -DCMAKE_BUILD_TYPE=Release \
    -DFLATBUFFERS_BUILD_TESTS=OFF \
    -DFLATBUFFERS_BUILD_FLATC=ON \
    -DFLATBUFFERS_BUILD_FLATLIB=OFF \
    -DFLATBUFFERS_BUILD_FLATHASH=OFF >/dev/null
cmake --build "${SRC}/build" --target flatc -j "$(nproc)" >/dev/null

[ -x "${SRC}/build/flatc" ] || { echo "ERROR: flatc not built" >&2; exit 1; }
mkdir -p "${OUT}"
cp -a "${SRC}/build/flatc" "${OUT}/flatc"

# Also materialize the matching v24 flatbuffers HEADERS. The generated *_generated.h
# static-asserts FLATBUFFERS_VERSION_MAJOR against flatbuffers/base.h, so consumers
# must compile against the SAME-version headers as the flatc that generated them — not
# the distro v23 /usr/include/flatbuffers. rush -I's ${OUT}/include before the system path.
rm -rf "${OUT}/include"
cp -a "${SRC}/include" "${OUT}/include"
[ -f "${OUT}/include/flatbuffers/base.h" ] || { echo "ERROR: v24 flatbuffers headers not materialized" >&2; exit 1; }

echo "OK: built flatc + materialized v24 headers at ${OUT}"
"${OUT}/flatc" --version
grep -oE "FLATBUFFERS_VERSION_MAJOR [0-9]+" "${OUT}/include/flatbuffers/base.h" | head -1
echo "PATH knob: prepend ${OUT} to PATH; -I ${OUT}/include for the v24 headers."
