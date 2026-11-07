// GPU runtime EXECUTION: drive the rush-built ROCm StreamExecutor to actually move data through
// the device — allocate device memory, create a stream, memcpy a buffer H2D then D2H, and verify
// the round-trip on the W7900. This proves the runtime EXECUTES (alloc + stream + memcpy via
// rocm_gpu_executor/rocm_stream/rocm_driver), not just discovers the device. Plugin-free: alloc +
// memcpy use no rocBLAS/MIOpen.
#include "xla/stream_executor/multi_platform_manager.h"
#include "xla/stream_executor/platform.h"
#include "xla/stream_executor/stream_executor.h"
#include "xla/stream_executor/stream.h"
#include "xla/stream_executor/device_memory.h"
#include <cstdio>
#include <vector>

int main() {
  auto p_or = stream_executor::MultiPlatformManager::PlatformWithName("ROCM");
  if (!p_or.ok()) {
    printf("FAIL: PlatformWithName(ROCM): %s\n", p_or.status().ToString().c_str());
    return 1;
  }
  stream_executor::Platform* p = p_or.value();
  auto e_or = p->ExecutorForDevice(0);
  if (!e_or.ok()) {
    printf("FAIL: ExecutorForDevice(0): %s\n", e_or.status().ToString().c_str());
    return 1;
  }
  stream_executor::StreamExecutor* exec = e_or.value();

  const int N = 16;
  stream_executor::Stream stream(exec);
  stream.Init();

  stream_executor::DeviceMemory<float> dmem = exec->AllocateArray<float>(N);
  if (dmem.is_null()) {
    printf("FAIL: AllocateArray<float>(%d) returned null\n", N);
    return 1;
  }

  std::vector<float> host_in(N), host_out(N, -1.0f);
  for (int i = 0; i < N; i++) host_in[i] = i * 2.0f + 1.0f;

  stream.ThenMemcpy(&dmem, host_in.data(), N * sizeof(float));   // H2D
  stream.ThenMemcpy(host_out.data(), dmem, N * sizeof(float));   // D2H
  auto st = stream.BlockHostUntilDone();
  if (!st.ok()) {
    printf("FAIL: BlockHostUntilDone: %s\n", st.ToString().c_str());
    return 1;
  }

  int bad = 0;
  for (int i = 0; i < N; i++) {
    if (host_out[i] != host_in[i]) bad++;
  }
  exec->Deallocate(&dmem);
  if (bad) {
    printf("FAIL: %d/%d mismatches in the H2D/D2H round-trip\n", bad, N);
    return 1;
  }
  printf("PASS: rush-built ROCm StreamExecutor H2D/D2H round-trip on the W7900 "
         "(N=%d, out[0]=%g out[15]=%g)\n", N, host_out[0], host_out[N - 1]);
  return 0;
}
