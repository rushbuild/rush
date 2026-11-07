# Minimal stub that simulates nccl_configure.bzl structure

# Simulate the multi-line template strings (lines 43-106)
_NCCL_DUMMY_BUILD_CONTENT = """
filegroup(
  name = "LICENSE",
  visibility = ["//visibility:public"],
)

cc_library(
  name = "nccl",
  visibility = ["//visibility:public"],
)

cc_library(
  name = "nccl_config",
  hdrs = ["nccl_config.h"],
  include_prefix = "third_party/nccl",
  visibility = ["//visibility:public"],
)
"""

_NCCL_ARCHIVE_BUILD_CONTENT = """
filegroup(
  name = "LICENSE",
  data = ["@nccl_archive//:LICENSE.txt"],
  visibility = ["//visibility:public"],
)

alias(
  name = "nccl",
  actual = "@nccl_archive//:nccl",
  visibility = ["//visibility:public"],
)

alias(
  name = "nccl_config",
  actual = "@nccl_archive//:nccl_config",
  visibility = ["//visibility:public"],
)
"""

_NCCL_ARCHIVE_STUB_BUILD_CONTENT = """
filegroup(
  name = "LICENSE",
  data = ["@nccl_archive//:LICENSE.txt"],
  visibility = ["//visibility:public"],
)

alias(
  name = "nccl",
  actual = "@nccl_archive//:nccl_via_stub",
  visibility = ["//visibility:public"],
)

alias(
  name = "nccl_headers",
  actual = "@nccl_archive//:nccl_headers",
  visibility = ["//visibility:public"],
)

alias(
  name = "nccl_config",
  actual = "@nccl_archive//:nccl_config",
  visibility = ["//visibility:public"],
)
"""

# Simple functions (10 total in real file)
def label(file):
    return "//third_party/nccl:{}".format(file)

def enable_cuda():
    return False

def get_cpu_value():
    return "Linux"

# Stub repository rule
def _nccl_autoconf_impl(ctx):
    ctx.file("BUILD", _NCCL_DUMMY_BUILD_CONTENT)
    ctx.file("nccl_config.h", "#define TF_NCCL_VERSION \"\"")

nccl_configure = repository_rule(
    implementation = _nccl_autoconf_impl,
    attrs = {},
)
