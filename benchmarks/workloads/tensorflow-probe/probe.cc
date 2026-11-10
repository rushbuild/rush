#include <iostream>
#include "tensorflow/core/framework/tensor.h"
#include "tensorflow/core/framework/tensor.pb.h"

int main() {
  tensorflow::Tensor original(tensorflow::DT_FLOAT, tensorflow::TensorShape({2, 3}));
  auto values = original.flat<float>();
  for (int i = 0; i != 6; ++i) values(i) = static_cast<float>(i + 1);
  tensorflow::TensorProto proto;
  original.AsProtoTensorContent(&proto);
  if (proto.tensor_content().size() != 6 * sizeof(float)) return 10;
  tensorflow::Tensor restored;
  if (!restored.FromProto(proto)) return 11;
  if (restored.dtype() != tensorflow::DT_FLOAT || restored.dims() != 2 ||
      restored.dim_size(0) != 2 || restored.dim_size(1) != 3) return 12;
  auto roundtrip = restored.flat<float>();
  for (int i = 0; i != 6; ++i)
    if (roundtrip(i) != static_cast<float>(i + 1)) return 13;
  tensorflow::Tensor slice = restored.Slice(1, 2);
  if (slice.dtype() != tensorflow::DT_FLOAT || slice.dims() != 2 ||
      slice.dim_size(0) != 1 || slice.dim_size(1) != 3) return 14;
  auto sliced = slice.unaligned_flat<float>();
  for (int i = 0; i != 3; ++i)
    if (sliced(i) != static_cast<float>(i + 4)) return 15;
  std::cout << "TF_TENSOR_ROUNDTRIP_SLICE_OK\n";
  return 0;
}
