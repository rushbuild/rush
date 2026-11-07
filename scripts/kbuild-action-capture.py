#!/usr/bin/env python3
"""Extract one complete Kbuild action boundary from a strace -ff capture."""

from __future__ import annotations

import argparse
import json
import os
import re
from collections import defaultdict, deque
from pathlib import Path
from typing import Any


SCHEMA = "org.rushbuild.kbuild-observed-action.v3"
LEGACY_SCHEMA = "org.rushbuild.kbuild-observed-action.v1"
LEGACY_SCHEMAS = {
    LEGACY_SCHEMA,
    "org.rushbuild.kbuild-observed-action.v2",
}
GRAPH_SCHEMAS = {
    "org.rushbuild.kbuild-observed-graph.v1",
    "org.rushbuild.kbuild-observed-graph.v2",
    "org.rushbuild.kbuild-observed-graph.v3",
    "org.rushbuild.kbuild-observed-graph.v4",
}
SYSCALL_RE = re.compile(rb"^([A-Za-z0-9_]+)\(")
CHILD_RE = re.compile(rb"\)\s+=\s+([0-9]+)$")
PROCESS_CALLS = {b"clone", b"clone3", b"fork", b"vfork"}
TERMINAL_PROCESS_STATE_CALLS = {b"epoll_wait", b"rt_sigprocmask"}
WRITE_FLAGS = (b"O_WRONLY", b"O_RDWR", b"O_CREAT", b"O_TRUNC", b"O_APPEND")
METADATA_CALLS = {
    b"access",
    b"faccessat",
    b"faccessat2",
    b"lstat",
    b"newfstatat",
    b"stat",
    b"statx",
}
SINGLE_PATH_MUTATIONS = {
    b"chmod",
    b"fchmodat",
    b"mkdir",
    b"mkdirat",
    b"mknod",
    b"mknodat",
    b"rmdir",
    b"truncate",
    b"unlink",
    b"unlinkat",
    b"utime",
    b"utimensat",
}
MULTI_PATH_MUTATIONS = {
    b"link",
    b"linkat",
    b"rename",
    b"renameat",
    b"renameat2",
    b"symlink",
    b"symlinkat",
}
RETURNED_PATH_RE = re.compile(rb"\)\s+=\s+[0-9]+<(/[^>]*)>")
DIRFD_RE = re.compile(rb"(?:AT_FDCWD|[0-9]+)<(/[^>]*)>,\s*")
AT_FDCWD_RE = re.compile(rb"AT_FDCWD<(/[^>]*)>")
FD_PATH_RE = re.compile(rb"^[^(]+\([0-9]+<(/[^>]*)>")
NULL_PATH_UTIMENSAT_RE = re.compile(rb"^utimensat\([0-9]+<(/[^>]*)>,\s*NULL,")


def c_string(data: bytes, position: int) -> tuple[bytes, int]:
    if position >= len(data) or data[position] != ord('"'):
        raise ValueError("expected C string")
    position += 1
    value = bytearray()
    escapes = {
        ord("a"): 7,
        ord("b"): 8,
        ord("f"): 12,
        ord("n"): 10,
        ord("r"): 13,
        ord("t"): 9,
        ord("v"): 11,
        ord("\\"): 92,
        ord('"'): 34,
    }
    while position < len(data):
        byte = data[position]
        position += 1
        if byte == ord('"'):
            return bytes(value), position
        if byte != ord("\\"):
            value.append(byte)
            continue
        if position == len(data):
            raise ValueError("unfinished C escape")
        escaped = data[position]
        position += 1
        if escaped in escapes:
            value.append(escapes[escaped])
        elif ord("0") <= escaped <= ord("7"):
            digits = bytes([escaped])
            while (
                len(digits) < 3
                and position < len(data)
                and ord("0") <= data[position] <= ord("7")
            ):
                digits += bytes([data[position]])
                position += 1
            value.append(int(digits, 8))
        elif escaped == ord("x"):
            start = position
            while position < len(data) and chr(data[position]).lower() in "0123456789abcdef":
                position += 1
            if start == position:
                raise ValueError("empty hexadecimal C escape")
            value.append(int(data[start:position], 16) & 0xFF)
        else:
            value.append(escaped)
    raise ValueError("unterminated C string")


