## 0.4.0 - 2026-09-15

### Highlights

- Added a headless `octa-runner` with a versioned JSONL protocol, structured events and results, cancellation, bounded I/O, and capability discovery for remote agents.
- Added portable task-result caching with content-addressed local storage, concurrent publication, crash-safe restoration, garbage collection, explicit capacity limits, and an optional HTTP remote cache.
- Added plugin-provided cache file contracts so specialized plugins can declare semantic inputs and outputs without duplicating them in every task.
- Added structured task outputs, artifacts, reports, diagnostics, and byte-accurate output streaming across the CLI, embedded runtime, and runner.
- Added reproducible plugin manifests and lock files with protocol compatibility, platform declarations, and executable digest verification.
- Added environment, file, process, and Vault secret providers with bounded values and centralized redaction.
- Improved execution and cache hot paths and added repeatable performance comparisons with go-task.

### Important changes

- The runner protocol is now version 3. Integrations using an earlier runner protocol must migrate before launching this release.
- The old timestamp/hash freshness cache has been replaced by an explicit task-result cache. Add `cache: {}` to opt a task into result reuse.
- Cache identity now includes the resolved task, inputs, selected environment, runtime, arguments, and exact plugin implementations. Existing cache state is not reused.
- Plugin installations intended for reproducible or agent execution must provide compatible manifests and a verified `Octa.lock`.

## 0.3.0 - 2026-09-04

### Highlights

- Added automatic monorepo discovery with cached project hierarchies and colon-based task namespaces.
- Added task and command conditions, deferred commands, timeouts, fail-fast execution, watch mode, and configurable concurrency.
- Added output-aware freshness checks, source and output exclusions, hash and timestamp strategies, and hierarchical `.octaignore` files.
- Added ordered, required, interactive, secret, shell-backed, and dynamically constrained variables, including CLI variable overrides.
- Added Octafile- and task-level dotenv loading, plugin-backed template evaluation, and file-backed template tasks.
- Added platform and architecture selectors for tasks, commands, and interpolated include paths.
- Added plugin task annotations, plugin-provided validation schemas, and configurable default command plugins.
- Added task search, default task execution, improved Octafile discovery, and automatic creation of task directories.
- Replaced platform-specific command shells with the cross-platform Brush shell and bundled commonly used core utilities.

### Important changes

- Shell commands now use Bash-compatible Brush syntax on every platform. Windows `cmd.exe` syntax such as `%NAME%`, `if exist`, and `exit /B` must be replaced with their Bash equivalents.
- The shell plugin bundles `base64`, `cat`, `cp`, `ls`, `mkdir`, `mktemp`, `mv`, `rm`, `sleep`, and `touch`, so these commands no longer require separate installation on Windows.

## 0.2.0 - 2025-01-20
Support for a plugin system for executors within tasks has been implemented. Built-in functionality for running the templating engine and shell commands has been moved to plugins

- First release

## 0.1.0 - 2024-12-22

- First release
