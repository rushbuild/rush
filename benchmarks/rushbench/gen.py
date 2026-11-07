#!/usr/bin/env python3
# Copyright (c) 2026 Luis Chamberlain. All rights reserved.
# SPDX-License-Identifier: MIT
"""
rushbench synthetic workspace generator.

Emits a Bazel-AND-rush-buildable synthetic C++ workspace with tunable
scale. The same generated tree builds with both `bazel build //...`
(via bazelisk pinned to .bazelversion) and `rush build //...`
with NO rush-specific dialect -- standard cc_library / cc_binary only.

Shape (from ChatGPT Pro's plan, project_rush_crush_bazel_plan.md):

  P packages, each a directory pkg_<i>/ containing
      BUILD          -- one cc_library "lib"
      lib.cc         -- defines int lib_<i>_sum(); the .cc #includes all
                        H headers and returns the sum of their constants
      hdr_0.h .. hdr_<H-1>.h  -- each defines one constexpr int constant

  A root cc_binary //:bench that depends on all P libraries and calls
  every lib_<i>_sum(), summing them so nothing is dead-code-eliminated.

The point is to calibrate the *manifest + ledger scale* (file-dep count)
to TensorFlow's, so a warm no-op exercises rush's stat-walk at TF scale
WITHOUT the minutes-long TF clean build. wide_20k (P=384, H=48) yields
~18,432 headers and ~20k file deps -- the TF-scale calibration target.

Usage:
    gen.py <family>            # one of: wide_1k wide_5k wide_10k wide_20k wide_40k
    gen.py --packages P --headers H --out DIR [--name NAME]
    gen.py --all [--out-root DIR]   # generate the whole family, print counts
    gen.py --print-only             # just print the family table, generate nothing
"""

import argparse
import os
import shutil
import sys

# (name, P packages, H headers-per-package)
FAMILY = [
    ("wide_1k", 64, 16),
    ("wide_5k", 160, 32),
    ("wide_10k", 256, 40),
    ("wide_20k", 384, 48),
    ("wide_40k", 512, 80),
]
FAMILY_MAP = {f[0]: f for f in FAMILY}

# Current Bazel removed native cc_library/cc_binary from the global BUILD-file
# environment. Default to Bazel 9 and explicit rules_cc loads; keep Bazel 6
# available for reproducing the published no-op baseline.
DEFAULT_BAZEL_VERSION = "9.1.1"
RULES_CC_VERSION = "0.2.17"


def bazel_major(version):
    """Return the integer Bazel major version from a version string."""
    try:
        return int(version.split(".", 1)[0])
    except (TypeError, ValueError):
        return 0


def use_rules_cc_loads(version):
    """Bazel 9+ requires explicit cc rule loads from rules_cc."""
    return bazel_major(version) >= 9


def counts(P, H, modern_bazel=False):
    """Return the file/dep counts for a (P, H) workspace."""
    headers = P * H
    cc_files = P            # one .cc per package
    build_files = P + 1     # per-package BUILD + root BUILD
    compile_actions = P + 1  # P library .cc + 1 root main.cc
    libraries = P
    # File deps roughly seen by the manifest/ledger walk: every header,
    # every .cc, every BUILD, plus root config files. This is the number we
    # calibrate against TF's ~18-21k.
    root_config_files = 4  # WORKSPACE, .bazelversion, .bazelrc, .bazelignore
    if modern_bazel:
        root_config_files += 1  # MODULE.bazel
    file_deps = headers + cc_files + build_files + root_config_files
    return {
        "packages": P,
        "headers_per_pkg": H,
        "headers": headers,
        "cc_files": cc_files,
        "build_files": build_files,
        "libraries": libraries,
        "compile_actions": compile_actions,
        "link_actions": 1,
        "file_deps_approx": file_deps,
    }


