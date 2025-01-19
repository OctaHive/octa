use std::{collections::HashMap, ffi::OsStr, io, path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use clap::Parser;
use futures::StreamExt;
use interprocess::local_socket::{
  tokio::{prelude::*, Stream},
  ListenerOptions,
};
use logger::{Logger, LoggerSystem};
use serde_json::Value;
use socket::interpret_local_socket_name;
use tokio::io::{AsyncWrite, ReadHalf};
use tokio::{
  io::{split, AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader, WriteHalf},
  sync::Mutex,
  task::JoinHandle,
};
use tokio_util::{
  codec::{FramedRead, LinesCodec},
  sync::CancellationToken,
};
use uuid::Uuid;

use protocol::{OctaCommand, PluginResponse, Schema, Version};

pub mod logger;
pub mod protocol;
pub mod socket;

#[derive(Clone)]
pub struct PluginSchema {
  pub key: String,
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
  /// Logging directory
  #[arg(long)]
  log_dir: Option<String>,

  /// Path to the socket
  #[arg(long)]
  socket_path: String,
}

#[async_trait]
pub trait Plugin: Send + Sync + 'static {
  fn version(&self) -> String;

  #[allow(clippy::too_many_arguments)]
  async fn execute_command(
    &self,
    id: String,
    command: String,
    args: Vec<String>,
    dir: PathBuf,
    vars: HashMap<String, Value>,
    envs: HashMap<String, String>,
    writer: Arc<Mutex<impl AsyncWrite + Send + 'static + std::marker::Unpin>>,
    logger: Arc<impl Logger>,
    cancel_token: CancellationToken,
  ) -> anyhow::Result<()>;
}

type ActiveCommands = Arc<Mutex<HashMap<String, JoinHandle<()>>>>;

pub async fn stream_output(
  stream: impl AsyncRead + Unpin,
  output_type: &str,
  writer: Arc<Mutex<WriteHalf<Stream>>>,
  id: String,
) -> io::Result<()> {
  let mut reader = FramedRead::new(stream, LinesCodec::new());

  while let Some(line_result) = reader.next().await {
    match line_result {
      Ok(line) => {
        let response = match output_type {
          "stdout" => PluginResponse::Stdout { id: id.clone(), line },
          "stderr" => PluginResponse::Stderr { id: id.clone(), line },
          _ => unreachable!(),
        };

        let response_json = serde_json::to_string(&response)? + "\n";
        writer.lock().await.write_all(response_json.as_bytes()).await?;
      },
      Err(e) => {
        let error = PluginResponse::Error {
          id: id.clone(),
          message: format!("Failed to read {}: {}", output_type, e),
        };
        let error_json = serde_json::to_string(&error)? + "\n";
        writer.lock().await.write_all(error_json.as_bytes()).await?;
      },
    }
  }
  Ok(())
}

async fn handle_command<W>(
  command: OctaCommand,
  writer: Arc<Mutex<W>>,
  active_commands: ActiveCommands,
  plugin: Arc<impl Plugin + 'static>,
  logger: Arc<impl Logger>,
  cancel_token: CancellationToken,
) -> anyhow::Result<()>
where
  W: AsyncWrite + Send + Unpin + 'static,
{
  match command {
    OctaCommand::Execute {
      command,
      args,
      dir,
      envs,
      vars,
    } => {
      let id = Uuid::new_v4().to_string();

      logger.log(&format!("Received execute command {:?}", command))?;

      {
        // Send started response
        let start_response = PluginResponse::Started { id: id.clone() };
        let start_json = serde_json::to_string(&start_response)? + "\n";
        writer.lock().await.write_all(start_json.as_bytes()).await?;

        logger.log(&format!("Send Start command for command id '{}'", &id))?;
      }

      // Clone what we need to move into the spawn
      let writer_clone = Arc::clone(&writer);
      let command_id = id.clone();
      let command_logger = logger.clone();

      // Spawn the command execution
      let handle = tokio::spawn(async move {
        if let Err(e) = plugin
          .execute_command(
            command_id.clone(),
            command,
            args,
            dir,
            vars,
            envs,
            writer_clone.clone(),
            command_logger,
            cancel_token.clone(),
          )
          .await
        {
          let error = PluginResponse::Error {
            id: command_id,
            message: format!("Command execution error: {}", e),
          };
          if let Ok(json) = serde_json::to_string(&error) {
            let error_json = json + "\n";
            let mut lock = writer_clone.lock().await;
            let _ = lock.write_all(error_json.as_bytes()).await;
          }
        }
      });

      // Store the handle with the original id
      active_commands.lock().await.insert(id, handle);
    },
    OctaCommand::Schema => {
      let response = PluginResponse::Error {
        id: "protocol_error".to_string(),
        message: "Unexpected Schema command".to_owned(),
      };
      writer
        .lock()
        .await
        .write_all(serde_json::to_string(&response)?.as_bytes())
        .await?;

      logger.log("Received unexpected Schema command")?;
    },
    OctaCommand::Hello(_) => {
      let response = PluginResponse::Error {
        id: "protocol_error".to_string(),
        message: "Unexpected Hello command".to_owned(),
      };
      writer
        .lock()
        .await
        .write_all(serde_json::to_string(&response)?.as_bytes())
        .await?;

      logger.log("Received unexpected Hello command")?;
    },
    OctaCommand::Shutdown => {
      logger.log("Receiving shutdown command")?;
      cancel_token.cancel();
    },
  }

  logger.log("Execute command process sucessfully")?;
  Ok(())
}

