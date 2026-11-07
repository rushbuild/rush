# Library 7 - simulates typical TensorFlow .bzl file
def lib7_rule(name, srcs = [], deps = []):
    """Rule from library 7"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib7_macro(name):
    """Macro from library 7"""
    lib7_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB7_CONST1 = "constant_value_1"
LIB7_CONST2 = "constant_value_2"
LIB7_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB7_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
