# Hardware detection - base level

def hardware_config():
    """Detect hardware capabilities"""
    return {
        "flags": ["HW_DETECTED=1"],
        "has_gpu": has_gpu(),
    }

def has_gpu():
    """Check for GPU support"""
    # In real TensorFlow, this would check for CUDA/ROCm
    return False
