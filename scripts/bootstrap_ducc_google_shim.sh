#!/usr/bin/env bash
# Bootstrap the @ducc `google/` threading shim.
#
# TensorFlow vendors DUCC (the FFT library) and overlays a small Google-style
# threading shim: third_party/ducc/workspace.bzl copies third_party/ducc/
# {threading.h,threading.cc,ducc0_custom_lowlevel_threading.h} into the @ducc
# repo as google/threading.h, google/threading.cc,
# google/ducc0_custom_lowlevel_threading.h — which @ducc//:fft lists in its
# srcs/hdrs (third_party/ducc/ducc.BUILD). rush materialized the DUCC source
# but never ran that copy, so @ducc//:fft -> @ducc//:google/threading.h was
# "not found on disk". This reproduces the copy. NO STUBS: these are the real
# TF-authored shim sources, identical to what bazel's repo rule installs.
set -euo pipefail

# TF's third_party/ducc lives under the main workspace.
TF_WS="${RUSH_TF_WORKSPACE:-$HOME/devel/rush/tensorflow}"
SHIM_SRC="${TF_WS}/third_party/ducc"
[ -f "${SHIM_SRC}/threading.h" ] || { echo "ERROR: ${SHIM_SRC}/threading.h not found; set RUSH_TF_WORKSPACE" >&2; exit 1; }

find_ducc_clone() {
    for c in \
        "${RUSH_DUCC_CLONE:-}" \
        "${TF_WS}/external/ducc" \
        "${TF_WS}/.cache/rush/external/ducc" \
        "${RUSH_GLOBAL_CLONE_CACHE:-/data/rush-cache/external}/ducc"; do
        [ -n "$c" ] && [ -f "$c/src/ducc0/infra/threading.h" ] && { echo "$c"; return 0; }
    done
    return 1
}
CLONE="$(find_ducc_clone)" || { echo "ERROR: DUCC clone (src/ducc0/infra/threading.h) not found" >&2; exit 1; }
echo "ducc clone:  ${CLONE}"
echo "shim source: ${SHIM_SRC}"

mkdir -p "${CLONE}/google"
for f in threading.h threading.cc ducc0_custom_lowlevel_threading.h; do
    cp -f "${SHIM_SRC}/${f}" "${CLONE}/google/${f}"
done

for f in threading.h threading.cc ducc0_custom_lowlevel_threading.h; do
    [ -f "${CLONE}/google/${f}" ] || { echo "ERROR: ${CLONE}/google/${f} missing after copy" >&2; exit 1; }
done
echo "OK: @ducc google/ shim installed ($(ls "${CLONE}/google" | wc -l) files) at ${CLONE}/google"
