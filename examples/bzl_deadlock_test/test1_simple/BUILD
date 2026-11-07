load(":simple.bzl", "simple_function", "SIMPLE_CONSTANT")

# Use the loaded symbols to verify it works
genrule(
    name = "test_simple",
    outs = ["output.txt"],
    cmd = "echo '" + SIMPLE_CONSTANT + "' > $@",
)
