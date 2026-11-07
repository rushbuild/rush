# TensorFlow build status

> Historical report imported from the published rush-status revision
> `844d03c2c47f2fd2094ee5592015018238258290`. Results below describe its original
> candidates and workloads. They were not rerun for the source-release
> baseline `8c6bb55e39de7d78283a3e57b0b89df11ee773ea`.
> Statements such as "now", "current" and "what works" refer to that
> report, not newly verified behavior of this checkout.

Rush builds TensorFlow from unmodified upstream sources and runs its C++
test corpus. The standard is Bazel parity: if it builds with Bazel it must
build with Rush, and every snag is either a rush bug (fix in rush) or an
upstreamable TF commit — never a hack script.

## Certification at a glance — run these gates

![TensorFlow acceptance gates](../../benchmarks/graphs/tensorflow-certification.svg)

A certificate is the recorded result of one named acceptance gate: its exact
case list, procedure, pass rule, and passing count. Use each row as a separate
gate. Run the action in the middle column, apply the pass rule in the right
column, and award one numerator point for every case that passes. Read **N/N**
as “every case in this named scope passed this exact gate.” Keep the scope,
procedure, and denominator attached whenever you repeat a certificate claim.

| Certificate | Run this gate | Award a pass when |
|---|---|---|
| TF 2.15 CPU `tf_cc_test` — 463/463 | Build and launch every `tf_cc_test` selected by `--test_tag_filters=-gpu`. Track the three `requires-gpu` targets in the GPU lane. | The test executable exits successfully. This is full test-program runtime evidence. |
| Repaired corpus re-sweep — 98/98 | Build each of the 98 formerly environmental-failure targets separately. Run `graph_constructor_test` as a separate runtime check. | The individual target build succeeds. Record the runtime check separately as 56/56 tests passed. |
| Differential correctness — 8/8 | Run the clean-build, one-file edit, incremental-build, and source-restoration sequence. Compare action graphs, cache decisions, and output digests at every step. | The graph stays stable where required, only affected actions miss cache, the edit changes the output digest, clean and incremental outputs match, and restoration returns the baseline digest. |
| Compatibility micro-conformance — 16/16 | Generate the 16 small workspaces and run the matching Bazel and Rush loading, analysis, compile, and link cases. | Rush accepts every Bazel-accepted case and rejects every Bazel-rejected case at the corresponding phase. |
| Python extension build + import — 2/2 | Build each pinned extension, inspect its ELF type and `PyInit_*` export, import it with the pinned Python, and call its smoke API. | The object is loadable, exports the expected initializer, imports, and returns the expected API result. This is import/runtime-smoke evidence. |

Produce a certificate by completing one row: record its exact case set, run
the stated procedure, apply the pass rule, and publish the passing count over
the evaluated count. Treat that N/N record as the test report for that named
gate. Run the dedicated wheel lane to certify wheel assembly, and add pinned
extension rows to expand Python coverage.

## What works

- **TF 2.15 CPU, certified.** Rush builds TF 2.15 and passes 463/463 of the
  `bazel test --test_tag_filters=-gpu` tf_cc_test set (the 3 remaining
  targets are requires-gpu). This includes proto compilation, op-wrapper
  codegen (`tf_gen_op_wrappers_cc`), alwayslink semantics, and full
  cc_binary/cc_test link closures with `--start-group` back-reference
  resolution.
- **libtensorflow_cc.so.** The shared library links, is dlopen-loadable
  (RTLD_NOW), and drives real workloads.
- **GPU / ROCm.** Rush builds and runs GPU tf_cc_tests on ROCm (TF 2.17,
  gfx1100): dlopen-loadable .so, GPU-test macro expansion, compile/link
  parity including response files and the ROCm runtime. A TF-scale GPU
  cc_binary (4056 targets) builds and converges on-device.
- **Configuration fidelity.** `--define` flows into `config_setting` /
  `select()` exactly like Bazel, and the parse cache is keyed by the defines
  fingerprint.
- **Two native Python extensions.** The pinned
  `//tensorflow/python/autograph/impl/testing:pybind_for_testing` target builds
  as ELF `ET_DYN`, exports `PyInit_pybind_for_testing`, imports, and passes its
  method smoke. The larger `//tensorflow/python/platform:_pywrap_tf2` target
  now also builds and imports, exports `PyInit__pywrap_tf2`, and passes its
  `is_enabled()` / `enable(True)` state smoke. This is a 2/2 pinned-target
  acceptance, not broader wheel certification.
