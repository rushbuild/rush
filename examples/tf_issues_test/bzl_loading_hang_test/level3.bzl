# Level 3 - loads hardware configs
load(":hardware.bzl", "hardware_config")

def level3_config():
    """Third level config that depends on hardware"""
    hw = hardware_config()
    return {
        "defines": ["LEVEL3=1"] + hw["flags"],
    }
