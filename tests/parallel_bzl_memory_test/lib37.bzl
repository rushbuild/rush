# Library 37 - simulates typical TensorFlow .bzl file
def lib37_rule(name, srcs = [], deps = []):
    """Rule from library 37"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib37_macro(name):
    """Macro from library 37"""
    lib37_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB37_CONST1 = "constant_value_1"
LIB37_CONST2 = "constant_value_2"
LIB37_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB37_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
