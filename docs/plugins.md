# Plugins workflow

Plugins are executable applications that communicate with Octa by exchanging serialized data over a local socket stream. Interaction with a plugin consists of three stages.

In the first stage, Octa launches the processes of all discovered plugins and communicates with the plugin using two commands: Hello and Schema. The Hello command is intended for synchronizing the versions of Octa and the plugin, while the Schema command is used to allow Octa to obtain the parameters for integrating the plugin into the task system.

After starting the plugin, Octa sends the Hello command to the plugin with the Octa version and waits for a response Hello command from the plugin, including the requested Octa version. Once the command is received from the plugin, Octa checks whether its version matches the requested version, and if the versions do not match, the plugin launch is terminated.

Here is what the Hello commands look like:

Octa Hello request:
| Field | Type | Description |
| --- | --- | --- |
| type | String | Plugin command type |
| version | String | Version of Octa engine |
| features | String | Octa features list. Reserved for feature use |

Example:

```json
{
  "type":"Hello"
  "payload":{
    "version": "0.2.0",
    "features": []
  }
}
```

Plugin Hello response:
| Field | Type | Description |
| --- | --- | --- |
| type | String | Plugin command type |
| version | String | Version of requested Octa engine in semver format |
| features | String | Octa features list. Reserved for feature use |

Example:

```json
{
  "type":"Hello"
  "payload":{
    "version": ">=0.2.0",
    "features": []
  }
}
```

After that, Octa sends the Schema command without any options to the plugin and waits for a Schema command in response. The response specifies the task attribute required by the plugin and can include a JSON Schema for its value.

Plugin Schema response:
| Field | Type | Description |
| --- | --- | --- |
| type | String | Plugin command type |
| key | String | Plugin task key |
| capabilities | Array | Optional generic capabilities implemented by the plugin |
| validation_schema | Object | Optional JSON Schema for the plugin task value |

Example:

```json
{
  "type":"Schema",
  "payload":{
    "key": "shell",
    "capabilities": ["shell"],
    "validation_schema": {
      "type": "string"
    }
  }
}
```

Octa compiles `validation_schema` once when loading the plugins and validates every matching task,
annotation, and command before execution. An invalid plugin schema prevents the Octafile from being
loaded. Omitting `validation_schema` keeps compatibility with older plugins: the task key is recognized,
but the plugin-specific value is not validated.

Capabilities describe behavior independently of a task key or executable name. The built-in shell
plugin advertises `shell`, which is used by `sh:` values and the `shell` Tera function and filter.
Octa rejects duplicate keys and capabilities so their resolution remains deterministic.

This concludes the first stage of interaction with the plugin.

The schema key also defines the YAML annotation for tasks handled by the plugin. For example, a
plugin that returns the `shell` key can be selected with `!shell`:

```yaml
tasks:
  build: !shell cargo build
```

The same key can be selected as `default_plugin` in the global Octa configuration or an Octafile.
Plain string tasks, commands, and conditions are then routed to that plugin. Octa validates the
configured value against the keys returned by loaded plugins; the plugin executable name does not
need to match its task-type key.

Task-type keys must be non-empty and must not collide with Octafile syntax such as `cmds`, `task`,
`if`, `timeout`, or `defer`. Octa rejects conflicting keys while loading plugin schemas so that a key
cannot be interpreted differently in tasks, commands, and conditions.

The tagged value is passed to the selected plugin. An annotation can also contain a structured
payload for plugins that accept configuration; structured values are serialized as JSON when sent
through the current string-based Execute protocol.

```yaml
tasks:
  deploy: !docker
    image: app:latest
    command: ./deploy
```

Octa rejects an annotation when no loaded plugin exposes the corresponding schema key. The existing
task attribute form, such as `shell: cargo build`, remains supported and is validated with the same
schema.

Next, Octa can send an arbitrary number of Execute commands, including concurrent commands over the
same connection. The SDK starts each command independently and identifies all responses by the ID
returned in `Started`; plugin implementations therefore must be safe to call concurrently. Here is
what the Execute commands look like:

Because `Execute` has no client-generated request ID, `Started` acknowledgements must be returned in
the same order as the corresponding requests. After that acknowledgement, output and terminal
responses from different command IDs may be interleaved freely.

