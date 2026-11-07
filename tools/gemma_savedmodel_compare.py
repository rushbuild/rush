#!/usr/bin/env python3

import argparse
import hashlib
import json
import math
from pathlib import Path

import numpy as np


def parse_args():
    parser = argparse.ArgumentParser(
        description="Compare Gemma SavedModel raw logits from Bazel and Rush."
    )
    parser.add_argument("--results-dir", type=Path, required=True)
    parser.add_argument(
        "--cases",
        nargs="+",
        default=["b1_s32", "b4_s32", "b1_s128"],
    )
    parser.add_argument("--vocab-size", type=int, default=256000)
    parser.add_argument("--chunk-elements", type=int, default=4_000_000)
    parser.add_argument("--output", type=Path)
    return parser.parse_args()


def sha256(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(8 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def compare(left_path, right_path, vocab_size, chunk_elements):
    left = np.memmap(left_path, mode="r", dtype=np.float32)
    right = np.memmap(right_path, mode="r", dtype=np.float32)
    if left.size != right.size:
        raise ValueError(
            f"element count differs: {left_path}={left.size}, "
            f"{right_path}={right.size}"
        )
    if left.size % vocab_size:
        raise ValueError(
            f"{left_path} has {left.size} elements, which is not divisible "
            f"by vocab size {vocab_size}"
        )

    max_abs = 0.0
    max_rel = 0.0
    sum_abs = 0.0
    sum_squared = 0.0
    exact = 0
    close_1e4 = True
    close_1e3 = True
    for start in range(0, left.size, chunk_elements):
        left_chunk = np.asarray(
            left[start : start + chunk_elements], dtype=np.float64
        )
        right_chunk = np.asarray(
            right[start : start + chunk_elements], dtype=np.float64
        )
        difference = np.abs(left_chunk - right_chunk)
        max_abs = max(max_abs, float(difference.max()))
        max_rel = max(
            max_rel,
            float(
                (
                    difference
                    / np.maximum(np.abs(left_chunk), 1e-6)
                ).max()
            ),
        )
        sum_abs += float(difference.sum())
        sum_squared += float(np.square(difference).sum())
        exact += int(np.count_nonzero(left_chunk == right_chunk))
        close_1e4 &= bool(
            np.allclose(left_chunk, right_chunk, rtol=1e-4, atol=1e-5)
        )
        close_1e3 &= bool(
            np.allclose(left_chunk, right_chunk, rtol=1e-3, atol=1e-4)
        )

    rows = left.size // vocab_size
    left_top1 = np.asarray(left).reshape(rows, vocab_size).argmax(axis=1)
    right_top1 = np.asarray(right).reshape(rows, vocab_size).argmax(axis=1)
    top1_matches = int(np.count_nonzero(left_top1 == right_top1))
    return {
        "elements": int(left.size),
        "max_abs": max_abs,
        "mean_abs": sum_abs / left.size,
        "rmse": math.sqrt(sum_squared / left.size),
        "max_rel_floor_1e-6": max_rel,
        "exact_fraction": exact / left.size,
        "allclose_rtol_1e-4_atol_1e-5": close_1e4,
        "allclose_rtol_1e-3_atol_1e-4": close_1e3,
        "top1_matches": top1_matches,
        "top1_total": int(rows),
        "top1_fraction": top1_matches / rows,
    }


def main():
    args = parse_args()
    summary = {"cases": {}}
    for case_name in args.cases:
        reference = args.results_dir / "reference" / f"{case_name}.bin"
        bazel = (
            args.results_dir
            / f"bazel_{case_name}"
            / "output_0.bin"
        )
        rush = (
            args.results_dir
            / f"rush_{case_name}"
            / "output_0.bin"
        )
        summary["cases"][case_name] = {
            "sha256": {
                "reference": sha256(reference),
                "bazel": sha256(bazel),
                "rush": sha256(rush),
            },
            "bazel_vs_rush": compare(
                bazel, rush, args.vocab_size, args.chunk_elements
            ),
            "reference_vs_bazel": compare(
                reference, bazel, args.vocab_size, args.chunk_elements
            ),
            "reference_vs_rush": compare(
                reference, rush, args.vocab_size, args.chunk_elements
            ),
        }

    rendered = json.dumps(summary, indent=2, sort_keys=True) + "\n"
    output = args.output or (
        args.results_dir / "correctness-summary.json"
    )
    output.write_text(rendered)
    print(rendered, end="")


if __name__ == "__main__":
    main()
