"""Test 5: TensorFlow-style pattern with select() and Label()."""

def if_dynamic_kernels(extra_deps, otherwise = []):
    """Mimics TensorFlow's if_dynamic_kernels."""
    # In real TensorFlow this uses select() and Label()
    # For now, just return otherwise since we don't have those implemented
    return otherwise

def tf_additional_plugin_deps():
    """Mimics TensorFlow's tf_additional_plugin_deps."""
    # Would use select() in real TensorFlow
    return []

TF_CONSTANT = "tensorflow_style"