- **Differential correctness harness.** The aquery-based incremental +
  hermeticity harness passes 8/8; action outputs are deterministic.
- **Parallel wavefront builds.** The build executor saturates all cores by
  default (`RUSH_BUILD_JOBS` tunes the cap); use mold/lld for large links
  (`RUSH_LINKER`).
- **TF-scale warm no-op.** On the 18,522-dependency manifest for
  `//tensorflow/core:framework`, the daemon measured **9.3 ms p50** over 30
  samples, versus Bazel 64.2 ms (54.5 ms with `--watchfs`). The watcher skips
  source revalidation; this is a scoped target measurement, not a whole-tree
  timing claim.

## Corpus repair, 2026-08-24

A sweep of the 98 corpus targets that had been filed as "environmental"
turned out to be six engine defects. Each one presented as a failure far
from its cause, which is why they read as host problems.

| Symptom the build printed | What was actually wrong |
|---|---|
| `This file was generated by a newer version of protoc` | TF's proto rules ran a bare `protoc` from `PATH` while the headers came from the pinned protobuf. rush generated with 3.12 and compiled against 3.21 |
| `conversion_data.c ... doesn't exist in the graph` | The label names a genrule OUTPUT FILE. rush had no output-file-to-producer map and looked for a target of that name |
| `undefined reference to absl::Cord::Cord(std::string&&)` | The pinned Abseil archives were built by a different compiler. The symbol was present under a different C++ mangling. rush recorded the build toolchain in a stamp file and never read it |
| `undefined reference to EVP_PKEY_free` | rush could not load `@rules_cc//cc:defs.bzl`, so BoringSSL's BUILD failed to parse, `@boringssl//:crypto` was stubbed, and the build continued |
| 52 undefined `riegeli` vtables | Same shape, one repo over: `@rules_proto//proto:defs.bzl` would not load |
| `@onednn//:mkl_dnn` stubbed on every target | TF's WORKSPACE names itself `org_tensorflow`; vendored BUILD files load `@org_tensorflow//third_party/mkl`. rush treated its own workspace as an unknown external repo |

Two patterns account for all six:

1. **rush resolved one tool in two places and the answers disagreed** —
   protoc, the C++ toolchain, the Abseil archives.
2. **rush met bad state, warned once, and continued** — a BUILD file that
   would not parse became an empty stub, and the parse error surfaced
   hundreds of archives later as an undefined symbol naming nothing
   related.

Both are now addressed. A stubbed package records the error that caused
it and names itself in whatever failure follows, including on builds
that succeed. `common_runtime:graph_constructor_test`, one of the 98,
builds and runs **56 of 56 tests green**.

Four more defects of the same two shapes surfaced after that table was
written:

| Symptom | What was wrong |
|---|---|
| `@local_xla//tsl/...bzl not found` | TF vendors TSL inside XLA. `dependency_graph.rs` remapped that for dependencies; `.bzl` loads took another route and did not. The corpus host had carried an **untracked symlink** since 2026-08-23 to paper over it, so the build worked only there |
| generated protos rejected by their own version guard | The generator used the resolved protoc while the compile hardcoded `-I/usr/include`. Fixing one half just reversed the error direction |
| `@highwayhash//:hh_types` and friends stubbed | TF supplies highwayhash's BUILD (upstream is a Makefile project) declaring 23 labels; rush synthesized 2 |
| `@com_google_protobuf//:protobuf_lite` stubbed | A real upstream target rush never exposed, though the bootstrap already builds `libprotobuf-lite.a` |

The last two were found **on builds that passed**. A successful build
now lists what it stubbed, and `scripts/audit_stub_labels.sh` compares
every synthesized repo BUILD against the labels the project declares.
That audit originally reported seven repos with gaps, including `@png`,
`@pybind11` and `@linenoise`. All seven have since been replaced with real
targets; the audit now reports zero missing declared labels.

