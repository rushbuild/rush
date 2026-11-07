# Compare build performance

Compare Rush with Bazel for TensorFlow and Modular. Compare Rush's Linux
replay path with GNU Make separately. Keep the build mode, workload and cache
state attached to every result.

## TensorFlow against Bazel

Measure unchanged command latency for `//tensorflow/core:framework`, with
18,522 retained dependencies. Across 30 samples, the medians are **9.3 ms
for Rush**, **64.2 ms for Bazel**, and **54.5 ms for Bazel with --watchfs**.
Read the [TensorFlow report](tensorflow.md) for the measured scope.

## Modular against Bazel

Use the same 4,175-target ledger at Modular pin `cc7155d`.

| Operation | Rush | Bazel | Measurement scope |
| --- | ---: | ---: | --- |
| Restore deleted outputs | **32 s** | 63 s | Populated disk caches |
| Leave inputs unchanged | **0.05 s** | 12.6 s | Warm no-op |
| Compile a novel one-file edit | **26.0 s** | 30.1 s | Distinct edit bytes per run |
| Repeat an already-cached edit | **0.15 s** | ~13 s | Reuse a known input state |
| Compile the whole tree | **362 s** | 611 s | One sample; cache-reset control needs revalidation |

Read the [Modular report](modular.md) for the stdlib slice, output checks,
runtime coverage and cache-reset requirement.

## Linux against GNU Make

Compare a populated-cache Rush replay into a clean output root with a clean
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
