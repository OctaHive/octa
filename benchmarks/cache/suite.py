"""Stable result model and acceptance checks for cache performance runs.

The runner records raw observations without deciding whether they are good.
This module is the deliberately small policy boundary: it converts samples to
summary statistics and evaluates the thresholds fixed in ``thresholds.md``.
Keeping those jobs separate lets old raw samples be re-evaluated after report
format changes without rerunning expensive 100,000-file fixtures.
"""

from __future__ import annotations

import json
import math
import statistics
from collections import Counter
from pathlib import Path
from typing import Any, Iterable


RESULT_SCHEMA_VERSION = 1
DEFAULT_THRESHOLDS_PATH = Path(__file__).with_name("thresholds.json")

PHASE_10_SCENARIOS = (
    "non-cache-noop",
    "inputs-1",
    "inputs-1000",
    "inputs-100000",
    "large-input",
    "outputs-10000",
    "large-output-compressible",
    "large-output-incompressible",
    "remote-loopback",
    "remote-latency-20ms",
    "remote-latency-80ms",
    "compiler-task-cache",
    "concurrent-remote-consumers",
    "parallel-cacheable-25",
    "overlapping-inputs-25",
    "cross-workspace-hit",
    "automatic-plugin-contract",
)

RELEASE_SCALE = {
    "inputs_medium": 1_000,
    "inputs_large": 100_000,
    "output_files": 10_000,
    "large_bytes": 1024 * 1024 * 1024,
}


def percentile(values: Iterable[float], quantile: float) -> float:
    """Return the nearest-rank percentile used by the existing benchmark suite."""

    ordered = sorted(values)
    if not ordered:
        raise ValueError("a percentile requires at least one value")
    if not 0 < quantile <= 1:
        raise ValueError("quantile must be in (0, 1]")
    rank = max(1, math.ceil(len(ordered) * quantile))
    return ordered[rank - 1]


def parse_cache_outcome(stdout: str) -> dict[str, Any] | None:
    """Extract the single task cache outcome from CLI or runner JSONL output."""

    event_outcomes: list[dict[str, Any]] = []
    for line in stdout.splitlines():
        try:
            event = json.loads(line)
        except (ValueError, TypeError):
            continue
        if event.get("type") != "finished":
            data = event.get("data", {})
            kind = data.get("type")
            if kind == "cache_hit":
                event_outcomes.append({"status": "hit", "restored_bytes": data.get("restored_bytes", 0)})
            elif kind == "cache_miss":
                event_outcomes.append({"status": "miss", "reason": data.get("reason")})
            elif kind == "cache_error":
                event_outcomes.append({"status": "error", "reason": data.get("reason")})
            continue
        outcomes = [task["cache"] for result in event.get("results", []) for task in result.get("tasks", []) if isinstance(task, dict) and isinstance(task.get("cache"), dict)]
        if outcomes:
            event_outcomes.extend(outcomes)
    if len(event_outcomes) == 1:
        return event_outcomes[0]
    if event_outcomes:
        statuses = Counter(outcome.get("status", "unknown") for outcome in event_outcomes)
        layers = Counter(outcome.get("layer", "none") for outcome in event_outcomes if outcome.get("layer"))
        result: dict[str, Any] = {
            "status": next(iter(statuses)) if len(statuses) == 1 else "mixed",
            "statuses": dict(statuses),
            "tasks": len(event_outcomes),
        }
        if len(layers) == 1:
            result["layer"] = next(iter(layers))
        return result
    return None


