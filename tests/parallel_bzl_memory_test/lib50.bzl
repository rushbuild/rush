# Library 50 - simulates typical TensorFlow .bzl file
def lib50_rule(name, srcs = [], deps = []):
    """Rule from library 50"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib50_macro(name):
    """Macro from library 50"""
    lib50_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB50_CONST1 = "constant_value_1"
LIB50_CONST2 = "constant_value_2"
LIB50_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB50_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
