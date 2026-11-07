# Library 43 - simulates typical TensorFlow .bzl file
def lib43_rule(name, srcs = [], deps = []):
    """Rule from library 43"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib43_macro(name):
    """Macro from library 43"""
    lib43_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB43_CONST1 = "constant_value_1"
LIB43_CONST2 = "constant_value_2"
LIB43_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB43_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
