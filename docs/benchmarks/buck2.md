# Compare Rush and Buck2 on generated C++ graphs

Measure local command and incremental build overhead with matching C++ sources.
Rush has lower median wall time in all eight measured rows: clean compilation,
unchanged commands, source edits and header edits at both graph sizes. Apply
that result to these synthetic, unoptimized native builds on one host.

## Read the measurements

Each cell gives the median and [minimum, maximum] in milliseconds across
20 samples per engine and operation. Alternate engine order within each
operation. Include daemon startup in clean builds; retain daemons for other
operations. Exclude four warm-up commands from the 320 measured commands.

| Headers / libraries | Operation | Rush ms | Buck2 ms |
| --- | --- | ---: | ---: |
| 1,024 / 64 | Clean compilation | 320.742 [316.564, 343.551] | 429.485 [410.158, 455.136] |
| 1,024 / 64 | Unchanged command | 2.376 [1.717, 3.098] | 6.036 [4.817, 7.213] |
| 1,024 / 64 | Novel source edit | 34.302 [31.662, 37.490] | 53.247 [51.442, 55.355] |
| 1,024 / 64 | Novel header edit | 33.959 [31.712, 36.107] | 53.606 [50.885, 57.008] |
| 18,432 / 384 | Clean compilation | 1,526.122 [1,492.252, 1,657.062] | 1,912.879 [1,884.438, 1,991.522] |
| 18,432 / 384 | Unchanged command | 3.472 [2.559, 4.494] | 5.912 [5.029, 6.670] |
| 18,432 / 384 | Novel source edit | 80.795 [77.348, 83.312] | 85.616 [82.383, 89.339] |
| 18,432 / 384 | Novel header edit | 80.188 [77.763, 83.437] | 85.918 [81.716, 90.345] |

Read every accepted sample and its action counts in the
[1,024-header data](data/buck2-1k-perf-20261108.json) and
[18,432-header data](data/buck2-20k-perf-20261108.json). Values in those files
use seconds. Preserve the ranges: the larger edit distributions overlap.
Twenty samples establish these medians, not a tail-latency guarantee or a
universal ranking of the engines.

## Freeze the inputs

Run date: 2026-11-08. Host scope: one Linux x86-64 host, labelled Host C.
Give each engine the same eight allowed CPUs and request eight jobs. Use
g++ 16.2.0 (Debian 16.2.0-3), GNU ar 2.47 and Python 3.14. Keep C++17,
`-O0` and `-g` on both sides. The host uses mixed memory kits without a full
memory-stability qualification; these bounded passes establish no such
qualification. The preflight load average was below one; no exclusive host
reservation was held. Run no Rust compilation concurrently with accepted
samples.

