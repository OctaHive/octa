#!/usr/bin/env python3
"""Black-box failure and backpressure tests for Codex through octa-runner."""

from __future__ import annotations

import argparse
import json
import subprocess
import tempfile
import time
from pathlib import Path

from codex_support import (
    PROCESS_TIMEOUT_SECONDS,
    decode_messages,
    install_codex_fixture,
    record_files,
    resource_events,
    run_environment,
    runner_start_frame,
    terminal_message,
)


REQUEST_ID = "codex-runner-conformance"
MARKER_TIMEOUT_SECONDS = 15
HEARTBEAT_SETTLE_SECONDS = 0.15
SLOW_CONSUMER_DELAY_SECONDS = 0.25


def workspace_with(parent: Path, name: str, task: str) -> Path:
    """Creates one isolated runner workspace from a complete task mapping."""
    workspace = parent / name
    workspace.mkdir()
    (workspace / "Octafile.yml").write_text(f"version: 1\ntasks:\n{task}", encoding="utf-8")
    return workspace


def start_frame(workspace: Path, plugins: Path) -> str:
    return runner_start_frame(REQUEST_ID, workspace, plugins, ["codex-run"])


def start_runner(runner: Path, workspace: Path, plugins: Path, environment: dict[str, str]) -> subprocess.Popen:
    process = subprocess.Popen(
        [runner],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=environment,
    )
    assert process.stdin is not None
    process.stdin.write(start_frame(workspace, plugins))
    process.stdin.flush()
    return process


def collect(process: subprocess.Popen) -> tuple[int, str, str]:
    """Closes the control stream and collects a bounded runner execution."""
    if process.stdin is not None:
        process.stdin.close()
        process.stdin = None
    try:
        stdout, stderr = process.communicate(timeout=PROCESS_TIMEOUT_SECONDS)
    except subprocess.TimeoutExpired:
        process.kill()
        stdout, stderr = process.communicate()
        raise AssertionError(f"runner exceeded {PROCESS_TIMEOUT_SECONDS}s\nstdout={stdout}\nstderr={stderr}")
    return process.returncode, stdout, stderr


def terminate(process: subprocess.Popen) -> None:
    """Prevents a failed assertion from leaving a runner or plugin tree alive."""
    if process.poll() is None:
        process.kill()
        process.communicate()


def wait_for_marker(process: subprocess.Popen, marker: Path) -> None:
    deadline = time.monotonic() + MARKER_TIMEOUT_SECONDS
    while time.monotonic() < deadline:
        if marker.is_file():
            return
        if process.poll() is not None:
            code, stdout, stderr = collect(process)
            raise AssertionError(f"runner exited before {marker.name}: code={code}\nstdout={stdout}\nstderr={stderr}")
        time.sleep(0.01)
    raise AssertionError(f"runner did not create {marker.name} within {MARKER_TIMEOUT_SECONDS}s")


def assert_no_record_files(workspace: Path) -> None:
    files = record_files(workspace)
    assert not files, f"failure retained partial Codex records: {files}"


def assert_heartbeat_stopped(heartbeat: Path) -> None:
    before = heartbeat.read_text(encoding="utf-8")
    time.sleep(HEARTBEAT_SETTLE_SECONDS)
    after = heartbeat.read_text(encoding="utf-8")
    assert before == after, "Codex descendant kept running after runner termination"


def assert_failed_without_resources(code: int, stdout: str, stderr: str) -> list[dict]:
    assert code == 1, f"unexpected runner code {code}\nstdout={stdout}\nstderr={stderr}"
    messages = decode_messages(stdout)
    assert terminal_message(messages)["status"] == "failed"
    assert not resource_events(messages), f"failed invocation published resources: {resource_events(messages)}"
    return messages


def cancellation_stops_the_process_tree(root: Path, runner: Path, plugins: Path, fixture: Path) -> None:
    workspace = workspace_with(
        root,
        "cancel",
        """  codex-run:
    codex: { prompt: wait for cancellation }
""",
    )
    codex = install_codex_fixture(fixture, root / "cancel-operator", "descendant")
    heartbeat = codex.with_suffix(".run-descendant-heartbeat")
    process = start_runner(runner, workspace, plugins, run_environment(plugins, codex))
    try:
        wait_for_marker(process, heartbeat)
        assert process.stdin is not None
        process.stdin.write(json.dumps({"type": "cancel", "request_id": REQUEST_ID}) + "\n")
        process.stdin.flush()
        code, stdout, stderr = collect(process)
    finally:
        terminate(process)

    assert code == 130, f"unexpected cancellation code {code}\nstdout={stdout}\nstderr={stderr}"
    messages = decode_messages(stdout)
    assert terminal_message(messages)["status"] == "cancelled"
    assert not resource_events(messages)
    assert_no_record_files(workspace)
    assert_heartbeat_stopped(heartbeat)


