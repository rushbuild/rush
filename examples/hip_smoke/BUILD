# HIP smoke test — a cc_binary whose source contains a HIP kernel. Built with
# `RUSH_GPU_CXX=/opt/rocm/bin/hipcc rush build //:hip_smoke`, which swaps the
# compiler+linker to hipcc so rush compiles the device code and links the HIP
# runtime. Running the result executes the kernel on the GPU (AMD W7900 / gfx1100).
cc_binary(
    name = "hip_smoke",
    srcs = ["hip_smoke.cpp"],
)