def string_array(data: bytes, position: int) -> tuple[list[bytes], int]:
    if position >= len(data) or data[position] != ord("["):
        raise ValueError("expected string array")
    position += 1
    values: list[bytes] = []
    while True:
        while position < len(data) and data[position] in b" \t":
            position += 1
        if position < len(data) and data[position] == ord("]"):
            return values, position + 1
        if data[position : position + 3] == b"...":
            raise ValueError("truncated string array")
        value, position = c_string(data, position)
        values.append(value)
        while position < len(data) and data[position] in b" \t":
            position += 1
        if position < len(data) and data[position] == ord(","):
            position += 1
            continue
        if position >= len(data) or data[position] != ord("]"):
            raise ValueError("malformed string array")


def exec_record(line: bytes) -> tuple[bytes, list[bytes], list[bytes]] | None:
    if not line.startswith(b"execve(") or not line.rstrip().endswith(b") = 0"):
        return None
    position = len(b"execve(")
    executable, position = c_string(line, position)
    position = line.index(b"[", position)
    argv, position = string_array(line, position)
    position = line.index(b"[", position)
    environment, _ = string_array(line, position)
    if not argv:
        raise ValueError("successful exec has empty argv")
    return executable, argv, environment


def trace_pid(path: Path, prefix: str) -> int | None:
    marker = prefix + "."
    suffix = path.name[len(marker) :] if path.name.startswith(marker) else ""
    return int(suffix) if suffix.isdigit() else None


def load_traces(trace_dir: Path, prefix: str) -> dict[int, list[bytes]]:
    traces: dict[int, list[bytes]] = {}
    for path in sorted(trace_dir.iterdir()):
        pid = trace_pid(path, prefix)
        if pid is not None and path.is_file():
            lines = path.read_bytes().splitlines()
            if any(
                (
                    b"<unfinished ...>" in line
                    and (
                        not line.rstrip().endswith(b") = ?")
                        or not (match := SYSCALL_RE.match(line))
                        or match.group(1) not in TERMINAL_PROCESS_STATE_CALLS
                    )
                )
                or line.startswith(b"<... ")
                for line in lines
            ):
                raise ValueError("selected capture has incomplete syscall records")
            traces[pid] = lines
    return traces


def process_graph(traces: dict[int, list[bytes]]) -> dict[int, set[int]]:
    children: dict[int, set[int]] = defaultdict(set)
    for pid, lines in traces.items():
        for line in lines:
            match = SYSCALL_RE.match(line)
            child = CHILD_RE.search(line)
            if match and match.group(1) in PROCESS_CALLS and child:
                children[pid].add(int(child.group(1)))
    return children


def descendants(root: int, children: dict[int, set[int]]) -> set[int]:
    result: set[int] = set()
    pending = deque([root])
    while pending:
        pid = pending.popleft()
        if pid in result:
            continue
        result.add(pid)
        pending.extend(children.get(pid, ()))
    return result


def process_parents(children: dict[int, set[int]]) -> dict[int, int]:
    parents: dict[int, int] = {}
    for parent, child_pids in children.items():
        for child in child_pids:
            if child in parents and parents[child] != parent:
                raise ValueError("captured process has more than one parent")
            parents[child] = parent
    return parents


def last_executable(lines: list[bytes]) -> bytes | None:
    records = [record for line in lines if (record := exec_record(line))]
    return records[-1][0] if records else None


def matching_analyzer_ancestor(
    traces: dict[int, list[bytes]],
    parents: dict[int, int],
    action_pid: int,
    analyzer: dict[str, Any],
) -> int:
    fallback = Path(analyzer["working_directory"])
    matches = []
    current = parents.get(action_pid)
    while current is not None:
        try:
            candidate = invocation(traces[current], fallback)
        except ValueError:
            candidate = None
        if candidate == analyzer:
            matches.append(current)
        current = parents.get(current)
    if len(matches) != 1:
        raise ValueError("action does not have exactly one matching analyzer ancestor")
    return matches[0]


def nested_analyzer_partition(
    traces: dict[int, list[bytes]],
    children: dict[int, set[int]],
    action_pid: int,
    analyzer_executable: bytes,
) -> tuple[set[int], set[int], set[int]]:
    whole = descendants(action_pid, children)
    parents = process_parents(children)
    candidates = {
        pid
        for pid in whole
        if pid != action_pid and last_executable(traces.get(pid, [])) == analyzer_executable
    }
    roots = {
        pid
        for pid in candidates
        if not any(ancestor in candidates for ancestor in ancestor_pids(pid, parents))
    }
    if len(roots) != 1:
        raise ValueError("action does not have exactly one nested analyzer subtree")
    excluded: set[int] = set()
    for root in roots:
        excluded.update(descendants(root, children))
    included = whole - excluded
    if action_pid not in included or not included:
        raise ValueError("nested analyzer exclusion removed the action boundary")
    return included, excluded, roots


