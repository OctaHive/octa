#!/usr/bin/env python3
"""Black-box Agent Ready contract test. Imports no Octa implementation crates."""

from __future__ import annotations

import argparse
import hashlib
import json
import platform
import shutil
import subprocess
import tempfile
from pathlib import Path


def runtime_platform() -> str:
    system = {"Darwin": "macos", "Linux": "linux", "Windows": "windows"}[platform.system()]
    machine = platform.machine().lower()
    architecture = "aarch64" if machine in {"arm64", "aarch64"} else "x86_64"
    return f"{system}-{architecture}"


def executable(name: str) -> str:
    return f"{name}.exe" if platform.system() == "Windows" else name


def digest(path: Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def write_locked_plugins(source: Path, destination: Path) -> Path:
    destination.mkdir()
    entries = []
    for name, capabilities in [("shell", ["shell"]), ("tpl", [])]:
        entrypoint = executable(f"octa_plugin_{name}")
        shutil.copy2(source / entrypoint, destination / entrypoint)
        sha256 = digest(destination / entrypoint)
        manifest = [
            "manifest_version: 1",
            f"name: {name}",
            'version: "conformance"',
            "protocol: 1",
            f"platforms: [{runtime_platform()}]",
            f"entrypoint: {entrypoint}",
            f"sha256: {sha256}",
        ]
        if capabilities:
            manifest.append(f"capabilities: [{', '.join(capabilities)}]")
        (destination / f"{name}.plugin.yml").write_text("\n".join(manifest) + "\n", encoding="utf-8")
        entries.extend(
            [
                f"  {name}:",
                '    version: "conformance"',
                "    protocol: 1",
                f"    platforms: [{runtime_platform()}]",
                f"    entrypoint: {entrypoint}",
                f"    sha256: {sha256}",
            ]
        )
        if capabilities:
            entries.append(f"    capabilities: [{', '.join(capabilities)}]")
        entries.append(f"    source: {name}.plugin.yml")
    lock = destination.parent / "Octa.lock"
    lock.write_text("version: 1\nplugins:\n" + "\n".join(entries) + "\n", encoding="utf-8")
    return lock


def messages(output: str) -> list[dict]:
    return [json.loads(line) for line in output.splitlines()]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--runner", type=Path, required=True)
    parser.add_argument("--plugins", type=Path, required=True)
    args = parser.parse_args()
    runner = args.runner.resolve()
    plugins = args.plugins.resolve()

    capabilities = json.loads(subprocess.run([runner, "capabilities"], check=True, text=True, stdout=subprocess.PIPE).stdout)
    assert capabilities["runner_protocols"] == [1]
    assert capabilities["event_schemas"] == [3]
    assert capabilities["plugin_protocols"] == [1]
    assert {"artifacts", "reports", "locked-plugins", "secret-providers"} <= set(capabilities["features"])

    with tempfile.TemporaryDirectory(prefix="octa-agent-conformance-") as temporary:
        root = Path(temporary) / "workspace with spaces-данные"
        root.mkdir()
        secret = "agent-conformance-secret"
        (root / "private").mkdir()
        (root / "private" / "token").write_text(secret + "\n", encoding="utf-8")
        (root / "secrets.yml").write_text(
            "version: 1\nproviders:\n  application:\n    type: file\n    root: private\n",
            encoding="utf-8",
        )
        (root / "Octafile.yml").write_text(
            """version: 1
vars:
  TOKEN:
    secret:
      provider: application
      key: token
tasks:
  ci:
    shell: mkdir -p dist reports && printf binary > dist/app && printf '<testsuite/>' > reports/junit.xml && echo "{{ TOKEN }}"
    artifacts:
      - name: application
        path: dist
    reports:
      - name: tests
        path: reports/junit.xml
        format: junit
""",
            encoding="utf-8",
        )
        locked_plugins = root / "locked-plugins"
        lock = write_locked_plugins(plugins, locked_plugins)
        start = {
            "type": "start",
            "protocol_version": 1,
            "request_id": "conformance",
            "request": {
                "workspace": str(root),
                "data_dir": str(root / ".octa"),
                "plugins_dir": str(locked_plugins),
                "plugin_lock": str(lock),
                "secrets_profile": "secrets.yml",
                "commands": ["ci"],
            },
        }
        completed = subprocess.run(
            [runner],
            input=json.dumps(start) + "\n",
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        assert completed.returncode == 0, completed.stderr
        assert secret not in completed.stdout
        output = messages(completed.stdout)
        assert output[0]["type"] == "hello"
        assert output[1] == {"type": "accepted", "request_id": "conformance"}
        events = [message["event"] for message in output if message["type"] == "event"]
        assert [event["sequence"] for event in events] == list(range(events[0]["sequence"], events[0]["sequence"] + len(events)))
        assert any(event["data"].get("type") == "artifact_registered" for event in events)
        assert any(event["data"].get("type") == "report_registered" for event in events)
        finished = output[-1]
        assert finished["type"] == "finished" and finished["status"] == "succeeded"
        assert "*****" in json.dumps(finished["results"][0]["stdout"])
        assert finished["results"][0]["tasks"][0]["artifacts"][0]["path"] == "dist"
        assert finished["results"][0]["tasks"][0]["reports"][0]["format"] == "junit"


if __name__ == "__main__":
    main()
