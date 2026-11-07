"""Level C - loads D"""
load("//:d.bzl", "func_d")

def func_c():
    """Function C that uses D"""
    func_d()