def gen_header(pkg_idx, hdr_idx):
    guard = f"PKG_{pkg_idx}_HDR_{hdr_idx}_H_"
    # A distinct constant per header so the sum is content-dependent.
    val = (pkg_idx * 1315423911 + hdr_idx * 2654435761) & 0xFFFF
    return (
        f"#ifndef {guard}\n"
        f"#define {guard}\n"
        f"// rushbench generated header pkg={pkg_idx} hdr={hdr_idx}\n"
        f"namespace pkg_{pkg_idx} {{\n"
        f"constexpr int kHdr{hdr_idx} = {val};\n"
        f"}}  // namespace pkg_{pkg_idx}\n"
        f"#endif  // {guard}\n"
    )


def gen_lib_cc(pkg_idx, H):
    includes = "".join(f'#include "hdr_{h}.h"\n' for h in range(H))
    terms = " + ".join(f"pkg_{pkg_idx}::kHdr{h}" for h in range(H))
    if not terms:
        terms = "0"
    return (
        f"// rushbench generated library source pkg={pkg_idx}\n"
        f"{includes}"
        f"\n"
        f"int lib_{pkg_idx}_sum() {{\n"
        f"  return {terms};\n"
        f"}}\n"
    )


def gen_pkg_build(pkg_idx, H, modern_bazel=False):
    hdrs = ", ".join(f'"hdr_{h}.h"' for h in range(H))
    load = ""
    if modern_bazel:
        load = 'load("@rules_cc//cc:defs.bzl", "cc_library")\n\n'
    return (
        f"{load}"
        f"# rushbench generated package pkg_{pkg_idx} ({H} headers)\n"
        f"cc_library(\n"
        f'    name = "lib",\n'
        f'    srcs = ["lib.cc"],\n'
        f"    hdrs = [{hdrs}],\n"
        f'    visibility = ["//visibility:public"],\n'
        f")\n"
    )


def gen_root_build(P, modern_bazel=False):
    deps = "".join(f'        "//pkg_{i}:lib",\n' for i in range(P))
    load = ""
    if modern_bazel:
        load = 'load("@rules_cc//cc:defs.bzl", "cc_binary")\n\n'
    return (
        f"{load}"
        f"# rushbench root binary depending on all {P} libraries\n"
        f"cc_binary(\n"
        f'    name = "bench",\n'
        f'    srcs = ["main.cc"],\n'
        f"    deps = [\n"
        f"{deps}"
        f"    ],\n"
        f")\n"
    )


def gen_main_cc(P):
    decls = "".join(f"int lib_{i}_sum();\n" for i in range(P))
    calls = "".join(f"  total += lib_{i}_sum();\n" for i in range(P))
    return (
        f"// rushbench root binary referencing all {P} libraries\n"
        f"#include <cstdio>\n"
        f"{decls}"
        f"\n"
        f"int main() {{\n"
        f"  long total = 0;\n"
        f"{calls}"
        f'  std::printf("rushbench total=%ld\\n", total);\n'
        f"  return 0;\n"
        f"}}\n"
    )


def write_file(path, content):
    with open(path, "w") as f:
        f.write(content)


def generate(out, P, H, name, bazel_version=DEFAULT_BAZEL_VERSION,
             rules_cc_version=RULES_CC_VERSION):
    if os.path.exists(out):
        shutil.rmtree(out)
    os.makedirs(out)

    modern_bazel = use_rules_cc_loads(bazel_version)

    write_file(os.path.join(out, "WORKSPACE"), f'workspace(name = "{name}")\n')
    write_file(os.path.join(out, ".bazelversion"), f"{bazel_version}\n")
    if modern_bazel:
        write_file(os.path.join(out, "MODULE.bazel"),
                   f'module(name = "{name}")\n'
                   f'bazel_dep(name = "rules_cc", '
                   f'version = "{rules_cc_version}")\n')
        write_file(os.path.join(out, ".bazelrc"),
                   "common --enable_bzlmod\n"
                   "build --spawn_strategy=local\n")
    else:
        # Keep older Bazel on the legacy WORKSPACE path (no Bzlmod) so legacy
        # published baselines can still be reproduced byte-for-byte.
        write_file(os.path.join(out, ".bazelrc"),
                   "common --noenable_bzlmod\n"
                   "build --spawn_strategy=local\n")
    # rush writes its scratch (rush-out/) and a default external-repo cache
    # (.cache/rush/external/) INTO the workspace. Those are not part of the
    # synthetic graph; tell Bazel to ignore them so `bazel build //...` does
    # not try to load rush's cached @com_google_absl etc. This is a Bazel-
    # native ignore (like .gitignore), not bench-target special-casing -- the
    # cc_library/cc_binary targets both tools build are untouched.
    write_file(os.path.join(out, ".bazelignore"),
               "rush-out\n"
               ".cache\n")
    write_file(os.path.join(out, "main.cc"), gen_main_cc(P))
    write_file(os.path.join(out, "BUILD"), gen_root_build(P, modern_bazel))

    for i in range(P):
        pkg = os.path.join(out, f"pkg_{i}")
        os.makedirs(pkg)
        write_file(os.path.join(pkg, "BUILD"), gen_pkg_build(i, H, modern_bazel))
        write_file(os.path.join(pkg, "lib.cc"), gen_lib_cc(i, H))
        for h in range(H):
            write_file(os.path.join(pkg, f"hdr_{h}.h"), gen_header(i, h))

    return counts(P, H, modern_bazel)


