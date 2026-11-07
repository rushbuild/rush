# Library 2 - simulates typical TensorFlow .bzl file
def lib2_rule(name, srcs = [], deps = []):
    """Rule from library 2"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib2_macro(name):
    """Macro from library 2"""
    lib2_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB2_CONST1 = "constant_value_1"
LIB2_CONST2 = "constant_value_2"
LIB2_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB2_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
