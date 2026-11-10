# Compare a pinned Modular stdlib package

Build `//mojo/stdlib/std:std` at commit
`4e87649567a82e3bdac178ea61d62daddc7477bb`. Compare supplied Rush binaries
before and after a change with the pinned native Bazel toolchain. Measure
one package, rather than the whole Modular tree or its runtime tests.

Use Linux with Python 3.9 or newer, pidfd support, Git and eight available
CPUs. Supply the complete Mojo compiler redistributable, preserving `bin/`,
`lib/`, file modes and internal symlinks. This profile uses Mojo
`1.0.0b3.dev2026080406`, compiler SHA256
`d72d688ed65788a577100299e69d56878289271ad2c8d35e8395e17a905e791b`, and
Bazel `10.0.0-pre-46c5e789f84c7bf4ba1edb105eefa7bc4ebc841b`, SHA256
`202e4c8ef0e1014e85648ba27d137769bb94e27a1cd04b1b347405b24f8c6f0a`.
Retain the compiler's license files; distribute no compiler binaries with Rush.

Choose a new disk-backed output directory outside source checkouts. Keep its
path short enough for Linux's Unix-domain socket limit. The controller clones
three source lanes and isolates homes, runtime directories, engine caches and
Bazel output bases. It changes affinity only for itself and its descendants.
Run one workload at a time on the measured host; this script does not reserve
host resources.

Run the controller and all its descendants inside a task-owned process-tree
containment boundary, such as a service cgroup configured to terminate the
whole group on stop, with a bounded TERM-to-KILL deadline. On interruption,
explicitly stop that owned unit and verify its descendants have drained before
reusing the CPUs. Do not rely on a terminal interrupt alone to stop compilers
or detached helper servers. The controller's timeout handler signals only its
still-unreaped direct child's process group; other exceptions propagate without
signalling a potentially recycled group. An outer stop does not turn an
interrupted run into a completed result. Preserve the failed root and use a
fresh root for the next attempt.

Use this controller for the November 10 comparison, SHA256
`7bdd77317fdb149f88d8c916f570c143312caedde0032592cf818305503eb30c`.
The November 9 results retain their earlier measured controller hash and data.

Pass the same arguments for each mode below. Replace these example paths:

```sh
python3 benchmarks/workloads/modular.py --mode prepare \
  --source /path/to/pinned-modular --toolchain /path/to/mojo-toolchain \
  --bazel /path/to/pinned-bazel --rush-before /path/to/rush-before \
  --rush-after /path/to/rush-after --output /short/new-run --jobs 8
```

Provision the upstream-pinned dependency archives before qualification. The
LLVM archive for commit `21b0fc84c2f5fac1d3f763cfa581762684ad2a89` must have
SHA256 `c4d55430a963e1b939df42703d8328a2092d73c08ceffc897b3e65059bb4a047`.
A fresh GitHub download returned a different digest during the recorded
experiment. Supply the exact original archive through a verified Bazel download
cache if needed. Do not replace the declared checksum or source with a local
patch to complete the benchmark. Keep dependency provisioning outside timing;
copy no action-result cache, stale host-platform answer or old output base.

For the pinned Bazel build, an isolated lane's download cache is under
`$RUN/<lane>/bazel-user/cache/repos/v1/content_addressable/sha256/`, where
`<lane>` is `rush-before`, `rush-after` or `bazel`. Copy the verified
`c4d55430...` directory from an existing download cache into the corresponding
location for each lane, preserving its `file` and `id-*` entries. Independently
hash `file` and require the complete expected digest above before using it.
Copy only the required immutable download, never the original Bazel output
base or generated host repositories. Fresh downloads of other pinned inputs
remain part of qualification rather than measured clean commands.

Run `--mode qualify` with the same six paths. Qualification clears compiled
outputs and action caches, builds each lane, requires one real Mojo precompile
and compares package bytes. It verifies prepared source revisions and tracked
files, then writes `ROOT-QUALIFIED.json` bound to the input and controller
hashes. This file records a build check; it grants no authority or permissions.

Run `--mode measure` with those paths and these explicit sample counts:

```text
--clean-samples 3 --noop-samples 10 --edit-samples 3
```

Keep fetched repositories, analysis metadata and normal language-tool caches.
For each clean command, clear engine outputs/action caches and Rush CAS, stop
the corresponding daemon, then include fresh startup in the measurement.
Keep daemons alive for unchanged commands and distinct comment edits appended
to `mojo/stdlib/std/builtin/int.mojo`. Require real compiler counts 1/0/1 for
clean/no-op/comment-edit commands and identical `std.mojoc` bytes throughout.
Rotate engine order within each mode. Use a monotonic clock and pidfd readiness
for completion; avoid timeout polling that quantizes short commands.

The controller stops only its task-local daemons and helper output bases. It
restores edited tracked files and checks each lane after measurement. Concurrent
or repeated qualification/measurement attempts in the same run root are rejected.
After an interruption or failure, preserve that root and start a fresh one;
do not delete evidence or forge a qualification file to resume in place.

Read `results.json` only when `complete` is true and cleanup/source restoration
checks pass. Keep raw logs and paths outside Git. Publish sanitized samples,
source/tool identities, ranges, action counts and output checks. Compare this
single package separately from optimized applications, GPU runtime programs
or multi-target builds. Read the [workload report](../../docs/benchmarks/modular.md)
for the measured scope and retained whole-tree records.
