# Library 12 - simulates typical TensorFlow .bzl file
def lib12_rule(name, srcs = [], deps = []):
    """Rule from library 12"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib12_macro(name):
    """Macro from library 12"""
    lib12_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB12_CONST1 = "constant_value_1"
LIB12_CONST2 = "constant_value_2"
LIB12_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB12_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
