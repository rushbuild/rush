# Compare Linux replay with GNU Make

Use the same kernel tree, configuration, toolchain and deterministic build
environment for Rush and GNU Make. The measured tree contains
[Lorenzo Stoakes's v1 build-speedup patches](https://lore.kernel.org/all/20260908-build-speedup-v1-0-5dc1ac01672d@kernel.org/).
Keep this baseline fixed when interpreting the results.

## Three-pair experiment

| Arm | Wall times | Median |
| --- | --- | ---: |
| Rush, populated-cache replay | 35.98 s, 41.19 s, 40.24 s | **40.24 s** |
| GNU Make, clean build | 50.92 s, 53.04 s, 50.15 s | **50.92 s** |

The median ratio is **1.27x**, or about **21% less wall time** for Rush.
Both arms use 32 jobs on one 96-core x86-64 host. Each pair matches 12,397
persistent output entries, including 11,561 regular files, with zero byte,
type, mode or symlink-target differences.

Compare these rows as **populated-cache replay versus clean compilation**.
An empty-cache Rush diagnostic takes 12:02.92. Require a separate matched
uncached experiment before claiming a compiler-scheduling advantage.

## Account for graph preparation

Run Make under the observer to capture the graph, derive action records,
assemble a validated snapshot and populate the content-addressed store before
replay. One complete fixture records 1:12:20 for capture, 6:48.55 for derivation
and 1:08:15 for assembly. These setup costs are excluded from the replay row.

The kernel baseline is commit
`35568770debb5fab088d5d6c2b6a535ad6e8cb4d`, tree
`116df9506075825b4f676ff33443a8f3acbc6e3e`. Use fixed timestamp, build
user/host labels, locale and timezone for parity. Read the
[public experiment record](https://github.com/rushbuild/rush-status/blob/26e6a51ee374c5cea6f86d33c7edf3e20923dabc/status/linux.md)
for candidate limitations and incomplete descriptor-policy validation.

## Check Lorenzo's upstream status

As of 2026-11-07, parts of the speedup work are merged into Linus's mainline.
The [first Kbuild speedup merge](https://github.com/torvalds/linux/commit/997208ce9f43f01febac956cf9aeffe89945e3c1)
is an ancestor of mainline commit
`a41e364995bbcfa627418a70fe913c35b7510728`. Included changes cover
[compiler and linker probe reuse](https://github.com/torvalds/linux/commit/454e00cf8ee5aff555ad5865b8c3dbf3731afcc9)
and [parallel kernel compression with pigz](https://github.com/torvalds/linux/commit/48ebe1cbbeb51879ba4d22775fee1654d94d1cc6).
Do not describe the entire effort as an unmerged proposal. This check does
not establish that every patch in the original v1 series merged unchanged.

Retain the v1-patched tree for this result. Run a new comparison to make a
claim against current mainline; do not substitute merged revisions into an
existing measurement.

## Complete the acceptance gate

Freeze one Rush candidate, kernel tree, configuration and environment.
Require fresh snapshot assembly and replay to match all output entries.
Run at least ten balanced alternating pairs, report every sample, and require
the 95% confidence interval for the paired speedup ratio to stay above 1.0.

Exercise changed source, changed environment, empty cache, corrupt cache and
interrupted output commits. Require correct rebuilding or refusal before a
partial output tree becomes visible. Keep graph preparation, unchanged
commands, incremental builds, cache restores and uncached compilation in
separate results. The three-pair result does not complete this gate.
