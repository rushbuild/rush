#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
#
# Generates BUILD files with .bzl dependencies to test lazy loading.
# Similar to gpbuild's gcfiles.py but for Bazel BUILD files.

import os
import argparse
import multiprocessing
from pathlib import Path

def generate_bzl_file(outdir):
    """Generate a .bzl file with helper macros"""
    bzl_content = '''"""Helper macros for generated libraries."""

def lib_with_defaults(name, srcs, **kwargs):
    """Wrapper around cc_library with default settings."""
    native.cc_library(
        name = name,
        srcs = srcs,
        copts = ["-O2", "-Wall"],
        **kwargs
    )

def test_suite_macro(name, libs):
    """Creates a test suite from a list of libraries."""
    tests = []
    for lib in libs:
        test_name = lib + "_test"
        native.cc_test(
            name = test_name,
            srcs = [lib + "_test.cc"],
            deps = [":" + lib],
        )
        tests.append(":" + test_name)

    native.test_suite(
        name = name,
        tests = tests,
    )
'''
    bzl_path = os.path.join(outdir, "defs.bzl")
    with open(bzl_path, "w") as f:
        f.write(bzl_content)
    return bzl_path

def generate_package_build(pkg_num, libs_per_pkg, use_bzl, has_deps, total_packages, outdir):
    """Generate BUILD file for a package"""
    pkg_dir = os.path.join(outdir, f"pkg{pkg_num:04d}")
    os.makedirs(pkg_dir, exist_ok=True)

    build_lines = []

    # Add load statement if using .bzl
    if use_bzl:
        build_lines.append('load("//:defs.bzl", "lib_with_defaults")\n\n')

    # Generate library targets
    for lib_idx in range(libs_per_pkg):
        lib_name = f"lib{lib_idx}"
        src_file = f"lib{lib_idx}.cc"

        # Add dependencies to previous package (if not first package)
        deps = []
        if has_deps and pkg_num > 0:
            prev_pkg = pkg_num - 1
            # Depend on first lib from previous package
            deps.append(f'"//pkg{prev_pkg:04d}:lib0"')

        deps_str = f"deps = [{', '.join(deps)}]," if deps else ""

        if use_bzl:
            # Use macro from .bzl file
            build_lines.append(f'lib_with_defaults(\n')
            build_lines.append(f'    name = "{lib_name}",\n')
            build_lines.append(f'    srcs = ["{src_file}"],\n')
            if deps_str:
                build_lines.append(f'    {deps_str}\n')
            build_lines.append(f')\n\n')
        else:
            # Use native rule directly
            build_lines.append(f'cc_library(\n')
            build_lines.append(f'    name = "{lib_name}",\n')
            build_lines.append(f'    srcs = ["{src_file}"],\n')
            if deps_str:
                build_lines.append(f'    {deps_str}\n')
            build_lines.append(f')\n\n')

    # Add a top-level binary in the last package
    if pkg_num == total_packages - 1:
        build_lines.append(f'cc_binary(\n')
        build_lines.append(f'    name = "main",\n')
        build_lines.append(f'    srcs = ["main.cc"],\n')
        # Depend on all libs in this package
        lib_deps = [f'":lib{i}"' for i in range(libs_per_pkg)]
        build_lines.append(f'    deps = [{", ".join(lib_deps)}],\n')
        build_lines.append(f')\n')

    build_path = os.path.join(pkg_dir, "BUILD")
    with open(build_path, "w") as f:
        f.writelines(build_lines)

    return pkg_dir

def main():
    parser = argparse.ArgumentParser(
        description="Generate BUILD files to test lazy .bzl loading"
    )
    parser.add_argument(
        "-p", "--packages",
        type=int,
        default=10,
        help="Number of packages to generate (default: 10)"
    )
    parser.add_argument(
        "-l", "--libs-per-package",
        type=int,
        default=5,
        help="Number of libraries per package (default: 5)"
    )
    parser.add_argument(
        "-b", "--use-bzl",
        action="store_true",
        help="Load macros from .bzl file (tests lazy loading)"
    )
    parser.add_argument(
        "-d", "--with-deps",
        action="store_true",
        help="Add dependencies between packages"
    )
    parser.add_argument(
        "-o", "--outdir",
        default="tests/lazy_loading_test",
        help="Output directory (default: tests/lazy_loading_test)"
    )

    args = parser.parse_args()

    # Create output directory
    os.makedirs(args.outdir, exist_ok=True)

    # Generate WORKSPACE file
    workspace_path = os.path.join(args.outdir, "WORKSPACE")
    with open(workspace_path, "w") as f:
        f.write('# Generated workspace for lazy loading tests\n')

    # Generate .bzl file if needed
    if args.use_bzl:
        bzl_path = generate_bzl_file(args.outdir)
        print(f"Generated {bzl_path}")

    # Generate BUILD file in root with exports
    root_build_path = os.path.join(args.outdir, "BUILD")
    with open(root_build_path, "w") as f:
        if args.use_bzl:
            f.write('exports_files(["defs.bzl"])\n')

    # Generate package BUILD files
    print(f"Generating {args.packages} packages...")
    for pkg_num in range(args.packages):
        pkg_dir = generate_package_build(
            pkg_num,
            args.libs_per_package,
            args.use_bzl,
            args.with_deps,
            args.packages,
            args.outdir
        )
        if pkg_num % 10 == 0:
            print(f"  Generated package {pkg_num}/{args.packages}")

    print(f"\nGeneration complete!")
    print(f"  Total packages: {args.packages}")
    print(f"  Libs per package: {args.libs_per_package}")
    print(f"  Total targets: {args.packages * args.libs_per_package + 1}")  # +1 for binary
    print(f"  Using .bzl macros: {args.use_bzl}")
    print(f"  With dependencies: {args.with_deps}")
    print(f"\nTest lazy loading:")
    print(f"  export RUSH_LAZY_LOADING=1")
    print(f"  rush build //{os.path.basename(args.outdir)}/pkg{args.packages-1:04d}:main")

if __name__ == "__main__":
    main()
