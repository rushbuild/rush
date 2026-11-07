<!--
Copyright (c) 2026 Luis Chamberlain. All rights reserved.
SPDX-License-Identifier: MIT
-->
# TensorFlow 2.17 semantic incremental validation - rush 43851c2

Date: 2026-06-27
Host: prune
Target: `//tensorflow/core:framework`
TensorFlow checkout: `/data/tensorflow`, commit `6887368d6d4`
Bazel: 6.1.0 from `.bazelversion`
rush: release binary from `43851c2`

This is the corrected TF incremental validation. The edit is semantic, not a
touch and not a comment: each iteration appends a unique exported marker
function after the closing TensorFlow namespace in:

`tensorflow/core/framework/function_handle_cache.cc`

`bazel aquery` showed this source in the `//tensorflow/core:framework`
`CppCompile` closure, and a throwaway `bazel build --subcommands` edit verified
the local action is:

`Compiling tensorflow/core/framework/function_handle_cache.cc`

An earlier candidate, `attr_value_util.cc`, was rejected because Bazel did not
compile it for this target under the tested configuration.

## Result

Median-of-3, no trimming, jobs=32.

| tool | p50 | min | max | actions |
|---|---:|---:|---:|---|
| Bazel | 1.432 s | 1.430 s | 1.448 s | 2 processes |
| rush | 1.692 s | 1.675 s | 1.696 s | 2 executed, 66 cached observations |

Bazel is 1.18x faster on this corrected TF semantic edit. rush's action
profiler shows the graph side is already narrow: only one compile and one
archive execute. The remaining gap is mostly action-side time:

| rush action | time, sample 1 |
|---|---:|
| `CppCompile function_handle_cache.cc` | 1392 ms |
| `CppArchive libframework_internal_impl.a` | 122 ms |

The warm rush settle before edits was 35 ms. The cold rush prime was 194 s
because it started a fresh release daemon and analyzed/materialized the TF
target; that is not included in the incremental comparison.

## Commands

Bazel:

```bash
bazel --nohome_rc --nosystem_rc \
  --output_base=/data/rush-bench/bazel-output-base \
  build --config=monolithic --remote_cache= \
  --remote_upload_local_results=false \
  --repository_cache=/data/rush-bench/bazel-repo-cache \
  --jobs=32 --local_cpu_resources=32 \
  //tensorflow/core:framework
```

rush:

```bash
RUSH_ENABLE_TF217_WORKSPACE_FACADES=1 \
RUSH_GLOBAL_CLONE_CACHE=/data/rush-cache/external \
/data/rush/target/release/rush build --daemon=auto \
  --define framework_shared_object=false \
  --define tflite_with_xnnpack=false \
  --define tensorflow_mkldnn_contraction_kernel=0 \
  --define no_gcp_support=true \
  --define no_aws_support=true \
  --define no_hdfs_support=true \
  --define no_nccl_support=true \
  --jobs=32 \
  //tensorflow/core:framework
```

The source file was restored after the run and verified clean.

Raw timing summary:
`benchmarks/tf_incremental_semantic_43851c2.json`
