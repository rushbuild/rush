# Library 28 - simulates typical TensorFlow .bzl file
def lib28_rule(name, srcs = [], deps = []):
    """Rule from library 28"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib28_macro(name):
    """Macro from library 28"""
    lib28_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB28_CONST1 = "constant_value_1"
LIB28_CONST2 = "constant_value_2"
LIB28_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB28_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
