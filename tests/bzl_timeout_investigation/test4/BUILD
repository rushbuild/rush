load(":strict.default.bzl", "py_strict_test")

# Test: Multiple parallel .bzl dependency chains
# BUILD -> strict.default.bzl -> tensorflow.bzl -> {platform_defs.bzl, rules_cc.bzl, cuda_defs.bzl}
py_strict_test(
    name = "multiple_loads_test",
    srcs = ["test.py"],
)
