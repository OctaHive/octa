## 0.8.0 - 2026-10-28

### Added

- Advertise `codex.native-mcp-evidence.v1` for bounded native MCP calls and consumed results in ordinary run-record Artifacts, including after cancellation, timeout or failed native result validation.

### Changed

- Codex trace and provenance records use format 2. Trace events include local observation timestamps; provenance identifies capture lifecycle, coverage, terminal presence, record count and the exact trace digest. Consumers of these files must support format 2 before upgrading.
- Failed and cancelled task results retain validated trace/provenance Artifact declarations for Agent output collection without publishing semantic outputs, result reports or cache entries.

### Compatibility

- Codex result format 1, plugin protocol 2, runner protocols, and supported Codex CLI 0.161.0 remain unchanged. Capture coverage describes only emitted native events; absent remote coverage or revisions remain unknown.

## 0.7.3 - 2026-10-28

### Fixed

- Place the Codex root approval option before `exec`, so the pinned native CLI accepts generated invocations.
- Accept the final completed assistant message preceding `turn.completed` as the native execution result. Reuse existing secret sanitization, byte limits and JSON Schema validation for plain and structured results.
- Preserve queued command completion when its progress channel closes, preventing intermittent `Connection closed` failures after successful short commands.

### Security

- Update the cache compression dependency to zstd 0.14.3 and its patched native bindings, resolving reported stream-context disclosure and decompression memory-safety vulnerabilities.

### Compatibility

- Supported Codex CLI 0.161.0, sandbox/approval policy, MCP configuration and all plugin/runner protocol versions remain unchanged.

## 0.7.2 - 2026-10-25

### Fixed

- Include Codex executable qualification and app-server model queries in one 10-second discovery deadline. A slow or stalled runtime returns unavailable model metadata and is terminated through the existing process owner; its stderr drain is cancelled as well.

### Compatibility

- Model catalogs, manual model entry, task execution and all plugin/runner protocol versions remain unchanged.

## 0.7.1 - 2026-10-25

### Added

- Plugins can expose bounded advisory model catalogs and parameter schemas through the existing plugin connection and `--describe-models`. Model identifiers remain open for explicit manual selection.
- Codex discovers model suggestions and per-model reasoning options from the operator-selected runtime without starting a task, inference turn or run records. Explicit refresh returns current metadata; unavailable discovery remains distinguishable from an empty catalog.

### Compatibility

- Model names and support remain runtime-owned. Suggestions are not an entitlement check or an exhaustive allowlist; execution reports an unsupported choice without selecting another model.
- Existing runner and plugin protocol versions, task execution, usage reporting and plugins without model discovery remain compatible. The Codex plugin advertises `model-catalog.v1`.

## 0.7.0 - 2026-10-24

### Added

- Codex tasks accept a finite set of extra CLI arguments and explicit HTTP/stdio MCP servers with selected tools, required startup and existing public/secret environment mappings.
- The plugin and release compatibility metadata advertise `codex.task-runtime-settings.v1` so callers can qualify support before dispatch.

### Compatibility and security

- Existing task fields, runner/plugin protocols, observations and usage records remain compatible. Omitting the new settings keeps the previous command; an explicit MCP map skips the user config. The outer Agent/backend must isolate other Codex configuration layers for exact MCP selection.
- Task arguments cannot replace sandbox, approval, hook, model or output controls. MCP bearer credentials require an explicit secret mapping and HTTPS, and credential values stay out of argv and audit records.
- External integration catalogs and authorization remain caller-owned; no OctaCity dependency or connection registry is introduced.
- Update Tokio to 1.53.3 for RUSTSEC-2026-0386 and register the owned raw-input descriptor with its explicit lifetime contract.

## 0.6.0 - 2026-10-20

### Added

- Codex reports bounded public command and file-operation observations through the existing plugin and runner event channel, with original observation times, safe outcomes and output references.
- Release manifests and compatibility metadata advertise the qualified observation capability; release checks reject missing capabilities.

### Changed

- Rust consumers constructing plugin/output `ProgressUpdate` values must initialize the new optional `observation` field. Use `None` for ordinary progress. Runner protocol 3, event schema 4 and plugin protocol 2 remain unchanged.
- Public operation observations use the existing ordered bounded output queue so delayed consumption cannot replace start/completion events with the latest progress update. Missing measurements remain unknown; summaries grant no acceptance authority.

### Security

- Update QUIC and TLS dependencies to patched versions of quinn-proto, rustls, rustls-webpki and webpki-roots.

## 0.5.1 - 2026-10-08

### Added

- Added an opt-in, capability-advertised blocking Codex `PreToolUse` hook that invokes one fingerprinted operator-selected authorization helper without exposing a task-controlled executable path.

### Changed

- Updated the exact supported Codex CLI release from `0.130.0` to stable `0.161.0`; prerelease and unqualified versions remain rejected before task inputs or credentials are exposed.

## 0.5.0 - 2026-10-06

### Highlights

- Added the official `codex` task plugin for bounded unattended Codex CLI execution through the existing Octa plugin and runner protocols.
- Added strict prompt, environment, structured-result, deliverable, audit-record, and supported Codex CLI `0.130.0` contracts without introducing an OctaCity-specific execution path.
- Added sanitized bounded JSONL progress, complete process-tree cancellation, schema-validated results, and generic artifact/report publication for Codex tasks.
- Added automatic local task-result caching for explicitly cacheable CLI task graphs while keeping headless runner caching explicitly negotiated.

### Important changes

- Release archives now contain `octa_plugin_codex`, `codex.plugin.yml`, and the matching `Octa.lock` entry on every supported platform. Codex CLI and its authentication remain operator-installed inputs and are not bundled.
- `OCTA_CODEX_EXECUTABLE` must identify an absolute operator-protected Codex CLI `0.130.0` executable. Unsupported or mutable executable identities are rejected before task inputs or credentials are exposed.
- Codex tasks are non-cacheable by default. Any explicit cache configuration remains the workflow author's responsibility and must describe all relevant model, tool, and external-service inputs.

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
