"""Level A - loads B"""
load("//:b.bzl", "func_b")

def func_a():
    """Function A that uses B"""
    func_b()
