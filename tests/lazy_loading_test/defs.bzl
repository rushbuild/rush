"""Helper macros for generated libraries."""

def lib_with_defaults(name, srcs, **kwargs):
    """Wrapper around cc_library with default settings."""
    native.cc_library(
        name = name,
        srcs = srcs,
        copts = ["-O2", "-Wall"],
        **kwargs
    )

def test_suite_macro(name, libs):
    """Creates a test suite from a list of libraries."""
    tests = []
    for lib in libs:
        test_name = lib + "_test"
        native.cc_test(
            name = test_name,
            srcs = [lib + "_test.cc"],
            deps = [":" + lib],
        )
        tests.append(":" + test_name)

    native.test_suite(
        name = name,
        tests = tests,
    )
