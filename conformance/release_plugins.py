#!/usr/bin/env python3
"""Black-box checks for the official plugin set in one release archive."""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import tempfile
from pathlib import Path

import codex_release_metadata


OFFICIAL_PLUGINS = ("codex", "junit", "shell", "tpl")
PROCESS_TIMEOUT_SECONDS = 60


def entrypoint(plugin: str, platform: str) -> str:
    """Returns the release filename selected by one runtime platform label."""
    suffix = ".exe" if platform.startswith("windows-") else ""
    return f"octa_plugin_{plugin}{suffix}"


def run_octa(octa: Path, plugins: Path, *arguments: str) -> subprocess.CompletedProcess[str]:
    """Runs the packaged Octa against an explicit packaged plugin directory."""
    environment = os.environ.copy()
    environment["OCTA_PLUGINS_DIR"] = str(plugins)
    return subprocess.run(
        [octa, *arguments],
        env=environment,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=PROCESS_TIMEOUT_SECONDS,
    )


def require_success(completed: subprocess.CompletedProcess[str], operation: str) -> None:
    """Reports bounded command diagnostics when release verification fails."""
    if completed.returncode != 0:
        raise AssertionError(
            f"{operation} failed with code {completed.returncode}\n"
            f"stdout={completed.stdout}\nstderr={completed.stderr}"
        )


def verify_release(
    octa: Path,
    plugins: Path,
    lock: Path,
    platform: str,
    codex_metadata: Path,
    codex_source: Path,
    plugin_version: str,
    plugin_protocol: int,
) -> None:
    """Validates manifest discovery, lock generation, and tamper rejection."""
    for plugin in OFFICIAL_PLUGINS:
        binary = plugins / entrypoint(plugin, platform)
        manifest = plugins / f"{plugin}.plugin.yml"
        assert binary.is_file(), f"release is missing {binary.name}"
        assert manifest.is_file(), f"release is missing {manifest.name}"

    require_success(
        run_octa(octa, plugins, "plugin", "verify", "--lock", str(lock)),
        "shipped plugin lock verification",
    )
    expected_metadata = codex_release_metadata.document(
        codex_source, plugin_version, plugin_protocol
    )
    assert codex_release_metadata.load(codex_metadata) == expected_metadata, (
        "Codex compatibility metadata does not match the packaged plugin"
    )

    with tempfile.TemporaryDirectory(prefix="octa-release-plugins-") as temporary:
        root = Path(temporary)
        copied_plugins = root / "plugins"
        shutil.copytree(plugins, copied_plugins)
        generated_lock = root / "Octa.lock"
        generated = run_octa(
            octa,
            copied_plugins,
            "plugin",
            "lock",
            "--output",
            str(generated_lock),
        )
        require_success(generated, "plugin lock generation")
        assert "Locked 4 plugins" in generated.stdout
        assert "codex:" in generated_lock.read_text(encoding="utf-8")
        require_success(
            run_octa(octa, copied_plugins, "plugin", "verify", "--lock", str(generated_lock)),
            "generated plugin lock verification",
        )

        # Any post-lock binary replacement must fail before a plugin process
        # starts. Appending avoids depending on executable formats or symbols.
        with (copied_plugins / entrypoint("codex", platform)).open("ab") as binary:
            binary.write(b"octa-release-tamper-test")
        rejected = run_octa(octa, copied_plugins, "plugin", "verify", "--lock", str(generated_lock))
        assert rejected.returncode != 0, "modified Codex binary passed lock verification"
        diagnostic = (rejected.stdout + rejected.stderr).lower()
        assert "codex" in diagnostic and "digest mismatch" in diagnostic, diagnostic


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--octa", type=Path, required=True)
    parser.add_argument("--plugins", type=Path, required=True)
    parser.add_argument("--lock", type=Path, required=True)
    parser.add_argument("--platform", required=True)
    parser.add_argument("--codex-metadata", type=Path, required=True)
    parser.add_argument("--codex-source", type=Path, required=True)
    parser.add_argument("--plugin-version", required=True)
    parser.add_argument("--plugin-protocol", type=int, required=True)
    args = parser.parse_args()
    verify_release(
        args.octa.resolve(),
        args.plugins.resolve(),
        args.lock.resolve(),
        args.platform,
        args.codex_metadata.resolve(),
        args.codex_source.resolve(),
        args.plugin_version,
        args.plugin_protocol,
    )


if __name__ == "__main__":
    main()
