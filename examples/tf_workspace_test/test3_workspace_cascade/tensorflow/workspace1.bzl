"""Workspace1 - Third in cascade (mimics tensorflow/workspace1.bzl)"""

def workspace():
    """Define external repositories at layer 1"""
    native.new_local_repository(
        name = "layer1_repo",
        path = ".",
        build_file_content = """
package(default_visibility = ["//visibility:public"])
exports_files(["workspace1.bzl"])
""",
    )

# Alias for convenience
tf_workspace1 = workspace
