# CRITICAL TEST: This mimics TensorFlow's exact pattern
# Load from missing external repo, then continue with local .bzl
# TensorFlow does this with @local_config_cuda, @local_config_rocm, etc.

# Missing external repo - should warn but not hang
load("@missing_cuda//cuda:build_defs.bzl", "if_cuda")

# Then load from local .bzl - this should still work
load("//:local.bzl", "local_func")

# Another missing repo
load("@missing_rocm//rocm:build_defs.bzl", "if_rocm")

# Another local load
load("//:utils.bzl", "util_func")

genrule(
    name = "main",
    outs = ["output.txt"],
    cmd = "echo 'test9: missing repo then local' > $@",
)
