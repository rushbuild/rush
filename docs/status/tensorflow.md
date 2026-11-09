# Build and test TensorFlow

Use TensorFlow's unmodified upstream sources. Compare the same targets and
configuration with Bazel, then check artifacts and program behavior. Keep each
acceptance scope distinct.

## Qualify fresh builds of the current release

Run the [TensorFlow 2.17 framework build checks](../benchmarks/tensorflow.md)
on both validation hosts. Rush before (`b003cc3`) and after (`b059a25`,
published in `f9d97fe`) stops during rule loading on undefined `JavaInfo`;
neither version reaches compilation. Bazel 6.5.0 completes the same target
with the selected CPU configuration on both hosts.

Repair the predeclared environment and native Java provider exports, then
repeat fresh builds before measuring latency. Count these as build attempts,
not a rerun of the TF 2.15 runtime corpus below. Keep the imported acceptance
counts attached to their original candidates and ledgers.

## Read the acceptance results

| Scope | Result | Apply this pass condition |
| --- | --- | --- |
| TF 2.15 non-GPU C++ test corpus | 463/463 | Build and launch every selected `tf_cc_test`; require successful program exit |
| Repaired target corpus | 98/98 | Build every named target; record the separate 56/56 `graph_constructor_test` runtime result separately |
| Differential correctness | 8/8 | Compare clean/edit/incremental/restore graphs, cache decisions and output digests |
| Compatibility micro-conformance | 16/16 | Match Bazel acceptance and rejection at loading, analysis, compile and link phases |
| Pinned native Python extensions | 2/2 | Build loadable objects, inspect initializer exports, import and call each smoke API |

Keep the three `requires-gpu` targets outside the non-GPU corpus. Count
executed programs for the CPU runtime gate; count builds for the repaired
target gate. Certify a Python wheel with a dedicated wheel procedure rather
than extending the two-extension denominator.

The extension targets are
`//tensorflow/python/autograph/impl/testing:pybind_for_testing` and
`//tensorflow/python/platform:_pywrap_tf2`. Inspect their `PyInit_*`
exports, import with the pinned interpreter and require their API results.

## Compare build-command latency

Complete fresh-build qualification first. The separate 30-sample unchanged
command record reports 9.3 ms for Rush, 64.2 ms for Bazel and 54.5 ms with
Bazel watchfs, but its exact timing candidate and complete sample series
remain unrecovered. Use [the timing report](../benchmarks/tensorflow.md) to
keep that record separate from current-release build failures.

## Extend the acceptance scope

Freeze the candidate, upstream tree, configuration and toolchains. Recover
the exact case ledgers and run each procedure with its Bazel control.
Publish evaluated denominators and missing coverage. If a ledger or harness
is unavailable, identify the gap before claiming its count.

Check compiler-discovered headers, Python wheel breadth, production Bzlmod
resolution and interoperable remote services against the code and open
reproducible issues. Use [measurement provenance](../benchmarks/PROVENANCE.md)
for candidate-specific records, completed checks and release-wide validation
that remains open.
