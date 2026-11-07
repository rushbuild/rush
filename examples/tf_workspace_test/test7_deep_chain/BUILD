# Test deep .bzl chain - A → B → C → D
# This tests if deep dependency chains cause hang

load("//:a.bzl", "func_a")

genrule(
    name = "main",
    outs = ["output.txt"],
    cmd = "echo 'test7: deep chain' > $@",
)
