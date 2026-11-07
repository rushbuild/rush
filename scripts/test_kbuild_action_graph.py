#!/usr/bin/env python3
"""Focused tests for kbuild-action-graph.py."""

from __future__ import annotations

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("kbuild-action-graph.py")
SPEC = importlib.util.spec_from_file_location("kbuild_action_graph", SCRIPT)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


def newc_entry(name: bytes, timestamp: int, payload: bytes = b"") -> bytes:
    fields = [
        1,
        0o100644,
        0,
        0,
        1,
        timestamp,
        len(payload),
        0,
        0,
        0,
        0,
        len(name) + 1,
        0,
    ]
    header = b"070701" + b"".join(f"{value:08x}".encode() for value in fields)
    value = header + name + b"\0"
    value += b"\0" * ((-len(value)) % 4)
    value += payload
    value += b"\0" * ((-len(value)) % 4)
    return value


def action(
    analyzer: str,
    goal: int,
    outputs: list[Path],
    inputs: list[dict[str, object]] | None = None,
) -> dict[str, object]:
    return {
        "schema": MODULE.ACTION_SCHEMA,
        "analyzer": {"identity": analyzer},
        "requested_goals": [[goal]],
        "commands": [{"argv": [list(b"true")]}],
        "persistent_outputs": [str(path) for path in outputs],
        "action_inputs": inputs or [],
    }


