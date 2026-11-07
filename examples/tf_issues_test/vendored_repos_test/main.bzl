# Load from vendored repos, similar to TensorFlow's tensorflow.bzl
load("@local_tsl//tsl:tsl.bzl", "tsl_library")
load("@local_xla//xla:xla.bzl", "xla_library")

def main_library(name, srcs, hdrs):
    """Combines TSL and XLA functionality"""
    native.cc_library(
        name = name,
        srcs = srcs,
        hdrs = hdrs,
        deps = [
            "@local_tsl//:tsl_base",
            "@local_xla//:xla_base",
        ],
    )
