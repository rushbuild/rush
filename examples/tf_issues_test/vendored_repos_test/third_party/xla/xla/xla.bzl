def xla_library(name, srcs, hdrs, deps = []):
    """XLA library macro"""
    native.cc_library(
        name = name,
        srcs = srcs,
        hdrs = hdrs,
        deps = deps,
    )
