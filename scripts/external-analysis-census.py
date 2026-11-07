#!/usr/bin/env python3
"""Summarize a strace -ff external build capture without leaking host paths."""

from __future__ import annotations

import argparse
import collections
import json
import os
import re
from pathlib import Path, PurePosixPath
from typing import BinaryIO


SCHEMA = "org.rushbuild.external-analysis-census.v2"
SYSCALL_RE = re.compile(rb"^([a-zA-Z0-9_]+)\(")
EXEC_RE = re.compile(rb'^execve\("([^"\\]*)",')
QUOTED_ABSOLUTE_RE = re.compile(rb'"(/[^"\\]*)"')
ANNOTATED_ABSOLUTE_RE = re.compile(rb"<(/[^>]*)>")
AT_CWD_PATH_RE = re.compile(rb'AT_FDCWD<(/[^>]*)>, "([^"\\]*)"')
CLONE_RESULT_RE = re.compile(rb"\)\s+=\s+([0-9]+)$")
WRITE_FLAGS = (b"O_WRONLY", b"O_RDWR", b"O_CREAT", b"O_TRUNC", b"O_APPEND")
MUTATING_SYSCALLS = {
    b"chmod",
    b"fchmodat",
    b"link",
    b"linkat",
    b"mkdir",
    b"mkdirat",
    b"mknod",
    b"mknodat",
    b"rename",
    b"renameat",
    b"renameat2",
    b"rmdir",
    b"symlink",
    b"symlinkat",
    b"truncate",
    b"unlink",
    b"unlinkat",
    b"utime",
    b"utimensat",
}
PROCESS_SYSCALLS = {b"clone", b"clone3", b"fork", b"vfork"}


def skip_c_string(data: bytes, position: int) -> int | None:
    if position >= len(data) or data[position] != ord('"'):
        return None
    position += 1
    while position < len(data):
        if data[position] == ord('"'):
            return position + 1
        if data[position] == ord("\\"):
            position += 1
            if position >= len(data):
                return None
        position += 1
    return None


def skip_string_array(data: bytes, position: int) -> tuple[int | None, str]:
    if position >= len(data) or data[position] != ord("["):
        return None, "pointer"
    position += 1
    while True:
        while position < len(data) and data[position] in b" \t":
            position += 1
        if position >= len(data):
            return None, "malformed"
        if data[position] == ord("]"):
            return position + 1, "complete"
        if data[position : position + 3] == b"...":
            return None, "truncated"
        position = skip_c_string(data, position)
        if position is None:
            return None, "malformed"
        while position < len(data) and data[position] in b" \t":
            position += 1
        if position < len(data) and data[position] == ord(","):
            position += 1
            continue
        if position >= len(data) or data[position] != ord("]"):
            return None, "malformed"


def successful_exec_shape(line: bytes) -> str:
    position = len(b"execve(")
    position = skip_c_string(line, position)
    if position is None:
        return "malformed"
    while position < len(line) and line[position] in b" \t":
        position += 1
    if position >= len(line) or line[position] != ord(","):
        return "malformed"
    position += 1
    while position < len(line) and line[position] in b" \t":
        position += 1
    position, shape = skip_string_array(line, position)
    if position is None:
        return "argv_" + shape
    while position < len(line) and line[position] in b" \t":
        position += 1
    if position >= len(line) or line[position] != ord(","):
        return "malformed"
    position += 1
    while position < len(line) and line[position] in b" \t":
        position += 1
    position, shape = skip_string_array(line, position)
    if position is None:
        return "environment_" + shape
    return "complete"


def classify_program(program: str) -> str:
    name = os.path.basename(program)
    if name in {"make", "gmake"}:
        return "make"
    if name in {"sh", "bash", "dash"}:
        return "shell"
    if name in {"cc", "gcc", "clang", "as"} or name.endswith(
        ("-gcc", "-clang")
    ):
        return "compile"
    if name in {"ld", "ld.lld", "gold"} or name.endswith("-ld"):
        return "link"
    if name in {"ar", "llvm-ar"} or name.endswith("-ar"):
        return "archive"
    if name in {"objcopy", "strip", "nm", "modpost", "fixdep"}:
        return "codegen"
    return "other"


