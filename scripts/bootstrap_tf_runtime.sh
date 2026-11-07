#!/usr/bin/env bash
# Populate the @tf_runtime clone cache with the genuine TFRT header/source tree.
#
# rush's @tf_runtime stub (external_deps.rs) expects the real TFRT tree at
# <RUSH_GLOBAL_CLONE_CACHE>/tf_runtime/include/tfrt/** and propagates
# -I<repo>/include via the synthesized cc_library `includes=["include"]`, so
# consumers that `#include "tfrt/concurrency/async_value.h"` (e.g.
# @local_xla//xla/runtime:async_runtime) resolve. The tree was not present in
# the global cache (only a BUILD stub), so the include did not resolve.
#
# TF fetches TFRT at the pinned commit (third_party/tf_runtime/workspace.bzl)
# into a bazel output_base. This copies that real tree (NO STUBS) into the
# global clone cache. include/ unblocks compiles; lib/ is copied too so the
# concurrency sources are available for the eventual link.
set -euo pipefail

CACHE="${RUSH_GLOBAL_CLONE_CACHE:-/data/rush-cache/external}"
DEST="${CACHE}/tf_runtime"

find_src() {
    for c in \
        "${RUSH_TF_RUNTIME_SRC:-}" \
        /tmp/rush-cache/bazel_output_base/external/tf_runtime \
        "$HOME/devel/tensorflow/.cache/rush/bazel_output_base/external/tf_runtime" \
        "$(ls -d "$HOME"/.cache/bazel/_bazel_*/*/external/tf_runtime 2>/dev/null | head -1)"; do
        [ -n "$c" ] && [ -f "$c/include/tfrt/concurrency/async_value.h" ] && { echo "$c"; return 0; }
    done
    return 1
}
SRC="$(find_src)" || {
    echo "ERROR: real TFRT tree (include/tfrt/concurrency/async_value.h) not found." >&2
    echo "       Set RUSH_TF_RUNTIME_SRC to a fetched @tf_runtime external dir." >&2
    exit 1
}
echo "tf_runtime source: ${SRC}"

mkdir -p "$DEST"
# Copy the header tree (required at compile) and the lib sources (for link).
cp -r "${SRC}/include" "${DEST}/"
[ -d "${SRC}/lib" ] && cp -r "${SRC}/lib" "${DEST}/" || true
[ -d "${SRC}/backends" ] && cp -r "${SRC}/backends" "${DEST}/" || true
# third_party/ ships concurrent_work_queue (CreateMultiThreadedWorkQueue impl)
# and llvm_derived (unique_any.h) — both needed to compile the TFRT core below.
[ -d "${SRC}/third_party" ] && cp -r "${SRC}/third_party" "${DEST}/" || true
# cpp_tests/ ships the header-only test fixtures (tfrt/cpp_tests/test_util.h)
# that TFRT's own *_test.cc sources include (e.g. work_queue_interface_test);
# its include root is cpp_tests/include, exposed via the stub `includes`.
[ -d "${SRC}/cpp_tests" ] && cp -r "${SRC}/cpp_tests" "${DEST}/" || true

# Strip TFRT's REAL nested BUILD/.bzl files. rush's @tf_runtime adapter
# (external_deps.rs) synthesizes its OWN root BUILD; if the upstream nested
# BUILDs are left on disk, rush's lazy loader tries to evaluate them (e.g.
# third_party/llvm_derived/BUILD loads @tf_runtime//:build_defs.bzl) and aborts
# the whole build. We only need the headers + .cc sources here, not TFRT's
# Bazel packaging — delete every BUILD/BUILD.bazel/*.bzl under the copied tree
# (the synthetic root BUILD is (re)written by rush after this script runs).
find "${DEST}" \( -name BUILD -o -name 'BUILD.bazel' -o -name '*.bzl' \) -type f -delete 2>/dev/null || true

[ -f "${DEST}/include/tfrt/concurrency/async_value.h" ] \
    || { echo "ERROR: copy failed — async_value.h missing in ${DEST}" >&2; exit 1; }
echo "tf_runtime: populated ${DEST}/include/tfrt ($(find "${DEST}/include/tfrt" -name '*.h' | wc -l) headers)"

