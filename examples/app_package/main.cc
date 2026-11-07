// Application using cross-package dependency
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

#include <iostream>
#include "math_utils.h"

int main() {
    std::cout << "Cross-Package Dependency Test\n";
    std::cout << "==============================\n\n";

    int a = 15, b = 7;

    std::cout << "Using math_utils from //examples/lib_package\n\n";
    std::cout << a << " + " << b << " = " << math_utils::add(a, b) << "\n";
    std::cout << a << " * " << b << " = " << math_utils::multiply(a, b) << "\n";
    std::cout << "factorial(5) = " << math_utils::factorial(5) << "\n";

    std::cout << "\n✅ Cross-package dependency working!\n";

    return 0;
}
