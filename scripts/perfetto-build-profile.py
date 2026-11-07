#!/usr/bin/env python3
"""Normalize and summarize Rush/Bazel Chrome trace profiles for Perfetto."""

from __future__ import annotations

import argparse
import gzip
import heapq
import json
import math
import statistics
from pathlib import Path
from typing import Any, Iterable


def open_text(path: Path, mode: str):
    if path.suffix == ".gz":
        return gzip.open(path, mode + "t", encoding="utf-8")
    return path.open(mode, encoding="utf-8")


def load_json(path: Path) -> Any:
    with open_text(path, "r") as stream:
        return json.load(stream)


def write_json(path: Path, value: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with open_text(path, "w") as stream:
        json.dump(value, stream, indent=2, sort_keys=True)
        stream.write("\n")


def percentile(values: Iterable[float], quantile: float) -> float | None:
    ordered = sorted(values)
    if not ordered:
        return None
    if len(ordered) == 1:
        return ordered[0]
    position = (len(ordered) - 1) * quantile
    lower = math.floor(position)
    upper = math.ceil(position)
    if lower == upper:
        return ordered[lower]
    fraction = position - lower
    return ordered[lower] * (1.0 - fraction) + ordered[upper] * fraction


def stats(values: Iterable[float]) -> dict[str, float | int | None]:
    samples = list(values)
    if not samples:
        return {
            "count": 0,
            "min": None,
            "median": None,
            "mad": None,
            "mean": None,
            "stdev": None,
            "max": None,
        }
    median = statistics.median(samples)
    return {
        "count": len(samples),
        "min": min(samples),
        "median": median,
        "mad": statistics.median(abs(sample - median) for sample in samples),
        "mean": statistics.mean(samples),
        "stdev": statistics.pstdev(samples),
        "max": max(samples),
    }


def metadata_event(pid: int, tid: int, name: str) -> dict[str, Any]:
    return {
        "name": "thread_name",
        "cat": "__metadata",
        "ph": "M",
        "ts": 0,
        "pid": pid,
        "tid": tid,
        "args": {"name": name},
    }


def complete_event(
    *,
    name: str,
    category: str,
    pid: int,
    tid: int,
    ts_us: float,
    dur_us: float,
    args: dict[str, Any] | None = None,
) -> dict[str, Any]:
    event: dict[str, Any] = {
        "name": name,
        "cat": category,
        "ph": "X",
        "ts": max(0.0, ts_us),
        "dur": max(0.0, dur_us),
        "pid": pid,
        "tid": tid,
    }
    if args:
        event["args"] = args
    return event


def relative_us(unix_ns: int, started_unix_ns: int) -> float:
    return max(0, unix_ns - started_unix_ns) / 1000.0


def action_name(record: dict[str, Any]) -> str:
    mnemonic = record.get("mnemonic") or "Action"
    owner = record.get("owner")
    output = record.get("primary_output")
    detail = owner or output
    return f"{mnemonic}: {detail}" if detail else str(mnemonic)


def allocate_lanes(records: list[dict[str, Any]]) -> int:
    active: list[tuple[int, int]] = []
    available: list[int] = []
    lane_count = 0
    for record in sorted(records, key=lambda item: item["started_unix_ns"]):
        started = int(record["started_unix_ns"])
        while active and active[0][0] <= started:
            _, lane = heapq.heappop(active)
            heapq.heappush(available, lane)
        if available:
            lane = heapq.heappop(available)
        else:
            lane = lane_count
            lane_count += 1
        record["_lane"] = lane
        heapq.heappush(active, (int(record["finished_unix_ns"]), lane))
    return lane_count


def counter_events(
    intervals: Iterable[tuple[int, int]],
    *,
    started_unix_ns: int,
    pid: int,
    tid: int,
    name: str,
) -> list[dict[str, Any]]:
    changes: dict[int, int] = {}
    for started, finished in intervals:
        changes[started] = changes.get(started, 0) + 1
        changes[finished] = changes.get(finished, 0) - 1
    active = 0
    events = []
    for timestamp in sorted(changes):
        active += changes[timestamp]
        events.append(
            {
                "name": name,
                "cat": "rush concurrency",
                "ph": "C",
                "ts": relative_us(timestamp, started_unix_ns),
                "pid": pid,
                "tid": tid,
                "args": {"count": active},
            }
        )
    return events


def load_action_records(path: Path) -> list[dict[str, Any]]:
    records = []
    with path.open(encoding="utf-8") as stream:
        for line_number, line in enumerate(stream, 1):
            if not line.strip():
                continue
            try:
                record = json.loads(line)
            except json.JSONDecodeError as error:
                raise ValueError(f"{path}:{line_number}: {error}") from error
            if record.get("event") != "action_timing":
                continue
            records.append(record)
    return records


def merge_rush(args: argparse.Namespace) -> None:
    trace_path = Path(args.trace)
    actions_path = Path(args.actions)
    output_path = Path(args.output)
    trace = load_json(trace_path)
    if not isinstance(trace, dict) or not isinstance(trace.get("traceEvents"), list):
        raise ValueError(f"{trace_path} is not a Chrome trace object")

    other_data = trace.setdefault("otherData", {})
    try:
        started_unix_ns = int(other_data["started_unix_ns"])
    except (KeyError, TypeError, ValueError) as error:
        raise ValueError(
            "Rush trace is missing otherData.started_unix_ns; "
            "rebuild Rush with the Perfetto clock metadata change"
        ) from error

    records = load_action_records(actions_path)
    lane_count = allocate_lanes(records)
    existing_events = trace["traceEvents"]
    pid = next(
        (
            int(event["pid"])
            for event in existing_events
            if isinstance(event, dict) and "pid" in event
        ),
        1,
    )
    existing_tids = [
        int(event["tid"])
        for event in existing_events
        if isinstance(event, dict) and isinstance(event.get("tid"), (int, float))
    ]
    lane_base = max(existing_tids, default=0) + 100
    counter_base = lane_base + lane_count + 100

    added_events: list[dict[str, Any]] = []
    for lane in range(lane_count):
        added_events.append(metadata_event(pid, lane_base + lane, f"Action lane {lane}"))
    added_events.append(metadata_event(pid, counter_base, "Rush action concurrency"))
    added_events.append(
        metadata_event(pid, counter_base + 1, "Rush execution concurrency")
    )

    wall_intervals = []
    execution_intervals = []
    for record in records:
        started = int(record["started_unix_ns"])
        finished = int(record["finished_unix_ns"])
        lane_tid = lane_base + int(record["_lane"])
        wall_intervals.append((started, finished))
        event_args = {
            key: record.get(key)
            for key in (
                "owner",
                "mnemonic",
                "action_id",
                "primary_output",
                "executable",
                "argv_len",
                "argv_bytes",
                "exit_code",
                "cached",
                "wall_ms",
                "execution_ms",
                "non_execution_ms",
            )
        }
        added_events.append(
            complete_event(
                name=action_name(record),
                category="rush action",
                pid=pid,
                tid=lane_tid,
                ts_us=relative_us(started, started_unix_ns),
                dur_us=(finished - started) / 1000.0,
                args=event_args,
            )
        )

        execution_started = record.get("execution_started_unix_ns")
        execution_finished = record.get("execution_finished_unix_ns")
        if execution_started is None or execution_finished is None:
            continue
        execution_started = int(execution_started)
        execution_finished = int(execution_finished)
        execution_intervals.append((execution_started, execution_finished))
        if execution_started > started:
            added_events.append(
                complete_event(
                    name="scheduler/non-execution",
                    category="rush action queue",
                    pid=pid,
                    tid=lane_tid,
                    ts_us=relative_us(started, started_unix_ns),
                    dur_us=(execution_started - started) / 1000.0,
                )
            )
        added_events.append(
            complete_event(
                name=f"{record.get('mnemonic', 'Action')}: execute",
                category="rush action execution",
                pid=pid,
                tid=lane_tid,
                ts_us=relative_us(execution_started, started_unix_ns),
                dur_us=(execution_finished - execution_started) / 1000.0,
            )
        )
        if finished > execution_finished:
            added_events.append(
                complete_event(
                    name="scheduler/post-execution",
                    category="rush action queue",
                    pid=pid,
                    tid=lane_tid,
                    ts_us=relative_us(execution_finished, started_unix_ns),
                    dur_us=(finished - execution_finished) / 1000.0,
                )
            )

    added_events.extend(
        counter_events(
            wall_intervals,
            started_unix_ns=started_unix_ns,
            pid=pid,
            tid=counter_base,
            name="active_actions",
        )
    )
    added_events.extend(
        counter_events(
            execution_intervals,
            started_unix_ns=started_unix_ns,
            pid=pid,
            tid=counter_base + 1,
            name="executing_actions",
        )
    )

    metadata = [
        event
        for event in existing_events + added_events
        if isinstance(event, dict) and event.get("ph") == "M"
    ]
    timed = [
        event
        for event in existing_events + added_events
        if not (isinstance(event, dict) and event.get("ph") == "M")
    ]
    timed.sort(
        key=lambda event: (
            float(event.get("ts", 0)),
            -float(event.get("dur", 0)),
            str(event.get("name", "")),
        )
    )
    trace["traceEvents"] = metadata + timed
    other_data["perfetto_action_timing"] = {
        "schema_version": 1,
        "action_count": len(records),
        "action_lane_count": lane_count,
    }
    write_json(output_path, trace)

    wall_values = [float(record.get("wall_ms") or 0.0) for record in records]
    execution_values = [
        float(record["execution_ms"])
        for record in records
        if record.get("execution_ms") is not None
    ]
    largest_wall = max(records, key=lambda record: record.get("wall_ms") or 0.0, default=None)
    largest_execution = max(
        (record for record in records if record.get("execution_ms") is not None),
        key=lambda record: record["execution_ms"],
        default=None,
    )
    action_summary = {
        "schema_version": 1,
        "action_count": len(records),
        "cached_action_count": sum(bool(record.get("cached")) for record in records),
        "action_lane_count": lane_count,
        "wall_ms": {
            **stats(wall_values),
            "p95": percentile(wall_values, 0.95),
            "p99": percentile(wall_values, 0.99),
            "total": sum(wall_values),
        },
        "execution_ms": {
            **stats(execution_values),
            "p95": percentile(execution_values, 0.95),
            "p99": percentile(execution_values, 0.99),
            "total": sum(execution_values),
        },
        "largest_wall_action": largest_wall,
        "largest_execution_action": largest_execution,
    }
    if args.summary:
        write_json(Path(args.summary), action_summary)


def is_action_event(event: dict[str, Any]) -> bool:
    category = str(event.get("cat", "")).lower()
    args = event.get("args")
    has_mnemonic = isinstance(args, dict) and bool(args.get("mnemonic"))
    return category == "rush action" or "action processing" in category or (
        has_mnemonic and "action" in category
    )


def event_description(event: dict[str, Any] | None) -> dict[str, Any] | None:
    if event is None:
        return None
    return {
        "name": event.get("name"),
        "category": event.get("cat"),
        "duration_ms": float(event.get("dur", 0)) / 1000.0,
        "timestamp_ms": float(event.get("ts", 0)) / 1000.0,
        "args": event.get("args"),
    }


def summarize_trace(args: argparse.Namespace) -> None:
    trace_path = Path(args.trace)
    trace = load_json(trace_path)
    events = trace.get("traceEvents", []) if isinstance(trace, dict) else trace
    complete = [
        event
        for event in events
        if isinstance(event, dict)
        and event.get("ph") == "X"
        and isinstance(event.get("dur"), (int, float))
    ]
    categories: dict[str, dict[str, float | int]] = {}
    for event in complete:
        category = str(event.get("cat", ""))
        entry = categories.setdefault(
            category, {"count": 0, "total_duration_ms": 0.0, "max_duration_ms": 0.0}
        )
        duration_ms = float(event["dur"]) / 1000.0
        entry["count"] = int(entry["count"]) + 1
        entry["total_duration_ms"] = float(entry["total_duration_ms"]) + duration_ms
        entry["max_duration_ms"] = max(float(entry["max_duration_ms"]), duration_ms)

    largest_event = max(complete, key=lambda event: float(event["dur"]), default=None)
    action_events = [event for event in complete if is_action_event(event)]
    largest_action = max(
        action_events, key=lambda event: float(event["dur"]), default=None
    )
    if complete:
        start_us = min(float(event.get("ts", 0)) for event in complete)
        finish_us = max(
            float(event.get("ts", 0)) + float(event.get("dur", 0))
            for event in complete
        )
    else:
        start_us = finish_us = 0.0
    summary = {
        "schema_version": 1,
        "trace": str(trace_path),
        "complete_event_count": len(complete),
        "action_event_count": len(action_events),
        "trace_span_ms": (finish_us - start_us) / 1000.0,
        "largest_event": event_description(largest_event),
        "largest_action": event_description(largest_action),
        "categories": dict(sorted(categories.items())),
    }
    write_json(Path(args.output), summary)


def format_number(value: float | int | None, digits: int = 3) -> str:
    if value is None:
        return "n/a"
    return f"{float(value):.{digits}f}"


def summarize_suite(args: argparse.Namespace) -> None:
    root = Path(args.root)
    results = []
    for path in sorted(root.glob("run-*/result.json")):
        result = load_json(path)
        result["_path"] = str(path.relative_to(root))
        trace_summary_path = result.get("trace_summary")
        if trace_summary_path:
            full_path = root / trace_summary_path
            if full_path.exists():
                result["_trace_summary"] = load_json(full_path)
        action_summary_path = result.get("action_summary")
        if action_summary_path:
            full_path = root / action_summary_path
            if full_path.exists():
                result["_action_summary"] = load_json(full_path)
        results.append(result)

    measured = [
        result
        for result in results
        if result.get("measured", True) and int(result.get("exit_code", 1)) == 0
    ]
    tools: dict[str, dict[str, Any]] = {}
    for tool in sorted({str(result["tool"]) for result in measured}):
        tool_results = [result for result in measured if result["tool"] == tool]
        walls = [float(result["wall_seconds"]) for result in tool_results]
        if tool == "rush":
            largest_actions = [
                float(
                    result["_action_summary"]["largest_execution_action"][
                        "execution_ms"
                    ]
                )
                for result in tool_results
                if result.get("_action_summary", {}).get("largest_execution_action")
            ]
            largest_action_walls = [
                float(result["_action_summary"]["largest_wall_action"]["wall_ms"])
                for result in tool_results
                if result.get("_action_summary", {}).get("largest_wall_action")
            ]
        else:
            largest_actions = [
                float(result["_trace_summary"]["largest_action"]["duration_ms"])
                for result in tool_results
                if result.get("_trace_summary", {}).get("largest_action")
            ]
            largest_action_walls = list(largest_actions)
        tools[tool] = {
            "wall_seconds": stats(walls),
            "largest_action_ms": stats(largest_actions),
            "largest_action_wall_ms": stats(largest_action_walls),
            "successful_runs": len(tool_results),
        }

    speedup = None
    if "bazel" in tools and "rush" in tools:
        rush_median = tools["rush"]["wall_seconds"]["median"]
        bazel_median = tools["bazel"]["wall_seconds"]["median"]
        if rush_median:
            speedup = bazel_median / rush_median

    summary = {
        "schema_version": 1,
        "target": results[0].get("target") if results else None,
        "run_count": len(results),
        "successful_measured_run_count": len(measured),
        "tools": tools,
        "bazel_over_rush_speedup": speedup,
    }
    write_json(root / "summary.json", summary)

    with (root / "runs.tsv").open("w", encoding="utf-8") as stream:
        stream.write(
            "run\ttool\trepetition\texit_code\twall_seconds\tuser_seconds\t"
            "system_seconds\tmax_rss_kb\tcpu_percent\n"
        )
        for result in results:
            stream.write(
                "\t".join(
                    str(result.get(key, ""))
                    for key in (
                        "run",
                        "tool",
                        "repetition",
                        "exit_code",
                        "wall_seconds",
                        "user_seconds",
                        "system_seconds",
                        "max_rss_kb",
                        "cpu_percent",
                    )
                )
                + "\n"
            )

    lines = [
        "# TensorFlow clean-build comparison",
        "",
        f"Target: `{summary['target']}`",
        "",
        "| Tool | Runs | Median wall | MAD | Min | Max | "
        "Largest action | Largest action wall |",
        "|---|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for tool, tool_summary in tools.items():
        wall = tool_summary["wall_seconds"]
        action = tool_summary["largest_action_ms"]
        action_wall = tool_summary["largest_action_wall_ms"]
        lines.append(
            f"| {tool} | {tool_summary['successful_runs']} | "
            f"{format_number(wall['median'])} s | "
            f"{format_number(wall['mad'])} s | "
            f"{format_number(wall['min'])} s | "
            f"{format_number(wall['max'])} s | "
            f"{format_number(action['median'])} ms | "
            f"{format_number(action_wall['median'])} ms |"
        )
    lines.extend(
        [
            "",
            (
                f"Bazel/Rush median speedup: **{speedup:.3f}x**"
                if speedup is not None
                else "Bazel/Rush median speedup: n/a"
            ),
            "",
            "Open each `trace.perfetto.json` or Bazel `trace.raw.json` in "
            "https://ui.perfetto.dev/.",
            "",
        ]
    )
    (root / "summary.md").write_text("\n".join(lines), encoding="utf-8")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest="command", required=True)

    merge = subparsers.add_parser(
        "merge-rush", help="merge Rush phase and action traces"
    )
    merge.add_argument("--trace", required=True)
    merge.add_argument("--actions", required=True)
    merge.add_argument("--output", required=True)
    merge.add_argument("--summary")
    merge.set_defaults(function=merge_rush)

    trace_summary = subparsers.add_parser(
        "summarize-trace", help="summarize one Chrome trace"
    )
    trace_summary.add_argument("--trace", required=True)
    trace_summary.add_argument("--output", required=True)
    trace_summary.set_defaults(function=summarize_trace)

    suite = subparsers.add_parser(
        "summarize-suite", help="aggregate benchmark result files"
    )
    suite.add_argument("--root", required=True)
    suite.set_defaults(function=summarize_suite)
    return parser


def main() -> None:
    parser = build_parser()
    arguments = parser.parse_args()
    arguments.function(arguments)


if __name__ == "__main__":
    main()
