## Context

Octa already has the seams needed to treat an autonomous coding run as an ordinary task: a process-isolated plugin protocol, schema discovery, ordered output and diagnostic responses, structured completion values, artifact/report registration, cancellation, task timeouts, secret-aware logging, and runner events. OctaCity agents already prepare a workspace and runtime, invoke `octa-runner`, and collect the generic result resources. See `proposal.md` for the motivation and `specs/codex-task-execution/spec.md` for externally observable requirements.

The Codex machine interface is an external executable contract. It can emit a long JSONL stream, spawn descendant tools, modify the workspace, and use credentials. Its output is therefore untrusted, potentially sensitive, and too large to buffer without explicit limits. A model run can also reach a valid terminal state such as `blocked` without representing an infrastructure failure.

The current plugin wire request already carries the names of resolved secret variables, but `octa-plugin::PluginCommand` does not expose those names to implementations. The SDK host redacts messages sent through its logger and errors returned by the implementation; plugin-owned protocol responses and trace files must be sanitized before bytes are written. The SDK must expose the existing field without changing the plugin wire version.

## Goals / Non-Goals

**Goals:**

- Keep Codex execution behind one task-level plugin boundary and reuse all generic Octa and OctaCity contracts after that boundary.
- Make the execution path deterministic at the orchestration level: validated configuration, explicit child environment, bounded event parsing, one terminal result, deterministic run-record formats, and exact deliverable paths.
- Preserve enough sanitized evidence to audit a dark-factory run, including well-formed semantic outcomes that are not `completed`.
- Ensure cancellation, timeout, malformed output, and plugin shutdown cannot leave Codex or descendant tools running.
- Keep the implementation split into cohesive internal modules without introducing public backend abstractions for a single Codex CLI adapter.

**Non-Goals:**

- Calling OctaCity APIs from Octa or the plugin, scheduling agents, leasing jobs, or uploading artifacts directly.
- Providing a second remote Codex service/backend or an Octafile option that selects local versus OctaCity execution.
- Bundling Codex credentials or the Codex executable inside the plugin binary.
- Making model execution hermetic or automatically cacheable.
- Generalizing the first implementation into an agent framework, marketplace, conversation service, or multi-turn human approval UI.

## Decisions

### 1. Implement one ordinary official execution plugin

Add an official workspace binary crate, `octa_plugin_codex`, using the existing `octa-plugin` SDK. Its schema key is `codex`; Octa performs normal schema validation and invokes it exactly like `shell`, `tpl`, or `junit`. The plugin emits only existing `PluginResponse` variants. Runner and OctaCity protocols remain unchanged.

The agent supplies the workspace, compatible plugin lock, runtime image or native environment, variables, and secret profile before starting `octa-runner`. The server sees only generic task events and registered resources. This keeps scheduling and execution in one direction and avoids coupling a reusable local tool to the control plane.

Alternatives considered:

- An OctaCity-specific client in Octa was rejected because it reverses the existing control flow and makes local execution a different product path.
- A new Codex-specific core task type or runner event family was rejected because the plugin protocol already carries the required semantics.

### 2. Use a concrete Codex CLI adapter with a narrow internal pipeline

The crate is divided by responsibility rather than by speculative interfaces:

1. `config` deserializes the validated plugin value and applies semantic bounds.
2. `invocation` resolves the executable, prompt, selected variables, child environment, and supported Codex machine-interface version.
3. `process` owns the child process tree, stdin, cancellation grace period, stdout, and stderr.
4. `events` incrementally decodes bounded JSONL frames into a small plugin-owned event model.
5. `records` sanitizes and writes trace, result, and provenance documents, then returns resource declarations.
6. The plugin entry point coordinates those modules and sends protocol responses.

There is one concrete CLI implementation. No `CodexBackend`, repository, or transport trait is added until a second real implementation requires substitution. Tests use a fixture executable selected through a test-only constructor or environment override at the composition edge, not a public production abstraction.

### 3. Pass the prompt through stdin and construct arguments from typed fields

The plugin invokes the non-interactive Codex JSONL mode with explicit approval and sandbox arguments derived from validated enums. The prompt is read once, bounded, hashed, and sent through stdin rather than placed on the command line. An optional JSON result schema is validated by the plugin and supplied to the supported Codex structured-output mechanism without interpreting prose as JSON.

