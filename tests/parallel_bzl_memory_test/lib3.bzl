# Library 3 - simulates typical TensorFlow .bzl file
def lib3_rule(name, srcs = [], deps = []):
    """Rule from library 3"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib3_macro(name):
    """Macro from library 3"""
    lib3_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB3_CONST1 = "constant_value_1"
LIB3_CONST2 = "constant_value_2"
LIB3_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB3_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
