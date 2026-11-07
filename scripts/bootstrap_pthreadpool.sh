#!/usr/bin/env bash
# Bootstrap real @pthreadpool source + libpthreadpool.a from the TF-pinned
# archive (plus its header-only @FXdiv dependency).
#
# TF 2.15's libtensorflow_cc.so links the TFLite kernels, and
# //tensorflow/lite/kernels:cpu_backend_context.h does
#   #include "pthreadpool.h"        // from @pthreadpool
# plus the kernels call pthreadpool_* at runtime. rush previously stubbed
# @pthreadpool empty, leaving "pthreadpool.h: No such file or directory".
# @pthreadpool is a real compiled C library; this materializes the genuine
# pinned source (the public header resolves as `pthreadpool.h` via the
# include/ dir) and builds a real libpthreadpool.a — NO STUBS.
#
# Pinned commits from tensorflow/tensorflow/workspace2.bzl (Maratyszcza).
# Source list + copts transcribed from pthreadpool's BUILD.bazel for the
# x86_64-linux default config (PTHREADS_IMPL_SRCS + ARCH_SPECIFIC_SRCS).
set -euo pipefail

PTP_SHA="4fe0e1e183925bf8cfa6aae24237e724a96479b8"
PTP_URL="https://github.com/Maratyszcza/pthreadpool/archive/${PTP_SHA}.tar.gz"
FXD_SHA="63058eff77e11aa15bf531df5dd34395ec3017c8"
FXD_URL="https://github.com/Maratyszcza/FXdiv/archive/${FXD_SHA}.tar.gz"

CACHE="${RUSH_GLOBAL_CLONE_CACHE:-/data/rush-cache/external}"
ROOT="${CACHE}/pthreadpool"
FXD_ROOT="${CACHE}/FXdiv"
mkdir -p "$CACHE"

fetch() {  # url sha dest witness
    local url="$1" sha="$2" dest="$3" witness="$4" pkg
    pkg="$(basename "$dest")"
    if [ ! -f "${dest}/${witness}" ]; then
        echo "${pkg}: downloading pinned ${sha} ..."
        local tmp; tmp="$(mktemp -d)"
        curl -fsSL "$url" -o "${tmp}/${pkg}.tgz"
        tar -xzf "${tmp}/${pkg}.tgz" -C "${tmp}"
        rm -rf "$dest"; mkdir -p "$dest"
        cp -r "${tmp}/${pkg}-${sha}/." "$dest/"
        rm -rf "$tmp"
    fi
    [ -f "${dest}/${witness}" ] || { echo "ERROR: ${pkg}/${witness} missing" >&2; exit 1; }
}

fetch "$FXD_URL" "$FXD_SHA" "$FXD_ROOT" "include/fxdiv.h"
fetch "$PTP_URL" "$PTP_SHA" "$ROOT" "include/pthreadpool.h"
echo "pthreadpool source: ${ROOT}"
echo "FXdiv source:       ${FXD_ROOT}"

CC="${CC:-gcc}"
# x86_64-linux config from pthreadpool's BUILD.bazel copts:
#   -std=gnu11 -O2 -DPTHREADPOOL_USE_CPUINFO=0 -DPTHREADPOOL_USE_FASTPATH=1
# plus the propagated define PTHREADPOOL_NO_DEPRECATED_API. Sync-primitive
# defines are left to pthreadpool's platform auto-detection (the BUILD's
# default branch adds none).
CFLAGS=(-c -fPIC -O2 -std=gnu11 -D_GNU_SOURCE
        -DPTHREADPOOL_USE_CPUINFO=0 -DPTHREADPOOL_USE_FASTPATH=1
        -DPTHREADPOOL_NO_DEPRECATED_API
        "-I${ROOT}/include" "-I${ROOT}/src" "-I${FXD_ROOT}/include")

OBJDIR="${ROOT}/.rush-obj"; rm -rf "$OBJDIR"; mkdir -p "$OBJDIR"

# default (non-arm) sync impl = PTHREADS_IMPL_SRCS; linux_x86_64 adds fastpath.
SRCS=(
    src/memory.c
    src/portable-api.c
    src/pthreads.c
    src/fastpath.c
)

objs=()
for s in "${SRCS[@]}"; do
    [ -f "${ROOT}/${s}" ] || { echo "ERROR: missing pthreadpool source ${s}" >&2; exit 1; }
    o="${OBJDIR}/$(echo "$s" | tr '/' '_').o"
    "$CC" "${CFLAGS[@]}" -o "$o" "${ROOT}/${s}"
    objs+=("$o")
done

ar rcs "${ROOT}/libpthreadpool.a" "${objs[@]}"
rm -rf "$OBJDIR"
echo "pthreadpool: built ${ROOT}/libpthreadpool.a ($(ar t "${ROOT}/libpthreadpool.a" | wc -l) objects)"