The Octa runtime or agent sandbox remains the outer security boundary. Codex's own sandbox setting is defense in depth and cannot expand permissions granted by the outer runtime. Arbitrary additional CLI arguments are not accepted because they would bypass unattended-policy, environment, and compatibility checks.

The implementation supports an explicit compatibility set for Codex CLI releases whose JSONL and structured-result behavior is covered by fixtures. It checks the executable before workspace mutation and fails closed outside that set. The release does not silently fall back to terminal scraping.

Alternatives considered:

- Shelling out through the shell plugin was rejected because quoting, cancellation, structured events, and secret handling would become ambiguous.
- Passing through arbitrary Codex flags was rejected because it makes the task schema non-authoritative and permits interactive modes.

### 4. Separate task intent from operator-provided execution material

The Octafile contains portable intent: prompt source, optional model and reasoning settings, result schema, required deliverables, and optional variable references such as a source revision. It does not contain credential values, a server URL, or an agent identity.

Credentials are selected by mapping a child environment name to a named Octa variable that must be marked secret. Public child environment values use a separate explicit mapping. The plugin starts from an empty environment and adds only a documented platform baseline, selected public values, selected credentials, and variables required by the configured Codex installation. Operator policy may set the executable path and compatibility pin in the job image or agent configuration; task authors cannot substitute an arbitrary executable.

To support file sanitization, add `secret_vars: Vec<String>` to `octa_plugin::PluginCommand` and populate it from the field already present on `OctaCommand::Execute`. This is an SDK exposure of existing wire data, not a protocol change. The plugin resolves redaction values from those named variables, rejects a credential mapping to a non-secret variable, and never serializes the values into configuration, provenance, or structured output.

### 5. Parse, normalize, sanitize, and stream each event once

Stdout is read as bounded newline-delimited frames. Every complete frame is parsed once and passed through this order:

1. validate frame size and JSON shape;
2. redact resolved string secrets as substrings and numeric or boolean secrets as standalone tokens in free-text fields;
3. append the sanitized event to a size-bounded JSONL trace;
4. translate supported activity to ordered Octa progress, stdout/stderr, or diagnostic responses;
5. update bounded terminal-result and usage state.

Stderr is separately bounded and streamed as redacted Octa stderr. Unknown non-terminal event types are retained in sanitized form and ignored for semantic translation so additive Codex events do not break a run. Malformed frames, oversized data, duplicate terminal events, or a missing terminal event are contract failures. The plugin stops the process tree before emitting its single failure response.

The trace is not described as raw: sanitization changes values by design. Its stable format records the trace format version, preserves event order, and marks redacted strings. This is the only credible way to satisfy both auditability and the no-secret-in-records requirement.

### 6. Distinguish orchestration success from the semantic coding outcome

A Codex invocation that reaches one well-formed terminal event, produces valid run records, and satisfies any configured result schema completes the plugin command with process-compatible code `0`. Its structured `outcome` records the semantic state, for example `completed`, `blocked`, `needs_input`, `budget_exhausted`, or `failed`. This lets Octa retain and publish the run records and deliverables for every auditable terminal outcome.

Transport failure, incompatible CLI, cancellation, malformed events, invalid result schema, unsafe/missing required deliverables, or inability to durably finish run records completes non-zero or returns a plugin error. Those are failures of the execution contract rather than model judgments. A workflow that requires only selected semantic outcomes adds an ordinary follow-up gate task that examines `tasks.<name>.outputs.outcome`; documentation provides that pattern.

This distinction is necessary because current Octa task execution deliberately discards structured outputs and resource registrations from a non-zero plugin completion. Changing that generic failure model would affect caching, invocation reuse, runner results, and all plugins, which is disproportionate to this change.

### 7. Write run records atomically and register resources only after validation

Each command uses a directory below a configurable safe relative root, defaulting to `.octa/codex-runs/<command-id>`. The command id is encoded as a filesystem-safe component and a collision is an error rather than an overwrite. Files are written to exclusive temporary siblings, flushed, and renamed only after the terminal result is normalized. The stable record set is:

- `trace.jsonl`: ordered, sanitized, bounded harness events;
- `result.json`: the normalized semantic outcome and optional schema-validated result;
- `provenance.json`: format versions, plugin and observed Codex versions, non-secret settings, prompt digest, optional supplied source revision, timing, outcome, and usage.

