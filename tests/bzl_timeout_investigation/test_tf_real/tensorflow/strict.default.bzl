"""Real TensorFlow strict.default.bzl - loads massive tensorflow.bzl"""

load("//tensorflow:tensorflow.bzl", _py_test = "py_test")

def py_strict_binary(name, **kwargs):
    native.py_binary(name = name, **kwargs)

def py_strict_library(name, **kwargs):
    native.py_library(name = name, **kwargs)

def py_strict_test(name, **kwargs):
    _py_test(name = name, **kwargs)
