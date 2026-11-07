load(":a.bzl", "a_function", "A_CONSTANT")

# Test nested loading: BUILD → a.bzl → b.bzl → c.bzl
genrule(
    name = "test_nested",
    outs = ["output.txt"],
    cmd = "echo '" + A_CONSTANT + "' > $@",
)
