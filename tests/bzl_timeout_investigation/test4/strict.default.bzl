"""Test 4: Load from tensorflow.bzl with multiple dependencies."""

load(":tensorflow.bzl", _py_test = "py_test", _tf_cc_library = "tf_cc_library")

def py_strict_test(name, **kwargs):
    _py_test(name = name, **kwargs)

def tf_strict_library(name, **kwargs):
    _tf_cc_library(name = name, **kwargs)
