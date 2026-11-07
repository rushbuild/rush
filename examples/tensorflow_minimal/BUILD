# Test TensorFlow-style BUILD with filegroup and alias

package(
    default_visibility = ["//visibility:public"],
    licenses = ["notice"],
)

config_setting(
    name = "linux",
    constraint_values = ["@platforms//os:linux"],
)

config_setting(
    name = "macos",
    constraint_values = ["@platforms//os:osx"],
)

filegroup(
    name = "core_headers",
    srcs = [
        "arena.h",
        "bitmap.h",
    ],
)

cc_library(
    name = "arena",
    srcs = ["arena.cc"],
    hdrs = [":core_headers"],
    deps = [],
)

# Test alias rule - creates a redirect to the arena target
alias(
    name = "arena_alias",
    actual = ":arena",
)

# Test select() with platform-specific compilation
cc_library(
    name = "platform_lib",
    srcs = ["arena.cc"],
    hdrs = select({
        ":linux": [":core_headers"],
        ":macos": [":core_headers"],
        "//conditions:default": [":core_headers"],
    }),
    deps = [],
)
