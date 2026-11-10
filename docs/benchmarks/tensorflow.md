# Check TensorFlow builds and program behavior

Build the framework and run a native Tensor consumer using the normal Auto
policy. Keep each selected target and its validation scope attached to its
result before comparing build times.

## Read the accepted 2026-11-10 checks

Use **Rush 2026-11-10 (`ad87da5`)**, commit
`ad87da5188201534d443c40481aba6b57ecc30d6`, tree
`87bb1112a4fe33fd2b3996ee65f4ab06788a5ebd`. The original TensorFlow 2.17.0
source is commit `ad6d8cc177d0c868982e39e0823d0efbfb95f04c`; its archive
SHA256 is `7f188a68313206ebb8284912125066b55af710bbf4c18c6bc00adcaa5ab03960`.

| Case | Selected work | Build interval | Samples | Accepted outcome |
| --- | --- | ---: | ---: | --- |
| C | Native `tensor_roundtrip` consumer and its framework dependency | 652.786 s | 1 | Build and one Tensor runtime pass; source/SDK/output before/after file checks pass, including 234 archives and 645 unique thin members |
| D | `//tensorflow/core:framework` | 674.906 s | 1 | Build and original-source audit pass; framework-subtree inventory contains 57 objects and 46 archives |

Use these single shared-host intervals as correctness diagnostics for
**different selected targets**. Unrelated GPU work was permitted on both
validation systems. The intervals measure the build command; source/profile setup,
C's pre-build SDK snapshot and C's runtime verification are outside them. Collect paired Bazel samples and complete distributions before computing
medians or speed ratios. D has no runtime or
SDK/runtime-bookend result. Use the [sanitized data](data/tensorflow-native-20261110.json)
for full-precision intervals, exact Rush and tool executable hashes,
source/probe digests and
separate integrity scopes.

Both cases use eight CPUs, eight jobs, optimization mode `opt`, the
`repair_cpu` and `monolithic` configurations, Clang 21, and these defines:

```text
framework_shared_object=false
tflite_with_xnnpack=false
tensorflow_mkldnn_contraction_kernel=0
no_gcp_support=true
__TENSORFLOW_NO_LGPL_DEPS__=1
no_aws_support=true
no_hdfs_support=true
no_nccl_support=true
```

Bazel 6.5.0 supplies configuration queries in this native profile. The host
build Python is 3.13; TensorFlow's default hermetic Python selection remains
in place. Keep these tools and configurations explicit in a new record.

Leave `RUSH_CPP_DISCOVERY` unset and run the normal build command.
C records `ConditionalIncludes` at `@fft2d//:fft2d`.
D records `ConditionalIncludes` at `@fft2d//:fft2d`.
That path executes the whole selected invocation locally with reuse disabled.
Run the normal command again; if either supported compiler-execution reason
recurs, expect all selected actions to execute again, including unchanged
repeats. Treat compiler dependency records as observations and validate the
required input closure separately before admitting reusable results. Qualify
reusable full-framework execution before reporting no-op or edit latency.

## Reproduce the target and probe

Start from a fresh copy of the pinned TensorFlow source. Use separate owned
Rush cache, output and runtime directories, a verified dependency-download
cache, local Linux execution and the scheduler disabled. Select Clang 21,
Bazel 6.5.0 and the host build Python explicitly. Keep the original upstream
files and dependency checksums unchanged.

Set `TF_SOURCE` to that TensorFlow workspace and `RUSH_REPO` to this Rush
checkout. Set `DOWNLOAD_CACHE`, `PYTHON_BIN_PATH` and `PYTHON_LIB_PATH` to the
selected download cache and host Python locations. In a fresh workspace,
write this case-local profile after expanding those variables:

```sh
umask 077
test ! -e "$TF_SOURCE/.bazelrc.user"
cat > "$TF_SOURCE/.bazelrc.user" <<EOF_PROFILE
common --repo_env=CC
common --repo_env=CXX
common --repository_cache=$DOWNLOAD_CACHE
build:repair_cpu --action_env=CC
build:repair_cpu --action_env=CXX
build:repair_cpu --action_env=PYTHON_BIN_PATH
build:repair_cpu --action_env=PYTHON_LIB_PATH
build:repair_cpu --python_path=$PYTHON_BIN_PATH
EOF_PROFILE
```

