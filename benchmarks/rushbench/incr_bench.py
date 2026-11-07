#!/usr/bin/env python3
# Copyright (c) 2026 Luis Chamberlain. All rights reserved.
# SPDX-License-Identifier: MIT
"""
rushbench INCREMENTAL measurement harness (Phase 3 profiling).

Where the no-op harness (noop_bench.py) measures the cost of proving NOTHING
changed, this one measures the cost of rebuilding after a SINGLE-FILE edit --
the incremental hot path that ChatGPT Pro flagged as likely compiler/linker-
bound rather than engine-bound. The job here is to PROVE which it is before we
build anything.

For each mutation case it:
  1. ensures a warm built tree (clean build once, then a settling no-op)
  2. applies the mutation (edit one .cc, touch-without-content-change, or no-op)
  3. times the rebuild
  4. for rush: parses RUSH_REUSE_TIMING (execute_analyzed ms = PASS-B wavefront
     wall) and counts how many compile/link SPAWNS actually fired vs how many
     targets the wavefront VISITED -- the action-count question.
  5. when --action-dir is set for rush: records RUSH_ACTION_TIMING JSONL and
     summarizes per-action wall time by mnemonic.
  6. for bazel: counts actions via the `(N processes)` build summary line.

The key honest output per case:
  rush:  wall, execute_analyzed(PASS-B) ms, targets_visited, spawns_fired
  bazel: wall, processes (= actions run)

Mutation cases (subset of the plan's matrix, the ones that separate engine
overhead from compiler cost):
  noop              no change            -> both should run 0 actions
  touch_nochange    rewrite same bytes   -> CORRECTNESS: digest unchanged ->
                                            both should run 0 actions
  edit_src          change returned value -> rebuild exactly 1 obj + 1 link
  edit_hdr_private  change one private hdr constant -> rebuild exactly
                                            1 obj + 1 link
  edit_src_comment  append source comment -> compile output should be
                                            byte-identical
  edit_hdr_private_comment append header comment -> compile output should be
                                            byte-identical
  edit_hdr_common   (n/a: headers are per-pkg in rushbench, same as private)

Usage:
  incr_bench.py --workspace DIR --tool {rush,bazel} [--iters 10]
                [--rush-bin PATH] [--bazel-bin PATH]
                [--cases noop,touch_nochange,edit_src,edit_hdr_private]
                [--pkg N] [--daemon] [--json OUT.json]
                [--action-dir DIR]
"""

import argparse
import collections
import json
import os
import platform
import re
import statistics
import subprocess
import sys
import time

RE_PROCS = re.compile(r"\((\d+)\s+process(?:es)?\)")
RE_EXEC = re.compile(r"execute_analyzed=([\d.]+)ms")
RE_DEPS = re.compile(r"deps=(\d+)")
RE_BUILDING = re.compile(r"^🔨 Building ", re.M)
# rush prints a compile spawn as a gcc/g++/cc1plus invocation; the invocation
# report logs legacy invocations. We count actual child-process compiles by the
# "Compiling" / "Linking" lines rush emits, plus a fallback on cc1plus spawns.
RE_COMPILE = re.compile(r"(Compiling|compile_source|Running.*g\+\+|cc1plus|🔧|Linking|relink)", re.I)


def read_governor():
    try:
        with open("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor") as f:
            return f.read().strip()
    except OSError:
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


def run(cmd, cwd, timeout, env=None):
    e = dict(os.environ)
    if env:
        e.update(env)
    t0 = time.perf_counter()
    r = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True,
                       timeout=timeout, env=e)
    dt = (time.perf_counter() - t0) * 1e3
    return dt, r.returncode, r.stdout, r.stderr


