#!/usr/bin/env bash
# gpu_guard.sh — cooperative GPU access on the shared rush W7900 host.
#
# Multiple agents may share the card. This guard yields the GPU to any OTHER compute process:
# it waits until no foreign KFD PID is using the GPU and a best-effort lock is free, takes the
# lock (so a cooperating peer that checks the same lock yields to us), runs the wrapped command,
# and releases the lock. Ground truth is rocm-smi (KFD PIDs); the lock is courtesy coordination.
#
# Usage:  scripts/gpu_guard.sh <command> [args...]
# Env:    GPU_MAX_WAIT (seconds to wait for a free GPU before giving up; default 1800)
#         GPU_LOCK     (lock path; default /tmp/rush_gpu.lock)
# Exit:   42 if the GPU stayed busy past GPU_MAX_WAIT (caller should retry later, not fail hard).
set -uo pipefail

LOCK="${GPU_LOCK:-/tmp/rush_gpu.lock}"
MAX_WAIT="${GPU_MAX_WAIT:-1800}"
ROCM_SMI="${ROCM_SMI:-/opt/rocm/bin/rocm-smi}"
ME="rush-agent-$$"

# GPU is busy if rocm-smi does NOT report the "No KFD PIDs" idle marker (i.e. some process is
# using the GPU compute engine). This counts our own runs too, but the guard only consults it
# while WAITING to acquire — before we have launched anything.
#
# Use grep -c (counts, reads all input), NOT grep -q: under `set -o pipefail`, `grep -q` closes
# the pipe on first match, rocm-smi then takes SIGPIPE, and the pipeline reports failure — which
# `! pipeline` flips into a FALSE "busy" exactly when the GPU is idle ("No KFD PIDs" present).
gpu_busy() {
  local idle
  idle="$("$ROCM_SMI" --showpids 2>/dev/null | grep -ci "No KFD PIDs")"
  [ "${idle:-0}" -eq 0 ]
}

lock_taken_by_other() {
  [ -f "$LOCK" ] && [ "$(cat "$LOCK" 2>/dev/null)" != "$ME" ]
}

waited=0
while gpu_busy || lock_taken_by_other; do
  # Steal a stale lock (holder gone / forgot to release) older than 1h.
  if [ -f "$LOCK" ] && [ "$(( $(date +%s) - $(stat -c %Y "$LOCK" 2>/dev/null || echo 0) ))" -gt 3600 ]; then
    rm -f "$LOCK"; continue
  fi
  if [ "$waited" -ge "$MAX_WAIT" ]; then
    echo "GPU_BUSY_TIMEOUT: GPU still busy after ${MAX_WAIT}s — yielding, retry later" >&2
    exit 42
  fi
  echo "GPU busy (another agent) — yielding, waited ${waited}s ..." >&2
  sleep 30
  waited=$(( waited + 30 ))
done

echo "$ME" > "$LOCK"
trap 'rm -f "$LOCK"' EXIT
echo "gpu_guard: acquired GPU (lock=$LOCK holder=$ME)" >&2
"$@"
