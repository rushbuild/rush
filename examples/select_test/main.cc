#include "platform.h"
#include <iostream>

int main() {
    std::cout << "Running on: " << get_platform_name() << std::endl;
    return 0;
}
