# Library 31 - simulates typical TensorFlow .bzl file
def lib31_rule(name, srcs = [], deps = []):
    """Rule from library 31"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib31_macro(name):
    """Macro from library 31"""
    lib31_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB31_CONST1 = "constant_value_1"
LIB31_CONST2 = "constant_value_2"
LIB31_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB31_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
