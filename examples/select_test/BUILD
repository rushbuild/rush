# Test platform-specific select() evaluation

config_setting(
    name = "linux",
    constraint_values = ["@platforms//os:linux"],
)

config_setting(
    name = "macos",
    constraint_values = ["@platforms//os:osx"],
)

cc_library(
    name = "platform_lib",
    srcs = select({
        ":linux": ["linux_impl.cc"],
        ":macos": ["macos_impl.cc"],
        "//conditions:default": ["generic_impl.cc"],
    }),
    hdrs = ["platform.h"],
)

cc_binary(
    name = "platform_test",
    srcs = ["main.cc"],
    deps = [":platform_lib"],
)
