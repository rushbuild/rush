"""TensorFlow-style tf_vendored repository rule"""

def _tf_vendored_impl(repository_ctx):
    parent_path = repository_ctx.path(repository_ctx.attr.parent).dirname
    relpath_parts = repository_ctx.attr.relpath.split("/")
    vendored_path = parent_path
    for part in relpath_parts:
        vendored_path = vendored_path.get_child(part)
    repository_ctx.symlink(vendored_path, ".")

tf_vendored = repository_rule(
    implementation = _tf_vendored_impl,
    attrs = {
        "parent": attr.label(default = "//:WORKSPACE"),
        "relpath": attr.string(),
    },
)