# Generate TFRT's variant-selected headers. tfrt/support/mutex.h and
# thread_environment.h are NOT shipped in the source tree — TFRT's BUILD copies
# the chosen variant (std vs absl) to the unprefixed name via a genrule/select.
# rush isn't running that genrule, so consumers (e.g.
# tfrt/host_context/device.h -> "tfrt/support/mutex.h") fail to find them. TF
# builds TFRT with the std variant (the //conditions:default), and rush compiles
# every TFRT TU against this same generated header, so the std variant is
# internally consistent. Generate them by copying the _std variant (NO STUBS —
# real upstream TFRT source, just performing the build's copy step).
SUP="${DEST}/include/tfrt/support"
gen_variant() {  # gen_variant <generated> <variant-source>
    local out="${SUP}/$1" src="${SUP}/$2"
    if [ ! -f "$out" ] && [ -f "$src" ]; then
        cp "$src" "$out"
        echo "tf_runtime: generated tfrt/support/$1 from $2"
    fi
}
gen_variant mutex.h std_mutex.h
gen_variant thread_environment.h thread_environment_std.h
[ -f "${SUP}/mutex.h" ] || { echo "ERROR: failed to generate tfrt/support/mutex.h" >&2; exit 1; }

# --- compile the TFRT core subset -> libtfrt.a (NO STUBS) ---
# #39 Phase R: libtensorflow_cc.so was UN-LOADABLE (dlopen failed on
# tfrt::internal::LogStream::write_impl) because the TFRT core symbols
# (tsl::AsyncValue::*, tfrt::HostContext, CreateMultiThreadedWorkQueue,
# Aligned{Alloc,Free}, CreateMallocAllocator, RegisterDeviceType, LogStream*)
# sit behind eagerly-relocated vtables/typeinfo/static-data — they block LOAD,
# not just lazy paths. The headers-only materialization above resolves compiles
# but defines nothing, so this compiles the genuine pinned source into a real
# libtfrt.a that the @tf_runtime adapter links. `-include cstdint`/`mutex` work
# around TFRT headers' missing <cstdint>/<mutex> under gcc-15.
[ -f "${DEST}/lib/support/logging.cc" ] || { echo "ERROR: TFRT lib/ sources not copied — cannot build libtfrt.a" >&2; exit 1; }

# Prefer the pinned absl install tree (TF-2.17 fb3621f4 built via bootstrap_absl_cpp.sh);
# fall back to the legacy in-cache clone. RUSH_ABSL_INCLUDE overrides. (Codex 019eebc6.)
ABSL="${RUSH_ABSL_INCLUDE:-${CACHE}/abseil-cpp/install/include}"
[ -d "$ABSL/absl" ] || ABSL="${CACHE}/com_google_absl"
LLVM_DIR="$(ls -d "${CACHE}"/llvm-project-* 2>/dev/null | grep -vE 'old|overlay' | head -1)"
LLVM_BUILD_INC="${LLVM_DIR}/build/include"     # generated headers (llvm-config.h, abi-breaking.h)
LLVM_SRC_INC="${LLVM_DIR}/llvm/include"
EIGEN="${CACHE}/eigen_archive"
for d in "$ABSL" "$LLVM_BUILD_INC" "$LLVM_SRC_INC" "$EIGEN"; do
    [ -d "$d" ] || { echo "ERROR: missing TFRT compile include root: $d" >&2; exit 1; }
done

CXX="${CXX:-g++}"
INCLUDES=(
    "-I${DEST}/include"
    "-I${DEST}/third_party/llvm_derived/include"
    "-I${DEST}/third_party/concurrent_work_queue/lib"
    "-I${ABSL}" "-I${LLVM_BUILD_INC}" "-I${LLVM_SRC_INC}" "-I${EIGEN}"
)
# TF 2.17 upstreamed TFRT'\''s concurrency into xla/tsl, so the TFRT headers forward
# to `xla/tsl/concurrency/*.h`, which in turn include `tsl/platform/*.h`. Add BOTH
# the xla root (dir containing xla/, so `xla/tsl/...` resolves) and the bundled tsl
# root (xla/third_party/tsl, so `tsl/platform/logging.h` resolves). (Codex 019eebc6.)
if [ -d "${RUSH_TFRT_XLA_INC:-}/xla/tsl" ]; then
    INCLUDES+=("-I${RUSH_TFRT_XLA_INC}")
    [ -d "${RUSH_TFRT_XLA_INC}/third_party/tsl/tsl/platform" ] && INCLUDES+=("-I${RUSH_TFRT_XLA_INC}/third_party/tsl")
    # tsl/platform/ml_dtypes.h pulls `ml_dtypes/include/float8.h` — resolve it (and
    # other @repo-prefixed includes) off the clone-cache root where ml_dtypes lives.
    [ -f "${CACHE}/ml_dtypes/include/float8.h" ] && INCLUDES+=("-I${CACHE}")