def ancestor_pids(pid: int, parents: dict[int, int]) -> list[int]:
    result = []
    current = parents.get(pid)
    while current is not None:
        result.append(current)
        current = parents.get(current)
    return result


def observed_working_directory(lines: list[bytes], fallback: Path) -> Path:
    after_exec = False
    for line in lines:
        if exec_record(line):
            after_exec = True
            continue
        if not after_exec:
            continue
        match = SYSCALL_RE.match(line)
        if match and match.group(1) in {b"chdir", b"fchdir"} and b"= -1" not in line:
            raise ValueError("working directory changed before it was observed")
        observed = AT_FDCWD_RE.search(line)
        if observed:
            return Path(os.fsdecode(observed.group(1))).resolve()
    return fallback.resolve()


def invocation(lines: list[bytes], cwd: Path) -> dict[str, object]:
    records = [record for line in lines if (record := exec_record(line))]
    if len(records) != 1:
        raise ValueError(f"process boundary requires one successful exec, found {len(records)}")
    executable, argv, environment = records[0]
    entries = []
    names: set[bytes] = set()
    for raw in environment:
        if b"=" not in raw:
            raise ValueError("captured environment entry has no equals sign")
        name, value = raw.split(b"=", 1)
        if not name or name in names:
            raise ValueError("captured environment repeats or omits a name")
        names.add(name)
        entries.append({"name": list(name), "value": list(value)})
    return {
        "executable": os.fsdecode(executable),
        "argv": [list(value) for value in argv],
        "environment": entries,
        "working_directory": str(observed_working_directory(lines, cwd)),
    }


def quoted_strings(line: bytes) -> list[bytes]:
    values = []
    position = 0
    while True:
        position = line.find(b'"', position)
        if position < 0:
            return values
        value, position = c_string(line, position)
        values.append(value)


def absolute_call_path(line: bytes, default_cwd: Path | None = None) -> Path | None:
    strings = quoted_strings(line)
    if not strings:
        return None
    raw = strings[0]
    if raw.startswith(b"/"):
        return Path(os.fsdecode(raw))
    base = DIRFD_RE.search(line)
    if base:
        return Path(os.fsdecode(base.group(1))) / os.fsdecode(raw)
    if not raw:
        return None
    if default_cwd:
        return default_cwd / os.fsdecode(raw)
    return None


def single_path_mutation(
    line: bytes, syscall: bytes, default_cwd: Path | None = None
) -> Path | None:
    path = absolute_call_path(line, default_cwd)
    if path is not None:
        return path
    if syscall == b"utimensat":
        match = NULL_PATH_UTIMENSAT_RE.match(line)
        if match:
            return Path(os.fsdecode(match.group(1)))
    return None


def rooted_argument(raw: bytes, base: Path | None) -> Path | None:
    if raw.startswith(b"/"):
        return Path(os.fsdecode(raw))
    if base is None:
        return None
    return base / os.fsdecode(raw)


def multi_path_mutation(
    line: bytes, syscall: bytes, default_cwd: Path | None = None
) -> tuple[Path | None, Path] | None:
    strings = quoted_strings(line)
    dirfds = [Path(os.fsdecode(match.group(1))) for match in DIRFD_RE.finditer(line)]
    if syscall in {b"rename", b"link"}:
        if len(strings) != 2:
            return None
        source = rooted_argument(strings[0], default_cwd)
        destination = rooted_argument(strings[1], default_cwd)
    elif syscall in {b"renameat", b"renameat2", b"linkat"}:
        if len(strings) != 2 or len(dirfds) != 2:
            return None
        source = rooted_argument(strings[0], dirfds[0])
        destination = rooted_argument(strings[1], dirfds[1])
    elif syscall == b"symlink":
        if len(strings) != 2:
            return None
        source = None
        destination = rooted_argument(strings[1], default_cwd)
    elif syscall == b"symlinkat":
        if len(strings) != 2 or len(dirfds) != 1:
            return None
        source = None
        destination = rooted_argument(strings[1], dirfds[0])
    else:
        return None
    if destination is None or (source is None and syscall not in {b"symlink", b"symlinkat"}):
        return None
    return (
        Path(os.path.normpath(source)) if source is not None else None,
        Path(os.path.normpath(destination)),
    )


