#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Exercise a real REAPI backend without assuming shared client/worker storage."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rush", type=Path, required=True)
    parser.add_argument("--endpoint", required=True)
    parser.add_argument("--toolchain", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    binary = args.rush.resolve()
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    workspace = root / "workspace"
    workspace.mkdir()
    nonce = uuid.uuid4().hex
    spec = {
        "arguments": ["/bin/sh", "-c", "g++ main.cc -o app && ./app > result"],
        "environment": {"PATH": "/usr/bin:/bin"}, "inputs": ["main.cc"],
        "outputs": ["app", "result"],
        "platform": {"toolchain": args.toolchain, "OSFamily": "linux"},
        "timeout_seconds": 10,
    }
    results = []

    def invoke(name, action, expect_success=True, expected_exit=None, deadline=30):
        manifest = root / (name + ".json")
        report = root / (name + "-report.json")
        manifest.write_text(json.dumps(action))
        command = [str(binary), "remote-execute", str(manifest),
                   "--execution-root", str(workspace), "--endpoint", args.endpoint,
                   "--report", str(report), "--deadline-seconds", str(deadline)]
        with (root / (name + ".log")).open("wb") as log:
            completed = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT,
                                       env={"PATH": os.environ["PATH"], "RUST_LOG": "error"},
                                       timeout=deadline + 20)
        assert (completed.returncode == 0) == expect_success, name
        if expected_exit is not None:
            assert completed.returncode == expected_exit, name
        outcome = json.loads(report.read_text()) if report.exists() else None
        results.append({"case": name, "passed": True, "exit_code": completed.returncode,
                        "cached": outcome["cached"] if outcome else None})
        (root / "summary.json").write_text(json.dumps({"complete": False, "cases": results}, indent=2) + "\n")
        return outcome

    def source(value):
        (workspace / "main.cc").write_text(
            f'// unique input {nonce}\n#include <cstdio>\n'
            f'int main() {{ std::printf("value=%d\\n", {value}); }}\n')

    source(41)
    first = invoke("execute", spec)
    assert not first["cached"]
    assert (workspace / "result").read_text() == "value=41\n"
    assert subprocess.check_output([str(workspace / "app")]) == b"value=41\n"
    original = hashlib.sha256((workspace / "app").read_bytes()).hexdigest()
    (workspace / "app").unlink()
    (workspace / "result").unlink()
    cached = invoke("restore", spec)
    assert cached["cached"] and cached["action_digest"] == first["action_digest"]
    assert hashlib.sha256((workspace / "app").read_bytes()).hexdigest() == original
    source(43)
    changed = invoke("source-edit", spec)
    assert not changed["cached"] and changed["action_digest"] != first["action_digest"]
    assert (workspace / "result").read_text() == "value=43\n"
    source(41)
    assert invoke("revert", spec)["cached"]
    assert (workspace / "result").read_text() == "value=41\n"
    uncached = dict(spec, no_cache=True)
    assert not invoke("no-cache-first", uncached)["cached"]
    assert not invoke("no-cache-second", uncached)["cached"]

    payload = (nonce.encode() * 131072)  # 4 MiB, larger than the batch-upload path.
    (workspace / "payload").write_bytes(payload)
    transfer = dict(spec, arguments=["/bin/cp", "payload", "copied"],
                    inputs=["payload"], outputs=["copied"])
    invoke("streamed-transfer", transfer)
    assert (workspace / "copied").read_bytes() == payload

    failure = dict(spec, arguments=["/bin/sh", "-c", "echo expected-failure >&2; exit 7"],
                   inputs=[], outputs=[])
    for name in ("failure-first", "failure-repeat"):
        outcome = invoke(name, failure, False, 7)
        assert not outcome["cached"]
    missing = dict(spec, arguments=["/bin/true"], inputs=[], outputs=["missing"])
    invoke("missing-output", missing, False)
    assert not (workspace / "missing").exists()
    (workspace / "guard").write_text("preserve")
    symlink = dict(spec, arguments=["/bin/ln", "-s", "/etc/passwd", "guard"],
                   inputs=[], outputs=["guard"], no_cache=True)
    invoke("reject-output-symlink", symlink, False)
    assert (workspace / "guard").read_text() == "preserve"
    invoke("reject-relative-output-symlink", dict(symlink,
        arguments=["/bin/ln", "-s", "main.cc", "guard"], inputs=["main.cc"]), False)
    assert (workspace / "guard").read_text() == "preserve"
    traversal = dict(spec, outputs=["../escape"])
    invoke("reject-traversal", traversal, False)
    assert not (root / "escape").exists()
    (workspace / "input-link").symlink_to("main.cc")
    invoke("reject-input-symlink", dict(spec, inputs=["input-link"]), False)
    timeout = dict(spec, arguments=["/bin/sleep", "10"], inputs=[], outputs=[],
                   timeout_seconds=1)
    invoke("worker-timeout", timeout, False, deadline=10)
    # The action itself remains bounded if a disconnected client cannot cancel it.
    invoke("client-deadline", dict(timeout, timeout_seconds=2), False, deadline=1)
    (root / "summary.json").write_text(json.dumps({"complete": True, "cases": results}, indent=2) + "\n")
    print(f"PASS: {len(results)} independent-backend cases")


if __name__ == "__main__":
    main()
