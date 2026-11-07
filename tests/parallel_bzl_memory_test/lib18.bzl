# Library 18 - simulates typical TensorFlow .bzl file
def lib18_rule(name, srcs = [], deps = []):
    """Rule from library 18"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib18_macro(name):
    """Macro from library 18"""
    lib18_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB18_CONST1 = "constant_value_1"
LIB18_CONST2 = "constant_value_2"
LIB18_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB18_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
