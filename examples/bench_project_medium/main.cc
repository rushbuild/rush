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
#include "lib_10.h"
#include "lib_11.h"
#include "lib_12.h"
#include "lib_13.h"
#include "lib_14.h"
#include "lib_15.h"
#include "lib_16.h"
#include "lib_17.h"
#include "lib_18.h"
#include "lib_19.h"
#include "lib_20.h"
#include "lib_21.h"
#include "lib_22.h"
#include "lib_23.h"
#include "lib_24.h"
#include "lib_25.h"
#include "lib_26.h"
#include "lib_27.h"
#include "lib_28.h"
#include "lib_29.h"
#include "lib_30.h"
#include "lib_31.h"
#include "lib_32.h"
#include "lib_33.h"
#include "lib_34.h"
#include "lib_35.h"
#include "lib_36.h"
#include "lib_37.h"
#include "lib_38.h"
#include "lib_39.h"
#include "lib_40.h"
#include "lib_41.h"
#include "lib_42.h"
#include "lib_43.h"
#include "lib_44.h"
#include "lib_45.h"
#include "lib_46.h"
#include "lib_47.h"
#include "lib_48.h"
#include "lib_49.h"

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
    lib_10::Processor p10; p10.initialize();
    lib_11::Processor p11; p11.initialize();
    lib_12::Processor p12; p12.initialize();
    lib_13::Processor p13; p13.initialize();
    lib_14::Processor p14; p14.initialize();
    lib_15::Processor p15; p15.initialize();
    lib_16::Processor p16; p16.initialize();
    lib_17::Processor p17; p17.initialize();
    lib_18::Processor p18; p18.initialize();
    lib_19::Processor p19; p19.initialize();
    lib_20::Processor p20; p20.initialize();
    lib_21::Processor p21; p21.initialize();
    lib_22::Processor p22; p22.initialize();
    lib_23::Processor p23; p23.initialize();
    lib_24::Processor p24; p24.initialize();
    lib_25::Processor p25; p25.initialize();
    lib_26::Processor p26; p26.initialize();
    lib_27::Processor p27; p27.initialize();
    lib_28::Processor p28; p28.initialize();
    lib_29::Processor p29; p29.initialize();
    lib_30::Processor p30; p30.initialize();
    lib_31::Processor p31; p31.initialize();
    lib_32::Processor p32; p32.initialize();
    lib_33::Processor p33; p33.initialize();
    lib_34::Processor p34; p34.initialize();
    lib_35::Processor p35; p35.initialize();
    lib_36::Processor p36; p36.initialize();
    lib_37::Processor p37; p37.initialize();
    lib_38::Processor p38; p38.initialize();
    lib_39::Processor p39; p39.initialize();
    lib_40::Processor p40; p40.initialize();
    lib_41::Processor p41; p41.initialize();
    lib_42::Processor p42; p42.initialize();
    lib_43::Processor p43; p43.initialize();
    lib_44::Processor p44; p44.initialize();
    lib_45::Processor p45; p45.initialize();
    lib_46::Processor p46; p46.initialize();
    lib_47::Processor p47; p47.initialize();
    lib_48::Processor p48; p48.initialize();
    lib_49::Processor p49; p49.initialize();

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
    total += p10.compute();
    total += p11.compute();
    total += p12.compute();
    total += p13.compute();
    total += p14.compute();
    total += p15.compute();
    total += p16.compute();
    total += p17.compute();
    total += p18.compute();
    total += p19.compute();
    total += p20.compute();
    total += p21.compute();
    total += p22.compute();
    total += p23.compute();
    total += p24.compute();
    total += p25.compute();
    total += p26.compute();
    total += p27.compute();
    total += p28.compute();
    total += p29.compute();
    total += p30.compute();
    total += p31.compute();
    total += p32.compute();
    total += p33.compute();
    total += p34.compute();
    total += p35.compute();
    total += p36.compute();
    total += p37.compute();
    total += p38.compute();
    total += p39.compute();
    total += p40.compute();
    total += p41.compute();
    total += p42.compute();
    total += p43.compute();
    total += p44.compute();
    total += p45.compute();
    total += p46.compute();
    total += p47.compute();
    total += p48.compute();
    total += p49.compute();

    auto end = std::chrono::high_resolution_clock::now();
    auto duration = std::chrono::duration_cast<std::chrono::milliseconds>(end - start);

    std::cout << "Total computation result: " << total << std::endl;
    std::cout << "Execution time: " << duration.count() << "ms" << std::endl;

    return 0;
}
