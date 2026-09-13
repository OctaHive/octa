"""Deterministic filesystem and task fixtures for the cache benchmark suite."""

from __future__ import annotations

import random
import shutil
from dataclasses import dataclass
from pathlib import Path

from suite import RELEASE_SCALE


GENERATOR = """#!/usr/bin/env python3
import argparse
import shutil
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("mode", choices=["single", "many", "copy"])
parser.add_argument("output", type=Path)
parser.add_argument("--count", type=int, default=1)
parser.add_argument("--source", type=Path)
args = parser.parse_args()
if args.mode == "single":
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_bytes(b"cache benchmark output\\n")
elif args.mode == "many":
    args.output.mkdir(parents=True, exist_ok=True)
    for index in range(args.count):
        (args.output / f"entry-{index:06d}.txt").write_text(f"{index:06d}\\n", encoding="ascii")
else:
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.source.open("rb") as source, args.output.open("wb") as target:
        shutil.copyfileobj(source, target, length=1024 * 1024)
"""


@dataclass(frozen=True)
class Scale:
    """Materialized sizes; quick mode preserves shapes but is not release evidence."""

    inputs_medium: int
    inputs_large: int
    output_files: int
    large_bytes: int
    runs: int
    warmups: int

    @classmethod
    def release(cls) -> "Scale":
        return cls(
            RELEASE_SCALE["inputs_medium"],
            RELEASE_SCALE["inputs_large"],
            RELEASE_SCALE["output_files"],
            RELEASE_SCALE["large_bytes"],
            15,
            3,
        )

    @classmethod
    def quick(cls) -> "Scale":
        return cls(50, 500, 100, 8 * 1024 * 1024, 2, 1)


@dataclass(frozen=True)
class Fixture:
    """One equivalent Octa/go-task workspace and its measured data volume."""

    name: str
    workspace: Path
    task: str
    logical_bytes: int
    output: Path | None
    octa_arguments: tuple[str, ...] = ()
    task_arguments: tuple[str, ...] = ()


def write_pair(workspace: Path, octa_tasks: str, go_tasks: str) -> None:
    workspace.mkdir(parents=True, exist_ok=True)
    (workspace / "Octafile.yml").write_text(f"version: 1\ntasks:\n{octa_tasks}", encoding="utf-8")
    (workspace / "Taskfile.yml").write_text(f"version: '3'\ntasks:\n{go_tasks}", encoding="utf-8")


def create_inputs(directory: Path, count: int, size: int = 64) -> int:
    """Create stable small files and return their total payload bytes."""

    directory.mkdir(parents=True, exist_ok=True)
    for index in range(count):
        prefix = f"input-{index:08d}:".encode("ascii")
        body = (prefix + b"x" * size)[:size]
        (directory / f"input-{index:08d}.txt").write_bytes(body)
    return count * size


