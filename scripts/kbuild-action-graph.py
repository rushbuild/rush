#!/usr/bin/env python3
"""Assemble observed Kbuild actions from one analyzer generation."""

from __future__ import annotations

import argparse
import json
import os
import re
from pathlib import Path
from typing import Any


ACTION_SCHEMA = "org.rushbuild.kbuild-observed-action.v3"
LEGACY_ACTION_SCHEMA = "org.rushbuild.kbuild-observed-action.v1"
LEGACY_ACTION_SCHEMAS = {
    LEGACY_ACTION_SCHEMA,
    "org.rushbuild.kbuild-observed-action.v2",
}
GRAPH_SCHEMA = "org.rushbuild.kbuild-observed-graph.v4"
LEGACY_GRAPH_SCHEMA = "org.rushbuild.kbuild-observed-graph.v1"
LEGACY_GRAPH_SCHEMAS = {
    LEGACY_GRAPH_SCHEMA,
    "org.rushbuild.kbuild-observed-graph.v2",
    "org.rushbuild.kbuild-observed-graph.v3",
}
CPIO_HEADER_BYTES = 110


def upgrade_action(value: Any) -> dict[str, Any]:
    if not isinstance(value, dict) or value.get("schema") not in {
        *LEGACY_ACTION_SCHEMAS,
        ACTION_SCHEMA,
    }:
        raise ValueError("input is not an observed Kbuild action")
    if not isinstance(value.get("persistent_outputs"), list):
        raise ValueError("observed Kbuild action has no output list")
    receipt_path = value.get("receipt_path")
    if value["schema"] == LEGACY_ACTION_SCHEMA and not isinstance(receipt_path, str):
        raise ValueError("legacy observed Kbuild action has no receipt")
    if receipt_path is not None and not isinstance(receipt_path, str):
        raise ValueError("observed Kbuild action has an invalid receipt")
    result = dict(value)
    result["schema"] = ACTION_SCHEMA
    if value["schema"] in LEGACY_ACTION_SCHEMAS:
        action = result.pop("action", None)
        if not isinstance(action, dict):
            raise ValueError("legacy observed Kbuild action has no command")
        result["commands"] = [action]
    elif not isinstance(result.get("commands"), list) or not result["commands"]:
        raise ValueError("observed Kbuild action has no commands")
    return result


def load_action(path: Path) -> dict[str, Any]:
    return upgrade_action(json.loads(path.read_text()))


def load_graph(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text())
    if not isinstance(value, dict) or value.get("schema") not in (
        LEGACY_GRAPH_SCHEMAS | {GRAPH_SCHEMA}
    ):
        raise ValueError("input is not an observed Kbuild graph")
    if not isinstance(value.get("actions"), list) or not isinstance(
        value.get("requested_outputs"), list
    ):
        raise ValueError("observed Kbuild graph has no action or output list")
    result = dict(value)
    result["actions"] = [upgrade_action(action) for action in value["actions"]]
    return result


def saved_command(path: Path) -> bytes:
    logical = bytearray()
    commands = []
    for physical in path.read_bytes().splitlines():
        continued = physical.rstrip().endswith(b"\\")
        part = physical.rstrip()[:-1] if continued else physical
        if logical:
            logical.extend(b" ")
        logical.extend(part)
        if continued:
            continue
        line = bytes(logical)
        logical.clear()
        if b" := " not in line:
            continue
        name, value = line.split(b" := ", 1)
        if name.startswith(b"savedcmd_") or name.startswith(b"cmd_"):
            commands.append(value.strip())
    if logical or len(commands) != 1 or not commands[0]:
        raise ValueError("initramfs receipt does not contain one saved command")
    if b"$" in commands[0]:
        raise ValueError("initramfs receipt contains an unsupported Make expansion")
    return commands[0]


def align_four(value: int) -> int:
    return (value + 3) & ~3