**Certified 2026-08-26: the full re-sweep reports 98 of 98 OK.** The
sweep ran every target one at a time against the pushed engine. Six of
the 98 needed two more engine fixes found after the table above was
written: a dependency-discovery predicate treated any label containing
`_test_` as skippable, so a cc_library test helper was never parsed and
five grappler tests died on its missing symbol at link; and `format()`
in Starlark rejected `"{0}"` indexed fields, so one package's .bzl
failed to evaluate, was stubbed, and surfaced as an unrelated undefined
symbol. Both carry regression tests.

The certification itself forced two cache-layer fixes. The
content-addressed store had no eviction and wrote 415 GB of blobs in
under an hour, aborting the first sweep on a full disk; blobs now evict
least-recently-used to a 50 GiB budget. Validating that exposed the
second: the ActionResult gc walked the cache root recursively and
enforced its 10 GB budget against the CAS's blobs, which is where CAS
bytes had been quietly going on daemon hosts. Each store now bounds
only its own files.

## Closed since the original TODO was written

Validation machines are identified only as host A and host B. They are
independent x86 systems; their private hostnames are intentionally omitted.

- **TF-scale no-op:** the daemon/inotify design is landed. The scoped
  18,522-dependency TF measurement is 9.3 ms p50 versus Bazel 64.2 ms;
  repeated commands skip load/analyze work.
- **Core Python rules:** `py_library`, `py_binary`, and `py_test` now execute
  as real rules. The Modular whole-repo certificate includes 1,063 real
  Python builds. The first pinned TF native-extension acceptance now also
  passes; broader extension and wheel coverage remain open.
- **Generator and output plumbing:** `$(RULEDIR)`, `$(execpath)`,
  plain-source `$(location)` spellings, import-root `PYTHONPATH`, and
  same-package `py_binary` tool execution work. Conflicting declared
  generator outputs are rejected package-wide, and generated headers can be
  consumed directly by `cc_binary`. Duplicate target declarations are now
  rejected during BUILD loading. Undefined main-repository dependencies now
  fail after real resolution is exhausted, and malformed main BUILD files fail
  at their loading cause. Singular link-option `$(location)` and `$(execpath)`
  now resolve exact direct prerequisites with one `DefaultInfo` output, and
  generated link prerequisites stay typed through analysis and execution. This
  closes the `quantized_function_library` blocker, five micro-conformance gaps,
  and the first TF native-extension version-script blocker.
- **Stub-label audit:** all seven reported repository gaps now have real
  targets; the audit reports zero gaps.
- **Explicit private visibility:** rule-target dependencies marked
  `//visibility:private` are accepted within their repository package and
  rejected across package or repository boundaries.
- **Package visibility defaults and pseudo-targets:** Rush main
  `88fa9f04180e` preserves omitted, `None`, and explicit-empty visibility as
  distinct states, applies `package(default_visibility)` after macro expansion,
  and enforces repository-exact `__pkg__` and `__subpackages__` boundaries.
  Same-package access remains unconditional, explicit target visibility wins
  over the package default, and an unsupported `package_group` that affects a
  decision fails loudly. Host A passes lazy and eager package parsing, eight
  focused visibility controls, the wrapped `cc_binary` analysis-error gate,
  744/744 serialized `rush-build` library tests, 187/187 `rush-rules` tests
  with one intentional ignore, 28/28 `rush-incremental` tests, and a release
  build. Host B was not run for that candidate, so no second-host result is
  claimed.
  **UNFIXED at that commit:** the no-explicit/no-package-default fallback was
  still permissive, `package_group` expansion was absent, and `exports_files`
  plus generated/output-alias inheritance remained open. A later Bazel 6.1
  control established that evaluation-time `native.existing_rules()`
  intentionally reports empty visibility for omitted, `None`, and
  explicit-empty values even under a public package default; Rush's matching
  observation is parity, not an open bug. Core `package_group` expansion is
  closed by `cbd671493e1f` below. The private no-default fallback and
  synthetic-alias inheritance are closed by `13fe9c9b9a5d`; the remaining
  compatibility items stay open.
- **Subpackage boundaries:** explicit source labels, `exports_files`, and
  package-relative `glob()`/`native.glob()` now stop at nested `BUILD` or
  `BUILD.bazel` packages in both the main workspace and external repositories.
- **Daemon-safe subpackage freshness:** the parsed-BUILD cache now records its
  tracked dependency manifest and validates it at the start of each command.
  Adding or removing a nested `BUILD`/`BUILD.bazel` marker therefore reparses
  stale glob results in both main and external repositories, while repeated
  reads within one command retain the cache hit. After integration, host A
  passes 735/735 `rush-build` library tests, 28/28 `rush-incremental` tests,
  and the persistent cache-replay integration gate. Host B passed the focused
  marker tests and incremental suite; its remaining full-suite rerun was not
  completed and is not waived.
