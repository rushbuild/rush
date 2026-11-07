#!/usr/bin/env bash
# Bootstrap the REAL @org_sqlite vendored amalgamation + libsqlite_amalgamation.a.
#
# TensorFlow 2.15's libtensorflow_cc.so links //tensorflow/core/lib/db (the
# SQLite-backed event/summary DB), which uses the sqlite3 C API. rush left
# @org_sqlite as a stub, so ~45 `sqlite3_*` C-API symbols (sqlite3_free,
# sqlite3_value_blob, sqlite3_bind_text64, …) were undefined in the .so. This
# compiles the genuine TF-pinned amalgamation (sqlite3.c) into a real
# libsqlite_amalgamation.a — NO STUBS, NOT a `-lsqlite3` system-lib substitution.
#
# URL / sha256 / copts transcribed from tensorflow/third_party/sqlite.BUILD +
# tensorflow/workspace2.bzl (sqlite-amalgamation-3430000, SQLITE_COPTS +
# SQLITE_OMIT_DEPRECATED).
set -euo pipefail

SQLITE_URL="https://www.sqlite.org/2023/sqlite-amalgamation-3430000.zip"
SQLITE_SHA="bb5849ae4d7129c09d20596379a0b3f7b1ac59cf9998eba5ef283ea9b6c000a5"

CACHE="${RUSH_GLOBAL_CLONE_CACHE:-/data/rush-cache/external}"
SQ_ROOT="${CACHE}/org_sqlite"   # @org_sqlite repo root (sqlite3.h lives here for -I)

mkdir -p "$SQ_ROOT"

# --- materialize real amalgamation source (idempotent, sha-verified) ---
if [ ! -f "${SQ_ROOT}/sqlite3.c" ]; then
    echo "sqlite: downloading pinned amalgamation 3430000 ..."
    tmp="$(mktemp -d)"
    curl -fsSL "$SQLITE_URL" -o "${tmp}/sq.zip"
    echo "${SQLITE_SHA}  ${tmp}/sq.zip" | sha256sum -c - \
        || { echo "ERROR: sqlite amalgamation sha256 mismatch" >&2; exit 1; }
    unzip -q "${tmp}/sq.zip" -d "${tmp}"
    src="${tmp}/sqlite-amalgamation-3430000"
    cp "${src}/sqlite3.c" "${src}/sqlite3.h" "${src}/sqlite3ext.h" "$SQ_ROOT/"
    rm -rf "$tmp"
fi
[ -f "${SQ_ROOT}/sqlite3.c" ] || { echo "ERROR: sqlite3.c missing after materialize" >&2; exit 1; }
echo "sqlite source: ${SQ_ROOT}"

# --- compile libsqlite_amalgamation.a (TF's SQLITE_COPTS + SQLITE_OMIT_DEPRECATED) ---
CC="${CC:-gcc}"
echo "sqlite: compiling sqlite3.c (real amalgamation) ..."
"$CC" -c -fPIC -O2 \
    -DSQLITE_ENABLE_JSON1 -DHAVE_DECL_STRERROR_R=1 -DHAVE_STDINT_H=1 \
    -DHAVE_INTTYPES_H=1 -D_FILE_OFFSET_BITS=64 -D_REENTRANT=1 \
    -DSQLITE_OMIT_DEPRECATED \
    -o "${SQ_ROOT}/sqlite3.o" "${SQ_ROOT}/sqlite3.c"
ar rcs "${SQ_ROOT}/libsqlite_amalgamation.a" "${SQ_ROOT}/sqlite3.o"
rm -f "${SQ_ROOT}/sqlite3.o"
[ -f "${SQ_ROOT}/libsqlite_amalgamation.a" ] || { echo "ERROR: libsqlite_amalgamation.a not produced" >&2; exit 1; }
echo "sqlite: built ${SQ_ROOT}/libsqlite_amalgamation.a ($(ar t "${SQ_ROOT}/libsqlite_amalgamation.a" | wc -l) objects, $(nm "${SQ_ROOT}/libsqlite_amalgamation.a" 2>/dev/null | grep -c ' T sqlite3_') sqlite3_* defs)"
