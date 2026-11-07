# Library 9 - simulates typical TensorFlow .bzl file
def lib9_rule(name, srcs = [], deps = []):
    """Rule from library 9"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib9_macro(name):
    """Macro from library 9"""
    lib9_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB9_CONST1 = "constant_value_1"
LIB9_CONST2 = "constant_value_2"
LIB9_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB9_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
