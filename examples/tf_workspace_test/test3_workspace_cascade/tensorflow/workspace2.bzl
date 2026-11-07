"""Workspace2 - Second in cascade (mimics tensorflow/workspace2.bzl)"""

def workspace():
    """Define external repositories at layer 2"""
    native.new_local_repository(
        name = "layer2_repo",
        path = ".",
        build_file_content = """
package(default_visibility = ["//visibility:public"])
exports_files(["workspace2.bzl"])
""",
    )

# Alias for convenience
tf_workspace2 = workspace