def task_timeout_stops_the_process_tree(root: Path, runner: Path, plugins: Path, fixture: Path) -> None:
    workspace = workspace_with(
        root,
        "timeout",
        """  codex-run:
    timeout: 750ms
    codex: { prompt: wait for task timeout }
""",
    )
    codex = install_codex_fixture(fixture, root / "timeout-operator", "descendant")
    heartbeat = codex.with_suffix(".run-descendant-heartbeat")
    process = start_runner(runner, workspace, plugins, run_environment(plugins, codex))
    try:
        wait_for_marker(process, heartbeat)
        code, stdout, stderr = collect(process)
    finally:
        terminate(process)

    messages = assert_failed_without_resources(code, stdout, stderr)
    assert terminal_message(messages)["results"][0]["conclusion"]["failure"]["kind"] == "timeout"
    assert_no_record_files(workspace)
    assert_heartbeat_stopped(heartbeat)


def slow_consumer_fails_boundedly(root: Path, runner: Path, plugins: Path, fixture: Path) -> None:
    workspace = workspace_with(
        root,
        "slow-consumer",
        """  codex-run:
    codex: { prompt: produce bounded activity }
""",
    )
    codex = install_codex_fixture(fixture, root / "noisy-operator", "noisy")
    heartbeat = codex.with_suffix(".run-descendant-heartbeat")
    process = start_runner(runner, workspace, plugins, run_environment(plugins, codex))
    try:
        wait_for_marker(process, heartbeat)
        # Hold the runner pipe long enough for the fixture to exceed the
        # plugin client's private response queue. The command must fail rather
        # than consume unbounded memory or stall unrelated socket routes.
        time.sleep(SLOW_CONSUMER_DELAY_SECONDS)
        code, stdout, stderr = collect(process)
    finally:
        terminate(process)

    messages = assert_failed_without_resources(code, stdout, stderr)
    assert "produced output faster" in json.dumps(messages)
    assert_no_record_files(workspace)
    assert_heartbeat_stopped(heartbeat)


def trace_limit_removes_partial_records(root: Path, runner: Path, plugins: Path, fixture: Path) -> None:
    workspace = workspace_with(
        root,
        "trace-limit",
        """  codex-run:
    codex: { prompt: exceed the trace budget }
""",
    )
    codex = install_codex_fixture(fixture, root / "trace-operator", "trace-overflow")
    heartbeat = codex.with_suffix(".run-descendant-heartbeat")
    completed = subprocess.run(
        [runner],
        input=start_frame(workspace, plugins),
        env=run_environment(plugins, codex),
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=PROCESS_TIMEOUT_SECONDS,
    )
    messages = assert_failed_without_resources(
        completed.returncode,
        completed.stdout,
        completed.stderr,
    )
    assert "trace exceeds" in json.dumps(messages)
    assert_no_record_files(workspace)
    assert_heartbeat_stopped(heartbeat)


def invalid_report_path_publishes_nothing(root: Path, runner: Path, plugins: Path, fixture: Path) -> None:
    workspace = workspace_with(
        root,
        "invalid-report",
        """  codex-run:
    codex:
      prompt: validate the declared report
      deliverables:
        - kind: report
          name: invalid-directory-report
          path: invalid-report
          format: fixture
""",
    )
    (workspace / "invalid-report").mkdir()
    codex = install_codex_fixture(fixture, root / "resource-operator", "complete")
    completed = subprocess.run(
        [runner],
        input=start_frame(workspace, plugins),
        env=run_environment(plugins, codex),
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=PROCESS_TIMEOUT_SECONDS,
    )
    messages = assert_failed_without_resources(
        completed.returncode,
        completed.stdout,
        completed.stderr,
    )
    serialized_messages = json.dumps(messages).lower()
    assert "must be a regular file" in serialized_messages, serialized_messages

    records = list((workspace / ".octa" / "codex-runs").rglob("*"))
    files = [path for path in records if path.is_file()]
    assert {path.name for path in files} == {"trace.jsonl", "result.json", "provenance.json"}
    assert not any("staging" in path.parts or path.name.endswith(".tmp") for path in records)
    for path in files:
        if path.name == "trace.jsonl":
            for line in path.read_text(encoding="utf-8").splitlines():
                json.loads(line)
        else:
            json.loads(path.read_text(encoding="utf-8"))


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--runner", type=Path, required=True)
    parser.add_argument("--plugins", type=Path, required=True)
    parser.add_argument("--codex-fixture", type=Path, required=True)
    args = parser.parse_args()

    runner = args.runner.resolve()
    plugins = args.plugins.resolve()
    fixture = args.codex_fixture.resolve()
    with tempfile.TemporaryDirectory(prefix="octa-codex-runner-") as temporary:
        root = Path(temporary)
        cancellation_stops_the_process_tree(root, runner, plugins, fixture)
        task_timeout_stops_the_process_tree(root, runner, plugins, fixture)
        slow_consumer_fails_boundedly(root, runner, plugins, fixture)
        trace_limit_removes_partial_records(root, runner, plugins, fixture)
        invalid_report_path_publishes_nothing(root, runner, plugins, fixture)


if __name__ == "__main__":
    main()
