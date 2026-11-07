#!/usr/bin/env bash
# Bootstrap REAL libducc.a + libfft2d.a from the TF-pinned ducc / fft2d source.
#
# #39 Phase R: libtensorflow_cc.so links //tensorflow/core/kernels:fft_impl
# (ducc FFT) and the fft2d real-FFT, but rush materialized only HEADERS for
# @ducc and @fft2d, leaving ducc0::detail_threading::* (4) and rdft (1)
# undefined — load-blocking (they sit behind eager relocs / referenced by the
# kernel TUs). This compiles the genuine pinned sources into real archives that
# the @ducc / @fft2d adapters link — NO STUBS.
set -euo pipefail

CACHE="${RUSH_GLOBAL_CLONE_CACHE:-/data/rush-cache/external}"
CXX="${CXX:-g++}"
CC_="${CC:-gcc}"

# --- ducc: src/ducc0/infra/threading.cc -> libducc.a ---
DUCC_SRC=""
for c in "${CACHE}/ducc/src" \
         /data/tensorflow/.cache/rush/bazel_output_base/external/ducc/src \
         "$(ls -d "$HOME"/.cache/bazel/_bazel_*/*/external/ducc/src 2>/dev/null | head -1)"; do
    [ -n "$c" ] && [ -f "$c/ducc0/infra/threading.cc" ] && { DUCC_SRC="$c"; break; }
done
[ -n "$DUCC_SRC" ] || { echo "ERROR: ducc src (ducc0/infra/threading.cc) not found" >&2; exit 1; }
echo "ducc src: ${DUCC_SRC}"
DUCC_OBJ="$(mktemp -d)"
"$CXX" -std=c++17 -fPIC -O2 -ffast-math -I"$DUCC_SRC" \
    -c "${DUCC_SRC}/ducc0/infra/threading.cc" -o "${DUCC_OBJ}/threading.o"
DUCC_OBJS=("${DUCC_OBJ}/threading.o")
# TF (2.16+) ships google/fft.cc in the @ducc clone — it explicitly instantiates
# ducc0::google::c2c<double> etc. (the FFT entry points TF's fft_ops call, and a .so dlopen
# load-blocker). Compile it too when present; it includes "ducc/..." via the ducc_include_root
# symlink + Eigen::ThreadPoolInterface. (TF-2.15's @ducc has no google/fft.cc → skipped.) (Codex 019eebc6.)
DUCC_GOOGLE_FFT="${CACHE}/ducc/google/fft.cc"
if [ -f "$DUCC_GOOGLE_FFT" ]; then
    "$CXX" -std=c++17 -fPIC -O2 -ffast-math \
        -I"${CACHE}/ducc_include_root" -I"${CACHE}/eigen_archive" -I"$DUCC_SRC" \
        -c "$DUCC_GOOGLE_FFT" -o "${DUCC_OBJ}/google_fft.o"
    DUCC_OBJS+=("${DUCC_OBJ}/google_fft.o")
fi
ar rcs "${CACHE}/ducc/libducc.a" "${DUCC_OBJS[@]}"
rm -rf "$DUCC_OBJ"
# grep a dumped symbol table, not `nm | grep -q` (under pipefail, grep -q exits
# on match → nm gets SIGPIPE → pipeline returns 141 → false negative).
nm "${CACHE}/ducc/libducc.a" > /tmp/ducc_syms.txt
grep -q " T _ZN5ducc016detail_threading10execStaticEmmmSt8functionIFvRNS0_9SchedulerEEE" /tmp/ducc_syms.txt \
    || { echo "ERROR: libducc.a missing ducc0::detail_threading::execStatic — NO STUBS" >&2; exit 1; }
if [ -f "$DUCC_GOOGLE_FFT" ]; then
    grep -q "google3c2cId" /tmp/ducc_syms.txt \
        || { echo "ERROR: libducc.a missing ducc0::google::c2c<double> — NO STUBS" >&2; exit 1; }
fi
echo "ducc: libducc.a = $(stat -c%s "${CACHE}/ducc/libducc.a") bytes ✓"

