def tsl_library(name, srcs, hdrs, deps = []):
    """TSL library macro"""
    native.cc_library(
        name = name,
        srcs = srcs,
        hdrs = hdrs,
        deps = deps,
    )
