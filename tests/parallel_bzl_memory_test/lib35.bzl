# Library 35 - simulates typical TensorFlow .bzl file
def lib35_rule(name, srcs = [], deps = []):
    """Rule from library 35"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib35_macro(name):
    """Macro from library 35"""
    lib35_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB35_CONST1 = "constant_value_1"
LIB35_CONST2 = "constant_value_2"
LIB35_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB35_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
