load(":file_2.bzl", "file_2_function", "FILE_2_VALUE")

def file_3_function():
    return "file_2_function() + file_3"

FILE_3_VALUE = "FILE_2_VALUE + _file_3"