def mutate(ws, case, pkg):
    """Apply the mutation; return a revert closure."""
    cc = os.path.join(ws, f"pkg_{pkg}", "lib.cc")
    hdr = os.path.join(ws, f"pkg_{pkg}", "hdr_0.h")

    if case == "noop":
        return lambda: None

    if case == "touch_nochange":
        # Rewrite the exact same bytes (mtime bumps, content digest identical).
        with open(cc) as f:
            orig = f.read()
        with open(cc, "w") as f:
            f.write(orig)
        os.utime(cc, None)
        return lambda: None  # content already identical; nothing to revert

    if case == "edit_src":
        with open(cc) as f:
            orig = f.read()
        delta = (time.time_ns() % 1_000_003) + 1
        edited, n = re.subn(
            r"(return\s+)([^;\n]+)(;)",
            lambda m: f"{m.group(1)}({m.group(2)}) + {delta}{m.group(3)}",
            orig,
            count=1,
        )
        if n != 1:
            raise ValueError(f"could not find return expression in {cc}")
        with open(cc, "w") as f:
            f.write(edited)
        def revert():
            with open(cc, "w") as f:
                f.write(orig)
        return revert

    if case == "edit_src_comment":
        with open(cc) as f:
            orig = f.read()
        # Append a unique harmless comment: action input changes, but compiler
        # output should stay byte-identical.
        token = f"\n// rushbench-incr-edit {time.time_ns()}\n"
        with open(cc, "w") as f:
            f.write(orig + token)
        def revert():
            with open(cc, "w") as f:
                f.write(orig)
        return revert

    if case == "edit_hdr_private":
        with open(hdr) as f:
            orig = f.read()
        delta = (time.time_ns() % 1_000_003) + 1
        edited, n = re.subn(
            r"(constexpr\s+int\s+kHdr0\s*=\s*)(\d+)(;)",
            lambda m: f"{m.group(1)}{int(m.group(2)) + delta}{m.group(3)}",
            orig,
            count=1,
        )
        if n != 1:
            raise ValueError(f"could not find kHdr0 constant in {hdr}")
        with open(hdr, "w") as f:
            f.write(edited)
        def revert():
            with open(hdr, "w") as f:
                f.write(orig)
        return revert

    if case == "edit_hdr_private_comment":
        with open(hdr) as f:
            orig = f.read()
        token = f"\n// rushbench-incr-hdr {time.time_ns()}\n"
        with open(hdr, "w") as f:
            f.write(orig + token)
        def revert():
            with open(hdr, "w") as f:
                f.write(orig)
        return revert

    raise ValueError(case)


def analyze_rush(stdout, stderr):
    out = stdout + "\n" + stderr
    m = RE_EXEC.search(out)
    exec_ms = float(m.group(1)) if m else None
    visited = len(RE_BUILDING.findall(out))
    # Count real compiles/links. rush's invocation report + log lines.
    spawns = len(RE_COMPILE.findall(out))
    return {"execute_analyzed_ms": exec_ms, "targets_visited": visited,
            "compile_spawns": spawns}


def analyze_bazel(stdout, stderr):
    out = stdout + "\n" + stderr
    m = RE_PROCS.search(out)
    procs = int(m.group(1)) if m else None
    return {"processes": procs}


def load_action_timings(path):
    if not path or not os.path.exists(path):
        return []
    records = []
    with open(path) as f:
        for lineno, line in enumerate(f, 1):
            line = line.strip()
            if not line:
                continue
            try:
                records.append(json.loads(line))
            except json.JSONDecodeError as e:
                raise ValueError(f"{path}:{lineno}: invalid JSONL: {e}") from e
    return records


