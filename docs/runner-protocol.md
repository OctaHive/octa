# Runner protocol

`octa-runner` is the headless execution entry point used by an Octa agent. It
executes the same runtime as the interactive `octa` CLI, but its standard input
and output are reserved for a versioned JSON Lines protocol.

The runner does not clone repositories, create sandboxes, contact an Octa
server, or upload artifacts. Its caller must prepare an absolute workspace and
the plugin binaries before starting the process.

An agent can inspect compatibility without starting a job:

```console
octa-runner capabilities
```

This prints one `capabilities` JSON object containing supported runner, event,
plugin and Octafile versions, the platform, feature names, and the optional
build commit.

## Transport

- stdin contains one JSON object per line from the caller;
- stdout contains one JSON object per line from the runner;
- stderr is reserved for failures that prevent protocol output;
- the first output is always `hello`;
- the first input must be `start`;
- one runner process executes one request;
- a matching `cancel` message may cancel the active request.

An input frame is limited to 1 MiB including its JSON payload. An oversized
or unterminated frame is rejected without buffering unbounded input. The
machine-readable schemas are
[`input-v1.schema.json`](../crates/octa-runner-protocol/schema/input-v1.schema.json) and
[`output-v1.schema.json`](../crates/octa-runner-protocol/schema/output-v1.schema.json).

Runtime event production uses a bounded queue. If the caller stops reading
stdout, backpressure eventually pauses task output instead of accumulating an
unbounded in-memory event backlog.

## Start

```json
{
  "type": "start",
  "protocol_version": 1,
  "request_id": "job-42-attempt-1",
  "request": {
    "workspace": "/workspace/project",
    "octafile": "Octafile.yml",
    "data_dir": "/workspace/.octa",
    "plugins_dir": "/opt/octa/plugins",
    "plugin_lock": "Octa.lock",
    "secrets_profile": ".octa/secrets.agent.yml",
    "commands": ["ci"],
    "variables": {"PROFILE": "release"},
    "arguments": [],
    "parallel": true,
    "failfast": true
  }
}
```

`workspace` must be an existing absolute directory and `commands` must contain
at least one task. Relative `octafile`, `data_dir`, and `plugins_dir` paths are
resolved from the workspace.

`plugin_lock` is optional for local compatibility and resolved relative to `workspace`. Agent
executions should always provide it. A locked run verifies protocol, platform, safe relative
entrypoint, and SHA-256 for every plugin before launching user code; verification never falls back
to an unlocked binary.

`secrets_profile` is optional and resolved relative to `workspace`. It maps the
logical references stored in Octafile to environment-specific providers. The
request never contains secret values; see [secret providers](secrets.md).

The request describes execution only. Repository, lease, sandbox, resource,
and server transport settings intentionally do not belong to this protocol.

Rust consumers should use the separately versioned
[`octa-runner-protocol`](../crates/octa-runner-protocol) crate. It owns the
wire DTOs, version constants, frame limit, and schemas without depending on
Octa's executor, Octafile parser, plugin manager, or async transport. Its crate
SemVer and `protocol_version` are independent compatibility boundaries.

## Cancel

```json
{"type":"cancel","request_id":"job-42-attempt-1"}
```

A malformed control message, a second `start`, or a command for a different
request produces an `error` message and is ignored. An oversized control frame
is a transport failure and cancels the execution. Cancellation is cooperative;
the caller may terminate the complete runner process or its sandbox if graceful
shutdown does not complete within its own deadline.

## Output

The initial handshake reports protocol compatibility:

```json
{
  "type": "hello",
  "protocol_version": 1,
  "octa_version": "0.3.0",
  "event_schema_version": 3,
  "plugin_protocol_version": 1
}
```

An accepted request is followed by zero or more event messages:

```json
{"type":"accepted","request_id":"job-42-attempt-1"}
{"type":"event","request_id":"job-42-attempt-1","event":{}}
```

`event` contains the existing Octa runtime event envelope documented in
[`events.md`](events.md). The runner preserves its sequence numbers.

A normally completed, failed, or cancelled execution ends with `finished`:

```json
{
  "type": "finished",
  "request_id": "job-42-attempt-1",
  "status": "succeeded",
  "results": []
}
```

`results` contains one structured `ExecutionResult` for every root command that
reached a terminal result. A task failure is a normal execution result, not a
runner protocol error. `status` is the terminal status of the complete request
and remains authoritative even when cancellation during bootstrap produces no
per-command result.

Invalid input, configuration failure, or runner infrastructure failure ends
with an `error` message:

```json
{
  "type": "error",
  "request_id": "job-42-attempt-1",
  "message": "..."
}
```

## Exit status

- `0`: every execution succeeded or was skipped;
- `1`: at least one execution failed;
- `2`: the request or execution configuration is invalid;
- `3`: runner infrastructure failed;
- `4`: the input protocol is malformed or incompatible;
- `130`: execution was cancelled by a control message or operating-system
  signal.

The structured terminal message is authoritative. Exit status exists so a
supervisor can still detect a runner that exits before producing one.
