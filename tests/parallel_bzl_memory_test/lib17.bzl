# Library 17 - simulates typical TensorFlow .bzl file
def lib17_rule(name, srcs = [], deps = []):
    """Rule from library 17"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib17_macro(name):
    """Macro from library 17"""
    lib17_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB17_CONST1 = "constant_value_1"
LIB17_CONST2 = "constant_value_2"
LIB17_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB17_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
