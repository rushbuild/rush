package(default_visibility = ["//visibility:public"])

# Simple target that depends on all cascaded repos
genrule(
    name = "main",
    srcs = [
        "@layer3_repo//:workspace3.bzl",
        "@layer2_repo//:workspace2.bzl",
        "@layer1_repo//:workspace1.bzl",
    ],
    outs = ["output.txt"],
    cmd = "echo 'Workspace cascade successful' > $@",
)
