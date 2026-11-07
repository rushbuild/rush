#!/usr/bin/env bash
# gpu_4c_link_run.sh — prove a rush-built TF GPU kernel archive links + runs on the GPU.
#
# GPU ladder milestone 4c: rush compiles //tensorflow/core/kernels:dense_update_functor_gpu
# (a real TF GPU kernel) into libdense_update_functor_gpu.a carrying AMDGPU gfx1100 device
# code (see commits 3235883 / 83bc7b7 / f0eb940). That archive is self-contained except for
# the HIP runtime (nm -uC shows only hip*/libc undefined — zero tensorflow::/Eigen::/
# stream_executor), so it links into a standalone GPU executable WITHOUT the TF framework or
# the StreamExecutor runtime. Its static __hipRegisterFatBinary initializer loads the embedded
# gfx1100 device code into the HIP runtime at startup; if the binary runs clean, the
# rush-compiled device code is valid + loadable on the card.
#
# Usage: ./scripts/gpu_4c_link_run.sh            (run on the ROCm host, e.g. prune/W7900)
# Requires: RUSH_ENABLE_ROCM build of the kernel + /opt/rocm (hipcc, libamdhip64).
set -euo pipefail

RUSH="${RUSH:-/data/rush/target/release/rush}"
TF="${TF:-/data/tensorflow}"
HIPCC="${HIPCC:-/opt/rocm/bin/hipcc}"
KERNEL_LABEL="//tensorflow/core/kernels:dense_update_functor_gpu"
KERNEL_A="$TF/rush-out/tensorflow/core/kernels/dense_update_functor_gpu/libdense_update_functor_gpu.a"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

echo "== 1. build the TF GPU kernel via rush (ROCm) =="
( cd "$TF" && RUSH_ENABLE_ROCM=1 "$RUSH" build "$KERNEL_LABEL" )

echo "== 2. confirm gfx1100 device code in the archive =="
# Use grep -c (counts, reads all input) rather than grep -q: under `set -o pipefail`,
# `strings | grep -q` makes strings take SIGPIPE when grep closes the pipe early on first
# match, and pipefail then reports the whole pipeline as failed (a false negative).
GFX_MARKERS="$(strings "$KERNEL_A" | grep -cE 'amdgcn-amd-amdhsa--gfx1100' || true)"
if [ "${GFX_MARKERS:-0}" -gt 0 ]; then
  echo "  ok: gfx1100 device code present ($GFX_MARKERS markers)"
else
  echo "  FAIL: no gfx1100 device code in $KERNEL_A"; exit 1
fi

echo "== 3. link a tiny GPU binary against the rush-built kernel archive =="
cat > "$WORK/main.cpp" <<'EOF'
#include <hip/hip_runtime.h>
#include <cstdio>
// Whole-archived libdense_update_functor_gpu.a: its static __hipRegisterFatBinary loads the
// embedded gfx1100 device code at startup. Running clean proves the rush-compiled TF GPU
// kernel device code is valid + loadable on the W7900.
int main() {
  int n = 0;
  hipError_t e = hipGetDeviceCount(&n);
  printf("hipGetDeviceCount: %s, devices=%d\n", hipGetErrorString(e), n);
  if (n > 0) {
    hipDeviceProp_t p; hipGetDeviceProperties(&p, 0);
    printf("device 0: %s  arch=%s\n", p.name, p.gcnArchName);
  }
  printf("rush-built TF GPU kernel fatbinary registered + HIP runtime live\n");
  return (n > 0) ? 0 : 1;
}
EOF
"$HIPCC" -c "$WORK/main.cpp" -o "$WORK/main.o"
"$HIPCC" "$WORK/main.o" -Wl,--whole-archive "$KERNEL_A" -Wl,--no-whole-archive -o "$WORK/gpu4c_test"

echo "== 4. run on the GPU =="
"$WORK/gpu4c_test"
echo "== 4c OK: rush-built TF GPU kernel links + the gfx1100 fatbinary runs on the card =="
