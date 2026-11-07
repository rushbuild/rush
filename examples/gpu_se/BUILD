# rush GPU StreamExecutor smoke targets (deployed into the TF workspace as //rush_rocm_smoke).
# Additive — NEW rush smoke harnesses, not a patch of any TF/XLA source. Link the rush-built ROCm
# StreamExecutor runtime via the GRANULAR targets (multi_platform_manager + the rocm platform/
# backend), deliberately NOT the :stream_executor aggregate (which drags in the BLAS/DNN plugins
# that have semantic ROCm-6 API skews).
#   gpu_se_query — static ROCmPlatform registration -> PlatformWithName -> ExecutorForDevice -> device discovery
#   gpu_se_exec  — alloc + stream + H2D/D2H memcpy round-trip (the runtime EXECUTES on the card)

# Shared config. rush doesn't propagate the @local_xla repo's include roots to a cross-repo
# consumer, and the global RUSH_ENABLE_ROCM copts don't reach this cross-repo smoke package, so the
# harness names the ROCm defines + the SE include roots explicitly (the same the SE targets compile
# with on this build host). Prune-host paths; smoke targets, not portable build code.
ROCM_SE_COPTS = [
    "-DTENSORFLOW_USE_ROCM=1",
    "-D__HIP_PLATFORM_AMD__",
    "-DEIGEN_USE_HIP",
    # rocm_config_root precedes /opt/rocm/include so the rush rocm/include mirror overlay wins.
    "-I/data/tensorflow/.cache/rush/external/local_config_rocm/rocm_config_root",
    "-I/opt/rocm/include",
    "-I/data/tensorflow/third_party/xla",
    "-I/data/tensorflow/third_party/xla/third_party/tsl",
    "-I/data/tensorflow/.cache/rush/external/com_google_absl",
    "-I/data/tensorflow/.cache/rush/external/com_google_protobuf",
    "-I/data/tensorflow/.cache/rush/bazel_output_base/external/com_google_protobuf/src",
    "-I/data/tensorflow/.cache/rush/external/eigen_archive",
    "-I/data/tensorflow/.cache/rush/external/ml_dtypes",
    "-I/data/tensorflow",
]

# The rocm/ backend references the HIP runtime (hipCtx*/hipGraph*/hipGetDevice/hipMalloc/hipMemcpy)
# directly, so link libamdhip64. NO rocBLAS/MIOpen — the alloc+memcpy closure is plugin-free.
ROCM_SE_LINKOPTS = [
    "-L/opt/rocm/lib",
    "-lamdhip64",
    "-Wl,-rpath,/opt/rocm/lib",
]

ROCM_SE_DEPS = [
    "@local_xla//xla/stream_executor:multi_platform_manager",
    "@local_xla//xla/stream_executor/rocm:rocm_platform",
]

# Faithful real impl of the one GpuDriver graph method TF 2.15's rocm_driver.cc lacks
# (GraphExecKernelNodeSetParams), wrapping ROCm 6.4.3's hipGraphExecKernelNodeSetParams.
ROCM_SE_COMPAT_SRCS = ["gpu_driver_rocm_graph_compat.cc"]

cc_binary(
    name = "gpu_se_query",
    srcs = ["gpu_se_query.cc"] + ROCM_SE_COMPAT_SRCS,
    copts = ROCM_SE_COPTS,
    linkopts = ROCM_SE_LINKOPTS,
    deps = ROCM_SE_DEPS,
)

cc_binary(
    name = "gpu_se_exec",
    srcs = ["gpu_se_exec.cc"] + ROCM_SE_COMPAT_SRCS,
    copts = ROCM_SE_COPTS,
    linkopts = ROCM_SE_LINKOPTS,
    deps = ROCM_SE_DEPS,
)