def create_large_file(path: Path, size: int, compressible: bool) -> None:
    """Stream a stable payload without allocating its complete size in memory."""

    path.parent.mkdir(parents=True, exist_ok=True)
    remaining = size
    with path.open("wb") as output:
        block_index = 0
        while remaining:
            length = min(remaining, 1024 * 1024)
            if compressible:
                symbols = random.Random(0xBB67AE8584CAA73B + block_index).randbytes(min(length, 64 * 1024))
                chunk = (symbols * ((length + len(symbols) - 1) // len(symbols)))[:length]
            else:
                # Fixture generation is outside measured commands. A unique
                # seeded block avoids both random-device noise and repeated
                # patterns that Zstandard could compress across windows.
                chunk = random.Random(0x6A09E667F3BCC909 + block_index).randbytes(length)
            output.write(chunk)
            remaining -= len(chunk)
            block_index += 1


def input_fixture(root: Path, name: str, count: int) -> Fixture:
    workspace = root / name
    logical_bytes = create_inputs(workspace / "inputs", count)
    (workspace / "generator.py").write_text(GENERATOR, encoding="utf-8")
    command = "python3 generator.py single outputs/result.txt"
    write_pair(
        workspace,
        _cached_task("build", '[generator.py, "inputs/**/*"]', "[outputs]", command),
        _go_task("build", '[generator.py, "inputs/**/*"]', "[outputs/result.txt]", command),
    )
    return Fixture(name, workspace, "build", logical_bytes, workspace / "outputs")


def large_input_fixture(root: Path, size: int) -> Fixture:
    workspace = root / "large-input"
    create_large_file(workspace / "inputs" / "payload.bin", size, compressible=False)
    (workspace / "generator.py").write_text(GENERATOR, encoding="utf-8")
    command = "python3 generator.py single outputs/result.txt"
    write_pair(
        workspace,
        _cached_task("build", '[generator.py, "inputs/**/*"]', "[outputs]", command),
        _go_task("build", '[generator.py, "inputs/**/*"]', "[outputs/result.txt]", command),
    )
    return Fixture("large-input", workspace, "build", size, workspace / "outputs")


def output_fixture(root: Path, name: str, count: int) -> Fixture:
    workspace = root / name
    create_inputs(workspace / "inputs", 1)
    (workspace / "generator.py").write_text(GENERATOR, encoding="utf-8")
    command = f"python3 generator.py many outputs --count {count}"
    write_pair(
        workspace,
        _cached_task("build", '[generator.py, "inputs/**/*"]', "[outputs]", command),
        _go_task("build", '[generator.py, "inputs/**/*"]', "[outputs]", command),
    )
    return Fixture(name, workspace, "build", count * 7, workspace / "outputs")


def large_output_fixture(root: Path, name: str, size: int, compressible: bool) -> Fixture:
    workspace = root / name
    source = workspace / "inputs" / "payload.bin"
    create_large_file(source, size, compressible)
    (workspace / "generator.py").write_text(GENERATOR, encoding="utf-8")
    command = "python3 generator.py copy outputs/payload.bin --source inputs/payload.bin"
    write_pair(
        workspace,
        _cached_task("build", '[generator.py, "inputs/**/*"]', "[outputs]", command),
        _go_task("build", '[generator.py, "inputs/**/*"]', "[outputs/payload.bin]", command),
    )
    return Fixture(name, workspace, "build", size, workspace / "outputs")


def parallel_fixture(root: Path, name: str, overlapping: bool) -> Fixture:
    workspace = root / name
    logical_bytes = create_inputs(workspace / "inputs", 25, size=64 * 1024)
    octa, task = [], []
    for index in range(25):
        inputs = '"inputs/**/*"' if overlapping else f"inputs/input-{index:08d}.txt"
        command = f"mkdir -p outputs && cp inputs/input-{index:08d}.txt outputs/result-{index:02d}.txt"
        octa.append(_cached_task(f"step{index:02d}", f"[{inputs}]", f"[outputs/result-{index:02d}.txt]", command))
        task.append(_go_task(f"step{index:02d}", f"[{inputs}]", f"[outputs/result-{index:02d}.txt]", command))
    dependencies = ", ".join(f"step{index:02d}" for index in range(25))
    octa.append(f"  all:\n    deps: [{dependencies}]\n")
    task.append(f"  all:\n    deps: [{dependencies}]\n")
    write_pair(workspace, "".join(octa), "".join(task))
    return Fixture(name, workspace, "all", logical_bytes, workspace / "outputs", ("--parallel",), ())


def plugin_contract_fixture(root: Path) -> Fixture:
    workspace = root / "automatic-plugin-contract"
    workspace.mkdir(parents=True, exist_ok=True)
    (workspace / "template.txt").write_text("hello {{ name }}", encoding="utf-8")
    write_pair(
        workspace,
        "  render:\n    cache: {}\n    vars: { name: benchmark }\n    tpl: { file: template.txt }\n",
        "  render:\n    cmds: [':']\n    silent: true\n",
    )
    return Fixture("automatic-plugin-contract", workspace, "render", len("hello {{ name }}"), None)


def compiler_fixture(root: Path) -> Fixture:
    workspace = root / "compiler-task-cache"
    (workspace / "src").mkdir(parents=True, exist_ok=True)
    (workspace / "src" / "main.rs").write_text('fn main() { println!("cache benchmark"); }\n', encoding="utf-8")
    (workspace / "Cargo.toml").write_text(
        '[package]\nname = "octa-cache-benchmark"\nversion = "0.0.0"\nedition = "2021"\n',
        encoding="utf-8",
    )
    command = "cargo build --release --target-dir build"
    write_pair(
        workspace,
        _cached_task("build", '[Cargo.toml, "src/**/*"]', "[build]", command),
        _go_task("build", '[Cargo.toml, "src/**/*"]', "[build/release/octa-cache-benchmark]", command),
    )
    logical = (workspace / "Cargo.toml").stat().st_size + (workspace / "src" / "main.rs").stat().st_size
    return Fixture("compiler-task-cache", workspace, "build", logical, workspace / "build")


def no_cache_fixture(root: Path) -> Fixture:
    workspace = root / "non-cache-noop"
    write_pair(
        workspace,
        "  run:\n    shell: ':'\n",
        "  run:\n    cmds: [':']\n    silent: true\n",
    )
    return Fixture("non-cache-noop", workspace, "run", 0, None)


def legacy_freshness_fixture(root: Path) -> Fixture:
    """Create the one-file task understood by the pre-cache Octa baseline."""

    workspace = root / "legacy-freshness"
    workspace.mkdir(parents=True, exist_ok=True)
    (workspace / "input.txt").write_text("input\n", encoding="ascii")
    (workspace / "Octafile.yml").write_text(
        "version: 1\n"
        "tasks:\n"
        "  build:\n"
        "    sources: [input.txt]\n"
        "    output: [output.txt]\n"
        "    cmds: [\"python3 -c 'from pathlib import Path; Path(\\\"output.txt\\\").write_text(\\\"output\\\\n\\\")'\"]\n",
        encoding="utf-8",
    )
    return Fixture("legacy-freshness", workspace, "build", 6, workspace / "output.txt")


def clone_workspace(source: Path, destination: Path) -> None:
    """Copy only semantic task inputs to a second absolute workspace."""

    shutil.copytree(source, destination, ignore=shutil.ignore_patterns("outputs", "build", "cache.toml", ".octa-*"))


def write_profile(
    path: Path,
    local_cache: Path,
    *,
    max_parallel_hashes: int,
    remote: dict[str, str] | None = None,
) -> None:
    """Write the strict operator profile used by one measured process."""

    lines = [
        'mode = "read_write"',
        'namespace = "benchmarks/cache-v1"',
        "",
        "[local]",
        f'directory = "{_toml_path(local_cache)}"',
        "max_bytes = 34359738368",
        "high_watermark_bytes = 32212254720",
        "low_watermark_bytes = 30064771072",
        "max_entries = 2000000",
        "",
        "[environment]",
        'identity = "octa-cache-benchmark-v1"',
        "",
        "[snapshot]",
        f"max_parallel_hashes = {max_parallel_hashes}",
        "max_entries = 1000000",
        "",
        "[bundle]",
        'compression = "zstd"',
        "compression_level = 3",
        "max_encoded_bytes = 21474836480",
        "max_expanded_bytes = 107374182400",
        "max_entries = 1000000",
    ]
    if remote:
        lines.extend(
            [
                "",
                "[remote]",
                f'endpoint = "{remote["endpoint"]}"',
                f'token_file = "{_toml_path(Path(remote["token_file"]))}"',
                f'ca_certificate_file = "{_toml_path(Path(remote["ca_certificate_file"]))}"',
                "request_timeout_seconds = 120",
                "max_parallel_transfers = 8",
                "max_retries = 0",
            ]
        )
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def _cached_task(name: str, inputs: str, outputs: str, command: str) -> str:
    return (
        f"  {name}:\n"
        "    files:\n"
        f"      inputs: {inputs}\n"
        f"      outputs: {outputs}\n"
        "    cache: {}\n"
        f"    shell: {command}\n"
    )


def _go_task(name: str, sources: str, generates: str, command: str) -> str:
    return (
        f"  {name}:\n"
        f"    sources: {sources}\n"
        f"    generates: {generates}\n"
        f"    cmds: ['{command}']\n"
        "    silent: true\n"
    )


def _toml_path(path: Path) -> str:
    return str(path.resolve()).replace("\\", "/").replace('"', '\\"')