def newc_timestamp(path: Path) -> int:
    data = path.read_bytes()
    offset = 0
    timestamps = set()
    while offset + CPIO_HEADER_BYTES <= len(data):
        header = data[offset : offset + CPIO_HEADER_BYTES]
        if header[:6] not in {b"070701", b"070702"}:
            raise ValueError("initramfs output is not a newc archive")
        try:
            fields = [
                int(header[6 + index * 8 : 14 + index * 8], 16)
                for index in range(13)
            ]
        except ValueError as error:
            raise ValueError("initramfs newc header is malformed") from error
        mtime = fields[5]
        size = fields[6]
        name_size = fields[11]
        name_start = offset + CPIO_HEADER_BYTES
        name_end = name_start + name_size
        if name_size < 2 or name_end > len(data) or data[name_end - 1] != 0:
            raise ValueError("initramfs newc name is malformed")
        name = data[name_start : name_end - 1]
        payload_start = align_four(name_end)
        payload_end = payload_start + size
        if payload_end > len(data):
            raise ValueError("initramfs newc payload is truncated")
        offset = align_four(payload_end)
        if name == b"TRAILER!!!":
            if size or len(timestamps) != 1:
                raise ValueError("initramfs archive has no uniform entry timestamp")
            return next(iter(timestamps))
        timestamps.add(mtime)
    raise ValueError("initramfs newc archive has no trailer")


def stabilize_initramfs(graph: dict[str, Any], receipt_path: Path) -> dict[str, Any]:
    receipt = absolute_path(receipt_path, "initramfs receipt")
    matches = [
        (index, action)
        for index, action in enumerate(graph.get("actions", []))
        if action.get("receipt_path") is not None
        and absolute_path(Path(action["receipt_path"]), "observed Kbuild receipt")
        == receipt
    ]
    if len(matches) != 1:
        raise ValueError("graph does not contain exactly one initramfs receipt")
    if not receipt.name.startswith(".") or not receipt.name.endswith(".cmd"):
        raise ValueError("initramfs receipt has an unsupported name")
    output = receipt.with_name(receipt.name[1:-4])
    action_index, action = matches[0]
    if str(output) not in action.get("persistent_outputs", []):
        raise ValueError("initramfs archive is not an action output")
    timestamp = newc_timestamp(output)
    command = saved_command(receipt)
    if b"gen_initramfs.sh" not in command or b" -d " in command:
        raise ValueError("initramfs command cannot be stabilized")
    match = re.fullmatch(
        rb"(.*gen_initramfs\.sh[ \t]+-o[ \t]+\S+[ \t]+-l[ \t]+\S+)"
        rb"([ \t]+)(\S+)",
        command,
    )
    if match is None:
        raise ValueError("initramfs command does not have one unquoted input")
    if not match.group(3).startswith(b"/"):
        raise ValueError("initramfs input is not absolute")
    stabilized = (
        match.group(1)
        + match.group(2)
        + b'-d "@'
        + str(timestamp).encode()
        + b'"'
        + match.group(2)
        + match.group(3)
    )
    result = dict(graph)
    result["actions"] = list(graph["actions"])
    transformed = json.loads(json.dumps(action))
    result["actions"][action_index] = transformed
    argv = transformed["commands"][-1]["argv"]
    if sum(bytes(argument).count(command) for argument in argv) != 2:
        raise ValueError("initramfs command does not have executable and receipt evidence")
    replaced = False
    for index, argument in enumerate(argv):
        raw = bytes(argument)
        if not replaced and command in raw:
            argv[index] = list(raw.replace(command, stabilized, 1))
            replaced = True
    if not replaced or sum(bytes(argument).count(command) for argument in argv) != 1:
        raise ValueError("initramfs command stabilization was ambiguous")
    return result


