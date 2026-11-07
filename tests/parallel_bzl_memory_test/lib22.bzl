# Library 22 - simulates typical TensorFlow .bzl file
def lib22_rule(name, srcs = [], deps = []):
    """Rule from library 22"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib22_macro(name):
    """Macro from library 22"""
    lib22_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB22_CONST1 = "constant_value_1"
LIB22_CONST2 = "constant_value_2"
LIB22_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB22_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
