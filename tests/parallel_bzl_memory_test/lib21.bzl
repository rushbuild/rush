# Library 21 - simulates typical TensorFlow .bzl file
def lib21_rule(name, srcs = [], deps = []):
    """Rule from library 21"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib21_macro(name):
    """Macro from library 21"""
    lib21_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB21_CONST1 = "constant_value_1"
LIB21_CONST2 = "constant_value_2"
LIB21_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB21_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
