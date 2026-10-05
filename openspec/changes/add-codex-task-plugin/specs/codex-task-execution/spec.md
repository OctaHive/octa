## Purpose

Provide a reproducible Octa task type for unattended Codex coding work that behaves the same locally and under an OctaCity agent while using the existing execution, event, result, and artifact contracts.

## ADDED Requirements

### Requirement: Typed Codex task configuration
The plugin SHALL expose a `codex` task key with a JSON Schema that rejects unknown fields and validates the prompt source, optional model settings, optional final-result schema, run-record location, and exact deliverable declarations before execution begins. A task SHALL provide exactly one inline prompt or workspace-relative prompt file. Every configured file or deliverable path SHALL be workspace-relative and SHALL reject absolute paths, parent traversal, control characters, and platform-ambiguous separators.

#### Scenario: Inline prompt is accepted
- **WHEN** a task supplies a non-empty inline prompt and otherwise valid options
- **THEN** Octa accepts the task as a valid `codex` invocation

#### Scenario: Prompt file is accepted
- **WHEN** a task supplies one safe workspace-relative prompt file and no inline prompt
- **THEN** Octa accepts the task and the plugin reads that file from the effective task working directory at execution time

#### Scenario: Ambiguous configuration is rejected
- **WHEN** a task supplies both prompt forms, neither prompt form, an unknown field, or an unsafe path
- **THEN** schema or plugin validation rejects the task before starting the Codex harness

### Requirement: Job-local harness execution
The plugin SHALL run the configured Codex CLI as a child of the current Octa task in the effective task working directory. It SHALL use the Codex JSONL execution mode and SHALL NOT contact an OctaCity server or depend on an OctaCity-specific protocol. The same task definition SHALL be executable by the interactive Octa CLI and by `octa-runner` when the environment provides a compatible Codex executable and authentication.

#### Scenario: Local execution
- **WHEN** a developer runs a valid Codex task with a compatible executable and local authentication
- **THEN** the plugin runs the harness in the task workspace and returns the result through the normal plugin protocol

#### Scenario: Agent execution
- **WHEN** an agent prepares the workspace, runtime, plugin set, operational environment, and secret profile before starting `octa-runner`
- **THEN** the plugin follows the same execution path and the agent observes only normal runner events and resources

#### Scenario: Harness is unavailable
- **WHEN** the configured executable is missing, not executable, or incompatible with the supported machine-readable contract
- **THEN** the plugin returns a bounded infrastructure error without attempting a free-form fallback

### Requirement: Unattended execution policy
The plugin SHALL run Codex with non-interactive approval behavior suitable for CI. It SHALL reject a configuration that can wait indefinitely for terminal input or human approval. Octa's task timeout SHALL remain the authoritative wall-clock limit for the complete invocation.

#### Scenario: Autonomous run
- **WHEN** a task starts with the supported unattended policy
- **THEN** Codex can inspect and modify the workspace without requesting terminal approval

#### Scenario: Interactive policy is requested
- **WHEN** configuration requests an approval or input mode that requires a human response
- **THEN** validation fails before the harness starts

### Requirement: Structured event translation
The plugin SHALL parse the Codex JSONL stream incrementally and translate supported harness activity into existing Octa stdout, stderr, progress, and diagnostic responses. It SHALL preserve event order within one invocation, bound every parsed frame and retained run record, and treat malformed or oversized harness output as a plugin failure rather than forwarding untrusted JSON as an Octa protocol frame.

#### Scenario: Command activity is streamed
- **WHEN** Codex reports tool activity and command output
- **THEN** Octa receives ordered progress and output events before the invocation completes

#### Scenario: Harness reports a diagnostic
- **WHEN** a Codex event contains a user-actionable failure and an optional source location
- **THEN** the plugin emits an Octa diagnostic with the supported severity and location data

#### Scenario: Invalid event stream is rejected
- **WHEN** Codex emits malformed JSONL, an unsupported terminal sequence, or a frame above the documented limit
- **THEN** the plugin cancels the child process tree and returns a bounded protocol error

### Requirement: Schema-validated final result
The plugin SHALL capture the final Codex message and, when configured, require it to satisfy the task's JSON result schema. A successful completion SHALL expose a bounded structured output containing the normalized outcome, final message or structured result, available harness identifiers, and available usage counters. Free-form parsing SHALL NOT be used to infer a structured result when a result schema is configured.

#### Scenario: Structured result succeeds
- **WHEN** Codex produces a final value that satisfies the configured schema
- **THEN** the plugin completes successfully and returns that value in its structured outputs

#### Scenario: Structured result is invalid
- **WHEN** Codex finishes but its final value does not satisfy the configured schema
- **THEN** the plugin returns a normal failed completion with a diagnostic that does not expose credentials

#### Scenario: Plain final message succeeds
- **WHEN** no result schema is configured and Codex produces a final message
- **THEN** the plugin returns the bounded message as its final structured output

