cc_library(
    name = "math_lib",
    srcs = ["add.cc", "sub.cc", "mul.cc", "div.cc"],
    hdrs = ["math_lib.h"],
)

cc_binary(
    name = "calculator",
    srcs = ["main.cc"],
    deps = [":math_lib"],
)
