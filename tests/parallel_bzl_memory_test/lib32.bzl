# Library 32 - simulates typical TensorFlow .bzl file
def lib32_rule(name, srcs = [], deps = []):
    """Rule from library 32"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib32_macro(name):
    """Macro from library 32"""
    lib32_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB32_CONST1 = "constant_value_1"
LIB32_CONST2 = "constant_value_2"
LIB32_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB32_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
