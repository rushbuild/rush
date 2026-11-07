# Build and test TensorFlow

Use TensorFlow's unmodified upstream sources. Compare the same targets and
configuration with Bazel, then check artifacts and program behavior. Keep each
acceptance scope distinct.

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

Run unchanged `//tensorflow/core:framework` commands over the
18,522-dependency manifest. Across 30 samples, Rush's median is **9.3 ms**,
Bazel's is **64.2 ms**, and Bazel with `--watchfs` is **54.5 ms**.
Read the [timing report](../benchmarks/tensorflow.md) for its exact scope.

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
