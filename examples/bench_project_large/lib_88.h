// Auto-generated header 88
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

#ifndef LIB_88_H
#define LIB_88_H

#include "lib_87.h"
#include "lib_86.h"
#include "lib_85.h"
#include "lib_84.h"

#include <vector>
#include <string>
#include <memory>

namespace lib_88 {

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

} // namespace lib_88

#endif // LIB_88_H
