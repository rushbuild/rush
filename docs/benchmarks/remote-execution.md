# Qualify remote execution

Execute declared actions through a standard REAPI backend and check the
returned artifacts. Rush passes the bounded C++ and file-action checks in
both host directions. Buck2 builds the same small C++ graph on the same
backend. A remote CUDA readback passes; HIP memory initialization fails.
Use these results to evaluate the interface within its tested scope.

## Freeze the implementation and tools

Run date: 2026-11-08, continuing into November 9 UTC. Engine commit:
[`7e185b1`](https://github.com/rushbuild/rush/commit/7e185b1049f0253b682ddee3c3de378c263e6f9e).
Engine candidate tree:
`d5b4df7738e901b8e94ee0ef771177c3cf46b6af`. All 2,006 candidate paths are
checked against their content hashes on the two runners. The report and
website updates change no engine source. The recorded unit-suite tree differs
only in smoke-harness shell quoting; Rust inputs and the release binary
match. Native cases also pass with quoted workspace paths. Build the release
binary with
Rust/Cargo 1.90.0, `cargo build --release --locked -j8`. Its SHA256 is
`bb59c5c93317f1e31b0b553ef25fdf8b28ed04878408277e81e066d71f896133`.

Use NativeLink 1.8.3's Linux x86-64 musl release from
[the upstream release](https://github.com/TraceMachina/nativelink/releases/tag/v1.8.3).
The compressed asset SHA256 is
`1a6596c96f059d4e99f7352164321e1f1cdff435275667ad8489e83f7e2977c5`.
Use separate persistent stores and one isolated worker per host. Bound the
CAS to 2 GiB, action cache to 100 MiB and worker fast CAS to 2 GiB. Limit
workers to eight CPUs. Route loopback REAPI listeners through authenticated
SSH tunnels. Clients and workers share no source or output directory.

Host C runs GCC 16.2.0; Host D runs GCC 15.3.0. Select them with distinct
immutable toolchain properties. Use Host C as client and Host D as worker,
then reverse the roles. Both hosts provide disk-backed CAS and task scratch
space. These small transfers establish no storage-capacity or throughput
qualification. Host C uses mixed memory kits without a full memory-stability
qualification; retain that limit when interpreting its passing checks.

Use the pinned Buck2 binary and prelude in
[the local comparison report](buck2.md#freeze-the-inputs): binary release
2026-11-01, revision `e8edae33cd7cbc6d042c58bb55a2a900ac6ad489`, prelude
`02bac0856e1f29d9bb30380072b1819216effe0a`. Copy the prelude into the test
workspace rather than linking to client-private files absent on the worker.

## Exercise file actions

Run [tests/reapi_smoke.py](../../tests/reapi_smoke.py) with a release binary,
endpoint, worker toolchain property and fresh output directory:

```sh
python3 tests/reapi_smoke.py --rush /path/to/rush \
  --endpoint http://127.0.0.1:9000 --toolchain my-gcc-image-v1 \
  --output /path/to/new-results
```

The harness uses distinct source bytes per run and checks these 16 cases
in each direction:

- Execute a C++ program and verify its content-dependent output.
- Delete outputs and restore the same executable bytes from the action cache.
- Edit the source, then revert it and reuse the original cached result.
- Run twice with remote cache reuse disabled.
- Upload and restore a 4 MiB file through the streaming path.
- Return exit status 7 twice with failure logs and no successful-cache claim.
- Reject missing outputs, absolute and relative output symlinks, output
  traversal and input symlinks; preserve an existing sentinel output.
- Enforce a worker timeout and report a client deadline expiration.

The client also rejects malformed/extra/duplicate returned outputs and invalid
CAS digests in unit coverage. A separate controlled CAS corruption check
verifies that bad downloaded bytes preserve the existing destination.

## Exercise native C++ builds

Run [tests/reapi_native_smoke.py](../../tests/reapi_native_smoke.py) with the
same four harness options. Generate two libraries with two headers each and
one executable. Set `RUSH_CXX=g++` and `RUSH_LINKER=bfd`; block client compiler
and archiver execution while permitting version discovery. Require the first
executable to print `rushbench total=98480`.

Nine checks cover first build, missing-output restoration, source and header
edits, unchanged reuse, damaged executable repair, unavailable backend,
compile failure and the absence of local compiler/archiver execution. They
pass in both directions. A separate unsupported-genrule check rejects the
request before producing an output. The first reverse attempt selected a
fast linker
available only on the client and failed remotely. Pinning the common linker
repairs that worker profile; it is an environment correction, not an engine
speedup.

Buck2 builds the same generated source bytes on Host D through its native
C++ prelude with `--remote-only -j8`, `local_enabled=False`,
`remote_enabled=True`, `remote_output_paths="output_paths"`, and the same
`OSFamily`/`toolchain` worker properties. Configure its engine, CAS and action
cache clients for that loopback gRPC endpoint with TLS disabled inside the
authenticated tunnel. Supply the prelude's system C++ and bootstrap Python
toolchains. Use C++17, `-O0` and `-g`. The Buck2 toolchain also specifies
`-fPIC -include cstdint -D_GNU_SOURCE`; retain each engine's native rule
behavior for the remaining arguments. This is a functional comparison, not
an assertion of identical compiler argument vectors or output bytes. Both
engines submit three compilations, two archives and one link.
Buck2 reports six remote actions and zero local actions; its executable
prints the same expected total. Buck2 edits, restoration, failure injection
and GPU execution remain unrun. No remote timing ratio follows from this
small interoperability check. Keep the [local timing table](buck2.md) scoped
to its independently pinned local-execution candidate.

## Check GPU runtime execution

Use [the probe](../../tests/reapi_gpu_probe.py) as an uploaded input to a
`remote-execute` manifest with `no_cache: true`. Allocate 1 MiB, fill every
byte with 109, copy it back and verify every byte before returning JSON.
Schedule each worker within its site's existing GPU allocation mechanism.

| Direction and runtime | Result | Assertion |
| --- | --- | --- |
| Host C → Host D, CUDA driver API | PASS | Actual device allocation/fill/readback; 1 MiB of verified bytes restored to client |
| Host D → Host C, HIP 6.4.3 | FAIL | Device enumeration succeeds after namespace provisioning; internal blit-kernel compilation fails and memory fill exits with signal 11 |
| Host D → Host C, existing HIP SDK 7.14 | FAIL | Device enumeration and allocation proceed; `hipMemset` returns status 2 |

The passing readback SHA256 is
`a00d1a356de13b72a2b0ac1338e5cd6f2fd0c02dcb37bcfd06160c85a69c33bb`.
The SDK retry mounts existing runtime libraries read-only; it changes no host
driver or shared software environment. Preserve both failed HIP profiles.
These cases measure runtime/device access and artifact transfer, not GPU
compilation or compute performance.

## Preserve rejection and validation results

Isolate the complete NativeLink process, not just its action subprocess.
Qualification found that NativeLink 1.8.3 can dereference an absolute output
symlink from its collector's filesystem and return it as a regular file.
The passing suite excludes that filesystem from the whole backend's namespace
and rejects reported symlinks. This is a required worker deployment control;
it is not a claim that the client can detect a dishonest backend.

The locked workspace check passes. CLI, command identity and remote unit
suites pass 18, 19 and 17 tests respectively, with none ignored. Cache,
repository and workspace suites pass 87 tests including doctests, with seven
ignored integration tests retained. The final orchestration suite records
1,086 passed, six failed and two ignored; unchanged base `203ae67` records
1,084 passed, eight failed and two ignored. Preserve the exact case names in
[the machine-readable acceptance summary](data/remote-execution-20261108.json).
Two replay failures vary between runs; the final candidate introduces no
additional failing case in this comparison. These failures keep the full
workspace baseline task open.

TLS deployment, nonempty instance routing, authenticated metadata, another
backend implementation, strict low-batch-limit integration, cancellation,
recovery and native external-repository/dynamic-dependency builds remain
unqualified. TensorFlow, Modular and Linux remote builds remain unrun.
Follow [the interface guide](../remote-execution.md) and extend acceptance
before expanding those claims.
