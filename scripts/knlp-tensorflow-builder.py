#!/usr/bin/env python3
# Copyright (c) 2026 Luis Chamberlain
# SPDX-License-Identifier: MIT
"""Build a KNLP TensorFlow request with Rush."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shlex
import shutil
import subprocess
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

SUPPORTED_NOOP_COPTS = {
    "-Wno-unused-command-line-argument",
}


def utc_now() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(8 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def write_json(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")
    temporary.replace(path)


def remove_managed_path(path: Path) -> None:
    if path.is_symlink() or path.is_file():
        path.unlink()
    elif path.is_dir():
        shutil.rmtree(path)


def translate_build_arguments(arguments: str) -> tuple[str, bool, list[str]]:
    tokens = shlex.split(arguments)
    compilation_mode: str | None = None
    cuda_enabled = False
    ignored: list[str] = []
    index = 0

    while index < len(tokens):
        token = tokens[index]
        if token in {"--config", "--compilation_mode", "-c"}:
            index += 1
            if index == len(tokens):
                raise ValueError(f"{token} requires a value")
            value = tokens[index]
            option = token
        elif token.startswith("--config="):
            value = token.split("=", 1)[1]
            option = "--config"
        elif token.startswith("--compilation_mode="):
            value = token.split("=", 1)[1]
            option = "--compilation_mode"
        elif token.startswith("--copt="):
            value = token.split("=", 1)[1]
            if value not in SUPPORTED_NOOP_COPTS:
                raise ValueError(f"unsupported Bazel compiler option: {value}")
            ignored.append(token)
            index += 1
            continue
        elif token == "--repo_env=TF_NEED_TENSORRT=0":
            ignored.append(token)
            index += 1
            continue
        else:
            raise ValueError(f"unsupported Bazel build argument: {token}")

        if option == "--config" and value == "cuda":
            cuda_enabled = True
        elif value in {"fastbuild", "opt", "dbg"}:
            if compilation_mode and compilation_mode != value:
                raise ValueError(
                    "conflicting compilation modes: " f"{compilation_mode} and {value}"
                )
            compilation_mode = value
        else:
            raise ValueError(f"unsupported {option} value: {value}")
        index += 1

    return compilation_mode or "fastbuild", cuda_enabled, ignored


def normalize_tf_cuda_arch(value: str) -> str:
    capabilities = [item.strip() for item in value.split(",") if item.strip()]
    if len(capabilities) != 1:
        raise ValueError(
            "TF_CUDA_COMPUTE_CAPABILITIES must select exactly one architecture"
        )
    capability = capabilities[0]
    for prefix in ("sm_", "compute_"):
        if capability.startswith(prefix):
            capability = capability[len(prefix) :]
            break
    if re.fullmatch(r"[0-9]+\.[0-9]+", capability):
        capability = capability.replace(".", "")
    if not re.fullmatch(r"[0-9]{2,3}", capability):
        raise ValueError(
            "invalid TF_CUDA_COMPUTE_CAPABILITIES value: "
            f"{capabilities[0]}"
        )
    return f"sm_{capability}"


def detect_cuda_arch(environment: dict[str, str]) -> str:
    configured = environment.get("RUSH_CUDA_ARCH")
    if configured:
        if not re.fullmatch(r"sm_[0-9]+", configured):
            raise ValueError(f"invalid RUSH_CUDA_ARCH: {configured}")

    tf_capabilities = environment.get("TF_CUDA_COMPUTE_CAPABILITIES")
    if tf_capabilities:
        requested = normalize_tf_cuda_arch(tf_capabilities)
        if configured and requested != configured:
            raise ValueError(
                "RUSH_CUDA_ARCH conflicts with "
                "TF_CUDA_COMPUTE_CAPABILITIES"
            )
        return requested

    if configured:
        return configured

    try:
        result = subprocess.run(
            [
                "nvidia-smi",
                "--query-gpu=compute_cap",
                "--format=csv,noheader",
            ],
            env=environment,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
    except OSError as exception:
        raise RuntimeError(
            "CUDA was requested but RUSH_CUDA_ARCH is unset and "
            "nvidia-smi is unavailable"
        ) from exception

    capabilities = {line.strip() for line in result.stdout.splitlines() if line.strip()}
    if result.returncode or not capabilities:
        detail = result.stderr.strip() or "no compute capability reported"
        raise RuntimeError(f"cannot detect CUDA compute capability: {detail}")
    if len(capabilities) != 1:
        joined = ", ".join(sorted(capabilities))
        raise RuntimeError(f"mixed CUDA compute capabilities are unsupported: {joined}")

    capability = capabilities.pop()
    match = re.fullmatch(r"([0-9]+)\.([0-9]+)", capability)
    if not match:
        raise RuntimeError(f"unexpected CUDA compute capability: {capability}")
    return f"sm_{match.group(1)}{match.group(2)}"


def request_environment(request: dict[str, Any]) -> dict[str, str]:
    requested = request.get("environment", {})
    if not isinstance(requested, dict):
        raise ValueError("request environment must be an object")
    invalid = [
        key
        for key, value in requested.items()
        if not isinstance(key, str) or not isinstance(value, str)
    ]
    if invalid:
        raise ValueError("request environment keys and values must be strings")
    environment = os.environ.copy()
    environment.update(requested)
    return environment


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--request", type=Path, required=True)
    parser.add_argument("--result", type=Path, required=True)
    args = parser.parse_args()

    request = json.loads(args.request.read_text())
    build = request["build"]
    source = Path(request["source"]["directory"]).resolve()
    output = Path(request["output_directory"]).resolve()
    output.mkdir(parents=True, exist_ok=True)

    output_root = output / "rush-out"
    bin_link = output / "rush-bin"
    workspace_cache = output / "workspace-cache"
    action_cache = output / "action-cache"
    profile = output / "build.profile.json"
    actions = output / "actions.jsonl"
    log_path = output / "build.log"
    for path in (output_root, bin_link, workspace_cache, action_cache):
        remove_managed_path(path)

    started_utc = utc_now()
    started = time.monotonic()
    status = 1
    error = None
    artifacts: list[dict[str, object]] = []
    version = None
    binary_directory = None
    command: list[str] = []
    ignored_arguments: list[str] = []
    cuda_arch = None

    try:
        if request["schema_version"] != 1:
            raise ValueError(f"unsupported request schema: {request['schema_version']}")
        if not source.is_dir():
            raise ValueError(f"TensorFlow source directory is missing: {source}")

        compilation_mode, cuda_enabled, ignored_arguments = translate_build_arguments(
            build["arguments"]
        )
        environment = request_environment(request)
        environment.update(
            {
                "RUSH_WORKSPACE_OUTPUT_ROOT": str(output_root),
                "RUSH_WORKSPACE_BIN_LINK": str(bin_link),
                "RUSH_WORKSPACE_CACHE_ROOT": str(workspace_cache),
                "RUSH_ACTION_TIMING": str(actions),
                "RUSH_BUILD_JOBS": str(build["jobs"]),
            }
        )
        environment.setdefault("RUSH_ACTION_AWARE_TARGET_WAVEFRONT", "1")
        environment.setdefault("RUSH_CRITICAL_PATH_TARGET_FRONTIER", "1")
        if cuda_enabled:
            environment["RUSH_ENABLE_CUDA"] = "1"
            cuda_arch = detect_cuda_arch(environment)
            environment["RUSH_CUDA_ARCH"] = cuda_arch

        rush = environment.get("RUSH_BIN", "rush")
        command = [
            rush,
            "build",
            "--daemon=off",
            f"--jobs={build['jobs']}",
            "-c",
            compilation_mode,
            "--use-scheduler",
            f"--disk-cache-dir={action_cache}",
        ]
        if build["capture_profile"]:
            command.append(f"--profile={profile}")
        command.append(build["target"])

        version_result = subprocess.run(
            [rush, "--version"],
            cwd=source,
            env=environment,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )
        version = version_result.stdout.strip()
        with log_path.open("w") as log:
            print(f"$ {shlex.join(command)}", file=log, flush=True)
            status = subprocess.run(
                command,
                cwd=source,
                env=environment,
                stdout=log,
                stderr=subprocess.STDOUT,
                check=False,
            ).returncode

        if status == 0:
            artifact_relpath = Path(build["artifact_relpath"])
            artifact_candidates = [
                bin_link / artifact_relpath,
                output_root / artifact_relpath,
                output_root / artifact_relpath.name,
            ]
            artifact = next(
                (candidate for candidate in artifact_candidates if candidate.is_file()),
                None,
            )
            if artifact is None:
                status = 1
                error = "expected artifact is missing; checked: " + ", ".join(
                    str(candidate) for candidate in artifact_candidates
                )
            else:
                binary_directory = artifact.parent.resolve()
                artifacts.append(
                    {
                        "path": str(artifact.resolve()),
                        "sha256": sha256_file(artifact),
                        "size_bytes": artifact.stat().st_size,
                    }
                )
    except (OSError, RuntimeError, TypeError, ValueError) as exception:
        error = str(exception)
        with log_path.open("a") as log:
            print(error, file=log)

    result = {
        "schema_version": request["schema_version"],
        "variant": request["variant"],
        "common_request_sha256": request["common_request_sha256"],
        "status": status,
        "started_utc": started_utc,
        "finished_utc": utc_now(),
        "wall_seconds": time.monotonic() - started,
        "engine": {
            "name": "rush",
            "version": version,
            "cuda_arch": cuda_arch,
            "ignored_compatibility_arguments": ignored_arguments,
        },
        "command": command,
        "log": str(log_path),
        "profile": (
            str(profile) if build["capture_profile"] and profile.is_file() else None
        ),
        "binary_directory": (
            str(binary_directory) if binary_directory is not None else None
        ),
        "artifacts": artifacts,
        "error": error,
    }
    write_json(args.result, result)
    print(json.dumps(result, indent=2, sort_keys=True))
    raise SystemExit(status)


if __name__ == "__main__":
    main()
