#include <iostream>
#include "greeter.h"
#include "math_utils.h"
#include "string_utils.h"

int main() {
    Greeter greeter;
    std::cout << greeter.greet("Resource Manager") << std::endl;

    std::cout << "Math: 5 + 3 = " << MathUtils::add(5, 3) << std::endl;
    std::cout << "Math: 4 * 7 = " << MathUtils::multiply(4, 7) << std::endl;

    std::cout << "Uppercase: " << StringUtils::uppercase("rush build system") << std::endl;
    std::cout << "Lowercase: " << StringUtils::lowercase("DYNAMIC MEMORY TRACKING") << std::endl;

    std::cout << "\n✓ Resource manager test successful!" << std::endl;
    return 0;
}
