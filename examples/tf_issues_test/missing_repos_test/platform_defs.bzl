# This .bzl file attempts to load from non-existent external repos
# Similar to TensorFlow's tensorflow.bzl

load("@local_config_cuda//cuda:build_defs.bzl", "if_cuda")
load("@local_config_rocm//rocm:build_defs.bzl", "if_rocm")

def platform_library(name, srcs, hdrs):
    """
    A macro that tries to use symbols from non-existent repos.
    Rush should handle this gracefully with fallback behavior.
    """
    native.cc_library(
        name = name,
        srcs = srcs,
        hdrs = hdrs,
        defines = if_cuda(["CUDA_ENABLED=1"], []) + if_rocm(["ROCM_ENABLED=1"], []),
    )
