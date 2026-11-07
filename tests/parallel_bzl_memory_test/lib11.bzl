# Library 11 - simulates typical TensorFlow .bzl file
def lib11_rule(name, srcs = [], deps = []):
    """Rule from library 11"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib11_macro(name):
    """Macro from library 11"""
    lib11_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB11_CONST1 = "constant_value_1"
LIB11_CONST2 = "constant_value_2"
LIB11_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB11_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
