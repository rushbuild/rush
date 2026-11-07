// Auto-generated header 27
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

#ifndef LIB_27_H
#define LIB_27_H

#include "lib_26.h"
#include "lib_25.h"
#include "lib_24.h"

#include <vector>
#include <string>
#include <memory>

namespace lib_27 {

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

} // namespace lib_27

#endif // LIB_27_H
