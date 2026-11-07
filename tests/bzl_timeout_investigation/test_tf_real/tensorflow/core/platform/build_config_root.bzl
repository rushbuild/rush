"""Stub build_config_root.bzl - creates nested load chain"""

# This creates more nested loads
load("//tensorflow/core/platform:build_config_root.default.bzl", "get_config")

def if_static(x):
    return x

def tf_gpu_tests_tags():
    return []
