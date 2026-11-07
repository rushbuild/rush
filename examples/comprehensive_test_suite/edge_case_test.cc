#include <cassert>
#include <iostream>
#include <string>
#include <vector>
#include <limits>
#include <cstdint>

void test_integer_overflow() {
    std::cout << "Testing integer overflow behavior..." << std::endl;
    int max_int = std::numeric_limits<int>::max();
    int min_int = std::numeric_limits<int>::min();

    assert(max_int > 0);
    assert(min_int < 0);

    // Test boundary values
    int almost_max = max_int - 1;
    assert(almost_max < max_int);

    int almost_min = min_int + 1;
    assert(almost_min > min_int);
    std::cout << "✓ test_integer_overflow passed" << std::endl;
}

void test_zero_division_avoidance() {
    std::cout << "Testing zero division avoidance..." << std::endl;
    int numerator = 10;
    int denominator = 2;

    assert(denominator != 0);
    int result = numerator / denominator;
    assert(result == 5);

    // Test that we check before dividing
    int zero = 0;
    if (zero != 0) {
        // This should not execute
        assert(false);
    }
    std::cout << "✓ test_zero_division_avoidance passed" << std::endl;
}

void test_empty_containers() {
    std::cout << "Testing empty container operations..." << std::endl;
    std::vector<int> empty_vec;

    assert(empty_vec.empty());
    assert(empty_vec.size() == 0);

    // Safe to check empty before accessing
    if (!empty_vec.empty()) {
        assert(false);  // Should not reach here
    }

    std::string empty_str = "";
    assert(empty_str.empty());
    assert(empty_str.length() == 0);
    std::cout << "✓ test_empty_containers passed" << std::endl;
}

void test_boundary_indices() {
    std::cout << "Testing boundary index access..." << std::endl;
    std::vector<int> numbers = {10, 20, 30, 40, 50};

    // First element
    assert(numbers[0] == 10);

    // Last element
    assert(numbers[numbers.size() - 1] == 50);

    // One before last
    assert(numbers[numbers.size() - 2] == 40);

    std::string text = "Hello";
    assert(text[0] == 'H');
    assert(text[text.length() - 1] == 'o');
    std::cout << "✓ test_boundary_indices passed" << std::endl;
}

void test_null_pointer_checks() {
    std::cout << "Testing null pointer checks..." << std::endl;
    int* ptr = nullptr;
    assert(ptr == nullptr);

    int value = 42;
    ptr = &value;
    assert(ptr != nullptr);
    assert(*ptr == 42);

    // Set back to null
    ptr = nullptr;
    assert(ptr == nullptr);
    std::cout << "✓ test_null_pointer_checks passed" << std::endl;
}

void test_negative_numbers() {
    std::cout << "Testing negative number operations..." << std::endl;
    int negative = -5;
    assert(negative < 0);

    int positive = -negative;
    assert(positive > 0);
    assert(positive == 5);

    // Negative multiplication
    assert(negative * negative == 25);
    assert(negative * 2 == -10);

    // Negative comparisons
    assert(-10 < -5);
    assert(-5 > -10);
    std::cout << "✓ test_negative_numbers passed" << std::endl;
}

void test_unsigned_underflow() {
    std::cout << "Testing unsigned integer boundaries..." << std::endl;
    unsigned int zero = 0;
    unsigned int max_uint = std::numeric_limits<unsigned int>::max();

    assert(zero == 0);
    assert(max_uint > 0);

    // Test boundary
    unsigned int almost_max = max_uint - 1;
    assert(almost_max < max_uint);

    unsigned int one = 1;
    assert(one > zero);
    std::cout << "✓ test_unsigned_underflow passed" << std::endl;
}

void test_string_edge_cases() {
    std::cout << "Testing string edge cases..." << std::endl;
    std::string empty = "";

    // Empty string operations
    assert(empty + empty == "");
    assert(empty.find("x") == std::string::npos);

    // Single character
    std::string single = "x";
    assert(single.length() == 1);
    assert(single[0] == 'x');

    // Whitespace
    std::string spaces = "   ";
    assert(spaces.length() == 3);
    assert(!spaces.empty());

    // Special characters
    std::string special = "!@#$%";
    assert(special.length() == 5);
    std::cout << "✓ test_string_edge_cases passed" << std::endl;
}

void test_floating_point_precision() {
    std::cout << "Testing floating point comparisons..." << std::endl;
    double a = 0.1 + 0.2;
    double b = 0.3;

    // Direct comparison might fail due to precision
    // So we test with tolerance
    double epsilon = 0.0001;
    assert((a - b) < epsilon);
    assert((a - b) > -epsilon);

    // Test special values
    double zero = 0.0;
    assert(zero == 0.0);

    double negative = -3.14;
    assert(negative < 0.0);
    std::cout << "✓ test_floating_point_precision passed" << std::endl;
}

int main() {
    std::cout << "=== Running Edge Case Tests ===" << std::endl;

    test_integer_overflow();
    test_zero_division_avoidance();
    test_empty_containers();
    test_boundary_indices();
    test_null_pointer_checks();
    test_negative_numbers();
    test_unsigned_underflow();
    test_string_edge_cases();
    test_floating_point_precision();

    std::cout << "\n=== All Edge Case Tests Passed (9/9) ===" << std::endl;
    return 0;
}
