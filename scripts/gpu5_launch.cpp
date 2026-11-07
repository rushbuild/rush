// Path Y: launch a rush-built TF GPU kernel (DenseUpdate<GpuDevice,float,ADD>) with real
// tensors on the W7900 and verify a correct result. The functor body lives in the rush-built
// libdense_update_functor_gpu.a (compiled from dense_update_functor_gpu.cu.cc). We DECLARE the
// specialization with NO body so the call emits an undefined extern the linker must resolve from
// the archive's weak symbol — never a local copy. Mangling must match the archive exactly:
// TTypes<float>::Flat == TensorMap<Tensor<float,1,1,long>,16,MakePointer> (Aligned=16, RowMajor).
#define EIGEN_USE_GPU
#define EIGEN_USE_HIP
#define TENSORFLOW_USE_ROCM 1
#define __HIP_PLATFORM_AMD__ 1
#include <hip/hip_runtime.h>
#include "unsupported/Eigen/CXX11/Tensor"
#include <cstdio>
#include <vector>

namespace tensorflow {
enum DenseUpdateType { ADD, SUB, ASSIGN };
namespace functor {
// Declaration only — body is in the rush-built archive (.cu.cc).
template <typename Device, typename T, DenseUpdateType OP>
struct DenseUpdate {
  void operator()(
      const Device& d,
      Eigen::TensorMap<Eigen::Tensor<T, 1, 1, long>, 16, Eigen::MakePointer> params,
      Eigen::TensorMap<Eigen::Tensor<const T, 1, 1, long>, 16, Eigen::MakePointer> update);
};
}  // namespace functor
}  // namespace tensorflow

int main() {
  const int N = 8;
  hipSetDevice(0);
  Eigen::GpuStreamDevice stream_device;
  Eigen::GpuDevice gpu(&stream_device);

  float *d_params = nullptr, *d_update = nullptr;
  hipMalloc(&d_params, N * sizeof(float));
  hipMalloc(&d_update, N * sizeof(float));
  std::vector<float> a(N), b(N);
  for (int i = 0; i < N; i++) { a[i] = i; b[i] = 100 + i; }
  hipMemcpy(d_params, a.data(), N * sizeof(float), hipMemcpyHostToDevice);
  hipMemcpy(d_update, b.data(), N * sizeof(float), hipMemcpyHostToDevice);

  Eigen::TensorMap<Eigen::Tensor<float, 1, 1, long>, 16, Eigen::MakePointer> params_map(d_params, N);
  Eigen::TensorMap<Eigen::Tensor<const float, 1, 1, long>, 16, Eigen::MakePointer> update_map(d_update, N);

  // Calls the rush-built archive symbol (ADD: params.device(d) += update).
  tensorflow::functor::DenseUpdate<Eigen::GpuDevice, float, tensorflow::ADD>()(gpu, params_map, update_map);
  hipStreamSynchronize(nullptr);

  std::vector<float> out(N);
  hipMemcpy(out.data(), d_params, N * sizeof(float), hipMemcpyDeviceToHost);
  int bad = 0;
  for (int i = 0; i < N; i++) {
    if (out[i] != a[i] + b[i]) { bad++; if (bad <= 3) printf("  mismatch[%d]: got %f want %f\n", i, out[i], a[i] + b[i]); }
  }
  if (bad) { printf("FAIL: %d mismatches\n", bad); return 1; }
  printf("PASS: rush-built DenseUpdate<GpuDevice,float,ADD> computed correctly on the GPU\n");
  printf("  out[0]=%g (want %g)  out[7]=%g (want %g)\n", out[0], a[0] + b[0], out[7], a[7] + b[7]);
  return 0;
}
