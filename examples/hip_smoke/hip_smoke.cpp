// HIP smoke test — runs a trivial kernel on the GPU to prove rush can drive the
// ROCm/HIP toolchain (hipcc) end to end. Built by rush as a cc_binary with the
// compiler swapped to hipcc via RUSH_GPU_CXX. Expected on an AMD W7900 (gfx1100):
//   GPU: AMD Radeon Pro W7900 (gfx arch gfx1100)
//   result: 100 101 102 103 104 105 106 107
#include <hip/hip_runtime.h>
#include <cstdio>

__global__ void add100(int *a) { a[threadIdx.x] += 100; }

int main() {
  const int n = 8;
  int h[n];
  int *d = nullptr;
  for (int i = 0; i < n; i++) h[i] = i;
  if (hipMalloc(&d, n * sizeof(int)) != hipSuccess) {
    printf("hipMalloc FAIL\n");
    return 2;
  }
  hipMemcpy(d, h, n * sizeof(int), hipMemcpyHostToDevice);
  add100<<<1, n>>>(d);
  hipDeviceSynchronize();
  hipMemcpy(h, d, n * sizeof(int), hipMemcpyDeviceToHost);
  hipFree(d);

  hipDeviceProp_t p;
  hipGetDeviceProperties(&p, 0);
  printf("GPU: %s (gfx arch %s)\n", p.name, p.gcnArchName);
  printf("result:");
  for (int i = 0; i < n; i++) printf(" %d", h[i]);
  printf("\n");
  return (h[7] == 107) ? 0 : 1;
}
