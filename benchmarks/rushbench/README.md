<!--
Copyright (c) 2026 Luis Chamberlain. All rights reserved.
SPDX-License-Identifier: MIT
-->
# rushbench — synthetic warm-no-op benchmark workspaces

`rushbench` is the inner-loop metric for rush's "beat Bazel's warm no-op"
work (ChatGPT Pro's plan, memory `project_rush_crush_bazel_plan.md`). It
generates a **single C++ workspace that builds identically with both
`bazel build //...` and `rush build //...`** — no rush-specific dialect,
standard `cc_library` / `cc_binary` only.

## Why synthetic

A tiny real target is instant for both tools (no signal). A full
TensorFlow build takes minutes per iteration (too slow to profile a
no-op against). rushbench sits in the middle: it calibrates the
**manifest + freshness-ledger scale** (file-dep count) to TF's
(~18–21k deps) while keeping the clean build to *seconds*. That lets us
measure where a warm no-op spends its ~120ms without paying TF's clean
build cost on every iteration.

## Shape

```
<workspace>/
  WORKSPACE            workspace(name=...)
  MODULE.bazel         bazel_dep(name="rules_cc", version="0.2.17")
  .bazelversion        9.1.1   (bazelisk fetches it)
  .bazelrc             --enable_bzlmod, --spawn_strategy=local
  BUILD                rules_cc-loaded cc_binary //:bench depending on all P libs
  main.cc              declares + calls every lib_<i>_sum(), sums them
  pkg_0/ .. pkg_<P-1>/
    BUILD              one rules_cc-loaded cc_library //pkg_i:lib
    lib.cc             #includes all H headers, returns their constant sum
    hdr_0.h .. hdr_<H-1>.h   each defines one constexpr int
```

Each `.cc` `#include`s every header in its package and returns the sum of
their constants, so nothing is dead-code-eliminated and every header is a
real input dependency. The root binary references every library so the
whole graph is live.

`P` separate **packages** (not one glob'd package) is deliberate: it
forces P+1 BUILD files and P distinct `cc_library` targets through the
loader / analysis / manifest / ledger, which is what stresses rush's
per-invocation stat-walk the way TF's many packages do.

## The family

| family   | P   | H  | libraries | headers | .cc | BUILD | compile actions | file deps ≈ |
|----------|-----|----|-----------|---------|-----|-------|-----------------|-------------|
| wide_1k  | 64  | 16 | 64        | 1,024   | 64  | 65    | 65              | 1,158       |
| wide_5k  | 160 | 32 | 160       | 5,120   | 160 | 161   | 161             | 5,446       |
| wide_10k | 256 | 40 | 256       | 10,240  | 256 | 257   | 257             | 10,758      |
| wide_20k | 384 | 48 | 384       | 18,432  | 384 | 385   | 385             | 19,206      |
| wide_40k | 512 | 80 | 512       | 40,960  | 512 | 513   | 513             | 41,990      |

`wide_20k` is the **TF-scale calibration target** (~18k headers / ~19k
file deps). `file deps ≈` counts every header + .cc + BUILD + root config
files — the rough universe the manifest/ledger walk touches.

Print the table without writing anything:

```
python3 gen.py --print-only
```

## Generating

```bash
# one family member (default out: ./gen/wide_20k next to gen.py)
python3 gen.py wide_20k

# whole family under ./gen/
python3 gen.py --all

# custom scale anywhere
python3 gen.py --packages 384 --headers 48 --out /data/rushbench/wide_20k

# reproduce the published Bazel 6.5 legacy-native-cc shape
python3 gen.py wide_20k --bazel-version 6.5.0
```

The generator **prints the actual file/dep counts** for everything it
emits, so the numbers in a results doc are never hand-typed.

## Building (identical invocation for both tools)

```bash
cd <workspace>
bazel build //...                 # bazelisk -> Bazel 9.1.1 (.bazelversion)
rush  build //...                 # rush daemon
```

`.bazelversion=9.1.1` plus explicit `load("@rules_cc//cc:defs.bzl", ...)`
keeps the workspace compatible with modern Bazel's removed native C++ rules.
Rush resolves that same load through its built-in `rules_cc` facade, which
delegates to Rush's native C++ rule implementations.

## Warm-no-op measurement

After a clean build, a second `//...` build is a warm no-op for both
tools. Measure it with:

```bash
python3 ../noop_bench.py --workspace <workspace> --tool rush     --iters 30
python3 ../noop_bench.py --workspace <workspace> --tool bazel    --iters 30
python3 ../noop_bench.py --workspace <workspace> --tool bazel-watchfs --iters 30
```

`noop_bench.py` reports the **full distribution (p50/p90/p95/p99/min)**,
not a hidden mean, and records load average + CPU governor + FS. Bazel is
measured **both** in its default (scan) mode and with `--watchfs` (its
watcher mode, default OFF) — comparing only against default would be
misleading (ChatGPT Pro's correctness warning #3).

## rush phase profiling

To see *where* rush's no-op time goes (so we optimize the real
bottleneck, not vibes), run with phase profiling on:

```bash
RUSH_PROFILE_PHASES=/tmp/phases.json rush build //...
```

This emits a structured JSON breakdown of the no-op decision path:
client startup / IPC, request decode, command_semantics,
analysis_key_lookup, manifest_validate (and the *second* validate —
measured separately to expose the double-validate), ledger_validate
(with path count), link_plan_lookup, and response. See
`benchmarks/RUSHBENCH_NOOP_BASELINE.md` for the wide_20k breakdown.
