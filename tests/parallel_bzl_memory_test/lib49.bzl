# Library 49 - simulates typical TensorFlow .bzl file
def lib49_rule(name, srcs = [], deps = []):
    """Rule from library 49"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib49_macro(name):
    """Macro from library 49"""
    lib49_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB49_CONST1 = "constant_value_1"
LIB49_CONST2 = "constant_value_2"
LIB49_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB49_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
