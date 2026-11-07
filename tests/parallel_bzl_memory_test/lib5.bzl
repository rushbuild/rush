# Library 5 - simulates typical TensorFlow .bzl file
def lib5_rule(name, srcs = [], deps = []):
    """Rule from library 5"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib5_macro(name):
    """Macro from library 5"""
    lib5_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB5_CONST1 = "constant_value_1"
LIB5_CONST2 = "constant_value_2"
LIB5_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB5_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
