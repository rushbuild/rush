"""Helpers .bzl that also tries to load from missing repo"""

# This mimics nested .bzl files trying to load from external repos
load("@missing_xla//xla:xla.bzl", "xla_config")

def helper_a():
    """Helper A"""
    pass

def helper_b():
    """Helper B"""
    pass
