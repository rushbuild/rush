package(default_visibility = ["//visibility:public"])

# THIS IS THE CRITICAL TEST:
# Load .bzl file from @local_tsl external repository
# This is where TensorFlow hangs because @local_tsl isn't registered

load("@local_tsl//tsl/platform/default:build_config_root.bzl", "tsl_build_config", "if_tsl_platform_default")

genrule(
    name = "main",
    outs = ["output.txt"],
    cmd = "echo 'Successfully loaded .bzl from @local_tsl' > $@",
)