def print_counts_row(name, P, H, c):
    print(f"  {name:<10} P={P:<4} H={H:<4} "
          f"libs={c['libraries']:<5} headers={c['headers']:<7} "
          f"cc={c['cc_files']:<5} BUILD={c['build_files']:<5} "
          f"compile_actions={c['compile_actions']:<5} "
          f"file_deps~={c['file_deps_approx']}")


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("family", nargs="?", help="family name (e.g. wide_20k)")
    ap.add_argument("--packages", type=int, help="P packages")
    ap.add_argument("--headers", type=int, help="H headers per package")
    ap.add_argument("--out", help="output directory")
    ap.add_argument("--name", default=None, help="workspace name")
    ap.add_argument("--bazel-version", default=DEFAULT_BAZEL_VERSION,
                    help=f"bazelisk version to pin (default: {DEFAULT_BAZEL_VERSION})")
    ap.add_argument("--rules-cc-version", default=RULES_CC_VERSION,
                    help=f"rules_cc module version for Bazel 9+ (default: {RULES_CC_VERSION})")
    ap.add_argument("--all", action="store_true", help="generate whole family")
    ap.add_argument("--out-root", default=None,
                    help="root dir for --all (default: ./gen next to gen.py)")
    ap.add_argument("--print-only", action="store_true",
                    help="print the family count table, generate nothing")
    args = ap.parse_args()

    here = os.path.dirname(os.path.abspath(__file__))

    if args.print_only:
        print("rushbench family (no files written):")
        modern_bazel = use_rules_cc_loads(args.bazel_version)
        for name, P, H in FAMILY:
            print_counts_row(name, P, H, counts(P, H, modern_bazel))
        return

    if args.all:
        root = args.out_root or os.path.join(here, "gen")
        os.makedirs(root, exist_ok=True)
        print(f"Generating rushbench family under {root}:")
        for name, P, H in FAMILY:
            out = os.path.join(root, name)
            c = generate(out, P, H, name, args.bazel_version,
                         args.rules_cc_version)
            print_counts_row(name, P, H, c)
            print(f"             -> {out}")
        return

    if args.family:
        if args.family not in FAMILY_MAP:
            sys.exit(f"unknown family '{args.family}'; "
                     f"choose from {', '.join(FAMILY_MAP)}")
        name, P, H = FAMILY_MAP[args.family]
        out = args.out or os.path.join(here, "gen", name)
        c = generate(out, P, H, name, args.bazel_version,
                     args.rules_cc_version)
        print(f"Generated {name} at {out}:")
        print_counts_row(name, P, H, c)
        return

    if args.packages is not None and args.headers is not None and args.out:
        name = args.name or os.path.basename(os.path.normpath(args.out))
        c = generate(args.out, args.packages, args.headers, name,
                     args.bazel_version, args.rules_cc_version)
        print(f"Generated custom workspace at {args.out}:")
        print_counts_row(name, args.packages, args.headers, c)
        return

    ap.print_help()
    sys.exit(2)


if __name__ == "__main__":
    main()