def summarize_samples(samples: list[dict[str, Any]], logical_bytes: int = 0) -> dict[str, Any]:
    """Aggregate raw process samples while retaining resource and cache signals."""

    if not samples:
        raise ValueError("a benchmark summary requires at least one sample")
    wall = [float(sample["wall_ms"]) for sample in samples]
    cpu = [float(sample.get("cpu_ms", 0.0)) for sample in samples]
    result: dict[str, Any] = {
        "samples": len(samples),
        "median_wall_ms": statistics.median(wall),
        "mean_wall_ms": statistics.mean(wall),
        "p95_wall_ms": percentile(wall, 0.95),
        "median_cpu_ms": statistics.median(cpu),
        "peak_rss_bytes": max(int(sample.get("peak_rss_bytes", 0)) for sample in samples),
        "median_filesystem_operations": statistics.median(
            int(sample.get("filesystem_operations", 0)) for sample in samples
        ),
    }
    statuses = Counter(
        sample["cache"].get("status", "unknown")
        for sample in samples
        if isinstance(sample.get("cache"), dict)
    )
    layers = Counter(
        sample["cache"].get("layer", "none")
        for sample in samples
        if isinstance(sample.get("cache"), dict)
    )
    result["cache_statuses"] = dict(statuses)
    result["cache_layers"] = dict(layers)
    reasons = Counter(
        sample["cache"].get("reason", "none")
        for sample in samples
        if isinstance(sample.get("cache"), dict) and sample["cache"].get("reason")
    )
    result["cache_reasons"] = dict(reasons)
    restored = [
        int(sample["cache"]["restored_bytes"])
        for sample in samples
        if isinstance(sample.get("cache"), dict) and "restored_bytes" in sample["cache"]
    ]
    if restored:
        result["median_restored_bytes"] = statistics.median(restored)
    if logical_bytes:
        result["logical_bytes"] = logical_bytes
        result["median_throughput_mib_s"] = logical_bytes / (1024 * 1024) / (result["median_wall_ms"] / 1000)
    kernels = [sample["kernel"] for sample in samples if isinstance(sample.get("kernel"), dict)]
    if kernels and logical_bytes:
        mebibytes = logical_bytes / (1024 * 1024)
        if all("elapsed_ns" in kernel for kernel in kernels):
            rates = [mebibytes / (kernel["elapsed_ns"] / 1_000_000_000) for kernel in kernels]
            result["median_hash_mib_per_second"] = statistics.median(rates)
        if all("pack_elapsed_ns" in kernel and "extract_elapsed_ns" in kernel for kernel in kernels):
            result["median_pack_mib_per_second"] = statistics.median(
                mebibytes / (kernel["pack_elapsed_ns"] / 1_000_000_000) for kernel in kernels
            )
            result["median_extract_mib_per_second"] = statistics.median(
                mebibytes / (kernel["extract_elapsed_ns"] / 1_000_000_000) for kernel in kernels
            )
            result["median_compression_ratio"] = statistics.median(
                kernel["descriptor"]["expanded_size_bytes"] / kernel["descriptor"]["encoded_size_bytes"]
                for kernel in kernels
            )
    for field in ("http_requests", "bytes_uploaded", "bytes_downloaded"):
        values = [int(sample[field]) for sample in samples if field in sample]
        if values:
            result[f"median_{field}"] = statistics.median(values)
    routes = Counter()
    for sample in samples:
        routes.update(sample.get("http_routes", {}))
    if routes:
        result["http_routes"] = dict(routes)
    return result


def _check(name: str, passed: bool | None, detail: str) -> dict[str, str]:
    return {"name": name, "status": "skip" if passed is None else ("pass" if passed else "fail"), "detail": detail}


def load_thresholds(path: Path = DEFAULT_THRESHOLDS_PATH) -> dict[str, Any]:
    """Load the versioned machine-readable policy without silent defaults."""

    document = json.loads(path.read_text(encoding="utf-8"))
    if document.get("schema_version") != 1:
        raise ValueError("unsupported cache benchmark threshold schema")
    return document


