#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Check native C++ remote builds, cache restoration and failure propagation."""

import argparse
import importlib.util
import json
from pathlib import Path
import shlex
import shutil
import subprocess
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rush", type=Path, required=True)
    parser.add_argument("--endpoint", required=True)
    parser.add_argument("--toolchain", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    binary = args.rush.resolve()
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    module_spec = importlib.util.spec_from_file_location("fixture", Path(__file__).resolve().parents[1] / "benchmarks/rushbench/gen.py")
    fixture = importlib.util.module_from_spec(module_spec)
    module_spec.loader.exec_module(fixture)
    workspace = root / "workspace"
    fixture.generate(str(workspace), 2, 2, "remote_native", "6.5.0")
    # Distinct source bytes force an independent first compile on a reused CAS.
    nonce = uuid.uuid4().hex
    for file in workspace.rglob("*.cc"):
        file.write_text("// " + nonce + "\n" + file.read_text())
    base = sum((p * 1315423911 + h * 2654435761) & 0xFFFF for p in range(2) for h in range(2))
    wrappers = root / "bin"
    wrappers.mkdir()
    forbidden = root / "local-execution-forbidden"
    for tool in ("g++", "gcc", "ar"):
        path = wrappers / tool
        path.write_text(f'''#!/bin/sh
if [ "$#" = 1 ] && [ "$1" = --version ]; then exec /usr/bin/{tool} --version; fi
echo local-action >> {shlex.quote(str(forbidden))}
exit 97
''')
        path.chmod(0o755)
    home = root / "home"
    home.mkdir()
    environment = {"HOME": str(home), "PATH": str(wrappers) + ":/usr/bin:/bin",
                   "RUST_LOG": "error", "RUSH_CXX": "g++", "RUSH_LINKER": "bfd"}
    command = [str(binary), "build", "//:bench", "--remote-executor", args.endpoint,
               "--remote-platform", "toolchain=" + args.toolchain,
               "--remote-platform", "OSFamily=linux", "--disk-cache=false", "-j8"]
    results = []

    def run(case, expected=None, invocation=None):
        with (root / (case + ".log")).open("wb") as log:
            result = subprocess.run(invocation or command, cwd=workspace, env=environment,
                                    stdout=log, stderr=subprocess.STDOUT, timeout=120)
        assert (result.returncode == 0) == (expected is not None), case
        assert not forbidden.exists(), "a remote failure attempted local execution"
        if expected is not None:
            value = subprocess.check_output([str(workspace / "rush-out/bench")])
            assert value == f"rushbench total={expected}\n".encode(), case
        results.append({"case": case, "passed": True})

    run("first-build", base)
    shutil.rmtree(workspace / "rush-out")
    run("restore-missing-outputs", base)
    source = workspace / "pkg_0/lib.cc"
    source.write_text(source.read_text().replace("  return ", "  return 7 + "))
    run("source-edit", base + 7)
    header = workspace / "pkg_0/hdr_0.h"
    header.write_text(header.read_text().replace("kHdr0 = 0;", "kHdr0 = 17;"))
    run("header-edit", base + 24)
    run("unchanged", base + 24)
    (workspace / "rush-out/bench").write_bytes(b"damaged")
    run("repair-corrupt-output", base + 24)
    shutil.rmtree(workspace / "rush-out")
    unavailable = list(command)
    unavailable[unavailable.index(args.endpoint)] = "http://127.0.0.1:1"
    run("unavailable-no-local-fallback", invocation=unavailable)
    assert not (workspace / "rush-out/bench").exists()
    source.write_text(source.read_text() + "\nthis is not valid C++;\n")
    run("compile-failure-no-local-fallback")
    assert not (workspace / "rush-out/bench").exists()
    results.append({"case": "local-compiler-and-archiver-never-executed", "passed": True})
    (root / "summary.json").write_text(json.dumps({"complete": True, "cases": results}, indent=2) + "\n")
    print(f"PASS: {len(results)} native remote-build cases")


if __name__ == "__main__":
    main()