def returned_path(line: bytes) -> Path | None:
    match = RETURNED_PATH_RE.search(line)
    return Path(os.fsdecode(match.group(1))) if match else None


def is_mktemp_runtime_entropy(path: Path, lines: list[bytes]) -> bool:
    raw = os.fsencode(path)
    if raw != b"/dev/urandom" and not raw.startswith(b"/dev/urandom<char "):
        return False
    executable = last_executable(lines)
    if executable is None or os.path.basename(executable) != b"mktemp":
        return False
    for line in lines:
        syscall = SYSCALL_RE.match(line)
        if (
            syscall
            and syscall.group(1) in {b"open", b"openat", b"openat2"}
            and b"O_CREAT" in line
            and b"O_EXCL" in line
            and b"= -1" not in line
        ):
            return True
    return False


def is_runtime_pseudo_path(path: Path) -> bool:
    path = Path(os.path.normpath(path))
    if path == Path("/dev/null") or os.fspath(path).startswith("/dev/null<"):
        return True
    parts = path.parts
    return (
        len(parts) >= 4
        and parts[0] == "/"
        and parts[1] == "proc"
        and (parts[2] == "self" or parts[2].isdigit())
        and (
            parts[3] == "fd"
            or (len(parts) == 4 and parts[3] in {"maps", "statm"})
        )
    )


def add_observation(observations: dict[Path, str], path: Path, kind: str) -> None:
    path = Path(os.path.normpath(path))
    if is_runtime_pseudo_path(path):
        return
    priority = {
        "missing": 0,
        "metadata": 1,
        "directory": 2,
        "file": 2,
        "symlink": 3,
        "directory_listing": 4,
    }
    previous = observations.get(path)
    if previous is None or priority[kind] > priority[previous]:
        observations[path] = kind


def present_kind(path: Path) -> str:
    if path.is_symlink():
        return "symlink"
    if path.is_file():
        return "file"
    if path.is_dir():
        return "directory"
    raise ValueError(f"observed input is not a regular file or symlink: {path}")


