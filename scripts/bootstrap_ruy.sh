#!/usr/bin/env bash
# Bootstrap real @ruy source + libruy.a from the TF-pinned ruy archive.
#
# TensorFlow 2.15's //tensorflow:tensorflow_cc links the TFLite runtime
# kernels (//tensorflow/lite/kernels:builtin_ops -> :cpu_backend_context /
# :cpu_backend_gemm), which #include "ruy/context.h" and link @ruy//ruy:*.
# rush previously *stubbed* @ruy (repository_manager.rs grouped it with the
# XNNPACK/mobile stack as "not needed"), which left ruy/context.h missing.
# It IS needed. This materializes the genuine pinned ruy source tree (so the
# ruy/*.h headers resolve at compile) and compiles a real libruy.a (so ruy's
# symbols resolve at the final .so link) — NO STUBS.
#
# Pinned commit + URL transcribed from tensorflow/third_party/ruy/workspace.bzl.
# The per-file SIMD copts (AVX / AVX2+FMA / AVX512) are transcribed from ruy's
# own ruy/build_defs.bzl (ruy_copts_avx / ruy_copts_avx2_fma / ruy_copts_avx512)
# — these MUST match per file or the AVX kernels miscompile/UB. cpuinfo.cc is
# compiled in ruy's no-cpuinfo configuration (RUY_HAVE_CPUINFO undefined) so it
# does not need @cpuinfo (also unmaterialized); ruy then uses its built-in
# conservative feature detection.
set -euo pipefail

RUY_SHA="3286a34cc8de6149ac6844107dfdffac91531e72"
RUY_URL="https://github.com/google/ruy/archive/${RUY_SHA}.tar.gz"

CACHE="${RUSH_GLOBAL_CLONE_CACHE:-/data/rush-cache/external}"
RUY_ROOT="${CACHE}/ruy"          # so `#include "ruy/context.h"` resolves with -I${RUY_ROOT}
RUY_PKG="${RUY_ROOT}/ruy"        # the ruy/ subdir containing the sources/headers

mkdir -p "$CACHE"

# --- materialize source (idempotent) ---
if [ ! -f "${RUY_PKG}/context.h" ]; then
    echo "ruy: downloading pinned ${RUY_SHA} ..."
    tmp="$(mktemp -d)"
    curl -fsSL "$RUY_URL" -o "${tmp}/ruy.tgz"
    tar -xzf "${tmp}/ruy.tgz" -C "${tmp}"
    rm -rf "$RUY_ROOT"
    mkdir -p "$RUY_ROOT"
    cp -r "${tmp}/ruy-${RUY_SHA}/." "$RUY_ROOT/"
    rm -rf "$tmp"
fi
[ -f "${RUY_PKG}/context.h" ] || { echo "ERROR: ruy/context.h missing after materialize" >&2; exit 1; }
echo "ruy source: ${RUY_ROOT}"

# --- compile libruy.a ---
CXX="${CXX:-g++}"
STD="-std=c++17"
COMMON=(-c -fPIC -O3 "$STD" "-I${RUY_ROOT}")

OBJDIR="${RUY_ROOT}/.rush-obj"
rm -rf "$OBJDIR"; mkdir -p "$OBJDIR"

# Generic (non-SIMD) ruy library sources. Transcribed from ruy/BUILD's
# library cc_library targets (excludes *_test, benchmarks, examples, tools).
GENERIC=(
    allocator.cc apply_multiplier.cc blocking_counter.cc block_map.cc
    context.cc context_get_ctx.cc cpuinfo.cc ctx.cc denormal.cc frontend.cc
    kernel_arm32.cc kernel_arm64.cc pack_arm.cc pmu.cc prepacked_cache.cc
    prepare_packed_matrices.cc system_aligned_alloc.cc thread_pool.cc
    trmul.cc tune.cc wait.cc
)
AVX=(kernel_avx.cc pack_avx.cc have_built_path_for_avx.cc)
AVX2_FMA=(kernel_avx2_fma.cc pack_avx2_fma.cc have_built_path_for_avx2_fma.cc)
AVX512=(kernel_avx512.cc pack_avx512.cc have_built_path_for_avx512.cc)

compile_group() {
    local outtag="$1"; shift
    local extra=("$@")
    # remaining args after the flag list are the files — split on the marker
    :
}

objs=()
do_compile() {  # do_compile <flag...> -- <file...>
    local flags=() files=() seen=0
    for a in "$@"; do
        if [ "$a" = "--" ]; then seen=1; continue; fi
        if [ $seen -eq 0 ]; then flags+=("$a"); else files+=("$a"); fi
    done
    for f in "${files[@]}"; do
        [ -f "${RUY_PKG}/${f}" ] || { echo "ERROR: missing ruy source ${f}" >&2; exit 1; }
        local o="${OBJDIR}/${f%.cc}.o"
        "$CXX" "${COMMON[@]}" "${flags[@]}" -o "$o" "${RUY_PKG}/${f}"
        objs+=("$o")
    done
}

echo "ruy: compiling generic sources ..."
do_compile -- "${GENERIC[@]}"
echo "ruy: compiling AVX sources ..."
do_compile -mavx -- "${AVX[@]}"
echo "ruy: compiling AVX2+FMA sources ..."
do_compile -mavx2 -mfma -- "${AVX2_FMA[@]}"
echo "ruy: compiling AVX512 sources ..."
do_compile -mavx512f -mavx512vl -mavx512cd -mavx512bw -mavx512dq -- "${AVX512[@]}"

# profiler instrumentation (separate package @ruy//ruy/profiler:instrumentation)
if [ -f "${RUY_PKG}/profiler/instrumentation.cc" ]; then
    echo "ruy: compiling profiler/instrumentation.cc ..."
    "$CXX" "${COMMON[@]}" -o "${OBJDIR}/profiler_instrumentation.o" \
        "${RUY_PKG}/profiler/instrumentation.cc"
    objs+=("${OBJDIR}/profiler_instrumentation.o")
fi

ar rcs "${RUY_ROOT}/libruy.a" "${objs[@]}"
rm -rf "$OBJDIR"
echo "ruy: built ${RUY_ROOT}/libruy.a ($(ar t "${RUY_ROOT}/libruy.a" | wc -l) objects)"
