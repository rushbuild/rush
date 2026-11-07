# Test many parallel .bzl loads - TensorFlow has 8+ loads in core/BUILD
# This tests if parallel loading causes hang

load("//:bzl1.bzl", "func1")
load("//:bzl2.bzl", "func2")
load("//:bzl3.bzl", "func3")
load("//:bzl4.bzl", "func4")
load("//:bzl5.bzl", "func5")
load("//:bzl6.bzl", "func6")
load("//:bzl7.bzl", "func7")
load("//:bzl8.bzl", "func8")

genrule(
    name = "main",
    outs = ["output.txt"],
    cmd = "echo 'test8: many parallel loads' > $@",
)
