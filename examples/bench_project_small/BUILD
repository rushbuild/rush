# Small benchmark project with 10 C++ files
# Copyright © 2025 Luis Chamberlain. All rights reserved.
# SPDX-License-Identifier: MIT

cc_library(
    name = "math_lib",
    srcs = [
        "lib_0.cc",
        "lib_1.cc",
        "lib_2.cc",
    ],
    hdrs = [
        "lib_0.h",
        "lib_1.h",
        "lib_2.h",
    ],
    visibility = ["//visibility:public"],
)

cc_library(
    name = "utils_lib",
    srcs = [
        "lib_3.cc",
        "lib_4.cc",
        "lib_5.cc",
    ],
    hdrs = [
        "lib_3.h",
        "lib_4.h",
        "lib_5.h",
    ],
    deps = [":math_lib"],
    visibility = ["//visibility:public"],
)

cc_library(
    name = "core_lib",
    srcs = [
        "lib_6.cc",
        "lib_7.cc",
        "lib_8.cc",
        "lib_9.cc",
    ],
    hdrs = [
        "lib_6.h",
        "lib_7.h",
        "lib_8.h",
        "lib_9.h",
    ],
    deps = [
        ":math_lib",
        ":utils_lib",
    ],
    visibility = ["//visibility:public"],
)

cc_binary(
    name = "bench_small",
    srcs = ["main.cc"],
    deps = [
        ":math_lib",
        ":utils_lib",
        ":core_lib",
    ],
)
