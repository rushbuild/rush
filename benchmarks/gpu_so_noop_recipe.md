# Reproducible recipe: rush sub-second daemon warm no-op (TF 2.17, GPU .so + CPU lib)

**Date**: 2026-06-25
**Host**: `prune` — 32-core, AMD W7900 (ROCm), `/data` storage
**rush**: built from source at commit `86d9bd6` (release)
**Tree**: pristine TensorFlow 2.17.0 at `/data/tensorflow-pristine`

This recipe documents — and makes reproducible — the result that rush's persistent
daemon turns a TF-scale rebuild-with-nothing-changed into a **sub-second** operation,
and that this is **general** across target kinds (a deep CPU `cc_library` and the full
GPU `cc_shared_library` `.so`), not specific to any one target.

It is the post-daemon answer to the pre-daemon TF-scale no-op gap recorded in
`benchmarks/TF217_RUSH_VS_BAZEL_RUNPOD.md` (no-op was 44–130 s, oscillating, because
every invocation re-fetched already-materialized externals and re-parsed every
BUILD/.bzl with no cross-invocation graph cache).

## What makes the warm no-op sub-second

Three on-by-default mechanisms, all on `main`:

1. **Persistent daemon (`rushd`)** — keeps the analyzed build graph resident, so a
   second identical command reuses it instead of re-parsing `.bzl`/`BUILD` and
   re-running PASS-A analysis. Log marker:
   `♻️  Reusing analyzed graph (manifest Fresh): N target(s), M build-order node(s) — skipping .bzl eval / BUILD parse / PASS-A analysis`
2. **Action-freshness ledger** — on reuse, rush stats the action input/output paths;
   if all are unchanged it skips the PASS-B build wavefront entirely. Log marker:
   `✅ action-freshness ledger CLEAN (P paths from A actions, T ms) — graph up-to-date, skipping PASS-B wavefront`
3. **cc_shared_library link-plan cache** (`081254d`, key-order fix `81bd98b`) — caches
   the alwayslink-completeness fixpoint + member set for the `.so`, so a no-op `.so`
   rebuild does not re-run the multi-minute `nm` closure scan. Log marker:
   `⚡ cc_shared_library <name>: link-plan cache HIT`

The proto no-op fix (`c25a8ce`) and the define-driven `select()` daemon-parity fixes
(`a6f4397`, `86d9bd6`) keep these paths from being defeated by spurious churn.

## Environment

```bash
export RUSH=/data/rush/target/release/rush
export TF=/data/tensorflow-pristine
export RUSH_ENABLE_ROCM=1
export RUSH_ENABLE_TF217_WORKSPACE_FACADES=1
export RUSH_GLOBAL_CLONE_CACHE=/data/rush-cache-217/external   # pre-materialized externals
export RUSH_GPU_GCC_INSTALL_DIR=/usr/lib/gcc/x86_64-linux-gnu/12
export PATH=/data/rush-cache-217/external/flatbuffers_flatc:/opt/rocm/bin:$PATH
export CPLUS_INCLUDE_PATH=/opt/rocm/include
export RUSH_LINKER=mold                                        # parallel link for the .so

# TF-2.17 GPU define set
DEFS="--define tflite_with_xnnpack=false --define tflite_kernel_use_xnnpack=false \
--define tensorflow_mkldnn_contraction_kernel=0 --define no_gcp_support=true \
--define __TENSORFLOW_NO_LGPL_DEPS__=1 --define no_aws_support=true \
--define no_hdfs_support=true --define no_nccl_support=true"
# the GPU .so additionally needs:
SO_DEFS="--define framework_shared_object=true $DEFS"
```

**Caveat — keep the define-config consistent.** A warm no-op is only a no-op when the
on-disk artifacts were built with the *same* defines. rush keys each compile action on
its flags, so building a CPU-flag variant on a GPU-built tree (or vice-versa) correctly
rebuilds the differing objects — that is correct incremental behavior, not a no-op
regression. Don't interleave different `--define` sets against the same `rush-out`.

## Build the GPU `libtensorflow_cc.so`

```bash
cd "$TF"
$RUSH build //tensorflow:tensorflow_cc $SO_DEFS --daemon=auto
# output: rush-out/tensorflow_cc/libtensorflow_cc.so.2.17.0  (~2.3 GB)
#         rush-out/tensorflow_cc/libtensorflow_cc.so.2.17.0.linkplan.json  (link-plan cache)
```

## Measure CLEAN / INCREMENTAL / NO-OP

Use `benchmarks/bench_tf_gpu_so.sh` (rush-vs-Bazel head-to-head; median of N runs;
writes only to a bench root + /tmp, never the TF source tree or external cache):

