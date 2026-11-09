#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Allocate, fill and verify a small device buffer in a remote GPU action.

Use a worker whose operator has admitted GPU work. This checks the selected
runtime and output transport; it measures no GPU build or compute throughput.
"""

import argparse
import ctypes as c
import hashlib
import json
from pathlib import Path


SIZE = 1024 * 1024
FILL = 109


def function(library, name, arguments):
    fn = getattr(library, name)
    fn.argtypes = arguments
    fn.restype = c.c_int

    def checked(*args):
        result = fn(*args)
        if result:
            raise RuntimeError(f"{name} returned {result}")
    return checked


def select_device(count, name, pattern):
    for index in range(count):
        label = c.create_string_buffer(256)
        name(label, len(label), index)
        if not pattern or pattern in label.value.decode():
            return index
    raise RuntimeError("no device matches the requested selector")


def cuda(pattern):
    lib = c.CDLL("libcuda.so.1")
    function(lib, "cuInit", [c.c_uint])(0)
    count = c.c_int()
    function(lib, "cuDeviceGetCount", [c.POINTER(c.c_int)])(c.byref(count))
    device = select_device(count.value,
        function(lib, "cuDeviceGetName", [c.c_char_p, c.c_int, c.c_int]), pattern)
    context = c.c_void_p()
    function(lib, "cuDevicePrimaryCtxRetain", [c.POINTER(c.c_void_p), c.c_int])(
        c.byref(context), device)
    pointer = c.c_uint64()
    host = c.create_string_buffer(SIZE)
    try:
        function(lib, "cuCtxSetCurrent", [c.c_void_p])(context)
        function(lib, "cuMemAlloc_v2", [c.POINTER(c.c_uint64), c.c_size_t])(
            c.byref(pointer), SIZE)
        function(lib, "cuMemsetD8_v2", [c.c_uint64, c.c_ubyte, c.c_size_t])(
            pointer, FILL, SIZE)
        function(lib, "cuMemcpyDtoH_v2", [c.c_void_p, c.c_uint64, c.c_size_t])(
            host, pointer, SIZE)
        function(lib, "cuCtxSynchronize", [])()
        return host.raw
    finally:
        if pointer.value:
            function(lib, "cuMemFree_v2", [c.c_uint64])(pointer)
        function(lib, "cuDevicePrimaryCtxRelease_v2", [c.c_int])(device)


def hip(pattern):
    lib = c.CDLL("libamdhip64.so")
    count = c.c_int()
    function(lib, "hipGetDeviceCount", [c.POINTER(c.c_int)])(c.byref(count))
    device = select_device(count.value,
        function(lib, "hipDeviceGetName", [c.c_char_p, c.c_int, c.c_int]), pattern)
    function(lib, "hipSetDevice", [c.c_int])(device)
    pointer = c.c_void_p()
    host = c.create_string_buffer(SIZE)
    try:
        function(lib, "hipMalloc", [c.POINTER(c.c_void_p), c.c_size_t])(
            c.byref(pointer), SIZE)
        function(lib, "hipMemset", [c.c_void_p, c.c_int, c.c_size_t])(
            pointer, FILL, SIZE)
        function(lib, "hipMemcpy", [c.c_void_p, c.c_void_p, c.c_size_t, c.c_int])(
            host, pointer, SIZE, 2)  # hipMemcpyDeviceToHost
        function(lib, "hipDeviceSynchronize", [])()
        return host.raw
    finally:
        if pointer.value:
            function(lib, "hipFree", [c.c_void_p])(pointer)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--backend", choices=("cuda", "hip"), required=True)
    parser.add_argument("--device-name-contains", default="")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    data = (cuda if args.backend == "cuda" else hip)(args.device_name_contains)
    assert data == bytes([FILL]) * SIZE
    args.output.write_text(json.dumps({"backend": args.backend, "bytes": SIZE,
        "sha256": hashlib.sha256(data).hexdigest(), "verified": True}) + "\n")


if __name__ == "__main__":
    main()
