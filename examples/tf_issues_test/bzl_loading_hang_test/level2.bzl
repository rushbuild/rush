# Level 2 - loads level 3 and platform
load(":level3.bzl", "level3_config")
load(":platform.bzl", "platform_check")

def level2_config():
    """Second level config that depends on level 3"""
    base = level3_config()
    platform = platform_check()
    return {
        "defines": base["defines"] + ["LEVEL2=1"],
        "platform": platform,
    }
