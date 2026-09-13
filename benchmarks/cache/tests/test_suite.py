"""Unit tests for the cache performance result model and acceptance gate."""

from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path


sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from suite import (  # noqa: E402
    PHASE_10_SCENARIOS,
    additional_checks,
    evaluate,
    load_thresholds,
    parse_cache_outcome,
    percentile,
    release_evidence_checks,
    summarize_samples,
)
from measure import measure, parse_strace_calls  # noqa: E402
from server import FileStore  # noqa: E402
from fixtures import Scale, input_fixture, legacy_freshness_fixture, write_profile  # noqa: E402
from run import changed_identity_fields, write_json_atomic  # noqa: E402


class SuiteTests(unittest.TestCase):
    def test_quick_scale_preserves_scenario_shapes_without_claiming_release_sizes(self) -> None:
        quick = Scale.quick()
        release = Scale.release()
        self.assertLess(quick.inputs_large, release.inputs_large)
        self.assertEqual(release.inputs_large, 100_000)
        self.assertEqual(release.output_files, 10_000)
        self.assertEqual(release.large_bytes, 1024 * 1024 * 1024)

    def test_input_fixture_and_profile_are_complete(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fixture = input_fixture(root, "inputs", 3)
            write_profile(root / "cache.toml", root / "cas", max_parallel_hashes=2)
            self.assertEqual(fixture.logical_bytes, 3 * 64)
            self.assertIn('"inputs/**/*"', (fixture.workspace / "Octafile.yml").read_text())
            self.assertIn("max_parallel_hashes = 2", (root / "cache.toml").read_text())

    def test_legacy_fixture_uses_the_removed_freshness_schema(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            fixture = legacy_freshness_fixture(Path(temporary))
            octafile = (fixture.workspace / "Octafile.yml").read_text(encoding="utf-8")
            self.assertIn("sources: [input.txt]", octafile)
            self.assertIn("output: [output.txt]", octafile)

    def test_reference_store_uses_create_if_absent_semantics(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            store = FileStore(Path(temporary))
            destination = store.blob_path(["blake3", "a" * 64, "4", "identity", "4", "1"])
            self.assertEqual(store.publish_bytes(destination, b"same").value, 201)
            self.assertEqual(store.publish_bytes(destination, b"same").value, 204)
            self.assertEqual(store.publish_bytes(destination, b"else").value, 409)

    def test_process_measurement_captures_output_and_resource_usage(self) -> None:
        result = measure(
            {
                "command": [sys.executable, "-c", "print('measured')"],
                "cwd": str(Path.cwd()),
            }
        )
        self.assertEqual(result["exit_code"], 0)
        self.assertEqual(result["stdout"], "measured\n")
        self.assertGreater(result["wall_ms"], 0)
        self.assertGreater(result["peak_rss_bytes"], 0)

    def test_strace_summary_parser_uses_the_named_calls_column(self) -> None:
        summary = """
% time     seconds  usecs/call     calls    errors syscall
------ ----------- ----------- --------- --------- ----------------
 77.00    0.001000           2       401        10 openat
 23.00    0.000300           1        99           stat
------ ----------- ----------- --------- --------- ----------------
100.00    0.001300           2       500        10 total
"""
        self.assertEqual(parse_strace_calls(summary), 500)
        self.assertEqual(parse_strace_calls(summary.replace("       500        10 total", "       500 total")), 500)

    def test_percentile_uses_nearest_rank(self) -> None:
        self.assertEqual(percentile([1.0, 2.0, 3.0, 4.0], 0.95), 4.0)
        self.assertEqual(percentile([4.0, 1.0, 3.0, 2.0], 0.50), 2.0)

    def test_cache_outcome_is_read_from_finished_jsonl_event(self) -> None:
        stdout = "noise\n" + json.dumps(
            {
                "type": "finished",
                "results": [
                    {
                        "tasks": [
                            {
                                "cache": {
                                    "status": "hit",
                                    "layer": "remote",
                                    "reason": None,
                                    "restored_bytes": 42,
                                }
                            }
                        ]
                    }
                ],
            }
        )
        self.assertEqual(
            parse_cache_outcome(stdout),
            {
                "status": "hit",
                "layer": "remote",
                "reason": None,
                "restored_bytes": 42,
            },
        )

    def test_cache_outcome_is_read_from_cli_execution_events(self) -> None:
        stdout = json.dumps(
            {
                "schema_version": 4,
                "category": "execution",
                "data": {"type": "cache_miss", "action": "blake3:test", "reason": "action_not_found"},
            }
        )
        self.assertEqual(parse_cache_outcome(stdout), {"status": "miss", "reason": "action_not_found"})

    def test_summary_retains_raw_resource_and_cache_metrics(self) -> None:
        summary = summarize_samples(
            [
                {
                    "wall_ms": 10.0,
                    "cpu_ms": 8.0,
                    "peak_rss_bytes": 100,
                    "filesystem_operations": 4,
                    "cache": {"status": "hit", "layer": "local", "restored_bytes": 42},
                    "http_routes": {"read_action": 1},
                },
                {
                    "wall_ms": 20.0,
                    "cpu_ms": 12.0,
                    "peak_rss_bytes": 120,
                    "filesystem_operations": 6,
                    "cache": {"status": "hit", "layer": "local", "restored_bytes": 84},
                    "http_routes": {"read_action": 1, "read_blob": 1},
                },
            ],
            logical_bytes=10 * 1024 * 1024,
        )
        self.assertEqual(summary["median_wall_ms"], 15.0)
        self.assertEqual(summary["p95_wall_ms"], 20.0)
        self.assertEqual(summary["peak_rss_bytes"], 120)
        self.assertEqual(summary["cache_statuses"], {"hit": 2})
        self.assertEqual(summary["median_restored_bytes"], 63)
        self.assertEqual(summary["http_routes"], {"read_action": 2, "read_blob": 1})
        self.assertAlmostEqual(summary["median_throughput_mib_s"], 666.666666, places=5)

    def test_acceptance_gate_rejects_a_non_cache_regression_over_five_percent(self) -> None:
        checks = evaluate(
            {
                "non-cache-noop": {
                    "current": {"median_wall_ms": 10.6, "p95_wall_ms": 11.0},
                    "baseline": {"median_wall_ms": 10.0, "p95_wall_ms": 10.5},
                }
            }
        )
        self.assertEqual(checks[0]["status"], "fail")
        self.assertIn("1.060", checks[0]["detail"])

    def test_geomean_and_overlapping_cpu_thresholds_are_enforced(self) -> None:
        checks = evaluate(
            {
                "non-cache-noop": {
                    "current": {"median_wall_ms": 10.4},
                    "baseline": {"median_wall_ms": 10.0},
                }
            }
        )
        geomean = next(check for check in checks if check["name"] == "non-cache-geomean-regression")
        self.assertEqual(geomean["status"], "fail")

        checks = additional_checks(
            {"hash_budget": 4},
            {
                "parallel-cacheable-25": {"current-cold": {"median_cpu_ms": 100.0}},
                "overlapping-inputs-25": {"current-cold": {"median_cpu_ms": 126.0}},
            },
            load_thresholds(),
            None,
        )
        overlap = next(check for check in checks if check["name"] == "overlapping-input-read-amplification")
        self.assertEqual(overlap["status"], "fail")

    def test_materialized_cache_hit_must_not_be_slower_than_go_task(self) -> None:
        checks = evaluate(
            {
                "inputs-1": {
                    "current-materialized": {"median_wall_ms": 10.1},
                    "current-warm": {"median_wall_ms": 14.0},
                    "go-task-warm": {"median_wall_ms": 10.0},
                }
            }
        )
        comparison = next(check for check in checks if check["name"] == "inputs-1-materialized-vs-go-task")
        restore = next(check for check in checks if check["name"] == "one-file-restore-overhead")
        self.assertEqual(comparison["status"], "fail")
        self.assertEqual(restore["status"], "pass")

    def test_quick_scale_does_not_claim_release_size_thresholds(self) -> None:
        checks = evaluate(
            {
                "outputs-10000": {
                    "current-warm": {"median_wall_ms": 10.0, "p95_wall_ms": 100.0},
                    "_dimensions": {"actual_scale": 100},
                }
            }
        )
        tail = next(check for check in checks if check["name"] == "many-output-hit-tail")
        self.assertEqual(tail["status"], "skip")

    def test_release_evidence_rejects_development_scale_and_missing_scenarios(self) -> None:
        checks = release_evidence_checks(
            {
                "mode": "quick",
                "runs": 3,
                "warmups": 1,
                "actual_scale": {
                    "inputs_medium": 50,
                    "inputs_large": 500,
                    "output_files": 100,
                    "large_bytes": 8 * 1024 * 1024,
                },
            },
            {"inputs-1": {"current-warm": {"samples": 3}}},
        )
        self.assertTrue(all(check["status"] == "skip" for check in checks))

    def test_threshold_policy_is_versioned_and_machine_readable(self) -> None:
        thresholds = load_thresholds()
        self.assertEqual(thresholds["schema_version"], 1)
        self.assertEqual(thresholds["non_cache_max_median_ratio"], 1.05)

    def test_phase_catalog_covers_every_required_workload_shape(self) -> None:
        self.assertEqual(
            set(PHASE_10_SCENARIOS),
            {
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
            },
        )

    def test_atomic_json_writer_replaces_a_complete_document(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "identity.json"
            write_json_atomic(path, {"version": 1})
            write_json_atomic(path, {"version": 2})
            self.assertEqual(json.loads(path.read_text(encoding="utf-8")), {"version": 2})
            self.assertFalse(path.with_suffix(".json.tmp").exists())

    def test_resume_identity_rejects_changed_missing_and_unknown_fields(self) -> None:
        self.assertEqual(changed_identity_fields({"host": "a"}, {"host": "a"}), [])
        self.assertEqual(changed_identity_fields({"host": "a"}, {}), ["host"])
        self.assertEqual(changed_identity_fields({"host": "a"}, {"host": "b", "old": True}), ["host", "old"])


if __name__ == "__main__":
    unittest.main()
