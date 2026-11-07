#include <cassert>
#include <iostream>

void test_will_pass() {
    std::cout << "Testing something that will pass..." << std::endl;
    assert(1 + 1 == 2);
    assert(true);
    std::cout << "✓ test_will_pass passed" << std::endl;
}

void test_will_fail() {
    std::cout << "Testing something that will fail..." << std::endl;
    std::cout << "This test intentionally fails to demonstrate error reporting" << std::endl;

    // This assertion will fail
    assert(2 + 2 == 5);

    // This line will never be reached
    std::cout << "✓ test_will_fail passed" << std::endl;
}

void test_will_not_run() {
    std::cout << "This test should not run because previous test failed" << std::endl;
    assert(true);
    std::cout << "✓ test_will_not_run passed" << std::endl;
}

int main() {
    std::cout << "=== Running Failing Test Suite ===" << std::endl;
    std::cout << "NOTE: This test is designed to fail for error reporting demonstration" << std::endl;
    std::cout << std::endl;

    test_will_pass();
    test_will_fail();  // This will fail and abort
    test_will_not_run();  // This will never execute

    std::cout << "\n=== All Tests Passed (3/3) ===" << std::endl;
    return 0;
}
