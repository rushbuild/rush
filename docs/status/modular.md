# Build and test Modular

Use a pinned Modular tree, resolve its declared toolchains, and build the
same target ledger with Rush and Bazel. Compare artifact bytes before
counting a parity result. Execute tests with their runfiles and harness
contracts before counting a runtime pass.

## Qualify the current package profile

Build `//mojo/stdlib/std:std` at Modular source `4e87649567` with
**Rush 2026-11-10 (`ad87da5`)**. The [two-host comparison](../benchmarks/modular.md)
passes all 96 compiler-count and output-byte checks across Rush 2026-11-08,
Rush 2026-11-10 and Bazel 10. All 36 measured compiles and 60 unchanged
commands produce the same package bytes.

Bazel has lower medians than Rush 2026-11-10 in all six host/mode
comparisons.
Rush 2026-11-10 takes 34.652 / 34.534 s for cleared outputs and
617.544 / 614.751 ms for unchanged commands on C / D. Inspect every sample
and range in the report. Profile and reduce analysis and validation costs
while preserving the dependency and artifact checks. Qualify whole-project
builds and runtime programs separately.

## Read the imported coverage records

Keep these results attached to their original source pins, toolchains and
target ledgers. Re-run the named cases to extend them to Rush 2026-11-10.

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

## Extend the acceptance scope

Freeze source revisions, toolchains and target ledgers. Execute FileCheck,
expected-crash, Python and lit tests with the same environment semantics as
the reference engine. Account for every selected, passed, failed, skipped
and excluded case. Reconcile artifact differences and toolchain discovery
with concrete reproductions.

Read [measurement provenance](../benchmarks/PROVENANCE.md) for source records
and missing coverage. Complete the release acceptance matrix before claiming
support beyond the measured pins and configurations.
