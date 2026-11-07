"""Config .bzl that loads from external repos and other .bzl files"""

# This mimics tensorflow.bzl loading from @local_tsl
load("@missing_local_tsl//tsl:tsl.bzl", "tsl_config")

# Load from another local .bzl
load("//:helpers.bzl", "helper_a", "helper_b")

def my_config():
    """Config function"""
    pass

def my_helper1():
    """Helper 1"""
    helper_a()

def my_helper2():
    """Helper 2"""
    helper_b()
