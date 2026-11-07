# Library 23 - simulates typical TensorFlow .bzl file
def lib23_rule(name, srcs = [], deps = []):
    """Rule from library 23"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib23_macro(name):
    """Macro from library 23"""
    lib23_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB23_CONST1 = "constant_value_1"
LIB23_CONST2 = "constant_value_2"
LIB23_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB23_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
