# Compare TensorFlow warm no-op latency

Build `//tensorflow/core:framework`, keep its inputs unchanged, and measure
the next build command. The measured manifest contains 18,522 dependencies.
Use the 30-sample median to compare command latency.

| Engine | Median | Samples |
| --- | ---: | ---: |
| Rush | **9.3 ms** | 30 |
| Bazel | 64.2 ms | 30 |
| Bazel with `--watchfs` | 54.5 ms | 30 |

![TensorFlow warm no-op command latency](graphs/bench-tensorflow-noop.svg)

The Bazel-to-Rush median ratios are **6.9x** for default Bazel and **5.9x**
for Bazel with watchfs in this experiment. No source changes and
no compilation or test execution enter this timing. Measure full builds,
incremental compilation and test runtime separately.

## Reproduce the comparison

Freeze both engines, the TensorFlow tree and configuration. Match the target,
environment and cache conditions, collect all 30 samples, and verify that
unchanged commands execute no unexpected actions. Report the distribution
and output checks. Do not derive p99 from a median.

Recover the complete sample series, exact timing-candidate identity and
toolchain/host record before claiming an independently reproduced result.
The [source record](PROVENANCE.md) identifies what is retained and what is
missing. Read [TensorFlow compatibility](../status/tensorflow.md) for build
and runtime acceptance scopes.
