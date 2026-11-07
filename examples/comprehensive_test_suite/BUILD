# Comprehensive Test Suite
#
# This BUILD file demonstrates a complete test suite with various test scenarios.
# It includes tests for basic arithmetic, string manipulation, containers, edge cases,
# and even an intentionally failing test for error reporting validation.

cc_test(
    name = "basic_test",
    srcs = ["basic_test.cc"],
)

cc_test(
    name = "string_test",
    srcs = ["string_test.cc"],
)

cc_test(
    name = "container_test",
    srcs = ["container_test.cc"],
)

cc_test(
    name = "edge_case_test",
    srcs = ["edge_case_test.cc"],
)

# This test intentionally fails to demonstrate error reporting
# Use this to verify that Rush correctly reports test failures
cc_test(
    name = "failing_test",
    srcs = ["failing_test.cc"],
)
