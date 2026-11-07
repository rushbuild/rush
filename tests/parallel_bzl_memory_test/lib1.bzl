# Library 1 - simulates typical TensorFlow .bzl file
def lib1_rule(name, srcs = [], deps = []):
    """Rule from library 1"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib1_macro(name):
    """Macro from library 1"""
    lib1_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB1_CONST1 = "constant_value_1"
LIB1_CONST2 = "constant_value_2"
LIB1_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB1_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