The plugin reopens and validates the final paths after the child and descendants have stopped. It registers trace and provenance as artifacts and result as a report with a versioned plugin-owned format. Required user deliverables are exact workspace-relative file or directory paths. They are checked after harness exit, then sent as existing artifact/report declarations. Octa's canonical resource validation remains the final workspace-containment and link-safety authority; the plugin never uploads bytes.

Run-record and deliverable registration is bounded by fixed default limits with conservative task-configurable reductions where useful. Limits are named constants and documented; they are not scattered literals.

### 8. Make cancellation a single-owner state machine

One command task owns the child process, event readers, trace writer, and terminal-response state. Cancellation closes stdin, requests graceful process-tree termination, waits a bounded grace period, and force-kills remaining descendants. Unix uses a dedicated process group; Windows uses a Job Object configured to terminate descendants on close. Reader tasks are joined after process termination so no event can be emitted after the terminal response.

The command coordinator is the only code allowed to emit `Completed` or `Error`. Concurrent process exit, cancellation, timeout, and plugin shutdown are reduced to one terminal decision. The plugin SDK cancellation token remains authoritative; no second internal timeout competes with Octa's task timeout, except short bounded teardown deadlines.

### 9. Keep cache planning opaque

`cache_plan` returns `None`. Model responses, remote tool state, credentials, and external services are not a complete deterministic filesystem contract. Authors can still provide a complete explicit Octa task cache contract if they accept the risk. The plugin does not claim prompt files or deliverables as an automatic contract because doing so would incorrectly imply that those are all semantic inputs and outputs.

### 10. Test the machine boundary without live model calls

Tests use a small cross-platform fixture executable that implements the required command-line and JSONL behavior. It can emit partial frames, unknown events, malformed or oversized data, duplicate/missing terminal events, structured results, secret echoes, descendants, and controlled shutdown races. Unit tests cover schema/config validation, redaction, event normalization, atomic records, and output bounds. Integration tests run the real plugin host through its socket protocol and verify cancellation, ordered streaming, dry-run behavior, resources, and process cleanup on Linux, macOS, and Windows.

An optional, ignored compatibility smoke test may run against an operator-installed supported Codex CLI without authentication. CI does not call a live model and does not require OpenAI credentials.

The official release workflow builds the new binary for supported platforms, writes its manifest and lock metadata, and includes a conformance example that runs entirely against the fixture.

## Risks / Trade-offs

- [Codex changes its machine-readable event contract] → Accept only explicitly supported releases, keep parsing in one adapter module, retain unknown additive events, and fail closed on terminal-contract changes.
- [A secret appears in an encoded or transformed form that exact-value redaction cannot recognize] → Minimize the child environment, never include credentials in prompts or provenance, document the redaction boundary, and treat outer sandbox/credential scoping as the primary protection.
- [A long run exhausts disk or memory] → Stream instead of buffering, enforce independent frame/trace/stderr/result limits, cancel on limit violation, and write records atomically.
- [Semantic failure reported as command success surprises task authors] → Expose a required normalized `outcome`, document the follow-up gate pattern, and reserve non-zero status for failures that make the run unauditable or invalid.
- [Codex or a tool leaves descendants running] → Own a process group/Job Object, join readers only after bounded tree termination, and cover descendant cleanup with platform integration tests.
- [Run records under the workspace affect later tasks] → Use one documented ignored state root, unique invocation directories, exclude it from automatic cache inputs, and require explicit cleanup through normal workspace lifecycle.
- [Operator and task policy disagree about model or sandbox settings] → Validate the final effective configuration before execution and let the stricter outer runtime policy win; the plugin never attempts to widen it.

## Migration Plan

1. Extend the plugin SDK command object with the already-transmitted secret-variable names and update existing plugin constructors/tests without changing the wire protocol version.
2. Add the Codex plugin, fixture executable, manifests, lock-generation coverage, examples, and documentation behind no default task usage.
3. Publish a release containing the plugin binary and its supported Codex CLI compatibility set.
4. Operators add a compatible Codex CLI and environment-specific authentication to local or agent execution images, then regenerate the plugin lock.
5. Adopt `codex` tasks incrementally. Removing the task or plugin lock entry is a complete rollback; no persisted server migration or Octafile version migration is required.
