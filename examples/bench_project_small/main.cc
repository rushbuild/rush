// Main benchmark driver
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

#include <iostream>
#include <chrono>
#include "lib_0.h"
#include "lib_1.h"
#include "lib_2.h"
#include "lib_3.h"
#include "lib_4.h"
#include "lib_5.h"
#include "lib_6.h"
#include "lib_7.h"
#include "lib_8.h"
#include "lib_9.h"

int main() {
    auto start = std::chrono::high_resolution_clock::now();

    // Initialize all processors
    lib_0::Processor p0; p0.initialize();
    lib_1::Processor p1; p1.initialize();
    lib_2::Processor p2; p2.initialize();
    lib_3::Processor p3; p3.initialize();
    lib_4::Processor p4; p4.initialize();
    lib_5::Processor p5; p5.initialize();
    lib_6::Processor p6; p6.initialize();
    lib_7::Processor p7; p7.initialize();
    lib_8::Processor p8; p8.initialize();
    lib_9::Processor p9; p9.initialize();

    // Compute results
    long long total = 0;
    total += p0.compute();
    total += p1.compute();
    total += p2.compute();
    total += p3.compute();
    total += p4.compute();
    total += p5.compute();
    total += p6.compute();
    total += p7.compute();
    total += p8.compute();
    total += p9.compute();

    auto end = std::chrono::high_resolution_clock::now();
    auto duration = std::chrono::duration_cast<std::chrono::milliseconds>(end - start);

    std::cout << "Total computation result: " << total << std::endl;
    std::cout << "Execution time: " << duration.count() << "ms" << std::endl;

    return 0;
}
