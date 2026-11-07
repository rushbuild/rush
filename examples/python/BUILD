py_library(
    name = "lib",
    srcs = ["lib.py"],
    visibility = ["//visibility:public"],
)

py_binary(
    name = "hello",
    srcs = ["hello.py"],
    deps = [":lib"],
)
