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

After that, Octa sends the Schema command without any options to the plugin and waits for a Schema command in response, specifying the task attribute required by the plugin. 

Plugin Hello response:
| Field | Type | Description |
| --- | --- | --- |
| type | String | Plugin command type |
| key | String | Plugin task key |

Example:

```json
{
  "type":"Schema"
  "payload":{
    "key": "shell"
  }
}
```

This concludes the first stage of interaction with the plugin.

Next, Octa can send an arbitrary number of Execute commands, which the plugin processes and executes. Here is what the Execute commands look like:

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
  "type":"StdOut",
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
  serve_plugin(SimplePlugin {}, PluginSchema { key: "key".to_owned() }).await
}
```
