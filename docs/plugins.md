# Plugins workflow

Plugins are executable applications that communicate with Octa by exchanging serialized data over a local socket stream.

Interaction with the plugin follows a three-stage process. In the first stage, Octa and the plugin exchange Hello messages. The plugin specifies the required version of Octa, and Octa checks compatibility. If the version is incompatible, the plugin loading process stops. If the plugin’s requested version is compatible, the server sends back a Hello command. After that, Octa sends the Schema command to the plugin and waits for a Schema command in response, specifying the task attribute required by the plugin. This concludes the first stage of interaction with the plugin.

Next, Octa can send an arbitrary number of Execute commands, which the plugin processes and executes. Finally, o third stage, upon completion, Octa sends the Shutdown command to the plugin and waits for a same Shutdown response.

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
