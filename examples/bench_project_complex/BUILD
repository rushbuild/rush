# Complex benchmark project with 100 C++ files and deep dependency trees
# Copyright © 2025 Luis Chamberlain. All rights reserved.
# SPDX-License-Identifier: MIT

# This project has deep dependency chains (8 levels)
# to stress-test the dependency graph analyzer

cc_library(
    name = "all_libs",
    srcs = glob(["lib_*.cc"]),
    hdrs = glob(["lib_*.h"]),
    visibility = ["//visibility:public"],
)

cc_binary(
    name = "bench_complex",
    srcs = ["main.cc"],
    deps = [":all_libs"],
)
