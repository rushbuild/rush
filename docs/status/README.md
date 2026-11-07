# Workload status

Keep Rush's status reports beside the implementation so a change and its
evidence can be reviewed in one pull request. This directory is the maintained
status location for the source release. The separate
[rush-status repository](https://github.com/rushbuild/rush-status) retains the
published historical record.

## Source-release validation

The source baseline is `8c6bb55e39de7d78283a3e57b0b89df11ee773ea`.
Its import checks cover metadata, ownership, documentation and maintenance
configuration. Full engine builds, workload suites and performance measurements
have **NOT RUN** against that baseline. Read the
[import validation record](../../maintenance/VALIDATION.md) before interpreting
the historical results below as evidence for a release.

| Workload | Report | Status of the source-release baseline |
| --- | --- | --- |
| TensorFlow | [Compatibility and acceptance gates](tensorflow.md) | Workload gates not rerun |
| Modular | [Build, artifact and runtime coverage](modular.md) | Workload gates not rerun |
| Performance | [Scoped benchmark reports](../benchmarks/README.md) | Measurements not rerun |

Historical results include a 463-test TensorFlow CPU corpus and a 4,175-target
Modular build ledger. Their denominators describe different checks. Neither
proves general compatibility, and a source-history reset does not renew them.
Each workload page links the original detailed report and its limitations.
The [import manifest](import-manifest.json) binds imported reports and graphs
to their published source revision and content hashes. Private Linux evidence
is maintained separately and is excluded from this import.

## Update a report with its code change

Record the exact Rush revision or tree, upstream revision, target/configuration,
command, toolchain and environment scope. Run each named gate and state its
assertion. Publish passing/evaluated counts and distinguish builds, program
execution, import smoke tests, graph comparisons and artifact parity. Keep
failed, skipped, unavailable and not-run results visible.

For timing, name cold compile, populated-cache restore, novel incremental edit,
repeated edit or warm no-op. Report all samples and the distribution, matched
reference commands and output parity. Update charts from the same data. Never
carry a historical result forward merely because its old TODO looks resolved
in the source.

File newly reproduced defects as GitHub Issues once the source project is
published. Keep [TODO.md](../../TODO.md) as the short priority list and record
task ownership in the maintenance queue. Historical open-work lists are inputs
to triage, not an automatically reopened backlog.

## Retire the separate tracker

Publish this directory with the source release, then publish the historical
tracker's relocation notice and archive that GitHub repository. Preserve its
commits, graphs, issues and links. Complete those steps in order so readers
always have an available destination. Until the replacement is public, the
historical tracker remains accessible and the archive transition is pending.
