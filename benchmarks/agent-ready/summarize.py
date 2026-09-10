#!/usr/bin/env python3
"""Convert hyperfine JSON files into one compact Agent Ready report."""

from __future__ import annotations

import argparse
import json
import statistics
from pathlib import Path


def percentile95(samples: list[float]) -> float:
    ordered = sorted(samples)
    return ordered[max(0, min(len(ordered) - 1, int(len(ordered) * 0.95 + 0.999999) - 1))]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    scenarios: dict[str, dict[str, dict[str, float]]] = {}
    for path in sorted(args.directory.glob("*.json")):
        if path.name in {"metadata.json", "summary.json"}:
            continue
        document = json.loads(path.read_text(encoding="utf-8"))
        scenario: dict[str, dict[str, float]] = {}
        for result in document["results"]:
            samples = result["times"]
            scenario[result["command"]] = {
                "mean_ms": statistics.mean(samples) * 1000,
                "median_ms": statistics.median(samples) * 1000,
                "stddev_ms": statistics.stdev(samples) * 1000 if len(samples) > 1 else 0,
                "p95_ms": percentile95(samples) * 1000,
            }
        scenarios[path.stem] = scenario
    (args.directory / "summary.json").write_text(
        json.dumps({"scenarios": scenarios}, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )

    lines = ["# Agent Ready performance results", "", "Times are milliseconds; lower is better.", ""]
    for name, scenario in scenarios.items():
        lines.extend([f"## {name}", "", "| command | median | mean | stddev | p95 |", "| --- | ---: | ---: | ---: | ---: |"])
        for command, metrics in scenario.items():
            lines.append(
                f"| {command} | {metrics['median_ms']:.2f} | {metrics['mean_ms']:.2f} | "
                f"{metrics['stddev_ms']:.2f} | {metrics['p95_ms']:.2f} |"
            )
        lines.append("")
    (args.directory / "report.md").write_text("\n".join(lines), encoding="utf-8")


if __name__ == "__main__":
    main()
