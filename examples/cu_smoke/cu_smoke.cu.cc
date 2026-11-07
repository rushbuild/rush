// A .cu.cc GPU kernel — the source form TensorFlow's GPU kernels take (CUDA-style
// __global__ + <<<>>> launch). hipcc compiles .cu.cc as HIP directly, so rush builds
// it as a cc_binary with the compiler swapped to hipcc (RUSH_GPU_CXX). Runs on the W7900.
#include <hip/hip_runtime.h>
#include <cstdio>
__global__ void scale2(float *a) {
  int i = blockIdx.x * blockDim.x + threadIdx.x;
  a[i] *= 2.0f;
}
int main() {
  const int n = 8;
  float h[n];
  float *d = nullptr;
  for (int i = 0; i < n; i++) h[i] = (float)i;
  hipMalloc(&d, n * sizeof(float));
  hipMemcpy(d, h, n * sizeof(float), hipMemcpyHostToDevice);
  scale2<<<1, n>>>(d);
  hipDeviceSynchronize();
  hipMemcpy(h, d, n * sizeof(float), hipMemcpyDeviceToHost);
  hipFree(d);
  hipDeviceProp_t p;
  hipGetDeviceProperties(&p, 0);
  printf("GPU: %s (gfx arch %s)\n", p.name, p.gcnArchName);
  printf("result:");
  for (int i = 0; i < n; i++) printf(" %.0f", h[i]);
  printf("\n");
  return (h[7] == 14.0f) ? 0 : 1;
}
