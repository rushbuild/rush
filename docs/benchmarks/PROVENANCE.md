# Measurement provenance

Use this record to distinguish a measured workload from validation of a
particular release candidate. The release presents project measurements;
the complete workload acceptance matrix for source baseline
`8c6bb55e39de7d78283a3e57b0b89df11ee773ea` remains open. This documentation
change runs no new build or performance experiment.

## TensorFlow and Modular

The [import manifest](../status/import-manifest.json) identifies source
reports from published status revision
`844d03c2c47f2fd2094ee5592015018238258290`, with original and destination
hashes. Read the detailed [TensorFlow](../status/history/tensorflow.md) and
[Modular](../status/history/modular.md) records for candidate-specific checks.
The archived tracker retains the full reporting and correction history.

The TensorFlow no-op summary supplies the target, retained dependency count,
30 samples and medians. It does not include the complete sample series,
exact timing-candidate identity or toolchain/host record alongside those rows.
Recover those inputs before claiming independent reproduction.

The Modular report supplies the upstream pin, target counts, host class,
sample convention and cache modes. Its cold-run cache clearing needs
revalidation. Use distinct bytes for novel-edit samples and retain repeat-edit
measurements separately. The stdlib runtime summary does not account for
every built target; recover the case ledger before expanding its denominator.

## Linux

The [public Linux record](https://github.com/rushbuild/rush-status/blob/26e6a51ee374c5cea6f86d33c7edf3e20923dabc/status/linux.md)
supplies all three timing pairs, kernel identity, parity counts, preparation
costs and incomplete acceptance gates. Keep raw private experiment material
outside this repository. The benchmark is tied to the stated v1-patched tree,
not an arbitrary newer kernel.

The upstream check uses Linus's repository at
`a41e364995bbcfa627418a70fe913c35b7510728` on 2026-11-07. Its ancestry
contains merge `997208ce9f43f01febac956cf9aeffe89945e3c1`. The linked
mainline commits establish partial integration of Lorenzo's speedup work,
not a patch-by-patch audit of all submitted revisions.

## Publish another result

Record candidate and reference revisions, upstream inputs, exact commands,
toolchain and environment scope, cache reset procedure, every sample and
output checks. Report failures, skips and missing coverage explicitly.
Update the report and graph data in one reviewed change.
