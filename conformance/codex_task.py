#!/usr/bin/env python3
"""Exercise one Codex Octafile through the local CLI and public runner."""

from __future__ import annotations

import argparse
import json
import shutil
import subprocess
import tempfile
from pathlib import Path

from codex_support import PROCESS_TIMEOUT_SECONDS, decode_messages, install_codex_fixture, run_environment, runner_request


FIXTURE_DIRECTORY = Path(__file__).resolve().parent / "fixtures" / "codex_task"


def prepare_workspace(parent: Path, name: str) -> Path:
    """Copies the shared Octafile and local plugin selection into a workspace."""
    workspace = parent / name
    shutil.copytree(FIXTURE_DIRECTORY, workspace)
    return workspace


def assert_gate(workspace: Path) -> None:
    marker = workspace / "semantic-outcome.verified"
    assert marker.read_text(encoding="utf-8") == "verified"


def run_cli(octa: Path, workspace: Path, environment: dict[str, str]) -> None:
    completed = subprocess.run(
        [octa, "--config", "octa-config.yml", "--output", "jsonl", "verify-outcome"],
        cwd=workspace,
        env=environment,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=PROCESS_TIMEOUT_SECONDS,
    )
    assert completed.returncode == 0, f"stdout={completed.stdout}\nstderr={completed.stderr}"
    assert_gate(workspace)


def runner_messages(runner: Path, workspace: Path, plugins: Path, environment: dict[str, str]) -> list[dict]:
    request = runner_request("codex-conformance", workspace, plugins, ["verify-outcome"])
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
    assert messages[0]["type"] == "hello"
    assert messages[1] == {"type": "accepted", "request_id": "codex-conformance"}
    finished = messages[-1]
    assert finished["type"] == "finished" and finished["status"] == "succeeded"

    events = [message["event"] for message in messages if message["type"] == "event"]
    event_types = [event["data"].get("type") for event in events]
    assert "artifact_registered" in event_types
    assert "report_registered" in event_types
    assert not any((event_type or "").startswith("codex") for event_type in event_types)

    tasks = finished["results"][0]["tasks"]
    codex_task = next(task for task in tasks if task["label"] == "codex-run")
    assert codex_task["outputs"]["outcome"] == "failed"
    codex_step = codex_task["steps"][0]
    assert codex_step["outputs"]["outcome"] == "failed"
    assert {artifact["name"] for artifact in codex_step["artifacts"]} == {
        "codex-run-provenance",
        "codex-run-trace",
    }
    assert [(report["name"], report["format"]) for report in codex_step["reports"]] == [
        ("codex-run-result", "octa.codex.result.v1")
    ]
    for path in codex_step["outputs"]["record_paths"].values():
        assert (workspace / path).is_file(), f"missing Codex run record {path}"
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
        codex = install_codex_fixture(fixture, root / "operator")
        environment = run_environment(plugins, codex)

        cli_workspace = prepare_workspace(root, "cli-workspace")
        run_cli(octa, cli_workspace, environment)

        runner_workspace = prepare_workspace(root, "runner-workspace")
        messages = runner_messages(runner, runner_workspace, plugins, environment)
        assert_generic_runner_contract(messages, runner_workspace)


if __name__ == "__main__":
    main()
