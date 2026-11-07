#!/usr/bin/env python3
"""Compare Rush, Bazel, and canonical CPU TensorFlow validation outputs."""

import argparse
import csv
import json
import statistics
from pathlib import Path

import numpy as np


DTYPES = {
    "float": np.float32,
    "float32": np.float32,
    "double": np.float64,
    "float64": np.float64,
    "int32": np.int32,
    "int64": np.int64,
    "uint8": np.uint8,
    "int8": np.int8,
    "bool": np.bool_,
}


def parse_shape(value: str) -> tuple[int, ...]:
    if value == "scalar" or not value:
        return ()
    return tuple(int(part) for part in value.split("x"))


def read_array(path: Path, dtype: str, shape: list[int] | tuple[int, ...]):
    array = np.fromfile(path, dtype=DTYPES[dtype])
    return array.reshape(tuple(shape))


def load_rows(case_root: Path) -> list[dict]:
    with (case_root / "outputs.tsv").open(newline="") as source:
        return list(csv.DictReader(source, delimiter="\t"))


def load_final(case_root: Path, name: str) -> np.ndarray:
    matches = [
        row
        for row in load_rows(case_root)
        if row["kind"] == "final" and row["name"] == name
    ]
    if len(matches) != 1:
        raise ValueError(f"{case_root}: expected one final output for {name}")
    row = matches[0]
    return read_array(Path(row["path"]), row["dtype"], parse_shape(row["shape"]))


def load_series(case_root: Path, name: str) -> np.ndarray:
    matches = sorted(
        (
            row
            for row in load_rows(case_root)
            if row["kind"] == "series" and row["name"] == name
        ),
        key=lambda row: int(row["step"]),
    )
    if not matches:
        raise ValueError(f"{case_root}: no series output for {name}")
    values = [
        read_array(Path(row["path"]), row["dtype"], parse_shape(row["shape"]))
        for row in matches
    ]
    return np.stack(values)


def load_reference(artifacts: Path, entry: dict) -> np.ndarray:
    return read_array(
        artifacts / entry["path"], entry["dtype"], tuple(entry["shape"])
    )


def numeric_metrics(left: np.ndarray, right: np.ndarray) -> dict:
    if left.shape != right.shape:
        return {
            "shape_match": False,
            "dtype_match": left.dtype == right.dtype,
            "left_dtype": str(left.dtype),
            "right_dtype": str(right.dtype),
            "left_shape": list(left.shape),
            "right_shape": list(right.shape),
            "exact": False,
        }
    dtype_match = left.dtype == right.dtype
    if not dtype_match:
        return {
            "shape_match": True,
            "dtype_match": False,
            "left_dtype": str(left.dtype),
            "right_dtype": str(right.dtype),
            "exact": False,
        }
    if np.issubdtype(left.dtype, np.integer) or np.issubdtype(
        left.dtype, np.bool_
    ):
        exact = bool(np.array_equal(left, right))
        return {"shape_match": True, "dtype_match": True, "exact": exact}
    if not np.issubdtype(left.dtype, np.number):
        exact = bool(np.array_equal(left, right))
        return {"shape_match": True, "dtype_match": True, "exact": exact}
    left64 = left.astype(np.float64)
    right64 = right.astype(np.float64)
    difference = np.abs(left64 - right64)
    denominator = np.maximum(np.abs(right64), 1e-12)
    left_flat = left64.reshape(-1)
    right_flat = right64.reshape(-1)
    norm_product = np.linalg.norm(left_flat) * np.linalg.norm(right_flat)
    cosine = (
        float(np.dot(left_flat, right_flat) / norm_product)
        if norm_product != 0
        else float(np.array_equal(left, right))
    )
    return {
        "shape_match": True,
        "dtype_match": True,
        "exact": bool(np.array_equal(left, right)),
        "finite": bool(np.isfinite(left64).all() and np.isfinite(right64).all()),
        "max_abs": float(difference.max(initial=0.0)),
        "max_rel": float((difference / denominator).max(initial=0.0)),
        "rmse": float(np.sqrt(np.mean(np.square(difference)))),
        "cosine": cosine,
    }


