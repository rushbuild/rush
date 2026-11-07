"""TensorFlow-style repository rules"""

def _tf_vendored_impl(repository_ctx):
    """Implementation of tf_vendored repository rule.

    This is a simplified version of TensorFlow's tf_vendored from third_party/repo.bzl:143-160
    Creates a symlink from workspace relpath to external repository.
    """
    parent_path = repository_ctx.path(repository_ctx.attr.parent).dirname

    # Navigate to the relative path (TensorFlow splits on "/" because get_child doesn't allow slashes)
    relpath_parts = repository_ctx.attr.relpath.split("/")
    vendored_path = parent_path
    for part in relpath_parts:
        vendored_path = vendored_path.get_child(part)

    # Create symlink: external repo root → workspace vendored path
    repository_ctx.symlink(vendored_path, ".")

tf_vendored = repository_rule(
    implementation = _tf_vendored_impl,
    attrs = {
        "parent": attr.label(default = "//:WORKSPACE"),
        "relpath": attr.string(),
    },
)
