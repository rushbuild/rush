# Library 6 - simulates typical TensorFlow .bzl file
def lib6_rule(name, srcs = [], deps = []):
    """Rule from library 6"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib6_macro(name):
    """Macro from library 6"""
    lib6_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB6_CONST1 = "constant_value_1"
LIB6_CONST2 = "constant_value_2"
LIB6_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB6_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
