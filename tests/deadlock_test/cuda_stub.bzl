"""Simulates cuda_configure.bzl pattern - loads nested files"""

load("@bazel_tools//tools/cpp:lib_cc_configure.bzl", "escape_string")
load(":common.bzl", "common_func")

def cuda_configure():
    """CUDA configuration that depends on bazel_tools"""
    return escape_string("cuda") + common_func()

CUDA_CONSTANT = "cuda_value"
