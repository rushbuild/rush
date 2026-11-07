# Library 20 - simulates typical TensorFlow .bzl file
def lib20_rule(name, srcs = [], deps = []):
    """Rule from library 20"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib20_macro(name):
    """Macro from library 20"""
    lib20_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB20_CONST1 = "constant_value_1"
LIB20_CONST2 = "constant_value_2"
LIB20_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB20_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
