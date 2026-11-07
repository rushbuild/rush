# Test .bzl nesting - level1.bzl loads level2.bzl
# This mimics TensorFlow's pattern where .bzl files load other .bzl files

load("//:level1.bzl", "func1")

genrule(
    name = "main",
    outs = ["output.txt"],
    cmd = "echo 'test6: nested bzl loading' > $@",
)
