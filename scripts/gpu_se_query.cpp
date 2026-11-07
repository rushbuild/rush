// GPU runtime device-query: register the rush-built ROCm StreamExecutor backend, find the ROCM
// platform via MultiPlatformManager, get the StreamExecutor for device 0, and print the device
// description. This exercises the rush-built ROCm runtime (rocm_platform + the rocm/ backend
// archives, NO BLAS/DNN plugins) end-to-end: static registration -> platform lookup -> executor ->
// device discovery on the W7900. The legacy-ABI overlay makes the runtime's gcnArch read return
// 1100 (verified separately).
#include "xla/stream_executor/multi_platform_manager.h"
#include "xla/stream_executor/platform.h"
#include "xla/stream_executor/stream_executor.h"
#include "xla/stream_executor/device_description.h"
#include <cstdio>
#include <string>

int main() {
  auto p_or = stream_executor::MultiPlatformManager::PlatformWithName("ROCM");
  if (!p_or.ok()) {
    printf("FAIL: PlatformWithName(ROCM): %s\n", p_or.status().ToString().c_str());
    return 1;
  }
  stream_executor::Platform* p = p_or.value();
  printf("platform=%s visible_devices=%d\n", std::string(p->Name()).c_str(),
         p->VisibleDeviceCount());
  if (p->VisibleDeviceCount() <= 0) {
    printf("FAIL: no visible ROCm devices\n");
    return 1;
  }
  auto e_or = p->ExecutorForDevice(0);
  if (!e_or.ok()) {
    printf("FAIL: ExecutorForDevice(0): %s\n", e_or.status().ToString().c_str());
    return 1;
  }
  stream_executor::StreamExecutor* e = e_or.value();
  const stream_executor::DeviceDescription& d = e->GetDeviceDescription();
  printf("PASS: rush-built ROCm StreamExecutor device 0: name=%s\n", d.name().c_str());
  return 0;
}
