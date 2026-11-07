#!/usr/bin/env bash
# Bootstrap libfarmhash.a from real farmhash source.
#
# TensorFlow's @farmhash_archive//:farmhash compiles src/farmhash.cc with
# -DNAMESPACE_FOR_HASH_FUNCTIONS=util, placing the API in `namespace util`
# (`util::Fingerprint64`) — what TF/tsl link against. rush had only an empty
# @farmhash_archive stub, so that symbol was undefined. This materializes the
# real single-translation-unit archive (mirrors bootstrap_nsync_cpp.sh); rush
# then seeds the @farmhash_archive//:farmhash stub's link flags with the .a.
# NO STUBS: genuine farmhash source compiled as TF builds it.
set -euo pipefail

find_src() {
    for c in \
        "${RUSH_FARMHASH_SRC:-}" \
        "$PWD/tensorflow/external/farmhash_archive" \
        "$PWD/external/farmhash_archive" \
        "$HOME/devel/rush/tensorflow/external/farmhash_archive" \
        "$(ls -d "$HOME"/.cache/bazel/_bazel_*/*/external/farmhash_archive 2>/dev/null | head -1)"; do
        [ -n "$c" ] && [ -f "$c/src/farmhash.cc" ] && { echo "$c"; return 0; }
    done
    return 1
}
SRC="$(find_src)" || { echo "ERROR: farmhash source (src/farmhash.cc) not found; set RUSH_FARMHASH_SRC" >&2; exit 1; }
echo "farmhash source: $SRC"

OUT="${RUSH_FARMHASH_OUT:-${RUSH_GLOBAL_CLONE_CACHE:-/data/rush-cache/external}/farmhash_archive}"
mkdir -p "$OUT"
echo "output dir:      $OUT"

CXX="${CXX:-g++}"
"$CXX" -x c++ -std=c++17 -fPIC -O2 -DNDEBUG -DNAMESPACE_FOR_HASH_FUNCTIONS=util \
    -I"$SRC/src" -c "$SRC/src/farmhash.cc" -o "$OUT/farmhash.o"

AR="${AR:-ar}"
LIB="$OUT/libfarmhash.a"
rm -f "$LIB"
"$AR" rcs "$LIB" "$OUT/farmhash.o"
echo "built: $LIB ($(du -h "$LIB" | cut -f1))"

if ! nm -C "$LIB" | grep -qE "\bT util::Fingerprint64\("; then
    echo "ERROR: libfarmhash.a does not define util::Fingerprint64 — wrong namespace define?" >&2
    exit 1
fi
echo "OK: util::Fingerprint64 defined"