def summarize_action_timings(records):
    by_mnemonic = collections.defaultdict(float)
    executed_by_mnemonic = collections.defaultdict(int)
    cached_by_mnemonic = collections.defaultdict(int)
    total = 0.0
    for r in records:
        mnemonic = r.get("mnemonic") or "unknown"
        wall = float(r.get("wall_ms") or 0.0)
        total += wall
        by_mnemonic[mnemonic] += wall
        if r.get("cached"):
            cached_by_mnemonic[mnemonic] += 1
        else:
            executed_by_mnemonic[mnemonic] += 1
    slowest = sorted(records, key=lambda r: float(r.get("wall_ms") or 0.0),
                     reverse=True)[:5]
    executed_count = sum(executed_by_mnemonic.values())
    cached_count = sum(cached_by_mnemonic.values())
    return {
        "count": len(records),
        "executed_count": executed_count,
        "cached_count": cached_count,
        "total_wall_ms": total,
        "by_mnemonic_ms": dict(sorted(by_mnemonic.items())),
        "executed_by_mnemonic": dict(sorted(executed_by_mnemonic.items())),
        "cached_by_mnemonic": dict(sorted(cached_by_mnemonic.items())),
        "slowest": slowest,
    }


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--workspace", required=True)
    ap.add_argument("--tool", required=True, choices=["rush", "bazel"])
    ap.add_argument("--iters", type=int, default=10)
    ap.add_argument("--rush-bin", default=os.environ.get("RUSH_BIN", "rush"))
    ap.add_argument("--bazel-bin", default=os.environ.get("BAZEL_BIN", "bazel"))
    ap.add_argument("--cases",
                    default="noop,touch_nochange,edit_src,edit_hdr_private")
    ap.add_argument("--pkg", type=int, default=0)
    ap.add_argument("--daemon", action="store_true",
                    help="rush: route through --daemon=auto (assumes a live daemon)")
    ap.add_argument("--clean-timeout", type=int, default=1800)
    ap.add_argument("--build-timeout", type=int, default=300)
    ap.add_argument("--json", default=None)
    ap.add_argument("--phase-dir", default=None,
                    help="rush: dir for per-iter RUSH_PROFILE_PHASES json")
    ap.add_argument("--action-dir", default=None,
                    help="rush: dir for per-iter RUSH_ACTION_TIMING jsonl")
    args = ap.parse_args()

    ws = os.path.abspath(args.workspace)
    if not os.path.isdir(ws):
        sys.exit(f"workspace not found: {ws}")
    cases = [c for c in args.cases.split(",") if c]

    if args.tool == "rush":
        if args.daemon:
            base = [args.rush_bin, "build", "--daemon=auto", "//..."]
        else:
            base = [args.rush_bin, "build", "//..."]
        analyze = analyze_rush
    else:
        base = [args.bazel_bin, "build", "//..."]
        analyze = analyze_bazel

    print(f"== rushbench incr: tool={args.tool} ws={ws} daemon={args.daemon}")
    print(f"   cmd: {' '.join(base)}")
    print(f"   host={platform.node()} governor={read_governor()} "
          f"cpus={os.cpu_count()} pkg={args.pkg}")

    # Warm clean build + a settling no-op so RUSH_REUSE_TIMING reflects warm.
    print("   warm-up clean build ...", flush=True)
    env = {}
    dt, rc, so, se = run(base, ws, args.clean_timeout,
                         {"RUSH_REUSE_TIMING": "1"})
    if rc != 0:
        print(se[-3000:], file=sys.stderr)
        sys.exit(f"warm-up failed rc={rc}")
    print(f"   warm-up clean build: {dt/1000:.2f}s")
    run(base, ws, args.build_timeout, {"RUSH_REUSE_TIMING": "1"})

    results = {}
    for case in cases:
        per = []
        for i in range(args.iters):
            revert = mutate(ws, case, args.pkg)
            penv = {"RUSH_REUSE_TIMING": "1"}
            if args.phase_dir:
                os.makedirs(args.phase_dir, exist_ok=True)
                penv["RUSH_PROFILE_PHASES"] = os.path.join(
                    args.phase_dir, f"{case}_{i}.json")
            action_path = None
            if args.action_dir:
                os.makedirs(args.action_dir, exist_ok=True)
                action_path = os.path.join(args.action_dir, f"{case}_{i}.jsonl")
                try:
                    os.remove(action_path)
                except FileNotFoundError:
                    pass
                penv["RUSH_ACTION_TIMING"] = action_path
            dt, rc, so, se = run(base, ws, args.build_timeout, penv)
            if rc != 0:
                print(se[-3000:], file=sys.stderr)
                sys.exit(f"case {case} iter {i} failed rc={rc}")
            rec = analyze(so, se)
            if args.tool == "rush" and action_path:
                action_records = load_action_timings(action_path)
                rec["action_timing_path"] = action_path
                rec["action_timing"] = summarize_action_timings(action_records)
            rec["wall_ms"] = dt
            per.append(rec)
            revert()
            # Re-settle so the NEXT iter starts from a fully-built tree
            # (the revert is itself an edit for edit_* cases).
            run(base, ws, args.build_timeout, {"RUSH_REUSE_TIMING": "1"})
            extra = ""
            if args.tool == "rush" and rec.get("execute_analyzed_ms") is not None:
                extra = (f" pass-b={rec['execute_analyzed_ms']:.1f}ms "
                         f"visited={rec['targets_visited']} "
                         f"spawns={rec['compile_spawns']}")
            if args.tool == "rush" and rec.get("action_timing"):
                at = rec["action_timing"]
                by = at.get("by_mnemonic_ms", {})
                hot = ", ".join(f"{k}={v:.1f}ms"
                                for k, v in sorted(by.items()))
                extra += (f" action_ms={at['total_wall_ms']:.1f} "
                          f"actions={at['count']}")
                if hot:
                    extra += f" [{hot}]"
                extra += (f" exec={at.get('executed_count', 0)} "
                          f"cached={at.get('cached_count', 0)}")
            elif args.tool == "bazel":
                extra = f" procs={rec.get('processes')}"
            print(f"   {case:<18} iter {i+1:>2}/{args.iters}: "
                  f"{dt:8.1f} ms{extra}", flush=True)

        walls = sorted(r["wall_ms"] for r in per)
        summary = {
            "case": case,
            "iters": args.iters,
            "wall_min_ms": walls[0],
            "wall_p50_ms": pct(walls, 0.50),
            "wall_p90_ms": pct(walls, 0.90),
            "wall_max_ms": walls[-1],
            "wall_mean_ms": statistics.fmean(walls),
            "samples": per,
        }
        if args.tool == "rush":
            ev = [r["execute_analyzed_ms"] for r in per
                  if r["execute_analyzed_ms"] is not None]
            if ev:
                summary["passb_p50_ms"] = pct(sorted(ev), 0.50)
            vis = [r["targets_visited"] for r in per]
            sp = [r["compile_spawns"] for r in per]
            summary["targets_visited_median"] = statistics.median(vis)
            summary["compile_spawns_median"] = statistics.median(sp)
            action_totals = [
                r["action_timing"]["total_wall_ms"] for r in per
                if r.get("action_timing")
            ]
            if action_totals:
                summary["action_wall_p50_ms"] = pct(sorted(action_totals), 0.50)
                summary["action_count_median"] = statistics.median(
                    r["action_timing"]["count"] for r in per
                    if r.get("action_timing")
                )
                summary["action_executed_median"] = statistics.median(
                    r["action_timing"].get("executed_count", 0) for r in per
                    if r.get("action_timing")
                )
                summary["action_cached_median"] = statistics.median(
                    r["action_timing"].get("cached_count", 0) for r in per
                    if r.get("action_timing")
                )
                mnemonics = sorted({
                    m
                    for r in per if r.get("action_timing")
                    for m in r["action_timing"]["by_mnemonic_ms"]
                })
                summary["action_by_mnemonic_p50_ms"] = {
                    m: pct(sorted(
                        r["action_timing"]["by_mnemonic_ms"].get(m, 0.0)
                        for r in per if r.get("action_timing")
                    ), 0.50)
                    for m in mnemonics
                }
        else:
            pr = [r["processes"] for r in per if r["processes"] is not None]
            if pr:
                summary["processes_median"] = statistics.median(pr)
        results[case] = summary

    print("\n== incremental summary ==")
    for case, s in results.items():
        line = (f"   {case:<18} wall p50={s['wall_p50_ms']:8.1f}ms "
                f"min={s['wall_min_ms']:8.1f}ms")
        if args.tool == "rush":
            line += (f" pass-b={s.get('passb_p50_ms', float('nan')):7.1f}ms "
                     f"visited={s.get('targets_visited_median')} "
                     f"spawns={s.get('compile_spawns_median')}")
            if "action_wall_p50_ms" in s:
                line += (f" action_p50={s['action_wall_p50_ms']:7.1f}ms "
                         f"actions={s.get('action_count_median')}")
                by = s.get("action_by_mnemonic_p50_ms", {})
                if by:
                    hot = ", ".join(f"{k}={v:.1f}ms"
                                    for k, v in sorted(by.items()))
                    line += f" [{hot}]"
                line += (f" exec={s.get('action_executed_median')} "
                         f"cached={s.get('action_cached_median')}")
        else:
            line += f" procs={s.get('processes_median')}"
        print(line)

    rec = {
        "tool": args.tool, "workspace": ws, "daemon": args.daemon,
        "host": platform.node(), "governor": read_governor(),
        "cpus": os.cpu_count(), "pkg": args.pkg, "cases": results,
    }
    if args.json:
        with open(args.json, "w") as f:
            json.dump(rec, f, indent=2)
        print(f"\n   wrote {args.json}")
    return rec


if __name__ == "__main__":
    main()
