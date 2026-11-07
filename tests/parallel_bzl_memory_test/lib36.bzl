# Library 36 - simulates typical TensorFlow .bzl file
def lib36_rule(name, srcs = [], deps = []):
    """Rule from library 36"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib36_macro(name):
    """Macro from library 36"""
    lib36_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB36_CONST1 = "constant_value_1"
LIB36_CONST2 = "constant_value_2"
LIB36_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB36_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
