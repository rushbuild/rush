"""Workspace3 - First in cascade (mimics tensorflow/workspace3.bzl)"""

def workspace():
    """Define external repositories at layer 3"""
    # In real TensorFlow this calls tf_vendored()
    # For this test we just register a simple marker
    native.new_local_repository(
        name = "layer3_repo",
        path = ".",
        build_file_content = """
package(default_visibility = ["//visibility:public"])
exports_files(["workspace3.bzl"])
""",
    )

# Alias for convenience
tf_workspace3 = workspace
