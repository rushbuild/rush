#!/usr/bin/env bash
# Rush — bootstrap the TF-pinned llvm-project clone for the MLIR
# dialect/transforms rung of //tensorflow/core:core_cpu.
#
# Phase 5.C.5a. WHY this exists:
#   TF 2.15 pins llvm-project @ 49cb1595 (~LLVM 17/18-dev, mid-2023).
#   The only installable system MLIR packages are 19/21/22 — all
#   NEWER. MLIR-19's AbstractType::get<T>() requires a static T::name
#   member (mlir/IR/TypeSupport.h) added AFTER TF's pin, so TF's
#   hand-written TypeBase-derived RefTypes fail to compile against
#   system MLIR headers. Patching TF source is FORBIDDEN (NO-STUBS).
#   The bazel-faithful fix is to compile (and later LINK) TF's MLIR C++
#   against TF's PINNED llvm-project — what bazel does via the
#   @llvm-project http_archive.
#
#   crates/rush-build/src/external_deps.rs already PREFERS this clone:
#   when build/tools/mlir/include/.../BuiltinLocationAttributes.h.inc
#   exists it exposes the clone's mlir/llvm includes ORDERED BEFORE
#   system llvm. So this script is pure ENVIRONMENT MATERIALIZATION.
#
# DESIGN (revised 2026-05-29 after the cse:Pass blocker, with Codex):
#   - `mlir-headers` is INCOMPLETE — it omits top-level pass incgens
#     like MLIRTransformsPassIncGen (=> mlir/Transforms/Passes.h.inc).
#     We DYNAMICALLY HARVEST every generated mlir/**/*.inc target from
#     `ninja -t targets` so no header is missed (no static enumeration).
#   - DEFAULT now builds the CORE MLIR/LLVM STATIC LIB set too: building
#     a lib forces ALL its tablegen'd headers AND produces the
#     libMLIR*.a / libLLVM*.a archives the LINK rung needs. Use
#     --headers-only for the cheap (compile-only) mode.
#   - VERSIONED DIR + SYMLINK + STAMP, built IN-PLACE: CMake/Ninja build
#     trees are NOT relocatable (absolute src/build paths bake into
#     CMakeCache/build.ninja/depfiles), so the old temp-dir+mv approach
#     left a non-re-runnable tree. We build in
#     <cache>/llvm-project-<commit> in place, write a completion stamp,
#     then atomically repoint <cache>/llvm-project at it.
#
# This script does NOT auto-run during `rush build`. Operators invoke
# it once per host. It applies TF's own third_party/llvm/*.patch set
# (TF external-dep patches, NOT TF source patches) before configuring.
#
# USAGE:
#   scripts/bootstrap_llvm_project_pinned.sh --dry-run
#   scripts/bootstrap_llvm_project_pinned.sh --apply                 # libs + all headers (default)
#   scripts/bootstrap_llvm_project_pinned.sh --apply --headers-only  # compile-only, cheap
#   scripts/bootstrap_llvm_project_pinned.sh --apply --force         # rebuild even if stamped
#
# Env knobs:
#   RUSH_GLOBAL_CLONE_CACHE  (default: /data/rush-cache/external)
#   RUSH_LLVM_BUILD_JOBS     (default: nproc)
#
# EXIT CODES:
#   0 — success (apply) or plan printed (dry-run)
#   1 — overwrite refused, checkout mismatch, patch/build failed
#   2 — environment problem (missing tools, invalid args)

set -euo pipefail

# ---------------------------------------------------------
# TF-pinned metadata (tensorflow/third_party/llvm/workspace.bzl).
# Bumping requires re-extracting from TF's pin AND updating
# docs/analysis/core-cpu-mlir-version-skew-2026-05-29.md.
# ---------------------------------------------------------

# LLVM_COMMIT is overridable so this script serves multiple TF pins (2.15 default below; e.g.
# TF 2.17.0 pins ae8627809076...). The cache dir (VDIR) is keyed by commit, and combined with
# RUSH_GLOBAL_CLONE_CACHE this keeps each TF tree's LLVM build fully isolated.
LLVM_COMMIT="${LLVM_COMMIT:-49cb1595c1b3ae1de3684fea6148363c15bae12a}"
LLVM_URL="https://github.com/llvm/llvm-project/archive/${LLVM_COMMIT}.tar.gz"
LLVM_MIRROR_URL="https://storage.googleapis.com/mirror.tensorflow.org/github.com/llvm/llvm-project/archive/${LLVM_COMMIT}.tar.gz"
LLVM_STRIP="llvm-project-${LLVM_COMMIT}"

