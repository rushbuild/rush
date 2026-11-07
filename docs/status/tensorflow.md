# TensorFlow status

The source-release baseline `8c6bb55e39de7d78283a3e57b0b89df11ee773ea`
has not rerun the TensorFlow workload gates. The following are inherited
published results, preserved with their original scope in the
[historical TensorFlow report](history/tensorflow.md). They establish prior
work and reproduction targets; they do not certify this release candidate.

## Historical acceptance results

| Scope | Published result | What was checked |
| --- | --- | --- |
| TF 2.15 non-GPU `tf_cc_test` corpus | 463/463 | Build and execute the selected C++ test programs; keep three `requires-gpu` targets separate |
| Previously failing target re-sweep | 98/98 | Individual target builds; a separate `graph_constructor_test` run reported 56/56 runtime tests |
| Differential correctness cases | 8/8 | Clean/edit/incremental/restore sequences, graph/cache decisions and output digests |
| Compatibility micro-conformance | 16/16 | Match Bazel acceptance and rejection at loading, analysis, compile and link phases |
| Pinned native Python extensions | 2/2 | Build loadable objects, inspect initializer exports, import and call their smoke APIs |

These reports cover multiple historical engine candidates and configurations.
The detailed report identifies candidates where recorded, single-host versus
two-host coverage, uncompleted gates and known baseline failures. The CPU
corpus executes programs; building the repaired corpus does not substitute
for running those programs. Two extension imports do not certify a wheel.

## TensorFlow build performance

The [warm no-op report](../benchmarks/tensorflow.md) records 30 unchanged
commands for `//tensorflow/core:framework`, with 18,522 retained dependencies:
Rush 9.3 ms p50, Bazel 64.2 ms p50, and Bazel with `--watchfs` 54.5 ms p50.
This measures command latency for one target, not full-tree compile time or
test execution. It has not been repeated for the source-release baseline.

## Re-establish the release baseline

Freeze the candidate, upstream TensorFlow tree, configuration and toolchains.
Recover the exact case ledgers and procedures for each acceptance row, then
rerun them with the corresponding Bazel control. Require the recorded behavior
for every case and report the evaluated denominator and missing coverage.
If an old command, harness or ledger is unavailable, record that gap rather
than constructing a new corpus under the old count.

Revalidate the historical questions about compiler-discovered headers, Python
wheel breadth, production Bzlmod resolution and interoperable remote services
against the current code before opening issues. Preserve both closed-work
evidence and unresolved findings in the historical report.
