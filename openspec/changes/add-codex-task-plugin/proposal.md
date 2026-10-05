## Why

Octa needs a first-class way to run autonomous coding work as an ordinary task so the same Octafile can drive local automation and distributed dark-factory builds through OctaCity. A Codex plugin can provide that capability while preserving the existing separation in which Octa executes the DAG, the agent prepares and supervises the job, and the server receives generic events, results, reports, and artifacts.

## What Changes

- Add an official `codex` execution plugin that runs the Codex harness inside the task workspace through a pinned Codex CLI executable.
- Define a typed task schema for prompts, model settings, structured result schemas, and exact deliverables.
- Translate the Codex JSONL event stream into existing Octa output, progress, and diagnostic responses without adding Codex-specific events to Octa Core.
- Produce bounded structured outputs plus deterministic run records for the final result, trace, and provenance, and register them through the existing artifact/report protocol.
- Propagate Octa cancellation and task timeouts to the complete Codex process tree and reject interactive approval flows that cannot complete unattended.
- Keep deployment-specific credentials and operational settings outside the Octafile so an agent can supply them through the existing runner variables, environment-specific secrets profile, and sandbox configuration.
- Treat Codex tasks as non-cacheable by default because model and external-tool execution are not deterministic.
- Add distribution metadata, documentation, examples, conformance fixtures, and cross-platform tests for the plugin.

## Capabilities

### New Capabilities

- `codex-task-execution`: Run a Codex coding task inside an Octa workspace, stream normalized progress, return a schema-validated result, and register auditable artifacts without introducing a Codex-specific OctaCity path.

### Modified Capabilities

None.

## Impact

- Adds a new official workspace crate and release plugin binary, manifest, and lock entry.
- Uses the existing plugin protocol, executor resource validation, runner event stream, secret redaction, and agent artifact collection contract.
- Requires a compatible Codex CLI executable in the local or agent execution image; installation and authentication remain operator responsibilities.
- Adds documented task configuration and example dark-factory workflows, but does not change OctaCity server or agent protocols.
