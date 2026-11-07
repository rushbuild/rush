"""Simulates @local_config_cuda//cuda:build_defs.bzl"""

def if_cuda(if_true, if_false = []):
    """Stub - no CUDA."""
    return if_false

def cuda_library(**kwargs):
    """Stub - falls back to cc_library."""
    native.cc_library(**kwargs)