def observation_kind(syscall: bytes, line: bytes) -> str:
    if b"= -1 ENOENT" in line:
        return "missing"
    if syscall in MUTATING_SYSCALLS:
        return "write"
    if syscall in {b"open", b"openat", b"openat2"} and any(
        flag in line for flag in WRITE_FLAGS
    ):
        return "write"
    return "read"


def normalized_root_path(
    raw: bytes, roots: dict[str, bytes]
) -> tuple[str, str] | None:
    for name, root in roots.items():
        if raw == root:
            return name, "."
        prefix = root + b"/"
        if raw.startswith(prefix):
            relative = raw[len(prefix) :]
            if b"\\" in relative:
                return None
            normalized = str(PurePosixPath(os.fsdecode(relative)))
            if normalized != ".." and not normalized.startswith("../"):
                return name, normalized
    return None


def observed_paths(
    syscall: bytes, line: bytes, roots: dict[str, bytes]
) -> set[tuple[str, str]]:
    paths: set[tuple[str, str]] = set()
    quoted_paths = list(QUOTED_ABSOLUTE_RE.finditer(line))
    # The first pathname-looking argument to symlink(2) is link contents, not
    # an accessed filesystem object. Kbuild uses an absolute source-root target
    # for the output tree's `source` link; counting that string as a source-tree
    # write would turn a zero-change build into a false mutation.
    if syscall in {b"symlink", b"symlinkat"} and quoted_paths:
        quoted_paths = quoted_paths[1:]
    for match in quoted_paths:
        rooted = normalized_root_path(match.group(1), roots)
        if rooted:
            paths.add(rooted)
    for match in ANNOTATED_ABSOLUTE_RE.finditer(line):
        if line[: match.start()].endswith(b"AT_FDCWD"):
            continue
        rooted = normalized_root_path(match.group(1), roots)
        if rooted:
            paths.add(rooted)
    match = AT_CWD_PATH_RE.search(line)
    if match and not match.group(2).startswith(b"/"):
        rooted = normalized_root_path(
            os.path.normpath(match.group(1) + b"/" + match.group(2)),
            roots,
        )
        if rooted:
            paths.add(rooted)
    return paths


