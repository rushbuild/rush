"""Test 3: Circular dependency - B loads A."""

load(":circular_a.bzl", "a_function")

def b_value():
    """This creates a circular dependency."""
    return ["b"] + a_function()

B_VALUE = "b"
