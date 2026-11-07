# Library 14 - simulates typical TensorFlow .bzl file
def lib14_rule(name, srcs = [], deps = []):
    """Rule from library 14"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib14_macro(name):
    """Macro from library 14"""
    lib14_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB14_CONST1 = "constant_value_1"
LIB14_CONST2 = "constant_value_2"
LIB14_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB14_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
