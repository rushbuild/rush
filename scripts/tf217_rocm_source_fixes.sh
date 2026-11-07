#!/usr/bin/env bash
# TF 2.17.0 ROCm source-consistency migration for the rush MIOpen DNN port.
#
# TF 2.17's ROCm code (some kernels + core/util) is internally out of sync with its OWN 2.17 XLA
# StreamExecutor refactor — the caller and the interface disagree WITHIN THE SAME RELEASE. These are
# legitimate source-consistency fixes (what upstream would do), NOT stubs (see Codex 019eebc6). Each
# edit is idempotent (grep-guarded) so re-running is safe. Applies to the ISOLATED /data/tensorflow-2.17
# tree only (never the certified 2.15 corpus at /data/tensorflow).
#
# Usage: TF=/data/tensorflow-2.17 bash scripts/tf217_rocm_source_fixes.sh
set -u
TF="${TF:-/data/tensorflow-2.17}"
n=0

# (1) core/util/rocm_solvers.cc — uses StreamExecutor::implementation(), removed in the 2.17 SE
# refactor (StreamExecutor IS the GpuExecutor now; cf. ExtractGpuExecutor in
# xla/stream_executor/gpu/gpu_executor.h). Drop the ->implementation() indirection so the existing
# static_cast<GpuExecutor*>(...->parent()) binds.
F="$TF/tensorflow/core/util/rocm_solvers.cc"
if [ -f "$F" ] && grep -q -- '->parent()->implementation()' "$F"; then
    sed -i 's/->parent()->implementation()/->parent()/g' "$F"
    echo "fixed: rocm_solvers.cc StreamExecutor::implementation() -> parent()"; n=$((n+1))
fi

# (2) kernels/lrn_op.cc — the ROCm LRN path calls DoNormalizeBackwardWithDimensions with a stray
# workspace_allocator (nullptr) ahead of scratch_allocator (9 args), but the base + ROCm DnnSupport
# declare 8 (one allocator). Drop the stray /*workspace_allocator=*/nullptr arg so it matches.
F="$TF/tensorflow/core/kernels/lrn_op.cc"
if [ -f "$F" ] && grep -q '/\*workspace_allocator=\*/nullptr, &scratch_allocator' "$F"; then
    sed -i 's@/\*workspace_allocator=\*/nullptr, &scratch_allocator@\&scratch_allocator@g' "$F"
    echo "fixed: lrn_op.cc DoNormalizeBackwardWithDimensions 9-arg -> 8-arg"; n=$((n+1))
fi

# (3) farmhash_gpu include_prefix materialization. @farmhash_gpu_archive//:farmhash_gpu sets
# include_prefix="third_party/farmhash_gpu" (so consumers include "third_party/farmhash_gpu/src/
# farmhash_gpu.h"), but rush doesn't yet implement Bazel include_prefix (only strip_include_prefix).
# Materialize the REAL fetched header at the prefixed workspace-relative path, where the kernel
# .cu.cc compiles' -I<workspace_root> resolves it. (Proper fix = general include_prefix support in
# rush's cc_library analyze [_virtual_includes farm]; deferred.) NOT a stub — the genuine header.
DST="$TF/third_party/farmhash_gpu/src/farmhash_gpu.h"
if [ ! -f "$DST" ]; then
    SRC=$(find "$TF/.cache" -path "*farmhash_gpu_archive/src/farmhash_gpu.h" 2>/dev/null | head -1)
    if [ -n "$SRC" ] && [ -f "$SRC" ]; then
        mkdir -p "$(dirname "$DST")"
        cp "$SRC" "$DST"
        echo "materialized: third_party/farmhash_gpu/src/farmhash_gpu.h (include_prefix)"; n=$((n+1))
    fi
fi

# (4) kernels/conv_ops_gpu.cc — the 2.17 SE GetMIOpenConvolveAlgorithms gained an output_type param
# (ConvolutionKind, element_type, OUTPUT_TYPE, Stream*, …), but the ROCm conv path still calls the
# 12-arg form (kind, element_type, stream, …) — missing output_type. Insert output_type =
# element_type (these conv code paths are uniform-dtype: input/output share T).
F="$TF/tensorflow/core/kernels/conv_ops_gpu.cc"
if [ -f "$F" ] && grep -q 'kind, se::dnn::ToDataType<T>::value, stream,' "$F"; then
    sed -i 's/kind, se::dnn::ToDataType<T>::value, stream,/kind, se::dnn::ToDataType<T>::value, se::dnn::ToDataType<T>::value, stream,/g' "$F"
    echo "fixed: conv_ops_gpu.cc GetMIOpenConvolveAlgorithms +output_type (12->13 args)"; n=$((n+1))
fi

echo "tf217_rocm_source_fixes: applied/confirmed ${n} edit(s)"
