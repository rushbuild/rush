#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Compare local C++ builds; retain raw logs outside the source checkout."""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import select
import shlex
import shutil
import signal
import statistics
import subprocess
import time


SPEC = importlib.util.spec_from_file_location(
    "rushbench_gen", Path(__file__).resolve().parents[1] / "rushbench/gen.py")
GEN = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GEN)


def source_digest(root):
    digest = hashlib.sha256()
    for path in sorted(root.rglob("*")):
        if path.suffix in {".cc", ".h"}:
            digest.update(str(path.relative_to(root)).encode() + b"\0")
            digest.update(path.read_bytes())
    return digest.hexdigest()


def setup(root, engine, packages, headers, prelude):
    workspace = root / engine
    GEN.generate(str(workspace), packages, headers, "comparison", "6.5.0")
    if engine == "buck2":
        for path in workspace.rglob("BUILD"):
            text = path.read_text().replace("cc_library(", "cxx_library(")
            text = text.replace("cc_binary(", "cxx_binary(")
            text = text.replace("hdrs =", "headers =")
            text = text.replace('"//visibility:public"', '"PUBLIC"')
            text = text.replace('    name = "bench",',
                                '    name = "bench",\n'
                                '    link_style = "static",')
            path.with_name("BUCK").write_text(text)
        (workspace / "prelude").symlink_to(prelude, target_is_directory=True)
        (workspace / ".buckconfig").write_text(
            "[cells]\nroot = .\nprelude = prelude\n"
            "toolchains = toolchains\nnone = none\n"
            "[cell_aliases]\nconfig = prelude\nfbcode = none\n"
            "fbsource = none\nbuck = none\n"
            "[parser]\ntarget_platform_detector_spec = "
            "target:root//...->prelude//platforms:default\n")
        (workspace / "toolchains").mkdir()
        (workspace / "toolchains/BUCK").write_text(
            'load("@prelude//toolchains:cxx.bzl", "system_cxx_toolchain")\n'
            'load("@prelude//toolchains:python.bzl", '
            '"system_python_bootstrap_toolchain")\n'
            'system_cxx_toolchain(name="cxx", compiler="gcc", '
            'compiler_type="gcc", cxx_compiler="g++", linker="g++", '
            'cxx_flags=["-O0", "-g", "-std=c++17", "-fPIC", "-include", '
            '"cstdint", "-D_GNU_SOURCE"], visibility=["PUBLIC"])\n'
            'system_python_bootstrap_toolchain(name="python_bootstrap", '
            'visibility=["PUBLIC"])\n')
    return workspace