async fn handle_conn(
  conn: Stream,
  plugin: Arc<impl Plugin + 'static>,
  schema: PluginSchema,
  logger: Arc<impl Logger>,
  cancel_token: CancellationToken,
) -> anyhow::Result<()> {
  let (reader, writer) = split(conn);
  let writer = Arc::new(Mutex::new(writer));
  let active_commands: ActiveCommands = Arc::new(Mutex::new(HashMap::new()));
  let mut reader = BufReader::new(reader);
  let mut buffer = String::new();

  process_hello(plugin.clone(), &mut reader, writer.clone(), logger.clone()).await?;

  process_schema(schema, &mut reader, writer.clone(), logger.clone()).await?;

  loop {
    buffer.clear();

    tokio::select! {
      read_result = reader.read_line(&mut buffer) => {
        match read_result {
          Ok(0) => break, // EOF
          Ok(_) => {
            match serde_json::from_str(&buffer) {
              Ok(cmd) => {
                if let Err(e) = handle_command(
                  cmd,
                  Arc::clone(&writer),
                  Arc::clone(&active_commands),
                  Arc::clone(&plugin),
                  Arc::clone(&logger),
                  cancel_token.clone(),
                ).await {
                  logger.log(&format!("Error handling command: {}", e))?;

                  break;
                }
              },
              Err(e) => {
                let response = PluginResponse::Error {
                  id: "parse_error".to_string(),
                  message: format!("Invalid command format: {}", e),
                };
                let response_json = serde_json::to_string(&response)? + "\n";
                writer.lock().await.write_all(response_json.as_bytes()).await?;
              }
            }
          },
          Err(e) => {
            logger.log(&format!("Error reading from connection: {}", e))?;

            break;
          }
        }
      }
      _ = cancel_token.cancelled() => {
        logger.log("Connection received shutdown signal")?;
        break;
      }
    }
  }

  // Graceful connection shutdown
  {
    let mut commands = active_commands.lock().await;
    for (_, handle) in commands.drain() {
      if let Err(e) = handle.await {
        logger.log(&format!("Error waiting for complete commands: {}", e))?;
      }
    }
  }

  let response = PluginResponse::Shutdown {
    message: "Plugin shutting down".to_string(),
  };
  writer
    .lock()
    .await
    .write_all(format!("{}\n", serde_json::to_string(&response)?).as_bytes())
    .await?;

  Ok(())
}

pub async fn process_hello<W>(
  plugin: Arc<impl Plugin + 'static>,
  reader: &mut BufReader<ReadHalf<Stream>>,
  writer: Arc<Mutex<W>>,
  logger: Arc<impl Logger>,
) -> anyhow::Result<()>
where
  W: AsyncWrite + Send + Unpin + 'static,
{
  let mut buffer = String::new();

  // Wait for octa Hello
  if reader.read_line(&mut buffer).await? == 0 {
    return Ok(());
  }

  match serde_json::from_str(&buffer) {
    Ok(OctaCommand::Hello(client_version)) => {
      if let Err(e) = logger.log(&format!("Client connected with version: {}", client_version.version)) {
        eprintln!("Failed to log message: {}", e);
      }

      // Send server Hello response with plugin version
      let response = PluginResponse::Hello(Version {
        version: plugin.version(),
        features: vec![],
      });
      let response_json = serde_json::to_string(&response)? + "\n";
      writer.lock().await.write_all(response_json.as_bytes()).await?;

      let _ = logger.log(&response_json.to_string());
    },
    Ok(command) => {
      let response = PluginResponse::Error {
        id: "protocol_error".to_string(),
        message: "Expected Hello command".to_string(),
      };
      writer
        .lock()
        .await
        .write_all(format!("{}\n", serde_json::to_string(&response)?).as_bytes())
        .await?;

      logger.log(&format!("Waiting for Hello command but received {:?}", command))?;
      return Ok(());
    },
    Err(e) => {
      let response = PluginResponse::Error {
        id: "parse_error".to_string(),
        message: format!("Invalid command format: {}", e),
      };
      writer
        .lock()
        .await
        .write_all(format!("{}\n", serde_json::to_string(&response)?).as_bytes())
        .await?;

      logger.log("Failed to deserialize received command")?;
      return Ok(());
    },
  }

  Ok(())
}

