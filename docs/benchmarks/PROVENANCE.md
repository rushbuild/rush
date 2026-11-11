# Measurement provenance

Bind each result to its dated Rush code, selected upstream target, tools,
configuration and output checks. Use the current workload reports to locate
accepted evidence; keep imported and historical measurements within their
own source and cache scopes.

## November 10 TensorFlow and Modular checks

Use Rush 2026-11-10, code revision
`ad87da5188201534d443c40481aba6b57ecc30d6`, source tree
`87bb1112a4fe33fd2b3996ee65f4ab06788a5ebd`. The source identity mapping
preserves all 2,090 tracked source files from the measured candidate and
retains the original executable, controller and evidence digests. Both CPU
systems pass the 21 selected source, component and release checks. Complete
broader suites and workload ledgers before expanding those checks' scope.

* [TensorFlow data](data/tensorflow-native-20261110.json) binds two default
  Auto builds to original TensorFlow 2.17 sources. C builds a native consumer
  and passes one Tensor serialization/restoration/slice program. D builds
  the framework and inventories its object/archive subtree. Require the
  recorded original-source checks on both systems. C also compares selected
  compiler/SDK inputs across the build and selected source/tool/output files
  across runtime. Keep those two file-comparison intervals distinct.
* [Modular data](data/modular-cpu-20261110.json) retains all 96 accepted
  samples for Rush 2026-11-08, Rush 2026-11-10 and Bazel 10. Require the
  36 measured precompiles, 60 unchanged commands and exact package bytes.
  Keep every novel-edit digest, sample range and source-restoration result.
  Use eight CPUs, eight jobs, a 32 GiB limit and zero swap. Drain task-owned
  CPU builds on both systems and measure hosts serially. Background GPU
  activity was permitted; retain that shared-system scope.
* [The separate Modular diagnostic](data/modular-profile-20261110.json)
  records three instrumented cold builds, one warm request, the cgroup
  memory peak and the warm phase ledger. Graph reuse succeeds while input
  validation dominates the measured request. Preserve validation when
  reducing that cost. Keep these instrumented rows outside the 96 samples.

Preserve raw paths, source archives, process records and compiler binaries
outside public Git. Follow the [TensorFlow procedure](tensorflow.md) and
[Modular controller instructions](../../benchmarks/workloads/README.md)
with independently supplied tools and fresh task directories. Collect
matched TensorFlow/Bazel work and complete timing distributions before
assigning a current speed ratio.

## November 9 workload checks

Compare Rush `b003cc3a3f068449510ba69e9ef1070c4598cc4a` with engine
`b059a257874270dbb17ebebeb2bee6f3546688c3` published in `f9d97fe`.
The before binary was built at `7e185b1049f0253b682ddee3c3de378c263e6f9e`;
`b003cc3` changes documentation only. The after binary was built at
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

## November 10 Linux snapshot and replay checks

Use [the current Linux record](data/linux-final-correctness-20261110.json)
with Rush code `ad87da5` and the pinned v1-patched kernel tree. Build an
ordinary Make-j8 reference at the future capture output pathname, retain
it by exclusive rename, then run a fresh Make-j1 observation there.
Derive 5,685 actions, assemble a new version-16 snapshot and key, and execute
every action through native replay with an empty cache.

Both systems pass all seven phases, 42 inspected directory checkpoints,
full output/hardlink parity against both Make references, all 4,827 persisted
command records and repeated output-stability checks. Require original
source bytes/modes/links, exact tools, binding and resource checks before
and after the run. Record local snapshot/key/executable hashes; retain the
23 original metadata hashes per system. Keep the checkpoint count scoped
to the inspected action profile and graph-layer maxima as structural data.

The recorded command intervals are single controller-observed correctness
diagnostics with completion/reap overhead. Include graph preparation and
collect matched repeated work before assigning a speed ratio. Qualify
populated-cache, unchanged-command, runtime and boot behavior separately.
The November 10 upstream check verifies a revised subset of Lorenzo's
speedup work in mainline; retain the measured v1 tree for these results.

## Historical Linux records

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
