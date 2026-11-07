// Auto-generated header 90
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

#ifndef LIB_90_H
#define LIB_90_H

#include "lib_89.h"
#include "lib_88.h"
#include "lib_87.h"
#include "lib_86.h"

#include <vector>
#include <string>
#include <memory>

namespace lib_90 {

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

} // namespace lib_90

#endif // LIB_90_H