Octa Execute request:
| Field | Type | Description |
| --- | --- | --- |
| type | String | Plugin command type |
| command | String | Command to execute by plugin |
| args | array | Array of arguments provided to octa when run some task |
| dir | String | Directory for execute command |
| envs | HashMap | A list of environment variables retrieved from the system and extended with variables defined in the task file |
| envs | HashMap | A list of variables defined in the octafile |
| dry | bool | True if Octa run in dry mode |

Example:
```json
{
  "type":"Execute",
  "payload":{
    "command":"echo Test",
    "args":[],
    "dir":".",
    "envs":{},
    "vars":{},
    "dry":false
  }
}
```

Upon receiving the Execute command, the plugin send `Started` command and after what can send `Stdout`, `Stderr`, and `ExitStatus` commands. There can be multiple `Stdout` and `Stderr` commands, and after receiving the ExitStatus command, Octa considers the command execution to be complete. Everything passed through the StdOut and StdErr commands is displayed on the screen by the Octa engine and also saved to a buffer to be returned as the task result.

Plugin Start response:
| Field | Type | Description |
| --- | --- | --- |
| id | String | Task identifier |

Example:
```json
{
  "type":"Start",
  "payload":{
    "id": "test-execution-id"
  }
}
```

Plugin StdOut response:
| Field | Type | Description |
| --- | --- | --- |
| id | String | Task identifier |
| line | String | Message text for output to stdout |

Example:
```json
{
  "type":"StdOut",
  "payload":{
    "id": "test-execution-id"
    "line": "test output"
  }
}
```

Plugin StdErr response:
| Field | Type | Description |
| --- | --- | --- |
| id | String | Task identifier |
| line | String | Error message text for output to stderr |

Example:
```json
{
  "type":"StdErr",
  "payload":{
    "id": "test-execution-id"
    "line": "test output"
  }
}
```

Plugin ExitStatus response:
| Field | Type | Description |
| --- | --- | --- |
| id | String | Task identifier |
| code | int | Task execution code |

Example:
```json
{
  "type":"ExitStatus",
  "payload":{
    "id": "test-execution-id"
    "code": 0
  }
}
```

Finally, o third stage, upon completion, Octa sends the Shutdown command to the plugin and waits for a same Shutdown response. After that, the plugin process will be terminated.

Example of Shutdown request:

```json
{
  "type":"Shutdown"
}
```

Example of Shutdown response:

```json
{
  "type":"Shutdown",
  "payload":{
    "message": "Shutting down"
  }
}
```

The first and last stages of communication with the plugin are abstracted within the SDK code for plugin creation. However, the execution stage (processing the Execute commands) is implemented by the plugin itself. At its minimal implementation, a plugin might look like this:

```rust
struct SimplePlugin {}

#[async_trait]
impl Plugin for SimplePlugin {
  /// Return plugin version
  fn version(&self) -> String {
    env!("CARGO_PKG_VERSION").to_owned()
  }

  async fn execute_command(
    &self,
    command: String,
    args: Vec<String>,
    dir: PathBuf,
    envs: HashMap<String, String>,
    writer: Arc<Mutex<impl AsyncWrite + Send + 'static + std::marker::Unpin>>,
    logger: Arc<impl Logger>,
    id: String,
    cancel_token: CancellationToken,
  ) -> anyhow::Result<()> {    
    // Execute some command code
    
    // Write comand output
    let response = ServerResponse::Stdout {
      id: id.clone(),
      line: "Plugin output",
    };
    
    // Write result
    let response = ServerResponse::ExitStatus {
      id: id.clone(), // ID executed command
      code: 0,
    };
    let response_json = serde_json::to_string(&response).unwrap() + "\n";
    let mut lock = writer.lock().await;
    let _ = lock.write_all(response_json).await;

    Ok(())
  }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
  serve_plugin(
    SimplePlugin {},
    PluginSchema {
      key: "key".to_owned(),
      validation_schema: serde_json::json!({
          "type": "object",
          "properties": {
            "image": { "type": "string" }
          },
          "required": ["image"],
          "additionalProperties": false
        })
        .as_object()
        .cloned(),
    },
  )
  .await
}
```
