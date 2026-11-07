# Library 4 - simulates typical TensorFlow .bzl file
def lib4_rule(name, srcs = [], deps = []):
    """Rule from library 4"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib4_macro(name):
    """Macro from library 4"""
    lib4_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB4_CONST1 = "constant_value_1"
LIB4_CONST2 = "constant_value_2"
LIB4_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB4_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