def evaluate(
    scenarios: dict[str, dict[str, dict[str, Any]]],
    thresholds: dict[str, Any] | None = None,
) -> list[dict[str, str]]:
    """Evaluate every threshold for which the result document has evidence.

    Missing platform-specific evidence is reported as ``skip``, never silently
    accepted. The release gate treats both ``fail`` and ``skip`` as incomplete;
    a quick developer smoke run can opt to reject only actual failures.
    """

    policy = thresholds or load_thresholds()
    checks: list[dict[str, str]] = []
    non_cache = scenarios.get("non-cache-noop", {})
    current = non_cache.get("current")
    baseline = non_cache.get("baseline")
    if current and baseline:
        ratio = current["median_wall_ms"] / baseline["median_wall_ms"]
        limit = float(policy["non_cache_max_median_ratio"])
        checks.append(_check("non-cache-regression", ratio <= limit, f"current/baseline median ratio={ratio:.3f}, limit={limit:.3f}"))
    else:
        checks.append(_check("non-cache-regression", None, "current and baseline samples are required"))

    comparable = [
        measurements["current"]["median_wall_ms"] / measurements["baseline"]["median_wall_ms"]
        for measurements in scenarios.values()
        if "current" in measurements and "baseline" in measurements
    ]
    if comparable:
        ratio = math.prod(comparable) ** (1 / len(comparable))
        limit = float(policy["non_cache_geomean_ratio"])
        checks.append(
            _check(
                "non-cache-geomean-regression",
                ratio <= limit,
                f"geometric mean current/baseline ratio={ratio:.3f}, limit={limit:.3f}",
            )
        )
    else:
        checks.append(_check("non-cache-geomean-regression", None, "comparable non-cache samples are required"))

    for name in ("inputs-1", "inputs-1000", "inputs-100000"):
        document = scenarios.get(name, {})
        current = document.get("current-materialized")
        reference = document.get("go-task-warm")
        if current and reference:
            ratio = current["median_wall_ms"] / reference["median_wall_ms"]
            limit = float(policy["materialized_max_go_task_ratio"])
            checks.append(
                _check(
                    f"{name}-materialized-vs-go-task",
                    ratio <= limit,
                    f"Octa/go-task median ratio={ratio:.3f}, limit={limit:.3f}",
                )
            )
        else:
            checks.append(
                _check(
                    f"{name}-materialized-vs-go-task",
                    None,
                    "materialized Octa and warm go-task samples are required",
                )
            )

    one_file = scenarios.get("inputs-1", {})
    restored = one_file.get("current-warm")
    references = [
        result["median_wall_ms"]
        for result in (one_file.get("go-task-warm"), one_file.get("legacy-freshness"))
        if result
    ]
    if restored and references:
        ratio = restored["median_wall_ms"] / max(references)
        limit = float(policy["warm_hit_max_reference_ratio"])
        checks.append(
            _check(
                "one-file-restore-overhead",
                ratio <= limit,
                f"restore/reference median ratio={ratio:.3f}, limit={limit:.3f}",
            )
        )
    else:
        checks.append(_check("one-file-restore-overhead", None, "one-file restore and reference samples are required"))

    large_scenario = scenarios.get("large-input", {})
    large = large_scenario.get("kernel-snapshot")
    large_scale = large_scenario.get("_dimensions", {}).get("actual_scale")
    if (
        large
        and "median_hash_mib_per_second" in large
        and isinstance(large_scale, int)
        and large_scale >= RELEASE_SCALE["large_bytes"]
    ):
        throughput = large["median_hash_mib_per_second"]
        limit = float(policy["large_input_min_mib_per_second"])
        checks.append(_check("large-input-throughput", throughput >= limit, f"{throughput:.1f} MiB/s, minimum={limit:.1f} MiB/s"))
    else:
        checks.append(_check("large-input-throughput", None, "large-input cold samples are required"))

    for scenario in ("large-output-compressible", "large-output-incompressible"):
        document = scenarios.get(scenario, {})
        kernel = document.get("kernel-roundtrip")
        actual_scale = document.get("_dimensions", {}).get("actual_scale")
        for operation in ("pack", "extract"):
            field = f"median_{operation}_mib_per_second"
            if (
                kernel
                and field in kernel
                and isinstance(actual_scale, int)
                and actual_scale >= RELEASE_SCALE["large_bytes"]
            ):
                throughput = kernel[field]
                limit = float(policy["bundle_min_mib_per_second"])
                checks.append(
                    _check(
                        f"{scenario}-{operation}-throughput",
                        throughput >= limit,
                        f"{throughput:.1f} MiB/s, minimum={limit:.1f} MiB/s",
                    )
                )
            else:
                checks.append(_check(f"{scenario}-{operation}-throughput", None, f"{scenario} kernel samples are required"))

    output_scenario = scenarios.get("outputs-10000", {})
    outputs = output_scenario.get("current-warm")
    if outputs and output_scenario.get("_dimensions", {}).get("actual_scale") == RELEASE_SCALE["output_files"]:
        ratio = outputs["p95_wall_ms"] / outputs["median_wall_ms"]
        limit = float(policy["many_output_hit_max_p95_median_ratio"])
        checks.append(_check("many-output-hit-tail", ratio <= limit, f"p95/median ratio={ratio:.3f}, limit={limit:.3f}"))
    else:
        checks.append(_check("many-output-hit-tail", None, "10,000-output warm samples are required"))

    for name in ("remote-loopback", "remote-latency-20ms", "remote-latency-80ms"):
        remote = scenarios.get(name, {}).get("current-warm")
        if remote and "median_http_requests" in remote:
            requests = remote["median_http_requests"]
            limit = int(policy["remote_hit_max_requests"])
            checks.append(_check(f"{name}-request-bound", requests <= limit, f"median requests={requests:g}, limit={limit}"))
        else:
            checks.append(_check(f"{name}-request-bound", None, f"{name} HTTP counters are required"))

    cross = scenarios.get("cross-workspace-hit", {}).get("current-warm")
    if cross:
        hits = cross.get("cache_statuses", {}).get("hit", 0)
        checks.append(_check("cross-workspace-portability", hits == cross.get("samples"), f"hits={hits}/{cross.get('samples', 0)}"))
    else:
        checks.append(_check("cross-workspace-portability", None, "cross-workspace samples are required"))

    return checks