def scan_inputs(
    traces: dict[int, list[bytes]],
    pids: set[int],
    default_cwd: Path | None = None,
    only_under: Path | None = None,
    strict_mutations: bool = True,
) -> tuple[dict[Path, str], set[Path]]:
    inputs: dict[Path, str] = {}
    writes: set[Path] = set()
    multi_path_mutations: list[tuple[bytes, Path | None, Path]] = []
    for pid in sorted(pids):
        for line in traces.get(pid, ()):
            match = SYSCALL_RE.match(line)
            if not match:
                continue
            syscall = match.group(1)
            if syscall == b"execve":
                record = exec_record(line)
                if record:
                    path = Path(os.fsdecode(record[0]))
                    if not path.is_absolute():
                        if default_cwd is None:
                            raise ValueError("relative exec has no captured working directory")
                        path = default_cwd / path
                    add_observation(inputs, path, "file")
                continue
            if syscall in {b"open", b"openat", b"openat2"}:
                path = returned_path(line)
                if path and any(flag in line for flag in WRITE_FLAGS):
                    path = Path(os.path.normpath(path))
                    if not is_runtime_pseudo_path(path):
                        writes.add(path)
                elif path:
                    if is_mktemp_runtime_entropy(path, traces.get(pid, [])):
                        continue
                    if b"O_DIRECTORY" not in line:
                        add_observation(inputs, path, "file")
                elif b"= -1 ENOENT" in line:
                    path = absolute_call_path(line, default_cwd)
                    if path:
                        add_observation(inputs, path, "missing")
                continue
            if syscall in SINGLE_PATH_MUTATIONS and b"= -1" not in line:
                path = single_path_mutation(line, syscall, default_cwd)
                if path:
                    path = Path(os.path.normpath(path))
                    if not is_runtime_pseudo_path(path):
                        writes.add(path)
                elif strict_mutations:
                    raise ValueError("successful file mutation has no resolved path")
                continue
            if syscall in METADATA_CALLS and b"= -1" not in line:
                path = absolute_call_path(line, default_cwd)
                if path:
                    add_observation(inputs, path, "metadata")
                continue
            if syscall in MULTI_PATH_MUTATIONS and b"= -1" not in line:
                mutation = multi_path_mutation(line, syscall, default_cwd)
                if mutation is None:
                    if strict_mutations:
                        raise ValueError("successful multi-path mutation has unresolved paths")
                    continue
                source, destination = mutation
                if not is_runtime_pseudo_path(destination):
                    writes.add(destination)
                multi_path_mutations.append((syscall, source, destination))
                continue
            if syscall == b"getdents64" and b"= -1" not in line:
                match = FD_PATH_RE.match(line)
                if match:
                    add_observation(
                        inputs, Path(os.fsdecode(match.group(1))), "directory_listing"
                    )
                continue
            if syscall in {b"readlink", b"readlinkat"} and b"= -1" not in line:
                path = absolute_call_path(line, default_cwd)
                if path:
                    add_observation(inputs, path, "symlink")
                continue
            if syscall in {b"readlink", b"readlinkat"} and b"= -1 EINVAL" in line:
                path = absolute_call_path(line, default_cwd)
                if path:
                    add_observation(inputs, path, "metadata")
                continue
            if b"= -1 ENOENT" in line:
                path = absolute_call_path(line, default_cwd)
                if path:
                    add_observation(inputs, path, "missing")
    for syscall, source, _ in multi_path_mutations:
        if source is None:
            continue
        if syscall in {b"rename", b"renameat", b"renameat2"}:
            if source not in writes and strict_mutations:
                raise ValueError("rename source was not written by the action")
        elif source not in writes:
            try:
                add_observation(inputs, source, present_kind(source))
            except ValueError:
                if strict_mutations:
                    raise
    for path in writes:
        inputs.pop(path, None)
    validated: dict[Path, str] = {}
    only_under = only_under.resolve() if only_under else None
    for path, kind in inputs.items():
        if only_under and path != only_under and only_under not in path.parents:
            continue
        if kind == "missing":
            if os.path.lexists(path):
                raise ValueError(f"captured missing input now exists: {path}")
        elif kind in {"file", "metadata"}:
            kind = present_kind(path)
        elif kind == "symlink" and not path.is_symlink():
            raise ValueError(f"captured symlink input changed shape: {path}")
        elif kind == "directory_listing" and not path.is_dir():
            raise ValueError(f"captured directory input changed shape: {path}")
        add_observation(validated, path, kind)
        if kind == "symlink":
            add_observation(validated, Path(os.path.realpath(path)), "file")
    return validated, writes


def rooted_subset(observations: dict[Path, str], root: Path) -> dict[Path, str]:
    root = root.resolve()
    return {
        path: kind
        for path, kind in observations.items()
        if path == root or root in path.parents
    }


def encode_inputs(observations: dict[Path, str]) -> list[dict[str, str]]:
    return [
        {"path": str(path), "kind": kind}
        for path, kind in sorted(observations.items(), key=lambda item: os.fsencode(item[0]))
    ]


def load_base_graph(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text())
    if not isinstance(value, dict) or value.get("schema") not in GRAPH_SCHEMAS:
        raise ValueError("input is not an observed Kbuild graph")
    actions = value.get("actions")
    if not isinstance(actions, list) or not actions:
        raise ValueError("base Kbuild graph has no actions")
    normalized_actions = []
    for action in actions:
        if not isinstance(action, dict) or action.get("schema") not in (
            LEGACY_SCHEMAS | {SCHEMA}
        ):
            raise ValueError("base Kbuild graph has an unsupported action schema")
        receipt_path = action.get("receipt_path")
        if action["schema"] == LEGACY_SCHEMA and not isinstance(receipt_path, str):
            raise ValueError("legacy base Kbuild action has no receipt")
        if receipt_path is not None and not isinstance(receipt_path, str):
            raise ValueError("base Kbuild action has an invalid receipt")
        normalized = dict(action)
        normalized["schema"] = SCHEMA
        if action["schema"] in LEGACY_SCHEMAS:
            command = normalized.pop("action", None)
            if not isinstance(command, dict):
                raise ValueError("legacy base Kbuild action has no command")
            normalized["commands"] = [command]
        elif not isinstance(normalized.get("commands"), list) or not normalized["commands"]:
            raise ValueError("base Kbuild action has no commands")
        normalized_actions.append(normalized)
    value = dict(value)
    value["actions"] = normalized_actions
    actions = normalized_actions
    analyzer = actions[0].get("analyzer")
    goals = actions[0].get("requested_goals")
    if any(
        action.get("schema") != SCHEMA
        or action.get("analyzer") != analyzer
        or action.get("requested_goals") != goals
        for action in actions
    ):
        raise ValueError("base Kbuild graph has mixed analyzer generations")
    return value


