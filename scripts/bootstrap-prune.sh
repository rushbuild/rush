#!/usr/bin/env bash
# bootstrap-prune.sh
#
# Idempotent setup for the prune dev host's TF build environment so every
# new session starts from a known-good state. Captures every env-level hack
# this repo depends on — system packages, header symlinks, upstream-archive
# fetches, consumer-workspace patches — in one reviewable file.
#
# CLAUDE.md "NO FUCKING STUBS" applies here too: if a workaround isn't
# expressible as a real fetch, install, or patch, it doesn't belong in this
# script.
#
# Usage:
#   scripts/bootstrap-prune.sh             # full setup on the current host
#   scripts/bootstrap-prune.sh --check     # verify everything is in place, non-destructive
#
# Env knobs:
#   TF_ROOT         — path to the TensorFlow checkout (default /data/tensorflow)
#   RUSH_CLONE_ROOT — global rush clone cache (default /data/rush-cache/external)
#   TFRT_COMMIT     — TFRT commit pinned by TF's workspace.bzl
#   SYS_LLVM_INCLUDE — system LLVM Config header root (default
#                      /usr/include/llvm-19/llvm/Config; override with e.g.
#                      /usr/include/llvm-18/llvm/Config on older distros)
#   SUDO            — sudo binary, set to "" to skip privileged steps

set -euo pipefail

MODE="${1:-install}"
TF_ROOT="${TF_ROOT:-/data/tensorflow}"
RUSH_CLONE_ROOT="${RUSH_CLONE_ROOT:-/data/rush-cache/external}"
# Pinned in TF: third_party/tf_runtime/workspace.bzl. Bump together.
TFRT_COMMIT="${TFRT_COMMIT:-70637966e2ec9afccc2cf4d51ed2391172b1b9c5}"
SYS_LLVM_INCLUDE="${SYS_LLVM_INCLUDE:-/usr/include/llvm-19/llvm/Config}"
SUDO="${SUDO:-sudo}"
# Absolute path to the patch dir (relative to this script).
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PATCH_DIR="$SCRIPT_DIR/../third_party_patches"

log() { printf '[bootstrap] %s\n' "$*" >&2; }
warn() { printf '[bootstrap] WARNING: %s\n' "$*" >&2; }
die() { printf '[bootstrap] ERROR: %s\n' "$*" >&2; exit 1; }

