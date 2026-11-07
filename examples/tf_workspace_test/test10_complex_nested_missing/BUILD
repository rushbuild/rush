# CRITICAL TEST: Full TensorFlow pattern combination
# 1. Load from missing external repo
# 2. Load from local .bzl that itself loads more .bzl files
# 3. Those .bzl files try to load from other missing external repos
# 4. Mixed with multiple parallel loads

# Missing external repo (like @local_config_cuda)
load("@missing_cuda//cuda:build_defs.bzl", "if_cuda")

# Local .bzl that has nested loads (like //tensorflow:tensorflow.bzl)
load("//:config.bzl", "my_config", "my_helper1", "my_helper2")

# Another missing repo
load("@missing_rocm//rocm:build_defs.bzl", "if_rocm")

# More local .bzl files
load("//:platform.bzl", "platform_config")
load("//:utils.bzl", "util_func")

# Another missing repo
load("@missing_tensorrt//:build_defs.bzl", "if_tensorrt")

genrule(
    name = "main",
    outs = ["output.txt"],
    cmd = "echo 'test10: complex nested with missing repos' > $@",
)
