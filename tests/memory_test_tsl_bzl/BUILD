# Atomic test: Load tsl.bzl and trigger its dependencies
# This simulates what happens during TensorFlow build

load("@local_tsl//tsl:tsl.bzl", "tsl_test")

# Minimal target that forces tsl.bzl evaluation
filegroup(
    name = "test",
    srcs = [],
)
