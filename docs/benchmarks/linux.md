# Qualify Linux snapshots before comparing replay

Build the pinned x86-64 Linux tree and require snapshot assembly plus exact
replay output checks before assigning a current Rush speed result. The
November 9 two-host check uses engine `b059a25` published in `f9d97fe`,
eight CPUs, `make -j8` and the supplied configuration seed.

## Read the login-credential build check

| Host | GNU Make control | Observation / graph derivation | Snapshot assembly | Replay |
| --- | --- | --- | --- | --- |
| Host C | Completes in 96.23 s; 12,397 entries | Completes; 5,684 actions | Refuses captured group credentials | Not run |
| Host D | Completes in 103.65 s; 12,395 entries | Completes; 5,684 actions | Refuses captured group credentials | Not run |

Both assemblies exit 1 with `Kbuild mandatory action credentials are not
isolatable`, caused by `local isolation cannot preserve distinct group
identities`. Neither times out or writes a snapshot. Count ordinary Make
completion separately from Rush replay acceptance. Assign no new Rush/Make
speed ratio and no before/after Linux improvement to these attempts.

The local executor supports equal fixed user/group identities and supplementary
groups projected to the primary or kernel overflow group. The login profile
contains group identities that this restricted executor cannot preserve.
A namespace started only for assembly cannot change the credentials already
captured in action records. Keep those records intact and start a new capture
when changing the environment.

Read [the full sanitized data](data/linux-build-retest-20261109.json) for
commands, source/tool hashes, normalized configurations, phase counts and
failures. Capture takes 1,275.70 / 1,357.68 s, derivation 241.12 / 252.98 s,
and failed assembly 2,199.22 / 2,300.08 s on C / D. Treat these as setup and
single-control diagnostics. Require a cheap credential preflight before
another expensive capture; assembly currently reaches this refusal late.

## Evaluate the mapped-namespace profile separately

Run every phase, including normalization, the Make control and capture, with
the outer prefix `unshare --user --map-current-user`. Keep the existing inner
assembly/replay namespaces and credential checks. Start fresh run roots with
the same gate, source, seed, tools and CPU budget. The two-host retry is in
progress. Its preliminary credential predicate passes; actual assembly and
replay must still establish full credential preservation and output parity.

## Reproduce the pinned source and configuration

Fetch Lorenzo's original 23-patch v1 series and apply it to the cover letter's
base. Keep the measured tree fixed instead of substituting current mainline:

```sh
git clone --filter=blob:none https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git linux-compare
git -C linux-compare checkout --detach 28924df2a08f440c73991b83028032c901de2ae4
b4 am -T -C -v 1 -o lorenzo-v1 -n speedup 20260908-build-speedup-v1-0-5dc1ac01672d@kernel.org
git -C linux-compare am ../lorenzo-v1/speedup.mbx
test "$(git -C linux-compare rev-parse 'HEAD^{tree}')" = 116df9506075825b4f676ff33443a8f3acbc6e3e
mkdir linux-source
git -C linux-compare archive HEAD | tar -x -C linux-source
```

The measured checkout is commit `35568770debb5fab088d5d6c2b6a535ad6e8cb4d`.
Recreated commit IDs can differ with committer metadata; require the exact
tree above. Build the exported `linux-source` tree without `.git` metadata,
as in the recorded builds; a Git checkout can change Kbuild version strings.
The archive digest identifies the retained input archive, whose metadata may
differ after recreating commits. All 23 measured patches match the freshly
fetched public series in order under whitespace-preserving patch IDs. Retain those IDs in the data.

Copy [the exact seed](data/linux-seed-20261109.config) into separate output
roots and run `make olddefconfig` outside timing. Require seed SHA256
`5e0134db8e9bb259ee5133a177f7feed45a11eec67c808adcf0454f1aeb9cfc5`.
Its GCC 11 header records the seed's origin; each new build normalizes it
with its own pinned compiler. Host C uses GCC 16.2.0 and Host D uses GCC
15.3.0. Both use GNU Make 4.4.1 and Binutils 2.47. Preserve each host's
normalized digest; call this a seeded x86-64 profile.

Build the release example from Rush `f9d97fe` with:

```sh
cargo build --release --locked -p rush-build --example kbuild_snapshot_gate -j8
```

The tested release example has SHA256
`67d84ddfefdca958442937dc9d205cb43cdbb01f8031d263dfb0f833d2c4395d`. Rebuild with Rust/Cargo 1.90, record the new executable digest and
repeat the gates; toolchains and build paths can change binary bytes.
Run commands from the pristine kernel source with the first eight
allowed CPUs, `make -j8`, umask `0077`, and the recorded fixed timestamp,
locale, timezone and build labels. Use separate task homes and caches. Expand
`SOURCE`, `GATE` and `RUN` in the data to canonical absolute paths. Wrap each
command with GNU `time -v`; the data use its elapsed field at 0.01-second
resolution. Configuration preparation and output fingerprinting stay outside
the command interval. Replay uses its native resource admission within the
eight-CPU affinity limit.

Capture independently on each host: snapshots bind local VFS and tool
identities. Run managed observation, graph derivation and snapshot assembly
in order, retaining the explicit outcomes. Use
`unshare --user --map-current-user` for assembly and replay. Require a complete snapshot
before timing replay; a successful `capture_uncertified` observation alone
supplies no replay acceptance.

Keep the captured output root in place for snapshot admission. With nonzero
analysis barriers, `replay-commit` swaps it internally for a clean root and
commits the completed result. Assembly leaves the replay cache empty. First
complete an empty-cache replay and require parity against the captured Make
tree, then time subsequent populated-cache replays separately. Report cache
hits and remaining executions; infer no compiler count from their aggregate
alone. Include output swapping and final backup removal in replay timing.

Compare every persistent name, type, mode, owner, symlink target, hardlink
class and file hash against capture outputs. The separate Make control uses
a different output path; its success alone establishes no cross-arm byte
parity. Keep toolchains and normalized configurations attached to each host.
No exclusive host reservation or full mixed-memory stability qualification
is claimed. Run no kernel installation, boot or GPU program in this scope.

## Read the separate 32-job replay experiment

Use the same kernel tree, configuration, toolchain and deterministic build
environment for Rush and GNU Make. The measured tree contains
[Lorenzo Stoakes's v1 build-speedup patches](https://lore.kernel.org/all/20260908-build-speedup-v1-0-5dc1ac01672d@kernel.org/).
Keep this baseline fixed when interpreting the results.

### Three-pair experiment

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

### Account for graph preparation

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

### Check Lorenzo's upstream status

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

### Complete the acceptance gate

Freeze one Rush candidate, kernel tree, configuration and environment.
Require fresh snapshot assembly and replay to match all output entries.
Run at least ten balanced alternating pairs, report every sample, and require
the 95% confidence interval for the paired speedup ratio to stay above 1.0.

Exercise changed source, changed environment, empty cache, corrupt cache and
interrupted output commits. Require correct rebuilding or refusal before a
partial output tree becomes visible. Keep graph preparation, unchanged
commands, incremental builds, cache restores and uncached compilation in
separate results. The three-pair result does not complete this gate.
