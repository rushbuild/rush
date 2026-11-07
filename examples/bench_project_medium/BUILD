# Medium benchmark project with 50 C++ files
# Copyright © 2025 Luis Chamberlain. All rights reserved.
# SPDX-License-Identifier: MIT

cc_library(
    name = "all_libs",
    srcs = glob(["lib_*.cc"]),
    hdrs = glob(["lib_*.h"]),
    visibility = ["//visibility:public"],
)

cc_binary(
    name = "bench_medium",
    srcs = ["main.cc"],
    deps = [":all_libs"],
)
