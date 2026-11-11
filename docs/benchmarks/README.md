# Compare build performance

Compare Rush with Bazel for TensorFlow and Modular. Compare Rush's Linux
replay path with GNU Make separately. Keep the build mode, workload and cache
state attached to every result.

## Rush against Buck2

Build identical generated C++ sources with each engine's native rules on one
Linux host with the same eight-CPU budget. The November 8 engine has lower medians in all eight
measured rows at 1,024 and 18,432 headers: clean compilation, unchanged
commands, source edits and header edits. Read [the 20-sample comparison](buck2.md)
for all samples, ranges, compiler counts and output checks. The larger edit
ranges overlap. Keep this synthetic result separate from the workloads below.

## TensorFlow build performance

Build TensorFlow 2.17 with Rush 2026-11-10 (`ad87da5`), eight CPUs and
eight jobs. The native Tensor consumer builds in 652.786 s and passes
one serialization/restoration/slice program. The framework target builds in
674.906 s and passes original-source and output-inventory checks.

Read the [procedure and exact data](tensorflow.md) for both selected-target
scopes, compiler/tool profiles and input/output checks. Use these single
shared-system intervals as correctness diagnostics. Collect matched Bazel
samples before assigning speed ratios. When Auto selects always-run execution for conditional includes or token
pasting, expect fresh execution for unchanged repeats.

## Modular build performance

Build the pinned CPU stdlib package with the same Mojo compiler, eight CPUs
and eight jobs per system. Compare Rush 2026-11-08 (`b059a25`) with
Rush 2026-11-10 (`ad87da5`) and Bazel 10.

| Host / operation | Rush 2026-11-08 | Rush 2026-11-10 | Bazel 10 |
| --- | ---: | ---: | ---: |
| Host C / Clear outputs and engine caches | 15.466 s | 34.652 s | 13.626 s |
| Host D / Clear outputs and engine caches | 15.944 s | 34.534 s | 14.108 s |
| Host C / Unchanged command | 3.169 ms | 617.544 ms | 65.840 ms |
| Host D / Unchanged command | 3.438 ms | 614.751 ms | 62.398 ms |
| Host C / Novel comment edit | 11.071 s | 11.711 s | 11.163 s |
| Host D / Novel comment edit | 11.744 s | 12.279 s | 11.771 s |

Read [every accepted sample and observed range](modular.md). All 96
commands pass work and package-byte checks. Use ten-sample unchanged-command
medians and three-sample cleared-output/edit medians. Retain fetched inputs,
analysis metadata and normal language-tool caches in the cleared-output
profile. Bazel has lower medians than Rush 2026-11-10 in all six host/mode
comparisons. Preserve input validation while reducing the recorded costs.

## Linux against GNU Make

Build the experimental Kbuild capture/replay example with Rush 2026-11-10
(`ad87da5`) on the recorded seeded x86-64 source. Both fresh captures,
version-16 snapshots and complete empty-cache native replays pass. Require
all 5,685 actions, zero cache hits, full output/hardlink parity against
both Make references and all 4,827 persisted command records.

| Host | Make-j8 control | Empty-cache native replay | Full output checks |
| --- | ---: | ---: | --- |
| Host C | 106.166 s | 849.059 s | Pass; 12,397 entries |
| Host D | 102.008 s | 851.792 s | Pass; 12,395 entries |

Use these single controller-observed intervals as correctness diagnostics.
Include capture, graph derivation and snapshot assembly when evaluating
preparation costs. Collect matched repeated builds before assigning speed
ratios. Read the [current procedure, phase costs and data](linux.md); keep
[earlier experiments](history/linux-20261109.md) with their own revisions.

## Interpret and reproduce the results

Separate compilation, populated-cache restore, novel edits, repeated edits
and unchanged commands. Do not compare the rows as if they perform the same
work. Freeze engine and upstream revisions, toolchains, configuration and
environment; retain all samples and require output parity.

Use the [measurement provenance](PROVENANCE.md) for source records and
missing reproduction details. Complete the release acceptance matrix before
extending these results to another configuration.
