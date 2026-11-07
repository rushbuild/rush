# Library 33 - simulates typical TensorFlow .bzl file
def lib33_rule(name, srcs = [], deps = []):
    """Rule from library 33"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib33_macro(name):
    """Macro from library 33"""
    lib33_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB33_CONST1 = "constant_value_1"
LIB33_CONST2 = "constant_value_2"
LIB33_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB33_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
