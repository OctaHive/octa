#!/usr/bin/env python3
"""Measure one child process with an isolated POSIX resource-usage scope.

The benchmark coordinator launches this helper once per sample. That matters
for ``ru_maxrss``: POSIX exposes a maximum over all children of one process, so
measuring every command in the coordinator would retain the largest earlier
sample and over-report later scenarios.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import re
import resource
import subprocess
import time
from pathlib import Path
from typing import Any


def parse_strace_calls(summary: str) -> int:
    """Read the aggregate call count from a portable ``strace -c`` table."""

    lines = [line.strip() for line in summary.splitlines() if line.strip()]
    header = next((line for line in lines if "calls" in line and "syscall" in line), None)
    total = next((line for line in reversed(lines) if line.endswith(" total")), None)
    if header is None or total is None:
        raise ValueError("strace summary has no calls header or total row")
    columns = re.sub(r"%\s*time", "percent_time", header).split()
    values = total.split()
    index = columns.index("calls")
    # The optional `errors` cell is blank when the total is zero, so token
    # counts can differ while every column through `calls` stays aligned.
    if len(values) <= index:
        raise ValueError("strace summary total row has no calls value")
    return int(values[index])


def measure(request: dict[str, Any]) -> dict[str, Any]:
    """Run a command and return timing, resource, and captured-output data."""

    environment = os.environ.copy()
    environment.update({str(key): str(value) for key, value in request.get("environment", {}).items()})
    started = time.perf_counter_ns()
    process = subprocess.run(
        request["command"],
        cwd=request["cwd"],
        env=environment,
        input=request.get("stdin"),
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        check=False,
    )
    elapsed_ns = time.perf_counter_ns() - started
    usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    rss_scale = 1 if platform.system() == "Darwin" else 1024
    return {
        "wall_ms": elapsed_ns / 1_000_000,
        "cpu_ms": (usage.ru_utime + usage.ru_stime) * 1000,
        "user_cpu_ms": usage.ru_utime * 1000,
        "system_cpu_ms": usage.ru_stime * 1000,
        "peak_rss_bytes": int(usage.ru_maxrss * rss_scale),
        # These are block-I/O operation counters from getrusage, not an
        # estimate derived from bytes. Linux syscall counts are an optional
        # separate strace probe in the coordinator.
        "filesystem_operations": int(usage.ru_inblock + usage.ru_oublock),
        "voluntary_context_switches": int(usage.ru_nvcsw),
        "involuntary_context_switches": int(usage.ru_nivcsw),
        "exit_code": process.returncode,
        "stdout": process.stdout,
        "stderr": process.stderr,
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("request", type=Path)
    parser.add_argument("result", type=Path)
    args = parser.parse_args()
    request = json.loads(args.request.read_text(encoding="utf-8"))
    result = measure(request)
    args.result.write_text(json.dumps(result, sort_keys=True) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