ensure_installed() {
    local pkgs=("$@") missing=()
    for p in "${pkgs[@]}"; do
        if ! dpkg -s "$p" >/dev/null 2>&1; then
            missing+=("$p")
        fi
    done
    if [ ${#missing[@]} -gt 0 ]; then
        if [ "$MODE" = "--check" ]; then
            warn "missing packages: ${missing[*]}"
            return 1
        fi
        log "installing: ${missing[*]}"
        $SUDO apt-get install -y "${missing[@]}"
    fi
}

ensure_symlink() {
    local target="$1" link="$2"
    if [ -L "$link" ] && [ "$(readlink "$link")" = "$target" ]; then
        return 0
    fi
    if [ -e "$link" ] && [ ! -L "$link" ]; then
        warn "$link exists and is not a symlink — leaving alone"
        return 0
    fi
    if [ "$MODE" = "--check" ]; then
        warn "missing symlink: $link -> $target"
        return 1
    fi
    log "creating symlink $link -> $target"
    $SUDO ln -sfT "$target" "$link"
}

# ---- system packages ----
ensure_installed \
    libabsl-dev \
    libgrpc++-dev \
    libgrpc-dev \
    protobuf-compiler \
    protobuf-compiler-grpc \
    libprotobuf-dev \
    libflatbuffers-dev \
    flatbuffers-compiler

# ---- tf_runtime upstream headers ----
# TF vendors only a workspace.bzl pointer; the real TFRT headers must be
# fetched from GitHub at the pinned commit. We stash them under
# <RUSH_CLONE_ROOT>/tf_runtime/include/tfrt/** so rush's external_deps.rs
# can resolve `@tf_runtime//:...` targets via resolve_clone_path().
tfrt_repo="$RUSH_CLONE_ROOT/tf_runtime"
tfrt_witness="$tfrt_repo/include/tfrt/concurrency/async_value.h"
if [ ! -f "$tfrt_witness" ]; then
    if [ "$MODE" = "--check" ]; then
        warn "missing tfrt headers under $tfrt_repo/include"
    else
        log "fetching tf_runtime @ $TFRT_COMMIT"
        tmp=$(mktemp -d)
        trap "rm -rf $tmp" EXIT
        curl -sL "https://github.com/tensorflow/runtime/archive/${TFRT_COMMIT}.tar.gz" \
            -o "$tmp/tfrt.tar.gz"
        tar -C "$tmp" -xzf "$tmp/tfrt.tar.gz"
        mkdir -p "$tfrt_repo"
        cp -r "$tmp/runtime-${TFRT_COMMIT}/include" "$tfrt_repo/"
        trap - EXIT
        rm -rf "$tmp"
    fi
fi

# TFRT's upstream BUILD has two genrules that resolve a std-vs-absl
# select() at Bazel-eval time — they `cp $< $@` the default-branch
# source (absl_*) to the canonical name the cc_library's hdrs list
# expects:
#
#   genrule(name="mutex.h",
#           srcs = [":use_std_thread" → "std_mutex.h",
#                   default        → "absl_mutex.h"],
#           outs=["include/tfrt/support/mutex.h"],
#           cmd="cp $< $@")
#   genrule(name="thread_environment.h",
#           srcs = [":use_std_thread" → "thread_environment_std.h",
#                   default        → "thread_environment_google.h"],
#           outs=["include/tfrt/support/thread_environment.h"],
#           cmd="cp $< $@")
#
# Rush doesn't expand genrules defined in the tfrt stub BUILD, so
# materialize the same files here via the same default-branch copy.
# See upstream: https://github.com/tensorflow/runtime/blob/$TFRT_COMMIT/BUILD
tfrt_support="$tfrt_repo/include/tfrt/support"
# Note: thread_environment_google.h is Google-internal-only and not
# shipped in the OSS tarball; OSS TFRT uses the `:use_std_thread`
# branch, which copies thread_environment_std.h instead.
for pair in "absl_mutex.h:mutex.h" "thread_environment_std.h:thread_environment.h"; do
    src_base="${pair%%:*}"
    dst_base="${pair##*:}"
    src="$tfrt_support/$src_base"
    dst="$tfrt_support/$dst_base"
    if [ -f "$src" ] && [ ! -f "$dst" ]; then
        if [ "$MODE" = "--check" ]; then
            warn "missing tfrt genrule output: $dst"
        else
            log "materializing tfrt upstream genrule: cp $src_base $dst_base"
            cp "$src" "$dst"
        fi
    fi
done

# ---- LLVM Config headers ----
# The TF-pinned llvm-project clone ships `.cmake.in` templates; the concrete
# Config/ headers are normally produced by a cmake configure step. We don't
# run cmake (too slow for a dev-loop bootstrap) so we import the concrete
# files from the host's llvm-19 install. The version skew is known — TF's
# pinned LLVM is close enough that `llvm-config.h`'s feature macros are a
# safe approximation for our non-codegen consumers. A proper fix is on the
# punch list: configure the TF-pinned clone via cmake (or use TF-bundled
# LLVM built through rush).
llvm_clone_cfg="$RUSH_CLONE_ROOT/llvm-project/llvm/include/llvm/Config"
if [ -d "$RUSH_CLONE_ROOT/llvm-project" ]; then
    mkdir -p "$llvm_clone_cfg"
    for f in abi-breaking.h llvm-config.h config.h AsmParsers.def AsmPrinters.def \
             Disassemblers.def Targets.def TargetExegesis.def TargetMCAs.def; do
        src="$SYS_LLVM_INCLUDE/$f"
        dst="$llvm_clone_cfg/$f"
        if [ -f "$src" ] && [ ! -f "$dst" ]; then
            if [ "$MODE" = "--check" ]; then
                warn "missing LLVM Config header: $dst"
            else
                log "copying llvm-19 $f into TF-pinned clone Config/"
                cp "$src" "$dst"
            fi
        fi
    done
fi

# ---- header symlinks under /usr/local/include ----
# Some consumer .cc files `#include "tfrt/..."` / `#include "llvm/..."` /
# `#include "llvm-c/..."` from targets whose BUILD doesn't depend on the
# matching rush cc_library, so the -I path rush produces doesn't cover
# them. Exposing the header roots under /usr/local/include — which gcc
# searches by default — backstops those paths. Removal plan: when rush's
# dep-chase wires every consumer through to @tf_runtime/@llvm-project
# cc_library deps, the symlinks can go.
ensure_symlink "$RUSH_CLONE_ROOT/tf_runtime/include/tfrt" /usr/local/include/tfrt
ensure_symlink "$RUSH_CLONE_ROOT/llvm-project/llvm/include/llvm" /usr/local/include/llvm
ensure_symlink "$RUSH_CLONE_ROOT/llvm-project/llvm/include/llvm-c" /usr/local/include/llvm-c

# ---- consumer-workspace patches ----
# `git apply` is idempotent enough — it refuses to reapply a clean patch
# and exits non-zero; we probe with --check first and skip already-applied.
if [ -d "$TF_ROOT/.git" ]; then
    for patch in "$PATCH_DIR"/tensorflow-*.patch; do
        [ -f "$patch" ] || continue
        if (cd "$TF_ROOT" && git apply --reverse --check "$patch") 2>/dev/null; then
            log "already applied: $(basename "$patch")"
        else
            if [ "$MODE" = "--check" ]; then
                warn "missing TF patch: $(basename "$patch")"
            else
                log "applying: $(basename "$patch")"
                (cd "$TF_ROOT" && git apply "$patch")
            fi
        fi
    done
fi

# ---- stale stub cleanup in rush-out/ ----
# Old rush binaries used to emit `// gentbl_cc_library stub ...` content
# into genfiles when tblgen failed. The new rush hard-fails instead, but
# leftover files on disk still trip the stub scanner and — worse — get
# reused unchanged on subsequent builds because rush's action cache keys
# on inputs, not content. Delete them so each session starts honest.
rush_out="$TF_ROOT/rush-out/genfiles"
if [ -d "$rush_out" ]; then
    stale=$(grep -rl --include="*.inc" --include="*.h" --include="*.cc" \
        -E "gentbl_cc_library stub|Stub emitted by rush|empty stub|placeholder content" \
        "$rush_out" 2>/dev/null || true)
    if [ -n "$stale" ]; then
        if [ "$MODE" = "--check" ]; then
            warn "stale stub files under $rush_out:"
            echo "$stale" | head -10
        else
            log "removing stale stub files under $rush_out"
            echo "$stale" | xargs -r rm -v >&2
        fi
    fi
fi

if [ "$MODE" = "--check" ]; then
    log "check complete"
else
    log "bootstrap complete"
fi
