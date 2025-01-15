use std::{
  collections::HashMap,
  ffi::OsStr,
  io,
  path::PathBuf,
  sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
  },
  time::Duration,
};

use async_trait::async_trait;
use clap::Parser;
use futures::StreamExt;
use interprocess::local_socket::{
  tokio::{prelude::*, Stream},
  ListenerOptions,
};
use lazy_static::lazy_static;
use logger::{Logger, LoggerSystem, PluginLogger};
use socket::interpret_local_socket_name;
use tokio::io::AsyncWrite;
use tokio::{
  io::{split, AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader, WriteHalf},
  sync::{broadcast, Mutex},
  task::JoinHandle,
};
use tokio_util::{
  codec::{FramedRead, LinesCodec},
  sync::CancellationToken,
};
use uuid::Uuid;

use protocol::{ClientCommand, ServerResponse, Version};

pub mod logger;
pub mod protocol;
pub mod socket;

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

lazy_static! {
  pub static ref SERVER_RUNNING: AtomicBool = AtomicBool::new(true);
}

lazy_static! {
  static ref SHUTDOWN_NOTIFY: (broadcast::Sender<()>, std::sync::Mutex<Option<broadcast::Receiver<()>>>) = {
    let (tx, rx) = broadcast::channel(1);
    (tx, std::sync::Mutex::new(Some(rx)))
  };
}

pub fn stop_server() {
  SERVER_RUNNING.store(false, Ordering::SeqCst);
  let _ = SHUTDOWN_NOTIFY.0.send(());
}