class KbuildActionGraphTest(unittest.TestCase):
    def test_stabilizes_initramfs_timestamp_and_preserves_receipt(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            receipt = root / ".initramfs_data.cpio.cmd"
            output = root / "initramfs_data.cpio"
            command = (
                b"sh /src/gen_initramfs.sh -o initramfs_data.cpio "
                b"-l .initramfs_data.cpio.d    /src/list"
            )
            receipt.write_bytes(
                b"savedcmd_initramfs_data.cpio := " + command + b"\n"
            )
            output.write_bytes(
                newc_entry(b"root", 123) + newc_entry(b"TRAILER!!!", 0)
            )
            captured = action("same", 1, [output, receipt])
            captured["receipt_path"] = str(receipt)
            shell = b"run " + command + b"; evidence " + command
            captured["commands"] = [{
                "argv": [list(b"sh"), list(b"-c"), list(shell)]
            }]
            graph = {
                "schema": MODULE.GRAPH_SCHEMA,
                "actions": [action("same", 1, [root / "receiptless"]), captured],
                "requested_outputs": [str(output)],
            }

            stabilized = MODULE.stabilize_initramfs(graph, receipt)

            argument = bytes(
                stabilized["actions"][1]["commands"][-1]["argv"][2]
            )
            self.assertIn(b'-d "@123"', argument)
            self.assertEqual(argument.count(command), 1)
            self.assertNotEqual(stabilized, graph)

    def test_upgrades_legacy_actions_and_graphs(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "output"
            receipt = root / ".output.cmd"
            legacy = action("same", 1, [output, receipt])
            legacy["schema"] = MODULE.LEGACY_ACTION_SCHEMA
            legacy["action"] = legacy.pop("commands")[0]
            legacy["receipt_path"] = str(receipt)
            graph_path = root / "graph.json"
            graph_path.write_text(
                json.dumps(
                    {
                        "schema": "org.rushbuild.kbuild-observed-graph.v2",
                        "actions": [legacy],
                        "requested_outputs": [str(output)],
                    }
                )
            )

            graph = MODULE.load_graph(graph_path)

            self.assertEqual(graph["actions"][0]["schema"], MODULE.ACTION_SCHEMA)
            self.assertEqual(graph["actions"][0]["receipt_path"], str(receipt))
            with self.assertRaisesRegex(ValueError, "legacy.*no receipt"):
                MODULE.upgrade_action({**legacy, "receipt_path": None})

    def test_rejects_nonuniform_initramfs_timestamp(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            archive = Path(temporary) / "archive"
            archive.write_bytes(
                newc_entry(b"one", 123)
                + newc_entry(b"two", 124)
                + newc_entry(b"TRAILER!!!", 0)
            )

            with self.assertRaisesRegex(ValueError, "uniform"):
                MODULE.newc_timestamp(archive)

    def test_assembles_one_generation_with_owned_request(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            first = root / "first"
            final = root / "final"
            graph = MODULE.assemble(
                [action("same", 1, [first]), action("same", 1, [final])],
                [final],
            )
            self.assertEqual(graph["schema"], MODULE.GRAPH_SCHEMA)
            self.assertEqual(len(graph["actions"]), 2)
            self.assertEqual(graph["requested_outputs"], [str(final)])
            self.assertEqual(graph["initial_inputs"], [])
            self.assertEqual(graph["empty_output_directory_listings"], [])

    def test_rejects_mixed_generation_and_ambiguous_outputs(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "out"
            with self.assertRaisesRegex(ValueError, "analyzer generation"):
                MODULE.assemble(
                    [action("first", 1, [output]), action("second", 1, [])],
                    [output],
                )
            with self.assertRaisesRegex(ValueError, "same output"):
                MODULE.assemble(
                    [action("same", 1, [output]), action("same", 1, [output])],
                    [output],
                )

    def test_rejects_unowned_requested_output(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with self.assertRaisesRegex(ValueError, "no observed producer"):
                MODULE.assemble(
                    [action("same", 1, [root / "owned"])], [root / "missing"]
                )

    def test_captures_explicit_initial_state_and_directory_members(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "out"
            listing = root / "listing"
            initial_dir = root / "bootstrap"
            listing.mkdir()
            initial_dir.mkdir()
            (listing / "z").write_bytes(b"")
            (listing / "a").write_bytes(b"")
            state = initial_dir / "state"
            state.write_bytes(b"captured\n")
            graph = MODULE.assemble(
                [
                    action(
                        "same",
                        1,
                        [output],
                        [
                            {
                                "path": str(listing),
                                "kind": "directory_listing",
                            }
                        ],
                    )
                ],
                [output],
                [initial_dir, state],
                [listing],
            )
            self.assertEqual(
                graph["initial_inputs"], [str(initial_dir), str(state)]
            )
            self.assertEqual(
                graph["empty_output_directory_listings"],
                [
                    {
                        "path": str(listing),
                        "members": [list(b"a"), list(b"z")],
                    }
                ],
            )

    def test_rejects_owned_initial_and_unobserved_listing(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "out"
            output.write_bytes(b"")
            listing = root / "listing"
            listing.mkdir()
            with self.assertRaisesRegex(ValueError, "observed producer"):
                MODULE.assemble(
                    [action("same", 1, [output])],
                    [output],
                    [output],
                )
            with self.assertRaisesRegex(ValueError, "not observed"):
                MODULE.assemble(
                    [action("same", 1, [output])],
                    [output],
                    empty_output_directories=[listing],
                )

    def test_preserves_lexical_symlink_identity(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target = root / "target"
            output = root / "output-link"
            alias = root / "initial-link"
            target.write_bytes(b"captured\n")
            output.symlink_to(target)
            alias.symlink_to(output)
            graph = MODULE.assemble(
                [action("same", 1, [output])],
                [output],
                [alias],
            )
            self.assertEqual(graph["requested_outputs"], [str(output)])
            self.assertEqual(graph["initial_inputs"], [str(alias)])

    def test_augments_one_legacy_graph_and_rejects_replacement(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / "out"
            state = root / "state"
            state.write_bytes(b"captured\n")
            legacy = {
                "schema": MODULE.LEGACY_GRAPH_SCHEMA,
                "actions": [action("same", 1, [output])],
                "requested_outputs": [str(output)],
            }
            appended_output = root / "appended"
            upgraded = MODULE.augment(
                legacy,
                [state],
                [],
                [action("same", 1, [appended_output])],
            )
            self.assertEqual(upgraded["schema"], MODULE.GRAPH_SCHEMA)
            self.assertEqual(len(upgraded["actions"]), 2)
            self.assertEqual(upgraded["initial_inputs"], [str(state)])
            with self.assertRaisesRegex(ValueError, "already carries"):
                MODULE.augment(upgraded, [], [])


if __name__ == "__main__":
    unittest.main()