### Requirement: Auditable run records
For every successfully normalized harness completion, the plugin SHALL create a unique workspace-relative run-record directory containing a bounded sanitized JSONL trace, a normalized result document, and a provenance document. The sanitized trace SHALL preserve the supported event structure and ordering while removing explicitly resolved secret values before bytes are written. Provenance SHALL include the plugin version, observed Codex version, selected model settings, prompt digest, source revision when supplied, timestamps, terminal outcome, and available usage counters, and SHALL exclude secret values and full environment dumps.

#### Scenario: Successful run records are registered
- **WHEN** a Codex invocation reaches a well-formed terminal result
- **THEN** the plugin registers the trace and provenance as artifacts and the normalized result as a report using a stable plugin-owned format identifier

#### Scenario: Concurrent runs do not collide
- **WHEN** two Codex invocations use the same working directory concurrently
- **THEN** each invocation writes and registers a distinct run-record directory

#### Scenario: Secret is present in execution context
- **WHEN** the task resolves one or more secret variables
- **THEN** no secret value appears in the trace, result, provenance, diagnostic, stdout, stderr, or structured output produced by the plugin

### Requirement: Exact deliverable registration
The plugin SHALL support a bounded list of named file or directory deliverables rooted in the effective task working directory. After a successful harness completion, it SHALL require each declared deliverable to exist and register it through the existing artifact or report response. It SHALL rely on Octa's resource validation for canonical workspace containment and SHALL never upload files itself.

#### Scenario: Declared artifact exists
- **WHEN** Codex creates an exact declared artifact path inside the workspace
- **THEN** the plugin registers the path and the runner exposes the standard artifact event for agent collection

#### Scenario: Declared report exists
- **WHEN** Codex creates an exact declared report file with a valid plugin-owned or user-selected format identifier
- **THEN** the plugin registers the report through the existing report protocol

#### Scenario: Deliverable is missing or escapes
- **WHEN** a required deliverable is missing, resolves outside the workspace, or resolves through an unsafe link
- **THEN** the invocation fails and no unsafe resource is published

### Requirement: Cancellation and process ownership
The plugin SHALL map Octa cancellation to the active Codex invocation, stop accepting new harness events, terminate the complete child process tree within a bounded grace period, and emit exactly one terminal plugin response. Dropping or shutting down the plugin SHALL not leave a Codex process or descendant running.

#### Scenario: Task cancellation
- **WHEN** Octa sends `Cancel` during a Codex invocation
- **THEN** the Codex process tree terminates within the bounded cancellation period and the task reports cancellation

#### Scenario: Plugin shutdown races completion
- **WHEN** shutdown and harness completion occur concurrently
- **THEN** the plugin emits at most one terminal response and leaves no descendant process behind

### Requirement: Explicit credential and environment exposure
The plugin SHALL construct a documented minimal child environment and SHALL expose credentials or additional environment entries to the Codex process only when explicitly selected by operator configuration. Task parameters, events, outputs, and run records SHALL contain references or redacted names rather than credential values. Credentials SHALL remain environment-specific and SHALL NOT be required in the Octafile or runner request payload.

#### Scenario: Agent supplies environment-specific authentication
- **WHEN** an agent resolves the configured authentication reference through the job's secret profile
- **THEN** the plugin can authenticate the harness without serializing the credential into JobSpec, runner events, or result documents

#### Scenario: Unselected secret variable exists
- **WHEN** the task context contains a secret variable not explicitly selected for the harness
- **THEN** the plugin does not place that variable in the child environment or prompt

### Requirement: Cache is opt-in and explicit
The plugin SHALL report no automatic filesystem cache contract for a Codex invocation. A Codex task SHALL therefore remain uncached unless the author deliberately supplies a complete explicit Octa cache contract, and documentation SHALL warn that external model and tool behavior can make such reuse unsound.

#### Scenario: Task omits cache configuration
- **WHEN** a Codex task has no explicit Octa cache configuration
- **THEN** Octa executes the harness normally and does not look up or publish a task result

#### Scenario: Automatic contract is requested
- **WHEN** Octa asks the plugin to plan a cache contract
- **THEN** the plugin reports that automatic cache planning is unavailable

### Requirement: Dry-run is side-effect free
The plugin SHALL honor `Execute.dry` without starting Codex, reading credentials, modifying the workspace, or creating run records. It SHALL validate the static invocation shape and return a successful dry-run completion with no registered resources.

#### Scenario: Dry-run task
- **WHEN** Octa executes a valid Codex task with dry-run enabled
- **THEN** the plugin completes without starting the Codex executable or creating files

### Requirement: Reproducible plugin distribution
The Codex plugin SHALL be distributed with the same versioned manifest, protocol declaration, platform labels, digest verification, and lock-file workflow as other official Octa plugins. The plugin SHALL report its supported Codex machine-interface range and reject an incompatible executable before accepting work.

#### Scenario: Locked agent execution
- **WHEN** an agent verifies a release lock containing the Codex plugin for its platform
- **THEN** Octa starts only the digest-matched plugin binary

#### Scenario: Unsupported Codex contract
- **WHEN** the installed Codex executable does not support the required JSONL and structured-result behavior
- **THEN** the plugin fails with a compatibility diagnostic before modifying the workspace
