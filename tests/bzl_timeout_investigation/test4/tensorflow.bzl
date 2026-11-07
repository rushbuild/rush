"""Test 4: tensorflow.bzl with MULTIPLE load() statements."""

# Load 1: Platform configuration
load(
    ":platform_defs.bzl",
    "if_static",
    "tf_exec_properties",
)

# Load 2: C++ rules
load(
    ":rules_cc.bzl",
    "cc_binary",
    "cc_library",
    "cc_test",
)

# Load 3: CUDA configuration
load(
    ":cuda_defs.bzl",
    "if_cuda",
    "cuda_library",
)

def py_test(name, **kwargs):
    """Simple wrapper."""
    native.py_test(name = name, **kwargs)

def tf_cc_library(name, **kwargs):
    """Uses multiple loaded symbols."""
    deps = kwargs.pop("deps", [])
    deps = deps + if_static(["//base:static"], [])
    deps = deps + if_cuda(["//cuda:runtime"], [])
    cc_library(name = name, deps = deps, **kwargs)
