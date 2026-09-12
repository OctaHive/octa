#!/usr/bin/env python3
"""Reproducible release-binary benchmarks for the Octa Agent Ready gate."""

from __future__ import annotations

import argparse
import json
import os
import platform
import shlex
import shutil
import subprocess
import tempfile
from datetime import datetime, timezone
from pathlib import Path


def quote(value: Path | str) -> str:
    return shlex.quote(str(value))


def write_pair(root: Path, octafile: str, taskfile: str) -> None:
    root.mkdir(parents=True, exist_ok=True)
    (root / "Octafile.yml").write_text(octafile, encoding="utf-8")
    (root / "Taskfile.yml").write_text(taskfile, encoding="utf-8")


def fixtures(root: Path) -> dict[str, Path]:
    result: dict[str, Path] = {}

    def add(name: str, octa_tasks: str, go_tasks: str) -> None:
        path = root / name
        write_pair(path, f"version: 1\ntasks:\n{octa_tasks}", f"version: '3'\ntasks:\n{go_tasks}")
        result[name] = path

    add("noop", "  run:\n    shell: ':'\n", "  run:\n    cmds: [':']\n    silent: true\n")
    add(
        "shell",
        "  run:\n    shell: printf octa-benchmark >/dev/null\n",
        "  run:\n    cmds: ['printf octa-benchmark >/dev/null']\n    silent: true\n",
    )

    octa_linear, go_linear = [], []
    octa_dag_linear, go_dag_linear = [], []
    for index in range(1, 26):
        dependency = f"\n    deps: [step{index - 1}]" if index > 1 else ""
        octa_linear.append(f"  step{index}:{dependency}\n    shell: ':'\n")
        go_linear.append(f"  step{index}:{dependency}\n    cmds: [':']\n    silent: true\n")
        empty_task = " {}" if not dependency else dependency
        octa_dag_linear.append(f"  step{index}:{empty_task}\n")
        go_dag_linear.append(f"  step{index}:{empty_task}\n")
    add("linear", "".join(octa_linear), "".join(go_linear))
    add("dag-linear", "".join(octa_dag_linear), "".join(go_dag_linear))

    octa_effectful, go_effectful = [], []
    for index in range(1, 26):
        dependency = f"\n    deps: [step{index - 1}]" if index > 1 else ""
        octa_effectful.append(f"  step{index}:{dependency}\n    shell: 'printf {index} >/dev/null'\n")
        go_effectful.append(f"  step{index}:{dependency}\n    cmds: ['printf {index} >/dev/null']\n    silent: true\n")
    add("linear-effectful", "".join(octa_effectful), "".join(go_effectful))

    octa_wide = [f"  leaf{index}:\n    shell: ':'\n" for index in range(1, 26)]
    go_wide = [f"  leaf{index}:\n    cmds: [':']\n    silent: true\n" for index in range(1, 26)]
    dependencies = ", ".join(f"leaf{index}" for index in range(1, 26))
    octa_wide.append(f"  run:\n    deps: [{dependencies}]\n")
    go_wide.append(f"  run:\n    deps: [{dependencies}]\n")
    add("wide", "".join(octa_wide), "".join(go_wide))

    add(
        "output",
        "  run:\n    shell: 'seq 1 20000; seq 1 20000 >&2'\n",
        "  run:\n    cmds: ['seq 1 20000; seq 1 20000 >&2']\n    silent: true\n",
    )
    return result


def octa_command(binary: Path, plugins: Path, workspace: Path, task: str, extra: str = "") -> str:
    return f"OCTA_PLUGINS_DIR={quote(plugins)} {quote(binary)} --dir {quote(workspace)} --quiet --silent {extra} {task} >/dev/null 2>&1"


def task_command(binary: Path, workspace: Path, task: str, extra: str = "") -> str:
    return f"{quote(binary)} --offline --dir {quote(workspace)} --silent {extra} {task} >/dev/null 2>&1"


def runner_command(binary: Path, plugins: Path, workspace: Path, task: str) -> str:
    request = json.dumps(
        {
            "type": "start",
            "protocol_version": 2,
            "request_id": "benchmark",
            "request": {
                "workspace": str(workspace),
                "data_dir": str(workspace / ".octa-runner"),
                "plugins_dir": str(plugins),
                "commands": [task],
                "quiet": True,
                "silence": True,
            },
        },
        separators=(",", ":"),
    )
    return f"printf '%s\\n' {quote(request)} | {quote(binary)} >/dev/null 2>&1"


