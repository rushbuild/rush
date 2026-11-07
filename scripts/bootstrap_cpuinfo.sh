#!/usr/bin/env bash
# Bootstrap real @cpuinfo source + libcpuinfo.a from the TF-pinned archive.
#
# TF 2.15's libtensorflow_cc.so links the TFLite kernels, and
# //tensorflow/lite/kernels:cpu_backend_context.cc does
#   #ifdef TFLITE_HAVE_CPUINFO
#   #include "include/cpuinfo.h"   // @cpuinfo//:cpuinfo_with_unstripped_include_path
# plus calls cpuinfo_* at runtime. rush previously stubbed @cpuinfo (mobile
# stack), leaving "include/cpuinfo.h: No such file or directory". @cpuinfo is a
# real compiled C library; this materializes the genuine pinned source (headers
# resolve `include/cpuinfo.h`) and builds a real libcpuinfo.a (x86_64-linux
# source set + the bundled clog) — NO STUBS.
#
# Pinned commit from tensorflow/tensorflow/workspace2.bzl (pytorch/cpuinfo).
# Source list transcribed from cpuinfo's BUILD.bazel COMMON_SRCS + x86 + linux.
set -euo pipefail

CPU_SHA="959002f82d7962a473d8bf301845f2af720e0aa4"
CPU_URL="https://github.com/pytorch/cpuinfo/archive/${CPU_SHA}.tar.gz"

CACHE="${RUSH_GLOBAL_CLONE_CACHE:-/data/rush-cache/external}"
ROOT="${CACHE}/cpuinfo"
mkdir -p "$CACHE"

if [ ! -f "${ROOT}/include/cpuinfo.h" ]; then
    echo "cpuinfo: downloading pinned ${CPU_SHA} ..."
    tmp="$(mktemp -d)"
    curl -fsSL "$CPU_URL" -o "${tmp}/cpuinfo.tgz"
    tar -xzf "${tmp}/cpuinfo.tgz" -C "${tmp}"
    rm -rf "$ROOT"; mkdir -p "$ROOT"
    cp -r "${tmp}/cpuinfo-${CPU_SHA}/." "$ROOT/"
    rm -rf "$tmp"
fi
[ -f "${ROOT}/include/cpuinfo.h" ] || { echo "ERROR: include/cpuinfo.h missing" >&2; exit 1; }
echo "cpuinfo source: ${ROOT}"

CC="${CC:-gcc}"
# cpuinfo build flags (from its BUILD.bazel): include + src + clog include on
# the search path, _GNU_SOURCE, and the stdio clog sink.
CFLAGS=(-c -fPIC -O2 -std=gnu99 -D_GNU_SOURCE -DCLOG_LOG_TO_STDIO=1
        -DCPUINFO_LOG_LEVEL=2
        "-I${ROOT}/include" "-I${ROOT}/src" "-I${ROOT}/deps/clog/include")

OBJDIR="${ROOT}/.rush-obj"; rm -rf "$OBJDIR"; mkdir -p "$OBJDIR"

# x86_64-linux library source set (no tests/mock/benchmarks).
SRCS=(
    src/api.c src/cache.c src/init.c src/log.c
    src/x86/init.c src/x86/info.c src/x86/isa.c src/x86/name.c
    src/x86/topology.c src/x86/uarch.c src/x86/vendor.c
    src/x86/cache/init.c src/x86/cache/descriptor.c src/x86/cache/deterministic.c
    src/x86/linux/init.c src/x86/linux/cpuinfo.c
    src/linux/cpulist.c src/linux/multiline.c src/linux/processors.c
    src/linux/smallfile.c
    deps/clog/src/clog.c
)

objs=()
for s in "${SRCS[@]}"; do
    [ -f "${ROOT}/${s}" ] || { echo "ERROR: missing cpuinfo source ${s}" >&2; exit 1; }
    o="${OBJDIR}/$(echo "$s" | tr '/' '_').o"
    "$CC" "${CFLAGS[@]}" -o "$o" "${ROOT}/${s}"
    objs+=("$o")
done

ar rcs "${ROOT}/libcpuinfo.a" "${objs[@]}"
rm -rf "$OBJDIR"
echo "cpuinfo: built ${ROOT}/libcpuinfo.a ($(ar t "${ROOT}/libcpuinfo.a" | wc -l) objects)"
