"""Second file - also loads from @bazel_tools"""

load("@bazel_tools//tools/cpp:windows_cc_configure.bzl", "find_msvc_tool")

def func2():
    """Function using different bazel_tools symbol"""
    return find_msvc_tool("test")

CONSTANT2 = "value2"