Export `CC`, `CXX` and `RUSH_CXX` for the selected Clang drivers, with
`RUSH_CXX` using the C++ driver. Export the selected `PYTHON_BIN_PATH` and
`PYTHON_LIB_PATH`. Make the selected Bazel available to Rush's configuration
resolver. Set `RUSH_WORKSPACE_CACHE_ROOT`, `RUSH_CACHE_ROOT`,
`RUSH_WORKSPACE_OUTPUT_ROOT`, `RUSH_WORKSPACE_BIN_LINK` and `XDG_RUNTIME_DIR`
to fresh owned locations outside the source tree; keep `RUSH_WORKSPACE_OUTPUT_ROOT` as `TF_OUTPUT` for the runtime command below.
Select an eight-CPU affinity as `EIGHT_CPU_SET` and use eight jobs. Use a
fresh HOME for the record, CPU-only TensorFlow selections
(`TF_NEED_CUDA=0`, `TF_NEED_ROCM=0`, `TF_NEED_TENSORRT=0`) and empty
`CUDA_VISIBLE_DEVICES`, `HIP_VISIBLE_DEVICES` and `ROCR_VISIBLE_DEVICES`.

Set `TARGET=//tensorflow/core:framework` for the framework check. Install the
probe below and choose `TARGET=//rush_runtime_probe:tensor_roundtrip` for
the program check. Build either selected target with the recorded flags and defines:

```sh
unset RUSH_CPP_DISCOVERY RUSH_USE_SCHEDULER
cd "$TF_SOURCE"
taskset -c "$EIGHT_CPU_SET" rush build "$TARGET" -j 8 -c opt \
  --config=repair_cpu --config=monolithic --daemon=auto \
  --define framework_shared_object=false \
  --define tflite_with_xnnpack=false \
  --define tensorflow_mkldnn_contraction_kernel=0 \
  --define no_gcp_support=true \
  --define __TENSORFLOW_NO_LGPL_DEPS__=1 \
  --define no_aws_support=true --define no_hdfs_support=true \
  --define no_nccl_support=true
```

For the program check, add these two published files before running the
build command:

```sh
mkdir -m 700 "$TF_SOURCE/rush_runtime_probe"
install -m 600 "$RUSH_REPO/benchmarks/workloads/tensorflow-probe/probe.cc" \
  "$TF_SOURCE/rush_runtime_probe/probe.cc"
install -m 600 "$RUSH_REPO/benchmarks/workloads/tensorflow-probe/BUILD.bazel.in" \
  "$TF_SOURCE/rush_runtime_probe/BUILD.bazel"
```

Set `TARGET=//rush_runtime_probe:tensor_roundtrip`, run the build command,
then run `"$TF_OUTPUT/tensor_roundtrip"`. Require exit zero and exactly
`TF_TENSOR_ROUNDTRIP_SLICE_OK` followed by a newline. The program checks a
float Tensor of shape `[2, 3]`, TensorProto content serialization/restoration,
and a `[1, 3]` row slice with nondefault values.

The fixture is copied verbatim from the accepted runtime input: `probe.cc`
is 1,213 bytes, SHA256
`f31bd4d31988ae31bca4d642a402bdca8540ea4a9e34000c93ce5a16385e558c`;
`BUILD.bazel.in` is 113 bytes, SHA256
`2577c1ce7d5f2acd584f72c1772db27dca63ff2bad3d2722013109a08b811355`.
Its installed name is `BUILD.bazel`.

Record the actual command, tool/profile digests, compiler work and outputs
for a new run. Compare the selected compiler/toolchain files before and
after the build. Check original source bytes/modes/links, expected additions,
and selected source/toolchain files and linked native outputs before and after runtime,
including every referenced thin-archive member. The commands reproduce the
target/profile and program-check shape; they do not replace those integrity
checks or reproduce the original shared-host durations by themselves.

## Keep the imported timing record historical

The separate imported unchanged-command record reports 30-sample medians:

| Variant | Recorded median | Recorded samples |
| --- | ---: | ---: |
| Rush | 9.3 ms | 30 |
| Bazel | 64.2 ms | 30 |
| Bazel with `--watchfs` | 54.5 ms | 30 |

Its exact timing candidate, full sample series and complete toolchain/host
record remain unrecovered. It does not qualify the current engine or the
fresh compiler-execution path above. The imported TF 2.15 **463/463** CPU
runtime corpus is also historical and was not rerun here; C's current runtime
count is one program.

Recover those timing inputs before claiming an independently reproduced
comparison. For a new comparison, match selected targets, configuration,
compiler work, environment and cache conditions; verify program/output
behavior and collect the complete distributions. Read the
[provenance record](PROVENANCE.md) and [current TensorFlow status](../status/tensorflow.md)
for the distinct acceptance scopes.