- **Daemon-safe loaded `.bzl` values:** Rush main `a21b059c1ca6` gives the
  process-global loaded-symbol cache a cross-command admission gate. Each
  generation carries a canonical transitive tracked-I/O manifest that is
  validated before reuse; same-generation hits replay the admitted dependency
  set, and generation-conditional removal cannot evict a concurrent refresh.
  Main-workspace and external-repository controls edit only a transitively
  loaded leaf `.bzl` and confirm that the next command observes the change.
  Host A passes 740/740 serialized `rush-build` library tests, 28/28
  `rush-incremental` tests, and a release build. Host B was not run for that
  candidate, so no second-host result is claimed. **UNFIXED baseline:** the
  default-parallel suite still has three watcher-timing failures (11/11 pass in
  isolation), serialized `rush-starlark` remains 202/217 with the same 15
  tracked-I/O/config-lock failures as the pre-change control, and the full
  `rush-build` package still reproduces its `hello_cpp` failure at exact
  `8d2ae93`.
- **Deferred `select()` semantics:** configurable values, including `+` and `|`
  composition, now remain unresolved until the active configuration is known.
  A `select()` with no matching branch and no default fails during analysis,
  bringing the default micro-conformance suite to **16/16 on host A and
  host B**.
  TensorFlow-scale expressions use a compact shared DAG: the pinned
  `//tensorflow/core:framework` gate analyzes 5,313 total nodes, 1,178 scoped
  nodes, and 589 actions with zero missing producers and 69 MiB peak RSS. That
  gate caught and eliminated an earlier Cartesian representation that reached
  161 GiB during validation.
- **Python native-extension graphing:** `py_library(data)` now contributes
  dependency edges without leaking data-only `PyInfo`. The first TF pybind
  wrapper now reaches 14 graph nodes, eight scoped nodes, and four actions with
  zero missing producers; a real build reaches the extension compile rather
  than returning false success.
- **Pinned pybind11 headers:** Rush now verifies TensorFlow's pybind11 v2.10.4
  bootstrap checksum and witness, atomically materializes the repository, and
  exposes the root target with 24 declared headers plus its real
  Python-runtime dependency. On host A and host B the materializer passes
  728/728 `rush-build` tests and release builds. The exact TF gate now resolves
  `@pybind11//:pybind11` and follows the transitive Python-runtime alias rather
  than using a host include.
- **Configured Python headers:** Rush selects strict `RUSH_PY3` or
  deterministically discovers TensorFlow's prefetched hermetic interpreter,
  queries it through tracked subprocess IO, and atomically materializes a
  typed `@local_config_python//:python_headers`. The combined host A gate adds
  five configured-repository targets and 163 implicit files; its copied
  `Python.h` hash exactly matches the pinned Python 3.10 source and the real
  `pybind_for_testing.o` compile completes. This configured header path is now
  part of the passing first native-extension acceptance.
- **Shared Python-extension output:** `cc_binary(linkshared = 1)` now matches a
  Bazel 6.1 control: booleans and integer 0/1 are accepted, other integers are
  rejected, compilation is PIC, the compiler driver links `-shared`, and the
  target name is the output filename verbatim. Scheduler, runtime, legacy, and
  recursive paths agree; host A and host B produced loadable ELF objects whose
  `ctypes` smoke returned the expected value. The pinned TF pybind target now
  analyzes as a `SharedLibrary` with eight targets, four actions, zero missing
  producers, and 34 MiB peak RSS.
- **First TF native-extension acceptance:** Rush main `8d2ae93db919` expands
  singular `$(location)` / `$(execpath)` references for exact direct
  prerequisites and carries generated link inputs as typed dependencies. On
  host A, the pinned
  `//tensorflow/python/autograph/impl/testing:pybind_for_testing` target builds
  successfully; the result is ELF `ET_DYN`, exports
  `PyInit_pybind_for_testing`, imports in Python, and
  `TestClassDef().method()` returns `None`. Host B was not run for this gate, so
  no second-host result is claimed.
  Plural `$(locations)` / `$(execpaths)` forms and
  generalized filegroup expansion remain **UNFIXED**, as does broader
  extension and wheel coverage.
