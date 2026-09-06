# Plugin protocol

Octa plugins are executable processes connected to the engine through a local socket. The socket
uses UTF-8 JSON Lines: every request and response is one complete JSON object followed by `\n`.
Messages use Serde's adjacent representation:

```json
{"type":"Variant","payload":{"field":"value"}}
```

The Rust definitions in [`crates/octa-plugin/src/protocol.rs`](../crates/octa-plugin/src/protocol.rs)
are the source of truth. Plugin authors should normally use the `octa-plugin` SDK and
`serve_plugin`; the wire format is documented here for compatibility and non-Rust implementations.

This private engine-to-plugin transport is distinct from Octa's public
[runtime event stream](events.md). Plugin command IDs are translated into stable plan-level step IDs
when events are emitted; event consumers do not need to implement or observe this protocol.

## Lifecycle

A plugin connection has three phases:

1. `Hello` negotiates the Octa version.
2. `Schema` registers the task key and capabilities.
3. Zero or more `Execute` requests run concurrently until `Shutdown`.

Octa may also send command-scoped `Cancel`, `Stdin`, `Resize`, and `CloseStdin` messages during the
execution phase.

## Handshake

Octa sends its exact version and currently enabled protocol features:

```json
{"type":"Hello","payload":{"version":"0.3.0","features":[]}}
```

The plugin responds with a semver requirement that must accept the engine version:

```json
{"type":"Hello","payload":{"version":">=0.3.0, <0.4.0","features":[]}}
```

| Field | Type | Meaning |
| --- | --- | --- |
| `version` | string | Exact engine version in the request; semver requirement in the response |
| `features` | string array | Negotiated feature names; currently empty |

A version mismatch terminates plugin startup.

## Schema discovery

After the handshake Octa sends:

```json
{"type":"Schema"}
```

The plugin responds with its task key and optional capabilities:

```json
{
  "type": "Schema",
  "payload": {
    "key": "shell",
    "supports_raw": true,
    "capabilities": ["shell"],
    "validation_schema": {"type": "string"}
  }
}
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `key` | string | required | Octafile task attribute and YAML annotation name |
| `supports_raw` | boolean | `false` | Plugin accepts raw execution and the terminal-input messages |
| `capabilities` | string array | `[]` | Generic behavior exposed independently of the plugin name |
| `validation_schema` | JSON object | omitted | JSON Schema used to validate the plugin task value |

Octa compiles `validation_schema` once while loading plugins, then validates matching tasks,
annotations, commands, and conditions before execution. Omitting it preserves compatibility with
older plugins but leaves the plugin-specific value unvalidated.

The built-in shell plugin advertises the `shell` capability. Octa uses that capability for plain
string commands, `sh:` variable values, and shell template helpers. Keys and capabilities must be
non-empty and unique across loaded plugins. Keys must not collide with Octafile syntax such as
`cmds`, `task`, `if`, `timeout`, or `defer`.

The schema key can be used as a regular field or YAML annotation:

```yaml
tasks:
  build:
    shell: cargo build

  deploy: !docker
    image: app:latest
    command: ./deploy
```

Structured plugin values are validated as structured JSON, then encoded into the current
`Execute.params` string as compact JSON.

## Starting a command

Octa sends an `Execute` request:

```json
{
  "type": "Execute",
  "payload": {
    "params": "cargo test",
    "args": ["--release"],
    "dir": "/workspace/project",
    "envs": {"CI": "true"},
    "vars": {"PROFILE": "release"},
    "secret_vars": ["TOKEN"],
    "redact_params": false,
    "raw": false,
    "dry": false
  }
}
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `params` | string | required | Plugin value; structured values arrive as compact JSON text |
| `args` | string array | required | Arguments passed to the selected Octa task |
| `dir` | path string | required | Effective task working directory |
| `envs` | string map | required | Effective process environment |
| `vars` | JSON value map | required | Resolved Octafile variables |
| `secret_vars` | string array | `[]` | Variable names whose values must be redacted from SDK diagnostics |
| `redact_params` | boolean | `false` | Treat the complete `params` value as sensitive |
| `raw` | boolean | `false` | Request byte-oriented interactive execution |
| `dry` | boolean | required | Describe/validate work without applying normal side effects |

The SDK assigns a command ID and must acknowledge the request before emitting command output:

```json
{"type":"Started","payload":{"id":"9e16d0f3-8aba-4d16-a765-755f627b65dc"}}
```

`Execute` does not contain a client-generated ID. Consequently, when requests arrive concurrently,
the plugin must send `Started` acknowledgements in the same order as the corresponding `Execute`
requests. Once acknowledged, messages for different command IDs may be interleaved freely.

Plugin implementations must therefore be concurrency-safe. The SDK starts each command in its own
task and routes later control messages by command ID.

## Transport and result limits

Octa bounds every stage between a plugin and the task result:

- one JSONL request or response frame may contain at most 1 MiB, excluding its trailing newline;
- every running command has its own 32-response mailbox;
- captured stdout and stderr share a 64 MiB task-result limit;
- each captured stream stays in memory through 1 MiB and then spills to a temporary file.

If one command fills its mailbox, Octa reports that command as failed, sends `Cancel`, and discards
its remaining messages through the terminal response. Other commands on the same plugin connection
continue normally; a noisy command therefore cannot block their routing or grow an unbounded host
queue. Exceeding the result limit also fails and cancels only that command. Raw terminal output is
still subject to these transport and result limits even though it bypasses presentation buffering.

## Output, diagnostics, and completion

Plugins should use `StdoutBytes` and `StderrBytes` for streaming command output. `bytes` is base64
encoded by the protocol serializer, and a chunk may end anywhere: it does not need to contain a
newline or complete UTF-8 character. Send chunks promptly rather than retaining them until a line
ending or command exit:

