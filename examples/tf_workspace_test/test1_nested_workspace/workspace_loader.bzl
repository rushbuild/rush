"""Loads external library with nested WORKSPACE"""

def load_external_libs():
    native.local_repository(
        name = "external_lib",
        path = "external_lib",
    )
