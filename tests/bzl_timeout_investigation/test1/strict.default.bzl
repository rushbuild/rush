"""Test 1: Minimal - No loads, just function definitions."""

# No load() statement - testing if basic .bzl evaluation works

def py_strict_binary(name, **kwargs):
    native.py_binary(name = name, **kwargs)

def py_strict_library(name, **kwargs):
    native.py_library(name = name, **kwargs)

def py_strict_test(name, **kwargs):
    native.py_test(name = name, **kwargs)
