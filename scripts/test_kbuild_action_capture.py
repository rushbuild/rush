#!/usr/bin/env python3
"""Focused tests for kbuild-action-capture.py."""

from __future__ import annotations

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("kbuild-action-capture.py")
SPEC = importlib.util.spec_from_file_location("kbuild_action_capture", SCRIPT)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class KbuildActionCaptureTest(unittest.TestCase):
    def test_base_graph_upgrades_legacy_and_keeps_receiptless_actions(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            analyzer = {"identity": "same"}
            goals = [[1]]
            legacy = {
                "schema": MODULE.LEGACY_SCHEMA,
                "analyzer": analyzer,
                "requested_goals": goals,
                "action": {"argv": [list(b"true")]},
                "receipt_path": str(root / ".legacy.cmd"),
            }
            receiptless = {
                "schema": MODULE.SCHEMA,
                "analyzer": analyzer,
                "requested_goals": goals,
                "commands": [{"argv": [list(b"true")]}],
                "receipt_path": None,
            }
            graph_path = root / "graph.json"
            graph_path.write_text(
                json.dumps(
                    {
                        "schema": "org.rushbuild.kbuild-observed-graph.v3",
                        "actions": [legacy, receiptless],
                    }
                )
            )

            graph = MODULE.load_base_graph(graph_path)

            self.assertEqual(
                [action["schema"] for action in graph["actions"]],
                [MODULE.SCHEMA, MODULE.SCHEMA],
            )
            self.assertIsNone(graph["actions"][1]["receipt_path"])
            graph_path.write_text(
                json.dumps(
                    {
                        "schema": "org.rushbuild.kbuild-observed-graph.v2",
                        "actions": [{**legacy, "receipt_path": None}],
                    }
                )
            )
            with self.assertRaisesRegex(ValueError, "legacy.*no receipt"):
                MODULE.load_base_graph(graph_path)

    def test_exact_exec_and_action_subtree_inputs(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            src = root / "src"
            obj = root / "obj"
            src.mkdir()
            obj.mkdir()
            source = src / "in.c"
            source.write_text("input\n")
            output = obj / "out"
            output.write_text("output\n")
            traces = {
                10: [
                    b'execve("/usr/bin/make", ["make", "out"], ["PATH=/usr/bin:/bin"]) = 0',
                    b'clone3({}, 88) = 11',
                ],
                11: [
                    b'execve("/bin/sh", ["sh", "-c", "cc in.c"], ["A=x\\n", "PATH=/usr/bin:/bin"]) = 0',
                    f'openat(AT_FDCWD<{src}>, "in.c", O_RDONLY) = 3<{source}>'.encode(),
                    f'newfstatat(AT_FDCWD<{obj}>, ".", {{st_mode=S_IFDIR|0755}}, 0) = 0'.encode(),
                    f'openat(AT_FDCWD<{obj}>, "out", O_WRONLY|O_CREAT, 0666) = 4<{output}>'.encode(),
                    b'chmod("out", 0755) = 0',
                ],
            }
            graph = MODULE.process_graph(traces)
            self.assertEqual(MODULE.descendants(10, graph), {10, 11})
            invocation = MODULE.invocation(traces[11], obj)
            self.assertEqual(invocation["argv"][2], list(b"cc in.c"))
            self.assertEqual(invocation["environment"][0]["value"], list(b"x\n"))
            self.assertEqual(invocation["working_directory"], str(src))
            inputs, writes = MODULE.scan_inputs(traces, {11}, obj)
            self.assertEqual(inputs[source], "file")
            self.assertEqual(inputs[obj], "directory")
            self.assertNotIn(output, inputs)
            self.assertIn(output, writes)

    def test_truncated_exec_fails_closed(self) -> None:
        line = b'execve("/bin/sh", ["sh", ...], ["A=B"]) = 0'
        with self.assertRaisesRegex(ValueError, "truncated"):
            MODULE.exec_record(line)

    def test_terminal_unfinished_record_is_complete(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            trace_dir = Path(temporary)
            terminal = b'epoll_wait(3,  <unfinished ...>) = ?'
            (trace_dir / "strace.10").write_bytes(terminal + b"\n")

            self.assertEqual(
                MODULE.load_traces(trace_dir, "strace"),
                {10: [terminal]},
            )

    def test_terminal_filesystem_record_still_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            trace_dir = Path(temporary)
            (trace_dir / "strace.10").write_bytes(
                b'openat(AT_FDCWD, "out", O_CREAT, 0666 '
                b'<unfinished ...>) = ?\n'
            )

            with self.assertRaisesRegex(ValueError, "incomplete syscall records"):
                MODULE.load_traces(trace_dir, "strace")

    def test_resumable_unfinished_record_still_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            trace_dir = Path(temporary)
            (trace_dir / "strace.10").write_bytes(
                b'epoll_wait(3,  <unfinished ...>\n'
                b'<... epoll_wait resumed>[], 1, -1) = 0\n'
            )

            with self.assertRaisesRegex(ValueError, "incomplete syscall records"):
                MODULE.load_traces(trace_dir, "strace")

    def test_cwd_change_before_observation_fails_closed(self) -> None:
        lines = [
            b'execve("/bin/sh", ["sh", "-c", "cd sub"], ["A=B"]) = 0',
            b'chdir("sub") = 0',
        ]
        with self.assertRaisesRegex(ValueError, "changed before it was observed"):
            MODULE.invocation(lines, Path("/fallback"))

    def test_runtime_pseudo_paths_are_not_inputs(self) -> None:
        traces = {
            10: [
                b'openat(AT_FDCWD</work>, "/dev/null", O_RDONLY) = '
                b'0</dev/null<char 1:3>>',
                b'openat(AT_FDCWD</work>, "/proc/self/fd", '
                b'O_RDONLY|O_DIRECTORY) = 3</proc/10/fd>',
                b'getdents64(3</proc/10/fd>, [], 32768) = 0',
                b'openat(AT_FDCWD</work>, "/proc/self/maps", O_RDONLY) = '
                b'4</proc/10/maps>',
                b'openat(AT_FDCWD</work>, "/proc/self/statm", O_RDONLY) = '
                b'5</proc/10/statm>',
            ]
        }
        inputs, writes = MODULE.scan_inputs(traces, {10}, Path("/work"))
        self.assertEqual(inputs, {})
        self.assertEqual(writes, set())

    def test_other_proc_paths_remain_observed(self) -> None:
        self.assertFalse(MODULE.is_runtime_pseudo_path(Path("/proc/cpuinfo")))
        self.assertFalse(MODULE.is_runtime_pseudo_path(Path("/proc/10/status")))

    def test_action_owned_temporary_can_be_renamed_to_output(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            temporary_output = root / ".out.tmp"
            output = root / "out"
            output.write_text("output\n")
            traces = {
                10: [
                    f'openat(AT_FDCWD<{root}>, ".out.tmp", O_WRONLY|O_CREAT, 0666) '
                    f'= 3<{temporary_output}>'.encode(),
                    f'renameat2(AT_FDCWD<{root}>, ".out.tmp", '
                    f'AT_FDCWD<{root}>, "out", RENAME_NOREPLACE) = 0'.encode(),
                ]
            }

            inputs, writes = MODULE.scan_inputs(traces, {10}, root)

            self.assertEqual(inputs, {})
            self.assertEqual(writes, {temporary_output, output})

    def test_rename_requires_an_action_owned_source(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "out"
            output.write_text("output\n")
            traces = {
                10: [
                    f'renameat2(AT_FDCWD<{root}>, ".external", '
                    f'AT_FDCWD<{root}>, "out", RENAME_NOREPLACE) = 0'.encode(),
                ]
            }

            with self.assertRaisesRegex(ValueError, "not written by the action"):
                MODULE.scan_inputs(traces, {10}, root)

    def test_hard_link_observes_source_and_owns_destination(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            destination = root / "destination"
            source.write_text("input\n")
            destination.write_text("input\n")
            traces = {10: [f'link("source", "destination") = 0'.encode()]}

            inputs, writes = MODULE.scan_inputs(traces, {10}, root)

            self.assertEqual(inputs, {source: "file"})
            self.assertEqual(writes, {destination})

    def test_symlink_target_is_payload_not_an_input_path(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            destination = root / "destination"
            destination.symlink_to("missing-target")
            traces = {
                10: [
                    f'symlinkat("missing-target", AT_FDCWD<{root}>, '
                    '"destination") = 0'.encode(),
                ]
            }

            inputs, writes = MODULE.scan_inputs(traces, {10}, root)

            self.assertEqual(inputs, {})
            self.assertEqual(writes, {destination})

    def test_multi_path_mutation_without_directory_evidence_fails(self) -> None:
        traces = {10: [b'renameat2(3, "tmp", 4, "out", RENAME_NOREPLACE) = 0']}

        with self.assertRaisesRegex(ValueError, "unresolved paths"):
            MODULE.scan_inputs(traces, {10}, Path("/fallback"))

    def test_null_path_utimensat_uses_annotated_descriptor(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "out"
            output.write_text("output\n")
            traces = {
                10: [
                    f'utimensat(3<{output}>, NULL, '
                    "[{tv_sec=1, tv_nsec=0}, {tv_sec=1, tv_nsec=0}], 0) = 0".encode()
                ]
            }

            inputs, writes = MODULE.scan_inputs(traces, {10}, Path(temporary))

            self.assertEqual(inputs, {})
            self.assertEqual(writes, {output})

    def test_null_path_utimensat_without_annotation_fails(self) -> None:
        traces = {
            10: [
                b"utimensat(3, NULL, "
                b"[{tv_sec=1, tv_nsec=0}, {tv_sec=1, tv_nsec=0}], 0) = 0"
            ]
        }

        with self.assertRaisesRegex(ValueError, "no resolved path"):
            MODULE.scan_inputs(traces, {10}, Path("/fallback"))

    def test_empty_at_path_on_anonymous_descriptor_is_not_cwd(self) -> None:
        traces = {
            10: [
                b'newfstatat(1<pipe:[123]>, "", {st_mode=S_IFIFO|0600}, '
                b'AT_EMPTY_PATH) = 0'
            ]
        }

        inputs, writes = MODULE.scan_inputs(traces, {10}, Path("/fallback"))

        self.assertEqual(inputs, {})
        self.assertEqual(writes, set())

    def test_ignores_only_mktemp_runtime_entropy(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            created = root / "temporary"
            created.write_bytes(b"")
            mktemp = {
                10: [
                    b'execve("/usr/bin/mktemp", ["mktemp", "/tmp/value.XXXXXX"], '
                    b'["A=B"]) = 0',
                    b'openat(AT_FDCWD, "/dev/urandom", O_RDONLY) = '
                    b'3</dev/urandom<char 1:9>>',
                    f'openat(AT_FDCWD, "{created}", '
                    f'O_RDWR|O_CREAT|O_EXCL, 0600) = 3<{created}>'.encode(),
                ]
            }
            inputs, writes = MODULE.scan_inputs(mktemp, {10}, root)
            self.assertNotIn(Path("/dev/urandom<char 1:9"), inputs)
            self.assertIn(created, writes)

            other = {
                10: [
                    b'execve("/usr/bin/true", ["true"], ["A=B"]) = 0',
                    *mktemp[10][1:],
                ]
            }
            with self.assertRaisesRegex(ValueError, "not a regular file"):
                MODULE.scan_inputs(other, {10}, root)

    def test_flattens_one_nested_analyzer_owned_by_base_graph(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            obj = root / "obj"
            obj.mkdir()
            nested_output = obj / "nested.o"
            nested_side_effect = obj / "nested.stamp"
            output = obj / "final"
            receipt = obj / ".final.cmd"
            nested_output.write_bytes(b"nested\n")
            nested_side_effect.write_bytes(b"stamp\n")
            output.write_bytes(b"final\n")
            receipt.write_bytes(b"savedcmd_final := link final\n")
            traces = {
                1: [
                    b'execve("/bin/sh", ["sh", "-c", "exec make all"], '
                    b'["A=B"]) = 0',
                    b"clone3({}, 88) = 10",
                ],
                10: [
                    b'execve("/bin/sh", ["/bin/sh", "-c", "link final"], '
                    b'["MAKE=/usr/bin/make"]) = 0',
                    b"clone3({}, 88) = 11",
                    b"clone3({}, 88) = 13",
                    f'openat(AT_FDCWD<{obj}>, "nested.o", O_RDONLY) '
                    f'= 3<{nested_output}>'.encode(),
                    f'openat(AT_FDCWD<{obj}>, "final", O_WRONLY|O_CREAT) '
                    f'= 4<{output}>'.encode(),
                    f'openat(AT_FDCWD<{obj}>, ".final.cmd", O_WRONLY|O_CREAT) '
                    f'= 5<{receipt}>'.encode(),
                ],
                11: [
                    b'execve("/usr/bin/make", ["make", "nested.o"], '
                    b'["MAKE=/usr/bin/make"]) = 0',
                    b"clone3({}, 88) = 12",
                    f'openat(AT_FDCWD<{obj}>, "nested.o", O_WRONLY|O_CREAT) '
                    f'= 6<{nested_output}>'.encode(),
                    f'openat(AT_FDCWD<{obj}>, "nested.stamp", O_WRONLY|O_CREAT) '
                    f'= 7<{nested_side_effect}>'.encode(),
                ],
                12: [b'execve("/bin/true", ["true"], ["A=B"]) = 0'],
                13: [b'execve("/usr/bin/true", ["true"], ["A=B"]) = 0'],
            }
            analyzer = MODULE.invocation(traces[1], root)
            base_graph = {
                "actions": [
                    {
                        "schema": MODULE.SCHEMA,
                        "analyzer": analyzer,
                        "requested_goals": [list(b"all")],
                        "persistent_outputs": [str(nested_output)],
                    }
                ]
            }

            captured, summary = MODULE.flatten_nested_analyzer_action(
                traces,
                base_graph,
                10,
                obj,
                obj,
                receipt,
                [output, receipt],
            )

            environment = {
                bytes(entry["name"]): bytes(entry["value"])
                for entry in captured["commands"][0]["environment"]
            }
            self.assertEqual(environment[b"MAKE"], b":")
            self.assertEqual(captured["analysis_inputs"], [])
            self.assertEqual(summary["nested_analyzer_roots"], 1)
            self.assertEqual(summary["excluded_processes"], 2)
            self.assertEqual(summary["excluded_persistent_outputs"], 2)
            self.assertEqual(summary["consumed_nested_outputs"], 1)
            self.assertIn(
                {"path": str(nested_output), "kind": "file"},
                captured["action_inputs"],
            )

    def test_flatten_rejects_unowned_nested_analyzer_output(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            obj = root / "obj"
            obj.mkdir()
            nested_output = obj / "nested.o"
            output = obj / "final"
            receipt = obj / ".final.cmd"
            for path in (nested_output, output, receipt):
                path.write_bytes(b"value\n")
            traces = {
                1: [
                    b'execve("/bin/sh", ["sh", "-c", "exec make"], '
                    b'["A=B"]) = 0',
                    b"clone3({}, 88) = 10",
                ],
                10: [
                    b'execve("/bin/sh", ["/bin/sh", "-c", "link"], '
                    b'["MAKE=/usr/bin/make"]) = 0',
                    b"clone3({}, 88) = 11",
                    f'openat(AT_FDCWD<{obj}>, "nested.o", O_RDONLY) '
                    f'= 3<{nested_output}>'.encode(),
                    f'openat(AT_FDCWD<{obj}>, "final", O_WRONLY) '
                    f'= 4<{output}>'.encode(),
                    f'openat(AT_FDCWD<{obj}>, ".final.cmd", O_WRONLY) '
                    f'= 5<{receipt}>'.encode(),
                ],
                11: [
                    b'execve("/usr/bin/make", ["make"], '
                    b'["MAKE=/usr/bin/make"]) = 0',
                    f'openat(AT_FDCWD<{obj}>, "nested.o", O_WRONLY) '
                    f'= 6<{nested_output}>'.encode(),
                ],
            }
            analyzer = MODULE.invocation(traces[1], root)
            base_graph = {
                "actions": [
                    {
                        "schema": MODULE.SCHEMA,
                        "analyzer": analyzer,
                        "requested_goals": [list(b"all")],
                        "persistent_outputs": [],
                    }
                ]
            }
            with self.assertRaisesRegex(ValueError, "unowned"):
                MODULE.flatten_nested_analyzer_action(
                    traces,
                    base_graph,
                    10,
                    obj,
                    obj,
                    receipt,
                    [output, receipt],
                )


if __name__ == "__main__":
    unittest.main()
