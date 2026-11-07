# Library 29 - simulates typical TensorFlow .bzl file
def lib29_rule(name, srcs = [], deps = []):
    """Rule from library 29"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib29_macro(name):
    """Macro from library 29"""
    lib29_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB29_CONST1 = "constant_value_1"
LIB29_CONST2 = "constant_value_2"
LIB29_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB29_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
