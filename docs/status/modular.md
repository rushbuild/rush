# Build and test Modular

Use a pinned Modular tree, resolve its declared toolchains, and build the
same target ledger with Rush and Bazel. Compare artifact bytes before
counting a parity result. Execute tests with their runfiles and harness
contracts before counting a runtime pass.

## Read the coverage results

| Scope | Result | Acceptance boundary |
| --- | --- | --- |
| Whole-repository non-GPU ledger, pin `cc7155d854a0` | 4,175/4,175 targets build, including 1,063 Python targets | Build acceptance |
| Comparable Mojo library artifacts | 38/39 byte-identical | Retain the `builtin_kernels` difference |
| stdlib non-GPU test targets | 335/335 build; 264 runtime passes reported | Recover the case ledger to reconcile every built target |
| Comparable H100 GPU runtime tests | 222/222 Bazel-passing cases also pass under Rush | Keep the selected 287-case corpus and exclusions separate from 763 candidates |
| `generate_pycross_bzl_lock` | Generated lockfile bytes match | One pinned generator/workspace/toolchain configuration |

Do not combine whole-repository and stdlib denominators: they use different
pins and environments. The stdlib runtime summary identifies 23 failures
without accounting for every remaining built target. Recover the full ledger
before expanding its runtime claim.

## Compare build performance

Use the [Modular benchmark tables](../benchmarks/modular.md) to separate
populated-cache restoration, no-op commands, novel edits and repeated edits.
A novel one-file edit takes **26.0 s** under Rush versus **30.1 s** under
Bazel. Repeating an already-cached edit takes **0.15 s** versus roughly
**13 s**. Clear the actual engine caches before measuring an uncached build.

## Extend the acceptance scope

Freeze source revisions, toolchains and target ledgers. Execute FileCheck,
expected-crash, Python and lit tests with the same environment semantics as
the reference engine. Account for every selected, passed, failed, skipped
and excluded case. Reconcile artifact differences and toolchain discovery
with concrete reproductions.

Read [measurement provenance](../benchmarks/PROVENANCE.md) for source records
and missing coverage. Complete the release acceptance matrix before claiming
support beyond the measured pins and configurations.
