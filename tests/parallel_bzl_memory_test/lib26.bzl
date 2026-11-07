# Library 26 - simulates typical TensorFlow .bzl file
def lib26_rule(name, srcs = [], deps = []):
    """Rule from library 26"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib26_macro(name):
    """Macro from library 26"""
    lib26_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB26_CONST1 = "constant_value_1"
LIB26_CONST2 = "constant_value_2"
LIB26_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB26_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
