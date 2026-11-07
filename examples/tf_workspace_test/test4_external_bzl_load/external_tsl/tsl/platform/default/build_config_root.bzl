"""Build configuration root - mimics TensorFlow's @local_tsl pattern"""

def tsl_build_config():
    """Build configuration for TSL (TensorFlow Support Library)"""
    return {
        "version": "2.16.0",
        "platform": "default",
    }

def if_tsl_platform_default(if_true, if_false = []):
    """Platform selection macro"""
    return if_true
