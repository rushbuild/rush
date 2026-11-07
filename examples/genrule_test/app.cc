#include "version.h"
#include "config.h"

#include <iostream>

void print_info() {
    std::cout << "Version: " << VERSION << std::endl;
    std::cout << "App: " << APP_NAME << std::endl;
    std::cout << "Max connections: " << MAX_CONNECTIONS << std::endl;
}
