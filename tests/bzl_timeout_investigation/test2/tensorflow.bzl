"""Test 2: Stub tensorflow.bzl with NO loads - just exports a dummy function."""

def py_test(name, **kwargs):
    """Dummy py_test implementation."""
    native.py_test(name = name, **kwargs)
