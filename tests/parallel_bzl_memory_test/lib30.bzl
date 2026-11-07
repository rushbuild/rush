# Library 30 - simulates typical TensorFlow .bzl file
def lib30_rule(name, srcs = [], deps = []):
    """Rule from library 30"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib30_macro(name):
    """Macro from library 30"""
    lib30_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB30_CONST1 = "constant_value_1"
LIB30_CONST2 = "constant_value_2"
LIB30_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB30_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
