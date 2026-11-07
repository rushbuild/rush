load(":strict.default.bzl", "py_strict_test")

# Test: 3-level deep .bzl loading chain
# BUILD -> strict.default.bzl -> tensorflow.bzl -> platform_defs.bzl
py_strict_test(
    name = "nested_test",
    srcs = ["test.py"],
)
