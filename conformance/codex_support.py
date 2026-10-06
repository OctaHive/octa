"""Shared process and wire helpers for the focused Codex conformance scenarios."""

from __future__ import annotations

import json
import os
import platform
import shutil
import stat
from pathlib import Path


RUNNER_PROTOCOL_VERSION = 3
PROCESS_TIMEOUT_SECONDS = 60


def executable(name: str) -> str:
    """Returns a native executable name for the current test platform."""
    return f"{name}.exe" if platform.system() == "Windows" else name


def install_codex_fixture(source: Path, destination: Path, mode: str = "failed-nonzero") -> Path:
    """Installs an operator-selected fake Codex CLI in one deterministic mode."""
    destination.mkdir()
    installed = destination / executable("codex")
    shutil.copy2(source, installed)
    if platform.system() != "Windows":
        installed.chmod(installed.stat().st_mode | stat.S_IXUSR)
    installed.with_suffix(".run-mode").write_text(f"{mode}\n", encoding="utf-8")
    return installed


def run_environment(plugins: Path, codex: Path) -> dict[str, str]:
    """Builds the operator environment shared by CLI and runner scenarios."""
    environment = os.environ.copy()
    environment["OCTA_PLUGINS_DIR"] = str(plugins)
    environment["OCTA_CODEX_EXECUTABLE"] = str(codex)
    return environment


def runner_request(
    request_id: str,
    workspace: Path,
    plugins: Path,
    commands: list[str],
    *,
    secrets_profile: str | None = None,
) -> dict:
    """Builds one public runner start request without importing Octa internals."""
    request = {
        "workspace": str(workspace),
        "data_dir": str(workspace / ".octa"),
        "plugins_dir": str(plugins),
        "plugins": ["codex"],
        "commands": commands,
    }
    if secrets_profile is not None:
        request["secrets_profile"] = secrets_profile
    return {
        "type": "start",
        "protocol_version": RUNNER_PROTOCOL_VERSION,
        "request_id": request_id,
        "request": request,
    }


def runner_start_frame(request_id: str, workspace: Path, plugins: Path, commands: list[str]) -> str:
    """Serializes one newline-delimited runner start frame."""
    return json.dumps(runner_request(request_id, workspace, plugins, commands)) + "\n"


def decode_messages(stdout: str | bytes) -> list[dict]:
    """Decodes newline-delimited runner messages from text or bytes."""
    return [json.loads(line) for line in stdout.splitlines()]


def terminal_message(messages: list[dict]) -> dict:
    """Returns the sole terminal response and rejects missing or duplicate terminals."""
    terminal = [message for message in messages if message["type"] == "finished"]
    assert len(terminal) == 1, f"expected one terminal message, got {len(terminal)}"
    return terminal[0]


def resource_events(messages: list[dict]) -> list[dict]:
    """Returns generic artifact and report events without Codex-specific coupling."""
    return [
        message
        for message in messages
        if message["type"] == "event"
        and message["event"]["data"].get("type") in {"artifact_registered", "report_registered"}
    ]


def record_files(workspace: Path) -> list[Path]:
    """Finds every stable or partial Codex run-record file in a workspace."""
    root = workspace / ".octa" / "codex-runs"
    return [] if not root.exists() else [path for path in root.rglob("*") if path.is_file()]
