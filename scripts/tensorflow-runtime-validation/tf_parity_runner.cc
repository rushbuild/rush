#include <algorithm>
#include <charconv>
#include <chrono>
#include <cstdint>
#include <cstring>
#include <fstream>
#include <iomanip>
#include <iostream>
#include <memory>
#include <sstream>
#include <string>
#include <utility>
#include <vector>

#include "tensorflow/core/framework/device_attributes.pb.h"
#include "tensorflow/core/framework/graph.pb.h"
#include "tensorflow/core/framework/tensor.h"
#include "tensorflow/core/framework/tensor_shape.h"
#include "tensorflow/core/framework/types.h"
#include "tensorflow/core/lib/core/errors.h"
#include "tensorflow/core/platform/env.h"
#include "tensorflow/core/platform/protobuf.h"
#include "tensorflow/core/public/session.h"
#include "tensorflow/core/protobuf/config.pb.h"

namespace {

using tensorflow::DataType;
using tensorflow::GraphDef;
using tensorflow::Session;
using tensorflow::Status;
using tensorflow::Tensor;
using tensorflow::TensorShape;

struct FeedSpec {
  std::string name;
  DataType dtype;
  TensorShape shape;
  std::string path;
};

struct Options {
  std::string graph;
  std::string output_dir;
  std::vector<FeedSpec> feeds;
  std::vector<std::string> series_fetches;
  std::vector<std::string> final_fetches;
  std::vector<std::string> targets;
  std::vector<std::string> init_targets;
  int steps = 1;
  int warmup = 0;
  int intra_op = 0;
  int inter_op = 0;
  bool require_gpu = false;
};

std::vector<std::string> Split(const std::string& value, char delimiter) {
  std::vector<std::string> parts;
  std::stringstream stream(value);
  std::string part;
  while (std::getline(stream, part, delimiter)) {
    parts.push_back(part);
  }
  return parts;
}

bool StartsWith(const std::string& value, const std::string& prefix) {
  return value.compare(0, prefix.size(), prefix) == 0;
}

Status ParseInteger(const std::string& value, const std::string& flag,
                    int* output) {
  int parsed = 0;
  const char* begin = value.data();
  const char* end = begin + value.size();
  const auto result = std::from_chars(begin, end, parsed);
  if (result.ec != std::errc() || result.ptr != end) {
    return tensorflow::errors::InvalidArgument("Invalid ", flag, ": ", value);
  }
  *output = parsed;
  return tensorflow::OkStatus();
}

Status ParseDType(const std::string& value, DataType* dtype) {
  if (value == "float" || value == "float32") {
    *dtype = tensorflow::DT_FLOAT;
  } else if (value == "double" || value == "float64") {
    *dtype = tensorflow::DT_DOUBLE;
  } else if (value == "int32") {
    *dtype = tensorflow::DT_INT32;
  } else if (value == "int64") {
    *dtype = tensorflow::DT_INT64;
  } else if (value == "uint8") {
    *dtype = tensorflow::DT_UINT8;
  } else if (value == "int8") {
    *dtype = tensorflow::DT_INT8;
  } else if (value == "bool") {
    *dtype = tensorflow::DT_BOOL;
  } else {
    return tensorflow::errors::InvalidArgument("Unsupported feed dtype: ",
                                                value);
  }
  return tensorflow::OkStatus();
}

Status ParseShape(const std::string& value, TensorShape* shape) {
  if (value.empty() || value == "scalar") {
    return tensorflow::OkStatus();
  }
  for (const std::string& dimension : Split(value, 'x')) {
    int parsed = 0;
    Status status = ParseInteger(dimension, "shape dimension", &parsed);
    if (!status.ok()) return status;
    if (parsed < 0) {
      return tensorflow::errors::InvalidArgument(
          "Feed dimensions must be non-negative: ", value);
    }
    shape->AddDim(parsed);
  }
  return tensorflow::OkStatus();
}

Status ParseFeed(const std::string& value, FeedSpec* feed) {
  std::vector<std::string> parts = Split(value, ',');
  if (parts.size() != 4) {
    return tensorflow::errors::InvalidArgument(
        "--feed requires NAME,DTYPE,SHAPE,FILE: ", value);
  }
  feed->name = parts[0];
  Status status = ParseDType(parts[1], &feed->dtype);
  if (!status.ok()) return status;
  status = ParseShape(parts[2], &feed->shape);
  if (!status.ok()) return status;
  feed->path = parts[3];
  return tensorflow::OkStatus();
}

Status ParseOptions(int argc, char** argv, Options* options) {
  for (int index = 1; index < argc; ++index) {
    const std::string arg(argv[index]);
    if (StartsWith(arg, "--graph=")) {
      options->graph = arg.substr(strlen("--graph="));
    } else if (StartsWith(arg, "--output-dir=")) {
      options->output_dir = arg.substr(strlen("--output-dir="));
    } else if (StartsWith(arg, "--feed=")) {
      FeedSpec feed;
      Status status = ParseFeed(arg.substr(strlen("--feed=")), &feed);
      if (!status.ok()) return status;
      options->feeds.push_back(std::move(feed));
    } else if (StartsWith(arg, "--series-fetch=")) {
      options->series_fetches.push_back(
          arg.substr(strlen("--series-fetch=")));
    } else if (StartsWith(arg, "--final-fetch=")) {
      options->final_fetches.push_back(arg.substr(strlen("--final-fetch=")));
    } else if (StartsWith(arg, "--target=")) {
      options->targets.push_back(arg.substr(strlen("--target=")));
    } else if (StartsWith(arg, "--init-target=")) {
      options->init_targets.push_back(arg.substr(strlen("--init-target=")));
    } else if (StartsWith(arg, "--steps=")) {
      Status status =
          ParseInteger(arg.substr(strlen("--steps=")), "--steps",
                       &options->steps);
      if (!status.ok()) return status;
    } else if (StartsWith(arg, "--warmup=")) {
      Status status =
          ParseInteger(arg.substr(strlen("--warmup=")), "--warmup",
                       &options->warmup);
      if (!status.ok()) return status;
    } else if (StartsWith(arg, "--intra-op=")) {
      Status status =
          ParseInteger(arg.substr(strlen("--intra-op=")), "--intra-op",
                       &options->intra_op);
      if (!status.ok()) return status;
    } else if (StartsWith(arg, "--inter-op=")) {
      Status status =
          ParseInteger(arg.substr(strlen("--inter-op=")), "--inter-op",
                       &options->inter_op);
      if (!status.ok()) return status;
    } else if (arg == "--require-gpu") {
      options->require_gpu = true;
    } else {
      return tensorflow::errors::InvalidArgument("Unknown argument: ", arg);
    }
  }

  if (options->graph.empty()) {
    return tensorflow::errors::InvalidArgument("--graph is required");
  }
  if (options->output_dir.empty()) {
    return tensorflow::errors::InvalidArgument("--output-dir is required");
  }
  if (options->steps <= 0 || options->warmup < 0) {
    return tensorflow::errors::InvalidArgument(
        "--steps must be positive and --warmup must be non-negative");
  }
  if (options->series_fetches.empty() && options->final_fetches.empty()) {
    return tensorflow::errors::InvalidArgument(
        "At least one --series-fetch or --final-fetch is required");
  }
  return tensorflow::OkStatus();
}

Status LoadFeed(const FeedSpec& spec,
                std::pair<std::string, Tensor>* loaded) {
  Tensor tensor(spec.dtype, spec.shape);
  const size_t element_size = tensorflow::DataTypeSize(spec.dtype);
  if (element_size == 0) {
    return tensorflow::errors::InvalidArgument(
        "Feed has variable-width or unsupported dtype: ", spec.name);
  }
  const size_t expected =
      static_cast<size_t>(tensor.NumElements()) * element_size;

  std::ifstream input(spec.path, std::ios::binary | std::ios::ate);
  if (!input) {
    return tensorflow::errors::NotFound("Cannot open feed file: ", spec.path);
  }
  const std::streamoff observed = input.tellg();
  if (observed < 0 || static_cast<size_t>(observed) != expected) {
    return tensorflow::errors::InvalidArgument(
        "Feed byte count mismatch for ", spec.name, ": expected ", expected,
        ", observed ", observed);
  }
  input.seekg(0);
  auto bytes = tensor.tensor_data();
  input.read(const_cast<char*>(bytes.data()), expected);
  if (!input) {
    return tensorflow::errors::DataLoss("Failed to read feed file: ",
                                        spec.path);
  }
  *loaded = {spec.name, std::move(tensor)};
  return tensorflow::OkStatus();
}

std::string Sanitize(const std::string& value) {
  std::string result;
  result.reserve(value.size());
  for (char character : value) {
    const bool safe =
        (character >= 'a' && character <= 'z') ||
        (character >= 'A' && character <= 'Z') ||
        (character >= '0' && character <= '9') || character == '-' ||
        character == '_';
    result.push_back(safe ? character : '_');
  }
  return result;
}

std::string ShapeString(const TensorShape& shape) {
  if (shape.dims() == 0) return "scalar";
  std::ostringstream output;
  for (int index = 0; index < shape.dims(); ++index) {
    if (index != 0) output << 'x';
    output << shape.dim_size(index);
  }
  return output.str();
}

Status WriteTensor(const std::string& output_dir, const std::string& kind,
                   int step, const std::string& name, const Tensor& tensor,
                   std::ofstream* manifest) {
  std::ostringstream filename;
  filename << kind;
  if (step >= 0) {
    filename << '-' << std::setfill('0') << std::setw(4) << step;
  }
  filename << '-' << Sanitize(name) << ".bin";
  const std::string path = output_dir + "/" + filename.str();

  std::ofstream output(path, std::ios::binary);
  if (!output) {
    return tensorflow::errors::Unknown("Cannot create output file: ", path);
  }
  const auto bytes = tensor.tensor_data();
  output.write(bytes.data(), bytes.size());
  if (!output) {
    return tensorflow::errors::DataLoss("Failed to write output file: ", path);
  }

  *manifest << kind << '\t' << step << '\t' << name << '\t'
            << tensorflow::DataTypeString(tensor.dtype()) << '\t'
            << ShapeString(tensor.shape()) << '\t' << path << '\n';
  return tensorflow::OkStatus();
}

Status RunTargets(Session* session,
                  const std::vector<std::pair<std::string, Tensor>>& feeds,
                  const std::vector<std::string>& targets) {
  if (targets.empty()) return tensorflow::OkStatus();
  std::vector<Tensor> ignored;
  return session->Run(feeds, {}, targets, &ignored);
}

int Main(int argc, char** argv) {
  Options options;
  Status status = ParseOptions(argc, argv, &options);
  if (!status.ok()) {
    std::cerr << status << '\n';
    return 2;
  }

  status = tensorflow::Env::Default()->RecursivelyCreateDir(options.output_dir);
  if (!status.ok()) {
    std::cerr << status << '\n';
    return 1;
  }

  GraphDef graph;
  status =
      tensorflow::ReadBinaryProto(tensorflow::Env::Default(), options.graph,
                                  &graph);
  if (!status.ok()) {
    std::cerr << status << '\n';
    return 1;
  }

  tensorflow::SessionOptions session_options;
  session_options.config.set_allow_soft_placement(true);
  session_options.config.mutable_gpu_options()->set_allow_growth(true);
  if (options.intra_op > 0) {
    session_options.config.set_intra_op_parallelism_threads(options.intra_op);
  }
  if (options.inter_op > 0) {
    session_options.config.set_inter_op_parallelism_threads(options.inter_op);
  }

  std::unique_ptr<Session> session(tensorflow::NewSession(session_options));
  if (!session) {
    std::cerr << "NewSession returned null\n";
    return 1;
  }

  std::vector<tensorflow::DeviceAttributes> devices;
  status = session->ListDevices(&devices);
  if (!status.ok()) {
    std::cerr << status << '\n';
    return 1;
  }
  bool has_gpu = false;
  std::ofstream device_output(options.output_dir + "/devices.tsv");
  device_output << "name\ttype\tmemory_limit\n";
  for (const auto& device : devices) {
    device_output << device.name() << '\t' << device.device_type() << '\t'
                  << device.memory_limit() << '\n';
    has_gpu = has_gpu || device.device_type() == "GPU";
  }
  if (options.require_gpu && !has_gpu) {
    std::cerr << "GPU was required, but this TensorFlow runtime exposed none\n";
    return 1;
  }

  status = session->Create(graph);
  if (!status.ok()) {
    std::cerr << status << '\n';
    return 1;
  }

  std::vector<std::pair<std::string, Tensor>> feeds;
  feeds.reserve(options.feeds.size());
  for (const FeedSpec& spec : options.feeds) {
    std::pair<std::string, Tensor> feed;
    status = LoadFeed(spec, &feed);
    if (!status.ok()) {
      std::cerr << status << '\n';
      return 1;
    }
    feeds.push_back(std::move(feed));
  }

  status = RunTargets(session.get(), {}, options.init_targets);
  if (!status.ok()) {
    std::cerr << "Initialization failed: " << status << '\n';
    return 1;
  }

  const std::vector<std::string>& run_fetches =
      options.series_fetches.empty() ? options.final_fetches
                                     : options.series_fetches;
  for (int step = 0; step < options.warmup; ++step) {
    std::vector<Tensor> ignored;
    status =
        session->Run(feeds, run_fetches, options.targets, &ignored);
    if (!status.ok()) {
      std::cerr << "Warmup step " << step << " failed: " << status << '\n';
      return 1;
    }
  }
  if (options.warmup > 0 && !options.init_targets.empty()) {
    status = RunTargets(session.get(), {}, options.init_targets);
    if (!status.ok()) {
      std::cerr << "Post-warmup reinitialization failed: " << status << '\n';
      return 1;
    }
  }

  std::ofstream timings(options.output_dir + "/timings.tsv");
  timings << "step\telapsed_ns\n";
  std::ofstream outputs(options.output_dir + "/outputs.tsv");
  outputs << "kind\tstep\tname\tdtype\tshape\tpath\n";

  for (int step = 0; step < options.steps; ++step) {
    std::vector<Tensor> fetched;
    const auto start = std::chrono::steady_clock::now();
    status =
        session->Run(feeds, run_fetches, options.targets, &fetched);
    const auto stop = std::chrono::steady_clock::now();
    if (!status.ok()) {
      std::cerr << "Measured step " << step << " failed: " << status << '\n';
      return 1;
    }
    const auto elapsed =
        std::chrono::duration_cast<std::chrono::nanoseconds>(stop - start)
            .count();
    timings << step << '\t' << elapsed << '\n';

    if (!options.series_fetches.empty()) {
      for (size_t index = 0; index < fetched.size(); ++index) {
        status = WriteTensor(options.output_dir, "series", step,
                             options.series_fetches[index], fetched[index],
                             &outputs);
        if (!status.ok()) {
          std::cerr << status << '\n';
          return 1;
        }
      }
    }
  }

  if (!options.final_fetches.empty()) {
    std::vector<Tensor> final_outputs;
    status = session->Run(feeds, options.final_fetches, {}, &final_outputs);
    if (!status.ok()) {
      std::cerr << "Final fetch failed: " << status << '\n';
      return 1;
    }
    for (size_t index = 0; index < final_outputs.size(); ++index) {
      status = WriteTensor(options.output_dir, "final", -1,
                           options.final_fetches[index], final_outputs[index],
                           &outputs);
      if (!status.ok()) {
        std::cerr << status << '\n';
        return 1;
      }
    }
  }

  status = session->Close();
  if (!status.ok()) {
    std::cerr << "Session close failed: " << status << '\n';
    return 1;
  }
  return 0;
}

}  // namespace

int main(int argc, char** argv) { return Main(argc, argv); }
