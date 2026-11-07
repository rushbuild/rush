load(":circular_a.bzl", "a_function")

# This should fail with circular load error
genrule(
    name = "test_circular",
    outs = ["output.txt"],
    cmd = "echo 'should not reach here' > $@",
)
