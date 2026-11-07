"""Simplified tensorflow.bzl with real load structure that triggers timeout"""

# These 14 loads at the top trigger nested/parallel loading that causes timeout
load(
    "//tensorflow/core/platform:build_config_root.bzl",
    "if_static",
    "tf_gpu_tests_tags",
)
load(
    "//tensorflow/core/platform:rules_cc.bzl",
    "cc_binary",
    "cc_library",
)

# Stub py_test function
def py_test(name, **kwargs):
    native.py_test(name = name, **kwargs)
