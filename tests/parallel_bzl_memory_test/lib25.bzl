# Library 25 - simulates typical TensorFlow .bzl file
def lib25_rule(name, srcs = [], deps = []):
    """Rule from library 25"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib25_macro(name):
    """Macro from library 25"""
    lib25_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB25_CONST1 = "constant_value_1"
LIB25_CONST2 = "constant_value_2"
LIB25_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB25_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
