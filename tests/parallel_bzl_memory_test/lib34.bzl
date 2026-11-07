# Library 34 - simulates typical TensorFlow .bzl file
def lib34_rule(name, srcs = [], deps = []):
    """Rule from library 34"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib34_macro(name):
    """Macro from library 34"""
    lib34_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB34_CONST1 = "constant_value_1"
LIB34_CONST2 = "constant_value_2"
LIB34_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB34_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