def compare_pair(
    left: np.ndarray, right: np.ndarray, atol: float, rtol: float
) -> dict:
    result = numeric_metrics(left, right)
    result["atol"] = atol
    result["rtol"] = rtol
    shape_and_dtype_match = bool(
        result["shape_match"] and result["dtype_match"]
    )
    exact_dtype = np.issubdtype(left.dtype, np.integer) or np.issubdtype(
        left.dtype, np.bool_
    )
    if exact_dtype:
        result["passed"] = bool(shape_and_dtype_match and result["exact"])
    elif np.issubdtype(left.dtype, np.number):
        result["passed"] = bool(
            shape_and_dtype_match
            and result.get("finite", False)
            and np.allclose(left, right, atol=atol, rtol=rtol, equal_nan=False)
        )
    else:
        result["passed"] = bool(shape_and_dtype_match and result["exact"])
    return result


def timing_stats(case_root: Path) -> dict:
    with (case_root / "timings.tsv").open(newline="") as source:
        rows = list(csv.DictReader(source, delimiter="\t"))
    milliseconds = [int(row["elapsed_ns"]) / 1_000_000.0 for row in rows]
    median = statistics.median(milliseconds)
    deviations = [abs(value - median) for value in milliseconds]
    return {
        "samples": len(milliseconds),
        "median_ms": median,
        "mad_ms": statistics.median(deviations),
        "min_ms": min(milliseconds),
        "max_ms": max(milliseconds),
    }