# TF's @llvm-project patch set, applied in workspace.bzl order.
LLVM_PATCHES=(generated.patch build.patch mathextras.patch toolchains.patch zstd.patch)

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUSH_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# TF_LLVM_DIR is overridable: the *.patch filenames are stable across TF releases but their
# CONTENTS are pinned to that release's LLVM commit, so a different TF pin needs ITS OWN patch
# set (e.g. TF 2.17 -> /data/tensorflow-2.17/third_party/llvm). Default = the rush TF submodule (2.15).
TF_LLVM_DIR="${TF_LLVM_DIR:-${RUSH_ROOT}/tensorflow/third_party/llvm}"

# ---------------------------------------------------------
# Arg parsing
# ---------------------------------------------------------

MODE=""
FORCE=0
HEADERS_ONLY=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run)      MODE="dry-run"; shift ;;
        --apply)        MODE="apply"; shift ;;
        --force)        FORCE=1; shift ;;
        --headers-only) HEADERS_ONLY=1; shift ;;
        -h|--help)      sed -n '2,60p' "$0"; exit 0 ;;
        *) echo "error: unknown arg: $1" >&2; exit 2 ;;
    esac
done
[[ -z "$MODE" ]] && { echo "error: must specify --dry-run or --apply" >&2; exit 2; }

# ---------------------------------------------------------
# Paths: versioned dir + stable symlink.
# ---------------------------------------------------------

CACHE_ROOT="${RUSH_GLOBAL_CLONE_CACHE:-/data/rush-cache/external}"
VDIR="${CACHE_ROOT}/llvm-project-${LLVM_COMMIT:0:16}"
LINK="${CACHE_ROOT}/llvm-project"
# #39: stamp is versioned by mode so the old "core-libs" stamp does NOT skip
# the new all-MLIR-libs bootstrap. Bump the suffix when the built lib set
# changes so existing clones rebuild on the next --apply.
STAMP_SUFFIX=$([[ $HEADERS_ONLY -eq 1 ]] && echo "headers-only-v1" || echo "mlir-all-libs-v1")
STAMP="${VDIR}/.rush-bootstrap-complete-${STAMP_SUFFIX}"
JOBS="${RUSH_LLVM_BUILD_JOBS:-$(nproc)}"

# #39 MLIR dialect bootstrap (Codex 019e8ee5, xhigh): the old hardcoded
# ~20-lib CORE_LIB_TARGETS list built only 39 libMLIR*.a — the MLIR DIALECT
# archives (libMLIRTosaDialect.a, libMLIRLinalgDialect.a, libMLIRMathDialect.a,
# libMLIRComplexDialect.a, libMLIRAffineDialect.a, libMLIRBufferizationDialect.a,
# ...) were NEVER built, leaving ~414 mlir::<dialect>::*Op TypeIDResolver
# symbols undefined in libtensorflow_cc.so (ldd -r). Fix: build ALL libMLIR*.a
# dynamically (harvested from `ninja -t targets all` below, mirroring the .inc
# harvest) — no static enumeration, no whack-a-mole (NO STUBS). Only LLVMSupport
# stays explicit; the rest of the LLVM archives are linked by commit 287's
# complete --start-group from whatever already exists. (Add an LLVM harvest only
# if a future ldd -r shows llvm:: gaps from unbuilt LLVM libs.)
LLVM_REQUIRED_LIB_TARGETS=(
    LLVMSupport
)

# ---------------------------------------------------------
# Tool checks
# ---------------------------------------------------------

require_tool() { command -v "$1" >/dev/null 2>&1 || { echo "error: required tool not on PATH: $1" >&2; exit 2; }; }
require_tool git; require_tool cmake; require_tool ninja
require_tool curl; require_tool tar; require_tool patch; require_tool awk

# ---------------------------------------------------------
# Plan / dry-run
# ---------------------------------------------------------

