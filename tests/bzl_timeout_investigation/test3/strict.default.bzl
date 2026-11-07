"""Test 3: Load from tensorflow.bzl which loads from platform_defs.bzl."""

load(":tensorflow.bzl", _py_test = "py_test")

def py_strict_test(name, **kwargs):
    _py_test(name = name, **kwargs)