pub async fn process_schema<W>(
  schema: PluginSchema,
  reader: &mut BufReader<ReadHalf<Stream>>,
  writer: Arc<Mutex<W>>,
  logger: Arc<impl Logger>,
) -> anyhow::Result<()>
where
  W: AsyncWrite + Send + Unpin + 'static,
{
  let mut buffer = String::new();

  // Wait for octa Hello
  if reader.read_line(&mut buffer).await? == 0 {
    return Ok(());
  }

  match serde_json::from_str(&buffer) {
    Ok(OctaCommand::Schema) => {
      let schema_response = PluginResponse::Schema(Schema { key: schema.key });
      let response_json = serde_json::to_string(&schema_response)? + "\n";
      writer.lock().await.write_all(response_json.as_bytes()).await?;

      let _ = logger.log(&response_json.to_string());
    },
    Ok(command) => {
      let response = PluginResponse::Error {
        id: "protocol_error".to_string(),
        message: "Expected Schema command".to_string(),
      };
      writer
        .lock()
        .await
        .write_all(format!("{}\n", serde_json::to_string(&response)?).as_bytes())
        .await?;

      logger.log(&format!("Waiting for Schema command but received {:?}", command))?;
      return Ok(());
    },
    Err(e) => {
      let response = PluginResponse::Error {
        id: "parse_error".to_string(),
        message: format!("Invalid command format: {}", e),
      };
      writer
        .lock()
        .await
        .write_all(format!("{}\n", serde_json::to_string(&response)?).as_bytes())
        .await?;

      logger.log("Failed to deserialize received command")?;
      return Ok(());
    },
  }

  Ok(())
}

