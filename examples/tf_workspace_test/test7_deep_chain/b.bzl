"""Level B - loads C"""
load("//:c.bzl", "func_c")

def func_b():
    """Function B that uses C"""
    func_c()
