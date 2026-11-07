# Compare Modular builds with Bazel

Build the same 4,175-target ledger at Modular pin `cc7155d`. Prefetch external
repositories for both engines and use the same configuration. The measurement
host is a 96-core x86-64 system. Report medians of three runs except for the
single-sample cold rows.

## Whole-repository results

| Operation | Rush | Bazel | Cache state |
| --- | ---: | ---: | --- |
| Compile the whole tree | **362 s** | 611 s | Cold-run intent; revalidate cache clearing |
| Restore deleted outputs | **32 s** | 63 s | Warm disk caches; output trees removed |
| Leave inputs unchanged | **0.05 s** | 12.6 s | Warm no-op; Bazel without disk cache |
| Leave inputs unchanged | **0.05 s** | 13.2 s | Warm no-op; Bazel with disk cache |
| Compile a novel one-file edit | **26.0 s** | 30.1 s | Distinct edit bytes for each run |
| Repeat an already-cached edit | **0.15 s** | ~13 s | Reuse a known input state |

Use the cache-restore row to compare output restoration: Rush takes about
half the time in this scope. Use the novel-edit row to compare an actual
recompile: 26.0 s versus 30.1 s. Keep the repeated-edit result separate;
its compiler input state already exists in the cache.

![Modular build modes](graphs/bench-full.svg)

<details>
<summary>View the same whole-repository data on a logarithmic axis</summary>

![Modular build modes on a logarithmic scale](graphs/bench-full-log.svg)

</details>

## Control cache state

Clear each engine's actual action cache and content-addressed store before
measuring an uncached compile. Removing outputs alone measures restoration.
Rush's default disk cache is under `$HOME/.cache/rush`; removing a workspace's
`.cache/rush` directory does not clear that store. The 362 s / 611 s cold-run
result has one sample and an incomplete cache-reset control. Revalidate that
control before using it as a confirmed uncached speed ratio.

Use distinct source bytes for every novel-edit sample. Repeating identical
comment text measures a reusable input state. Retain that as a separate
repeated-edit experiment rather than mixing its time into the compile median.

## Stdlib slice

Use the 945-target ledger and report its scope separately from the
whole-repository tree.

| Operation | Rush | Bazel without disk cache | Bazel with warm disk cache |
| --- | ---: | ---: | ---: |
| Compile the slice | 39 s | 245 s | 42 s |
| Leave inputs unchanged | 0.35 s | 3.4 s | 3.5 s |
| Rebuild after one file changes | 23 s | 24 s | 24 s |

Apply the same cache-reset requirement to cold rows.

![Modular stdlib slice](graphs/bench-stdlib.svg)

## Check output and runtime behavior

The build ledger reports 4,175 accepted targets, including 1,063 Python
targets. Comparable Mojo library artifacts match byte for byte in 38 of 39
cases; keep the remaining `builtin_kernels` difference visible.

The selected H100 GPU corpus contains 287 of 763 candidate tests. All 222
comparable Bazel-passing tests also pass when built by Rush. The record also
reports 12 shared failures and 36 Rush passes whose Bazel wrappers do not run
outside their runfiles environment. Recover the full case ledger to reconcile
every selected and excluded test before claiming complete runtime coverage.

Virtual output materialization reports fewer written bytes but no measured
wall-time improvement on this host. Do not add a speedup for that feature.

Read the [Modular status](../status/modular.md) for acceptance procedures and
the [measurement provenance](PROVENANCE.md) for source records. Update plots
and tables from the same sample set when repeating the comparison.
