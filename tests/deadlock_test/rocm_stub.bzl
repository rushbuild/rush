"""Simulates rocm_configure.bzl pattern - also loads cuda_stub.bzl"""

load(":cuda_stub.bzl", "cuda_configure")
load("@bazel_tools//tools/cpp:windows_cc_configure.bzl", "find_msvc_tool")

def rocm_configure():
    """ROCm configuration that depends on CUDA"""
    return find_msvc_tool("rocm") + cuda_configure()

ROCM_CONSTANT = "rocm_value"
