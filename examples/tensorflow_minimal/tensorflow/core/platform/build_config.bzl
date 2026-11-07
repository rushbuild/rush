# Stub .bzl file for TensorFlow build configuration macros

def tf_proto_library(
    name,
    srcs = [],
    deps = [],
    visibility = None,
    testonly = 0,
    cc_api_version = 2,
    make_default_target_header_only = False,
    protodeps = [],
    exports = [],
    **kwargs):
    """Stub implementation of tf_proto_library.

    For now, this creates a header-only cc_library as a placeholder.
    A full implementation would generate .pb.h/.pb.cc files from .proto files.
    """
    # Create a stub cc_library that depends on the proto dependencies
    native.cc_library(
        name = name,
        hdrs = [],  # Would normally be generated from srcs
        deps = deps + protodeps,
        visibility = visibility,
        testonly = testonly,
    )
