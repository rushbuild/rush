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

| Workload and operation | Rush | Reference | Scope |
| --- | ---: | ---: | --- |
| TensorFlow unchanged command | **9.3 ms** | Bazel 64.2 ms; watchfs 54.5 ms | 30-sample p50; `//tensorflow/core:framework`; 18,522 dependencies |
| Modular output restoration | **32 s** | Bazel 63 s | 4,175 targets; populated disk caches |
| Modular unchanged command | **0.05 s** | Bazel 12.6 s | Warm no-op |
| Modular novel one-file edit | **26.0 s** | Bazel 30.1 s | Distinct input bytes per sample |
| Modular repeated cached edit | **0.15 s** | Bazel ~13 s | Reuse a known input state |

Read the [TensorFlow](docs/benchmarks/tensorflow.md) and
[Modular](docs/benchmarks/modular.md) reports for configuration, samples and
cache controls. Keep command latency, compilation and restoration separate.
The single-sample Modular compile row (362 s versus 611 s) requires
cache-reset revalidation; do not use it as a confirmed uncached speed ratio.

### Compare the Linux kernel with GNU Make

Populated-cache Rush replay takes **40.24 s** versus **50.92 s** for a clean
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
