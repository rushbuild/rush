# Library 42 - simulates typical TensorFlow .bzl file
def lib42_rule(name, srcs = [], deps = []):
    """Rule from library 42"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib42_macro(name):
    """Macro from library 42"""
    lib42_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB42_CONST1 = "constant_value_1"
LIB42_CONST2 = "constant_value_2"
LIB42_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB42_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