class Census:
    def __init__(self, source_root: Path, output_root: Path) -> None:
        self.roots = {
            "src": os.fsencode(source_root.resolve()),
            "obj": os.fsencode(output_root.resolve()),
        }
        self.trace_files = 0
        self.trace_bytes = 0
        self.lines = 0
        self.unfinished = 0
        self.resumed = 0
        self.terminal_unfinished = 0
        self.truncated_exec = 0
        self.incomplete_exec = 0
        self.pointer_argv_exec = 0
        self.pointer_environment_exec = 0
        self.clone_edges = 0
        self.syscalls: collections.Counter[str] = collections.Counter()
        self.programs: collections.Counter[str] = collections.Counter()
        self.program_classes: collections.Counter[str] = collections.Counter()
        self.pid_program: dict[int, str] = {}
        self.writer_pids: set[int] = set()
        self.observations = {
            root: {kind: set() for kind in ("read", "write", "missing")}
            for root in self.roots
        }

    def scan_file(self, path: Path, pid: int) -> None:
        self.trace_files += 1
        self.trace_bytes += path.stat().st_size
        with path.open("rb", buffering=1024 * 1024) as stream:
            self.scan_stream(stream, pid)

    def scan_stream(self, stream: BinaryIO, pid: int) -> None:
        for line in stream:
            self.lines += 1
            if b"<unfinished ...>" in line:
                if line.rstrip().endswith(b") = ?"):
                    self.terminal_unfinished += 1
                else:
                    self.unfinished += 1
            if line.startswith(b"<... "):
                self.resumed += 1
                continue
            syscall_match = SYSCALL_RE.match(line)
            if not syscall_match:
                continue
            syscall = syscall_match.group(1)
            self.syscalls[os.fsdecode(syscall)] += 1
            if syscall == b"execve" and b") = 0" in line:
                match = EXEC_RE.match(line)
                if match:
                    program = os.fsdecode(match.group(1))
                    self.pid_program[pid] = program
                    self.programs[os.path.basename(program)] += 1
                    self.program_classes[classify_program(program)] += 1
                shape = successful_exec_shape(line)
                if shape in {"argv_truncated", "environment_truncated"}:
                    self.truncated_exec += 1
                if shape != "complete":
                    self.incomplete_exec += 1
                if shape == "argv_pointer":
                    self.pointer_argv_exec += 1
                if shape == "environment_pointer":
                    self.pointer_environment_exec += 1
            if syscall in PROCESS_SYSCALLS and CLONE_RESULT_RE.search(line.rstrip()):
                self.clone_edges += 1
            if not any(root in line for root in self.roots.values()):
                continue
            kind = observation_kind(syscall, line)
            for root, relative in observed_paths(syscall, line, self.roots):
                self.observations[root][kind].add(relative)
                if root == "obj" and kind == "write":
                    self.writer_pids.add(pid)

    def result(self, output_root: Path) -> dict[str, object]:
        cmd_files = 0
        output_files = 0
        output_symlinks = 0
        output_directories = 0
        for _, directories, files in os.walk(output_root):
            output_directories += len(directories)
            for name in files:
                output_files += 1
                cmd_files += name.endswith(".cmd")
            # os.walk lists directory symlinks in directories and file
            # symlinks in files. Count both without following them.
            # This is a census denominator, not an output manifest.
        for path in output_root.rglob("*"):
            output_symlinks += path.is_symlink()

        candidate_actions = 0
        writer_programs: collections.Counter[str] = collections.Counter()
        for pid in self.writer_pids:
            program = self.pid_program.get(pid)
            if not program:
                continue
            category = classify_program(program)
            writer_programs[category] += 1
            if category not in {"make", "shell"}:
                candidate_actions += 1

        getdents = self.syscalls.get("getdents64", 0)
        risks = []
        if getdents == 0:
            risks.append("directory_membership_not_captured")
        if self.truncated_exec:
            risks.append("truncated_exec_records")
        if self.incomplete_exec:
            risks.append("incomplete_exec_records")
        if self.unfinished != self.resumed:
            risks.append("unfinished_resumed_count_mismatch")

        return {
            "schema": SCHEMA,
            "trace": {
                "files": self.trace_files,
                "bytes": self.trace_bytes,
                "lines": self.lines,
                "unfinished_records": self.unfinished,
                "resumed_records": self.resumed,
                "terminal_unfinished_records": self.terminal_unfinished,
                "truncated_successful_execs": self.truncated_exec,
                "incomplete_successful_execs": self.incomplete_exec,
                "pointer_argv_successful_execs": self.pointer_argv_exec,
                "pointer_environment_successful_execs": (
                    self.pointer_environment_exec
                ),
            },
            "processes": {
                "successful_execs": sum(self.programs.values()),
                "program_classes": dict(sorted(self.program_classes.items())),
                "programs_top": self.programs.most_common(25),
                "clone_edges": self.clone_edges,
                "object_writer_processes": len(self.writer_pids),
                "candidate_action_processes": candidate_actions,
                "writer_program_classes": dict(sorted(writer_programs.items())),
            },
            "filesystem": {
                root: {
                    f"unique_{kind}_paths": len(values)
                    for kind, values in kinds.items()
                }
                for root, kinds in self.observations.items()
            },
            "directory_membership": {
                "getdents64_calls": getdents,
                "captured": getdents > 0,
            },
            "output_tree": {
                "files": output_files,
                "directories": output_directories,
                "symlinks": output_symlinks,
                "cmd_files": cmd_files,
            },
            "syscalls_top": self.syscalls.most_common(30),
            "risks": risks,
        }


def trace_pid(path: Path, prefix: str) -> int | None:
    marker = prefix + "."
    if not path.name.startswith(marker):
        return None
    suffix = path.name[len(marker) :]
    return int(suffix) if suffix.isdigit() else None


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--trace-dir", required=True, type=Path)
    parser.add_argument("--trace-prefix", default="strace")
    parser.add_argument("--source-root", required=True, type=Path)
    parser.add_argument("--output-root", required=True, type=Path)
    parser.add_argument("--json", type=Path)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    census = Census(args.source_root, args.output_root)
    for path in sorted(args.trace_dir.iterdir()):
        pid = trace_pid(path, args.trace_prefix)
        if pid is not None and path.is_file():
            census.scan_file(path, pid)
    result = census.result(args.output_root)
    encoded = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if args.json:
        args.json.write_text(encoded, encoding="utf-8")
    else:
        print(encoded, end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
