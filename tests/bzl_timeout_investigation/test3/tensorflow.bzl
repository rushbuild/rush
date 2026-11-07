"""Test 3: tensorflow.bzl with ONE load() statement."""

# Single load from another .bzl file (2 levels deep)
load(
    ":platform_defs.bzl",
    "if_dynamic_kernels",
    "if_static",
    "tf_exec_properties",
    "tf_gpu_tests_tags",
)

def py_test(name, **kwargs):
    """Uses loaded symbols."""
    tags = tf_gpu_tests_tags()
    native.py_test(name = name, tags = tags, **kwargs)

def cc_library_with_platform(name, **kwargs):
    """Uses if_static."""
    deps = kwargs.pop("deps", [])
    deps = deps + if_static(["//base:static_lib"], [])
    native.cc_library(name = name, deps = deps, **kwargs)