# --- fft2d: fftsg.c (rdft) + siblings -> libfft2d.a ---
FFT_SRC=""
for c in "${CACHE}/fft2d" \
         /data/tensorflow/.cache/rush/bazel_output_base/external/fft2d \
         "$(ls -d "$HOME"/.cache/bazel/_bazel_*/*/external/fft2d 2>/dev/null | head -1)"; do
    [ -n "$c" ] && [ -f "$c/fftsg.c" ] && { FFT_SRC="$c"; break; }
done
[ -n "$FFT_SRC" ] || { echo "ERROR: fft2d src (fftsg.c) not found" >&2; exit 1; }
echo "fft2d src: ${FFT_SRC}"
FFT_OBJ="$(mktemp -d)"
# ONLY the fftsg split-radix path (TF's FFT2D_SRCS = fftsg.c + fftsg2d.c) + alloc (fftsg2d.c's
# alloc_1d/free_1d). NOT fft4g / fft8g / fft4f2d: those radix-4/8 variants REDEFINE rdft / cdft /
# ddst / dfst (and rdft2d/cdft2d) that fftsg already provides — harmless under a plain link (the
# linker picks one) but a multiple-definition error once libfft2d.a is whole-archived into
# libtensorflow_cc.so (rush external_deps.rs fft2d_linkopts). (Codex 019eebc6.)
for f in fftsg fftsg2d alloc; do
    [ -f "${FFT_SRC}/${f}.c" ] && "$CC_" -fPIC -O2 -c "${FFT_SRC}/${f}.c" -o "${FFT_OBJ}/${f}.o"
done
ar rcs "${CACHE}/fft2d/libfft2d.a" "${FFT_OBJ}"/*.o
rm -rf "$FFT_OBJ"
nm "${CACHE}/fft2d/libfft2d.a" > /tmp/fft2d_syms.txt
grep -qw "T rdft" /tmp/fft2d_syms.txt \
    || { echo "ERROR: libfft2d.a missing rdft — NO STUBS" >&2; exit 1; }
echo "fft2d: libfft2d.a = $(stat -c%s "${CACHE}/fft2d/libfft2d.a") bytes ✓"

# --- flatbuffers: src/util.cpp -> libflatbuffers_util.a (ClassicLocale::instance_) ---
# @flatbuffers is register_system_stub'd (header-only); only the runtime's
# util.cpp static data (flatbuffers::ClassicLocale::instance_) is undefined and
# blocks dlopen. Compile JUST util.cpp from the pinned source against the pinned
# flatbuffers headers — the @flatbuffers stub's runtime_cc link flags pick it up
# (build_orchestrator_ext.rs shared_link_flags_for_stub). NO STUBS.
FB_SRC=""
for c in /data/tensorflow/.cache/rush/bazel_output_base/external/flatbuffers \
         "${CACHE}/flatbuffers" \
         "$(ls -d "$HOME"/.cache/bazel/_bazel_*/*/external/flatbuffers 2>/dev/null | head -1)"; do
    [ -n "$c" ] && [ -f "$c/src/util.cpp" ] && [ -f "$c/include/flatbuffers/util.h" ] && { FB_SRC="$c"; break; }
done
[ -n "$FB_SRC" ] || { echo "ERROR: flatbuffers src/util.cpp + include not found" >&2; exit 1; }
echo "flatbuffers src: ${FB_SRC}"
mkdir -p "${CACHE}/flatbuffers"
FB_OBJ="$(mktemp -d)"
"$CXX" -std=c++17 -fPIC -O2 -I"${FB_SRC}/include" \
    -c "${FB_SRC}/src/util.cpp" -o "${FB_OBJ}/util.o"
ar rcs "${CACHE}/flatbuffers/libflatbuffers_util.a" "${FB_OBJ}/util.o"
rm -rf "$FB_OBJ"
nm "${CACHE}/flatbuffers/libflatbuffers_util.a" > /tmp/fb_syms.txt
grep -q " [TVu] _ZN11flatbuffers13ClassicLocale9instance_E" /tmp/fb_syms.txt \
    || grep -q "ClassicLocale9instance_E" /tmp/fb_syms.txt \
    || { echo "ERROR: libflatbuffers_util.a missing ClassicLocale::instance_ — NO STUBS" >&2; exit 1; }
echo "flatbuffers: libflatbuffers_util.a = $(stat -c%s "${CACHE}/flatbuffers/libflatbuffers_util.a") bytes ✓"
