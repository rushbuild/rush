# Library 24 - simulates typical TensorFlow .bzl file
def lib24_rule(name, srcs = [], deps = []):
    """Rule from library 24"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib24_macro(name):
    """Macro from library 24"""
    lib24_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB24_CONST1 = "constant_value_1"
LIB24_CONST2 = "constant_value_2"
LIB24_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB24_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
