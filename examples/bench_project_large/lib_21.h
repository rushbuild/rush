// Auto-generated header 21
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

#ifndef LIB_21_H
#define LIB_21_H

#include "lib_20.h"
#include "lib_19.h"
#include "lib_18.h"
#include "lib_17.h"

#include <vector>
#include <string>
#include <memory>

namespace lib_21 {

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

} // namespace lib_21

#endif // LIB_21_H
