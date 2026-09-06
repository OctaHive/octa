# Runtime event stream

`octa --output jsonl` is the supported machine interface for observing a run. Consumers must not
parse human output: terminal renderers may change their wording or layout without changing this
contract.

Every line is one UTF-8 JSON object described by the checked-in
[version 2 JSON Schema](../crates/octa-output/schema/events-v2.schema.json). The current schema and
the frozen version 1 schema are embedded in the `octa-output` crate as `EVENT_SCHEMA_V2` and
`EVENT_SCHEMA_V1`; `EVENT_SCHEMA_VERSION` is written to every new entry.

## Envelope and ordering

Every entry contains:

| Field | Meaning |
| --- | --- |
| `schema_version` | Event contract version; currently `2` |
| `sequence` | Strictly increasing position in this Octa process's output stream |
| `timestamp` | RFC 3339 emission time |
| `category` | `execution`, `diagnostic`, or `document` |
| `data` | Category-specific event payload |

All entries, including logical stderr, are emitted on process stdout. Consumers should process
them in `sequence` order. A timestamp is descriptive and is not an ordering key.

## Execution hierarchy

The execution hierarchy is explicit:

```text
run (`run_id`)
└── task invocation (`scope.id`, optional `scope.parent_task_id`)
    └── executable step (`step.id`, `step.parent_task_id`)
        ├── streamed output (`step_id`, `command_id`)
        └── transient progress (`step_id`, `command_id`)
```

- A run is one invocation of the executor and is bounded by `run_started` and `run_finished`.
- A scope is one task invocation, not merely a task name. Calling the same task twice creates two
  distinct scope IDs. A nested task's `parent_task_id` points to the calling scope; a root scope
  omits it.
- A step is one executable plugin command in the expanded plan. Its `parent_task_id` always equals
  the containing scope's `id`. Step IDs are unique within a run and remain stable for that plan's
  lifetime, but are not persistent identifiers across separate runs.
- An output or task-specific diagnostic carries `step_id` when it came from a particular step.
  `command_id` identifies the concrete plugin-protocol execution and is intentionally separate from
  the plan-level step ID.

A `progress` event carries a human-readable `message` plus optional `current`, `total`, and `unit`.
It is transient command state, not stdout/stderr, and is therefore not included in output capture or
`ExecutionResult.outputs`. Consumers may display or store progress events, but must not require one
before accepting a terminal step event. If a producer outpaces its consumer, Octa coalesces pending
updates per command and preserves the newest value; progress delivery is best-effort and never
delays terminal output. Plugins that do not support structured progress continue to work; the
`replacing` renderer falls back to the latest output line.

An output payload with `format: "bytes"` carries a base64-encoded chunk from an ordinary stdout or
stderr pipe. Chunks are ordered but may split lines and UTF-8 characters; consumers must concatenate
bytes per `command_id` and stream before decoding when complete text is required. `format:
"raw_bytes"` is reserved for an exclusive raw/PTY session. The number and boundaries of chunks are
not stable API behavior.

Scope and step declarations are emitted in plan declaration order before scheduling. A scope starts
when its first DAG node is considered for execution. A step starts only after it has acquired
scheduler capacity; condition, freshness, and cache evaluation are part of the started step. A
normally executed step follows
`step_declared → step_started → step_finished`. Work cancelled before it starts can go directly from
declared to a terminal `step_finished` event. `scope_finished` is emitted only after all of the
scope's steps have reached a terminal state. A deferred command is represented by its own child
scope; if it calls a task, that task is a child of the deferred-command scope. Following
`parent_task_id` therefore always reaches the task that registered the defer without losing the
cleanup boundary.

## Versioning policy

Versions 1 and 2 are frozen as exact external contracts. Version 2 adds the `progress` execution
event; version 1 remains available unchanged. Renaming or removing a field, changing a field's type
or requiredness, adding a field or event variant, or changing an enum value requires a new
`schema_version` and a new immutable schema file. Corrections that only clarify prose do not.

Consumers should reject schema versions they do not support and validate records against the
matching schema. They should correlate IDs only inside their owning run. Octa may change
human-readable messages, task labels, timestamps, and the number or size of output chunks without a
schema version change; those values are data, not protocol structure.

JSON output cannot be combined with raw/PTY mode because terminal control bytes would corrupt the
JSON Lines stream.

## Embedded execution API

Structured observation and presentation are separate. `Console::with_event_sink` accepts an
`EventSink` alongside its `ConsoleRenderer`. The sink receives every fully sequenced
`ConsoleEntry` in global order, whether or not the renderer displays it. Presentation-only updates
used by interactive renderers are not runtime records and do not reach the sink.

The sink executes on the console writer thread. It should return promptly; an application that
sends events over a network should enqueue them into its own bounded channel and choose an explicit
backpressure policy. A sink error is returned to the event producer, but the corresponding renderer
call is still attempted.

The live event stream and the Rust `ExecutionResult` use the same `run_id`, task IDs, and step IDs.
Events are the streaming observation interface; the result returned by `Executor::execute` is the
authoritative terminal snapshot with timestamps and structured conclusions for the run, its tasks,
and their steps. Each `StepResult.outputs` map contains the typed values supplied by the plugin's
terminal `Completed` response. `OutputReference` points back to matching output events instead of
retaining a second copy of their payloads. Expected execution failures are represented in the
snapshot, while an `ExecutorError` return means a complete snapshot could not be formed or
published.

Each returned task has a `main` or `deferred` role. A failed deferred task remains visible without
changing an otherwise successful run conclusion; failure to form or publish its terminal result is
an `ExecutorError`. A declared task that was never scheduled because a dependency failed has a
`skipped` conclusion.

`Executor::start` consumes an executor and returns an `ExecutionHandle`. The handle exposes
the run ID, a clonable `CancellationToken`, idempotent `cancel`, and `wait`/`cancel_and_wait` methods.
Dropping a live handle requests cooperative cancellation instead of detaching work silently. Use
`start_with_token` when the execution must be a child of an application-owned cancellation tree;
the handle owns a child token, so cancelling it never cancels its parent or sibling executions.

Most applications should use `ExecutionEngine::start(ExecutionRequest)` instead. The engine owns
the reusable execution dependencies and performs DAG construction before starting the executor.
Its handle therefore covers both preparation and task execution. Cancellation or configuration
failure during preparation still produces matching `RunStarted`/`RunFinished` events and a
structured terminal `ExecutionResult`; infrastructure failures remain `ExecutorError` values.

`ExecutionEngine::prepare` returns a `PreparedExecution` for callers such as the CLI that need to
declare several plans in order, inspect watch targets, or choose batch presentation before any plan
starts. Plugin discovery, plugin startup, and loading the `Octafile` are application bootstrap and
remain outside individual execution requests.
