load(":heavy_load.bzl", "heavy_function")

# Test loading under resource pressure
genrule(
    name = "test_resource_pressure",
    outs = ["output.txt"],
    cmd = "echo 'heavy load complete' > $@",
)
