# Build TensorFlow and run the native Tensor probe

Use the [recorded CPU profile](../benchmarks/tensorflow.md#reproduce-the-target-and-probe)
to build TensorFlow's framework with the normal `rush build` command. Add the
native Tensor probe to check program behavior as well as build completion.

The accepted 2026-11-10 checks use **Rush 2026-11-10 (`ad87da5`)**, commit
`ad87da5188201534d443c40481aba6b57ecc30d6`, tree
`87bb1112a4fe33fd2b3996ee65f4ab06788a5ebd`. Both cases use TensorFlow 2.17.0
commit `ad6d8cc177d0c868982e39e0823d0efbfb95f04c`, eight CPUs and eight jobs.

## Read the current results by their scope

| Case | Selected target | Build | Runtime and integrity checks |
| --- | --- | --- | --- |
| C | `//rush_runtime_probe:tensor_roundtrip`, with `//tensorflow/core:framework` as a dependency | Pass | One native Tensor program passes; original sources match the archive; selected compiler/SDK inputs and linked outputs pass before/after file checks |
| D | `//tensorflow/core:framework` | Pass | Original-source audit passes; the framework subtree contains 57 objects and 46 archives; runtime was not run |

The C program fills a float Tensor of shape `[2, 3]` with values 1 through 6,
serializes and restores it through `TensorProto`, and checks a `[1, 3]` row
slice containing 4 through 6. It exits successfully and prints
`TF_TENSOR_ROUNDTRIP_SLICE_OK`. Compare the selected compiler/toolchain files before and after the build.
Then compare the selected source, toolchain and native-output files before
and after the Tensor program. Its output checks cover 234 native archives
and 645 unique thin-member objects. Keep that denominator attached to the
consumer build and its runtime checks.

Both cases preserve the original 31,981 source files and two symlinks,
including file modes. C adds two declared probe files plus the case profile
and three execution-control files. D adds the profile and those three
control files. Keep D's 57-object/46-archive inventory scoped to the
`tensorflow/core/framework` output subtree; it is not the C runtime
bookend denominator.

Both validation systems also pass all 21 source, component and release checks.
Read the [sanitized result data](../benchmarks/data/tensorflow-native-20261110.json)
for exact source, binary, probe and evidence digests.

## Use the default compiler policy

Leave `RUSH_CPP_DISCOVERY` unset. C records `ConditionalIncludes` at `@fft2d//:fft2d`.
D records `ConditionalIncludes` at `@fft2d//:fft2d`. Rush analyzes the same
request afresh and runs the whole selected invocation locally with reuse
disabled. Auto also accepts the supported `TokenPasting` reason. Expect
header guards with nested includes to use this path in many header-heavy
builds. Treat compiler dependency records as observations; validate the
required input closure separately before admitting reusable results.

Run an unchanged repeat expecting the selected actions to execute afresh
when this path is needed. Qualify cached full-framework builds and unchanged
command latency separately. Supported literal builds and pure Mojo builds
keep their existing reuse behavior. Use local Linux execution with the
scheduler disabled for this always-run path. `rush test` keeps strict literal
analysis for its nested builds.

## Reproduce the native program check

Copy the published [probe source](../../benchmarks/workloads/tensorflow-probe/probe.cc)
and [BUILD template](../../benchmarks/workloads/tensorflow-probe/BUILD.bazel.in)
into the TensorFlow workspace's `rush_runtime_probe` package. Preserve their
bytes and use the [build and runtime procedure](../benchmarks/tensorflow.md#reproduce-the-target-and-probe).
Require the successful build, exact runtime token and unchanged source,
SDK/toolchain and native-output inputs for the scope being checked.

## Keep the historical records separate

The imported **TF 2.15 non-GPU C++ runtime record is 463/463**. That corpus
was not rerun for this engine; the current C result is one Tensor program.
Keep the other imported target, extension and compatibility counts within
their original [provenance](../benchmarks/PROVENANCE.md).

The separate **9.3 ms unchanged-command record** has an unrecovered exact
timing candidate and complete sample series. Read it as a historical record
in the [timing report](../benchmarks/tensorflow.md#keep-the-imported-timing-record-historical).
The current build intervals are single shared-host correctness diagnostics
for different selected targets. Run a matched comparison before drawing a
Bazel speed ratio or cached-build conclusion.
