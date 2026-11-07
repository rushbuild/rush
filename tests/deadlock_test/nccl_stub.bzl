"""Simulates nccl_configure.bzl pattern - also loads cuda_stub.bzl"""

load(":cuda_stub.bzl", "cuda_configure", "CUDA_CONSTANT")
load(":common.bzl", "common_func")

def nccl_configure():
    """NCCL configuration that depends on CUDA"""
    return CUDA_CONSTANT + cuda_configure() + common_func()

NCCL_CONSTANT = "nccl_value"
