// Test demonstrating cross-package dependencies in tests
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

#include <iostream>
#include <cassert>
#include "math_utils.h"

void test_math_utils_add() {
    assert(math_utils::add(5, 7) == 12);
    assert(math_utils::add(0, 0) == 0);
    assert(math_utils::add(-3, 3) == 0);
    std::cout << "✓ test_math_utils_add passed\n";
}

void test_math_utils_multiply() {
    assert(math_utils::multiply(3, 4) == 12);
    assert(math_utils::multiply(0, 10) == 0);
    assert(math_utils::multiply(-2, 5) == -10);
    std::cout << "✓ test_math_utils_multiply passed\n";
}

void test_math_utils_factorial() {
    assert(math_utils::factorial(0) == 1);
    assert(math_utils::factorial(1) == 1);
    assert(math_utils::factorial(5) == 120);
    assert(math_utils::factorial(6) == 720);
    std::cout << "✓ test_math_utils_factorial passed\n";
}

int main() {
    std::cout << "Running math_test (cross-package dependency test)...\n";

    test_math_utils_add();
    test_math_utils_multiply();
    test_math_utils_factorial();

    std::cout << "\nAll math_utils tests passed!\n";
    return 0;
}
