load(":base.bzl", "base_function", "BASE_VALUE")

def file_1_function():
    return "base_function() + file_1"

FILE_1_VALUE = "BASE_VALUE + _file_1"
