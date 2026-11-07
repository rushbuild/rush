#!/usr/bin/env bash
# gpu5_launch_with_data.sh — prove a rush-built TF GPU kernel COMPUTES correctly on the GPU.
#
# GPU ladder milestone 5 (beyond 4c "fatbinary loads"): rush compiles
# //tensorflow/core/kernels:dense_update_functor_gpu to gfx1100 device code; this script links
# the rush-built libdense_update_functor_gpu.a into a harness that LAUNCHES
# functor::DenseUpdate<Eigen::GpuDevice,float,ADD> over real device tensors and verifies
# params[i] == a[i]+b[i] on the W7900.
#
# Provenance rigor (the DenseUpdate GPU operator() is a WEAK template instantiation, so a naive
# harness that instantiates the functor locally would silently run its OWN copy and prove
# nothing): the harness (scripts/gpu5_launch.cpp) DECLARES the specialization with NO body, so the
# call is an undefined extern that must resolve from the archive. The script asserts (a) the
# harness object references DenseUpdate as Undefined (U), (b) linking WITHOUT the archive FAILS
# (negative control), and (c) the linked binary runs and computes the correct result on the card.
#
# Usage: ./scripts/gpu5_launch_with_data.sh        (run on the ROCm host / W7900, e.g. prune)
set -euo pipefail

RUSH="${RUSH:-/data/rush/target/release/rush}"
TF="${TF:-/data/tensorflow}"
HIPCC="${HIPCC:-/opt/rocm/bin/hipcc}"
SRC="${SRC:-/data/rush/scripts/gpu5_launch.cpp}"
KERNEL_LABEL="//tensorflow/core/kernels:dense_update_functor_gpu"
KERNEL_A="$TF/rush-out/tensorflow/core/kernels/dense_update_functor_gpu/libdense_update_functor_gpu.a"
EXT="$TF/.cache/rush/external"
INC="-I$EXT/eigen_archive -I$EXT/ml_dtypes -I$EXT/ml_dtypes/ml_dtypes -I$EXT/local_config_rocm/rocm_config_root -I/opt/rocm/include -I$TF"
WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT

echo "== 1. build the TF GPU kernel via rush (ROCm) =="
( cd "$TF" && RUSH_ENABLE_ROCM=1 "$RUSH" build "$KERNEL_LABEL" )
GFX="$(strings "$KERNEL_A" | grep -cE 'amdgcn-amd-amdhsa--gfx1100' || true)"
[ "${GFX:-0}" -gt 0 ] && echo "  ok: gfx1100 device code present ($GFX)" || { echo "  FAIL: no gfx1100 in $KERNEL_A"; exit 1; }

echo "== 2. compile the launch-with-data harness (hipcc, ABI-matched includes) =="
"$HIPCC" --offload-arch=gfx1100 -std=c++17 -O2 $INC -c "$SRC" -o "$WORK/gpu5.o"

echo "== 3. provenance: harness references DenseUpdate as UNDEFINED (not a local copy) =="
if nm "$WORK/gpu5.o" | grep -E '^ +U .*DenseUpdateIN5Eigen9GpuDeviceEf' >/dev/null; then
  echo "  ok: DenseUpdate<GpuDevice,float,..> is an undefined extern in the harness"
else
  echo "  FAIL: harness does not reference the archive symbol as undefined"; exit 1
fi

echo "== 4. negative control: linking WITHOUT the archive must FAIL =="
if "$HIPCC" "$WORK/gpu5.o" -o "$WORK/neg" 2>/dev/null; then
  echo "  FAIL: linked without the archive — the harness has a local copy, proof is hollow"; exit 1
else
  echo "  ok: link fails without the archive (the rush-built symbol is load-bearing)"
fi

echo "== 5. link whole-archive + run on the GPU =="
"$HIPCC" "$WORK/gpu5.o" -Wl,--whole-archive "$KERNEL_A" -Wl,--no-whole-archive -o "$WORK/gpu5_test"
HIP_VISIBLE_DEVICES=0 "$WORK/gpu5_test"
echo "== 5 OK: rush-built TF GPU kernel computed a correct result on the W7900 =="
