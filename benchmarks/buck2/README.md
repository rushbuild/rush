# Compare Rush and Buck2 locally

Build the same generated C++ program with each engine. Use native C/C++ rules,
one static library per package and a binary that prints a sum depending on
every header. Translate only build declarations: `cc_library` to `cxx_library`,
`cc_binary` to `cxx_binary`, headers and public visibility. Keep source bytes
identical. Use the standard Buck2 prelude and its system C++ toolchain.

Run on Linux with Python 3.9 or newer, GCC, GNU ar and a kernel supporting
pidfds. Keep the output directory outside a source checkout and select a path
that does not exist. The harness creates separate workspaces, homes, runtime
directories and compiler logging wrappers. It changes affinity only for itself
and its children; the first N allowed CPUs define the requested N-CPU budget.
It terminates only its verified task-local Rush daemon and its Buck2 server.
Keep the output path short enough for Linux's Unix-domain socket path limit.

Pin the engine binaries and prelude before running. The current report uses
Rush source `b059a257874270dbb17ebebeb2bee6f3546688c3`, built with
`cargo build --release --locked -j8` and Rust 1.90.0. Use the
[Buck2 2026-11-01 release](https://github.com/meta/buck2/releases/tag/2026-11-01)
and [prelude 02bac085](https://github.com/meta/buck2-prelude/tree/02bac0856e1f29d9bb30380072b1819216effe0a).
Check binary identities against [the report](../../docs/benchmarks/buck2.md).

```sh
python3 benchmarks/buck2/run.py \
  --rush /path/to/rush --buck2 /path/to/buck2 \
  --prelude /path/to/pinned-prelude \
  --output /path/to/new-evaluation --packages 64 --headers 16 \
  --jobs 8 --samples 20
```

Repeat with 384 packages and 48 headers in another new output directory.
These shapes contain 1,024 and 18,432 headers. Both use C++17, `-O0`, `-g`,
the same compiler installation and an eight-CPU budget. Preserve native
include, dependency-file, archive and link planning; the full commands differ.
Keep compiler instrumentation enabled for both engines.

For each clean pair, stop the corresponding daemon and clear build state and
task-owned caches before timing the command, including daemon startup. Require
P+1 compiles, P archives and one link for P library packages. Keep the final
daemon alive for an excluded warm-up, then measure unchanged commands and
distinct source and header edits. Alternate engine order by sample. Keep the
last source edit fixed during header edits. Require zero compiler/archive/link
calls on unchanged inputs and actual compilation, archiving and linking on
edits. Execute the output after every command and require its expected sum.

Read samples.json for timings, source digest, CPU budget, action counts and
output checks. Use the median and range of all accepted samples. Raw compiler
and engine logs contain local paths; review and sanitize results before sharing.
The harness blocks until each command exits and does not implement a benchmark
timeout. Stop the run explicitly if a command hangs; preserve its failed logs.

Treat this as a small local control-plane experiment. It measures neither an
optimized application nor TensorFlow, Modular, Linux, remote execution, cache
restoration, failure recovery or byte-identical artifacts. The printed sum is
a bounded functional check. Expand correctness and workload coverage separately.

Run the separate incremental correctness controls against the same binary:

```sh
python3 tests/incremental_perf_smoke.py \
  --rush /path/to/rush --output /short/new-controls
```

Require the sixteen cases to pass before accepting performance improvements.
Keep this output directory short enough for Unix-domain socket discovery.
Retain the emitted case summary and logs outside the source checkout.
