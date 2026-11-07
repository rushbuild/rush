# Minimal stub that simulates rocm_configure.bzl structure

# Simulate the hardcoded include paths (lines 191-200 in real file)
_ROCM_INCLUDE_DIRS = [
    "/opt/rocm/hsa/include",
    "/opt/rocm/hip/include",
    "/opt/rocm/include",
    "/opt/rocm/include/hip",
    "/opt/rocm/include/rocprim",
    "/opt/rocm/include/rocsolver",
    "/opt/rocm/include/rocblas",
    "/opt/rocm/llvm/lib/clang/8.0/include",
    "/opt/rocm/llvm/lib/clang/9.0.0/include",
    "/opt/rocm/llvm/lib/clang/10.0.0/include",
    "/opt/rocm/llvm/lib/clang/11.0.0/include",
    "/opt/rocm/llvm/lib/clang/12.0.0/include",
    "/opt/rocm/llvm/lib/clang/13.0.0/include",
    "/opt/rocm/llvm/lib/clang/14.0.0/include",
    "/opt/rocm/llvm/lib/clang/15.0.0/include",
    "/opt/rocm/llvm/lib/clang/16.0.0/include",
    "/opt/rocm/llvm/lib/clang/17.0.0/include",
    "/opt/rocm/hcc/compiler/lib/clang/10.0.0/include/",
    "/opt/rocm/hcc/lib/clang/10.0.0/include",
    "/opt/rocm/hcc/include",
]

# Simulate large library paths list (lines 345-366)
_ROCM_LIBS = {
    "amdhip64": [
        "/opt/rocm/hip/lib64/libamdhip64.so",
        "/opt/rocm/hip/lib64/stubs/libamdhip64.so",
        "/opt/rocm/hip/lib/x86_64-linux-gnu/libamdhip64.so",
        "/opt/rocm/hip/lib/libamdhip64.so",
        "/opt/rocm/hip/libamdhip64.so",
    ],
    "rocblas": [
        "/opt/rocm/lib64/librocblas.so",
        "/opt/rocm/lib64/stubs/librocblas.so",
        "/opt/rocm/lib/x86_64-linux-gnu/librocblas.so",
        "/opt/rocm/lib/librocblas.so",
        "/opt/rocm/librocblas.so",
    ],
    "hipfft": [
        "/opt/rocm/lib64/libhipfft.so",
        "/opt/rocm/lib64/stubs/libhipfft.so",
        "/opt/rocm/lib/x86_64-linux-gnu/libhipfft.so",
        "/opt/rocm/lib/libhipfft.so",
        "/opt/rocm/libhipfft.so",
    ],
    "hiprand": [
        "/opt/rocm/lib64/libhiprand.so",
        "/opt/rocm/lib64/stubs/libhiprand.so",
        "/opt/rocm/lib/x86_64-linux-gnu/libhiprand.so",
        "/opt/rocm/lib/libhiprand.so",
        "/opt/rocm/libhiprand.so",
    ],
    "MIOpen": [
        "/opt/rocm/miopen/lib64/libMIOpen.so",
        "/opt/rocm/miopen/lib64/stubs/libMIOpen.so",
        "/opt/rocm/miopen/lib/x86_64-linux-gnu/libMIOpen.so",
        "/opt/rocm/miopen/lib/libMIOpen.so",
        "/opt/rocm/miopen/libMIOpen.so",
    ],
    "rccl": [
        "/opt/rocm/rccl/lib64/librccl.so",
        "/opt/rocm/rccl/lib64/stubs/librccl.so",
        "/opt/rocm/rccl/lib/x86_64-linux-gnu/librccl.so",
        "/opt/rocm/rccl/lib/librccl.so",
        "/opt/rocm/rccl/librccl.so",
    ],
    "hipsparse": [
        "/opt/rocm/lib64/libhipsparse.so",
        "/opt/rocm/lib64/stubs/libhipsparse.so",
        "/opt/rocm/lib/x86_64-linux-gnu/libhipsparse.so",
        "/opt/rocm/lib/libhipsparse.so",
        "/opt/rocm/libhipsparse.so",
    ],
    "roctracer64": [
        "/opt/rocm/roctracer/lib64/libroctracer64.so",
        "/opt/rocm/roctracer/lib64/stubs/libroctracer64.so",
        "/opt/rocm/roctracer/lib/x86_64-linux-gnu/libroctracer64.so",
        "/opt/rocm/roctracer/lib/libroctracer64.so",
        "/opt/rocm/roctracer/libroctracer64.so",
    ],
    "rocsolver": [
        "/opt/rocm/lib64/librocsolver.so",
        "/opt/rocm/lib64/stubs/librocsolver.so",
        "/opt/rocm/lib/x86_64-linux-gnu/librocsolver.so",
        "/opt/rocm/lib/librocsolver.so",
        "/opt/rocm/librocsolver.so",
    ],
}

