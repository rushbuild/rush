# Library 27 - simulates typical TensorFlow .bzl file
def lib27_rule(name, srcs = [], deps = []):
    """Rule from library 27"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib27_macro(name):
    """Macro from library 27"""
    lib27_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB27_CONST1 = "constant_value_1"
LIB27_CONST2 = "constant_value_2"
LIB27_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB27_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