- **Cross-repository proto compile includes:** Rush main `f5255a993caa`
  propagates transitive `CcInfo` include roots into `tf_proto_library`
  scheduler, runtime, and legacy compile commands while retaining generated
  headers as typed action inputs. A fresh host A build of
  `//tensorflow/python/platform:_pywrap_tf2` produced the previously failing
  main-repository and `@local_tsl` `error_codes.pb.o` objects and advanced to
  the extension source compile. The focused regression and an exact-main
  release build pass. That run next exposed the generic `cc_binary` include
  defect closed by `5b62bade4a8c` below; the extension is still not counted as
  certified. Host B was not run for that candidate, so no second-host result
  is claimed.
- **Package-rooted generic `cc_binary` compiles:** Rush main `5b62bade4a8c`
  makes the workspace root, registered external workspace roots, and
  `genfiles` unconditional runtime include roots instead of adding them only
  under ROCm or CUDA. A real host A compile/link regression resolves
  `dep/public.h` through a typed dependency `CcInfo`, retains the header as an
  action input, and checks each root appears exactly once. Host A passes 188/188
  `rush-rules` tests with one intentional ignore and a release build. The
  pre-change TensorFlow control failed to find
  `tensorflow/core/platform/enable_tf2_utils.h`; Bazel's matching aquery
  carries its workspace quote root. That run next stopped at the fail-closed
  `//tensorflow:internal` package-group gate; `cbd671493e1f` closes it below,
  and `_pywrap_tf2` is now the second certified extension. Host B was not run
  for that candidate, so no second-host result is claimed.
- **Package-group visibility expansion:** Rush main `cbd671493e1f` carries
  `package_group` package specifications and transitive `includes` as graph
  metadata, so visibility-only groups lazy-load without becoming provider or
  build-order dependencies. It implements exact, subtree, repository-root,
  negative, declaring-repository, and include-union semantics; same-package
  access remains unconditional, while unknown groups, wrong target kinds,
  invalid patterns, cycles, and nonmembers fail loudly. Host A passes 7/7
  focused controls, 750/750 serialized `rush-build` library tests, 188/188
  `rush-rules` tests with one intentional ignore, lazy and eager package
  parsing, a release build, and 15/15 Bazel 6.1 differential outcomes. The
  exact `//tensorflow/core:lib_internal --analyze-only` acceptance passes with
  916 scoped nodes and 444 complete actions, crossing the former
  `//tensorflow:internal` blocker. The corresponding fresh-output build also
  exits zero and materializes 505 payload files (305 ELF objects, 100 generated
  `.pb.cc`, and 100 generated `.pb.h`) in a 221 MiB output tree, including both
  `lib_internal_impl` objects. Raw evidence remains private. That invocation
  did not certify reusable retained state, so its next command remains
  conservative;
  no warm incremental claim is made. A clean-output-base Bazel 6.1
  `sync --only=llvm-project` control also passes. The first failed
  `@llvm-project` probe omitted both the standard clone cache and Bazel PATH, so
  that probe alone was inconclusive. A later exact-client run isolated a Rush
  defect: the loader fallback ignored the client's environment. Rush main
  `1bf4f689ca7b` closes it below. Loading-time package-group validation is
  closed by `9b5adddf761d`. **UNFIXED:** Rush does not model Bazel's compatibility
  flag for `public` / `private` package specifications.
- **Second TF native-extension acceptance:** On current main, a fresh-output
  host A build of `//tensorflow/python/platform:_pywrap_tf2` exits zero and
  produces a 49,940,920-byte ELF `ET_DYN` object with SHA-256
  `af63739ba089fd5a99c21a2e7ca760e958ebd79b3200951625a9845e577654a5`.
  It exports `PyInit__pywrap_tf2`, all dynamic libraries resolve, and the
  pinned hermetic Python 3.10 imports it. The API smoke observes disabled,
  calls `enable(True)` with a `None` result, observes enabled, and restores the
  original state. The raw build log and artifact remain private. This raises
  the pinned native-extension certificate to **2/2**; it does not certify a
  wheel or the
  complete extension set. Host B was not run for this gate, so no second-host
  result is claimed.
