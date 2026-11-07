# Library 41 - simulates typical TensorFlow .bzl file
def lib41_rule(name, srcs = [], deps = []):
    """Rule from library 41"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib41_macro(name):
    """Macro from library 41"""
    lib41_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB41_CONST1 = "constant_value_1"
LIB41_CONST2 = "constant_value_2"
LIB41_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB41_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
