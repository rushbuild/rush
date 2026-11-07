"""Simulates tensorflow/core/platform:rules_cc.bzl"""

def cc_binary(**kwargs):
    native.cc_binary(**kwargs)

def cc_library(**kwargs):
    native.cc_library(**kwargs)

def cc_test(**kwargs):
    native.cc_test(**kwargs)
