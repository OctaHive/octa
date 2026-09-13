#!/usr/bin/env python3
"""Run the release-binary task-result cache performance matrix.

Full mode materializes the exact Phase 10 sizes and uses the fixed 15-sample
policy. ``--quick`` is a development smoke test only. Each completed scenario
is written atomically, so an interrupted full run can continue with
``--resume`` without mixing partial samples into a report.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import platform
import shutil
import ssl
import subprocess
import sys
import tempfile
import time
import urllib.request
import uuid
from concurrent.futures import ThreadPoolExecutor
from contextlib import AbstractContextManager
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable

from fixtures import (
    Fixture,
    Scale,
    clone_workspace,
    compiler_fixture,
    input_fixture,
    large_input_fixture,
    large_output_fixture,
    legacy_freshness_fixture,
    no_cache_fixture,
    output_fixture,
    parallel_fixture,
    plugin_contract_fixture,
    write_profile,
)
from measure import parse_strace_calls
from suite import PHASE_10_SCENARIOS, RESULT_SCHEMA_VERSION, parse_cache_outcome


HERE = Path(__file__).resolve().parent
REPOSITORY = HERE.parents[1]
MEASURE = HERE / "measure.py"
SERVER = HERE / "server.py"


def remove_path(path: Path | None) -> None:
    if path is None:
        return
    if path.is_dir() and not path.is_symlink():
        shutil.rmtree(path)
    else:
        path.unlink(missing_ok=True)


def write_json_atomic(path: Path, document: dict[str, Any]) -> None:
    """Publish one resumable harness document without exposing partial JSON."""

    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(document, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    os.replace(temporary, path)


def changed_identity_fields(current: dict[str, Any], previous: dict[str, Any]) -> list[str]:
    """Return every top-level field that makes two resumable runs incompatible."""

    return sorted(key for key in current.keys() | previous.keys() if current.get(key) != previous.get(key))


class RemoteServer(AbstractContextManager["RemoteServer"]):
    """Own one isolated disk-backed HTTPS fixture process."""

    def __init__(
        self,
        root: Path,
        latency_ms: int,
        token: Path,
        certificate: Path,
        ca_certificate: Path,
        key: Path,
    ) -> None:
        self.root = root
        self.latency_ms = latency_ms
        self.token = token
        self.certificate = certificate
        self.ca_certificate = ca_certificate
        self.key = key
        self.ready = root / "ready.json"
        self.process: subprocess.Popen[str] | None = None
        self.endpoint = ""

    def __enter__(self) -> "RemoteServer":
        self.root.mkdir(parents=True, exist_ok=True)
        self.process = subprocess.Popen(
            [
                sys.executable,
                str(SERVER),
                "--root",
                str(self.root / "objects"),
                "--certificate",
                str(self.certificate),
                "--key",
                str(self.key),
                "--token",
                self.token.read_text(encoding="utf-8").strip(),
                "--latency-ms",
                str(self.latency_ms),
                "--ready-file",
                str(self.ready),
            ],
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            if self.ready.exists():
                self.endpoint = json.loads(self.ready.read_text(encoding="utf-8"))["endpoint"]
                return self
            if self.process.poll() is not None:
                _, error = self.process.communicate()
                raise RuntimeError(f"cache fixture exited during startup: {error}")
            time.sleep(0.02)
        raise TimeoutError("cache fixture did not become ready")

    def metrics(self) -> dict[str, Any]:
        context = ssl.create_default_context()
        context.check_hostname = False
        context.verify_mode = ssl.CERT_NONE
        with urllib.request.urlopen(self.endpoint + "__metrics", context=context, timeout=5) as response:
            return json.load(response)

    def configuration(self) -> dict[str, str]:
        return {
            "endpoint": self.endpoint,
            "token_file": str(self.token),
            "ca_certificate_file": str(self.ca_certificate),
        }

    def __exit__(self, *_error: object) -> None:
        if self.process is None:
            return
        self.process.terminate()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=5)


class Harness:
    """Scenario coordinator; measured child execution is delegated to measure.py."""

    def __init__(self, args: argparse.Namespace, scratch: Path, scale: Scale) -> None:
        self.args = args
        self.scratch = scratch
        self.scale = scale
        self.fixtures = scratch / "fixtures"
        self.caches = scratch / "caches"
        self.measurements = scratch / "measurements"
        self.output = args.output.resolve()
        self.output.mkdir(parents=True, exist_ok=True)
        self.measurements.mkdir(parents=True, exist_ok=True)
        self.common_environment = {
            "NO_COLOR": "1",
            "OCTA_PLUGINS_DIR": str(args.plugins.resolve()),
        }
        self.hash_budget = args.hash_budget

    def selected(self, name: str) -> bool:
        return not self.args.scenarios or name in self.args.scenarios

    def completed(self, name: str) -> bool:
        if not self.args.resume:
            return False
        path = self.output / f"{name}.json"
        if not path.is_file():
            return False
        required = {
            "large-input": "kernel-snapshot",
            "large-output-compressible": "kernel-roundtrip",
            "large-output-incompressible": "kernel-roundtrip",
        }.get(name)
        if required is None:
            return True
        document = json.loads(path.read_text(encoding="utf-8"))
        return required in document.get("measurements", {})

    def recorded_without_kernel(self, name: str) -> bool:
        """Return whether resume needs only the deterministic kernel probe."""

        return self.args.resume and (self.output / f"{name}.json").is_file()

    def record(
        self,
        fixture: Fixture,
        measurements: dict[str, list[dict[str, Any]]],
        **dimensions: Any,
    ) -> None:
        document = {
            "schema_version": RESULT_SCHEMA_VERSION,
            "scenario": fixture.name,
            "logical_bytes": fixture.logical_bytes,
            "dimensions": dimensions,
            "measurements": measurements,
        }
        destination = self.output / f"{fixture.name}.json"
        write_json_atomic(destination, document)
        print(f"recorded {fixture.name}", flush=True)

    def measure(
        self,
        command: list[str],
        workspace: Path,
        *,
        stdin: str | None = None,
        environment: dict[str, str] | None = None,
        capture_json: bool = False,
        expected_hit_layer: str | None = None,
    ) -> dict[str, Any]:
        identifier = uuid.uuid4().hex
        request = self.measurements / f"{identifier}.request.json"
        result = self.measurements / f"{identifier}.result.json"
        merged = dict(self.common_environment)
        merged["OCTA_DATA_DIR"] = str(self.scratch / "runtime-data" / identifier)
        if environment:
            merged.update(environment)
        request.write_text(
            json.dumps(
                {
                    "command": command,
                    "cwd": str(workspace),
                    "environment": merged,
                    "stdin": stdin,
                }
            ),
            encoding="utf-8",
        )
        completed = subprocess.run([sys.executable, str(MEASURE), str(request), str(result)], check=False)
        request.unlink(missing_ok=True)
        if completed.returncode != 0 or not result.exists():
            raise RuntimeError(f"measurement helper failed for {' '.join(command)}")
        sample = json.loads(result.read_text(encoding="utf-8"))
        result.unlink()
        if sample["exit_code"] != 0:
            raise RuntimeError(
                f"benchmark command failed ({sample['exit_code']}): {' '.join(command)}\n"
                f"stdout={sample['stdout'][-4000:]}\nstderr={sample['stderr'][-4000:]}"
            )
        sample["cache"] = parse_cache_outcome(sample["stdout"])
        if expected_hit_layer and sample["cache"] and sample["cache"].get("status") == "hit":
            sample["cache"].setdefault("layer", expected_hit_layer)
        if capture_json:
            sample["kernel"] = json.loads(sample["stdout"])
        del sample["stdout"]
        del sample["stderr"]
        return sample

    def octa_command(
        self,
        fixture: Fixture,
        profile: Path | None = None,
        binary: Path | None = None,
        extra: tuple[str, ...] = (),
    ) -> list[str]:
        command = [str((binary or self.args.octa).resolve()), "--dir", str(fixture.workspace), "--quiet", "--output", "json"]
        if profile:
            command.extend(["--cache-profile", str(profile)])
        command.extend(fixture.octa_arguments)
        command.extend(extra)
        command.append(fixture.task)
        return command

    def filesystem_probe(
        self,
        command: list[str],
        workspace: Path,
        environment: dict[str, str] | None = None,
    ) -> dict[str, Any] | None:
        """Count file-related syscalls outside the timed sample population."""

        if self.args.strace is None:
            return None
        summary = self.measurements / f"{uuid.uuid4().hex}.strace"
        merged = os.environ.copy()
        merged.update(self.common_environment)
        if environment:
            merged.update(environment)
        completed = subprocess.run(
            [str(self.args.strace.resolve()), "-f", "-qq", "-c", "-e", "trace=%file", "-o", str(summary), *command],
            cwd=workspace,
            env=merged,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        if completed.returncode != 0:
            raise RuntimeError(f"filesystem trace failed ({completed.returncode}): {completed.stderr[-4000:]}")
        text = summary.read_text(encoding="utf-8")
        summary.unlink()
        return {"file_syscalls": parse_strace_calls(text), "strace_summary": text}

    def task_command(self, fixture: Fixture) -> list[str]:
        return [
            str(self.args.task.resolve()),
            "--offline",
            "--dir",
            str(fixture.workspace),
            "--silent",
            *fixture.task_arguments,
            fixture.task,
        ]

    def samples(
        self,
        command: Callable[[], list[str]],
        fixture: Fixture,
        prepare: Callable[[], None],
        *,
        prime: Callable[[], None] | None = None,
        environment: dict[str, str] | None = None,
    ) -> list[dict[str, Any]]:
        if prime:
            prime()
        for _ in range(self.scale.warmups):
            prepare()
            self.measure(command(), fixture.workspace, environment=environment)
        result = []
        for _ in range(self.scale.runs):
            prepare()
            result.append(self.measure(command(), fixture.workspace, environment=environment))
        return result

    def local_cache_samples(
        self,
        fixture: Fixture,
        cache: Path,
        warm: bool,
        *,
        materialized: bool = False,
    ) -> list[dict[str, Any]]:
        profile = fixture.workspace / "cache.toml"

        def configure() -> None:
            write_profile(profile, cache, max_parallel_hashes=self.hash_budget)

        def cold_prepare() -> None:
            remove_path(fixture.output)
            remove_path(cache)
            configure()

        def warm_prime() -> None:
            cold_prepare()
            self.measure(self.octa_command(fixture, profile), fixture.workspace)

        def warm_prepare() -> None:
            if not materialized:
                remove_path(fixture.output)
            configure()

        samples = self.samples(
            lambda: self.octa_command(fixture, profile),
            fixture,
            warm_prepare if warm else cold_prepare,
            prime=warm_prime if warm else None,
        )
        expected = "hit" if warm else "miss"
        if not all((sample.get("cache") or {}).get("status") == expected for sample in samples):
            raise RuntimeError(f"{fixture.name} expected every current sample to be a cache {expected}")
        if warm:
            for sample in samples:
                if (sample.get("cache") or {}).get("status") == "hit":
                    sample["cache"].setdefault("layer", "local")
        return samples

    def go_task_samples(self, fixture: Fixture, warm: bool) -> list[dict[str, Any]]:
        def prime() -> None:
            remove_path(fixture.output)
            self.measure(self.task_command(fixture), fixture.workspace)

        def prepare() -> None:
            if not warm:
                remove_path(fixture.output)

        return self.samples(lambda: self.task_command(fixture), fixture, prepare, prime=prime if warm else None)

    def benchmark_no_cache(self) -> None:
        fixture = no_cache_fixture(self.fixtures)
        measurements = {
            "current": self.samples(lambda: self.octa_command(fixture), fixture, lambda: None),
            "go-task": self.samples(lambda: self.task_command(fixture), fixture, lambda: None),
        }
        if self.args.baseline_octa:
            measurements["baseline"] = self.samples(
                lambda: self.octa_command(fixture, binary=self.args.baseline_octa),
                fixture,
                lambda: None,
                environment={"OCTA_PLUGINS_DIR": str(self.args.baseline_plugins.resolve())},
            )
        self.record(fixture, measurements, filesystem_probe=self.filesystem_probe(self.octa_command(fixture), fixture.workspace))

    def benchmark_local_fixture(self, fixture: Fixture, include_go_task: bool = True) -> None:
        cache = self.caches / fixture.name
        measurements = {
            "current-cold": self.local_cache_samples(fixture, cache, False),
            "current-warm": self.local_cache_samples(fixture, cache, True),
            # Keep restoration and freshness checks separate. go-task leaves
            # outputs materialized on its warm path, so this is the comparable
            # measurement while current-warm continues to time full CAS restore.
            "current-materialized": self.local_cache_samples(fixture, cache, True, materialized=True),
        }
        if include_go_task:
            measurements["go-task-cold"] = self.go_task_samples(fixture, False)
            measurements["go-task-warm"] = self.go_task_samples(fixture, True)
        if fixture.name == "inputs-1" and self.args.baseline_octa:
            legacy = legacy_freshness_fixture(self.fixtures)

            def prime_legacy() -> None:
                remove_path(legacy.output)
                self.measure(
                    self.octa_command(legacy, binary=self.args.baseline_octa),
                    legacy.workspace,
                    environment={"OCTA_PLUGINS_DIR": str(self.args.baseline_plugins.resolve())},
                )

            measurements["legacy-freshness"] = self.samples(
                lambda: self.octa_command(legacy, binary=self.args.baseline_octa),
                legacy,
                lambda: None,
                prime=prime_legacy,
                environment={"OCTA_PLUGINS_DIR": str(self.args.baseline_plugins.resolve())},
            )
        remove_path(fixture.output)
        write_profile(fixture.workspace / "cache.toml", cache, max_parallel_hashes=self.hash_budget)
        self.record(
            fixture,
            measurements,
            actual_scale=self._actual_scale(fixture.name),
            filesystem_probe=self.filesystem_probe(
                self.octa_command(fixture, fixture.workspace / "cache.toml"), fixture.workspace
            ),
        )

    def benchmark_cross_workspace(self) -> None:
        fixture = input_fixture(self.fixtures, "cross-workspace-producer", 1)
        consumer_path = self.fixtures / "cross-workspace-hit"
        clone_workspace(fixture.workspace, consumer_path)
        consumer = Fixture("cross-workspace-hit", consumer_path, fixture.task, fixture.logical_bytes, consumer_path / "outputs")
        cache = self.caches / "cross-workspace"
        producer_profile = fixture.workspace / "cache.toml"
        consumer_profile = consumer.workspace / "cache.toml"
        write_profile(producer_profile, cache, max_parallel_hashes=self.hash_budget)
        remove_path(fixture.output)
        remove_path(cache)
        self.measure(self.octa_command(fixture, producer_profile), fixture.workspace)

        def prepare() -> None:
            remove_path(consumer.output)
            write_profile(consumer_profile, cache, max_parallel_hashes=self.hash_budget)

        samples = self.samples(lambda: self.octa_command(consumer, consumer_profile), consumer, prepare)
        if not all((sample.get("cache") or {}).get("status") == "hit" for sample in samples):
            raise RuntimeError("equivalent second workspace did not receive a local cache hit")
        for sample in samples:
            if (sample.get("cache") or {}).get("status") == "hit":
                sample["cache"].setdefault("layer", "local")
        self.record(consumer, {"current-warm": samples}, producer_workspace=str(fixture.workspace))

    def benchmark_remote(self, name: str, latency_ms: int) -> None:
        producer = input_fixture(self.fixtures, f"{name}-producer", 1)
        consumer_path = self.fixtures / name
        clone_workspace(producer.workspace, consumer_path)
        consumer = Fixture(name, consumer_path, producer.task, producer.logical_bytes, consumer_path / "outputs")
        server_root = self.scratch / "servers" / name
        with RemoteServer(
            server_root,
            latency_ms,
            self.args.token,
            self.args.certificate,
            self.args.ca_certificate,
            self.args.key,
        ) as server:
            producer_cache = self.caches / f"{name}-producer"
            producer_profile = producer.workspace / "cache.toml"
            write_profile(
                producer_profile,
                producer_cache,
                max_parallel_hashes=self.hash_budget,
                remote=server.configuration(),
            )
            remove_path(producer.output)
            remove_path(producer_cache)
            publication_before = server.metrics()
            publication = self.measure(self.octa_command(producer, producer_profile), producer.workspace)
            publication.update(metric_delta(publication_before, server.metrics()))
            consumer_cache = self.caches / name
            consumer_profile = consumer.workspace / "cache.toml"

            def prepare() -> None:
                remove_path(consumer.output)
                remove_path(consumer_cache)
                write_profile(
                    consumer_profile,
                    consumer_cache,
                    max_parallel_hashes=self.hash_budget,
                    remote=server.configuration(),
                )

            def measured() -> dict[str, Any]:
                prepare()
                before = server.metrics()
                sample = self.measure(
                    self.octa_command(consumer, consumer_profile),
                    consumer.workspace,
                    expected_hit_layer="remote",
                )
                after = server.metrics()
                sample.update(metric_delta(before, after))
                if (sample.get("cache") or {}).get("status") != "hit":
                    raise RuntimeError(f"{name} expected a remote cache hit")
                return sample

            for _ in range(self.scale.warmups):
                measured()
            samples = [measured() for _ in range(self.scale.runs)]
            self.record(consumer, {"current-warm": samples}, latency_ms=latency_ms, publication=publication)

    def benchmark_snapshot_kernel(self, large_input: Fixture) -> None:
        snapshot = []
        command = [str(self.args.kernel.resolve()), "snapshot", str(large_input.workspace), "inputs/**/*"]
        for _ in range(self.scale.warmups):
            self.measure(command, large_input.workspace, capture_json=True)
        for _ in range(self.scale.runs):
            snapshot.append(self.measure(command, large_input.workspace, capture_json=True))
        self._append_measurements(large_input.name, "kernel-snapshot", snapshot)

    def benchmark_roundtrip_kernel(self, fixture: Fixture) -> None:
        source = fixture.workspace / "inputs" / "payload.bin"
        output = fixture.workspace / "outputs" / "payload.bin"
        output.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, output)
        samples = []
        for index in range(self.scale.warmups + self.scale.runs):
            bundle = self.scratch / "kernel" / fixture.name / f"bundle-{index}.bin"
            staging = self.scratch / "kernel" / fixture.name / f"staging-{index}"
            bundle.parent.mkdir(parents=True, exist_ok=True)
            command = [str(self.args.kernel.resolve()), "roundtrip", str(fixture.workspace), "outputs", str(bundle), str(staging)]
            try:
                sample = self.measure(command, fixture.workspace, capture_json=True)
            finally:
                remove_path(staging)
                remove_path(bundle)
            if index >= self.scale.warmups:
                samples.append(sample)
        self._append_measurements(fixture.name, "kernel-roundtrip", samples)

    def _append_measurements(self, scenario: str, label: str, samples: list[dict[str, Any]]) -> None:
        path = self.output / f"{scenario}.json"
        document = json.loads(path.read_text(encoding="utf-8"))
        document["measurements"][label] = samples
        write_json_atomic(path, document)

    def benchmark_plugin_contract(self) -> None:
        fixture = plugin_contract_fixture(self.fixtures)
        cache = self.caches / fixture.name
        measurements = {
            "current-cold": self.local_cache_samples(fixture, cache, False),
            "current-warm": self.local_cache_samples(fixture, cache, True),
        }
        no_cache_path = self.fixtures / "automatic-plugin-contract-no-cache"
        clone_workspace(fixture.workspace, no_cache_path)
        octafile = no_cache_path / "Octafile.yml"
        octafile.write_text(octafile.read_text(encoding="utf-8").replace("    cache: {}\n", ""), encoding="utf-8")
        no_cache = Fixture(fixture.name, no_cache_path, fixture.task, fixture.logical_bytes, None)
        measurements["current-nocache"] = self.samples(lambda: self.octa_command(no_cache), no_cache, lambda: None)
        self.record(fixture, measurements)

    def benchmark_compiler(self) -> None:
        fixture = compiler_fixture(self.fixtures)
        cache = self.caches / fixture.name
        measurements = {
            "current-cold": self.local_cache_samples(fixture, cache, False),
            "current-warm": self.local_cache_samples(fixture, cache, True),
        }
        tool_path = self.fixtures / "compiler-tool-cache-local"
        clone_workspace(fixture.workspace, tool_path)
        octafile = tool_path / "Octafile.yml"
        octafile.write_text("version: 1\ntasks:\n  build:\n    shell: cargo build --release --target-dir build\n", encoding="utf-8")
        tool = Fixture(fixture.name, tool_path, "build", fixture.logical_bytes, tool_path / "build")
        measurements["local-tool-cold"] = self.samples(
            lambda: self.octa_command(tool), tool, lambda: remove_path(tool.output)
        )

        def prime() -> None:
            remove_path(tool.output)
            self.measure(self.octa_command(tool), tool.workspace)

        measurements["local-tool-warm"] = self.samples(lambda: self.octa_command(tool), tool, lambda: None, prime=prime)
        self.record(
            fixture,
            measurements,
            distributed_tool_cache=False,
            note="Phase 7 was intentionally deferred; local Cargo reuse is recorded as a baseline only.",
        )

    def benchmark_concurrent_agents(self) -> None:
        producer = input_fixture(self.fixtures, "concurrent-producer", 1)
        name = "concurrent-remote-consumers"
        with RemoteServer(
            self.scratch / "servers" / name,
            0,
            self.args.token,
            self.args.certificate,
            self.args.ca_certificate,
            self.args.key,
        ) as server:
            protocol = runner_protocol(self.args.runner)
            producer_cache = self.caches / "concurrent-producer"
            request = runner_request(protocol, producer, producer_cache, self.args, server.configuration())
            remove_path(producer.output)
            remove_path(producer_cache)
            publication_before = server.metrics()
            publication = self.measure([str(self.args.runner.resolve())], producer.workspace, stdin=request)
            publication.update(metric_delta(publication_before, server.metrics()))

            def group(iteration: int) -> dict[str, Any]:
                root = self.fixtures / f"concurrent-group-{iteration}"
                remove_path(root)
                roots = []
                for index in range(self.args.consumers):
                    workspace = root / f"agent-{index}"
                    clone_workspace(producer.workspace, workspace)
                    fixture = Fixture(name, workspace, producer.task, producer.logical_bytes, workspace / "outputs")
                    local = self.caches / f"concurrent-{iteration}-{index}"
                    remove_path(local)
                    roots.append((fixture, local))
                before = server.metrics()
                started = time.perf_counter_ns()
                with ThreadPoolExecutor(max_workers=self.args.consumers) as pool:
                    samples = list(
                        pool.map(
                            lambda pair: self.measure(
                                [str(self.args.runner.resolve())],
                                pair[0].workspace,
                                stdin=runner_request(protocol, pair[0], pair[1], self.args, server.configuration()),
                            ),
                            roots,
                        )
                    )
                elapsed = (time.perf_counter_ns() - started) / 1_000_000
                after = server.metrics()
                if not all((sample.get("cache") or {}).get("status") == "hit" for sample in samples):
                    raise RuntimeError("a concurrent runner did not receive the seeded remote action")
                result = {
                    "wall_ms": elapsed,
                    "cpu_ms": sum(sample["cpu_ms"] for sample in samples),
                    "peak_rss_bytes": sum(sample["peak_rss_bytes"] for sample in samples),
                    "filesystem_operations": sum(sample["filesystem_operations"] for sample in samples),
                    "cache": {"status": "hit", "layer": "remote"},
                    "agents": len(samples),
                }
                result.update(metric_delta(before, after))
                return result

            for index in range(self.scale.warmups):
                group(-index - 1)
            samples = [group(index) for index in range(self.scale.runs)]
            fixture = Fixture(name, producer.workspace, producer.task, producer.logical_bytes, None)
            self.record(fixture, {"current-warm": samples}, consumers=self.args.consumers, publication=publication)

    def _actual_scale(self, name: str) -> int | None:
        return {
            "inputs-1": 1,
            "inputs-1000": self.scale.inputs_medium,
            "inputs-100000": self.scale.inputs_large,
            "outputs-10000": self.scale.output_files,
            "large-input": self.scale.large_bytes,
            "large-output-compressible": self.scale.large_bytes,
            "large-output-incompressible": self.scale.large_bytes,
        }.get(name)


def metric_delta(before: dict[str, Any], after: dict[str, Any]) -> dict[str, Any]:
    routes = {
        route: int(after["routes"].get(route, 0) - before["routes"].get(route, 0))
        for route in set(before["routes"]) | set(after["routes"])
        if after["routes"].get(route, 0) != before["routes"].get(route, 0)
    }
    return {
        "http_requests": int(after["requests"] - before["requests"]),
        "bytes_uploaded": int(
            after["blob_bytes_uploaded"]
            + after["metadata_bytes_uploaded"]
            - before["blob_bytes_uploaded"]
            - before["metadata_bytes_uploaded"]
        ),
        "bytes_downloaded": int(
            after["blob_bytes_downloaded"]
            + after["metadata_bytes_downloaded"]
            - before["blob_bytes_downloaded"]
            - before["metadata_bytes_downloaded"]
        ),
        "http_routes": routes,
    }


def runner_protocol(binary: Path) -> int:
    capabilities = json.loads(subprocess.run([str(binary.resolve()), "capabilities"], check=True, text=True, stdout=subprocess.PIPE).stdout)
    return max(capabilities["runner_protocols"])


def runner_request(
    protocol: int,
    fixture: Fixture,
    local_cache: Path,
    args: argparse.Namespace,
    remote: dict[str, str],
) -> str:
    system = {"Darwin": "macos", "Windows": "windows"}.get(platform.system(), "linux")
    architecture = "arm64" if platform.machine().lower() in {"arm64", "aarch64"} else "amd64"
    request = {
        "type": "start",
        "protocol_version": protocol,
        "request_id": uuid.uuid4().hex,
        "request": {
            "workspace": str(fixture.workspace.resolve()),
            "data_dir": str((fixture.workspace / ".runner-data").resolve()),
            "plugins_dir": str(args.plugins.resolve()),
            "commands": [fixture.task],
            "quiet": True,
            "silence": True,
            "cache": {
                "mode": "read_write",
                "namespace": "benchmarks/cache-v1",
                "local_directory": str(local_cache.resolve()),
                "runtime": {
                    "kind": "native",
                    "os": system,
                    "architecture": architecture,
                    "environment": {"algorithm": "blake3", "hash": "a" * 64, "size_bytes": 24},
                },
                "remote": {
                    "endpoint": remote["endpoint"],
                    "token_file": remote["token_file"],
                    "ca_certificate_file": remote["ca_certificate_file"],
                    "request_timeout_seconds": 120,
                    "max_parallel_transfers": 8,
                },
            },
        },
    }
    return json.dumps(request, separators=(",", ":")) + "\n"


def version(command: list[str]) -> str:
    return subprocess.run(command, check=True, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT).stdout.strip()


def file_sha256(path: Path) -> str:
    """Identify the exact executable bytes used by an independently resumable run."""

    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def directory_sha256(path: Path) -> str:
    """Identify direct plugin-distribution files without hashing build internals."""

    digest = hashlib.sha256()
    for entry in sorted(candidate for candidate in path.iterdir() if candidate.is_file() and candidate.suffix != ".d"):
        relative = entry.relative_to(path).as_posix().encode("utf-8")
        digest.update(len(relative).to_bytes(4, "big"))
        digest.update(relative)
        digest.update(bytes.fromhex(file_sha256(entry)))
    return digest.hexdigest()


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--octa", type=Path, required=True)
    parser.add_argument("--runner", type=Path, required=True)
    parser.add_argument("--kernel", type=Path, required=True)
    parser.add_argument("--plugins", type=Path, required=True)
    parser.add_argument("--task", type=Path, default=Path(shutil.which("task") or "task"))
    parser.add_argument("--baseline-octa", type=Path)
    parser.add_argument("--baseline-plugins", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--quick", action="store_true")
    parser.add_argument("--resume", action="store_true")
    parser.add_argument("--runs", type=int)
    parser.add_argument("--warmups", type=int)
    parser.add_argument("--hash-budget", type=int, default=max(1, min(os.cpu_count() or 1, 8)))
    parser.add_argument("--consumers", type=int, default=8)
    parser.add_argument("--strace", type=Path, default=Path(shutil.which("strace")) if shutil.which("strace") else None)
    parser.add_argument("--scenarios", type=lambda value: set(value.split(",")))
    parser.add_argument("--scratch", type=Path)
    parser.add_argument("--token", type=Path)
    parser.add_argument("--certificate", type=Path, default=REPOSITORY / "crates/octa-runner/tests/fixtures/cache-cert.pem")
    parser.add_argument("--ca-certificate", type=Path, default=REPOSITORY / "crates/octa-runner/tests/fixtures/cache-ca.pem")
    parser.add_argument("--key", type=Path, default=REPOSITORY / "crates/octa-runner/tests/fixtures/cache-key.pem")
    args = parser.parse_args()
    for path in [args.octa, args.runner, args.kernel, args.plugins, args.task, args.certificate, args.ca_certificate, args.key]:
        if not path.exists():
            parser.error(f"required path does not exist: {path}")
    if args.baseline_octa and not args.baseline_octa.exists():
        parser.error(f"baseline binary does not exist: {args.baseline_octa}")
    if bool(args.baseline_octa) != bool(args.baseline_plugins):
        parser.error("--baseline-octa and --baseline-plugins must be provided together")
    if args.baseline_plugins and not args.baseline_plugins.is_dir():
        parser.error(f"baseline plugin directory does not exist: {args.baseline_plugins}")
    if args.strace and not args.strace.is_file():
        parser.error(f"strace binary does not exist: {args.strace}")
    if args.scenarios:
        unknown = args.scenarios.difference(PHASE_10_SCENARIOS)
        if unknown:
            parser.error(f"unknown scenarios: {', '.join(sorted(unknown))}")
    if args.hash_budget < 1 or args.consumers < 1:
        parser.error("hash budget and consumer count must be positive")
    return args


def main() -> None:
    args = parse_arguments()
    scale = Scale.quick() if args.quick else Scale.release()
    if args.runs:
        scale = Scale(scale.inputs_medium, scale.inputs_large, scale.output_files, scale.large_bytes, args.runs, scale.warmups)
    if args.warmups is not None:
        scale = Scale(scale.inputs_medium, scale.inputs_large, scale.output_files, scale.large_bytes, scale.runs, args.warmups)

    managed_results = (
        list(args.output.glob("*.json")) + [path for path in (args.output / "report.md",) if path.exists()]
        if args.output.exists()
        else []
    )
    if managed_results and not args.resume:
        raise SystemExit(f"output directory already contains benchmark results: {args.output}; use --resume or a new directory")
    args.output.mkdir(parents=True, exist_ok=True)

    run_identity = {
        "mode": "quick" if args.quick else "release",
        "runs": scale.runs,
        "warmups": scale.warmups,
        "actual_scale": {
            "inputs_medium": scale.inputs_medium,
            "inputs_large": scale.inputs_large,
            "output_files": scale.output_files,
            "large_bytes": scale.large_bytes,
        },
        "host": {
            "system": platform.platform(),
            "machine": platform.machine(),
            "processor": platform.processor(),
            "python": platform.python_version(),
        },
        "hash_budget": args.hash_budget,
        "consumers": args.consumers,
        "executables": {
            "octa_sha256": file_sha256(args.octa.resolve()),
            "runner_sha256": file_sha256(args.runner.resolve()),
            "kernel_sha256": file_sha256(args.kernel.resolve()),
            "plugins_sha256": directory_sha256(args.plugins.resolve()),
            "baseline_octa_sha256": file_sha256(args.baseline_octa.resolve()) if args.baseline_octa else None,
            "baseline_plugins_sha256": directory_sha256(args.baseline_plugins.resolve()) if args.baseline_plugins else None,
            "go_task_sha256": file_sha256(args.task.resolve()),
            "strace_sha256": file_sha256(args.strace.resolve()) if args.strace else None,
        },
        "fixtures": {
            "thresholds_sha256": file_sha256(HERE / "thresholds.json"),
            "certificate_sha256": file_sha256(args.certificate.resolve()),
            "ca_certificate_sha256": file_sha256(args.ca_certificate.resolve()),
            "key_sha256": file_sha256(args.key.resolve()),
        },
    }
    identity_path = args.output / "run-identity.json"
    metadata_path = args.output / "metadata.json"
    if args.resume:
        if not identity_path.exists():
            raise SystemExit("cannot resume benchmark results without run-identity.json")
        previous = json.loads(identity_path.read_text(encoding="utf-8"))
        mismatches = changed_identity_fields(run_identity, previous)
        if mismatches:
            raise SystemExit(f"cannot resume with changed benchmark identity fields: {', '.join(mismatches)}")
    else:
        write_json_atomic(identity_path, run_identity)

    managed_scratch = args.scratch is None
    scratch = Path(tempfile.mkdtemp(prefix="octa-cache-bench-")) if managed_scratch else args.scratch.resolve()
    scratch.mkdir(parents=True, exist_ok=True)
    if args.token is None:
        args.token = scratch / "cache-token"
        args.token.write_text("octa-cache-benchmark-token\n", encoding="utf-8")
        args.token.chmod(0o600)
    harness = Harness(args, scratch, scale)
    started = datetime.now(timezone.utc)

    try:
        local_fixtures: dict[str, Fixture] = {}
        if harness.selected("non-cache-noop") and not harness.completed("non-cache-noop"):
            harness.benchmark_no_cache()
        for name, count in [("inputs-1", 1), ("inputs-1000", scale.inputs_medium), ("inputs-100000", scale.inputs_large)]:
            if harness.selected(name) and not harness.completed(name):
                local_fixtures[name] = input_fixture(harness.fixtures, name, count)
                harness.benchmark_local_fixture(local_fixtures[name])
        if harness.selected("large-input") and not harness.completed("large-input"):
            fixture = large_input_fixture(harness.fixtures, scale.large_bytes)
            local_fixtures[fixture.name] = fixture
            if not harness.recorded_without_kernel(fixture.name):
                harness.benchmark_local_fixture(fixture)
            harness.benchmark_snapshot_kernel(fixture)
        if harness.selected("outputs-10000") and not harness.completed("outputs-10000"):
            fixture = output_fixture(harness.fixtures, "outputs-10000", scale.output_files)
            harness.benchmark_local_fixture(fixture)
        for name, compressible in [("large-output-compressible", True), ("large-output-incompressible", False)]:
            if harness.selected(name) and not harness.completed(name):
                fixture = large_output_fixture(harness.fixtures, name, scale.large_bytes, compressible)
                local_fixtures[name] = fixture
                if not harness.recorded_without_kernel(name):
                    harness.benchmark_local_fixture(fixture)
                harness.benchmark_roundtrip_kernel(fixture)
        for name, latency in [("remote-loopback", 0), ("remote-latency-20ms", 20), ("remote-latency-80ms", 80)]:
            if harness.selected(name) and not harness.completed(name):
                harness.benchmark_remote(name, latency)
        if harness.selected("cross-workspace-hit") and not harness.completed("cross-workspace-hit"):
            harness.benchmark_cross_workspace()
        if harness.selected("parallel-cacheable-25") and not harness.completed("parallel-cacheable-25"):
            harness.benchmark_local_fixture(parallel_fixture(harness.fixtures, "parallel-cacheable-25", False), False)
        if harness.selected("overlapping-inputs-25") and not harness.completed("overlapping-inputs-25"):
            harness.benchmark_local_fixture(parallel_fixture(harness.fixtures, "overlapping-inputs-25", True), False)
        if harness.selected("automatic-plugin-contract") and not harness.completed("automatic-plugin-contract"):
            harness.benchmark_plugin_contract()
        if harness.selected("compiler-task-cache") and not harness.completed("compiler-task-cache"):
            harness.benchmark_compiler()
        if harness.selected("concurrent-remote-consumers") and not harness.completed("concurrent-remote-consumers"):
            harness.benchmark_concurrent_agents()

    finally:
        metadata = {
            "schema_version": RESULT_SCHEMA_VERSION,
            "generated_at": datetime.now(timezone.utc).isoformat(),
            "started_at": started.isoformat(),
            "git_commit": version(["git", "rev-parse", "HEAD"]),
            "git_dirty": bool(version(["git", "status", "--porcelain"])),
            "system": platform.platform(),
            "machine": platform.machine(),
            "processor": platform.processor(),
            "python": platform.python_version(),
            "octa": version([str(args.octa.resolve()), "--version"]),
            "runner_capabilities": json.loads(version([str(args.runner.resolve()), "capabilities"])),
            "go_task": version([str(args.task.resolve()), "--version"]),
            "baseline_octa": version([str(args.baseline_octa.resolve()), "--version"]) if args.baseline_octa else None,
            "baseline_plugins": str(args.baseline_plugins.resolve()) if args.baseline_plugins else None,
            **run_identity,
            "rustc": version(["rustc", "--version", "--verbose"]),
            "cargo": version(["cargo", "--version", "--verbose"]),
        }
        write_json_atomic(metadata_path, metadata)
        if managed_scratch:
            shutil.rmtree(scratch, ignore_errors=True)


if __name__ == "__main__":
    main()
