// Auto-generated header 26
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

#ifndef LIB_26_H
#define LIB_26_H

#include "lib_25.h"
#include "lib_24.h"
#include "lib_23.h"

#include <vector>
#include <string>
#include <memory>

namespace lib_26 {

class Processor {
public:
    Processor();
    ~Processor();

    int compute();
    double process(int x, double y);
    void initialize();
    std::vector<int> transform(const std::vector<int>& input);

private:
    int internal_state_;
};

// Utility functions
int factorial(int n);
double power(double base, int exp);
std::string format_result(int value);

} // namespace lib_26

#endif // LIB_26_H