def replace_environment_value(
    captured: dict[str, Any], name: bytes, expected: bytes, replacement: bytes
) -> dict[str, Any]:
    result = json.loads(json.dumps(captured))
    matches = [
        entry
        for entry in result["environment"]
        if bytes(entry["name"]) == name
    ]
    if len(matches) != 1 or bytes(matches[0]["value"]) != expected:
        raise ValueError("captured action has an unexpected analyzer environment")
    matches[0]["value"] = list(replacement)
    return result


def flatten_nested_analyzer_action(
    traces: dict[int, list[bytes]],
    base_graph: dict[str, Any],
    action_pid: int,
    action_cwd: Path,
    output_root: Path,
    receipt: Path,
    outputs: list[Path],
) -> tuple[dict[str, Any], dict[str, int]]:
    actions = base_graph["actions"]
    analyzer = actions[0]["analyzer"]
    goals = actions[0]["requested_goals"]
    children = process_graph(traces)
    parents = process_parents(children)
    matching_analyzer_ancestor(traces, parents, action_pid, analyzer)
    action_cwd = action_cwd.resolve()
    captured_action = invocation(traces[action_pid], action_cwd)
    make_values = [
        bytes(entry["value"])
        for entry in captured_action["environment"]
        if bytes(entry["name"]) == b"MAKE"
    ]
    if len(make_values) != 1 or not Path(os.fsdecode(make_values[0])).is_absolute():
        raise ValueError("captured action has no absolute nested analyzer path")
    nested_analyzer_executable = make_values[0]
    included, excluded, nested_roots = nested_analyzer_partition(
        traces, children, action_pid, nested_analyzer_executable
    )

    action_inputs, writes = scan_inputs(traces, included, action_cwd)
    _, excluded_writes = scan_inputs(traces, excluded, action_cwd)
    output_root = output_root.resolve()
    declared_outputs = {
        Path(os.path.normpath(path))
        for action in actions
        for path in action.get("persistent_outputs", [])
    }
    outputs = [Path(os.path.normpath(path)) for path in outputs]
    if any(not path.is_absolute() for path in outputs):
        raise ValueError("flattened persistent output is not absolute")
    if len(set(outputs)) != len(outputs) or set(outputs) & declared_outputs:
        raise ValueError("flattened persistent output is repeated or already produced")
    for path in outputs:
        if not os.path.lexists(path) or path not in writes:
            raise ValueError("flattened persistent output was not observed as a write")
    persistent_writes = {
        path
        for path in writes
        if (path == output_root or output_root in path.parents)
        and os.path.lexists(path)
    }
    if persistent_writes != set(outputs):
        raise ValueError("flattened action left an undeclared persistent output")

    excluded_persistent = {
        path
        for path in excluded_writes
        if (path == output_root or output_root in path.parents)
        and os.path.lexists(path)
    }
    consumed_nested_outputs = excluded_persistent.intersection(action_inputs)
    if not consumed_nested_outputs:
        raise ValueError("flattened action does not consume a nested analyzer output")
    if not consumed_nested_outputs <= declared_outputs:
        raise ValueError("flattened action consumes an unowned nested analyzer output")

    receipt = Path(os.path.normpath(receipt))
    if not receipt.is_absolute() or receipt not in outputs:
        raise ValueError("flattened receipt is not one of the persistent outputs")
    captured_action = replace_environment_value(
        captured_action, b"MAKE", nested_analyzer_executable, b":"
    )
    result = {
        "schema": SCHEMA,
        "analyzer": analyzer,
        "commands": [captured_action],
        "receipt_path": str(receipt),
        "action_inputs": encode_inputs(action_inputs),
        "persistent_outputs": [str(path) for path in outputs],
        "analysis_inputs": [],
        "requested_goals": goals,
    }
    summary = {
        "action_processes": len(included),
        "excluded_processes": len(excluded),
        "nested_analyzer_roots": len(nested_roots),
        "action_inputs": len(action_inputs),
        "persistent_outputs": len(outputs),
        "excluded_persistent_outputs": len(excluded_persistent),
        "consumed_nested_outputs": len(consumed_nested_outputs),
    }
    return result, summary


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--trace-dir", required=True, type=Path)
    parser.add_argument("--trace-prefix", default="strace")
    parser.add_argument("--source-root", required=True, type=Path)
    parser.add_argument("--output-root", required=True, type=Path)
    parser.add_argument("--analyzer-pid", type=int)
    parser.add_argument("--base-graph", type=Path)
    parser.add_argument("--flatten-nested-analyzer", action="store_true")
    parser.add_argument("--analyzer-cwd", required=True, type=Path)
    parser.add_argument("--action-pid", required=True, type=int)
    parser.add_argument("--action-cwd", required=True, type=Path)
    parser.add_argument("--receipt", required=True, type=Path)
    parser.add_argument("--output", required=True, action="append", type=Path)
    parser.add_argument("--goal", required=True, action="append")
    parser.add_argument("--json", required=True, type=Path)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    traces = load_traces(args.trace_dir, args.trace_prefix)
    if args.action_pid not in traces:
        raise ValueError("selected action PID has no trace")
    if args.flatten_nested_analyzer:
        if args.base_graph is None or args.analyzer_pid is not None:
            raise ValueError(
                "nested-analyzer flattening requires only --base-graph"
            )
        base_graph = load_base_graph(args.base_graph)
        analyzer = base_graph["actions"][0]["analyzer"]
        if args.analyzer_cwd.resolve() != Path(
            analyzer["working_directory"]
        ).resolve():
            raise ValueError("analyzer working directory differs from base graph")
        goals = [list(os.fsencode(goal)) for goal in args.goal]
        if goals != base_graph["actions"][0]["requested_goals"]:
            raise ValueError("requested goals differ from base graph")
        args.source_root.resolve(strict=True)
        result, summary = flatten_nested_analyzer_action(
            traces,
            base_graph,
            args.action_pid,
            args.action_cwd,
            args.output_root,
            args.receipt,
            args.output,
        )
        args.json.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
        print(json.dumps(summary, sort_keys=True))
        return 0
    if args.base_graph is not None or args.analyzer_pid is None:
        raise ValueError("ordinary capture requires only --analyzer-pid")
    if args.analyzer_pid not in traces:
        raise ValueError("selected analyzer PID has no trace")
    children = process_graph(traces)
    action_pids = descendants(args.action_pid, children)
    action_inputs, writes = scan_inputs(traces, action_pids, args.action_cwd.resolve())

    programs = {}
    for pid, lines in traces.items():
        records = [record for line in lines if (record := exec_record(line))]
        if records:
            programs[pid] = os.path.basename(os.fsdecode(records[-1][0]))
    analyzer_pids = {pid for pid, program in programs.items() if program in {"make", "gmake"}}
    analysis_inputs, _ = scan_inputs(
        traces,
        analyzer_pids,
        only_under=args.source_root.resolve(),
        strict_mutations=False,
    )
    analysis_inputs = rooted_subset(analysis_inputs, args.source_root)

    outputs = [path.resolve() for path in args.output]
    for path in outputs:
        if not path.exists() or path not in writes:
            raise ValueError("declared persistent output was not observed as a write")
    output_root = args.output_root.resolve()
    undeclared_persistent = sorted(
        path
        for path in writes
        if (path == output_root or output_root in path.parents)
        and path.exists()
        and path not in outputs
    )
    if undeclared_persistent:
        raise ValueError("action left an undeclared persistent output")

    result = {
        "schema": SCHEMA,
        "analyzer": invocation(traces[args.analyzer_pid], args.analyzer_cwd),
        "commands": [invocation(traces[args.action_pid], args.action_cwd)],
        "receipt_path": str(args.receipt.resolve()),
        "action_inputs": encode_inputs(action_inputs),
        "persistent_outputs": [str(path) for path in outputs],
        "analysis_inputs": encode_inputs(analysis_inputs),
        "requested_goals": [list(os.fsencode(goal)) for goal in args.goal],
    }
    args.json.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    print(
        json.dumps(
            {
                "action_processes": len(action_pids),
                "action_inputs": len(action_inputs),
                "analysis_inputs": len(analysis_inputs),
                "persistent_outputs": len(outputs),
            },
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
