// Simple test demonstrating Rush test execution
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

#include <iostream>
#include <cassert>

int add(int a, int b) {
    return a + b;
}

int multiply(int a, int b) {
    return a * b;
}

void test_add() {
    assert(add(2, 3) == 5);
    assert(add(-1, 1) == 0);
    assert(add(0, 0) == 0);
    std::cout << "✓ test_add passed\n";
}

void test_multiply() {
    assert(multiply(2, 3) == 6);
    assert(multiply(-2, 3) == -6);
    assert(multiply(0, 5) == 0);
    std::cout << "✓ test_multiply passed\n";
}

int main() {
    std::cout << "Running simple_test...\n";

    test_add();
    test_multiply();

    std::cout << "\nAll tests passed!\n";
    return 0;
}
