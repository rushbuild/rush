#include <iostream>
#include "vendor_utils.h"

int main() {
    std::cout << "Vendor library version: " << vendor::get_version() << std::endl;
    return 0;
}
