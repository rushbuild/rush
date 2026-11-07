# Library 19 - simulates typical TensorFlow .bzl file
def lib19_rule(name, srcs = [], deps = []):
    """Rule from library 19"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib19_macro(name):
    """Macro from library 19"""
    lib19_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB19_CONST1 = "constant_value_1"
LIB19_CONST2 = "constant_value_2"
LIB19_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB19_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
