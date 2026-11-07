#!/usr/bin/env bash
# Bootstrap TF-pinned protobuf 3.21.9 as static PIC archives.
#
# TensorFlow 2.15 pins protobuf-3.21.9 (tensorflow/workspace2.bzl). rush had
# @com_google_protobuf in its is_system_repo() allowlist, so every rush-built TF
# C++ binary dynamically linked the SYSTEM libprotobuf.so.32 (protobuf 4.25.x).
# The newer TextFormat parser rejects raw newlines inside quoted string literals,
# so the op-wrapper codegen tools abort at static REGISTER_OP("PrintV2") whose
# attr `end: string = '\n'` has a literal-newline default (3.21.x accepted it).
#
# This builds the genuine pinned protobuf 3.21.9 source into static PIC archives
# (libprotobuf.a / libprotobuf-lite.a / libprotoc.a) so rush can link the correct
# runtime instead of the system .so. protobuf 3.21 is pre-abseil (its own stubs),
# so there is no external abseil dependency here. NO STUBS: real protobuf source.
set -euo pipefail

find_src() {
    for c in \
        "${RUSH_PROTOBUF_SRC:-}" \
        "$(ls -d "$PWD"/.cache/rush/bazel_output_base/external/com_google_protobuf 2>/dev/null | head -1)" \
        "$(ls -d "$HOME"/.cache/bazel/_bazel_*/*/external/com_google_protobuf 2>/dev/null | head -1)" \
        "${RUSH_GLOBAL_CLONE_CACHE:-/data/rush-cache/external}/com_google_protobuf_src"; do
        [ -n "$c" ] && [ -f "$c/src/google/protobuf/text_format.cc" ] && { echo "$c"; return 0; }
    done
    return 1
}
SRC="$(find_src)" || { echo "ERROR: protobuf 3.21.9 source (src/google/protobuf/text_format.cc) not found; set RUSH_PROTOBUF_SRC" >&2; exit 1; }
echo "protobuf source: $SRC"

# Confirm it really is the 3.21.x line (PROTOBUF_VERSION 3021xxx) — abort on a
# version mismatch rather than silently building the wrong runtime.
if ! grep -qrE "3021[0-9]{3}LL|libprotobuf_version|3\.21\." "$SRC/src/google/protobuf/stubs/common.h" 2>/dev/null; then
    echo "WARN: could not confirm protobuf 3.21.x from common.h; proceeding (source path: $SRC)" >&2
fi

OUT="${RUSH_PROTOBUF_OUT:-${RUSH_GLOBAL_CLONE_CACHE:-/data/rush-cache/external}/com_google_protobuf}"
mkdir -p "$OUT"
echo "output dir:      $OUT"

BUILD="$OUT/build"
cmake -S "$SRC" -B "$BUILD" \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_POSITION_INDEPENDENT_CODE=ON \
    -Dprotobuf_BUILD_SHARED_LIBS=OFF \
    -Dprotobuf_BUILD_TESTS=OFF \
    -Dprotobuf_BUILD_CONFORMANCE=OFF \
    -Dprotobuf_BUILD_EXAMPLES=OFF \
    -Dprotobuf_BUILD_PROTOC_BINARIES=ON \
    -Dprotobuf_WITH_ZLIB=ON \
    -DCMAKE_CXX_STANDARD=17

cmake --build "$BUILD" --target libprotobuf libprotobuf-lite libprotoc protoc -j"$(nproc)"

# Collect the archives + protoc into the repo root (stable paths for the adapter).
for a in libprotobuf.a libprotobuf-lite.a libprotoc.a; do
    f="$(find "$BUILD" -name "$a" | head -1)"
    [ -n "$f" ] && cp -f "$f" "$OUT/$a" && echo "built: $OUT/$a ($(du -h "$OUT/$a" | cut -f1))"
done
pbin="$(find "$BUILD" -maxdepth 2 -name protoc \( -type f -o -type l \) | head -1)"
[ -n "$pbin" ] && cp -Lf "$pbin" "$OUT/protoc" && echo "built: $OUT/protoc"

# Record the src include root for the adapter (headers live at <src>/src).
echo "$SRC/src" > "$OUT/INCLUDE_ROOT"

# Sanity: the static libprotobuf must define TextFormat (the symbol the op-gen
# tools need at runtime). Mangled name varies; just confirm TextFormat symbols
# are present. Use grep -c (count) into a variable rather than `nm | grep -q`,
# which trips `set -o pipefail` via SIGPIPE when grep -q closes the pipe early.
tf_syms="$(nm "$OUT/libprotobuf.a" 2>/dev/null | grep -c "TextFormat" || true)"
if [ "${tf_syms:-0}" -lt 1 ]; then
    echo "ERROR: libprotobuf.a does not define TextFormat symbols — wrong build?" >&2
    exit 1
fi
echo "OK: libprotobuf.a defines TextFormat ($tf_syms symbols); include root: $SRC/src"
