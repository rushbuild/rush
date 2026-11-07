# Library 46 - simulates typical TensorFlow .bzl file
def lib46_rule(name, srcs = [], deps = []):
    """Rule from library 46"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib46_macro(name):
    """Macro from library 46"""
    lib46_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB46_CONST1 = "constant_value_1"
LIB46_CONST2 = "constant_value_2"
LIB46_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB46_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
