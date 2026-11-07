// Auto-generated header 91
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

#ifndef LIB_91_H
#define LIB_91_H

#include "lib_90.h"
#include "lib_89.h"
#include "lib_88.h"
#include "lib_87.h"
#include "lib_86.h"
#include "lib_85.h"
#include "lib_84.h"
#include "lib_83.h"

#include <vector>
#include <string>
#include <memory>

namespace lib_91 {

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

} // namespace lib_91

#endif // LIB_91_H
