#!/usr/bin/env python3
"""Generate and validate the Codex compatibility document shipped by Octa."""

from __future__ import annotations

import argparse
import json
import re
from pathlib import Path
from typing import Any


FORMAT_VERSION = 2
PLUGIN_NAME = "codex"
CODEX_PRODUCT = "codex-cli"
SELECTION_ENVIRONMENT = "OCTA_CODEX_EXECUTABLE"
TOOL_AUTHORIZER_ENVIRONMENT = "OCTA_CODEX_TOOL_AUTHORIZER"
TOOL_AUTHORIZATION_CAPABILITY = "codex.blocking-pre-tool-authorization.v1"
MANIFEST_PATH = "plugins/codex.plugin.yml"
_SUPPORTED_VERSIONS = re.compile(
    r'^const SUPPORTED_CODEX_VERSIONS: &\[&str\] = &\[(?P<versions>[^]]+)\];$',
    re.MULTILINE,
)
_STRING = re.compile(r'"([0-9]+\.[0-9]+\.[0-9]+)"')


def supported_versions(source: Path) -> list[str]:
    """Reads the exact versions enforced by the plugin implementation."""
    match = _SUPPORTED_VERSIONS.search(source.read_text(encoding="utf-8"))
    if match is None:
        raise ValueError("could not read SUPPORTED_CODEX_VERSIONS from the Codex plugin")
    versions = _STRING.findall(match.group("versions"))
    if not versions or len(set(versions)) != len(versions):
        raise ValueError("Codex plugin must declare unique supported semantic versions")
    return versions


def document(source: Path, plugin_version: str, plugin_protocol: int) -> dict[str, Any]:
    """Builds the canonical release metadata from implementation-owned values."""
    return {
        "format_version": FORMAT_VERSION,
        "plugin": {
            "name": PLUGIN_NAME,
            "version": plugin_version,
            "protocol": plugin_protocol,
            "manifest": MANIFEST_PATH,
            "capabilities": [TOOL_AUTHORIZATION_CAPABILITY],
        },
        "executable": {
            "product": CODEX_PRODUCT,
            "supported_versions": supported_versions(source),
            "selection_environment": SELECTION_ENVIRONMENT,
        },
        "tool_authorization": {
            "mode": "blocking_pre_tool_use",
            "capability": TOOL_AUTHORIZATION_CAPABILITY,
            "selection_environment": TOOL_AUTHORIZER_ENVIRONMENT,
            "hook_event": "PreToolUse",
        },
    }


def load(path: Path) -> dict[str, Any]:
    """Loads one bounded JSON object without accepting trailing data."""
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ValueError("Codex compatibility metadata must be a JSON object")
    return value


def write(path: Path, value: dict[str, Any]) -> None:
    """Writes deterministic release bytes."""
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", type=Path, required=True)
    parser.add_argument("--plugin-version", required=True)
    parser.add_argument("--plugin-protocol", type=int, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--verify", action="store_true")
    arguments = parser.parse_args()

    expected = document(arguments.source, arguments.plugin_version, arguments.plugin_protocol)
    if arguments.verify:
        if load(arguments.output) != expected:
            raise SystemExit("Codex compatibility metadata does not match the plugin implementation")
        return
    write(arguments.output, expected)


if __name__ == "__main__":
    main()
