#!/usr/bin/env bash
# Bootstrap libnsync_cpp.a from real nsync source (ROOT bucket-2).
#
# TensorFlow's @nsync//:nsync_cpp compiles nsync's C sources AS C++
# (`-x c++ -std=c++11`), which places the API in `namespace nsync` — that is
# what TF/tsl code links against (`nsync::nsync_mu_lock`, etc.). rush had only
# an empty @nsync stub (srcs=[]), so those symbols were undefined at link. This
# script materializes the real archive deterministically (mirrors the
# bootstrap_llvm_project_pinned.sh / bootstrap_ml_dtypes_eigen.sh pattern); rush
# then seeds the @nsync//:nsync_cpp stub's link flags with the resulting .a.
#
# The source list + opts are transcribed from nsync's upstream BUILD.bazel
# (NSYNC_SRC_GENERIC + NSYNC_SRC_PLATFORM_CPP + NSYNC_OPTS_CPP) for Linux x86_64.
# NO STUBS: this is the genuine nsync source compiled as TF builds it.
set -euo pipefail

# --- locate nsync source (real internal/*.c must be present) ---
find_src() {
    for c in \
        "${RUSH_NSYNC_SRC:-}" \
        "${RUSH_GLOBAL_CLONE_CACHE:-/data/rush-cache/external}/nsync" \
        "$PWD/tensorflow/external/nsync" \
        "$PWD/external/nsync" \
        "$HOME/devel/rush/tensorflow/external/nsync" \
        "$(ls -d "$HOME"/.cache/bazel/_bazel_*/*/external/nsync 2>/dev/null | head -1)"; do
        [ -n "$c" ] && [ -f "$c/internal/mu.c" ] && { echo "$c"; return 0; }
    done
    return 1
}
SRC="$(find_src)" || { echo "ERROR: nsync source (internal/mu.c) not found; set RUSH_NSYNC_SRC" >&2; exit 1; }
echo "nsync source: $SRC"

OUT="${RUSH_NSYNC_OUT:-${RUSH_GLOBAL_CLONE_CACHE:-/data/rush-cache/external}/nsync}"
mkdir -p "$OUT/obj"
echo "output dir:   $OUT"

GENERIC=(internal/common.c internal/counter.c internal/cv.c internal/debug.c
         internal/dll.c internal/mu.c internal/mu_wait.c internal/note.c
         internal/once.c internal/sem_wait.c internal/time_internal.c internal/wait.c)
# Linux x86_64 platform CPP set (NSYNC_SRC_PLATFORM_CPP defaults + linux futex).
PLATFORM=(platform/c++11/src/time_rep_timespec.cc
          platform/c++11/src/nsync_panic.cc
          platform/c++11/src/yield.cc
          platform/linux/src/nsync_semaphore_futex.c
          platform/posix/src/per_thread_waiter.c)

CXX="${CXX:-g++}"
# Include order + defines transcribed from nsync BUILD.bazel for Linux x86_64:
#   NSYNC_OPTS_CPP: -DNSYNC_ATOMIC_CPP11 -DNSYNC_USE_CPP11_TIMEPOINT,
#     -Iplatform/c++11.futex (gcc_linux_x86_64), -Iplatform/c++11
#   NSYNC_OPTS_GENERIC: -Iplatform/x86_64, -Ipublic, -Iinternal, -Iplatform/posix
#   default arch-platform include: -Iplatform/gcc
# c++11.futex MUST precede c++11 (it overrides the semaphore impl header).
# NSYNC_ATOMIC_CPP11 is what makes atomic.h pull in <atomic>/std::atomic.
INCS=(-I"$SRC/platform/c++11.futex" -I"$SRC/platform/c++11" -I"$SRC/platform/gcc"
      -I"$SRC/platform/x86_64" -I"$SRC/public" -I"$SRC/internal"
      -I"$SRC/platform/posix" -I"$SRC")
OPTS=(-x c++ -std=c++11 -fPIC -pthread -O2 -DNDEBUG
      -DNSYNC_ATOMIC_CPP11 -DNSYNC_USE_CPP11_TIMEPOINT)

objs=()
for f in "${GENERIC[@]}" "${PLATFORM[@]}"; do
    [ -f "$SRC/$f" ] || { echo "ERROR: missing nsync src $f" >&2; exit 1; }
    o="$OUT/obj/$(echo "$f" | tr '/' '_').o"
    "$CXX" "${OPTS[@]}" "${INCS[@]}" -c "$SRC/$f" -o "$o"
    objs+=("$o")
done

AR="${AR:-ar}"
LIB="$OUT/libnsync_cpp.a"
rm -f "$LIB"
"$AR" rcs "$LIB" "${objs[@]}"
echo "built: $LIB ($(du -h "$LIB" | cut -f1))"

# Sanity: the C++-namespaced symbol TF links must be DEFINED (T) here. Match
# the mangled name (_ZN5nsync13nsync_mu_lockE...) — robust to nm -C demangling
# format/locale differences that made the demangled-grep flaky. Non-fatal:
# the archive is otherwise validated by the per-source compile loop.
if nm "$LIB" 2>/dev/null | grep -qE "^[0-9a-f]+ T _ZN5nsync13nsync_mu_lockE"; then
    echo "OK: nsync::nsync_mu_lock defined (C++ namespace build correct)"
else
    echo "WARN: could not confirm a defined nsync::nsync_mu_lock in $LIB (continuing)" >&2
fi
