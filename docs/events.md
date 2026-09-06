# Runtime event stream

`octa --output jsonl` is the supported machine interface for observing a run. Consumers must not
parse human output: terminal renderers may change their wording or layout without changing this
contract.

Every line is one UTF-8 JSON object described by the checked-in
[version 1 JSON Schema](../crates/octa-output/schema/events-v1.schema.json). The same schema is
embedded in the `octa-output` crate as `EVENT_SCHEMA_V1`; `EVENT_SCHEMA_VERSION` is the version
written to every entry.

## Envelope and ordering

Every entry contains:

| Field | Meaning |
| --- | --- |
| `schema_version` | Event contract version; currently `1` |
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
        └── streamed output (`step_id`, `command_id`)
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

Version 1 is frozen as an exact external contract. Renaming or removing a field, changing a field's
type or requiredness, adding a field or event variant, or changing an enum value requires a new
`schema_version` and a new immutable schema file. Corrections that only clarify prose do not.

Consumers should reject schema versions they do not support and validate records against the
matching schema. They should correlate IDs only inside their owning run. Octa may change
human-readable messages, task labels, timestamps, and the number or size of output chunks without a
schema version change; those values are data, not protocol structure.

JSON output cannot be combined with raw/PTY mode because terminal control bytes would corrupt the
JSON Lines stream.
