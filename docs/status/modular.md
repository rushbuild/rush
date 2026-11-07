# Modular status

The source-release baseline `8c6bb55e39de7d78283a3e57b0b89df11ee773ea`
has not rerun Modular's workload gates. The
[historical Modular report](history/modular.md) preserves the published pins,
test scopes, reproduction example and unresolved findings.

## Historical coverage

| Scope | Published result | Limit |
| --- | --- | --- |
| Whole-repository non-GPU build ledger, pin `cc7155d854a0` | 4,175/4,175 targets build, including 1,063 Python targets | Build acceptance, not execution of every test |
| Comparable Mojo library artifacts | 38/39 byte-identical | One `builtin_kernels` artifact differed |
| stdlib non-GPU test target build | 335/335 build | Reported runtime success covered 264 tests; the full runtime census needs its original ledger |
| Comparable H100 GPU kernel runtime tests | 222/222 agree on passing outcomes | The selected 287-case corpus came from 763 candidates; shared failures and exclusions remain visible in the benchmark report |
| `generate_pycross_bzl_lock` | Generated lockfile bytes matched | One pinned generator/workspace/toolchain configuration |

The stdlib and whole-repository reports use different pins and environments.
The stdlib report names 23 failing tests but does not reconcile every remaining
case with the 335 built targets in its summary. Recover the case ledger before
claiming a complete runtime denominator; do not infer that all 335 passed.

## Modular build performance

Read the [Modular benchmark report](../benchmarks/modular.md) for its original
data, plots and methodology corrections. The corrected whole-tree comparison
separates a novel one-file edit (Rush 26.0 s versus Bazel 30.1 s) from repeating
an edit whose bytes were already cached (Rush 0.15 s versus roughly 13 s).
The earlier 14.6 s value mixed those situations and must not be reused as the
novel-edit result. Cold results also carry the published cache-reset caveat.

## Re-establish the release baseline

Freeze the source candidate, upstream pin, toolchains and target ledger. Build
the exact Bazel-accepted set, compare artifacts, and execute tests with their
actual FileCheck, expected-crash, runfiles and environment contracts. Account
for every selected, passed, failed, skipped and excluded case. Rerun timings
with distinct content per novel edit and explicitly controlled cache state.

Revalidate the historical Python runtime corpus, `builtin_kernels` artifact
difference, lit runner, test environments and toolchain discovery before
opening current issues. A historical open item is not proof of a current bug.
