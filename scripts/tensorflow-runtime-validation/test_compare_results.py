#!/usr/bin/env python3
"""Regression tests for TensorFlow result comparison failures."""

import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import numpy as np


SCRIPT = Path(__file__).with_name("compare_results.py")
SPEC = importlib.util.spec_from_file_location("compare_results", SCRIPT)
COMPARATOR = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(COMPARATOR)


class PairComparisonTests(unittest.TestCase):
    def test_finite_float_positive_control(self):
        result = COMPARATOR.compare_pair(
            np.array([1.0], dtype=np.float32),
            np.array([1.0], dtype=np.float32),
            1e-6,
            1e-5,
        )
        self.assertTrue(result["passed"])

    def test_matching_infinities_are_rejected(self):
        with np.errstate(invalid="ignore"):
            result = COMPARATOR.compare_pair(
                np.array([np.inf], dtype=np.float32),
                np.array([np.inf], dtype=np.float32),
                1e-6,
                1e-5,
            )
        self.assertFalse(result["finite"])
        self.assertFalse(result["passed"])

    def test_unequal_int64_values_are_rejected_exactly(self):
        result = COMPARATOR.compare_pair(
            np.array([2**53], dtype=np.int64),
            np.array([2**53 + 1], dtype=np.int64),
            1e-6,
            1e-5,
        )
        self.assertFalse(result["exact"])
        self.assertFalse(result["passed"])

    def test_dtype_mismatch_is_rejected(self):
        result = COMPARATOR.compare_pair(
            np.array([1], dtype=np.int32),
            np.array([1], dtype=np.int64),
            1e-6,
            1e-5,
        )
        self.assertFalse(result["dtype_match"])
        self.assertFalse(result["passed"])


class CommandLineComparisonTests(unittest.TestCase):
    def run_case(self, root: Path, *, empty: bool = False, tie: bool = False):
        root.mkdir()
        for name in ("rush", "bazel", "artifacts"):
            (root / name).mkdir()
        artifacts = root / "artifacts"
        cases = []
        if not empty:
            reference = np.array([[1.0, 1.000002]], dtype=np.float32)
            bazel = reference[:, ::-1].copy() if tie else reference.copy()
            reference.tofile(artifacts / "reference.bin")
            cases = [
                {
                    "id": "logits",
                    "kind": "inference",
                    "model": "synthetic",
                    "batch": 1,
                    "references": {
                        "final": [
                            {
                                "name": "logits:0",
                                "path": "reference.bin",
                                "dtype": "float32",
                                "shape": [1, 2],
                            }
                        ]
                    },
                }
            ]
            for name, value in (("rush", reference), ("bazel", bazel)):
                case_root = root / name / "logits"
                case_root.mkdir()
                tensor = case_root / "output.bin"
                value.tofile(tensor)
                (case_root / "outputs.tsv").write_text(
                    "kind\tname\tstep\tpath\tdtype\tshape\n"
                    f"final\tlogits:0\t0\t{tensor}\tfloat32\t1x2\n"
                )
                (case_root / "timings.tsv").write_text(
                    "elapsed_ns\n1000000\n1000000\n"
                )
        (artifacts / "inference_manifest.json").write_text(
            json.dumps({"cases": cases})
        )
        (artifacts / "training_manifest.json").write_text(
            json.dumps({"cases": []})
        )
        command = [
            sys.executable,
            str(SCRIPT),
            "--rush",
            str(root / "rush"),
            "--bazel",
            str(root / "bazel"),
            "--artifacts",
            str(artifacts),
            "--out",
            str(root / "comparison"),
        ]
        result = subprocess.run(command, capture_output=True, text=True, check=False)
        report_path = root / "comparison" / "comparison.json"
        report = json.loads(report_path.read_text())
        return result, report

    def test_cli_positive_control(self):
        with tempfile.TemporaryDirectory() as directory:
            result, report = self.run_case(Path(directory) / "positive")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(report["passed"])

    def test_top1_disagreement_fails_report_and_exit(self):
        with tempfile.TemporaryDirectory() as directory:
            result, report = self.run_case(Path(directory) / "top1", tie=True)
        comparison = report["cases"][0]["comparisons"][0]
        self.assertFalse(comparison["top1"]["passed"])
        self.assertFalse(comparison["passed"])
        self.assertFalse(report["passed"])
        self.assertNotEqual(result.returncode, 0)

    def test_empty_matrix_fails_report_and_exit(self):
        with tempfile.TemporaryDirectory() as directory:
            result, report = self.run_case(Path(directory) / "empty", empty=True)
        self.assertIn("validation matrix contains no cases", report["errors"])
        self.assertFalse(report["passed"])
        self.assertNotEqual(result.returncode, 0)


if __name__ == "__main__":
    unittest.main()
