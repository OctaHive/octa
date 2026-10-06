#!/usr/bin/env python3
"""Prove Codex authentication values stay inside the selected child environment."""

from __future__ import annotations

import argparse
import json
import secrets
import subprocess
import tempfile
from pathlib import Path

from codex_support import (
    PROCESS_TIMEOUT_SECONDS,
    decode_messages,
    install_codex_fixture,
    record_files,
    run_environment,
    runner_request,
)


# Both grouped CLI output and runner terminal capture spill after 1 MiB. The
# fixture emits a bounded payload above this threshold and below record limits.
DISK_SPILL_THRESHOLD_BYTES = 1024 * 1024
REQUEST_ID = "codex-secret-conformance"


def prepare_workspace(parent: Path, name: str, secret: str) -> Path:
    """Creates one task whose only credential source is a file-provider profile."""
    workspace = parent / name
    private = workspace / "private"
    private.mkdir(parents=True)
    (private / "codex-auth").write_text(secret, encoding="utf-8")
    (workspace / "secrets.yml").write_text(
        """version: 1
providers:
  authentication:
    type: file
    root: private
""",
        encoding="utf-8",
    )
    (workspace / "octa-config.yml").write_text("plugins: [codex]\n", encoding="utf-8")
    (workspace / "Octafile.yml").write_text(
        """version: 1
vars:
  CODEX_AUTH:
    secret:
      provider: authentication
      key: codex-auth
tasks:
  codex-run:
    codex:
      prompt: Exercise secret-provider redaction through the Codex fixture.
      environment:
        secret:
          OCTA_CODEX_FIXTURE_SECRET: CODEX_AUTH
""",
        encoding="utf-8",
    )
    return workspace


def assert_secret_absent(secret: str, surfaces: dict[str, str | bytes]) -> None:
    """Fails without printing the credential or the possibly tainted surface."""
    encoded = secret.encode()
    for name, value in surfaces.items():
        payload = value if isinstance(value, bytes) else value.encode()
        if encoded in payload:
            raise AssertionError(f"secret leaked through {name}")


def run_cli(octa: Path, workspace: Path, environment: dict[str, str], secret: str) -> None:
    """Exercises the disk-backed grouped-output spool with redacted activity."""
    completed = subprocess.run(
        [
            octa,
            "--config",
            "octa-config.yml",
            "--secrets-profile",
            "secrets.yml",
            "--output",
            "group",
            "codex-run",
        ],
        cwd=workspace,
        env=environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=PROCESS_TIMEOUT_SECONDS,
    )
    if completed.returncode != 0:
        raise AssertionError(f"CLI secret conformance failed with code {completed.returncode}")
    if len(completed.stdout) <= DISK_SPILL_THRESHOLD_BYTES:
        raise AssertionError("CLI output did not cross the disk-spool threshold")
    assert b"*****" in completed.stdout + completed.stderr
    assert_secret_absent(secret, {"CLI stdout": completed.stdout, "CLI/plugin stderr": completed.stderr})


def run_runner(
    runner: Path,
    workspace: Path,
    plugins: Path,
    environment: dict[str, str],
    secret: str,
) -> None:
    """Exercises runner events and the disk-backed terminal output capture."""
    request = runner_request(REQUEST_ID, workspace, plugins, ["codex-run"], secrets_profile="secrets.yml")
    serialized_request = json.dumps(request)
    assert_secret_absent(secret, {"runner JobSpec fixture": serialized_request})
    completed = subprocess.run(
        [runner],
        input=(serialized_request + "\n").encode(),
        env=environment,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=PROCESS_TIMEOUT_SECONDS,
    )
    if completed.returncode != 0:
        raise AssertionError(f"runner secret conformance failed with code {completed.returncode}")
    # The SDK's persisted file logger is covered by its own host test; runner
    # conformance owns the separate plugin-process stderr capture.
    assert_secret_absent(
        secret,
        {"runner events/results": completed.stdout, "runner/plugin process stderr": completed.stderr},
    )

    messages = decode_messages(completed.stdout)
    terminal = messages[-1]
    assert terminal["type"] == "finished" and terminal["status"] == "succeeded"
    captured = json.dumps(terminal["results"][0]["stdout"], ensure_ascii=False).encode()
    if len(captured) <= DISK_SPILL_THRESHOLD_BYTES:
        raise AssertionError("runner output did not cross the disk-capture threshold")
    assert b"*****" in captured
    events = [message["event"] for message in messages if message["type"] == "event"]
    assert any(event["data"].get("type") == "artifact_registered" for event in events)
    assert any(event["data"].get("type") == "report_registered" for event in events)


def assert_records_are_sanitized(workspace: Path, secret: str) -> None:
    """Validates every stable run record without exposing its bytes on failure."""
    files = record_files(workspace)
    assert len(files) == 3, f"expected three stable run records, found {len(files)}"
    assert {path.name for path in files} == {"trace.jsonl", "result.json", "provenance.json"}
    for path in files:
        payload = path.read_bytes()
        assert_secret_absent(secret, {f"run record {path.name}": payload})
        if path.name == "trace.jsonl":
            for line in payload.splitlines():
                json.loads(line)
        else:
            json.loads(payload)

    state_root = workspace / ".octa"
    state_files = [path for path in state_root.rglob("*") if path.is_file()]
    assert_secret_absent(
        secret,
        {f"Octa state {path.relative_to(state_root)}": path.read_bytes() for path in state_files},
    )


def assert_reference_files_contain_no_value(workspace: Path, secret: str) -> None:
    """Checks portable task/profile fixtures contain only the logical reference."""
    surfaces = {
        path.name: path.read_bytes()
        for path in [workspace / "Octafile.yml", workspace / "secrets.yml", workspace / "octa-config.yml"]
    }
    assert_secret_absent(secret, surfaces)


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
    secret = f"octa-codex-auth-{secrets.token_hex(24)}"
    with tempfile.TemporaryDirectory(prefix="octa-codex-secret-") as temporary:
        root = Path(temporary)
        codex = install_codex_fixture(fixture, root / "operator", "secret-spill")
        environment = run_environment(plugins, codex)
        assert_secret_absent(
            secret,
            {f"operator environment variable {name}": value for name, value in environment.items()},
        )

        cli_workspace = prepare_workspace(root, "cli-workspace", secret)
        assert_reference_files_contain_no_value(cli_workspace, secret)
        run_cli(octa, cli_workspace, environment, secret)
        assert_records_are_sanitized(cli_workspace, secret)

        runner_workspace = prepare_workspace(root, "runner-workspace", secret)
        assert_reference_files_contain_no_value(runner_workspace, secret)
        run_runner(runner, runner_workspace, plugins, environment, secret)
        assert_records_are_sanitized(runner_workspace, secret)


if __name__ == "__main__":
    main()
