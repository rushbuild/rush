# Build and replay Linux

## Rush 2026-11-10: current-source correctness

Build the Kbuild capture/replay example with **Rush 2026-11-10 (`ad87da5`)**,
code revision `ad87da5188201534d443c40481aba6b57ecc30d6`, source tree
`87bb1112a4fe33fd2b3996ee65f4ab06788a5ebd`. Use the source-bound data
record below for the original build identity and executable digests.

| Host | Derived actions | Analysis barriers | Empty replay | Persistent entries |
| --- | ---: | ---: | --- | ---: |
| Host C | 5,685 | 3,067 | 5,685 executed; 0 hits | 12,397 |
| Host D | 5,685 | 3,067 | 5,685 executed; 0 hits | 12,395 |

Build an ordinary Make-j8 reference at the future capture object pathname.
Retain the complete reference by exclusive rename. Capture a fresh Make-j1
build at the same pathname with eight available CPUs. Derive the action graph,
assemble a fresh version-16 snapshot and replay every action directly with
eight Rush CPU slots and an empty action cache.

Require full bytes, types, modes, owners, symlink targets and hardlink topology
equality against both Make references. Compare all 4,827 persisted .cmd/source/output
records and repeat output walks to verify stability. Validate the full exported
source archive, tool identities, frozen binding and resource envelope after replay.

Use the capflags action profile to verify 42 complete directory checkpoints,
local uncacheable admission and the mandatory live clean-entry policy.
Scope that checkpoint count to the inspected action. Treat dependency-layer
maxima as structural graph measurements.

Run replay through the Rush native driver. Preserve the source-audited exact
link-vmlinux profile that substitutes the shell builtin `:` for represented
timestamp work. Measure nested Make processes separately before making a
process-count claim.

Read [the dated source-bound record](data/linux-final-correctness-20261110.json)
for exact executable, snapshot, source, seed and tool/version identities.
Build the gate with `cargo build --release --locked -p rush-build --example kbuild_snapshot_gate -j8`.
Apply the 23 ordered public patches and export the kernel tree described in
the retained source recipe below. Use fresh capture, snapshot and cache roots.

Scope these records to single shared-host correctness checks under CPU8,
32 GiB and zero swap. Qualify populated-cache, warm, speed ratios, installation,
boot and runtime behavior separately before publishing those results.

## Inspect command costs

Each cell is one controller-observed command interval in seconds, including
completion/reap overhead. Keep the profiles distinct: ordinary Make uses
eight jobs, capture observes Make with one job, and native replay executes
the resulting graph with eight available CPU slots. These unpaired
correctness diagnostics establish no speed ratio. Include graph preparation
when evaluating the total cost of using a new snapshot.

| Phase | Host C | Host D |
| --- | ---: | ---: |
| Ordinary Make-j8 reference | 106.166 | 102.008 |
| Make-j1 observation | 1801.402 | 1840.891 |
| Graph derivation | 160.880 | 164.893 |
| Snapshot assembly | 366.992 | 379.283 |
| Empty-cache native replay | 849.059 | 851.792 |

Profile replay admission, analysis barriers and active workers before
claiming a scheduling advantage. Retain output and dependency checks while
reducing these costs. Qualify populated-cache and unchanged-command paths
with separate runs.

## Reproduce the fixed profile

Build the `kbuild_snapshot_gate` example from code revision `ad87da5`.
Export the exact v1-patched kernel tree using the source recipe below; keep
the exported source free of Git metadata. Copy the published configuration
seed and normalize it with the selected compiler through `make olddefconfig`.
Clear the inherited environment. Use the first eight recorded CPUs,
umask `0077`, a fresh task home,
`PATH=/usr/bin:/bin`, `LANG=C`, `LC_ALL=C`, and `TZ=UTC`. Set:

```sh
export SOURCE_DATE_EPOCH=1790154919
export KBUILD_BUILD_TIMESTAMP=@1790154919
export KBUILD_BUILD_USER=builder KBUILD_BUILD_HOST=builder
export KBUILD_BUILD_VERSION=1
```

Start the whole profile in `unshare --user --map-current-user`, including
the ordinary reference and observation. Use the same absolute object
pathname for the Make-j8 reference and fresh Make-j1 capture, retaining
the complete reference by exclusive rename before capture. Keep the
managed output root absent until observation creates it. Use goal `all`
and `--jobserver-style=pipe` for both Make commands.

Run the example's `managed-observe-process`, `derive-graph` and
`assemble-managed-graph` operations in order. Retain the managed transaction
and every returned input/snapshot key. Use the recorded stream limits of
32 GiB, 100,000,000 records, 1,000,000 processes and 64 MiB per frame.
The public example prints its complete argument syntax. Start assembly
and replay in the additional current-user namespace used by this profile.
Keep the captured output root in place for admission, and invoke
`replay-clean-commit` with a fresh cache. Let the managed operation replace
and commit its own output root; preserve the original capture records.

Require the complete output, hardlink, command-inventory, source/tool and
resource checks above. Record new executable and snapshot hashes for each
independently rebuilt gate. Keep snapshots and keys local to the filesystem
and tool identities they capture.

## Recreate the kernel source

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
roots and run `make olddefconfig` outside timing. Copy the normalized result
to `RUN/normalized.config` and seed the separate Make-control root with it.
Keep the managed capture output root absent; let capture create and seed it.
Create the phase metadata directories before their recorded commands.
Require seed SHA256
`5e0134db8e9bb259ee5133a177f7feed45a11eec67c808adcf0454f1aeb9cfc5`.
Its GCC 11 header records the seed's origin; each new build normalizes it
with its own pinned compiler. Host C uses GCC 16.2.0 and Host D uses GCC
15.3.0. Both use GNU Make 4.4.1 and Binutils 2.47. Preserve each host's
normalized digest; call this a seeded x86-64 profile.

## Check Lorenzo's upstream status

As checked on 2026-11-10, Linux mainline contains a revised subset of
[Lorenzo Stoakes's original 23-patch build-speedup series](https://lore.kernel.org/all/20260908-build-speedup-v1-0-5dc1ac01672d@kernel.org/).
The [first merged batch](https://github.com/torvalds/linux/commit/997208ce9f43f01febac956cf9aeffe89945e3c1)
includes symbol-table, toolchain-probe and compression improvements.
The bounded revision mapping verifies counterparts for original entries
1–9, 12 and 23. Read the [later v4 cover](https://lore.kernel.org/all/20260923-build-speedup-v4-0-73128809a4a4@kernel.org/)
for changed or dropped proposals. Keep this measured v1-patched tree fixed;
run a fresh comparison to evaluate current mainline.


## Read earlier experiments

Read the [November 9 report](history/linux-20261109.md) with its recorded
engine, snapshot and failure scopes. Keep the separate 32-job populated-cache
comparison attached to that historical record. Use the current checks above
for Rush 2026-11-10.
