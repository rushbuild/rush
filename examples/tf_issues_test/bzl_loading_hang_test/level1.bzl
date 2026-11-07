# Level 1 - loads level 2
load(":level2.bzl", "level2_config")
load(":platform.bzl", "platform_select")

def level1_library(name, srcs, hdrs):
    """First level macro that loads from level 2"""
    config = level2_config()
    native.cc_library(
        name = name,
        srcs = srcs,
        hdrs = hdrs,
        defines = config["defines"] + platform_select(),
    )
