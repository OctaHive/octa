#!/usr/bin/env python3
"""Summarize raw cache benchmark samples and enforce the fixed release gate."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any

from suite import additional_checks, evaluate, load_thresholds, release_evidence_checks, summarize_samples, RESULT_SCHEMA_VERSION


def load_run(directory: Path) -> tuple[dict[str, Any], dict[str, dict[str, dict[str, Any]]]]:
    metadata = json.loads((directory / "metadata.json").read_text(encoding="utf-8"))
    if metadata.get("schema_version") != RESULT_SCHEMA_VERSION:
        raise ValueError("unsupported cache benchmark metadata schema")
    scenarios: dict[str, dict[str, dict[str, Any]]] = {}
    for path in sorted(directory.glob("*.json")):
        if path.name in {"metadata.json", "run-identity.json", "summary.json"}:
            continue
        document = json.loads(path.read_text(encoding="utf-8"))
        if document.get("schema_version") != RESULT_SCHEMA_VERSION:
            raise ValueError(f"unsupported schema in {path}")
        name = document["scenario"]
        scenarios[name] = {
            label: summarize_samples(samples, int(document.get("logical_bytes", 0)))
            for label, samples in document["measurements"].items()
        }
        scenarios[name]["_dimensions"] = document.get("dimensions", {})
    return metadata, scenarios


def _legacy_median(path: Path) -> float:
    document = json.loads(path.read_text(encoding="utf-8"))
    values = [result["median"] * 1000 for result in document["results"] if result["command"] == "octa-current"]
    if len(values) != 1:
        raise ValueError("legacy hyperfine document must contain one octa-current result")
    return values[0]


def markdown(metadata: dict[str, Any], scenarios: dict[str, dict[str, dict[str, Any]]], checks: list[dict[str, str]]) -> str:
    lines = [
        "# Task-result cache performance results",
        "",
        f"Mode: `{metadata['mode']}`; samples: {metadata['runs']}; host: `{metadata['system']}`.",
        "",
        "## Acceptance checks",
        "",
        "| check | status | detail |",
        "| --- | --- | --- |",
    ]
    for check in checks:
        lines.append(f"| {check['name']} | {check['status']} | {check['detail']} |")
    for name, labels in scenarios.items():
        lines.extend(["", f"## {name}"])
        publication = labels.get("_dimensions", {}).get("publication")
        if publication:
            lines.extend(
                [
                    "",
                    "Remote publication probe: "
                    f"{publication.get('http_requests', 0)} requests, "
                    f"{publication.get('bytes_uploaded', 0)} bytes uploaded, "
                    f"{publication.get('bytes_downloaded', 0)} bytes downloaded.",
                ]
            )
        filesystem_probe = labels.get("_dimensions", {}).get("filesystem_probe")
        if filesystem_probe:
            lines.extend(["", f"Filesystem probe: {filesystem_probe['file_syscalls']} file-related syscalls."])
        lines.extend(["", "| measurement | median ms | p95 ms | CPU ms | peak RSS MiB | cache |", "| --- | ---: | ---: | ---: | ---: | --- |"])
        for label, metrics in labels.items():
            if label == "_dimensions":
                continue
            cache = ", ".join(f"{key}={value}" for key, value in metrics.get("cache_statuses", {}).items()) or "n/a"
            lines.append(
                f"| {label} | {metrics['median_wall_ms']:.2f} | {metrics['p95_wall_ms']:.2f} | "
                f"{metrics['median_cpu_ms']:.2f} | {metrics['peak_rss_bytes'] / 1024 / 1024:.2f} | {cache} |"
            )
            for field in ("median_hash_mib_per_second", "median_pack_mib_per_second", "median_extract_mib_per_second", "median_compression_ratio", "median_http_requests", "median_bytes_uploaded", "median_bytes_downloaded"):
                if field in metrics:
                    lines.append(f"| ↳ {field} | {metrics[field]:.2f} |  |  |  |  |")
    return "\n".join(lines) + "\n"


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("directory", type=Path)
    parser.add_argument("--thresholds", type=Path)
    parser.add_argument("--legacy-warm", type=Path)
    parser.add_argument("--strict", action="store_true", help="fail on missing evidence as well as failed thresholds")
    args = parser.parse_args()
    thresholds = load_thresholds(args.thresholds) if args.thresholds else load_thresholds()
    metadata, scenarios = load_run(args.directory)
    checks = evaluate(scenarios, thresholds)
    legacy_median = _legacy_median(args.legacy_warm) if args.legacy_warm else None
    checks.extend(additional_checks(metadata, scenarios, thresholds, legacy_median))
    checks.extend(release_evidence_checks(metadata, scenarios))
    summary = {"schema_version": RESULT_SCHEMA_VERSION, "metadata": metadata, "scenarios": scenarios, "checks": checks}
    (args.directory / "summary.json").write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    (args.directory / "report.md").write_text(markdown(metadata, scenarios, checks), encoding="utf-8")
    failures = [check for check in checks if check["status"] == "fail" or (args.strict and check["status"] == "skip")]
    if failures:
        for failure in failures:
            print(f"{failure['status']}: {failure['name']}: {failure['detail']}")
        raise SystemExit(1)


if __name__ == "__main__":
    main()
