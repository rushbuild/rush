#!/bin/bash
# Bootstrap the REAL TF-pinned C++ abseil-cpp (headers + static libs).
#
# rush's @com_google_absl stub compiles/links against the SYSTEM Debian absl, but
# the host absl has drifted (lts_20240722+, and dropped the deprecated absl::Nonnull
# alias) past TF 2.17's pin fb3621f4 (= lts_20230802). Compiling TF against the
# pinned headers (via CPLUS_INCLUDE_PATH) then linking system libabsl yields ~608
# undefined refs to absl::lts_20230802::* (Status::Unref etc.) — a header/lib ABI
# skew. Build the pinned absl from source so headers AND libs are the SAME lts, and
# rush links these instead of system -labsl. NO STUBS — genuine pinned abseil-cpp.
set -euo pipefail

ABSL_COMMIT="${ABSL_COMMIT:-fb3621f4f897824c0dbe0615fa94543df6192f30}"
CACHE_ROOT="${RUSH_GLOBAL_CLONE_CACHE:-/data/rush-cache/external}"
SRC="${CACHE_ROOT}/abseil-cpp"          # clone (headers) — already materialized
PREFIX="${CACHE_ROOT}/abseil-cpp/install"  # built libs + headers install tree

if [ -f "${PREFIX}/lib/libabsl_strings.a" ] || [ -f "${PREFIX}/lib64/libabsl_strings.a" ]; then
    echo "OK: pinned absl already built at ${PREFIX}"
    exit 0
fi

command -v cmake >/dev/null || { echo "ERROR: cmake required" >&2; exit 1; }
if [ ! -f "${SRC}/CMakeLists.txt" ]; then
    echo "==> cloning abseil-cpp ${ABSL_COMMIT}"
    git clone --quiet https://github.com/abseil/abseil-cpp "${SRC}"
    git -C "${SRC}" checkout --quiet "${ABSL_COMMIT}"
fi

echo "==> building pinned absl (static, -fPIC) — this takes several minutes"
cmake -S "${SRC}" -B "${SRC}/build" \
    -DCMAKE_BUILD_TYPE=Release \
    -DABSL_PROPAGATE_CXX_STD=ON \
    -DCMAKE_POSITION_INDEPENDENT_CODE=ON \
    -DBUILD_SHARED_LIBS=OFF \
    -DABSL_BUILD_TESTING=OFF \
    -DCMAKE_CXX_STANDARD=17 \
    -DCMAKE_INSTALL_PREFIX="${PREFIX}" >/dev/null
cmake --build "${SRC}/build" -j "$(nproc)" >/dev/null
cmake --install "${SRC}/build" >/dev/null

LIBDIR="${PREFIX}/lib"; [ -d "$LIBDIR" ] || LIBDIR="${PREFIX}/lib64"
[ -f "${LIBDIR}/libabsl_strings.a" ] || { echo "ERROR: libabsl_strings.a not built" >&2; exit 1; }
echo "OK: pinned absl built — $(ls "${LIBDIR}"/libabsl_*.a | wc -l) static libs at ${LIBDIR}"
echo "    headers at ${PREFIX}/include (absl/base/nullability.h -> $(grep -c Nonnull "${PREFIX}/include/absl/base/nullability.h" 2>/dev/null) Nonnull refs)"
