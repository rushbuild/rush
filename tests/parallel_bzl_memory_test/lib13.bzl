# Library 13 - simulates typical TensorFlow .bzl file
def lib13_rule(name, srcs = [], deps = []):
    """Rule from library 13"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib13_macro(name):
    """Macro from library 13"""
    lib13_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB13_CONST1 = "constant_value_1"
LIB13_CONST2 = "constant_value_2"
LIB13_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB13_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
