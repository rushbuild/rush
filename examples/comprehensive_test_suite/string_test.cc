#include <cassert>
#include <iostream>
#include <string>
#include <algorithm>

void test_string_concatenation() {
    std::cout << "Testing string concatenation..." << std::endl;
    std::string hello = "Hello";
    std::string world = "World";
    std::string result = hello + " " + world;
    assert(result == "Hello World");

    std::string empty = "";
    assert(empty + "test" == "test");
    assert("test" + empty == "test");
    std::cout << "✓ test_string_concatenation passed" << std::endl;
}

void test_string_length() {
    std::cout << "Testing string length..." << std::endl;
    std::string empty = "";
    assert(empty.length() == 0);

    std::string hello = "Hello";
    assert(hello.length() == 5);

    std::string spaces = "   ";
    assert(spaces.length() == 3);
    std::cout << "✓ test_string_length passed" << std::endl;
}

void test_string_comparison() {
    std::cout << "Testing string comparison..." << std::endl;
    std::string str1 = "apple";
    std::string str2 = "banana";
    std::string str3 = "apple";

    assert(str1 == str3);
    assert(str1 != str2);
    assert(str1 < str2);
    assert(str2 > str1);
    std::cout << "✓ test_string_comparison passed" << std::endl;
}

void test_string_substring() {
    std::cout << "Testing string substring..." << std::endl;
    std::string text = "Hello World";

    assert(text.substr(0, 5) == "Hello");
    assert(text.substr(6, 5) == "World");
    assert(text.substr(0, 11) == "Hello World");
    assert(text.substr(6) == "World");
    std::cout << "✓ test_string_substring passed" << std::endl;
}

void test_string_find() {
    std::cout << "Testing string find..." << std::endl;
    std::string text = "The quick brown fox";

    assert(text.find("quick") == 4);
    assert(text.find("fox") == 16);
    assert(text.find("The") == 0);
    assert(text.find("missing") == std::string::npos);
    std::cout << "✓ test_string_find passed" << std::endl;
}

void test_string_manipulation() {
    std::cout << "Testing string manipulation..." << std::endl;
    std::string text = "hello";

    // Convert to uppercase
    std::string upper = text;
    std::transform(upper.begin(), upper.end(), upper.begin(), ::toupper);
    assert(upper == "HELLO");

    // Replace
    std::string original = "Hello World";
    original.replace(6, 5, "Rush");
    assert(original == "Hello Rush");

    std::cout << "✓ test_string_manipulation passed" << std::endl;
}

void test_string_empty() {
    std::cout << "Testing string empty checks..." << std::endl;
    std::string empty = "";
    std::string nonempty = "test";

    assert(empty.empty());
    assert(!nonempty.empty());
    assert(empty.length() == 0);
    std::cout << "✓ test_string_empty passed" << std::endl;
}

int main() {
    std::cout << "=== Running String Manipulation Tests ===" << std::endl;

    test_string_concatenation();
    test_string_length();
    test_string_comparison();
    test_string_substring();
    test_string_find();
    test_string_manipulation();
    test_string_empty();

    std::cout << "\n=== All String Tests Passed (7/7) ===" << std::endl;
    return 0;
}
