# Library 40 - simulates typical TensorFlow .bzl file
def lib40_rule(name, srcs = [], deps = []):
    """Rule from library 40"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib40_macro(name):
    """Macro from library 40"""
    lib40_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB40_CONST1 = "constant_value_1"
LIB40_CONST2 = "constant_value_2"
LIB40_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB40_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