pub async fn serve_plugin(plugin: impl Plugin + 'static, schema: PluginSchema) -> anyhow::Result<()> {
  let args = Args::parse();

  let plugin = Arc::new(plugin);
  let plugin_name: String = std::env::current_exe()
    .ok()
    .as_ref()
    .and_then(|path| path.file_stem())
    .map(|stem| stem.to_string_lossy().into_owned())
    .map(|stem| stem.strip_prefix("octa_plugin_").map(|s| s.to_owned()).unwrap_or(stem))
    .unwrap_or_else(|| "(unknown)".into());

  let cancel_token = CancellationToken::new();

  let logger_system = LoggerSystem::new(&plugin_name, args.log_dir)?;
  let logger = logger_system.get_logger();

  let signal_cancel_token = cancel_token.clone();
  let signal_logger = logger.clone();
  ctrlc::set_handler(move || {
    let _ = signal_logger.log("Shutting down server...");

    signal_cancel_token.cancel();
  })?;

  let socket_name = interpret_local_socket_name(OsStr::new(&args.socket_path))?;

  logger.log(&format!(
    "Plugin {} starting with version: {}",
    plugin_name,
    plugin.version()
  ))?;
  logger.log(&format!("Connect to socket {}", args.socket_path))?;

  let opts = ListenerOptions::new().name(socket_name);
  let listener = match opts.create_tokio() {
    Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
      let msg = format!(
        "Error: could not start {} plugin because the socket file is occupied.
      Please check if {} is in use by another process and try again.",
        plugin_name, args.socket_path
      );
      logger.log(&msg)?;
      logger_system.shutdown()?;

      eprintln!("{}", msg);
      return Err(e.into());
    },
    x => x?,
  };

  let active_connections = Arc::new(Mutex::new(Vec::new()));

  loop {
    tokio::select! {
      accept_result = listener.accept() => {
        match accept_result {
          Ok(conn) => {
            let plugin_clone = Arc::clone(&plugin);
            let logger_clone = Arc::clone(&logger);
            let cancel_token = cancel_token.clone();
            let plugin_schema = schema.clone();
            let handle = tokio::spawn(async move {
              if let Err(e) = handle_conn(conn, plugin_clone, plugin_schema, logger_clone, cancel_token).await {
                // TODO: change to logger
                eprintln!("Error while handling connection: {e}");
              }
            });

            // Store the handle
            active_connections.lock().await.push(handle);
          },
          Err(e) => {
            logger.log(&format!("There was an error with an incoming connection: {}", e))?;

            continue;
          }
        }
      }
      _ = cancel_token.cancelled() => {
        logger.log("Received shutdown signal")?;
        break;
      }
    }
  }

  logger.log("Shutting down plugin...")?;

  // Wait for all active connections to complete
  let mut handles = active_connections.lock().await;
  for handle in handles.drain(..) {
    if let Err(e) = handle.await {
      logger.log(&format!("Error waiting for connection to close: {}", e))?;
    }
  }

  logger.log("Plugin shutdown complete")?;

  // Wait log message writed to disk
  tokio::time::sleep(Duration::from_millis(100)).await;

  logger_system.shutdown()?;

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use logger::MockLogger;
  use std::sync::Arc;
  use std::time::Duration;
  use tempfile::tempdir;
  use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

  struct MockPlugin {
    version: String,
    execution_delay: Option<Duration>,
    should_fail: bool,
    output_lines: Vec<String>,
  }

  #[async_trait]
  impl Plugin for MockPlugin {
    fn version(&self) -> String {
      self.version.clone()
    }

    async fn execute_command(
      &self,
      id: String,
      command: String,
      args: Vec<String>,
      _dir: PathBuf,
      _vars: HashMap<String, Value>,
      _envs: HashMap<String, String>,
      writer: Arc<Mutex<impl AsyncWrite + Send + 'static + std::marker::Unpin>>,
      logger: Arc<impl Logger>,
      cancel_token: CancellationToken,
    ) -> anyhow::Result<()> {
      logger.log(&format!("Executing command: {} {:?}", command, args))?;

      for line in &self.output_lines {
        if cancel_token.is_cancelled() {
          return Ok(());
        }

        let stdout = PluginResponse::Stdout {
          id: id.clone(),
          line: line.clone(),
        };
        writer
          .lock()
          .await
          .write_all((serde_json::to_string(&stdout)? + "\n").as_bytes())
          .await?;

        if let Some(delay) = self.execution_delay {
          tokio::time::sleep(delay).await;
        }
      }

      if self.should_fail {
        return Err(anyhow::anyhow!("Command failed"));
      }

      let response = PluginResponse::ExitStatus {
        id: id.clone(),
        code: 0,
      };
      writer
        .lock()
        .await
        .write_all((serde_json::to_string(&response)? + "\n").as_bytes())
        .await?;

      Ok(())
    }
  }

  async fn read_responses(reader: impl AsyncRead + Unpin) -> Vec<PluginResponse> {
    let mut responses = Vec::new();
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await.unwrap() {
      responses.push(serde_json::from_str(&line).unwrap());
    }
    responses
  }

  #[tokio::test]
  async fn test_handle_command_with_output() {
    let (reader, writer) = tokio::io::duplex(1024);
    let writer = Arc::new(Mutex::new(writer));
    let active_commands = Arc::new(Mutex::new(HashMap::new()));

    let plugin = Arc::new(MockPlugin {
      version: "1.0.0".to_string(),
      execution_delay: None,
      should_fail: false,
      output_lines: vec!["line 1".to_string(), "line 2".to_string(), "line 3".to_string()],
    });

    let logger = Arc::new(MockLogger::new());
    let cancel_token = CancellationToken::new();

    let command = OctaCommand::Execute {
      command: "test".to_string(),
      args: vec!["arg1".to_string(), "arg2".to_string()],
      dir: PathBuf::from("/test/dir"),
      envs: {
        let mut map = HashMap::new();
        map.insert("ENV1".to_string(), "value1".to_string());
        map
      },
      vars: HashMap::new(),
    };

    let response_handle = tokio::spawn(async move { read_responses(reader).await });

    let result = handle_command(
      command,
      writer,
      active_commands.clone(),
      plugin,
      logger.clone(),
      cancel_token,
    )
    .await;

    assert!(result.is_ok());

    tokio::time::sleep(Duration::from_millis(100)).await;

    let responses = response_handle.await.unwrap();

    assert!(matches!(responses[0], PluginResponse::Started { .. }));
    assert!(matches!(responses[1], PluginResponse::Stdout { .. }));
    assert!(matches!(responses[2], PluginResponse::Stdout { .. }));
    assert!(matches!(responses[3], PluginResponse::Stdout { .. }));
    assert!(matches!(responses[4], PluginResponse::ExitStatus { .. }));

    let mock_logger = logger.as_any().downcast_ref::<MockLogger>().unwrap();
    let log_messages = mock_logger.get_messages().await;
    assert!(!log_messages.is_empty());
  }

  #[tokio::test]
  async fn test_handle_command_shutdown() {
    let (reader, writer) = tokio::io::duplex(1024);
    let writer = Arc::new(Mutex::new(writer));
    let active_commands = Arc::new(Mutex::new(HashMap::new()));

    let plugin = Arc::new(MockPlugin {
      version: "1.0.0".to_string(),
      execution_delay: None,
      should_fail: false,
      output_lines: vec![],
    });

    let logger = Arc::new(MockLogger::new());
    let cancel_token = CancellationToken::new();

    let command = OctaCommand::Shutdown;

    let _response_handle = tokio::spawn(async move { read_responses(reader).await });

    let result = handle_command(
      command,
      writer,
      active_commands,
      plugin,
      logger.clone(),
      cancel_token.clone(),
    )
    .await;

    assert!(result.is_ok());
    assert!(cancel_token.is_cancelled());

    let mock_logger = logger.as_any().downcast_ref::<MockLogger>().unwrap();
    let log_messages = mock_logger.get_messages().await;
    assert!(log_messages.is_empty());
  }

  #[tokio::test]
  async fn test_handle_command_with_failure() {
    let (reader, writer) = tokio::io::duplex(1024);
    let writer = Arc::new(Mutex::new(writer));
    let active_commands = Arc::new(Mutex::new(HashMap::new()));

    let plugin = Arc::new(MockPlugin {
      version: "1.0.0".to_string(),
      execution_delay: None,
      should_fail: true,
      output_lines: vec!["error output".to_string()],
    });

    let logger = Arc::new(MockLogger::new());
    let cancel_token = CancellationToken::new();

    let command = OctaCommand::Execute {
      command: "failing_command".to_string(),
      args: vec![],
      dir: PathBuf::from("."),
      envs: HashMap::new(),
      vars: HashMap::new(),
    };

    let response_handle = tokio::spawn(async move { read_responses(reader).await });

    let result = handle_command(
      command,
      writer,
      active_commands.clone(),
      plugin,
      logger.clone(),
      cancel_token,
    )
    .await;

    assert!(result.is_ok());

    let responses = response_handle.await.unwrap();

    assert!(matches!(responses[0], PluginResponse::Started { .. }));
    assert!(matches!(responses[1], PluginResponse::Stdout { .. }));
    assert!(matches!(responses[2], PluginResponse::Error { .. }));

    let mock_logger = logger.as_any().downcast_ref::<MockLogger>().unwrap();
    let log_messages = mock_logger.get_messages().await;
    assert!(!log_messages.is_empty());
  }

  #[tokio::test]
  async fn test_handle_command_with_long_running_task() {
    let (reader, writer) = tokio::io::duplex(1024);
    let writer = Arc::new(Mutex::new(writer));
    let active_commands = Arc::new(Mutex::new(HashMap::new()));

    let plugin = Arc::new(MockPlugin {
      version: "1.0.0".to_string(),
      execution_delay: Some(Duration::from_millis(50)),
      should_fail: false,
      output_lines: vec!["line 1".to_string(), "line 2".to_string()],
    });

    let logger = Arc::new(MockLogger::new());
    let cancel_token = CancellationToken::new();

    let command = OctaCommand::Execute {
      command: "long_running".to_string(),
      args: vec![],
      dir: PathBuf::from("."),
      envs: HashMap::new(),
      vars: HashMap::new(),
    };

    let response_handle = tokio::spawn(async move { read_responses(reader).await });

    let result = handle_command(
      command,
      writer,
      active_commands.clone(),
      plugin,
      logger.clone(),
      cancel_token,
    )
    .await;

    assert!(result.is_ok());

    // Wait for execution to complete
    tokio::time::sleep(Duration::from_millis(200)).await;

    let responses = response_handle.await.unwrap();

    assert!(matches!(responses[0], PluginResponse::Started { .. }));
    assert!(matches!(responses[1], PluginResponse::Stdout { .. }));
    assert!(matches!(responses[2], PluginResponse::Stdout { .. }));
    assert!(matches!(responses[3], PluginResponse::ExitStatus { .. }));

    let mock_logger = logger.as_any().downcast_ref::<MockLogger>().unwrap();
    let log_messages = mock_logger.get_messages().await;
    assert!(!log_messages.is_empty());
  }

  #[tokio::test]
  async fn test_handle_command_cancellation() {
    let (reader, writer) = tokio::io::duplex(1024);
    let writer = Arc::new(Mutex::new(writer));
    let active_commands = Arc::new(Mutex::new(HashMap::new()));

    let plugin = Arc::new(MockPlugin {
      version: "1.0.0".to_string(),
      execution_delay: Some(Duration::from_millis(100)),
      should_fail: false,
      output_lines: vec!["line 1".to_string(), "line 2".to_string()],
    });

    let logger = Arc::new(MockLogger::new());
    let cancel_token = CancellationToken::new();

    let command = OctaCommand::Execute {
      command: "to_be_cancelled".to_string(),
      args: vec![],
      dir: PathBuf::from("."),
      envs: HashMap::new(),
      vars: HashMap::new(),
    };

    let response_handle = tokio::spawn(async move { read_responses(reader).await });

    // Start command handling
    let handle_future = handle_command(
      command,
      writer,
      active_commands.clone(),
      plugin,
      logger.clone(),
      cancel_token.clone(),
    );

    // Wait for Started response and first output
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Cancel the command
    cancel_token.cancel();

    let result = handle_future.await;
    assert!(result.is_ok());

    let responses = response_handle.await.unwrap();

    // We should at least see the Started response
    assert!(matches!(responses[0], PluginResponse::Started { .. }));

    // Check if we got any output before cancellation
    if responses.len() > 1 {
      assert!(matches!(responses[1], PluginResponse::Stdout { .. }));
    }

    // Verify cancellation state
    assert!(cancel_token.is_cancelled());

    let mock_logger = logger.as_any().downcast_ref::<MockLogger>().unwrap();
    let log_messages = mock_logger.get_messages().await;
    assert!(!log_messages.is_empty());
  }

  #[tokio::test]
  async fn test_handle_command_unexpected_hello() {
    let (reader, writer) = tokio::io::duplex(1024);
    let writer = Arc::new(Mutex::new(writer));
    let active_commands = Arc::new(Mutex::new(HashMap::new()));

    let plugin = Arc::new(MockPlugin {
      version: "1.0.0".to_string(),
      execution_delay: None,
      should_fail: false,
      output_lines: vec![],
    });

    let logger = Arc::new(MockLogger::new());
    let cancel_token = CancellationToken::new();

    let command = OctaCommand::Hello(Version {
      version: "1.0.0".to_string(),
      features: vec![],
    });

    let response_handle = tokio::spawn(async move { read_responses(reader).await });

    let result = handle_command(command, writer, active_commands, plugin, logger.clone(), cancel_token).await;

    assert!(result.is_ok());

    let responses = response_handle.await.unwrap();
    assert_eq!(responses.len(), 1);
    assert!(matches!(responses[0], PluginResponse::Error { .. }));

    let mock_logger = logger.as_any().downcast_ref::<MockLogger>().unwrap();
    let log_messages = mock_logger.get_messages().await;
    assert!(!log_messages.is_empty());
  }

  #[tokio::test]
  async fn test_handle_command_with_empty_output() {
    let (reader, writer) = tokio::io::duplex(1024);
    let writer = Arc::new(Mutex::new(writer));
    let active_commands = Arc::new(Mutex::new(HashMap::new()));

    let plugin = Arc::new(MockPlugin {
      version: "1.0.0".to_string(),
      execution_delay: None,
      should_fail: false,
      output_lines: vec![],
    });

    let logger = Arc::new(MockLogger::new());
    let cancel_token = CancellationToken::new();

    let command = OctaCommand::Execute {
      command: "empty_output".to_string(),
      args: vec![],
      dir: PathBuf::from("."),
      envs: HashMap::new(),
      vars: HashMap::new(),
    };

    let response_handle = tokio::spawn(async move { read_responses(reader).await });

    let result = handle_command(
      command,
      writer,
      active_commands.clone(),
      plugin,
      logger.clone(),
      cancel_token,
    )
    .await;

    assert!(result.is_ok());

    let responses = response_handle.await.unwrap();
    assert_eq!(responses.len(), 2); // Only Started and ExitStatus
    assert!(matches!(responses[0], PluginResponse::Started { .. }));
    assert!(matches!(responses[1], PluginResponse::ExitStatus { .. }));

    let mock_logger = logger.as_any().downcast_ref::<MockLogger>().unwrap();
    let log_messages = mock_logger.get_messages().await;
    assert!(!log_messages.is_empty());
  }

  #[tokio::test]
  async fn test_command_with_environment_variables() {
    let (reader, writer) = tokio::io::duplex(1024);
    let writer = Arc::new(Mutex::new(writer));
    let active_commands = Arc::new(Mutex::new(HashMap::new()));

    let plugin = Arc::new(MockPlugin {
      version: "1.0.0".to_string(),
      execution_delay: None,
      should_fail: false,
      output_lines: vec!["env test".to_string()],
    });

    let logger = Arc::new(MockLogger::new());
    let cancel_token = CancellationToken::new();

    let mut envs = HashMap::new();
    envs.insert("TEST_VAR1".to_string(), "value1".to_string());
    envs.insert("TEST_VAR2".to_string(), "value2".to_string());

    let command = OctaCommand::Execute {
      command: "env_test".to_string(),
      args: vec![],
      dir: PathBuf::from("."),
      envs,
      vars: HashMap::new(),
    };

    let response_handle = tokio::spawn(async move { read_responses(reader).await });

    let result = handle_command(
      command,
      writer,
      active_commands.clone(),
      plugin,
      logger.clone(),
      cancel_token,
    )
    .await;

    assert!(result.is_ok());

    let responses = response_handle.await.unwrap();
    assert!(matches!(responses[0], PluginResponse::Started { .. }));
    assert!(matches!(responses[1], PluginResponse::Stdout { .. }));
    assert!(matches!(responses[2], PluginResponse::ExitStatus { .. }));

    let mock_logger = logger.as_any().downcast_ref::<MockLogger>().unwrap();
    let log_messages = mock_logger.get_messages().await;
    assert!(!log_messages.is_empty());
  }

  #[tokio::test]
  async fn test_active_commands_cleanup() {
    let (reader, writer) = tokio::io::duplex(1024);
    let writer = Arc::new(Mutex::new(writer));
    let active_commands = Arc::new(Mutex::new(HashMap::new()));

    let plugin = Arc::new(MockPlugin {
      version: "1.0.0".to_string(),
      execution_delay: Some(Duration::from_millis(50)),
      should_fail: false,
      output_lines: vec!["test".to_string()],
    });

    let logger = Arc::new(MockLogger::new());
    let cancel_token = CancellationToken::new();

    let command = OctaCommand::Execute {
      command: "test".to_string(),
      args: vec![],
      dir: PathBuf::from("."),
      envs: HashMap::new(),
      vars: HashMap::new(),
    };

    let _response_handle = tokio::spawn(async move { read_responses(reader).await });

    let result = handle_command(
      command,
      writer,
      active_commands.clone(),
      plugin,
      logger.clone(),
      cancel_token,
    )
    .await;

    assert!(result.is_ok());

    // Verify command was added to active_commands
    {
      let commands = active_commands.lock().await;
      assert_eq!(commands.len(), 1);
    }

    // Wait for command to complete
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Verify command handle has completed
    {
      let commands = active_commands.lock().await;
      for (_, handle) in commands.iter() {
        assert!(handle.is_finished());
      }
    }

    let mock_logger = logger.as_any().downcast_ref::<MockLogger>().unwrap();
    let log_messages = mock_logger.get_messages().await;
    assert!(!log_messages.is_empty());
  }

  #[tokio::test]
  async fn test_handle_command_concurrent_execution() {
    let (reader, writer) = tokio::io::duplex(1024);
    let writer = Arc::new(Mutex::new(writer));
    let active_commands = Arc::new(Mutex::new(HashMap::new()));

    let plugin = Arc::new(MockPlugin {
      version: "1.0.0".to_string(),
      execution_delay: None,
      should_fail: false,
      output_lines: vec!["output".to_string()],
    });

    let logger = Arc::new(MockLogger::new());
    let cancel_token = CancellationToken::new();

    // Launch multiple commands concurrently
    let mut handles = vec![];
    for i in 0..3 {
      let command = OctaCommand::Execute {
        command: format!("cmd{}", i),
        args: vec![],
        dir: PathBuf::from("."),
        envs: HashMap::new(),
        vars: HashMap::new(),
      };

      let writer_clone = writer.clone();
      let active_commands_clone = active_commands.clone();
      let plugin_clone = plugin.clone();
      let logger_clone = logger.clone();
      let cancel_token_clone = cancel_token.clone();

      handles.push(tokio::spawn(async move {
        handle_command(
          command,
          writer_clone,
          active_commands_clone,
          plugin_clone,
          logger_clone,
          cancel_token_clone,
        )
        .await
      }));
    }

    // Wait for all commands to complete
    for handle in handles {
      let result = handle.await.unwrap();
      assert!(result.is_ok());
    }

    // Close the writer to signal no more data
    drop(writer);

    let response_handle = tokio::spawn(async move { read_responses(reader).await });
    let responses = response_handle.await.unwrap();

    // Verify we got all the expected responses
    let started_count = responses
      .iter()
      .filter(|r| matches!(r, PluginResponse::Started { .. }))
      .count();
    let exit_count = responses
      .iter()
      .filter(|r| matches!(r, PluginResponse::ExitStatus { .. }))
      .count();

    assert_eq!(started_count, 3);
    assert_eq!(exit_count, 3);
  }

  #[tokio::test]
  async fn test_handle_conn() {
    // Create a temporary directory for the socket
    let temp_dir = tempdir().unwrap();
    let socket_path = temp_dir.path().join("test.sock");
    let socket_name_server = interpret_local_socket_name(OsStr::new(&socket_path)).unwrap();
    let socket_name_client = interpret_local_socket_name(OsStr::new(&socket_path)).unwrap();
    let cancel_token = CancellationToken::new();
    let logger = Arc::new(MockLogger::new());

    let plugin = Arc::new(MockPlugin {
      version: "1.0.0".to_string(),
      execution_delay: None,
      should_fail: false,
      output_lines: vec!["test".to_string()],
    });

    let schema = PluginSchema { key: "key".to_owned() };

    // Create a listener for the socket
    let listener = ListenerOptions::new().name(socket_name_server).create_tokio().unwrap();

    // Spawn a task to accept connections
    let listener_handle = {
      let cancel_token = cancel_token.clone();

      tokio::spawn(async move {
        loop {
          tokio::select! {
            result = listener.accept() => {
              match result {
                Ok(stream) => {
                  let _ = handle_conn(stream, plugin.clone(), schema.clone(), logger.clone(), cancel_token.clone()).await;
                }
                Err(_) => {
                  // Handle accept error if necessary
                  break; // Exit the loop on error
                }
              }
            }
            _ = cancel_token.cancelled() => {
              break; // Exit the loop if the token is cancelled
            }
          }
        }
      })
    };

    // Create a client to connect to the server
    let client_stream = Stream::connect(socket_name_client).await.unwrap();
    let (reader, mut writer) = client_stream.split();
    let response_handle = tokio::spawn(async move { read_responses(reader).await });

    // Send a Hello command
    let hello_command = OctaCommand::Hello(Version {
      version: "1.0.0".to_string(),
      features: vec!["feature1".to_string()],
    });
    let hello_json = serde_json::to_string(&hello_command).unwrap() + "\n";
    writer.write_all(hello_json.as_bytes()).await.unwrap();
    writer.flush().await.unwrap();

    // Send a Schema command
    let schema_command = OctaCommand::Schema;
    let schema_json = serde_json::to_string(&schema_command).unwrap() + "\n";
    writer.write_all(schema_json.as_bytes()).await.unwrap();
    writer.flush().await.unwrap();

    let cmd_command = OctaCommand::Execute {
      command: "".to_owned(),
      args: vec!["arg1".to_string(), "arg2".to_string()],
      dir: PathBuf::from("/test/dir"),
      envs: HashMap::new(),
      vars: HashMap::new(),
    };
    let cmd_json = serde_json::to_string(&cmd_command).unwrap() + "\n";
    writer.write_all(cmd_json.as_bytes()).await.unwrap();
    writer.flush().await.unwrap();

    // Clean up
    drop(writer); // Close the writer to signal no more data

    tokio::time::sleep(Duration::from_millis(100)).await;

    cancel_token.cancel();

    let responses = response_handle.await.unwrap();

    // Verify the response
    match &responses[0] {
      PluginResponse::Hello(version) => {
        assert_eq!(version.version, "1.0.0");
        assert_eq!(version.features.len(), 0);
      },
      _ => panic!("Expected Hello response"),
    }

    match &responses[1] {
      PluginResponse::Schema(schema) => {
        assert_eq!(schema.key, "key");
      },
      _ => panic!("Expected Schema response"),
    }

    // Verify the response
    assert!(matches!(&responses[2], PluginResponse::Started { .. }));

    listener_handle.await.unwrap(); // Wait for the listener task to finish
  }
}