def run_hyperfine(output: Path, name: str, commands: list[tuple[str, str]], warmup: int, runs: int, prepare: str | None = None) -> None:
    arguments = ["hyperfine", "--warmup", str(warmup), "--runs", str(runs), "--export-json", str(output / f"{name}.json")]
    if prepare:
        arguments.extend(["--prepare", prepare])
    for label, command in commands:
        arguments.extend(["--command-name", label, command])
    subprocess.run(arguments, check=True)


def version(command: list[str]) -> str:
    return subprocess.run(command, check=True, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT).stdout.strip()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--octa", type=Path, required=True)
    parser.add_argument("--runner", type=Path, required=True)
    parser.add_argument("--plugins", type=Path, required=True)
    parser.add_argument("--task", type=Path, default=Path(shutil.which("task") or "task"))
    parser.add_argument("--baseline-octa", type=Path)
    parser.add_argument("--baseline-plugins", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--warmup", type=int, default=3)
    parser.add_argument("--runs", type=int, default=15)
    args = parser.parse_args()

    for path in [args.octa, args.runner, args.task]:
        if not path.exists():
            parser.error(f"binary does not exist: {path}")
    if bool(args.baseline_octa) != bool(args.baseline_plugins):
        parser.error("--baseline-octa and --baseline-plugins must be provided together")
    args.octa = args.octa.resolve()
    args.runner = args.runner.resolve()
    args.plugins = args.plugins.resolve()
    args.task = args.task.resolve()
    if args.baseline_octa:
        args.baseline_octa = args.baseline_octa.resolve()
        args.baseline_plugins = args.baseline_plugins.resolve()
    args.output.mkdir(parents=True, exist_ok=True)

    with tempfile.TemporaryDirectory(prefix="octa-agent-ready-bench-") as temporary:
        workspaces = fixtures(Path(temporary))
        os.environ["NO_COLOR"] = "1"

        tools = [("octa-current", args.octa, args.plugins)]
        if args.baseline_octa:
            tools.append(("octa-baseline", args.baseline_octa, args.baseline_plugins))

        for scenario in ["noop", "shell", "dag-linear", "linear", "linear-effectful", "output"]:
            workspace = workspaces[scenario]
            task_name = "step25" if scenario in {"dag-linear", "linear", "linear-effectful"} else "run"
            commands = [(label, octa_command(binary, plugins, workspace, task_name)) for label, binary, plugins in tools]
            commands.append(("go-task", task_command(args.task, workspace, task_name)))
            run_hyperfine(args.output, scenario, commands, args.warmup, args.runs)

        for mode, octa_extra, task_extra in [("sequential", "--concurrency 1", "--concurrency 1"), ("parallel", "--parallel", "")]:
            workspace = workspaces["wide"]
            commands = [(label, octa_command(binary, plugins, workspace, "run", octa_extra)) for label, binary, plugins in tools]
            commands.append(("go-task", task_command(args.task, workspace, "run", task_extra)))
            run_hyperfine(args.output, f"wide-{mode}", commands, args.warmup, args.runs)

        runner_workspace = workspaces["noop"]
        run_hyperfine(
            args.output,
            "runner-overhead",
            [
                ("octa-cli", octa_command(args.octa, args.plugins, runner_workspace, "run")),
                ("octa-runner", runner_command(args.runner, args.plugins, runner_workspace, "run")),
            ],
            args.warmup,
            args.runs,
        )

    metadata = {
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "git_commit": subprocess.run(["git", "rev-parse", "HEAD"], check=True, text=True, stdout=subprocess.PIPE).stdout.strip(),
        "git_dirty": bool(subprocess.run(["git", "status", "--porcelain"], check=True, text=True, stdout=subprocess.PIPE).stdout),
        "system": platform.platform(),
        "machine": platform.machine(),
        "processor": platform.processor(),
        "python": platform.python_version(),
        "hyperfine": version(["hyperfine", "--version"]),
        "octa": version([str(args.octa), "--version"]),
        "runner_capabilities": json.loads(version([str(args.runner), "capabilities"])),
        "go_task": version([str(args.task), "--version"]),
        "warmup": args.warmup,
        "runs": args.runs,
        "baseline_octa": str(args.baseline_octa) if args.baseline_octa else None,
    }
    (args.output / "metadata.json").write_text(json.dumps(metadata, indent=2, sort_keys=True) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
