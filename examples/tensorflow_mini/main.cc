#include <iostream>
#include "platform.h"

int main() {
    std::cout << "TensorFlow Mini - Platform: " << GetPlatformName() << std::endl;
    return 0;
}
