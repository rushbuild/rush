load(":file_1.bzl", "file_1_function", "FILE_1_VALUE")

def file_2_function():
    return "file_1_function() + file_2"

FILE_2_VALUE = "FILE_1_VALUE + _file_2"
