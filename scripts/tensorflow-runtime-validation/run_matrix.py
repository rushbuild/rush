#!/usr/bin/env python3
"""Execute a generated validation matrix with one TensorFlow runner."""

import argparse
import hashlib
import json
import os
import subprocess
import time
from pathlib import Path


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def load_cases(artifacts: Path, mode: str) -> list[dict]:
    cases = []
    if mode in ("all", "inference"):
        cases.extend(
            json.loads((artifacts / "inference_manifest.json").read_text())["cases"]
        )
    if mode in ("all", "training"):
        cases.extend(
            json.loads((artifacts / "training_manifest.json").read_text())["cases"]
        )
    return cases


def feed_argument(artifacts: Path, feed: dict) -> str:
    shape = "x".join(str(dimension) for dimension in feed["shape"]) or "scalar"
    path = (artifacts / feed["path"]).resolve()
    return f"--feed={feed['name']},{feed['dtype']},{shape},{path}"


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--runner", required=True, type=Path)
    parser.add_argument("--artifacts", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument(
        "--mode", choices=("all", "inference", "training"), default="all"
    )
    parser.add_argument("--inference-steps", type=int, default=30)
    parser.add_argument("--inference-warmup", type=int, default=10)
    parser.add_argument("--training-warmup", type=int, default=3)
    parser.add_argument("--require-gpu", action="store_true")
    parser.add_argument("--intra-op", type=int, default=0)
    parser.add_argument("--inter-op", type=int, default=0)
    args = parser.parse_args()

    runner = args.runner.resolve()
    artifacts = args.artifacts.resolve()
    output = args.out.resolve()
    if output.exists():
        raise SystemExit(f"refusing to overwrite existing output: {output}")
    output.mkdir(parents=True)

    cases = load_cases(artifacts, args.mode)
    environment = os.environ.copy()
    environment.setdefault("CUDA_VISIBLE_DEVICES", "0")
    environment.setdefault("TF_CPP_MIN_LOG_LEVEL", "1")
    environment.setdefault("TF_CUDNN_DETERMINISTIC", "1")
    environment.setdefault("TF_DETERMINISTIC_OPS", "1")
    environment.setdefault("TF_ENABLE_ONEDNN_OPTS", "0")
    started = time.time()
    run_manifest = {
        "schema_version": 1,
        "runner": str(runner),
        "runner_sha256": sha256(runner),
        "artifacts": str(artifacts),
        "mode": args.mode,
        "require_gpu": args.require_gpu,
        "started_unix": started,
        "environment": {
            key: environment[key]
            for key in [
                "CUDA_VISIBLE_DEVICES",
                "TF_CPP_MIN_LOG_LEVEL",
                "TF_CUDNN_DETERMINISTIC",
                "TF_DETERMINISTIC_OPS",
                "TF_ENABLE_ONEDNN_OPTS",
            ]
        },
        "cases": [],
    }
    command_log = (output / "commands.jsonl").open("w")

    for case in cases:
        case_output = output / case["id"]
        case_output.mkdir()
        (case_output / "case.json").write_text(
            json.dumps(case, indent=2, sort_keys=True) + "\n"
        )
        steps = (
            args.inference_steps
            if case["kind"] == "inference"
            else int(case["steps"])
        )
        warmup = (
            args.inference_warmup
            if case["kind"] == "inference"
            else args.training_warmup
        )
        command = [
            str(runner),
            f"--graph={(artifacts / case['graph']).resolve()}",
            f"--output-dir={case_output}",
            f"--steps={steps}",
            f"--warmup={warmup}",
        ]
        if args.require_gpu:
            command.append("--require-gpu")
        if args.intra_op > 0:
            command.append(f"--intra-op={args.intra_op}")
        if args.inter_op > 0:
            command.append(f"--inter-op={args.inter_op}")
        command.extend(feed_argument(artifacts, feed) for feed in case["feeds"])
        command.extend(
            f"--series-fetch={name}" for name in case.get("series_fetches", [])
        )
        command.extend(
            f"--final-fetch={name}" for name in case.get("final_fetches", [])
        )
        command.extend(f"--target={name}" for name in case.get("targets", []))
        command.extend(
            f"--init-target={name}" for name in case.get("init_targets", [])
        )
        command_log.write(
            json.dumps({"case": case["id"], "command": command}) + "\n"
        )
        command_log.flush()

        case_started = time.time()
        with (case_output / "runner.log").open("w") as log:
            completed = subprocess.run(
                command,
                env=environment,
                stdout=log,
                stderr=subprocess.STDOUT,
                check=False,
            )
        run_manifest["cases"].append(
            {
                "id": case["id"],
                "kind": case["kind"],
                "returncode": completed.returncode,
                "elapsed_seconds": time.time() - case_started,
            }
        )
        (output / "manifest.json").write_text(
            json.dumps(run_manifest, indent=2, sort_keys=True) + "\n"
        )
        if completed.returncode != 0:
            raise SystemExit(
                f"{case['id']} failed with rc={completed.returncode}; "
                f"see {case_output / 'runner.log'}"
            )

    command_log.close()
    run_manifest["finished_unix"] = time.time()
    run_manifest["elapsed_seconds"] = time.time() - started
    (output / "manifest.json").write_text(
        json.dumps(run_manifest, indent=2, sort_keys=True) + "\n"
    )


if __name__ == "__main__":
    main()
