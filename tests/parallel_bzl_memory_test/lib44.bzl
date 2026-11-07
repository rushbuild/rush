# Library 44 - simulates typical TensorFlow .bzl file
def lib44_rule(name, srcs = [], deps = []):
    """Rule from library 44"""
    native.filegroup(
        name = name,
        srcs = srcs,
    )

def lib44_macro(name):
    """Macro from library 44"""
    lib44_rule(
        name = name + "_generated",
        srcs = [],
    )

# Some constants to make the file larger
LIB44_CONST1 = "constant_value_1"
LIB44_CONST2 = "constant_value_2"
LIB44_LIST = ["item1", "item2", "item3", "item4", "item5"]
LIB44_DICT = {
    "key1": "value1",
    "key2": "value2",
    "key3": "value3",
}