def additional_checks(
    metadata: dict[str, Any],
    scenarios: dict[str, dict[str, dict[str, Any]]],
    thresholds: dict[str, Any],
    legacy_median: float | None,
) -> list[dict[str, str]]:
    """Evaluate cross-scenario and resource bounds kept out of basic summaries."""

    checks: list[dict[str, str]] = []
    outputs = scenarios.get("outputs-10000", {})
    cold, warm = outputs.get("current-cold"), outputs.get("current-warm")
    output_scale = outputs.get("_dimensions", {}).get("actual_scale")
    if cold and warm and output_scale == RELEASE_SCALE["output_files"]:
        passed = warm["median_wall_ms"] < cold["median_wall_ms"]
        checks.append(
            _check(
                "many-output-hit-improves-execution",
                passed,
                f"warm={warm['median_wall_ms']:.1f}ms cold={cold['median_wall_ms']:.1f}ms",
            )
        )
    else:
        checks.append(
            _check(
                "many-output-hit-improves-execution",
                None,
                "cold and warm 10,000-output samples are required",
            )
        )

    plugin = scenarios.get("automatic-plugin-contract", {})
    planned, plain = plugin.get("current-warm"), plugin.get("current-nocache")
    if planned and plain:
        ratio = planned["median_wall_ms"] / plain["median_wall_ms"]
        limit = float(thresholds["automatic_plugin_plan_max_median_ratio"])
        checks.append(
            _check(
                "automatic-plugin-planning-overhead",
                ratio <= limit,
                f"warm/nocache ratio={ratio:.3f}, limit={limit:.3f}",
            )
        )
    else:
        checks.append(_check("automatic-plugin-planning-overhead", None, "plugin contract samples are required"))

    compiler = scenarios.get("compiler-task-cache", {})
    tool_cold, tool_warm = compiler.get("local-tool-cold"), compiler.get("local-tool-warm")
    if tool_cold and tool_warm:
        passed = tool_warm["median_wall_ms"] < tool_cold["median_wall_ms"]
        checks.append(
            _check(
                "local-compiler-tool-cache-improves-build",
                passed,
                f"warm={tool_warm['median_wall_ms']:.1f}ms cold={tool_cold['median_wall_ms']:.1f}ms",
            )
        )
    else:
        checks.append(_check("local-compiler-tool-cache-improves-build", None, "compiler samples are required"))

    baseline_rss = scenarios.get("non-cache-noop", {}).get("current", {}).get("peak_rss_bytes")
    allowance = int(thresholds["streaming_max_rss_overhead_bytes"])
    for scenario, label in [
        ("large-input", "kernel-snapshot"),
        ("large-output-compressible", "kernel-roundtrip"),
        ("large-output-incompressible", "kernel-roundtrip"),
    ]:
        sample = scenarios.get(scenario, {}).get(label)
        if sample and baseline_rss is not None:
            overhead = max(0, sample["peak_rss_bytes"] - baseline_rss)
            checks.append(
                _check(f"{scenario}-rss", overhead <= allowance, f"overhead={overhead} bytes, limit={allowance}")
            )
        else:
            checks.append(_check(f"{scenario}-rss", None, f"{scenario} and no-cache RSS samples are required"))

    many = scenarios.get("inputs-100000", {})
    many_cold = many.get("current-cold")
    actual_entries = many.get("_dimensions", {}).get("actual_scale")
    if many_cold and baseline_rss is not None and actual_entries == RELEASE_SCALE["inputs_large"]:
        limit = allowance + int(thresholds["many_entries_max_rss_bytes_per_entry"]) * int(actual_entries)
        overhead = max(0, many_cold["peak_rss_bytes"] - baseline_rss)
        checks.append(_check("many-input-rss", overhead <= limit, f"overhead={overhead} bytes, limit={limit}"))
    else:
        checks.append(_check("many-input-rss", None, "100,000-input RSS samples are required"))

    one = scenarios.get("inputs-1", {})
    current = one.get("current-warm")
    go_task = one.get("go-task-warm")
    measured_legacy = one.get("legacy-freshness")
    reference_legacy = measured_legacy["median_wall_ms"] if measured_legacy else legacy_median
    if current and go_task and reference_legacy is not None:
        reference = max(go_task["median_wall_ms"], reference_legacy)
        ratio = current["median_wall_ms"] / reference
        limit = float(thresholds["warm_hit_max_reference_ratio"])
        checks.append(
            _check(
                "one-file-warm-reference",
                ratio <= limit,
                f"current/reference ratio={ratio:.3f}, limit={limit:.3f}",
            )
        )
    else:
        checks.append(
            _check(
                "one-file-warm-reference",
                None,
                "current, go-task, and legacy freshness samples are required",
            )
        )

    parallel = scenarios.get("parallel-cacheable-25", {}).get("current-cold")
    overlapping = scenarios.get("overlapping-inputs-25", {}).get("current-cold")
    if parallel and overlapping:
        ratio = overlapping["median_cpu_ms"] / parallel["median_cpu_ms"]
        limit = float(thresholds["overlapping_max_cpu_ratio"])
        checks.append(
            _check(
                "overlapping-input-read-amplification",
                ratio <= limit,
                f"overlapping/disjoint CPU ratio={ratio:.3f}, limit={limit:.3f}; hash budget={metadata.get('hash_budget')}",
            )
        )
    else:
        checks.append(
            _check(
                "overlapping-input-read-amplification",
                None,
                "parallel and overlapping-input samples are required",
            )
        )
    return checks


