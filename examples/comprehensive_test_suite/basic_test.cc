#include <cassert>
#include <iostream>

void test_addition() {
    std::cout << "Testing basic addition..." << std::endl;
    assert(2 + 2 == 4);
    assert(0 + 0 == 0);
    assert(-5 + 5 == 0);
    assert(100 + 200 == 300);
    std::cout << "✓ test_addition passed" << std::endl;
}

void test_subtraction() {
    std::cout << "Testing basic subtraction..." << std::endl;
    assert(10 - 5 == 5);
    assert(0 - 0 == 0);
    assert(-5 - (-5) == 0);
    assert(100 - 200 == -100);
    std::cout << "✓ test_subtraction passed" << std::endl;
}

void test_multiplication() {
    std::cout << "Testing basic multiplication..." << std::endl;
    assert(3 * 4 == 12);
    assert(0 * 100 == 0);
    assert(-5 * 5 == -25);
    assert(-5 * -5 == 25);
    std::cout << "✓ test_multiplication passed" << std::endl;
}

void test_division() {
    std::cout << "Testing basic division..." << std::endl;
    assert(10 / 2 == 5);
    assert(100 / 4 == 25);
    assert(-10 / 2 == -5);
    assert(7 / 2 == 3);  // Integer division
    std::cout << "✓ test_division passed" << std::endl;
}

void test_boolean_logic() {
    std::cout << "Testing boolean logic..." << std::endl;
    assert(true && true);
    assert(!(false && true));
    assert(true || false);
    assert(!(false && false));
    assert(!false);
    assert(true == true);
    assert(false == false);
    std::cout << "✓ test_boolean_logic passed" << std::endl;
}

void test_comparisons() {
    std::cout << "Testing comparison operators..." << std::endl;
    assert(5 > 3);
    assert(3 < 5);
    assert(5 >= 5);
    assert(5 <= 5);
    assert(5 == 5);
    assert(5 != 4);
    std::cout << "✓ test_comparisons passed" << std::endl;
}

int main() {
    std::cout << "=== Running Basic Arithmetic Tests ===" << std::endl;

    test_addition();
    test_subtraction();
    test_multiplication();
    test_division();
    test_boolean_logic();
    test_comparisons();

    std::cout << "\n=== All Basic Tests Passed (6/6) ===" << std::endl;
    return 0;
}