- **BCR source and patch materialization groundwork:** Rush main
  `b31310090a6a` extends the experimental native Bzlmod resolver to fetch an
  absent versioned module from a Bazel Central Registry, verify the source
  archive and every ordered patch with SHA-256, apply patches with zero fuzz,
  and publish only the complete patched tree through unique staging and an
  atomic rename. It validates Bazel module coordinates, normalizes relaxed
  SemVer build metadata, rejects unsafe patch paths, and fails closed on
  unsupported materialization fields. On host A, 15 focused tests pass with one
  intentional network ignore, the separately enabled cold
  `grpc@1.74.1` BCR patch-and-reuse test passes, all 760 serialized
  `rush-build` library tests pass with one ignore, and a release build passes.
  This is a real source/patch primitive, not a MODULE-only build certificate:
  the native resolver is still experimental and is not wired into the
  production MODULE-only or daemon analysis path. **UNFIXED:** mirrors,
  explicit archive types, overlays, MVS, module-extension evaluation,
  `use_repo` mappings, production resolution/certificate persistence,
  permissive transitive parsing, immutable-tree rehashing, and host-patch
  portability. Host B was not run for that candidate, so no second-host result
  is claimed.
- **Native Bzlmod input and freshness prerequisite:** Rush main
  `92f678c64918` removes the unsafe constructor-time experimental invocation
  while making the resolver's local inputs explicit. Root and transitive
  `MODULE.bazel` files, ordered vendored candidates, symlink targets, and
  `RUSH_BZLMOD_*` settings are tracked; resolutions round-trip through serde,
  while a separately canonicalized semantic fingerprint supplies stable
  identity. Invalid repository names, apparent-name collisions, and unsupported
  MODULE semantics fail closed. `MODULE.bazel` also participates in command
  configuration invalidation, and zero-stat watcher admission falls back to a
  full manifest check for dependencies outside continuous analysis coverage.
  On current main, host A passes 26 focused Bzlmod tests with one intentional
  network ignore, 29 watcher tests, five workspace-configuration tests, a
  790-test affected suite with six ignores after filtering five exact-base
  tests and two exact-parent visibility fixtures, and a release build. The
  provisioned unmodified-TF
  `//tensorflow/core:lib_internal --analyze-only` control resolves 916 scoped
  targets and 444 complete actions with zero missing producers in 73.9 MiB RSS.
  Raw evidence remains private.
  This is a freshness prerequisite, not production native-Bzlmod wiring, and TF
  2.15 is a WORKSPACE regression gate rather than a MODULE-only certificate.
  Host B was not run for that candidate, so no second-host result is claimed.
- **Client-scoped Bazel repository fallback:** Rush main `1bf4f689ca7b`
  fixes the loader-side compatibility path that read the resident process
  environment and spawned a bare `bazel`. It now resolves an exact executable
  from tracked per-command `RUSH_BAZEL_PATH` or `PATH`, rejects empty, missing,
  non-executable, and non-UTF-8 values without falling back to the host, tracks
  `RUSH_WORKSPACE_CACHE_ROOT`, and preserves the operating-system spawn cause.
  A Bazel 6.1 control materializes and queries TensorFlow's declared
  `@llvm-project` repository. With the fix, a real host A TensorFlow analysis
  loads `@llvm-project//mlir:tblgen.bzl` and advances to the separate
  `package()`-ordering blocker. Host A and host B each pass seven focused
  Bazel-loader tests and the broader eight-test filter. The Bazel fallback is
  still a compatibility bridge, not native repository resolution.
- **File-wide package metadata ordering:** Rush main `c4eaddf3b196` accepts a
  `package()` declaration after package groups or ordinary rules and applies
  its defaults retroactively to every eligible rule in the file, including
  rules created by loaded macros. Package groups and exported source files do
  not inherit rule defaults. Host A and host B each pass two focused ordering
  tests and six broader package tests; four Rush CLI controls and matching
  Bazel 6.1
  controls cover direct declarations, loaded macros, `native.package()`,
  `package_group`, and `exports_files`. A real host A TensorFlow analysis now
  evaluates the formerly failing BUILD file and creates 147 graph nodes. Its
  next independent blocker is the missing
  `@pybind11_bazel//:build_defs.bzl` load.
