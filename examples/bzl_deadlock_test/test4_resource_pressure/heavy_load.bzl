"""Test 4: Resource pressure - Load many .bzl files to stress resource manager."""

# Load a bunch of helpers to create pressure
load(":helper1.bzl", "helper1")
load(":helper2.bzl", "helper2")
load(":helper3.bzl", "helper3")
load(":helper4.bzl", "helper4")
load(":helper5.bzl", "helper5")

def heavy_function():
    """Function that uses all helpers."""
    return [
        helper1(),
        helper2(),
        helper3(),
        helper4(),
        helper5(),
    ]

HEAVY_CONSTANT = "heavy"
