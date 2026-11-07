"""Test 5: Force consecutive loads with minimal content to ensure parallel trigger."""

load(":a.bzl", "func_a")
load(":b.bzl", "func_b")
load(":c.bzl", "func_c")
load(":d.bzl", "func_d")
load(":e.bzl", "func_e")

# Only after loads do we have other statements

def combined():
    return func_a() + func_b() + func_c() + func_d() + func_e()