def assemble(
    actions: list[dict[str, Any]],
    requested_outputs: list[Path],
    initial_inputs: list[Path] | None = None,
    empty_output_directories: list[Path] | None = None,
) -> dict[str, Any]:
    if not actions:
        raise ValueError("Kbuild graph requires at least one action")
    if not requested_outputs:
        raise ValueError("Kbuild graph requires at least one requested output")

    analyzer = actions[0].get("analyzer")
    goals = actions[0].get("requested_goals")
    declared: set[Path] = set()
    for action in actions:
        if action.get("schema") != ACTION_SCHEMA:
            raise ValueError("Kbuild graph contains an unsupported action schema")
        if action.get("analyzer") != analyzer or action.get("requested_goals") != goals:
            raise ValueError("Kbuild graph actions do not share one analyzer generation")
        for raw_output in action.get("persistent_outputs", []):
            output = absolute_path(Path(raw_output), "observed Kbuild output")
            if output in declared:
                raise ValueError("Kbuild graph captures the same output more than once")
            declared.add(output)

    requested: list[str] = []
    seen: set[Path] = set()
    for raw_output in requested_outputs:
        output = absolute_path(raw_output, "requested Kbuild output")
        if output in seen:
            raise ValueError("Kbuild graph repeats a requested output")
        if output not in declared:
            raise ValueError("requested Kbuild output has no observed producer")
        seen.add(output)
        requested.append(str(output))

    initial: list[str] = []
    seen_initial: set[Path] = set()
    for raw_input in initial_inputs or []:
        path = absolute_path(raw_input, "initial Kbuild input")
        if path in seen_initial:
            raise ValueError("Kbuild graph repeats an initial input")
        if path in declared:
            raise ValueError("initial Kbuild input has an observed producer")
        if not path.exists() and not path.is_symlink():
            raise ValueError("initial Kbuild input does not exist")
        seen_initial.add(path)
        initial.append(str(path))

    listings: list[dict[str, Any]] = []
    seen_listings: set[Path] = set()
    observed_listings = {
        absolute_path(Path(value["path"]), "observed directory listing")
        for action in actions
        for value in action.get("action_inputs", [])
        if value.get("kind") == "directory_listing"
    }
    for raw_directory in empty_output_directories or []:
        directory = absolute_path(raw_directory, "empty-output directory")
        if directory in seen_listings:
            raise ValueError("Kbuild graph repeats an empty-output directory")
        if directory not in observed_listings:
            raise ValueError("empty-output directory was not observed by an action")
        seen_listings.add(directory)
        listings.append(capture_directory_listing(directory))

    return {
        "schema": GRAPH_SCHEMA,
        "actions": actions,
        "requested_outputs": requested,
        "initial_inputs": initial,
        "empty_output_directory_listings": listings,
    }


def absolute_path(path: Path, what: str) -> Path:
    if not path.is_absolute():
        raise ValueError(f"{what} is not absolute")
    return Path(os.path.normpath(os.fspath(path)))


def capture_directory_listing(path: Path) -> dict[str, Any]:
    try:
        with os.scandir(os.fsencode(path)) as entries:
            members = sorted(entry.name for entry in entries)
    except OSError as error:
        raise ValueError(f"cannot read empty-output directory: {error}") from error
    return {
        "path": str(path),
        "members": [list(member) for member in members],
    }


def augment(
    graph: dict[str, Any],
    initial_inputs: list[Path],
    empty_output_directories: list[Path],
    appended_actions: list[dict[str, Any]] | None = None,
) -> dict[str, Any]:
    if graph.get("schema") not in LEGACY_GRAPH_SCHEMAS | {GRAPH_SCHEMA}:
        raise ValueError("input is not an observed Kbuild graph")
    if graph.get("initial_inputs") or graph.get(
        "empty_output_directory_listings"
    ):
        raise ValueError("Kbuild graph already carries initial state")
    return assemble(
        [
            upgrade_action(action)
            for action in graph.get("actions", []) + (appended_actions or [])
        ],
        [Path(path) for path in graph.get("requested_outputs", [])],
        initial_inputs,
        empty_output_directories,
    )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--action", action="append", type=Path)
    source.add_argument("--graph", type=Path)
    parser.add_argument("--append-action", action="append", type=Path, default=[])
    parser.add_argument("--stabilize-initramfs-receipt", type=Path)
    parser.add_argument(
        "--requested-output", action="append", type=Path, default=[]
    )
    parser.add_argument("--initial-input", action="append", type=Path, default=[])
    parser.add_argument(
        "--empty-output-directory", action="append", type=Path, default=[]
    )
    parser.add_argument("--json", required=True, type=Path)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.graph is not None:
        if args.requested_output:
            raise ValueError("--requested-output cannot replace graph outputs")
        graph = load_graph(args.graph)
        if args.stabilize_initramfs_receipt is not None:
            graph = stabilize_initramfs(graph, args.stabilize_initramfs_receipt)
        result = augment(
            graph,
            args.initial_input,
            args.empty_output_directory,
            [load_action(path) for path in args.append_action],
        )
    else:
        if args.append_action or args.stabilize_initramfs_receipt is not None:
            raise ValueError("graph transforms require --graph")
        result = assemble(
            [load_action(path) for path in args.action],
            args.requested_output,
            args.initial_input,
            args.empty_output_directory,
        )
    args.json.write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    print(
        json.dumps(
            {
                "actions": len(result["actions"]),
                "requested_outputs": len(result["requested_outputs"]),
                "initial_inputs": len(result["initial_inputs"]),
                "empty_output_directory_listings": len(
                    result["empty_output_directory_listings"]
                ),
            },
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
