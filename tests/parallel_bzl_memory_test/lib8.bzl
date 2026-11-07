# Library 8 - simulates typical TensorFlow .bzl file
def lib8_rule(name, srcs = [], deps = []):
    """Rule from library 8"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib8_macro(name):
    """Macro from library 8"""
    lib8_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB8_CONST1 = "constant_value_1"
LIB8_CONST2 = "constant_value_2"
LIB8_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB8_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