fi
CXXFLAGS=(-std=c++17 -fPIC -O1 -DNDEBUG -include cstdint -include mutex)

OBJDIR="${DEST}/.rush-obj"
rm -rf "$OBJDIR"; mkdir -p "$OBJDIR"

# Cohesive TFRT base layers that define the referenced symbols. Best-effort:
# a few siblings pull heavier BEF/MLIR deps we don't need and may fail — the
# REQUIRED-symbol assertion below is the real gate (a partial .a is refused).
ok=0
for f in "${DEST}"/lib/support/*.cc "${DEST}"/lib/concurrency/*.cc \
         "${DEST}"/lib/host_context/*.cc \
         "${DEST}"/lib/dtype/*.cc \
         "${DEST}"/lib/tensor/*.cc \
         "${DEST}"/lib/core_runtime/*.cc \
         "${DEST}"/third_party/concurrent_work_queue/lib/*.cc; do
    [ -f "$f" ] || continue
    b="$(basename "$f" .cc)__$(basename "$(dirname "$f")")"
    if "$CXX" "${CXXFLAGS[@]}" "${INCLUDES[@]}" -c "$f" -o "${OBJDIR}/${b}.o" 2>"${OBJDIR}/${b}.err"; then
        ok=$((ok+1))
    fi
done
ar rcs "${DEST}/libtfrt.a" "${OBJDIR}"/*.o
echo "tf_runtime: compiled ${ok} objects -> libtfrt.a ($(stat -c%s "${DEST}/libtfrt.a") bytes)"

# Verify the TFRT core entry points by their DEMANGLED names — Abseil-version
# AGNOSTIC (TF-2.15 pinned absl lts_20230125, TF-2.17 lts_20230802; the exact
# inline namespace is an impl detail of the pinned headers). nm -C -g --defined-only
# checks REAL external definitions, not undefined refs. (Codex 019eebc6.)
nm -C -g --defined-only "${DEST}/libtfrt.a" > "${OBJDIR}/syms.demangled.txt"
# The TFRT-CORE entry points libtfrt.a MUST define. The tsl::AsyncValue::* / BlockUntilReady
# symbols are NOT listed: in TF 2.17 TFRT's concurrency was upstreamed into xla/tsl, so those
# are defined by the xla/tsl targets compiled directly into libtensorflow_cc.so (the dlopen
# probe never flags them) — NOT by libtfrt.a. (In TF 2.15 they happened to live in TFRT; this
# core set is the version-stable gate either way.) (Codex 019eebc6.)
REQUIRED_DEMANGLED=(
    "tfrt::internal::LogStream::write_impl("
    "tfrt::internal::LogStream::~LogStream()"
    "tfrt::internal::LogStream::current_pos() const"
    "tfrt::internal::LogStreamFatal::LogStreamFatal("
    "typeinfo for tfrt::internal::LogStream"
    "tfrt::HostContext::HostContext("
    "tfrt::CreateMultiThreadedWorkQueue("
    "tfrt::CreateMallocAllocator()"
    "tfrt::AlignedAlloc("
    "tfrt::RegisterDeviceType(llvm::StringRef)"
    "tfrt::operator<<(std::ostream&, tfrt::DType)"
)
missing=0
for s in "${REQUIRED_DEMANGLED[@]}"; do
    grep -Fq "$s" "${OBJDIR}/syms.demangled.txt" || { echo "  MISSING: $s"; missing=$((missing+1)); }
done
[ "$missing" -eq 0 ] || { echo "ERROR: libtfrt.a missing ${missing} required demangled symbol(s) — refusing partial archive (NO STUBS)" >&2; exit 1; }
echo "tf_runtime: all ${#REQUIRED_DEMANGLED[@]} required demangled symbols defined in libtfrt.a -> ${DEST}/libtfrt.a"
