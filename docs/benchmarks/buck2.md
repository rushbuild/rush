# Compare Rush and Buck2 on generated C++ graphs

Use these results as a local, synthetic comparison of command and incremental
build overhead. Rush takes less time in all four modes at 1,024 headers. At
18,432 headers, Rush takes less time for clean compilation; Buck2 takes less
time for unchanged commands and one-file rebuilds. Preserve both outcomes.

## Read the measurements

Each cell gives the median and [minimum, maximum] in milliseconds across
five samples per engine and operation. Alternate engine order within each
operation. Include daemon startup in clean builds; retain daemons for other
operations. Keep four warm-up commands outside the 80 measured commands.

| Headers / libraries | Operation | Rush ms | Buck2 ms |
| --- | --- | ---: | ---: |
| 1,024 / 64 | Clean compilation | 325.301 [323.596, 332.468] | 435.652 [417.275, 451.780] |
| 1,024 / 64 | Unchanged command | 3.064 [2.127, 3.371] | 5.819 [5.508, 6.414] |
| 1,024 / 64 | Novel source edit | 38.708 [36.867, 40.272] | 53.984 [51.763, 54.934] |
| 1,024 / 64 | Novel header edit | 38.184 [36.747, 39.554] | 54.720 [52.817, 55.746] |
| 18,432 / 384 | Clean compilation | 1,579.802 [1,575.856, 1,592.667] | 1,938.202 [1,879.901, 1,952.277] |
| 18,432 / 384 | Unchanged command | 10.094 [7.643, 11.328] | 6.643 [6.119, 6.789] |
| 18,432 / 384 | Novel source edit | 143.193 [135.969, 146.356] | 86.733 [85.307, 87.002] |
| 18,432 / 384 | Novel header edit | 141.872 [135.267, 145.772] | 85.618 [85.037, 87.347] |

Read every accepted sample and its action counts in the
[1,024-header data](data/buck2-1k-20261108.json) and
[18,432-header data](data/buck2-20k-20261108.json). Values in those files use
seconds. Keep the full distributions; five samples provide an initial
comparison, not a tail-latency or statistical-confidence claim.

## Freeze the inputs

Run date: 2026-11-08. Host scope: one Linux x86-64 host, labelled Host C.
Give each engine the same eight allowed CPUs and request eight jobs. Verify
Rush's resource manager observes eight CPUs. Use g++ 16.2.0
(Debian 16.2.0-3), GNU ar 2.47 and Python 3.14. Keep C++17, `-O0` and `-g`
on both sides. The host uses mixed memory kits without a full memory-stability
qualification; these bounded passes establish no full-memory qualification.
The host was lightly loaded at preflight; no exclusive host reservation was
held. Run no Rust compilation concurrently with accepted samples.

Build Rush from
[`72f0e3f615ffa864bbefca0cd2fe498c4f43f52e`](https://github.com/rushbuild/rush/commit/72f0e3f615ffa864bbefca0cd2fe498c4f43f52e),
tree `3f377c8e4f20d603e6cae8db56b21da02659ef59`, using Rust/Cargo 1.90.0:

```sh
cargo build --release --locked --bin rush -j8
```

Check all 1,980 tracked source files against that revision before accepting
the result. The measured binary's SHA256 is
`2e956e7c7103ac841992df4ceb1e02ac63967b50dfea95d0fe6489834168ff21`.
The later comparison-page commits change no engine source.

Use the Linux GNU binary from
[Buck2 release 2026-11-01](https://github.com/meta/buck2/releases/tag/2026-11-01).
It reports `2026-10-31-e8edae33cd7cbc6d042c58bb55a2a900ac6ad489`.
Check the compressed asset SHA256
`a2f7830c240c682c32826ae0b30bf8864d264e866475abaafcc0857d8133cd53`
and decompressed binary SHA256
`769191b5d9da8d95ca8c7ace58a7b34c9535b519c188d5fe8c418387282a60ac`.
Use [prelude commit 02bac0856e1f29d9bb30380072b1819216effe0a](https://github.com/meta/buck2-prelude/tree/02bac0856e1f29d9bb30380072b1819216effe0a),
as selected by that release's prelude_hash asset. Run neither engine against
a floating release or prelude.

## Hold the work constant

Pin run.py SHA256
`eb5ba0adda247c1561c87931746712d271c618f8b4adb5f43ccc9321e4affa10`.
Use [the harness](../../benchmarks/buck2/README.md) to generate two independent
copies of each program. Require identical source/header path-and-content
digests before building. Read the digests in the sample files. Translate
build declarations to each engine's native rules and retain the standard
Buck2 prelude. Configure system GCC and static library linkage. Execute these
commands from their respective generated workspace roots:

```sh
rush build //:bench -j 8 --daemon=auto
buck2 --isolation-dir comparison build //:bench -j 8 \
  --local-only --show-full-output
```

Give both processes the same inherited CPU affinity. Leave default watcher
behavior enabled. Use separate task-owned homes, caches and runtime roots.
Before each clean sample, stop its daemon and remove its build state and
task-owned caches. Use `buck2 clean` for Buck2 and clear Rush's output,
workspace cache, home cache and daemon discovery state. Recreate no global
cache and use no remote service. Keep the operating-system page cache warm;
this is a clean-output/empty-action-cache comparison, not a cold-disk test.

Log compiler, archiver and linker invocations with the same shell-wrapper
mechanism. Expand Buck2 response files when inspecting effective flags.
Keep each engine's native include paths, dependency tracking, archive flags,
link grouping and helper actions. Rush's main compilation omits several
library flags that Buck2 also applies to main; the full command lines and
debug output paths differ. These are comparable native builds, not identical
action descriptions or a measurement of scheduler throughput alone.

## Require actual work and correct output

Accept all 20 clean commands only after counting exactly P+1 C++ compilations,
P archive creations and one link for P libraries. Accept all 20 unchanged
commands only after counting zero compiler, archive and link calls. Accept
all 40 source/header edit commands only after observing one compilation,
one archive creation and one link in each engine. Change input values for
every edit; keep the final source edit fixed while changing the header.

Execute the binary after every measured command and every warm-up. All
84/84 executions print the expected sum derived from every generated header
and the current edit. This functional check detects a stale binary in this
fixture; it establishes neither byte-identical artifacts nor application-wide
correctness. No engine source change is included in this comparison.

## Keep diagnostics separate

Exclude setup probes and earlier diagnostic runs from the published tables.
The first probes correct an output-path assumption and the harness's timing
wait method. A later cache-reset probe correctly rejects a purported clean
Rush build with zero compiler calls: the daemon retains outputs in its
home cache even when that probe requests `--disk-cache=false`. Clear the
task-owned home cache and require full compiler counts in the accepted run.

An in-process-clean/daemon-restart diagnostic recompiles the entire Rush
graph on the first source edit. The accepted run uses daemon mode throughout
and recompiles exactly one source for every edit. Keep that transition as a
separate follow-up; do not attribute it to the accepted default-daemon path.
The final CPU-affinity control supersedes unrestricted-CPU diagnostics.

Profile the larger graph's unchanged and edit paths next. Check daemon option
handling separately, including job limits and disabling the disk cache. Run
optimized builds, larger compilation-heavy fixtures, output restoration,
missing-output/failure controls, more samples and a second host before
extending the comparison. TensorFlow, Modular and Linux were not built with
Buck2 in this experiment; their existing reference-engine results do not
rank Buck2.
