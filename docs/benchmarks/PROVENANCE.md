# Measurement provenance

Use this record to distinguish a measured workload from validation of a
particular release candidate. The release presents project measurements;
the complete workload acceptance matrix for source baseline
`8c6bb55e39de7d78283a3e57b0b89df11ee773ea` remains open. The November 9 checks
below exercise the published incremental engine without changing its source.

## November 9 workload checks

Compare Rush `b003cc3a3f068449510ba69e9ef1070c4598cc4a` with engine
`b059a257874270dbb17ebebeb2bee6f3546688c3` published in `f9d97fe`.
The before binary was built at `7e185b1049f0253b682ddee3c3de378c263e6f9e`;
`b003cc3` changes documentation only. The current binary was built at
`b059a25`; `f9d97fe` changes documentation only. Freeze the 2,012-file
published source archive and verify it on both hosts.
Use identical before/after executable bytes on both hosts, with task-local
source copies, homes, caches and runtime directories. Limit work to eight
CPUs and eight requested jobs. Run no competing task build during accepted
timings; no exclusive host reservation is claimed. Host C uses mixed memory
without a full memory-stability qualification.

* [TensorFlow data](data/tensorflow-build-retest-20261109.json) records pristine
  2.17 sources, tool/binary hashes, command/environment templates and both
  hosts' failed Rush builds plus successful Bazel reference builds. These
  failures establish a rule-loading boundary; they supply no speed ratio.
* [Modular data](data/modular-cpu-20261109.json) retains all 96 accepted samples,
  compiler counts, edit digests, output hashes, upstream/tool pins and the
  controller hash. The report covers one CPU stdlib package. Require actual
  prepare/qualify/measure checks on each host before accepting timing data.
  Exclude all 96 preliminary polling-timer rows; the accepted samples use a
  monotonic clock and pidfd completion notification.

Read the workload reports for precise cache scopes and assertions. Keep raw
paths, process records, source archives and compiler binaries outside public
Git. The [public controller](../../benchmarks/workloads/README.md) explains
how to repeat the Modular experiment with independently supplied tools.

## Imported TensorFlow and Modular records

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

The [November 9 data](data/linux-build-retest-20261109.json) records two
complete login-profile attempts through snapshot refusal. Make controls,
managed observation and derivation complete; assembly refuses group
credentials and no replay runs. Retain the exact seed, source recipe,
ordered public-patch verification, tool versions/hashes and host-specific
normalized configurations. Use GNU time's elapsed field for setup diagnostics.
The separate [mapped-profile data](data/linux-mapped-retest-20261109.json)
retains all three attempts: a directory-witness capture refusal and two
completed captures/derivations whose assemblies refuse an unenforceable
`init/` checkpoint. None produces a snapshot or replay. Keep failure timings
as diagnostics and assign no current Linux speed ratio.

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