- **Private default visibility and synthetic aliases:** Rush main
  `13fe9c9b9a5d` treats ordinary targets with neither explicit visibility nor
  a package default as private across packages while retaining unconditional
  same-package access. `source_file` and `config_setting` preserve their
  current compatibility behavior, synthetic external system-library stubs are
  explicitly public, and generated-file, generated-output, shared-library,
  and implicit-proto aliases inherit their producer's visibility. On host A,
  Bazel 6.1 accepts a public generated output and rejects a private one, 11/11
  focused visibility controls pass, all 762 serialized `rush-build` library
  tests pass with one intentional ignore, and a release build passes. A real
  TensorFlow `//tensorflow/core:lib_internal --analyze-only` run resolves 895
  scoped targets and all 421 actions with no missing producers. **UNFIXED:**
  `exports_files` and config-setting compatibility selection, nonconfigurable
  package-group attribute validation, and compatibility-mode public/private
  package specifications. Host B was not run for that candidate, so no
  second-host result is claimed.
- **Evaluation-time visibility parity:** A Bazel 6.1 control calls
  `native.existing_rules()` after declaring targets under
  `package(default_visibility = ["//visibility:public"])`. Omitted visibility,
  explicit `None`, and explicit empty visibility all appear as `()` during
  evaluation. Cross-package analysis then accepts the omitted and `None`
  targets because they inherit the package default, while rejecting the
  explicit-empty target. Rush already has the same two-stage behavior, so the
  proposed eager rewrite was discarded and no Rush code change was made.
- **Exported-source visibility and reachable package-group validation:** Rush
  main `2b5302c47852` gives `exports_files()` omission and explicit `None` the
  Bazel 6.1 public default independently of package visibility, while retaining
  explicit empty, private, and granted visibility. The metadata survives eager
  and lazy evaluation, the parse bridge, direct package registration, and
  missing-source synthesis; conflicting duplicate declarations fail. Rush also
  detects nested `select()` / configurable values in the nonconfigurable
  `package_group` `packages` and `includes` attributes before platform
  resolution and reports the original cause when the group is referenced.
  Host A passes 32 source-resolution tests, two Starlark export tests, five
  parser export tests, 12 package-group tests, two include-prefix
  integrations, a release build, and 803 serialized `rush-build` tests with
  six ignores after
  filtering one exact-base failure. Host B passes the focused 2, 5, 12, and
  2-test groups plus a release build. **UNFIXED:** config-setting compatibility
  selection and the compatibility flag for public/private package
  specifications. Loading-time rejection is closed by `9b5adddf761d` below.
- **Loading-time package-group validation:** Rush main `9b5adddf761d`
  validates the outer sequence and every element of `package_group` `packages`
  and `includes` while the package loads, across direct, native, and
  macro-expanded calls. An unreferenced malformed group now rejects a requested
  good sibling from the same package, matching Bazel instead of deferring the
  error until a visibility decision. Constant concatenations, tuples,
  comprehensions, and dead branches remain accepted; `Label` values are
  accepted for `includes` and rejected for `packages`. Host A and host B each
  pass three Starlark package-group tests, the type-name test, and 12
  `rush-build`
  package-group tests, with Bazel 6.1 differential controls for outer and
  element types.
- **Android declarations fail at configuration instead of disappearing:** Rush
  main `84266bcca131` replaces the rules_android no-op facade with forwarding
  macros for `android_library`, `android_binary`, and `aar_import`. Each
  declaration is retained as a real graph node. Unrequested host C++ siblings
  remain loadable, while selecting or transitively reaching an Android target
  now fails during configured-target validation with the missing Android
  semantics named explicitly. Host A and host B each pass the focused facade,
  rule-instance, and scoped-configuration tests. Android toolchain selection,
  resource and manifest processing, AAR handling, dexing, and APK packaging
  remain unsupported.
- **Java declarations fail at configuration instead of disappearing:** Rush
  main `256e4b1516c3` retains `java_proto_library`,
  `java_lite_proto_library`, `java_library`, `java_binary`, `java_import`,
  `java_plugin`, and `java_test` as real graph nodes. The TF 2.17
  `rules_java` facade forwards its five build-rule declarations through native
  capture instead of dropping them as no-ops, and direct string dependencies
  remain graph edges. Unrequested host C++ siblings still build, while a
  selected or transitively reached Java target fails before analysis or
  execution. Host A and host B each pass the six focused `rush-build` Java
  tests, the parser and Starlark tests, and the real facade-load regression.
  Java
  toolchains, providers, code generation, compilation, launchers, test
  runners, annotation processing, and runfiles remain unsupported.
