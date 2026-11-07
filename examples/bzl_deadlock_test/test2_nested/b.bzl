"""Test 2: Middle layer - B loads C."""

load(":c.bzl", "c_function")

def b_function():
    """Function that uses c_function."""
    return c_function() + ["b"]

B_CONSTANT = "b"
