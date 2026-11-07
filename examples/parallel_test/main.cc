// SPDX-License-Identifier: MIT
// Copyright (C) 2025 Luis Chamberlain <mcgrof@kernel.org>

#include "math_lib.h"
#include <iostream>

int main() {
    int x = 10;
    int y = 5;

    std::cout << "Testing math library operations:" << std::endl;
    std::cout << x << " + " << y << " = " << add(x, y) << std::endl;
    std::cout << x << " - " << y << " = " << subtract(x, y) << std::endl;
    std::cout << x << " * " << y << " = " << multiply(x, y) << std::endl;
    std::cout << x << " / " << y << " = " << divide(x, y) << std::endl;

    return 0;
}