Build Rush from
[`b059a257874270dbb17ebebeb2bee6f3546688c3`](https://github.com/rushbuild/rush/commit/b059a257874270dbb17ebebeb2bee6f3546688c3),
tree `ba26c38784145ed7c27f427cf423456a3cf944ea`, using Rust/Cargo 1.90.0:

```sh
cargo build --release --locked -j8
```

Check all 2,010 tracked files against that revision before accepting the
result. The measured binary's SHA256 is
`9869e0b075a3923b07cef4365cfbf9def89a7e71b2f4646ba5b66bae0933390a`.
The report and sample publication change no engine source.

Use the Linux GNU binary from
[Buck2 release 2026-11-01](https://github.com/meta/buck2/releases/tag/2026-11-01).
It reports `2026-10-31-e8edae33cd7cbc6d042c58bb55a2a900ac6ad489`.
Check the compressed asset SHA256
`a2f7830c240c682c32826ae0b30bf8864d264e866475abaafcc0857d8133cd53`
and decompressed binary SHA256
`769191b5d9da8d95ca8c7ace58a7b34c9535b519c188d5fe8c418387282a60ac`.
Use [prelude commit 02bac0856e1f29d9bb30380072b1819216effe0a](https://github.com/meta/buck2-prelude/tree/02bac0856e1f29d9bb30380072b1819216effe0a),
as selected by that release's prelude_hash asset. Pin both engines and prelude.

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
measure clean outputs and empty action caches, rather than cold-disk reads.

Log compiler, archiver and linker invocations with the same shell wrappers.
Expand Buck2 response files when inspecting effective flags. Keep each engine's
native include paths, dependency tracking, archive flags, link grouping and
helper actions. Buck2 also applies `-fPIC`, `-include cstdint` and
`-D_GNU_SOURCE` to main; Rush's main compile omits those library defaults.
Full command lines and debug paths differ. Treat these as native builds of
matching source bytes; do not infer identical action descriptions, artifact
bytes or isolated scheduler throughput.

## Require actual work and correct output

Accept all 80 clean commands only after counting exactly P+1 C++ compilations,
P archive creations and one link for P libraries. Accept all 80 unchanged
commands only after counting zero compiler, archive and link calls. Accept
all 160 source/header edit commands only after counting exactly one compilation,
one archive creation and one link. Change input values for every edit; keep
the final source edit fixed while changing the header.

Execute the binary after every measured command and warm-up. All 324/324
executions print the expected sum derived from every generated header and
current edit. Use this check to reject stale fixture binaries; require separate
artifact-parity and application tests for broader correctness claims.

## Remove repeated graph work while preserving freshness

Profile base `b003cc3` before changing the larger graph. Its 18,817 nodes include
implicit source-file targets for 18,432 headers. A one-file edit activates only
the changed library and its binary sink, yet graph preparation and retained-map
copying scale with the whole graph. Compiler, archive and link action timing
accounts for about 65 ms in the initial diagnostic profile.

Retain immutable archive boundaries, canonical forest keys, genrule indices
and binary header layouts. Prepare providers for the active targets and their
direct dependency boundary, with full-map fallback for compatibility lookups
and scheduler/producer validation. Borrow the retained artifact forest and
publish a small replacement overlay under the existing generation, coverage,
ledger and watcher gates. Preserve copy-on-write for outstanding readers.

Partition analysis dependencies by established watcher coverage and directly
validate every uncovered dependency. Keep metadata checks authoritative,
preserve process-probe ordering, and reanalyze changed analysis values even
when they also belong to an action ledger. Keep unhealthy/overflowed watchers
and lost continuity on the full-validation path. Cache immutable layout rather
than include contents; reread header reachability on every command.

Correct the source digest memo too: a same-size write with restored mtime must
invalidate through ctime. Include device and mode in the signature, retain
thin-archive member tracking, and version the legacy ActionId namespace to
reject potentially stale older action-cache entries. Restart older workspace
daemons when replacing the executable. Expect a cache miss on the legacy
namespace transition; rebuilt executable identity also invalidates old cold
certificates. Avoid a redundant thin-archive probe only after the memo has
classified a file as plain.

## Check the changed behavior

Run the [daemon controls](../../tests/incremental_perf_smoke.py) outside the
performance samples. Pass all 16 cases: clean/unchanged builds, an aged-header
same-size edit with restored mtime, unchanged reuse after an edit, identical
content touches, missing/corrupt executables, missing objects/archives,
compile failure/recovery, changed include reachability and a loaded rule file
that is also an action input. Check three successive daemon profiles for fresh,
nonempty phase records. Stop only the harness's verified task-local daemons.

Verify the frozen source on both approved runners. Pass the locked workspace
check, all five intermediate commit checks, action/graph/incremental/verified
suites (430 passed, one ignored doctest), C++ rules (195 passed, one ignored)
and command crates (47 passed). Run checks on Host D with Rust 1.96.0; keep
these correctness results separate from Host C's release timings.

Retain the orchestration result: 1,091 passed, eight failed, two ignored.
The eight failures belong to the reproduced base set: Modular/configuration
closure, external snapshot/replay and Kbuild provenance/capture. Two replay
failures vary across base runs. Broader serial library checks retain Starlark
254 passed/eight failed and TensorFlow rules 69 passed/two failed, with matching
failure sets reproduced on unchanged base `b003cc3`. The full analysis test
command also fails to compile the unchanged `cold_load_attribution` integration
test because its initializer lacks `put_symbols_cache` and `resolve_lazy_loads`;
reproduce that error on the base. Claim no full-workspace test pass.

<details>
<summary>Locate the inherited failures</summary>

Run each command separately against the named engine revision and base:

```sh
cargo test -p rush-build --lib --locked -j8 -- --test-threads=1
cargo test -p rush-starlark --lib --locked -j8 -- --test-threads=1
cargo test -p tensorflow-rules --lib --locked -j8 -- --test-threads=1
cargo test -p rush-starlark --test cold_load_attribution --locked -j8 --no-run
```

Orchestration failures:

```text
build_orchestrator_ext::source_file_resolution_tests::modular_pycross_environment_alias_keeps_default_host_selection
build_orchestrator_ext::source_file_resolution_tests::semantic_config_setting_errors_are_deferred_to_the_configured_closure
external_analysis_snapshot::tests::initial_state_requires_owned_shape_and_matching_observation
external_replay::tests::restart_noop_cas_restore_and_dirty_closure_are_make_free
external_replay::tests::sandboxed_uncacheable_action_executes_on_every_replay
kbuild_snapshot::tests::generated_input_provenance_censuses_directory_effect_epochs
kbuild_snapshot::tests::generated_input_provenance_partitions_analyzer_directories
kbuild_snapshot::tests::graph_capture_infers_and_replays_exact_producer_edges
```

Starlark library failures:

```text
builtins::tests::test_tf_additional_lib_deps
bzl_loader::tests::env_on_existing_repo_with_build_skips
bzl_loader::tests::test_repo_root_affects_workspace_bzl_resolution
bzl_loader::tests::test_resolve_relative_bzl
bzl_loader::tests::test_resolve_workspace_bzl
config_defines::tests::build_setting_values_are_scoped_but_flag_conditions_fail_closed
evaluator::tests::test_function_call
tests::test_function_def
```

TensorFlow rule failures:

```text
tf_proto_library::tests::scheduler_required_tools_create_grpc_library_includes_grpc_plugin
tf_proto_library::tests::shape_unsupported_fires_when_create_grpc_library_set
```

</details>

## Extend the comparison with new evidence

Retain the [five-sample source and report](https://github.com/rushbuild/rush/blob/b003cc3a3f068449510ba69e9ef1070c4598cc4a/docs/benchmarks/buck2.md)
and its linked data as a separate record. Exclude setup, ancestor-watch,
intermediate optimization and profiler diagnostics from the current table.
Preserve the rejected stale-memo and overlong socket-path controls; do not count
failed attempts as passing cases. Preserve the cache-reset diagnostic that
required clearing the daemon's home CAS before a purported clean build.

Run optimized and compilation-heavy graphs, more varied edits, an independently
measured second host and remote timings before extending the speed claim.
Check daemon job-limit/disk-cache option routing and restart transitions
separately. TensorFlow, Modular and Linux were not built with Buck2 here;
retain their reference-engine reports without assigning Buck2 a rank.
