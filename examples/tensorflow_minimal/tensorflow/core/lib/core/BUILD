# Test package() and filegroup with load()

load("//tensorflow:tensorflow.default.bzl", "filegroup")
load("//tensorflow/core/platform:rules_cc.bzl", "cc_library")

package(
    default_visibility = ["//tensorflow/core:__subpackages__"],
    licenses = ["notice"],
)

cc_library(
    name = "arena",
    srcs = ["arena.cc"],
    hdrs = ["arena.h"],
    deps = [],
)

filegroup(
    name = "mobile_srcs",
    srcs = ["arena.h", "bitmap.h"],
)