#[async_trait]
pub trait Plugin: Send + Sync + 'static {
  fn version(&self) -> String;

  #[allow(clippy::too_many_arguments)]
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
          "stdout" => ServerResponse::Stdout { id: id.clone(), line },
          "stderr" => ServerResponse::Stderr { id: id.clone(), line },
          _ => unreachable!(),
        };

        let response_json = serde_json::to_string(&response)? + "\n";
        writer.lock().await.write_all(response_json.as_bytes()).await?;
      },
      Err(e) => {
        let error = ServerResponse::Error {
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
  command: ClientCommand,
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
    ClientCommand::Execute {
      command,
      args,
      dir,
      envs,
    } => {
      let id = Uuid::new_v4().to_string();

      logger.log(&format!("Received execute command {:?}", command))?;

      {
        // Send started response
        let start_response = ServerResponse::Started { id: id.clone() };
        let start_json = serde_json::to_string(&start_response)? + "\n";
        writer.lock().await.write_all(start_json.as_bytes()).await?;

        logger.log(&format!("Send Start command for command id '{}'", &id))?;
      }

      // Clone what we need to move into the spawn
      let writer_clone = Arc::clone(&writer);
      let command_id = id.clone();

      // Spawn the command execution
      let handle = tokio::spawn(async move {
        if let Err(e) = plugin
          .execute_command(
            command,
            args,
            dir,
            envs,
            writer_clone.clone(),
            logger,
            command_id.clone(),
            cancel_token.clone(),
          )
          .await
        {
          let error = ServerResponse::Error {
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
    ClientCommand::Hello(_) => {
      let response = ServerResponse::Error {
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
    ClientCommand::Shutdown => {
      logger.log("Receiving shutdown command")?;
      cancel_token.cancel();
      stop_server();
    },
  }
  Ok(())
}

async fn handle_conn(
  conn: Stream,
  plugin: Arc<impl Plugin + 'static>,
  logger: Arc<PluginLogger>,
  cancel_token: CancellationToken,
) -> anyhow::Result<()> {
  let (reader, writer) = split(conn);
  let writer = Arc::new(Mutex::new(writer));
  let active_commands: ActiveCommands = Arc::new(Mutex::new(HashMap::new()));
  let mut reader = BufReader::new(reader);
  let mut buffer = String::new();
  let mut shutdown_rx = SHUTDOWN_NOTIFY.0.subscribe();

  // Wait for octa Hello
  if reader.read_line(&mut buffer).await? == 0 {
    return Ok(());
  }

  match serde_json::from_str(&buffer) {
    Ok(ClientCommand::Hello(client_version)) => {
      if let Err(e) = logger.log(&format!("Client connected with version: {}", client_version.version)) {
        eprintln!("Failed to log message: {}", e);
      }

      // Send server Hello response with plugin version
      let response = ServerResponse::Hello(Version {
        version: plugin.version(),
        features: vec![],
      });
      let response_json = serde_json::to_string(&response)? + "\n";
      writer.lock().await.write_all(response_json.as_bytes()).await?;

      let _ = logger.log(&response_json.to_string());
    },
    Ok(command) => {
      let response = ServerResponse::Error {
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
      let response = ServerResponse::Error {
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

  loop {
    if !SERVER_RUNNING.load(Ordering::SeqCst) {
      break;
    }

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
                let response = ServerResponse::Error {
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
      _ = shutdown_rx.recv() => {
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

  let response = ServerResponse::Shutdown {
    message: "Plugin shutting down".to_string(),
  };
  writer
    .lock()
    .await
    .write_all(format!("{}\n", serde_json::to_string(&response)?).as_bytes())
    .await?;

  Ok(())
}

pub async fn serve_plugin(plugin: impl Plugin + 'static) -> anyhow::Result<()> {
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
    stop_server();
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

  let mut shutdown_rx = SHUTDOWN_NOTIFY.0.subscribe();
  let active_connections = Arc::new(Mutex::new(Vec::new()));

  loop {
    if !SERVER_RUNNING.load(Ordering::SeqCst) {
      break;
    }

    tokio::select! {
      accept_result = listener.accept() => {
        match accept_result {
          Ok(conn) => {
            let plugin_clone = Arc::clone(&plugin);
            let logger_clone = Arc::clone(&logger);
            let cancel_token = cancel_token.clone();
            let handle = tokio::spawn(async move {
              if let Err(e) = handle_conn(conn, plugin_clone, logger_clone, cancel_token).await {
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
      _ = shutdown_rx.recv() => {
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
  use tokio::io::{AsyncWriteExt, BufReader};

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
      command: String,
      args: Vec<String>,
      _dir: PathBuf,
      _envs: HashMap<String, String>,
      writer: Arc<Mutex<impl AsyncWrite + Send + 'static + std::marker::Unpin>>,
      logger: Arc<impl Logger>,
      id: String,
      cancel_token: CancellationToken,
    ) -> anyhow::Result<()> {
      logger.log(&format!("Executing command: {} {:?}", command, args))?;

      for line in &self.output_lines {
        if cancel_token.is_cancelled() {
          return Ok(());
        }

        let stdout = ServerResponse::Stdout {
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

      let response = ServerResponse::ExitStatus {
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

  async fn read_responses(reader: impl AsyncRead + Unpin) -> Vec<ServerResponse> {
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

    let command = ClientCommand::Execute {
      command: "test".to_string(),
      args: vec!["arg1".to_string(), "arg2".to_string()],
      dir: PathBuf::from("/test/dir"),
      envs: {
        let mut map = HashMap::new();
        map.insert("ENV1".to_string(), "value1".to_string());
        map
      },
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

    assert!(matches!(responses[0], ServerResponse::Started { .. }));
    assert!(matches!(responses[1], ServerResponse::Stdout { .. }));
    assert!(matches!(responses[2], ServerResponse::Stdout { .. }));
    assert!(matches!(responses[3], ServerResponse::Stdout { .. }));
    assert!(matches!(responses[4], ServerResponse::ExitStatus { .. }));

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

    let command = ClientCommand::Shutdown;

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
    assert!(!SERVER_RUNNING.load(Ordering::SeqCst));

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

    let command = ClientCommand::Execute {
      command: "failing_command".to_string(),
      args: vec![],
      dir: PathBuf::from("."),
      envs: HashMap::new(),
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

    assert!(matches!(responses[0], ServerResponse::Started { .. }));
    assert!(matches!(responses[1], ServerResponse::Stdout { .. }));
    assert!(matches!(responses[2], ServerResponse::Error { .. }));

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

    let command = ClientCommand::Execute {
      command: "long_running".to_string(),
      args: vec![],
      dir: PathBuf::from("."),
      envs: HashMap::new(),
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

    assert!(matches!(responses[0], ServerResponse::Started { .. }));
    assert!(matches!(responses[1], ServerResponse::Stdout { .. }));
    assert!(matches!(responses[2], ServerResponse::Stdout { .. }));
    assert!(matches!(responses[3], ServerResponse::ExitStatus { .. }));

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

    let command = ClientCommand::Execute {
      command: "to_be_cancelled".to_string(),
      args: vec![],
      dir: PathBuf::from("."),
      envs: HashMap::new(),
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
    assert!(matches!(responses[0], ServerResponse::Started { .. }));

    // Check if we got any output before cancellation
    if responses.len() > 1 {
      assert!(matches!(responses[1], ServerResponse::Stdout { .. }));
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

    let command = ClientCommand::Hello(Version {
      version: "1.0.0".to_string(),
      features: vec![],
    });

    let response_handle = tokio::spawn(async move { read_responses(reader).await });

    let result = handle_command(command, writer, active_commands, plugin, logger.clone(), cancel_token).await;

    assert!(result.is_ok());

    let responses = response_handle.await.unwrap();
    assert_eq!(responses.len(), 1);
    assert!(matches!(responses[0], ServerResponse::Error { .. }));

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

    let command = ClientCommand::Execute {
      command: "empty_output".to_string(),
      args: vec![],
      dir: PathBuf::from("."),
      envs: HashMap::new(),
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
    assert!(matches!(responses[0], ServerResponse::Started { .. }));
    assert!(matches!(responses[1], ServerResponse::ExitStatus { .. }));

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

    let command = ClientCommand::Execute {
      command: "env_test".to_string(),
      args: vec![],
      dir: PathBuf::from("."),
      envs,
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
    assert!(matches!(responses[0], ServerResponse::Started { .. }));
    assert!(matches!(responses[1], ServerResponse::Stdout { .. }));
    assert!(matches!(responses[2], ServerResponse::ExitStatus { .. }));

    let mock_logger = logger.as_any().downcast_ref::<MockLogger>().unwrap();
    let log_messages = mock_logger.get_messages().await;
    assert!(!log_messages.is_empty());
  }

  #[tokio::test]
  async fn test_server_running_state() {
    // Reset server state
    SERVER_RUNNING.store(true, Ordering::SeqCst);

    assert!(SERVER_RUNNING.load(Ordering::SeqCst));

    stop_server();

    assert!(!SERVER_RUNNING.load(Ordering::SeqCst));

    // Reset for other tests
    SERVER_RUNNING.store(true, Ordering::SeqCst);
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

    let command = ClientCommand::Execute {
      command: "test".to_string(),
      args: vec![],
      dir: PathBuf::from("."),
      envs: HashMap::new(),
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
}
