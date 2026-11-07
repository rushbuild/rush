# Minimal BUILD file mimicking TensorFlow patterns

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
        ":linux": ["linux_platform.cc"],
        ":macos": ["macos_platform.cc"],
        "//conditions:default": ["generic_platform.cc"],
    }),
    hdrs = ["platform.h"],
)

cc_binary(
    name = "tf_mini",
    srcs = ["main.cc"],
    deps = [":platform_lib"],
)