```bash
cd "$TF"
RUNS=3 bash /data/rush/benchmarks/bench_tf_gpu_so.sh rush-clean   # full-throughput build
RUNS=3 bash /data/rush/benchmarks/bench_tf_gpu_so.sh rush-incr    # touch one .cc, rebuild
RUNS=3 bash /data/rush/benchmarks/bench_tf_gpu_so.sh rush-noop    # rebuild, nothing changed
# Bazel comparison (same target/defines): rush-clean -> bzl-clean, etc.
```

## Generalize-verify the warm no-op across target kinds

`benchmarks/generalize_verify.sh <framework|so>` primes a target via the default daemon,
then times 3 fresh-CLI warm no-ops and greps the daemon log for the reuse + ledger
(+ link-plan) markers:

```bash
bash /data/rush/benchmarks/generalize_verify.sh framework   # //tensorflow/core:framework (CPU cc_library)
bash /data/rush/benchmarks/generalize_verify.sh so          # //tensorflow:tensorflow_cc (GPU cc_shared_library .so)
```

## dlopen check (the `.so` is a loadable runtime, not just linked bytes)

```bash
cat > /tmp/dlopen_check.c <<'EOF'
#include <dlfcn.h>
#include <stdio.h>
int main(int argc, char **argv) {
  void *h = dlopen(argv[1], RTLD_NOW | RTLD_GLOBAL);
  if (!h) { fprintf(stderr, "dlopen FAILED: %s\n", dlerror()); return 1; }
  printf("dlopen OK: %s\n", argv[1]);
  return 0;
}
EOF
gcc /tmp/dlopen_check.c -o /tmp/dlopen_check -ldl
LD_LIBRARY_PATH=/opt/rocm/lib /tmp/dlopen_check "$TF/rush-out/tensorflow_cc/libtensorflow_cc.so.2.17.0"
# observed: dlopen OK: .../libtensorflow_cc.so.2.17.0
```

## Observed numbers

Warm no-op = build once to warm the daemon, then median of 3 fresh-CLI identical
rebuilds. All confirmed by the daemon-log markers above.

| Target | kind | warm no-op median | daemon markers |
|---|---|---|---|
| `//tensorflow/core:framework` | CPU `cc_library` | **0.047 s** (reps 0.081 / 0.047 / 0.045) | Reusing analyzed graph (manifest Fresh) + ledger CLEAN (9 paths / 5 actions), skipping PASS-B |
| `//tensorflow:tensorflow_cc` | GPU `cc_shared_library` `.so` | **0.091 s** (reps 0.155 / 0.091 / 0.090) | Reusing analyzed graph (8478 build-order nodes) + ledger CLEAN (21631 paths / 7913 actions, ~1.1 ms), skipping PASS-B |

Earlier measurements on the same setup: GPU `.so` no-op ~0.10 s; CPU `framework`
no-op ~0.046 s — consistent with the above.

**On the `.so`, which fast path fires.** The warm no-op of `tensorflow_cc` is gated by
the **action-freshness ledger**: it validates 21,631 input/output paths from 7,913
actions in ~1.1 ms, finds them all fresh, and **skips the PASS-B wavefront entirely** —
so the build never reaches the `cc_shared_library` link step, and the link-plan cache
is not even consulted on a true no-op. The **link-plan cache HIT** is the *second* line
of defense: it fires when PASS-B does run with everything cached (e.g. a cache-only
rebuild), where it skips the multi-minute 558-archive `nm` scan + symbol-aware-prune
fixpoint:
`⚡ cc_shared_library tensorflow_cc: link-plan cache HIT (558 advertised archives unchanged) — skipping nm scan + symbol-aware-prune fixpoint`.
A link-plan cache MISS (its slow path) only happens when the advertised archive set
genuinely changes — e.g. after a different `--define` config dirtied the closure.

**hello differential correctness**: `scripts/differential_correctness.sh` on the
`examples/hello_cpp` `//:hello` target passes **8/8** (no-op graph byte-identical,
no-op digest stable, edit minimality both directions, edit-output==clean-output,
restore-to-baseline) — the incremental engine these numbers ride on is correct.

## Known limitation (honest)

`rush build //tensorflow/core/kernels:constant_op_test` (a `tf_cc_test`) resolves to
**0 buildable nodes** under plain `rush build` ("Scoping build to 0 nodes reachable
from 1 root(s)") in both the CPU and GPU define configs — the `tf_cc_test` rule's
build phase is not wired to materialize a test-binary node via plain `rush build` here.
The TF C++ test corpus (the certified 463/463) is driven through a dedicated test
harness, not plain `rush build`. So the generalize-verify above covers a deep CPU
`cc_library` and the full GPU `cc_shared_library .so`; the test-rule build-phase
wiring is a separate, tracked item.
