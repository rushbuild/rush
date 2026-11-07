# Library 38 - simulates typical TensorFlow .bzl file
def lib38_rule(name, srcs = [], deps = []):
    """Rule from library 38"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib38_macro(name):
    """Macro from library 38"""
    lib38_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB38_CONST1 = "constant_value_1"
LIB38_CONST2 = "constant_value_2"
LIB38_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB38_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