class Engine:
    def __init__(self, root, name, binary, workspace, jobs):
        self.name, self.binary, self.workspace = name, binary, workspace
        self.log = root / (name + "-compiler.log")
        self.log.touch()
        bindir = root / (name + "-bin")
        bindir.mkdir()
        for tool in ("g++", "gcc", "ar"):
            actual = shutil.which(tool)
            if not actual:
                raise RuntimeError("missing compiler tool: " + tool)
            wrapper = bindir / tool
            wrapper.write_text(
                "#!/bin/sh\n"
                "printf '%s\\n' " + shlex.quote(tool + " ") +
                '"$*" >> ' + shlex.quote(str(self.log)) + "\nexec " +
                shlex.quote(actual) + ' "$@"\n')
            wrapper.chmod(0o755)
        self.env = {"PATH": str(bindir) + os.pathsep + os.environ["PATH"],
                    "HOME": str(root / (name + "-home")),
                    "LANG": "C.UTF-8", "RUSH_CXX": str(bindir / "g++")}
        Path(self.env["HOME"]).mkdir()
        runtime = root / (name + "-runtime")
        runtime.mkdir(mode=0o700)
        self.env["XDG_RUNTIME_DIR"] = str(runtime)
        if name == "rush":
            self.command = [str(binary), "build", "//:bench", "-j", str(jobs),
                            "--daemon=auto"]
        else:
            self.command = [str(binary), "--isolation-dir", "comparison",
                            "build", "//:bench", "-j", str(jobs),
                            "--local-only", "--show-full-output"]

    def invoke(self, argv, log):
        with log.open("wb") as output:
            return subprocess.run(argv, cwd=self.workspace, env=self.env,
                                  stdout=output, stderr=subprocess.STDOUT,
                                  check=True)

    def stop(self, root):
        if self.name == "buck2":
            self.invoke([str(self.binary), "--isolation-dir", "comparison",
                         "kill"], root / "buck2-stop.log")
            return
        # Rush has no shutdown CLI. Signal only a verified task-local daemon;
        # the pidfd keeps a recycled process ID from receiving the signal.
        for path in Path(self.env["XDG_RUNTIME_DIR"]).rglob("metadata.json"):
            pid = json.loads(path.read_text())["pid"]
            try:
                fd = os.pidfd_open(pid)
            except ProcessLookupError:
                continue
            try:
                process = Path("/proc") / str(pid)
                if (process / "cwd").resolve() != self.workspace:
                    raise RuntimeError("refuse to signal a different workspace")
                if (process / "exe").resolve() != self.binary:
                    raise RuntimeError("refuse to signal a different executable")
                if b"daemon" not in (process / "cmdline").read_bytes().split(b"\0"):
                    raise RuntimeError("refuse to signal a non-daemon process")
                signal.pidfd_send_signal(fd, signal.SIGTERM)
                if not select.select([fd], [], [], 10)[0]:
                    raise RuntimeError("task daemon failed to exit")
            finally:
                os.close(fd)

    def clean(self, log):
        if self.name == "buck2":
            self.invoke([str(self.binary), "--isolation-dir", "comparison",
                         "clean"], log)
        else:
            self.stop(log.parent)
            for name in ("rush-out", ".cache"):
                shutil.rmtree(self.workspace / name, ignore_errors=True)
            shutil.rmtree(Path(self.env["HOME"]) / ".cache", ignore_errors=True)
            runtime = Path(self.env["XDG_RUNTIME_DIR"])
            shutil.rmtree(runtime)
            runtime.mkdir(mode=0o700)

    def build(self, mode, sample, expected, root):
        before = self.log.stat().st_size
        command = list(self.command)
        start = time.perf_counter_ns()
        log = root / f"{self.name}-{mode}-{sample}.log"
        self.invoke(command, log)
        seconds = (time.perf_counter_ns() - start) / 1e9
        with self.log.open() as stream:
            stream.seek(before)
            actions = stream.read().splitlines()
        compiles = sum(line.startswith("g++ ") and " -c " in line
                       for line in actions)
        archives = sum(line.startswith("ar ") and "--version" not in line
                       for line in actions)
        links = sum(line.startswith("g++ ") and " -c " not in line
                    and "--version" not in line
                    for line in actions)
        if self.name == "rush":
            executable = self.workspace / "rush-out/bench"
        else:
            outputs = [line.split(" ", 1)[1] for line in log.read_text().splitlines()
                       if line.startswith("root//:bench ")]
            if len(outputs) != 1:
                raise RuntimeError("cannot locate Buck2 executable; see " + str(log))
            executable = self.workspace / outputs[0]
        value = subprocess.check_output([str(executable)], text=True).strip()
        if value != f"rushbench total={expected}":
            raise RuntimeError(f"{self.name} stale/wrong output: {value}")
        return {"engine": self.name, "mode": mode, "sample": sample,
                "seconds": seconds, "compiles": compiles,
                "archives": archives, "links": links, "output": value}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rush", type=Path, required=True)
    parser.add_argument("--buck2", type=Path, required=True)
    parser.add_argument("--prelude", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--packages", type=int, default=64)
    parser.add_argument("--headers", type=int, default=16)
    parser.add_argument("--jobs", type=int, default=8)
    parser.add_argument("--samples", type=int, default=5)
    args = parser.parse_args()
    if min(args.packages, args.headers, args.jobs, args.samples) < 1:
        parser.error("counts must be positive")
    for binary in (args.rush, args.buck2):
        if not binary.is_file() or not os.access(binary, os.X_OK):
            parser.error("missing executable: " + str(binary))
    if not args.prelude.is_dir():
        parser.error("missing prelude directory")
    cpus = sorted(os.sched_getaffinity(0))[:args.jobs]
    if len(cpus) != args.jobs:
        parser.error("not enough allowed CPUs for the requested budget")
    # Bound this process and its children; change no other host workload.
    os.sched_setaffinity(0, cpus)
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    engines = [Engine(root, name, binary.resolve(),
                      setup(root, name, args.packages, args.headers,
                            args.prelude.resolve()), args.jobs)
               for name, binary in (("rush", args.rush), ("buck2", args.buck2))]
    digests = [source_digest(e.workspace) for e in engines]
    if digests[0] != digests[1]:
        raise RuntimeError("source bytes differ")
    total = sum((p * 1315423911 + h * 2654435761) & 0xFFFF
                for p in range(args.packages) for h in range(args.headers))
    originals = {e.name: {name: (e.workspace / name).read_text()
                         for name in ("pkg_0/lib.cc", "pkg_0/hdr_0.h")}
                 for e in engines}
    records = []

    def record(engine, mode, sample, expected):
        result = engine.build(mode, sample, expected, root)
        counts = (result["compiles"], result["archives"], result["links"])
        if mode == "clean" and counts != (args.packages + 1, args.packages, 1):
            raise RuntimeError(f"incomplete clean work: {result}")
        if mode == "noop" and counts != (0, 0, 0):
            raise RuntimeError(f"unchanged command executed actions: {result}")
        if mode in {"source_edit", "header_edit"} and min(counts) < 1:
            raise RuntimeError(f"edit skipped expected work: {result}")
        records.append(result)
        (root / "samples.json").write_text(json.dumps({
            "packages": args.packages, "headers_per_package": args.headers,
            "jobs_requested": args.jobs, "cpu_affinity": cpus,
            "source_sha256": digests[0], "samples": records,
        }, indent=2) + "\n")
        print(json.dumps(result), flush=True)

    try:
        # Each clean sample starts without outputs or retained analysis state.
        for sample in range(args.samples):
            for engine in engines[::1 if sample % 2 == 0 else -1]:
                engine.clean(root / f"{engine.name}-clean-reset-{sample}.log")
                record(engine, "clean", sample, total)
        for engine in engines:
            record(engine, "warmup", 0, total)
        for mode in ("noop", "source_edit", "header_edit"):
            for sample in range(args.samples):
                delta = 101 + sample
                for engine in engines[::1 if sample % 2 == 0 else -1]:
                    expected = total
                    if mode != "noop":
                        source = originals[engine.name]["pkg_0/lib.cc"]
                        header = originals[engine.name]["pkg_0/hdr_0.h"]
                        if mode == "source_edit":
                            source = source.replace("  return ",
                                                    f"  return {delta} + ")
                            expected += delta
                        else:
                            # Keep the final source edit; change only the header.
                            source = source.replace("  return ",
                                f"  return {100 + args.samples} + ")
                            header = header.replace("kHdr0 = 0;",
                                                    f"kHdr0 = {delta};")
                            expected += delta + 100 + args.samples
                        (engine.workspace / "pkg_0/lib.cc").write_text(source)
                        (engine.workspace / "pkg_0/hdr_0.h").write_text(header)
                    record(engine, mode, sample, expected)
    finally:
        for engine in engines:
            engine.stop(root)
    for mode in ("clean", "noop", "source_edit", "header_edit"):
        print(mode, {e.name: statistics.median(
            r["seconds"] for r in records
            if r["engine"] == e.name and r["mode"] == mode) for e in engines})


if __name__ == "__main__":
    main()
