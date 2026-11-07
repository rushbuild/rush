load(
    "//tensorflow:tensorflow.bzl",
    "tf_cc_binary",
    "tf_copts",
)

package(
    default_visibility = ["//visibility:public"],
    licenses = ["notice"],
)

tf_cc_binary(
    name = "tf_parity_runner",
    testonly = 1,
    srcs = ["tf_parity_runner.cc"],
    copts = tf_copts(),
    linkstatic = 1,
    deps = [
        "//tensorflow/core:core_cpu",
        "//tensorflow/core:framework",
        "//tensorflow/core:framework_internal",
        "//tensorflow/core:framework_lite",
        "//tensorflow/core:lib",
        "//tensorflow/core:protos_all_cc",
        "//tensorflow/core:tensorflow",
    ],
)
