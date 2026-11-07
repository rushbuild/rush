# Simple hello world test for resource manager

cc_library(
    name = "greeter",
    srcs = ["greeter.cc"],
    hdrs = ["greeter.h"],
)

cc_library(
    name = "math_utils",
    srcs = ["math_utils.cc"],
    hdrs = ["math_utils.h"],
)

cc_library(
    name = "string_utils",
    srcs = ["string_utils.cc"],
    hdrs = ["string_utils.h"],
)

cc_binary(
    name = "hello",
    srcs = ["main.cc"],
    deps = [
        ":greeter",
        ":math_utils",
        ":string_utils",
    ],
)
