# Library 39 - simulates typical TensorFlow .bzl file
def lib39_rule(name, srcs = [], deps = []):
    """Rule from library 39"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib39_macro(name):
    """Macro from library 39"""
    lib39_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB39_CONST1 = "constant_value_1"
LIB39_CONST2 = "constant_value_2"
LIB39_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB39_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
