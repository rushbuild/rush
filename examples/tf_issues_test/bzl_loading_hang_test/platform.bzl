# Platform detection - loads hardware
load(":hardware.bzl", "has_gpu")

def platform_check():
    """Check platform capabilities"""
    return "gpu" if has_gpu() else "cpu"

def platform_select():
    """Return platform-specific defines"""
    if platform_check() == "gpu":
        return ["GPU_ENABLED=1"]
    return ["CPU_ONLY=1"]
