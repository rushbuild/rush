# Test loading TensorFlow's ACTUAL tensorflow.bzl file (first 20 lines only)
# This will test the real pattern that causes the hang

load("//tensorflow/core/platform:build_config_root.bzl", "if_static")
load("@local_config_cuda//cuda:build_defs.bzl", "if_cuda")

genrule(
    name = "main",
    outs = ["output.txt"],
    cmd = "echo 'test11: real tensorflow pattern' > $@",
)
