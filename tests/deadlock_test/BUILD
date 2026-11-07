# Deadlock test case - simulates TensorFlow workspace2.bzl pattern
# Multiple files load cuda_stub.bzl concurrently
# cuda_stub.bzl loads from @bazel_tools (stub repo)

load(":cuda_stub.bzl", "cuda_configure")
load(":rocm_stub.bzl", "rocm_configure")
load(":nccl_stub.bzl", "nccl_configure")

filegroup(
    name = "test",
    srcs = [],
)
