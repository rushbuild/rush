# Library 47 - simulates typical TensorFlow .bzl file
def lib47_rule(name, srcs = [], deps = []):
    """Rule from library 47"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib47_macro(name):
    """Macro from library 47"""
    lib47_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB47_CONST1 = "constant_value_1"
LIB47_CONST2 = "constant_value_2"
LIB47_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB47_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
