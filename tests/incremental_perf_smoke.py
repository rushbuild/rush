#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Check daemon invalidation, restoration and sink-header reuse on real builds."""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import subprocess


SPEC = importlib.util.spec_from_file_location(
    "comparison", Path(__file__).resolve().parents[1] / "benchmarks/buck2/run.py")
BENCH = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(BENCH)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rush", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    binary = args.rush.resolve()
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    os.sched_setaffinity(0, sorted(os.sched_getaffinity(0))[:8])
    cases = []

    def record(name):
        cases.append({"case": name, "passed": True})
        (root / "summary.json").write_text(json.dumps(
            {"complete": False, "cases": cases}, indent=2) + "\n")

    def engine(name):
        folder = root / name
        folder.mkdir()
        workspace = BENCH.setup(folder, "rush", 2, 2, Path("."))
        return BENCH.Engine(folder, "rush", binary, workspace, 8), folder

    def check(e, folder, name, expected, counts=None):
        result = e.build(name, 0, expected, folder)
        if counts is not None:
            assert tuple(result[k] for k in ("compiles", "archives", "links")) == counts, result
        assert list(Path(e.env["XDG_RUNTIME_DIR"]).rglob("metadata.json")), "daemon discovery missing; choose a shorter output path"
        record(name)

    e, folder = engine("cpp")
    try:
        header = e.workspace / "pkg_0/hdr_0.h"
        initial = header.stat()
        os.utime(header, ns=(initial.st_atime_ns, initial.st_mtime_ns - 10_000_000_000))
        check(e, folder, "clean", 98480, (3, 2, 1))
        check(e, folder, "unchanged", 98480, (0, 0, 0))
        header = e.workspace / "pkg_0/hdr_0.h"
        original_header = header.read_text()
        old = header.stat()
        header.write_text(original_header.replace("kHdr0 = 0;", "kHdr0 = 7;"))
        os.utime(header, ns=(old.st_atime_ns, old.st_mtime_ns))
        check(e, folder, "same-size-edit-with-restored-mtime", 98487, (1, 1, 1))
        check(e, folder, "unchanged-after-edit", 98487, (0, 0, 0))
        os.utime(header, None)
        check(e, folder, "identical-content-touch", 98487, (0, 0, 0))
        (e.workspace / "rush-out/bench").unlink()
        check(e, folder, "missing-executable", 98487)
        (e.workspace / "rush-out/bench").write_bytes(b"damaged")
        check(e, folder, "damaged-executable", 98487)
        next((e.workspace / "rush-out").rglob("*.o")).unlink()
        check(e, folder, "missing-object", 98487)
        next((e.workspace / "rush-out").rglob("*.a")).unlink()
        check(e, folder, "missing-archive", 98487)
        source = e.workspace / "pkg_0/lib.cc"
        original_source = source.read_text()
        source.write_text(original_source + "\n#error expected-test-failure\n")
        try:
            e.invoke(e.command, folder / "expected-failure.log")
        except subprocess.CalledProcessError:
            record("compile-failure-propagated")
        else:
            raise AssertionError("invalid C++ reported success")
        source.write_text(original_source)
        check(e, folder, "recover-after-failure", 98487)
    finally:
        e.stop(folder)

    e, folder = engine("headers")
    try:
        (e.workspace / "BUILD").write_text('''cc_library(name="headers", hdrs=["first.h", "second.h"])
cc_binary(name="bench", srcs=["main.cc"], deps=[":headers"])
''')
        (e.workspace / "main.cc").write_text('#include <cstdio>\n#include "first.h"\nint main(){std::printf("rushbench total=%d\\n", value());}\n')
        first = e.workspace / "first.h"
        second = e.workspace / "second.h"
        first.write_text("inline int value(){return 11;}\n")
        second.write_text("#define VALUE 22\n")
        check(e, folder, "header-layout-initial", 11)
        first.write_text('#include "second.h"\ninline int value(){return VALUE;}\n')
        check(e, folder, "header-include-reachability-change", 22)
        second.write_text("#define VALUE 33\n")
        check(e, folder, "newly-reachable-header-edit", 33)
    finally:
        e.stop(folder)

    e, folder = engine("analysis")
    try:
        definitions = e.workspace / "defs.bzl"
        definitions.write_text("VALUE = 11\n")
        (e.workspace / "BUILD").write_text('''load(":defs.bzl", "VALUE")
genrule(name="copy", srcs=["defs.bzl"], outs=["value"], cmd="echo %s > $@" % VALUE)
''')
        command = ["//:copy" if word == "//:bench" else word for word in e.command]
        e.invoke(command, folder / "initial.log")
        output = e.workspace / "rush-out/genfiles/value"
        assert output.read_text().strip() == "11"
        e.invoke(command, folder / "unchanged.log")
        logs = list(Path(e.env["XDG_RUNTIME_DIR"]).rglob("rushd.log"))
        assert logs and any("watcher CLEAN" in p.read_text() for p in logs), "watcher fast path was not exercised"
        record("dual-role-analysis-input-warm")
        definitions.write_text("VALUE = 22\n")
        e.invoke(command, folder / "changed.log")
        assert output.read_text().strip() == "22", "reused an old expanded action after its analysis input changed"
        record("dual-role-analysis-action-input-change")
    finally:
        e.stop(folder)

    (root / "summary.json").write_text(json.dumps(
        {"complete": True, "cases": cases}, indent=2) + "\n")
    print(f"PASS: {len(cases)} daemon incremental controls")


if __name__ == "__main__":
    main()
