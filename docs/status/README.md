# Evaluate the experimental release

Build the named upstream tree, compare its artifacts with the reference
engine, and run the selected test programs. Read each result within its
target, configuration, toolchain and cache scope.

| Workload | Report | Measurement or acceptance scope |
| --- | --- | --- |
| TensorFlow | [Build and runtime gates](tensorflow.md) | Current TF 2.17 rule-loading failure on both hosts; separate TF 2.15 runtime and import records |
| Modular | [Build and artifact coverage](modular.md) | Current CPU package: 96/96 compiler/output checks; separate multi-target and runtime records |
| Linux kernel | [Replay against GNU Make](../benchmarks/linux.md) | Make/capture/derivation pass both; assembly refuses group credentials; mapped-profile retry in progress |
| Remote execution | [REAPI and worker checks](../benchmarks/remote-execution.md) | File actions and native C++ in both host directions; CUDA readback; HIP failure retained |
| Performance | [Compare build modes](../benchmarks/README.md) | Keep compile, restore, novel edit, repeated edit and no-op separate |

Use the [measurement provenance](../benchmarks/PROVENANCE.md) to locate
candidate identities, reporting records and missing reproduction inputs.
Complete the release acceptance matrix before extending these results to
another configuration. Read the [source-import checks](../../maintenance/VALIDATION.md)
for the validation performed on the imported code baseline.

## Update status with the implementation

Record the exact Rush revision or tree, upstream revision, configuration,
command, toolchain and environment scope. Run each named gate and state its
assertion. Publish passing/evaluated counts. Distinguish successful builds,
program execution, import smoke checks, graph comparisons and artifact parity.

For timing, identify the build mode and cache state. Retain every sample,
compare matching reference commands, and require output parity. Publish
failed, skipped, unavailable and not-run cases. Update charts and tables
from the same data.

File reproducible defects in the source project's GitHub Issues. Keep
[TODO.md](../../TODO.md) as the short priority list and record task ownership
in the maintenance queue. Use the [archived tracker](https://github.com/rushbuild/rush-status)
for reporting history; maintain release status here.
