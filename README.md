<p align="center">
  <img src="images/rush.png" alt="Rush — orange gear-shaped crab in motion" width="320">
</p>

# Rush

Rush is an experimental build system written in Rust. It reads Bazel-style
workspaces, evaluates build rules, schedules actions, and caches their outputs.
The project explores how persistent analysis and content-addressed storage can
reduce repeated build work.

Rush is under active development. Compatibility depends on the rules,
toolchain and configuration you use. Validate it against your existing build
before relying on its results. Use the named workloads and build modes below
to evaluate the experimental release.

## Compare the dated build results

Use **Rush 2026-11-10 (`ad87da5`)**, code revision
[`ad87da5188201534d443c40481aba6b57ecc30d6`](https://github.com/rushbuild/rush/commit/ad87da5188201534d443c40481aba6b57ecc30d6).
Build the named target and require its output checks before comparing latency.
Read [Rush vs Buck2](https://rushbuild.github.io/rush/buck2.html) for the
architecture and the separately dated synthetic comparison.

### TensorFlow build performance

Build TensorFlow 2.17 with the default compiler policy, eight CPUs and eight
jobs. Run the native Tensor probe to check serialization, restoration and a
row slice containing nondefault values.

| Case | Selected target | Rush build interval | Accepted checks |
| --- | --- | ---: | --- |
| C | Native Tensor consumer and its framework dependency | 652.786 s | Build, one Tensor program, original source and selected tool/output file checks |
| D | `//tensorflow/core:framework` | 674.906 s | Build, original source and framework object/archive inventory |

Read the [TensorFlow procedure and data](docs/benchmarks/tensorflow.md).
Use these single shared-system intervals within their selected-target scopes.
Bazel 6.5 supplies configuration queries in this profile. Collect matched
Bazel build samples before assigning a speed ratio. When Auto selects always-run execution for conditional includes or token
pasting, run the selected invocation afresh, including unchanged repeats.

### Modular build performance

Build `//mojo/stdlib/std:std` with the pinned Mojo compiler. Compare the dated
Rush revisions and Bazel 10 using eight CPUs and eight jobs on each system.

| Host / operation | Rush 2026-11-08 | Rush 2026-11-10 | Bazel 10 |
| --- | ---: | ---: | ---: |
| Host C / Clear outputs and engine caches | 15.466 s | 34.652 s | 13.626 s |
| Host D / Clear outputs and engine caches | 15.944 s | 34.534 s | 14.108 s |
| Host C / Unchanged command | 3.169 ms | 617.544 ms | 65.840 ms |
| Host D / Unchanged command | 3.438 ms | 614.751 ms | 62.398 ms |
| Host C / Novel comment edit | 11.071 s | 11.711 s | 11.163 s |
| Host D / Novel comment edit | 11.744 s | 12.279 s | 11.771 s |

All 96 commands pass their compiler-count and package-byte checks: 36
precompiles and 60 unchanged commands. Read ten-sample no-op medians and
three-sample clean/edit medians within this package scope. Bazel has lower
medians than Rush 2026-11-10 in all six host/mode comparisons. Profile and
reduce fresh analysis and input-validation costs while preserving correctness.
Run hosts serially after draining task-owned CPU builds on both systems.
Background GPU activity was permitted in these measurements.

Read the [Modular report](docs/benchmarks/modular.md) for every sample,
ranges, the dated plot, diagnostic phases and reproduction instructions.

### Linux kernel build validation

Build the experimental `kbuild_snapshot_gate` example from Rush 2026-11-10
(`ad87da5`). Capture the pinned seeded x86-64 build, assemble a fresh
version-16 snapshot, and execute all 5,685 actions with an empty cache.
Both systems pass full output and hardlink parity against fresh Make-j8
and captured Make-j1 references, including all 4,827 persisted command records.

| Host | Make-j8 control | Empty-cache native replay | Full output checks |
| --- | ---: | ---: | --- |
| Host C | 106.166 s | 849.059 s | Pass; 12,397 entries |
| Host D | 102.008 s | 851.792 s | Pass; 12,395 entries |

Read these as single controller-observed command intervals. Include graph
preparation costs and collect matched repeated runs before assigning speed
ratios. The [Linux report](docs/benchmarks/linux.md) records every phase,
source/seed/tool identity, checkpoint scope and pass condition.

Use the measured tree carrying [Lorenzo Stoakes's v1 build-speedup patches](https://lore.kernel.org/all/20260908-build-speedup-v1-0-5dc1ac01672d@kernel.org/).
Mainline incorporates a revised subset in the [first merged batch](https://github.com/torvalds/linux/commit/997208ce9f43f01febac956cf9aeffe89945e3c1).
Keep this patched baseline fixed when interpreting the result; read the
report's dated upstream check and [later v4 proposal](https://lore.kernel.org/all/20260923-build-speedup-v4-0-73128809a4a4@kernel.org/).

## Build and try it

Install a Rust toolchain with Cargo, a C/C++ toolchain, and the native development
dependencies required by your platform. Some integration tests require additional
tools. The Linux capture and observation code requires Linux.

```sh
cargo build --release --locked --bin rush -j8
./target/release/rush --help
```

Try the small C++ example from its workspace:

```sh
cd examples/hello_cpp
../../target/release/rush build //:hello
```

Run the same target again to exercise reuse. For correctness investigations,
compare clean outputs and source/header edits with the reference build. A fast
no-op alone does not establish dependency correctness.

## Execute on remote workers

Use `rush build --remote-executor ENDPOINT --remote-platform toolchain=IMAGE`
for scoped native C++ builds. Use `rush remote-execute action.json` for explicit
file actions, including GPU commands on an allocated worker. Rush uploads
inputs, restores verified outputs and propagates remote failures. Read the
[remote execution guide](docs/remote-execution.md) and
[acceptance results](docs/benchmarks/remote-execution.md) for setup and limits.

## Find your way around

The workspace contains separate crates for analysis, action identity, execution,
caching, resource admission and command transport. [MAINTAINERS](MAINTAINERS)
maps paths to responsible components and review roles.
[The architecture guide](docs/architecture.md) explains their boundaries and
the proposed order for splitting the largest modules.

Read [CONTRIBUTING.md](CONTRIBUTING.md) before preparing a change. Both Claude
Code and Codex use [AGENTS.md](AGENTS.md). The
[maintenance guide](maintenance/README.md) adopts Chores' scoped repair,
evidence and human-delegation process. Its automation policy is a proposal;
this checkout installs no recurring agent jobs.

See [the release website](https://rushbuild.github.io/rush/) for the project overview and
[workload status](docs/status/README.md) for scoped compatibility reports and
[build performance](docs/benchmarks/README.md). [TODO.md](TODO.md) lists the
first maintenance tasks. Read each workload report with its case counts and
measurement scope. Keep raw build outputs and private experiments outside the
source distribution. Preserve license
notices when redistributing Rush and its included third-party material.

## License

Rush is distributed under the [MIT license](LICENSES/MIT). Included dependencies and
fixtures retain their own license notices.
