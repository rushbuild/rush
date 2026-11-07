#include <cassert>
#include <iostream>
#include <vector>
#include <algorithm>
#include <numeric>

void test_vector_creation() {
    std::cout << "Testing vector creation..." << std::endl;
    std::vector<int> empty;
    assert(empty.size() == 0);
    assert(empty.empty());

    std::vector<int> numbers = {1, 2, 3, 4, 5};
    assert(numbers.size() == 5);
    assert(!numbers.empty());
    std::cout << "✓ test_vector_creation passed" << std::endl;
}

void test_vector_access() {
    std::cout << "Testing vector element access..." << std::endl;
    std::vector<int> numbers = {10, 20, 30, 40, 50};

    assert(numbers[0] == 10);
    assert(numbers[2] == 30);
    assert(numbers[4] == 50);
    assert(numbers.front() == 10);
    assert(numbers.back() == 50);
    std::cout << "✓ test_vector_access passed" << std::endl;
}

void test_vector_push_pop() {
    std::cout << "Testing vector push and pop..." << std::endl;
    std::vector<int> numbers;

    numbers.push_back(1);
    assert(numbers.size() == 1);
    assert(numbers[0] == 1);

    numbers.push_back(2);
    numbers.push_back(3);
    assert(numbers.size() == 3);

    numbers.pop_back();
    assert(numbers.size() == 2);
    assert(numbers.back() == 2);
    std::cout << "✓ test_vector_push_pop passed" << std::endl;
}

void test_vector_iteration() {
    std::cout << "Testing vector iteration..." << std::endl;
    std::vector<int> numbers = {1, 2, 3, 4, 5};

    int sum = 0;
    for (int n : numbers) {
        sum += n;
    }
    assert(sum == 15);

    // Test with iterator
    int count = 0;
    for (auto it = numbers.begin(); it != numbers.end(); ++it) {
        count++;
    }
    assert(count == 5);
    std::cout << "✓ test_vector_iteration passed" << std::endl;
}

void test_vector_algorithms() {
    std::cout << "Testing vector with STL algorithms..." << std::endl;
    std::vector<int> numbers = {5, 2, 8, 1, 9, 3};

    // Find element
    auto it = std::find(numbers.begin(), numbers.end(), 8);
    assert(it != numbers.end());
    assert(*it == 8);

    // Sort
    std::sort(numbers.begin(), numbers.end());
    assert(numbers[0] == 1);
    assert(numbers[5] == 9);

    // Accumulate (sum)
    int sum = std::accumulate(numbers.begin(), numbers.end(), 0);
    assert(sum == 28);  // 1+2+3+5+8+9
    std::cout << "✓ test_vector_algorithms passed" << std::endl;
}

void test_vector_resize() {
    std::cout << "Testing vector resize and clear..." << std::endl;
    std::vector<int> numbers = {1, 2, 3};

    numbers.resize(5);
    assert(numbers.size() == 5);
    assert(numbers[3] == 0);  // Default initialized

    numbers.resize(2);
    assert(numbers.size() == 2);

    numbers.clear();
    assert(numbers.empty());
    assert(numbers.size() == 0);
    std::cout << "✓ test_vector_resize passed" << std::endl;
}

void test_vector_comparison() {
    std::cout << "Testing vector comparison..." << std::endl;
    std::vector<int> v1 = {1, 2, 3};
    std::vector<int> v2 = {1, 2, 3};
    std::vector<int> v3 = {1, 2, 4};

    assert(v1 == v2);
    assert(v1 != v3);
    assert(v1 < v3);
    std::cout << "✓ test_vector_comparison passed" << std::endl;
}

void test_multidimensional_vector() {
    std::cout << "Testing multidimensional vector..." << std::endl;
    std::vector<std::vector<int>> matrix = {
        {1, 2, 3},
        {4, 5, 6},
        {7, 8, 9}
    };

    assert(matrix.size() == 3);
    assert(matrix[0].size() == 3);
    assert(matrix[1][1] == 5);
    assert(matrix[2][2] == 9);
    std::cout << "✓ test_multidimensional_vector passed" << std::endl;
}

int main() {
    std::cout << "=== Running Container Tests ===" << std::endl;

    test_vector_creation();
    test_vector_access();
    test_vector_push_pop();
    test_vector_iteration();
    test_vector_algorithms();
    test_vector_resize();
    test_vector_comparison();
    test_multidimensional_vector();

    std::cout << "\n=== All Container Tests Passed (8/8) ===" << std::endl;
    return 0;
}
