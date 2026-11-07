#include <iostream>
#include "external_lib/utils.h"

int main() {
    std::cout << "External lib value: " << external_lib::get_value() << std::endl;
    return 0;
}
