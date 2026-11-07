# Minimal stub that simulates cuda_configure.bzl structure without repository_ctx
# This tests the AST memory footprint of the file structure

# Simulate the large dictionary structure (lines 554-632 in real file)
_CHECK_CUDA_LIBS = {
    "cuda": ("libcuda.so", "/usr/local/cuda/lib64/stubs", None, False),
    "cudart": ("libcudart.so", "/usr/local/cuda/lib64", "11.0", False),
    "cudart_static": ("libcudart_static.a", "/usr/local/cuda/lib64", "11.0", True),
    "cublas": ("libcublas.so", "/usr/local/cuda/lib64", "11", False),
    "cublasLt": ("libcublasLt.so", "/usr/local/cuda/lib64", "11", False),
    "cusolver": ("libcusolver.so", "/usr/local/cuda/lib64", "11", False),
    "curand": ("libcurand.so", "/usr/local/cuda/lib64", "10", False),
    "cufft": ("libcufft.so", "/usr/local/cuda/lib64", "10", False),
    "cudnn": ("libcudnn.so", "/usr/local/cuda/lib64", "8", False),
    "cupti": ("libcupti.so", "/usr/local/cuda/extras/CUPTI/lib64", "11.0", False),
    "cusparse": ("libcusparse.so", "/usr/local/cuda/lib64", "11", False),
}

# Simulate multi-line template strings (lines 750-773)
_DUMMY_CROSSTOOL_BZL_FILE = """
def error_gpu_disabled():
  fail("ERROR: Building with --config=cuda but TensorFlow is not configured " +
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

_DUMMY_CROSSTOOL_BUILD_FILE = """
load("//crosstool:error_gpu_disabled.bzl", "error_gpu_disabled")

error_gpu_disabled()
"""

# Simulate 30 function definitions (condensed versions)
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
        "host_compiler_warnings",
        "linker_bin_path",
        "compiler_deps",
        "msvc_cl_path",
        "msvc_env_include",
        "msvc_env_lib",
        "msvc_env_path",
        "msvc_env_tmp",
        "msvc_lib_path",
        "msvc_link_path",
        "msvc_ml_path",
        "unfiltered_compile_flags",
        "win_compiler_deps",
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
    fail("\n%sCuda Configuration Error:%s %s\n" % (red, no_color, msg))

def compute_capabilities():
    capabilities = "compute_35,compute_52".split(",")
    cc_list = sorted([x.replace(".", "") for x in capabilities])
    capabilities = ["sm_%s" % x for x in cc_list[:-1]] + ["compute_%s" % cc_list[-1]]
    capabilities = dict(zip(capabilities, capabilities)).keys()
    for capability in capabilities:
        if not capability.startswith(("compute_", "sm_")):
            auto_configure_fail("Invalid compute capability: %s" % capability)
        for prefix in ["compute_", "sm_"]:
            if not capability.startswith(prefix):
                continue
            if len(capability) == len(prefix) + 2 and capability[-2:].isdigit():
                continue
            auto_configure_fail("Invalid compute capability: %s" % capability)
    return capabilities

def lib_name(base_name, cpu_value, version = None, static = False):
    version = "" if not version else "." + version
    if cpu_value in ("Linux", "FreeBSD"):
        if static:
            return "lib%s.a" % base_name
        return "lib%s.so%s" % (base_name, version)
    elif cpu_value == "Windows":
        return "%s.lib" % base_name
    elif cpu_value == "Darwin":
        if static:
            return "lib%s.a" % base_name
        return "lib%s%s.dylib" % (base_name, version)
    else:
        auto_configure_fail("Invalid cpu_value: %s" % cpu_value)

def matches_version(environ_version, detected_version):
    environ_version_parts = environ_version.split(".")
    detected_version_parts = detected_version.split(".")
    if len(detected_version_parts) < len(environ_version_parts):
        return False
    for i, part in enumerate(detected_version_parts):
        if i >= len(environ_version_parts):
            break
        if part != environ_version_parts[i]:
            return False
    return True

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

def make_copy_dir_rule(name, src_dir, out_dir, exceptions = None):
    src_dir = src_dir.replace("\\", "/")
    if src_dir[-1] == "/":
        src_dir = src_dir[:-1]
    out_dir = out_dir.replace("\\", "/")
    if out_dir[-1] == "/":
        out_dir = out_dir[:-1]

    # Simulated outs list
    outs = ['        "dummy_out_%d",' % i for i in range(50)]

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

# Stub repository rule
def _cuda_autoconf_impl(ctx):
    ctx.file("BUILD", "")

cuda_configure = repository_rule(
    implementation = _cuda_autoconf_impl,
    attrs = {},
)