```json
{"type":"StdoutBytes","payload":{"id":"command-id","bytes":"Y29tcGlsaW5n"}}
{"type":"StderrBytes","payload":{"id":"command-id","bytes":"d2FybmluZzog"}}
```

`Stdout` and `Stderr` remain supported for plugins that naturally produce complete UTF-8 lines:

```json
{"type":"Stdout","payload":{"id":"command-id","line":"compiled crate"}}
{"type":"Stderr","payload":{"id":"command-id","line":"warning: unused value"}}
```

The `line` value does not need a trailing newline; Octa supplies its logical line ending. Do not use
the line variants for partial output because Octa cannot expose that data before the response is
sent. Both forms pass through the selected output renderer, prefixes, buffering, and stream
suppression. The Rust SDK's `stream_output` helper uses byte responses and does not wait for newline.

A plugin can report a structured diagnostic instead of making Octa parse arbitrary stderr text:

```json
{
  "type": "Diagnostic",
  "payload": {
    "id": "command-id",
    "level": "error",
    "message": "unexpected token",
    "location": {"file": "src/main.rs", "line": 18, "column": 7}
  }
}
```

`level` is one of `trace`, `debug`, `info`, `warn`, or `error`. `location` is optional; `line` and
`column` inside it are also optional. When GitHub Actions annotations are enabled, available
coordinates are forwarded as annotation properties.

Every started command must finish with exactly one terminal response:

```json
{"type":"ExitStatus","payload":{"id":"command-id","code":0}}
```

or:

```json
{"type":"Error","payload":{"id":"command-id","message":"failed to start compiler"}}
```

`ExitStatus` represents a process-like result. `Error` represents a plugin/protocol failure. After
either response, Octa removes the command ID; sending later output for it is a protocol violation.

## Raw and PTY execution

A plugin must declare `supports_raw: true` before Octa will send `Execute.raw: true`. Raw mode uses
the same byte response variants as normal streaming, but treats them as an exclusive terminal
protocol and bypasses presentation transforms. Base64 lets arbitrary bytes survive the UTF-8
transport:

```json
{"type":"StdoutBytes","payload":{"id":"command-id","bytes":"G1sySg=="}}
{"type":"StderrBytes","payload":{"id":"command-id","bytes":"d2FybmluZw=="}}
```

For a raw command, Octa can send terminal input after `Started`:

```json
{"type":"Stdin","payload":{"id":"command-id","bytes":"eWVzCg=="}}
{"type":"Resize","payload":{"id":"command-id","rows":40,"cols":120}}
{"type":"CloseStdin","payload":{"id":"command-id"}}
```

| Request | Meaning |
| --- | --- |
| `Stdin` | Forward decoded bytes to the command's standard input |
| `Resize` | Update the command PTY window size |
| `CloseStdin` | Deliver end-of-input to this command |

The built-in shell plugin allocates a real PTY, forwards resize events, and replies with byte
events. Other plugins may implement a different terminal backend, but they must preserve byte order
and command isolation. Raw output bypasses line prefixes and buffering because either transformation
could corrupt a terminal protocol.

## Cancellation

Octa cancels one command without shutting down the plugin:

```json
{"type":"Cancel","payload":{"id":"command-id"}}
```

The plugin should stop the command and still send `ExitStatus` or `Error`. The SDK exposes a
command-scoped `CancellationToken` and waits for the terminal response with a bounded timeout.

## Shutdown

At the end of the engine lifecycle Octa sends:

```json
{"type":"Shutdown"}
```

The plugin responds:

```json
{"type":"Shutdown","payload":{"message":"Shutting down"}}
```

The SDK cancels remaining commands, closes the connection, and terminates the plugin process if it
does not exit normally.

## Minimal Rust plugin

The SDK owns handshake, schema discovery, command registration, routing, cancellation, and shutdown.
A plugin only implements `Plugin::version` and `Plugin::execute_command`:

```rust
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use octa_plugin::{
  logger::Logger, protocol::PluginResponse, serve_plugin, Plugin, PluginCommand, PluginSchema,
};
use tokio::{io::{AsyncWrite, AsyncWriteExt}, sync::Mutex};
use tokio_util::sync::CancellationToken;

struct EchoPlugin;

#[async_trait]
impl Plugin for EchoPlugin {
  fn version(&self) -> String {
    env!("CARGO_PKG_VERSION").to_owned()
  }

  async fn execute_command(
    &self,
    command: PluginCommand,
    writer: Arc<Mutex<impl AsyncWrite + Send + Unpin + 'static>>,
    _logger: Arc<impl Logger>,
    _cancel_token: CancellationToken,
  ) -> Result<()> {
    let output = PluginResponse::Stdout {
      id: command.id.clone(),
      line: command.command,
    };
    let done = PluginResponse::ExitStatus {
      id: command.id,
      code: 0,
    };

    let mut writer = writer.lock().await;
    for response in [output, done] {
      writer.write_all(serde_json::to_string(&response)?.as_bytes()).await?;
      writer.write_all(b"\n").await?;
    }
    writer.flush().await?;
    Ok(())
  }
}

#[tokio::main]
async fn main() -> Result<()> {
  serve_plugin(
    EchoPlugin,
    PluginSchema {
      key: "echo".to_owned(),
      supports_raw: false,
      capabilities: vec![],
      validation_schema: serde_json::json!({"type": "string"})
        .as_object()
        .cloned(),
    },
  )
  .await
}
```

For raw support, consume `PluginCommand.input`, emit `StdoutBytes`/`StderrBytes`, and set
`PluginSchema.supports_raw` to `true`. Normal commands should also prefer byte responses when output
can be incremental. Use the built-in shell plugin as the reference implementation.
