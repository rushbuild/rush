#!/usr/bin/env python3
# Copyright (c) 2026 Luis Chamberlain. All rights reserved.
# SPDX-License-Identifier: MIT
"""
rushbench warm-no-op measurement harness.

Measures the warm NO-OP latency of `rush build //...` or `bazel build //...`
on a rushbench workspace and reports the FULL distribution (p50/p90/p95/p99/
min/max) -- never a hidden mean. Records load average, CPU governor, and FS so
the numbers are reproducible.

Bazel is measured in two modes per ChatGPT Pro's correctness warning #3:
  --tool bazel           default (scan) path -- Bazel's out-of-the-box no-op
  --tool bazel-watchfs   --watchfs (its watcher path, default OFF)
Comparing rush only against Bazel default would be misleading; report both.

Protocol:
  1. one warm-up clean build (so the no-op has something to be a no-op over)
  2. N timed no-op invocations of the identical `//...` command
  3. parse wall time per invocation; report the distribution

Usage:
  noop_bench.py --workspace DIR --tool {rush,bazel,bazel-watchfs} [--iters 30]
                [--rush-bin PATH] [--bazel-bin PATH] [--json OUT.json]
                [--no-clean]   # skip the warm-up clean build (already built)
"""

import argparse
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys
import time


def read_governor():
    p = "/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"
    try:
        with open(p) as f:
            return f.read().strip()
    except OSError:
        return "unknown"


def read_loadavg():
    try:
        return os.getloadavg()
    except (OSError, AttributeError):
        return (float("nan"),) * 3


def fs_type(path):
    # Best-effort: stat -f -c %T works on Linux coreutils.
    try:
        out = subprocess.run(["stat", "-f", "-c", "%T", path],
                             capture_output=True, text=True, timeout=10)
        return out.stdout.strip() or "unknown"
    except Exception:
        return "unknown"


def pct(sorted_vals, q):
    if not sorted_vals:
        return float("nan")
    if len(sorted_vals) == 1:
        return sorted_vals[0]
    idx = q * (len(sorted_vals) - 1)
    lo = int(idx)
    hi = min(lo + 1, len(sorted_vals) - 1)
    frac = idx - lo
    return sorted_vals[lo] * (1 - frac) + sorted_vals[hi] * frac


def build_cmd(tool, rush_bin, bazel_bin):
    if tool == "rush":
        return [rush_bin, "build", "//..."]
    if tool == "bazel":
        return [bazel_bin, "build", "//..."]
    if tool == "bazel-watchfs":
        return [bazel_bin, "build", "--watchfs", "//..."]
    raise ValueError(tool)


def run_once(cmd, cwd, timeout):
    t0 = time.perf_counter()
    r = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True,
                       timeout=timeout)
    dt = (time.perf_counter() - t0) * 1e3  # ms
    return dt, r.returncode, r.stderr


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--workspace", required=True)
    ap.add_argument("--tool", required=True,
                    choices=["rush", "bazel", "bazel-watchfs"])
    ap.add_argument("--iters", type=int, default=30)
    ap.add_argument("--rush-bin", default=os.environ.get("RUSH_BIN", "rush"))
    ap.add_argument("--bazel-bin", default=os.environ.get("BAZEL_BIN", "bazel"))
    ap.add_argument("--clean-timeout", type=int, default=1800)
    ap.add_argument("--noop-timeout", type=int, default=120)
    ap.add_argument("--no-clean", action="store_true",
                    help="skip the warm-up clean build")
    ap.add_argument("--json", default=None, help="write the result record here")
    args = ap.parse_args()

    ws = os.path.abspath(args.workspace)
    if not os.path.isdir(ws):
        sys.exit(f"workspace not found: {ws}")

    cmd = build_cmd(args.tool, args.rush_bin, args.bazel_bin)

    print(f"== rushbench no-op: tool={args.tool} ws={ws}")
    print(f"   cmd: {' '.join(cmd)}")
    print(f"   host={platform.node()} governor={read_governor()} "
          f"fs={fs_type(ws)} cpus={os.cpu_count()}")
    print(f"   loadavg(before)={read_loadavg()}")

    # 1) warm-up clean build
    if not args.no_clean:
        print("   warm-up clean build ...", flush=True)
        dt, rc, err = run_once(cmd, ws, args.clean_timeout)
        if rc != 0:
            print(err[-2000:], file=sys.stderr)
            sys.exit(f"warm-up build failed (rc={rc})")
        print(f"   warm-up done in {dt/1000:.2f}s")

    # 2) one extra no-op to settle caches, then N timed no-ops
    run_once(cmd, ws, args.noop_timeout)

    samples = []
    for i in range(args.iters):
        dt, rc, err = run_once(cmd, ws, args.noop_timeout)
        if rc != 0:
            print(err[-2000:], file=sys.stderr)
            sys.exit(f"no-op iter {i} failed (rc={rc})")
        samples.append(dt)
        print(f"   iter {i+1:>3}/{args.iters}: {dt:8.2f} ms", flush=True)

    s = sorted(samples)
    rec = {
        "tool": args.tool,
        "workspace": ws,
        "iters": args.iters,
        "host": platform.node(),
        "governor": read_governor(),
        "fs": fs_type(ws),
        "cpus": os.cpu_count(),
        "loadavg_after": read_loadavg(),
        "min_ms": s[0],
        "p50_ms": pct(s, 0.50),
        "p90_ms": pct(s, 0.90),
        "p95_ms": pct(s, 0.95),
        "p99_ms": pct(s, 0.99),
        "max_ms": s[-1],
        "mean_ms": statistics.fmean(s),
        "samples_ms": samples,
    }

    print("\n== distribution (ms) ==")
    for k in ("min_ms", "p50_ms", "p90_ms", "p95_ms", "p99_ms", "max_ms", "mean_ms"):
        print(f"   {k:<8} {rec[k]:10.2f}")

    if args.json:
        with open(args.json, "w") as f:
            json.dump(rec, f, indent=2)
        print(f"\n   wrote {args.json}")

    return rec


if __name__ == "__main__":
    main()
