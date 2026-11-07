// Auto-generated header 17
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

#ifndef LIB_17_H
#define LIB_17_H

#include "lib_16.h"
#include "lib_15.h"
#include "lib_14.h"

#include <vector>
#include <string>
#include <memory>

namespace lib_17 {

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

} // namespace lib_17

#endif // LIB_17_H
