/* Copyright 2026 The TensorFlow Authors. All Rights Reserved.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
==============================================================================*/

// gpu_matmul_workload: a tiny, self-contained TensorFlow C++ program that runs
// real compute on an accelerator. It performs power iteration to estimate the
// dominant eigenvalue of a fixed 2x2 matrix A = [[3, 2], [-1, 0]] by repeatedly
// evaluating y = A * x and renormalizing x = y / ||y||. Every op is placed on
// the requested device (default "/device:GPU:0") so the MatMul / reduction
// kernels execute on the GPU. Pass --device=/device:CPU:0 to run on the host.
//
// The program builds its graph in-process via the C++ ops API, runs it with a
// ClientSession (log_device_placement enabled), and prints the per-iteration
// eigenvalue estimate plus the vector. No external data or model files.

#include <cmath>
#include <cstdio>
#include <string>
#include <vector>

#include "tensorflow/cc/client/client_session.h"
#include "tensorflow/cc/ops/standard_ops.h"
#include "tensorflow/core/framework/tensor.h"
#include "tensorflow/core/platform/init_main.h"
#include "tensorflow/core/platform/logging.h"
#include "tensorflow/core/platform/types.h"
#include "tensorflow/core/protobuf/config.pb.h"
#include "tensorflow/core/public/session_options.h"
#include "tensorflow/core/util/command_line_flags.h"

namespace tensorflow {
namespace example {

int RunWorkload(const string& device, int num_iterations) {
  // Build the dataflow graph: everything pinned to `device`.
  Scope root = Scope::NewRootScope().WithDevice(device);
  using namespace ::tensorflow::ops;  // NOLINT(build/namespaces)

  // A = [[3, 2], [-1, 0]] as a float constant.
  auto a = Const<float>(root, {{3.f, 2.f}, {-1.f, 0.f}});

  // x is fed each iteration (2x1 column vector).
  auto x = Placeholder(root.WithOpName("x"), DT_FLOAT);

  // y = A * x  (this is the GPU MatMul kernel).
  auto y = MatMul(root.WithOpName("y"), a, x);

  // y_norm = sqrt(sum(y .^ 2))  (Square + Sum reduction on the device).
  auto y2 = Square(root, y);
  auto y2_sum = Sum(root, y2, /*axis=*/0);
  auto y_norm = Sqrt(root, y2_sum);

  // y_normalized = y / y_norm.
  auto y_normalized = Div(root.WithOpName("y_normalized"), y, y_norm);

  if (!root.status().ok()) {
    LOG(ERROR) << root.status();
    return 1;
  }

  // Session with device-placement logging so we can confirm ops ran on the GPU.
  SessionOptions options;
  options.config.set_log_device_placement(true);
  ClientSession session(root, options);

  // x0 = [1, 0]^T.
  Tensor x_val(DT_FLOAT, TensorShape({2, 1}));
  x_val.flat<float>()(0) = 1.f;
  x_val.flat<float>()(1) = 0.f;

  std::printf("gpu_matmul_workload: device=%s iterations=%d\n", device.c_str(),
              num_iterations);

  float lambda = 0.f;
  for (int iter = 0; iter < num_iterations; ++iter) {
    std::vector<Tensor> outputs;
    Status s = session.Run({{x, x_val}}, {y, y_normalized}, &outputs);
    if (!s.ok()) {
      LOG(ERROR) << "Run failed: " << s;
      return 1;
    }
    const Tensor& y_out = outputs[0];
    const Tensor& y_norm_out = outputs[1];

    // Rayleigh-quotient eigenvalue estimate: lambda ~= x^T y (since ||x|| = 1).
    auto x_flat = x_val.flat<float>();
    auto y_flat = y_out.flat<float>();
    lambda = x_flat(0) * y_flat(0) + x_flat(1) * y_flat(1);

    std::printf("iter %03d  lambda = %9.6f  x = [% 9.6f % 9.6f]\n", iter, lambda,
                x_flat(0), x_flat(1));

    // Feed the renormalized vector back in for the next iteration.
    x_val = y_norm_out;
  }

  // For A = [[3,2],[-1,0]] the dominant eigenvalue is 2.
  std::printf("converged dominant eigenvalue estimate = %9.6f (expected ~2)\n",
              lambda);
  return 0;
}

}  // namespace example
}  // namespace tensorflow

int main(int argc, char* argv[]) {
  tensorflow::string device = "/device:GPU:0";
  int num_iterations = 30;
  std::vector<tensorflow::Flag> flag_list = {
      tensorflow::Flag("device", &device,
                       "Device to pin all ops to (e.g. /device:GPU:0 or "
                       "/device:CPU:0)."),
      tensorflow::Flag("num_iterations", &num_iterations,
                       "Number of power-iteration steps to run."),
  };
  const tensorflow::string usage =
      tensorflow::Flags::Usage(argv[0], flag_list);
  if (!tensorflow::Flags::Parse(&argc, argv, flag_list)) {
    LOG(ERROR) << usage;
    return -1;
  }
  tensorflow::port::InitMain(argv[0], &argc, &argv);
  if (argc > 1) {
    LOG(ERROR) << "Unknown argument " << argv[1] << "\n" << usage;
    return -1;
  }
  return tensorflow::example::RunWorkload(device, num_iterations);
}