print_plan() {
    cat <<EOF
=== bootstrap_llvm_project_pinned plan ===
TF-pinned llvm-project commit : ${LLVM_COMMIT}
source (mirror first)         : ${LLVM_MIRROR_URL}
TF patch set (in order)       : ${LLVM_PATCHES[*]}  (from ${TF_LLVM_DIR})
versioned build dir           : ${VDIR}
stable symlink                : ${LINK} -> $(basename "$VDIR")
mode                          : $([[ $HEADERS_ONLY -eq 1 ]] && echo headers-only || echo "core-libs (default)")
build jobs                    : ${JOBS}

CMake configure (LLVM+MLIR, static, X86 target, RTTI on, WERROR off,
  -include cstdint for modern-gcc compat) IN-PLACE at \$VDIR/build.

Header completeness: dynamically harvest every
  tools/mlir/include/mlir/**/*.inc CUSTOM_COMMAND target from
  \`ninja -t targets all\` (covers mlir/Transforms/Passes.h.inc etc.
  that the \`mlir-headers\` phony misses).

$([[ $HEADERS_ONLY -eq 0 ]] && echo "Lib rung (default): LLVMSupport + ALL libMLIR*.a (dynamically harvested from ninja -t targets all; builds every MLIR dialect/interface archive + transitive deps).")

Publish: write ${STAMP}; atomically repoint ${LINK}.
EOF
}

if [[ "$MODE" == "dry-run" ]]; then
    print_plan; echo; echo "(dry-run: no changes made)"; exit 0
fi

# ---------------------------------------------------------
# Apply
# ---------------------------------------------------------

print_plan

# Already complete?
if [[ -f "$STAMP" && $FORCE -eq 0 ]]; then
    echo "==> already bootstrapped (${STAMP} present); repointing symlink and exiting"
    ln -sfn "$(basename "$VDIR")" "$LINK"
    exit 0
fi

mkdir -p "$CACHE_ROOT"

# --- Fetch + extract + patch (skip if source already present) ---
if [[ ! -d "${VDIR}/llvm" || $FORCE -eq 1 ]]; then
    echo "==> fetching + extracting llvm-project @ ${LLVM_COMMIT}"
    rm -rf "$VDIR"; mkdir -p "$VDIR"
    tb="${VDIR}/.src.tar.gz"
    curl --fail --silent --show-error --location --output "$tb" "$LLVM_MIRROR_URL" \
        || curl --fail --silent --show-error --location --output "$tb" "$LLVM_URL" \
        || { echo "error: fetch failed" >&2; exit 1; }
    tar -xzf "$tb" -C "$VDIR" --strip-components=1 "$LLVM_STRIP"
    rm -f "$tb"
    echo "==> applying TF llvm patch set"
    for p in "${LLVM_PATCHES[@]}"; do
        pf="${TF_LLVM_DIR}/${p}"
        [[ -f "$pf" ]] || { echo "error: TF patch missing: $pf" >&2; exit 1; }
        [[ -s "$pf" ]] || { echo "  skip empty patch: $p"; continue; }
        echo "  patch -p1 < $p"
        patch -d "$VDIR" -p1 --forward < "$pf" || { echo "error: patch failed: $p" >&2; exit 1; }
    done
else
    echo "==> reusing existing patched source at ${VDIR}"
fi

# --- Configure IN-PLACE (fresh build dir; correct absolute paths) ---
echo "==> cmake configure (in-place)"
rm -rf "${VDIR}/build"
cmake -S "${VDIR}/llvm" -B "${VDIR}/build" -G Ninja \
    -DCMAKE_BUILD_TYPE=Release \
    -DLLVM_ENABLE_PROJECTS=mlir \
    -DLLVM_TARGETS_TO_BUILD="${RUSH_LLVM_TARGETS:-AArch64;X86;AMDGPU;NVPTX}" \
    -DLLVM_ENABLE_WERROR=OFF \
    -DCMAKE_CXX_FLAGS="-include cstdint" \
    -DBUILD_SHARED_LIBS=OFF \
    -DLLVM_BUILD_LLVM_DYLIB=OFF -DLLVM_LINK_LLVM_DYLIB=OFF \
    -DLLVM_ENABLE_RTTI=ON \
    -DLLVM_INCLUDE_TESTS=OFF -DMLIR_INCLUDE_TESTS=OFF \
    -DLLVM_INCLUDE_EXAMPLES=OFF -DLLVM_INCLUDE_BENCHMARKS=OFF \
    -DLLVM_INCLUDE_DOCS=OFF -DMLIR_INCLUDE_DOCS=OFF \
    -DMLIR_ENABLE_BINDINGS_PYTHON=OFF \
    -DLLVM_ENABLE_ZLIB=OFF -DLLVM_ENABLE_ZSTD=OFF \
    -DLLVM_ENABLE_TERMINFO=OFF -DLLVM_ENABLE_LIBEDIT=OFF \
    -DLLVM_ENABLE_LIBXML2=OFF

# --- Dynamically harvest every generated mlir .inc target ---
echo "==> harvesting mlir/**/*.inc generation targets"
mapfile -t INC_TARGETS < <(
    ninja -C "${VDIR}/build" -t targets all 2>/dev/null \
        | awk -F: '/^tools\/mlir\/include\/mlir\/.*\.inc: CUSTOM_COMMAND/ {print $1}'
)
echo "    ${#INC_TARGETS[@]} .inc targets"

# --- Dynamically harvest every libMLIR*.a static-archive target (#39) ---
# Mirrors the .inc harvest: ask ninja for every static-library link target
# under lib/libMLIR*.a so ALL dialect/interface libs (and their transitive
# deps) build, with no static target list. Ninja names static libs as
# `lib/libMLIRFoo.a: CXX_STATIC_LIBRARY_LINKER__MLIRFoo_Release`; the path is
# itself a valid build target and naming it builds its full dep graph.
LIB_TARGETS=()
if [[ $HEADERS_ONLY -eq 0 ]]; then
    echo "==> harvesting MLIR static library targets"
    mapfile -t LIB_TARGETS < <(
        ninja -C "${VDIR}/build" -t targets all 2>/dev/null \
            | awk -F: '/^lib\/libMLIR.*\.a: CXX_STATIC_LIBRARY_LINKER/ {print $1}' \
            | LC_ALL=C sort -u
    )
    [[ ${#LIB_TARGETS[@]} -gt 0 ]] \
        || { echo "error: no libMLIR*.a targets harvested" >&2; exit 1; }
    echo "    ${#LIB_TARGETS[@]} libMLIR*.a targets"

    # --- MLIR ExecutionEngine RUNTIME libs ---
    # CMake builds these as SHARED libs under their own LOWERCASE names, so
    # the libMLIR*.a harvest above cannot see them. XLA's CPU JIT links their
    # symbols directly: simple_orc_jit.cc references memrefCopy, and without
    # libmlir_c_runner_utils.so the link fails with "undefined reference to
    # memrefCopy" in a file that names neither MLIR nor this bootstrap.
    #
    # rush already knows how to link them - build_orchestrator_ext.rs emits
    # -l<target> with an rpath for @llvm-project//mlir:mlir_* and gates that
    # on the .so existing. The .so simply was never built.
    mapfile -t RUNTIME_TARGETS < <(
        ninja -C "${VDIR}/build" -t targets all 2>/dev/null \
            | awk -F: '/^lib\/libmlir_[a-z0-9_]*\.so.*: CXX_SHARED_LIBRARY_LINKER/ {print $1}' \
            | LC_ALL=C sort -u
    )
    if [[ ${#RUNTIME_TARGETS[@]} -gt 0 ]]; then
        echo "    ${#RUNTIME_TARGETS[@]} MLIR runtime .so targets"
        LIB_TARGETS+=("${RUNTIME_TARGETS[@]}")
    else
        echo "    warning: no libmlir_*.so runtime targets found; XLA's CPU" >&2
        echo "    JIT will fail at link on memrefCopy" >&2
    fi

    # --- Harvest every libLLVM*.a too (GPU lane) ---
    # XLA's GPU backend (xla/service/gpu/llvm_gpu_backend/gpu_backend_lib.cc)
    # calls LLVMInitializeAMDGPU{TargetInfo,TargetMC,AsmParser,AsmPrinter} and
    # links the AMDGPU LLVM codegen (libLLVMAMDGPUCodeGen.a / Info / Desc /
    # AsmParser / Disassembler / Utils). With AMDGPU now in
    # LLVM_TARGETS_TO_BUILD, Targets.def declares the init fns (fixes the
    # compile) and these archives exist — harvest them all so the cc.so link
    # finds them (mirrors the libMLIR*.a harvest; the comment above noted to add
    # an LLVM harvest once an llvm:: / AMDGPU gap appears, which it now has).
    mapfile -t LLVM_LIB_TARGETS < <(
        ninja -C "${VDIR}/build" -t targets all 2>/dev/null \
            | awk -F: '/^lib\/libLLVM.*\.a: CXX_STATIC_LIBRARY_LINKER/ {print $1}' \
            | LC_ALL=C sort -u
    )
    [[ ${#LLVM_LIB_TARGETS[@]} -gt 0 ]] \
        || { echo "error: no libLLVM*.a targets harvested" >&2; exit 1; }
    echo "    ${#LLVM_LIB_TARGETS[@]} libLLVM*.a targets"
    LIB_TARGETS+=("${LLVM_LIB_TARGETS[@]}")
fi

# --- Build: tblgen/pdll tools + all headers + (default) all MLIR libs ---
# mlir-pdll MUST be the pinned binary: system mlir-pdll-19 emits
# `::llvm::LogicalResult` (LogicalResult moved mlir->llvm in newer LLVM)
# which the pinned headers (mlir::LogicalResult) reject. rush's
# gentbl_cc_library prefers <clone>/build/bin/<tool>, so building it here
# makes PDLL codegen namespace-consistent with the pinned headers.
BUILD_TARGETS=(llvm-tblgen mlir-tblgen mlir-pdll llvm-headers mlir-headers "${INC_TARGETS[@]}")
if [[ $HEADERS_ONLY -eq 0 ]]; then
    BUILD_TARGETS+=("${LLVM_REQUIRED_LIB_TARGETS[@]}" "${LIB_TARGETS[@]}")
    echo "==> ninja: headers + ${#INC_TARGETS[@]} incgens + ${#LLVM_REQUIRED_LIB_TARGETS[@]} LLVM required libs + ${#LIB_TARGETS[@]} MLIR libs"
else
    echo "==> ninja: headers + ${#INC_TARGETS[@]} incgens (headers-only)"
fi
ninja -C "${VDIR}/build" -j "$JOBS" "${BUILD_TARGETS[@]}"

# --- Verify witnesses ---
echo "==> verifying witnesses"
for w in \
    "${VDIR}/build/tools/mlir/include/mlir/IR/BuiltinLocationAttributes.h.inc" \
    "${VDIR}/build/tools/mlir/include/mlir/Transforms/Passes.h.inc" \
    "${VDIR}/build/include/llvm/Config/llvm-config.h" \
    "${VDIR}/mlir/include/mlir/IR/TypeSupport.h" \
    "${VDIR}/build/bin/mlir-tblgen"; do
    [[ -e "$w" ]] || { echo "error: expected witness not produced: $w" >&2; exit 1; }
done
if [[ $HEADERS_ONLY -eq 0 ]]; then
    [[ -e "${VDIR}/build/lib/libLLVMSupport.a" ]] \
        || { echo "error: LLVMSupport requested but libLLVMSupport.a missing" >&2; exit 1; }
    # Every harvested libMLIR*.a must exist after the build (#39).
    for rel in "${LIB_TARGETS[@]}"; do
        [[ -e "${VDIR}/build/${rel}" ]] \
            || { echo "error: harvested MLIR lib missing after build: ${rel}" >&2; exit 1; }
    done
fi

# --- Stamp + atomic symlink swap ---
date -u +"bootstrap complete %Y-%m-%dT%H:%M:%SZ commit=${LLVM_COMMIT} headers_only=${HEADERS_ONLY}" > "$STAMP" 2>/dev/null || echo "complete" > "$STAMP"
ln -sfn "$(basename "$VDIR")" "$LINK"

echo
echo "=== DONE ==="
echo "pinned llvm-project at: ${VDIR}"
echo "symlink: ${LINK} -> $(readlink "$LINK")"
echo "rush external_deps.rs will select the pinned MLIR include dirs."
