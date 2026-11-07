load(":strict.default.bzl", "py_strict_library", "py_strict_test")

# Test: load a .bzl file that loads another .bzl file (1 level deep)
py_strict_library(
    name = "test_lib",
    srcs = ["dummy.py"],
)

py_strict_test(
    name = "test_test",
    srcs = ["dummy_test.py"],
)