def manifests(artifacts: Path) -> list[dict]:
    cases = []
    for name in ["inference_manifest.json", "training_manifest.json"]:
        cases.extend(json.loads((artifacts / name).read_text())["cases"])
    return cases


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--rush", required=True, type=Path)
    parser.add_argument("--bazel", required=True, type=Path)
    parser.add_argument("--artifacts", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--parity-atol", type=float, default=1e-6)
    parser.add_argument("--parity-rtol", type=float, default=1e-5)
    parser.add_argument("--reference-inference-atol", type=float, default=1e-3)
    parser.add_argument("--reference-inference-rtol", type=float, default=1e-3)
    parser.add_argument("--reference-training-atol", type=float, default=1e-2)
    parser.add_argument("--reference-training-rtol", type=float, default=1e-2)
    args = parser.parse_args()

    rush = args.rush.resolve()
    bazel = args.bazel.resolve()
    artifacts = args.artifacts.resolve()
    output = args.out.resolve()
    output.mkdir(parents=True, exist_ok=True)
    report = {"schema_version": 1, "cases": [], "errors": [], "passed": True}
    summary_rows = []
    validation_cases = manifests(artifacts)
    if not validation_cases:
        report["errors"].append("validation matrix contains no cases")
        report["passed"] = False

    for case in validation_cases:
        case_id = case["id"]
        case_report = {
            "id": case_id,
            "kind": case["kind"],
            "model": case["model"],
            "batch": case["batch"],
            "comparisons": [],
            "convergence": [],
        }
        tensors = []
        for entry in case["references"].get("series", []):
            tensors.append(("series", entry))
        for entry in case["references"].get("final", []):
            tensors.append(("final", entry))

        for kind, reference_entry in tensors:
            name = reference_entry["name"]
            loader = load_series if kind == "series" else load_final
            rush_value = loader(rush / case_id, name)
            bazel_value = loader(bazel / case_id, name)
            reference_value = load_reference(artifacts, reference_entry)
            parity = compare_pair(
                rush_value,
                bazel_value,
                args.parity_atol,
                args.parity_rtol,
            )
            if case["kind"] == "inference":
                reference_atol = args.reference_inference_atol
                reference_rtol = args.reference_inference_rtol
            else:
                reference_atol = args.reference_training_atol
                reference_rtol = args.reference_training_rtol
            rush_reference = compare_pair(
                rush_value, reference_value, reference_atol, reference_rtol
            )
            bazel_reference = compare_pair(
                bazel_value, reference_value, reference_atol, reference_rtol
            )
            comparison = {
                "kind": kind,
                "name": name,
                "rush_vs_bazel": parity,
                "rush_vs_cpu_reference": rush_reference,
                "bazel_vs_cpu_reference": bazel_reference,
            }
            if (
                case["kind"] == "inference"
                and kind == "final"
                and rush_value.ndim == 2
            ):
                reference_top1 = np.argmax(reference_value, axis=1)
                comparison["top1"] = {
                    "rush_vs_bazel": bool(
                        np.array_equal(
                            np.argmax(rush_value, axis=1),
                            np.argmax(bazel_value, axis=1),
                        )
                    ),
                    "rush_vs_cpu_reference": bool(
                        np.array_equal(
                            np.argmax(rush_value, axis=1), reference_top1
                        )
                    ),
                    "bazel_vs_cpu_reference": bool(
                        np.array_equal(
                            np.argmax(bazel_value, axis=1), reference_top1
                        )
                    ),
                }
                comparison["top1"]["passed"] = all(
                    comparison["top1"].values()
                )
            comparison["passed"] = (
                parity["passed"]
                and rush_reference["passed"]
                and bazel_reference["passed"]
                and comparison.get("top1", {}).get("passed", True)
            )
            case_report["comparisons"].append(comparison)
            report["passed"] = report["passed"] and comparison["passed"]

            if case["kind"] == "training" and kind == "series":
                for label, values in [
                    ("rush", rush_value),
                    ("bazel", bazel_value),
                    ("cpu_reference", reference_value),
                ]:
                    flat = values.reshape(-1)
                    convergence = {
                        "runtime": label,
                        "initial_loss": float(flat[0]),
                        "final_loss": float(flat[-1]),
                        "finite": bool(np.isfinite(flat).all()),
                        "decreased": bool(flat[-1] < flat[0]),
                    }
                    convergence["passed"] = (
                        convergence["finite"] and convergence["decreased"]
                    )
                    case_report["convergence"].append(convergence)
                    report["passed"] = (
                        report["passed"] and convergence["passed"]
                    )

        rush_timing = timing_stats(rush / case_id)
        bazel_timing = timing_stats(bazel / case_id)
        case_report["timing"] = {
            "rush": rush_timing,
            "bazel": bazel_timing,
            "rush_over_bazel": (
                rush_timing["median_ms"] / bazel_timing["median_ms"]
            ),
        }
        case_passed = (
            bool(case_report["comparisons"])
            and all(
                comparison["passed"]
                for comparison in case_report["comparisons"]
            )
            and all(item["passed"] for item in case_report["convergence"])
        )
        report["passed"] = report["passed"] and case_passed
        summary_rows.append(
            [
                case_id,
                "PASS" if case_passed else "FAIL",
                f"{rush_timing['median_ms']:.3f}",
                f"{bazel_timing['median_ms']:.3f}",
                f"{case_report['timing']['rush_over_bazel']:.4f}",
            ]
        )
        report["cases"].append(case_report)

    (output / "comparison.json").write_text(
        json.dumps(report, indent=2, sort_keys=True) + "\n"
    )
    lines = [
        "# TensorFlow runtime validation",
        "",
        f"Overall: **{'PASS' if report['passed'] else 'FAIL'}**",
        "",
        "| Case | Accuracy | Rush median ms | Bazel median ms | Rush/Bazel |",
        "|---|---:|---:|---:|---:|",
    ]
    lines.extend("| " + " | ".join(row) + " |" for row in summary_rows)
    lines.append("")
    (output / "summary.md").write_text("\n".join(lines))
    if not report["passed"]:
        raise SystemExit("one or more validation comparisons failed")


if __name__ == "__main__":
    main()
