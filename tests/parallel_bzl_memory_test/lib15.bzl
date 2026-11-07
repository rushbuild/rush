# Library 15 - simulates typical TensorFlow .bzl file
def lib15_rule(name, srcs = [], deps = []):
    """Rule from library 15"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib15_macro(name):
    """Macro from library 15"""
    lib15_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB15_CONST1 = "constant_value_1"
LIB15_CONST2 = "constant_value_2"
LIB15_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB15_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
