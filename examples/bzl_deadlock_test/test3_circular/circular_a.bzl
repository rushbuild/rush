"""Test 3: Circular dependency - A loads B, B loads A."""

load(":circular_b.bzl", "b_value")

def a_function():
    """This should trigger circular load detection."""
    return ["a"] + b_value()

A_VALUE = "a"
