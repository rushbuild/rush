# Compare build performance

Compare Rush with Bazel for TensorFlow and Modular. Compare Rush's Linux
replay path with GNU Make separately. Keep the build mode, workload and cache
state attached to every result.

## Rush against Buck2

Build identical generated C++ sources with each engine's native rules on one
Linux host with the same eight-CPU budget. Rush has lower medians in all eight
measured rows at 1,024 and 18,432 headers: clean compilation, unchanged
commands, source edits and header edits. Read [the 20-sample comparison](buck2.md)
for all samples, ranges, compiler counts and output checks. The larger edit
ranges overlap. Keep this synthetic result separate from the workloads below.

## TensorFlow against Bazel

Build pristine TensorFlow 2.17's `//tensorflow/core:framework` with eight CPUs
and jobs. The November 9 checks fail during rule loading on both hosts with
both Rush versions; `JavaInfo` is undefined and no compiler runs. Bazel 6.5.0
completes on both hosts. Read [the build-check report](tensorflow.md) and
sanitized configuration data. Assign no speed ratio to failed builds.

Keep the imported 9.3 ms / 64.2 ms unchanged-command record separate. Its exact
timing candidate and sample series remain unrecovered; it does not validate
the current engine.

## Modular against Bazel

Build the pinned CPU stdlib package `//mojo/stdlib/std:std`. Compare Rush before
and after the incremental changes, then use the same Mojo compiler under
native Bazel. Each no-op median uses ten samples; clean and novel comment-edit
medians use three each. Use eight CPUs and eight requested jobs on both hosts.

| Host / operation | Rush before | Rush current | Bazel |
| --- | ---: | ---: | ---: |
| Host C / unchanged command | 3.62 ms | **2.66 ms** | 53.57 ms |
| Host D / unchanged command | 3.89 ms | **2.93 ms** | 56.12 ms |
| Host C / clear engine outputs and caches | 14.919 s | 14.783 s | 12.986 s |
| Host D / clear engine outputs and caches | 15.398 s | 15.360 s | 13.470 s |
| Host C / novel comment edit | 10.822 s | 10.803 s | 10.858 s |
| Host D / novel comment edit | 11.222 s | 11.208 s | 11.310 s |

Read [the complete report and ranges](modular.md), all 96 accepted samples,
compiler counts, output digests and the reproducible controller. Clean/edit
ranges overlap; use the lower no-op medians within this single-package scope.
Retain compiler/tool caches and analysis metadata in the clean-output profile.
Keep the separate 4,175-target and 945-target records attached to their original
pins and cache limitations.

## Linux against GNU Make

Read the separate three-pair record while fresh two-host snapshot checks run.
Compare its populated-cache Rush replay into a clean output root with a clean
Make build of the same Linux tree carrying Lorenzo Stoakes's v1 build-speedup
patches. Three alternating pairs give **40.24 s for Rush** and **50.92 s for
Make** at the median, with exact output parity. Treat this as a three-pair
experiment; the larger acceptance gate remains open. Read the
[Linux report](linux.md) for setup cost, empty-cache behavior and patch status.

## Interpret and reproduce the results

Separate compilation, populated-cache restore, novel edits, repeated edits
and unchanged commands. Do not compare the rows as if they perform the same
work. Freeze engine and upstream revisions, toolchains, configuration and
environment; retain all samples and require output parity.

Use the [measurement provenance](PROVENANCE.md) for source records and
missing reproduction details. Complete the release acceptance matrix before
extending these results to another configuration.