def release_evidence_checks(
    metadata: dict[str, Any],
    scenarios: dict[str, dict[str, dict[str, Any]]],
) -> list[dict[str, str]]:
    """Reject a partial or development-scale run as release evidence.

    Quick runs intentionally exercise the same orchestration with small data.
    Without this independent gate, a smoke run could accidentally satisfy a
    throughput threshold while never materializing the promised 100,000 files
    or 1 GiB streams.
    """

    release = metadata.get("mode") == "release"

    def evidence(passed: bool) -> bool | None:
        return passed if release else None

    checks = [
        _check("release-mode", True if release else None, f"mode={metadata.get('mode')}"),
        _check(
            "release-sample-count",
            evidence(int(metadata.get("runs", 0)) >= 15),
            f"samples={metadata.get('runs', 0)}, minimum=15",
        ),
        _check(
            "release-warmup-count",
            evidence(int(metadata.get("warmups", 0)) >= 3),
            f"warmups={metadata.get('warmups', 0)}, minimum=3",
        ),
    ]
    missing = sorted(set(PHASE_10_SCENARIOS).difference(scenarios))
    checks.append(
        _check(
            "release-scenario-matrix",
            evidence(not missing),
            "complete" if not missing else f"missing={','.join(missing)}",
        )
    )
    actual = metadata.get("actual_scale", {})
    for name, expected in RELEASE_SCALE.items():
        checks.append(
            _check(
                f"release-scale-{name.replace('_', '-')}",
                evidence(actual.get(name) == expected),
                f"actual={actual.get(name)}, expected={expected}",
            )
        )
    undersampled = sorted(
        f"{scenario}/{label}"
        for scenario, measurements in scenarios.items()
        for label, summary in measurements.items()
        if label != "_dimensions" and int(summary.get("samples", 0)) < 15
    )
    checks.append(
        _check(
            "release-measurement-samples",
            evidence(not undersampled),
            "complete" if not undersampled else f"undersampled={','.join(undersampled)}",
        )
    )
    return checks