- **Configurable visibility and compatibility modes:** Rush main
  `715884a1fff9` loads every real `select()` condition and visibility-only
  package group as analysis metadata, validates condition type and visibility,
  follows native alias chains, and checks groups on direct roots, same-package
  dependencies, configured-condition scope, and direct package-group includes.
  It implements Bazel's four config-setting/package-group visibility migration
  flags, preserves explicit main-versus-external repository identity even when
  both names share one filesystem path, and applies `.bazelrc` common, build,
  named-config, and CLI precedence with a segment-local `--`. Daemon discovery
  now uses protocol generation 1.1 and rejects stale metadata before connect.
  Host A and host B each pass 61 dependency-graph tests, 23 rule-bridge tests,
  one same-path repository-identity test, one alias-default test, eight CLI
  tests,
  and two daemon-rollover tests. Bazel 6.1 differentials on both hosts cover
  canonical defaults, the main-repository `conditions`, `visibility`, and
  `external` packages, package-group modes, and unmatched-select error order.
  The historical host-condition shorthand described at this commit is retired
  by `d86eb65b165a` below.
- **Declared configuration conditions and canonical command routing:** Rush
  main `d86eb65b165a` removes the synthetic main-repository
  `//conditions:{linux,unix,macos,darwin,windows,freebsd,x86_64,aarch64,arm64,
  arm}` meanings. It loads real `config_setting`, constraint, and alias
  metadata; rejects missing, wrong-typed, ambiguous, or invisible conditions;
  and chooses the strict predicate superset deterministically. It scopes definitions,
  repository mappings, and command configuration so one workspace or daemon
  command cannot contaminate another. It routes the public build command
  through this configured dependency graph instead of the legacy builder.
  Host B passes 19/19 configuration-registry tests, 9/9 select tests, 6/6
  typed-dictionary tests, the malformed-predicate gate, 3/3 parser-select
  tests, 189/189 rule tests, 874/874 serialized build tests, and the CLI check.
  The broad Starlark suite has inherited failures: the exact parent fails
  18 tests in parallel, while this series fails nine, so no new failure is
  waived or hidden. Host A passes the commit-specific gates recorded in the
  five atomic commits.
- **Configuration freshness:** `--define` changes are part of C++ action
  freshness. Compiler-discovered header tracking remains open.

## Open work recorded at the historical revision

- **Track compiler-discovered headers.** Declared and generated C++ inputs and
  configuration changes are keyed, but compile freshness must also ingest the
  compiler's actual include dependency set.
- **Extend TF Python and wheel coverage.** Both pinned acceptance targets,
  `//tensorflow/python/autograph/impl/testing:pybind_for_testing` and
  `//tensorflow/python/platform:_pywrap_tf2`, are built, loadable, exported
  through their `PyInit_*` symbols, import-smoked, and API-smoked: **2/2**.
  Extend that proof to more native-extension targets and certify the Python
  wheel slice end to end. Plural location forms and generalized filegroup
  expansion remain open where future targets require them.
- **Full Bzlmod at TF scale.** Source-archive and ordered-patch materialization,
  tracked local resolution inputs, canonical result identity, and conservative
  watcher coverage are proven in the experimental resolver. Persist and invoke
  that real resolution in the production MODULE-only and daemon paths, then
  extend the protobuf-to-gRPC fixture through MVS/version selection, module
  extensions, and `use_repo` mappings. TF 2.15 itself is a WORKSPACE workload,
  so it remains a separate certification lane.
- **Implement, then validate, remote caching/execution.** `--remote_cache` is
  currently unwired and the executor path uses a Rush-specific protocol.
  Implement interoperable Remote Execution API v2 Capabilities, CAS/
  ByteStream, ActionCache, and Execution clients, then prove cross-host cache
  hits and remote compilation.

## Reproduce

```sh
# in an unmodified TF 2.15 checkout
rush build //tensorflow/tools/pip_package:build_pip_package  # or any target
rush build --define tflite_with_xnnpack=false //tensorflow:libtensorflow_cc.so.2.15.0
```

Run the CPU test corpus with the self-contained harness (auto-staged by the
test runner); expected result: 463/463.
