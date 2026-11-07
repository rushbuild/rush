// Auto-generated header 4
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

#ifndef LIB_4_H
#define LIB_4_H

#include "lib_3.h"
#include "lib_2.h"
#include "lib_1.h"
#include "lib_0.h"

#include <vector>
#include <string>
#include <memory>

namespace lib_4 {

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

} // namespace lib_4

#endif // LIB_4_H
