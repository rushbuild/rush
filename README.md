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

## Compare build times

Read [Rush vs Buck2](https://rushbuild.github.io/rush/buck2.html) for the
architecture, adoption requirements and matched-workload comparison.

The November 9 checks compare Rush before the incremental changes with the
current engine on two Linux hosts, using eight CPUs and eight requested jobs.

| Workload and operation | Rush before | Rush current | Bazel reference |
| --- | ---: | ---: | ---: |
| Modular stdlib package, unchanged command, Host C | 3.62 ms | **2.66 ms** | 53.57 ms |
| Modular stdlib package, unchanged command, Host D | 3.89 ms | **2.93 ms** | 56.12 ms |
| TensorFlow 2.17 framework, fresh build, both hosts | Rule loading fails | Rule loading fails | Build completes |

Use the Modular medians within their ten-sample no-op scope. All 96 Modular
commands pass compiler-count and package-byte checks. Clean-output and novel
comment-edit differences are small with overlapping ranges; these checks
establish no broad compilation speedup. TensorFlow executes zero compiler
calls under both Rush versions because `JavaInfo` fails to load. Assign no
speed ratio to those failed builds.

Read the [TensorFlow](docs/benchmarks/tensorflow.md) and
[Modular](docs/benchmarks/modular.md) reports for every sample, exact engine
and toolchain pins, configuration, cache controls and separate imported
multi-target records.

### Compare the Linux kernel with GNU Make

Read the separate three-pair experiment while the new two-host snapshot
checks run. That retained experiment reports **40.24 s** for populated-cache
Rush replay versus **50.92 s** for a clean
Make build at the median of three alternating pairs, with 12,397 persistent
output entries matched. Both arms use the same tree carrying
[Lorenzo Stoakes's v1 build-speedup patches](https://lore.kernel.org/all/20260908-build-speedup-v1-0-5dc1ac01672d@kernel.org/).
Parts of that work have merged into mainline; keep the measured v1 baseline
fixed. Read the [Linux report](docs/benchmarks/linux.md) for upstream links,
setup cost, empty-cache behavior and the remaining acceptance gate.

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
