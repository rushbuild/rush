# TensorFlow warm no-op command latency

This historical measurement is imported from the published status revision
`844d03c2c47f2fd2094ee5592015018238258290`. It has not been rerun for
source baseline `8c6bb55e39de7d78283a3e57b0b89df11ee773ea`.

![Historical TensorFlow warm no-op latency](graphs/bench-tensorflow-noop.svg)

| Engine | Published p50 | Samples | Scope |
| --- | ---: | ---: | --- |
| Rush | 9.3 ms | 30 | Unchanged `//tensorflow/core:framework`; 18,522 retained dependencies |
| Bazel | 64.2 ms | 30 | Same warm no-op target |
| Bazel `--watchfs` | 54.5 ms | 30 | Same warm no-op target with filesystem watching |

These are build-command medians. No source changed and no compilation or test
runtime is represented. The source report does not supply the complete sample
series, exact timing-candidate identity or toolchain/host record alongside
these rows. Recover those records before treating this as a reproducible
comparison or attributing it to the source-release baseline. Do not infer p99
or a whole-tree speedup from medians.

For a new report, freeze both engines and the upstream configuration, run the
same unchanged target with matched environment and cache conditions, retain
every sample, and confirm no unexpected actions executed. Publish the exact
command lines, output checks, distribution and scope. Measure cold builds,
novel edits and populated-cache restores as separate experiments.
