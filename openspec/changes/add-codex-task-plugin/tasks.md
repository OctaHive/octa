## 1. Expose secret metadata to plugin implementations

- [x] 1.1 Add the already-transmitted secret-variable names to `octa_plugin::PluginCommand`, document that they identify redaction-sensitive entries in `vars`, and verify SDK protocol/host tests prove the wire version and serialized request are unchanged.
- [x] 1.2 Update official plugins, examples, and test constructors for the expanded command object, and verify `cargo test -p octa-plugin -p octa_plugin_shell -p octa_plugin_tpl -p octa_plugin_junit` passes on the host platform.
- [x] 1.3 Add SDK tests for resolving nested scalar redaction values from the named variables and for excluding ordinary variables, and verify no secret value appears in captured plugin diagnostics.

## 2. Create the Codex plugin contract

- [x] 2.1 Add `octa_plugin_codex` to the workspace with focused `config`, `invocation`, `process`, `events`, and `records` modules and module-level documentation, and verify the crate builds without introducing a public backend trait.
- [x] 2.2 Define the deny-unknown-fields input schema and Rust configuration types for exactly one prompt source, model/reasoning settings, result schema, safe run-record root, explicit public/secret variable mappings, optional source revision, and bounded deliverables; verify schema and semantic-validation tests cover valid forms and every invalid/unsafe path form.
- [x] 2.3 Define and document the bounded output schema for semantic outcome, final message or structured result, harness identifiers, usage, and record paths; verify representative successful and blocked values validate while oversized or malformed values fail.
- [x] 2.4 Return no automatic cache plan and implement side-effect-free dry-run validation; verify tests prove dry-run does not read prompt files or secrets, spawn the fixture executable, or create run-record paths.

## 3. Build a constrained Codex invocation

- [ ] 3.1 Implement bounded prompt loading, BLAKE3 prompt identity, stdin delivery, and typed construction of unattended JSONL/structured-result arguments; verify unit tests cover inline/file prompts, Unicode, size limits, and the absence of prompt text and arbitrary pass-through flags in the process command line.
- [ ] 3.2 Construct the child environment from a documented minimal platform baseline plus explicit public and secret variable mappings, reject credential mappings to variables not marked secret, and verify tests prove unselected environment and secret values are absent.
- [ ] 3.3 Resolve only the operator-selected Codex executable, validate it against the documented compatibility set before workspace mutation, and verify missing, non-executable, malformed-version, and unsupported-version fixtures return bounded compatibility diagnostics without a shell fallback.

## 4. Stream and sanitize harness events

- [ ] 4.1 Implement a bounded incremental JSONL decoder with one terminal-event state machine, and verify tests cover split reads, Unicode boundaries, unknown additive events, malformed/oversized frames, duplicate terminals, missing terminals, and preserved ordering.
- [ ] 4.2 Normalize supported harness activity into existing stdout, stderr, progress, and diagnostic responses, and verify a socket-level plugin test observes activity before the terminal response with no Codex-specific protocol variants.
- [ ] 4.3 Sanitize resolved secret scalars recursively before logging, forwarding, retaining, or writing event data, and verify adversarial fixtures cannot expose selected secrets in stdout, stderr, diagnostics, structured output, or the sanitized trace.
- [ ] 4.4 Enforce independent named limits for stderr, trace, final message, structured result, and usage metadata, and verify each limit cancels the fixture process and returns a bounded error without retaining an incomplete record as a valid result.

## 5. Own the process lifecycle

- [ ] 5.1 Implement direct child spawning with piped stdin/stdout/stderr and complete descendant ownership through a Unix process group and Windows Job Object, and verify platform tests observe no surviving descendant after normal completion, error, or plugin drop.
- [ ] 5.2 Coordinate process exit, SDK cancellation, reader completion, and one terminal plugin response with a bounded graceful-then-forced teardown; verify race tests cover cancel-before-spawn, cancel-during-output, exit-versus-cancel, timeout, and shutdown.
- [ ] 5.3 Add the cross-platform Codex fixture executable used by integration tests, including controllable partial frames, descendants, terminal outcomes, secret echoes, malformed output, and shutdown behavior; verify fixture scenarios run without network or credentials.

## 6. Validate results and publish auditable resources

- [ ] 6.1 Normalize well-formed terminal states into the documented semantic outcomes and validate configured structured results with the declared JSON Schema; verify semantic failure remains an auditable code-zero result while missing/invalid contract data fails execution.
- [ ] 6.2 Atomically write versioned sanitized `trace.jsonl`, `result.json`, and `provenance.json` under a unique invocation directory, and verify tests cover concurrent commands, collision refusal, partial-write cleanup, stable serialization, prompt digest, optional source revision, and absence of secret/environment dumps.
- [ ] 6.3 Revalidate finalized run-record and exact deliverable paths after process-tree termination, then emit existing artifact/report declarations only for safe required resources; verify missing paths, symlink escapes, devices, platform-ambiguous paths, and mutation races publish nothing unsafe.
- [ ] 6.4 Add an end-to-end plugin-host test that executes the fixture through the local socket and verifies ordered events, normalized outputs, versioned report format, artifact declarations, and exactly one terminal response.

## 7. Integrate with Octa and runner behavior

- [ ] 7.1 Add an Octafile/CLI integration fixture for a local Codex task and a semantic-outcome gate task, and verify the same fixture produces generic outputs, reports, and artifacts through `octa-runner` without an OctaCity dependency.
- [ ] 7.2 Add runner tests for cancellation, task timeout, slow event consumers, bounded records, and resource-path validation with the Codex fixture, and verify failures leave no child process or publishable partial records.
- [ ] 7.3 Add secret-provider integration coverage that supplies an authentication variable through the existing secrets profile, and verify the value is absent from JobSpec fixtures, runner events, plugin logs, spool/output captures, and all run records.

## 8. Distribute and document the plugin

- [ ] 8.1 Add Codex plugin manifest/release packaging and lock-generation coverage for every supported Octa target, and verify release tests locate the platform binary and reject a digest mismatch.
- [ ] 8.2 Document installation responsibility, supported Codex CLI versions, configuration fields, minimal environment, authentication through Octa secrets, audit-record formats, cache warning, cancellation behavior, and the separation from OctaCity; verify documentation examples parse against the plugin schema.
- [ ] 8.3 Add local and agent-oriented example Octafiles showing inline/file prompts, structured results, declared deliverables, and semantic gating, and verify conformance runs entirely with the fixture and produces only standard runner events/resources.
- [ ] 8.4 Run workspace formatting, strict Clippy, tests, rustdoc missing-doc checks, and the repository coverage gate on Linux, macOS, and Windows; verify no target-specific warnings, hangs, descendant leaks, or coverage regressions remain.
