"""Test 6: Load from external repository."""

# This should load from @mock_external_repo
load("@mock_external_repo//helpers:external_helper.bzl", "external_function", "EXTERNAL_CONSTANT")

def use_external():
    """Use external function."""
    return external_function() + ["local"]

LOCAL_CONSTANT = EXTERNAL_CONSTANT + "_local"
