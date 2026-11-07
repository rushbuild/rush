"""Test any() and all() built-ins"""

# Test any()
result1 = any([False, True, False])  # Should be True
result2 = any([False, False])  # Should be False
result3 = any([])  # Should be False
result4 = any([0, 1, 2])  # Should be True

# Test all()
result5 = all([True, True, True])  # Should be True
result6 = all([True, False])  # Should be False
result7 = all([])  # Should be True
result8 = all([1, 2, 3])  # Should be True

# Export results
TEST_RESULTS = {
    "any_mixed": result1,
    "any_all_false": result2,
    "any_empty": result3,
    "any_numbers": result4,
    "all_true": result5,
    "all_mixed": result6,
    "all_empty": result7,
    "all_numbers": result8,
}
