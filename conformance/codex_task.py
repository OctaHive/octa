#!/usr/bin/env python3
"""Exercise the documented Codex examples through the CLI and public runner."""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import tempfile
from pathlib import Path

from codex_support import (
    PROCESS_TIMEOUT_SECONDS,
    decode_messages,
    install_codex_fixture,
    run_environment,
    runner_request,
)


EXAMPLES_DIRECTORY = Path(__file__).resolve().parents[1] / "example" / "codex"
STANDARD_EXECUTION_EVENTS = {
    "run_started",
    "run_finished",
    "scope_declared",
    "scope_started",
    "scope_finished",
    "step_declared",
    "step_started",
    "step_finished",
    "output",
    "progress",
    "artifact_registered",
    "report_registered",
}


def prepare_workspace(parent: Path, name: str, example: str) -> Path:
    """Copies one user-facing example into an isolated conformance workspace."""
    workspace = parent / name
    shutil.copytree(EXAMPLES_DIRECTORY / example, workspace)
    return workspace


def prepare_agent_secrets(workspace: Path) -> None:
    """Supplies deployment-owned authentication without changing the Octafile."""
    private = workspace / "private"
    private.mkdir()
    (private / "codex-auth").write_text("fixture-authentication\n", encoding="utf-8")
    (workspace / "secrets.yml").write_text(
        """version: 1
providers:
  authentication:
    type: file
    root: private
""",
        encoding="utf-8",
    )


def assert_gate(workspace: Path) -> None:
    marker = workspace / "semantic-outcome.verified"
    assert marker.read_text(encoding="utf-8") == "verified"


def run_cli(octa: Path, workspace: Path, environment: dict[str, str]) -> None:
    completed = subprocess.run(
        [octa, "--config", "octa-config.yml", "--output", "jsonl", "verify-review"],
        cwd=workspace,
        env=environment,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=PROCESS_TIMEOUT_SECONDS,
    )
    assert completed.returncode == 0, f"stdout={completed.stdout}\nstderr={completed.stderr}"
    assert (workspace / "out" / "review.md").is_file()
    assert_gate(workspace)


def runner_messages(
    runner: Path,
    workspace: Path,
    plugins: Path,
    environment: dict[str, str],
) -> list[dict]:
    request = runner_request(
        "codex-conformance",
        workspace,
        plugins,
        ["verify-implementation"],
        secrets_profile="secrets.yml",
    )
    completed = subprocess.run(
        [runner],
        input=json.dumps(request) + "\n",
        env=environment,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=PROCESS_TIMEOUT_SECONDS,
    )
    assert completed.returncode == 0, f"stdout={completed.stdout}\nstderr={completed.stderr}"
    return decode_messages(completed.stdout)


def assert_generic_runner_contract(messages: list[dict], workspace: Path) -> None:
    """Checks only the generic runner vocabulary consumed by any agent."""
    assert {message["type"] for message in messages} == {
        "hello",
        "accepted",
        "event",
        "finished",
    }
    assert messages[0]["type"] == "hello"
    assert messages[1] == {"type": "accepted", "request_id": "codex-conformance"}
    finished = messages[-1]
    assert finished["type"] == "finished" and finished["status"] == "succeeded"

    events = [message["event"] for message in messages if message["type"] == "event"]
    assert all(event["category"] in {"diagnostic", "execution"} for event in events)
    event_types = [event["data"].get("type") for event in events]
    assert {event_type for event_type in event_types if event_type} <= STANDARD_EXECUTION_EVENTS
    assert "artifact_registered" in event_types
    assert "report_registered" in event_types

    expected_artifacts = {"codex-run-provenance", "codex-run-trace", "proposed-patch"}
    expected_reports = {
        ("codex-run-result", "octa.codex.result.v1"),
        ("implementation-summary", "codex.implementation.v1"),
    }
    assert {
        event["data"]["artifact"]["name"]
        for event in events
        if event["data"].get("type") == "artifact_registered"
    } == expected_artifacts
    assert {
        (event["data"]["report"]["name"], event["data"]["report"]["format"])
        for event in events
        if event["data"].get("type") == "report_registered"
    } == expected_reports

    tasks = finished["results"][0]["tasks"]
    codex_task = next(task for task in tasks if task["label"] == "implement")
    assert codex_task["outputs"]["outcome"] == "completed"
    codex_step = codex_task["steps"][0]
    assert codex_step["outputs"]["outcome"] == "completed"
    assert codex_step["outputs"]["structured_result"] == {"outcome": "completed", "files": 2}
    assert {artifact["name"] for artifact in codex_step["artifacts"]} == expected_artifacts
    assert {(report["name"], report["format"]) for report in codex_step["reports"]} == expected_reports
    for path in codex_step["outputs"]["record_paths"].values():
        assert (workspace / path).is_file(), f"missing Codex run record {path}"
    assert (workspace / "out" / "change.patch").is_file()
    assert (workspace / "out" / "summary.json").is_file()
    assert_gate(workspace)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--octa", type=Path, required=True)
    parser.add_argument("--runner", type=Path, required=True)
    parser.add_argument("--plugins", type=Path, required=True)
    parser.add_argument("--codex-fixture", type=Path, required=True)
    args = parser.parse_args()

    octa = args.octa.resolve()
    runner = args.runner.resolve()
    plugins = args.plugins.resolve()
    fixture = args.codex_fixture.resolve()
    with tempfile.TemporaryDirectory(prefix="octa-codex-conformance-") as temporary:
        root = Path(temporary)
        local_codex = install_codex_fixture(fixture, root / "local-operator", "example-local")
        cli_workspace = prepare_workspace(root, "cli-workspace", "local")
        run_cli(octa, cli_workspace, run_environment(plugins, local_codex))

        agent_codex = install_codex_fixture(fixture, root / "agent-operator", "example-agent")
        runner_workspace = prepare_workspace(root, "runner-workspace", "agent")
        prepare_agent_secrets(runner_workspace)
        messages = runner_messages(
            runner,
            runner_workspace,
            plugins,
            run_environment(plugins, agent_codex),
        )
        assert_generic_runner_contract(messages, runner_workspace)


if __name__ == "__main__":
    main()
