# Library 45 - simulates typical TensorFlow .bzl file
def lib45_rule(name, srcs = [], deps = []):
    """Rule from library 45"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib45_macro(name):
    """Macro from library 45"""
    lib45_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB45_CONST1 = "constant_value_1"
LIB45_CONST2 = "constant_value_2"
LIB45_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB45_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