# Simulate multi-line template
_DUMMY_CROSSTOOL_BZL_FILE = """
def error_gpu_disabled():
  fail("ERROR: Building with --config=rocm but TensorFlow is not configured " +
       "to build with GPU support. Please re-run ./configure and enter 'Y' " +
       "at the prompt to build with GPU support.")

  native.genrule(
      name = "error_gen_crosstool",
      outs = ["CROSSTOOL"],
      cmd = "echo 'Should not be run.' && exit 1",
  )

  native.filegroup(
      name = "crosstool",
      srcs = [":CROSSTOOL"],
      output_licenses = ["unencumbered"],
  )
"""

# Simulate 20 function definitions
def to_list_of_strings(elements):
    quoted_strings = ["\"" + element + "\"" for element in elements]
    return ", ".join(quoted_strings)

def verify_build_defines(params):
    missing = []
    for param in [
        "cxx_builtin_include_directories",
        "extra_no_canonical_prefixes_flags",
        "host_compiler_path",
        "host_compiler_prefix",
        "linker_bin_path",
        "unfiltered_compile_flags",
    ]:
        if ("%{" + param + "}") not in params:
            missing.append(param)
    if missing:
        fail("Missing variables: " + str(missing))

def find_cc():
    return "/usr/bin/gcc"

def auto_configure_fail(msg):
    red = "\033[0;31m"
    no_color = "\033[0m"
    fail("\n%sROCm Configuration Error:%s %s\n" % (red, no_color, msg))

def auto_configure_warning(msg):
    yellow = "\033[1;33m"
    no_color = "\033[0m"
    print("\n%sAuto-Configuration Warning:%s %s\n" % (yellow, no_color, msg))

def lib_name(lib, version = "", static = False):
    if static:
        return "lib%s.a" % lib
    else:
        if version:
            version = ".%s" % version
        return "lib%s.so%s" % (lib, version)

def amdgpu_targets():
    amdgpu_targets_str = "gfx900,gfx906,gfx908,gfx90a,gfx1030"
    amdgpu_targets = amdgpu_targets_str.split(",")
    for amdgpu_target in amdgpu_targets:
        if amdgpu_target[:3] != "gfx":
            auto_configure_fail("Invalid AMDGPU target: %s" % amdgpu_target)
    return amdgpu_targets

def compute_rocm_extra_copts(amdgpu_targets):
    amdgpu_target_flags = ["--amdgpu-target=" +
                           amdgpu_target for amdgpu_target in amdgpu_targets]
    return str(amdgpu_target_flags)

def make_copy_dir_rule(name, src_dir, out_dir, exceptions = None):
    src_dir = src_dir.replace("\\", "/")
    if src_dir[-1] == "/":
        src_dir = src_dir[:-1]
    out_dir = out_dir.replace("\\", "/")
    if out_dir[-1] == "/":
        out_dir = out_dir[:-1]
    outs = ['        "dummy_out_%d",' % i for i in range(40)]
    out_dir_var = "$(@D)/%s" % out_dir if len(outs) > 1 else "$(@D)"
    post_cmd = ""
    if exceptions != None:
        for x in exceptions:
            post_cmd += " ; rm -fR " + out_dir_var + "/" + x
    return """genrule(
    name = "%s",
    outs = [
%s
    ],
    cmd = \"""cp -rLf "%s/." "%s/" %s\""",
)""" % (name, "\n".join(outs), src_dir, out_dir_var, post_cmd)

def make_copy_files_rule(name, srcs, outs):
    cmds = []
    for src, out in zip(srcs, outs):
        cmds.append('cp -f "%s" "$(location %s)"' % (src, out))
    outs = [('        "%s",' % out) for out in outs]
    return """genrule(
    name = "%s",
    outs = [
%s
    ],
    cmd = \"""%s \""",
)""" % (name, "\n".join(outs), " && \\\n".join(cmds))

# Stub repository rule
def _rocm_autoconf_impl(ctx):
    ctx.file("BUILD", "")

rocm_configure = repository_rule(
    implementation = _rocm_autoconf_impl,
    attrs = {},
)
