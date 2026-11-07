#!/usr/bin/env bash
# Bootstrap real PINNED Abseil (lts_20230125) static archives for TF 2.15.
#
# rush COMPILES TensorFlow against the TF-pinned Abseil source headers
# (inline namespace lts_20230125). The system libabsl (Debian lts_20240722)
# is a DIFFERENT inline namespace, so linking `-labsl_*` leaves every
# absl::lts_20230125::* symbol undefined at the libtensorflow_cc.so link.
# This builds the pinned source into real static PIC archives that match the
# headers — NO STUBS. external_deps.rs wraps every libabsl_*.a in one
# canonical --start-group target (@com_google_absl//absl:all_pinned_static).
#
# The pinned source already lives in the clone cache (fetched as
# @com_google_absl); this just cmake-builds it in place. Build with the SAME
# compiler/std/ABI rush uses for TF; Abseil builds with RTTI (do NOT add
# -fno-rtti).
set -euo pipefail

CACHE="${RUSH_GLOBAL_CLONE_CACHE:-/data/rush-cache/external}"
SRC="${CACHE}/com_google_absl"
BUILD="${SRC}/.rush-cmake-build"
PREFIX="${SRC}/.rush-install"
LIBDIR="${SRC}/.rush-absl/lib"

CC="${CC:-${RUSH_CC:-/usr/bin/gcc}}"
CXX="${CXX:-${RUSH_CXX:-/usr/bin/g++}}"
JOBS="${JOBS:-$(nproc)}"
ABSL_CXXFLAGS="${RUSH_ABSL_CXXFLAGS:-${CXXFLAGS:-}}"

[ -f "${SRC}/CMakeLists.txt" ] || { echo "ERROR: missing ${SRC}/CMakeLists.txt (is @com_google_absl materialized?)" >&2; exit 1; }
grep -q 'ABSL_OPTION_INLINE_NAMESPACE_NAME lts_20230125' \
  "${SRC}/absl/base/options.h" || { echo "ERROR: ${SRC} is not the pinned lts_20230125 Abseil" >&2; exit 1; }

rm -rf "$BUILD" "$PREFIX" "$LIBDIR"
mkdir -p "$LIBDIR"

cmake -S "$SRC" -B "$BUILD" \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_C_COMPILER="$CC" \
  -DCMAKE_CXX_COMPILER="$CXX" \
  -DCMAKE_CXX_STANDARD=17 \
  -DCMAKE_CXX_STANDARD_REQUIRED=ON \
  -DCMAKE_CXX_EXTENSIONS=OFF \
  -DCMAKE_CXX_FLAGS="$ABSL_CXXFLAGS" \
  -DCMAKE_POSITION_INDEPENDENT_CODE=ON \
  -DBUILD_SHARED_LIBS=OFF \
  -DABSL_PROPAGATE_CXX_STD=ON \
  -DABSL_ENABLE_INSTALL=ON \
  -DABSL_BUILD_TESTING=OFF \
  -DBUILD_TESTING=OFF

cmake --build "$BUILD" --parallel "$JOBS"
cmake --install "$BUILD" --prefix "$PREFIX" >/dev/null 2>&1 || true

# Collect every static archive (prefer the install tree; fall back to the
# build tree). De-dup by basename, install wins.
find "$PREFIX" "$BUILD" -type f -name 'libabsl_*.a' -print0 2>/dev/null \
  | sort -z \
  | while IFS= read -r -d '' lib; do cp -f "$lib" "$LIBDIR/$(basename "$lib")"; done

count="$(find "$LIBDIR" -maxdepth 1 -name 'libabsl_*.a' | wc -l)"
[ "$count" -gt 50 ] || { echo "ERROR: too few Abseil archives built: $count" >&2; exit 1; }

# Sanity: confirm the pinned inline namespace. Capture nm output before grepping:
# with pipefail, `nm | grep -q` can report failure when grep exits early and nm
# receives SIGPIPE. The compiler-neutral C++17 template validation follows as
# an actual compile/link probe; demangled template names differ between GCC and
# Clang and are not a valid cross-toolchain ABI check.
if command -v nm >/dev/null 2>&1; then
    STATUS_NM="$BUILD/absl-status.nm"
    nm "$LIBDIR/libabsl_status.a" > "$STATUS_NM"

    grep -q 'lts_20230125' "$STATUS_NM" || {
        echo "ERROR: Abseil archives lack the lts_20230125 namespace" >&2
        exit 1
    }
    echo "absl: confirmed lts_20230125 archive namespace"
fi

# Compile and link through the selected compiler, not just `nm` the archives.
# GCC and Clang can emit different template symbols for the same Abseil LTS
# headers. A cache rebuilt with one compiler must not silently pass validation
# and then fail when TensorFlow objects are compiled with the other.
PROBE_SOURCE="$BUILD/rush-absl-toolchain-probe.cc"
PROBE_BINARY="$BUILD/rush-absl-toolchain-probe"
cat > "$PROBE_SOURCE" <<'EOF'
#include <string>
#include <utility>

#include "absl/strings/cord.h"

int main() {
  std::string value = "rush-absl-toolchain-probe";
  absl::Cord cord(std::move(value));
  return cord.empty();
}
EOF
read -r -a absl_cxxflags <<< "$ABSL_CXXFLAGS"
absl_archives=("$LIBDIR"/libabsl_*.a)
"$CXX" "${absl_cxxflags[@]}" -std=c++17 -I"$SRC" \
  "$PROBE_SOURCE" \
  -Wl,--start-group "${absl_archives[@]}" -Wl,--end-group \
  -pthread -o "$PROBE_BINARY"
"$PROBE_BINARY"

cxx_resolved="$(command -v "$CXX" 2>/dev/null || printf '%s' "$CXX")"
cxx_resolved="$(readlink -f "$cxx_resolved" 2>/dev/null || printf '%s' "$cxx_resolved")"
cxx_version="$("$CXX" --version 2>&1)"
cxx_version="${cxx_version%%$'\n'*}"
cxx_sha256="unavailable"
if [ -f "$cxx_resolved" ] && command -v sha256sum >/dev/null 2>&1; then
    cxx_sha256="$(sha256sum "$cxx_resolved" | awk '{print $1}')"
fi
cat > "$LIBDIR/.rush-toolchain" <<EOF
schema=1
cxx_command=$CXX
cxx_resolved=$cxx_resolved
cxx_sha256=$cxx_sha256
cxx_version=$cxx_version
cxx_target=$("$CXX" -dumpmachine 2>/dev/null || printf unknown)
cxxflags=$ABSL_CXXFLAGS
standard=c++17
EOF
echo "absl: compiler/archive link probe passed with $cxx_version"

rm -rf "$BUILD" "$PREFIX"
echo "absl: built $count pinned lts_20230125 archives in $LIBDIR"
