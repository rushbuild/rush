load(":local_file.bzl", "use_external", "LOCAL_CONSTANT")

# Test external repository .bzl loading
genrule(
    name = "test_external_repo",
    outs = ["output.txt"],
    cmd = "echo '" + LOCAL_CONSTANT + "' > $@",
)
