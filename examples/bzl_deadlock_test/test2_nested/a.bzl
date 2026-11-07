"""Test 2: Nested .bzl loading - A loads B loads C."""

load(":b.bzl", "b_function")

def a_function():
    """Function that uses b_function."""
    return b_function() + ["a"]

A_CONSTANT = "a"
