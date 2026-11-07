"""Level 1 .bzl file that loads level2.bzl"""

load("//:level2.bzl", "func2")

def func1():
    """Function that uses func2"""
    func2()
