# Library 16 - simulates typical TensorFlow .bzl file
def lib16_rule(name, srcs = [], deps = []):
    """Rule from library 16"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib16_macro(name):
    """Macro from library 16"""
    lib16_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB16_CONST1 = "constant_value_1"
LIB16_CONST2 = "constant_value_2"
LIB16_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB16_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
