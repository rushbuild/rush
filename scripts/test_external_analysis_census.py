#!/usr/bin/env python3
"""Focused tests for external-analysis-census.py."""

from __future__ import annotations

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("external-analysis-census.py")
SPEC = importlib.util.spec_from_file_location("external_analysis_census", SCRIPT)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class CensusTest(unittest.TestCase):
    def test_counts_actions_paths_and_directory_membership_without_roots(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            src = root / "src"
            obj = root / "obj"
            trace = root / "trace"
            src.mkdir()
            obj.mkdir()
            trace.mkdir()
            (obj / ".sample.cmd").write_text("savedcmd_x := cc\n")
            (obj / "x.o").write_bytes(b"object")

            (trace / "strace.10").write_text(
                f'execve("/usr/bin/make", ["make"], ["PATH=/usr/bin"]) = 0\n'
                f'clone3({{}}, 88) = 11\n'
                f'openat(AT_FDCWD<{src}>, "input.c", O_RDONLY) = 3<{src}/input.c>\n'
                f'newfstatat(AT_FDCWD<{src}>, "missing.h", 0x0, 0) = -1 ENOENT (No such file or directory)\n'
                f'getdents64(3<{src}>, 0x0, 32768) = 24\n'
            )
            (trace / "strace.11").write_text(
                f'execve("/usr/bin/gcc", ["gcc", "-c"], ["PATH=/usr/bin"]) = 0\n'
                f'openat(AT_FDCWD<{src}/subdir>, "../input.c", O_RDONLY) = 3<{src}/input.c>\n'
                f'openat(AT_FDCWD<{obj}>, "x.o", O_WRONLY|O_CREAT|O_TRUNC, 0666) = 4<{obj}/x.o>\n'
                f'symlinkat("{src}", AT_FDCWD<{obj}>, "source") = 0\n'
            )

            census = MODULE.Census(src, obj)
            census.scan_file(trace / "strace.10", 10)
            census.scan_file(trace / "strace.11", 11)
            result = census.result(obj)

            self.assertEqual(
                result["schema"], "org.rushbuild.external-analysis-census.v2"
            )
            self.assertEqual(result["trace"]["files"], 2)
            self.assertEqual(result["trace"]["incomplete_successful_execs"], 0)
            self.assertEqual(result["processes"]["successful_execs"], 2)
            self.assertEqual(result["processes"]["candidate_action_processes"], 1)
            self.assertEqual(result["processes"]["clone_edges"], 1)
            self.assertEqual(result["filesystem"]["src"]["unique_read_paths"], 2)
            self.assertEqual(result["filesystem"]["src"]["unique_write_paths"], 0)
            self.assertEqual(result["filesystem"]["src"]["unique_missing_paths"], 1)
            self.assertEqual(result["filesystem"]["obj"]["unique_write_paths"], 2)
            self.assertTrue(result["directory_membership"]["captured"])
            self.assertEqual(result["output_tree"]["cmd_files"], 1)
            encoded = json.dumps(result)
            self.assertNotIn(str(root), encoded)

    def test_reports_pointer_only_successful_exec_environment(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            src = root / "src"
            obj = root / "obj"
            src.mkdir()
            obj.mkdir()
            census = MODULE.Census(src, obj)
            census.scan_stream(
                iter(
                    [
                        b'execve("/bin/sh", ["sh", "-c", "true"], '
                        b'0x7ffd0000 /* 42 vars */) = 0\n'
                    ]
                ),
                10,
            )

            result = census.result(obj)

            self.assertEqual(result["processes"]["successful_execs"], 1)
            self.assertEqual(result["trace"]["truncated_successful_execs"], 0)
            self.assertEqual(result["trace"]["incomplete_successful_execs"], 1)
            self.assertEqual(
                result["trace"]["pointer_environment_successful_execs"], 1
            )
            self.assertIn("incomplete_exec_records", result["risks"])

    def test_accepts_literal_ellipsis_in_successful_exec_arguments(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            src = root / "src"
            obj = root / "obj"
            src.mkdir()
            obj.mkdir()
            census = MODULE.Census(src, obj)
            census.scan_stream(
                iter(
                    [
                        b'execve("/usr/bin/git", ["git", "rev-list", '
                        b'"v7.2...HEAD"], ["PATH=/usr/bin"]) = 0\n'
                    ]
                ),
                10,
            )

            result = census.result(obj)

            self.assertEqual(result["trace"]["truncated_successful_execs"], 0)
            self.assertEqual(result["trace"]["incomplete_successful_execs"], 0)
            self.assertNotIn("truncated_exec_records", result["risks"])

    def test_reports_only_structurally_truncated_exec_arrays(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            src = root / "src"
            obj = root / "obj"
            src.mkdir()
            obj.mkdir()
            census = MODULE.Census(src, obj)
            census.scan_stream(
                iter(
                    [
                        b'execve("/bin/sh", ["sh", ...], '
                        b'["PATH=/usr/bin"]) = 0\n'
                    ]
                ),
                10,
            )

            result = census.result(obj)

            self.assertEqual(result["trace"]["truncated_successful_execs"], 1)
            self.assertEqual(result["trace"]["incomplete_successful_execs"], 1)
            self.assertIn("truncated_exec_records", result["risks"])

    def test_separates_terminal_unfinished_records(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            src = root / "src"
            obj = root / "obj"
            src.mkdir()
            obj.mkdir()
            census = MODULE.Census(src, obj)
            census.scan_stream(
                iter(
                    [
                        b'read(3,  <unfinished ...>\n',
                        b'<... read resumed>"x", 1) = 1\n',
                        b'epoll_wait(4,  <unfinished ...>) = ?\n',
                    ]
                ),
                10,
            )

            result = census.result(obj)

            self.assertEqual(result["trace"]["unfinished_records"], 1)
            self.assertEqual(result["trace"]["resumed_records"], 1)
            self.assertEqual(result["trace"]["terminal_unfinished_records"], 1)
            self.assertNotIn(
                "unfinished_resumed_count_mismatch", result["risks"]
            )


if __name__ == "__main__":
    unittest.main()
