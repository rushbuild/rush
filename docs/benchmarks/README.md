# Build performance reports

These are scoped historical measurements imported from the published
`rush-status` reports. None has been rerun for source baseline
`8c6bb55e39de7d78283a3e57b0b89df11ee773ea`. Keep the workload, cache state,
reference and sample count attached when quoting a result.

## TensorFlow build performance

[TensorFlow warm no-op](tensorflow.md) reports 30 unchanged commands for
`//tensorflow/core:framework`, with 18,522 retained dependencies. Its medians
are 9.3 ms for Rush, 64.2 ms for Bazel and 54.5 ms for Bazel with `--watchfs`.
It supplies no full-tree cold or incremental compile comparison.

## Modular build performance

[Modular build modes](modular.md) preserves the 4,175-target whole-tree and
945-target stdlib-slice tables, charts, runtime scope and corrections. The
novel-edit and repeated-edit measurements are separate. Read the original
cache-reset caveat before interpreting the cold-build rows.

## Add a measurement

Record exact candidate/reference identities, upstream pin, commands, flags,
environment and host class. State which caches and outputs are retained or
cleared, measure multiple counterbalanced samples, and require output parity.
Store sanitized samples and chart inputs beside the report. Keep private raw
logs and machine/account identities outside the public tree. Link the report
from the [status index](../status/README.md) in the same reviewed change.
