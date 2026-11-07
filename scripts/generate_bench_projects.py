#!/usr/bin/env python3
# Generate benchmark C++ projects
# Copyright © 2025 Luis Chamberlain. All rights reserved.
# SPDX-License-Identifier: MIT

import os
import sys
from pathlib import Path

def generate_header(index, deps=[]):
    """Generate a C++ header file"""
    includes = "\n".join([f'#include "lib_{d}.h"' for d in deps])

    content = f"""// Auto-generated header {index}
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

#ifndef LIB_{index}_H
#define LIB_{index}_H

{includes}

#include <vector>
#include <string>
#include <memory>

namespace lib_{index} {{

class Processor {{
public:
    Processor();
    ~Processor();

    int compute();
    double process(int x, double y);
    void initialize();
    std::vector<int> transform(const std::vector<int>& input);

private:
    int internal_state_;
}};

// Utility functions
int factorial(int n);
double power(double base, int exp);
std::string format_result(int value);

}} // namespace lib_{index}

#endif // LIB_{index}_H
"""
    return content

def generate_source(index, deps=[]):
    """Generate a C++ source file"""
    includes = "\n".join([f'#include "lib_{d}.h"' for d in deps])

    dep_calls = "\n    ".join([
        f"lib_{d}::Processor p{d}; p{d}.initialize();"
        for d in deps
    ])

    content = f"""// Auto-generated source {index}
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

#include "lib_{index}.h"
{includes}
#include <algorithm>
#include <numeric>
#include <cmath>
#include <sstream>

namespace lib_{index} {{

Processor::Processor() : internal_state_(0) {{
    // Initialize with some computation
    for (int i = 0; i < 100; i++) {{
        internal_state_ += i * i;
    }}
}}

Processor::~Processor() {{
    // Cleanup
    internal_state_ = 0;
}}

int Processor::compute() {{
    std::vector<int> data;
    data.reserve(1000);

    for (int i = 0; i < 1000; i++) {{
        data.push_back(i * i + internal_state_);
    }}

    return std::accumulate(data.begin(), data.end(), 0);
}}

double Processor::process(int x, double y) {{
    double result = static_cast<double>(x) * y;

    // Simulate some computational work
    for (int i = 0; i < 100; i++) {{
        result = result * 1.001 + 0.001;
        result = std::sin(result) * std::cos(result);
    }}

    return result;
}}

void Processor::initialize() {{
    {dep_calls}

    // Perform initialization
    internal_state_ = 0;
    for (int i = 0; i < 10000; i++) {{
        internal_state_ += i % 256;
    }}
}}

std::vector<int> Processor::transform(const std::vector<int>& input) {{
    std::vector<int> result = input;

    std::transform(result.begin(), result.end(), result.begin(),
                   [this](int val) {{ return val + internal_state_; }});

    std::sort(result.begin(), result.end());

    return result;
}}

int factorial(int n) {{
    if (n <= 1) return 1;
    return n * factorial(n - 1);
}}

double power(double base, int exp) {{
    if (exp == 0) return 1.0;
    if (exp < 0) return 1.0 / power(base, -exp);

    double result = 1.0;
    for (int i = 0; i < exp; i++) {{
        result *= base;
    }}
    return result;
}}

std::string format_result(int value) {{
    std::ostringstream oss;
    oss << "Result_{index}: " << value;
    return oss.str();
}}

}} // namespace lib_{index}
"""
    return content

def generate_main(num_files):
    """Generate main.cc that uses all libraries"""
    includes = "\n".join([f'#include "lib_{i}.h"' for i in range(num_files)])

    initializations = "\n    ".join([
        f"lib_{i}::Processor p{i}; p{i}.initialize();"
        for i in range(num_files)
    ])

    computations = "\n    ".join([
        f"total += p{i}.compute();"
        for i in range(num_files)
    ])

    content = f"""// Main benchmark driver
// Copyright © 2025 Luis Chamberlain. All rights reserved.
// SPDX-License-Identifier: MIT

#include <iostream>
#include <chrono>
{includes}

int main() {{
    auto start = std::chrono::high_resolution_clock::now();

    // Initialize all processors
    {initializations}

    // Compute results
    long long total = 0;
    {computations}

    auto end = std::chrono::high_resolution_clock::now();
    auto duration = std::chrono::duration_cast<std::chrono::milliseconds>(end - start);

    std::cout << "Total computation result: " << total << std::endl;
    std::cout << "Execution time: " << duration.count() << "ms" << std::endl;

    return 0;
}}
"""
    return content

def generate_project(project_dir, num_files, deps_per_file=2):
    """Generate a complete benchmark project"""
    project_path = Path(project_dir)
    project_path.mkdir(parents=True, exist_ok=True)

    print(f"Generating {num_files} C++ files in {project_dir}...")

    # Generate header and source files with dependencies
    for i in range(num_files):
        # Calculate dependencies (depend on previous files)
        deps = []
        for j in range(1, min(deps_per_file + 1, i + 1)):
            deps.append(i - j)

        # Write header file
        header_path = project_path / f"lib_{i}.h"
        with open(header_path, 'w') as f:
            f.write(generate_header(i, deps))

        # Write source file
        source_path = project_path / f"lib_{i}.cc"
        with open(source_path, 'w') as f:
            f.write(generate_source(i, deps))

    # Generate main.cc
    main_path = project_path / "main.cc"
    with open(main_path, 'w') as f:
        f.write(generate_main(num_files))

    print(f"✓ Generated {num_files} header files")
    print(f"✓ Generated {num_files} source files")
    print(f"✓ Generated main.cc")
    print(f"✓ Project complete: {project_dir}")

def main():
    script_dir = Path(__file__).parent
    examples_dir = script_dir.parent / "examples"

    # Generate benchmark projects
    projects = [
        ("bench_project_small", 10, 2),      # 10 files, 2 deps each
        ("bench_project_medium", 50, 3),     # 50 files, 3 deps each
        ("bench_project_large", 200, 4),     # 200 files, 4 deps each
        ("bench_project_complex", 100, 8),   # 100 files, 8 deps each (deep tree)
    ]

    for project_name, num_files, deps_per_file in projects:
        project_dir = examples_dir / project_name
        generate_project(project_dir, num_files, deps_per_file)
        print()

    print("All benchmark projects generated successfully!")

if __name__ == "__main__":
    main()
