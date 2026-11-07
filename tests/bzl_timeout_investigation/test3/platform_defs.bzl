"""Test 3: Simulates //tensorflow/core/platform:build_config_root.bzl"""

# Leaf .bzl file - no loads

def if_dynamic_kernels(if_true, if_false = []):
    """Stub implementation."""
    return if_false

def if_static(if_true, if_false = []):
    """Stub implementation."""
    return if_true

def tf_exec_properties(kwargs):
    """Stub implementation."""
    return kwargs

def tf_gpu_tests_tags():
    """Stub implementation."""
    return ["gpu"]
