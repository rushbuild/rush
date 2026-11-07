"""First file - loads from @bazel_tools (like cuda_configure.bzl does)"""

load("@bazel_tools//tools/cpp:lib_cc_configure.bzl", "escape_string")

def func1():
    """Function using bazel_tools symbol"""
    return escape_string("test")

CONSTANT1 = "value1"
