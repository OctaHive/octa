use std::{collections::HashMap, ffi::OsStr, io, path::PathBuf, sync::Arc, time::Duration};

use async_trait::async_trait;
use clap::Parser;
use interprocess::local_socket::{
  tokio::{prelude::*, Stream},
  ListenerOptions,
};
use logger::{collect_value_redactions, redact, Logger, LoggerSystem, RedactingLogger};
use protocol::{OctaCommand, PluginResponse, Schema, Version};
use serde_json::{Map, Value};
use socket::interpret_local_socket_name;
use tokio::io::{AsyncReadExt, AsyncWrite, ReadHalf};
use tokio::{
  io::{split, AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader},
  sync::{mpsc, oneshot, Mutex},
  task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

pub mod logger;
pub mod protocol;
pub mod socket;

/// Capability used for values that execute a command and return its stdout.
pub const SHELL_CAPABILITY: &str = "shell";

#[derive(Clone)]
pub struct PluginSchema {
  pub key: String,
  pub supports_raw: bool,
  pub capabilities: Vec<String>,
  pub validation_schema: Option<Map<String, Value>>,
}

/// Host-side terminal input delivered to an interactive plugin command.
#[derive(Debug)]
pub enum PluginInput {
  Bytes(Vec<u8>),
  Resize { rows: u16, cols: u16 },
  Close,
}

/// Complete, command-scoped input passed to a plugin implementation.
pub struct PluginCommand {
  pub id: String,
  pub dry: bool,
  pub command: String,
  pub args: Vec<String>,
  pub dir: PathBuf,
  pub vars: HashMap<String, Value>,
  pub envs: HashMap<String, String>,
  pub raw: bool,
  pub input: mpsc::UnboundedReceiver<PluginInput>,
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

  async fn execute_command(
    &self,
    command: PluginCommand,
    writer: Arc<Mutex<impl AsyncWrite + Send + 'static + std::marker::Unpin>>,
    logger: Arc<impl Logger>,
    cancel_token: CancellationToken,
  ) -> anyhow::Result<()>;
}

struct ActiveCommand {
  handle: JoinHandle<()>,
  cancel_token: CancellationToken,
  input: mpsc::UnboundedSender<PluginInput>,
}

type ActiveCommands = Arc<Mutex<HashMap<String, ActiveCommand>>>;

pub async fn stream_output<W>(
  mut stream: impl AsyncRead + Unpin,
  output_type: &str,
  writer: Arc<Mutex<W>>,
  id: String,
) -> io::Result<()>
where
  W: AsyncWrite + Send + Unpin,
{
  if !matches!(output_type, "stdout" | "stderr") {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("unsupported output stream: {output_type}"),
    ));
  }
  let mut buffer = vec![0; 8 * 1024];
  loop {
    let count = match stream.read(&mut buffer).await {
      Ok(0) => return Ok(()),
      Ok(count) => count,
      Err(error) => {
        let response = PluginResponse::Error {
          id,
          message: format!("Failed to read {output_type}: {error}"),
        };
        let response_json = serde_json::to_string(&response)? + "\n";
        writer.lock().await.write_all(response_json.as_bytes()).await?;
        return Ok(());
      },
    };
    let bytes = buffer[..count].to_vec();
    let response = if output_type == "stdout" {
      PluginResponse::StdoutBytes { id: id.clone(), bytes }
    } else {
      PluginResponse::StderrBytes { id: id.clone(), bytes }
    };
    let response_json = serde_json::to_string(&response)? + "\n";
    writer.lock().await.write_all(response_json.as_bytes()).await?;
  }
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
      id,
      params,
      args,
      dir,
      envs,
      vars,
      secret_vars,
      redact_params,
      raw,
      dry,
    } => {
      // Pass values rather than names to the logger because plugin diagnostics contain rendered data.
      let mut redactions = Vec::new();
      for value in secret_vars.iter().filter_map(|name| vars.get(name)) {
        collect_value_redactions(value, &mut redactions);
      }
      if redact_params {
        redactions.push(params.clone());
      }
      let command_logger = Arc::new(RedactingLogger::new(logger.clone(), redactions.clone()));

      command_logger.log(&format!("Received execute command {:?}", params))?;

      {
        // Send started response
        let start_response = PluginResponse::Started { id: id.clone() };
        let start_json = serde_json::to_string(&start_response)? + "\n";
        writer.lock().await.write_all(start_json.as_bytes()).await?;

        command_logger.log(&format!("Send Start command for command id '{}'", id))?;
      }

      // Clone what we need to move into the spawn
      let writer_clone = Arc::clone(&writer);
      let command_id = id.clone();
      let command_logger = command_logger.clone();

      // Spawn the command execution
      // Every command gets a child token so it can be stopped without shutting down the plugin.
      let command_cancel_token = cancel_token.child_token();
      let execute_cancel_token = command_cancel_token.clone();
      let (input, input_rx) = mpsc::unbounded_channel();
      let active_commands_for_task = active_commands.clone();
      let (registered_tx, registered_rx) = oneshot::channel();
      let handle = tokio::spawn(async move {
        // Do not finish before the command has been inserted into the registry.
        let _ = registered_rx.await;
        if let Err(e) = plugin
          .execute_command(
            PluginCommand {
              id: command_id.clone(),
              dry,
              command: params,
              args,
              dir,
              vars,
              envs,
              raw,
              input: input_rx,
            },
            writer_clone.clone(),
            command_logger,
            execute_cancel_token,
          )
          .await
        {
          // Plugin errors bypass the logger, so sanitize the protocol response explicitly.
          let error = PluginResponse::Error {
            id: command_id.clone(),
            message: redact(&format!("Command execution error: {}", e), &redactions),
          };
          if let Ok(json) = serde_json::to_string(&error) {
            let error_json = json + "\n";
            let mut lock = writer_clone.lock().await;
            let _ = lock.write_all(error_json.as_bytes()).await;
          }
        }
        active_commands_for_task.lock().await.remove(&command_id);
      });

      // Store the handle with the original id
      active_commands.lock().await.insert(
        id,
        ActiveCommand {
          handle,
          cancel_token: command_cancel_token,
          input,
        },
      );
      let _ = registered_tx.send(());
    },
    OctaCommand::Cancel { id } => {
      if let Some(command) = active_commands.lock().await.get(&id) {
        command.cancel_token.cancel();
      }
    },
    OctaCommand::Stdin { id, bytes } => {
      let input = active_commands
        .lock()
        .await
        .get(&id)
        .map(|command| command.input.clone());
      if let Some(input) = input {
        let _ = input.send(PluginInput::Bytes(bytes));
      }
    },
    OctaCommand::Resize { id, rows, cols } => {
      let input = active_commands
        .lock()
        .await
        .get(&id)
        .map(|command| command.input.clone());
      if let Some(input) = input {
        let _ = input.send(PluginInput::Resize { rows, cols });
      }
    },
    OctaCommand::CloseStdin { id } => {
      let input = active_commands
        .lock()
        .await
        .get(&id)
        .map(|command| command.input.clone());
      if let Some(input) = input {
        let _ = input.send(PluginInput::Close);
      }
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
  let commands = {
    let mut commands = active_commands.lock().await;
    commands.drain().map(|(_, command)| command).collect::<Vec<_>>()
  };
  for command in commands {
    if let Err(e) = command.handle.await {
      logger.log(&format!("Error waiting for complete commands: {}", e))?;
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
      let schema_response = PluginResponse::Schema(Schema {
        key: schema.key,
        supports_raw: schema.supports_raw,
        capabilities: schema.capabilities,
        validation_schema: schema.validation_schema,
      });
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
  use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
  };
  use tempfile::tempdir;
  use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadBuf};

  struct ErrorReader;

  impl AsyncRead for ErrorReader {
    fn poll_read(self: Pin<&mut Self>, _context: &mut Context<'_>, _buffer: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
      Poll::Ready(Err(io::Error::other("broken stream")))
    }
  }

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
      request: PluginCommand,
      writer: Arc<Mutex<impl AsyncWrite + Send + 'static + std::marker::Unpin>>,
      logger: Arc<impl Logger>,
      cancel_token: CancellationToken,
    ) -> anyhow::Result<()> {
      let PluginCommand { id, command, args, .. } = request;
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
  async fn stream_output_forwards_bytes_before_newline_or_eof() {
    let (mut source_writer, source_reader) = tokio::io::duplex(1024);
    let (sink_reader, sink_writer) = tokio::io::duplex(4096);
    let writer = Arc::new(Mutex::new(sink_writer));
    let forwarder = tokio::spawn(stream_output(source_reader, "stdout", writer, "command".to_owned()));
    let mut responses = BufReader::new(sink_reader).lines();

    source_writer.write_all(b"partial").await.unwrap();
    let response = tokio::time::timeout(Duration::from_secs(1), responses.next_line())
      .await
      .expect("partial output was buffered")
      .unwrap()
      .unwrap();

    assert!(matches!(
      serde_json::from_str::<PluginResponse>(&response).unwrap(),
      PluginResponse::StdoutBytes { id, bytes } if id == "command" && bytes == b"partial"
    ));
    drop(source_writer);
    forwarder.await.unwrap().unwrap();
  }

  #[tokio::test]
  async fn stream_output_forwards_stderr_bytes() {
    let (mut source_writer, source_reader) = tokio::io::duplex(1024);
    let (sink_reader, sink_writer) = tokio::io::duplex(4096);
    let forwarder = tokio::spawn(stream_output(
      source_reader,
      "stderr",
      Arc::new(Mutex::new(sink_writer)),
      "command".to_owned(),
    ));
    let mut responses = BufReader::new(sink_reader).lines();

    source_writer.write_all(b"warning").await.unwrap();
    let response = responses.next_line().await.unwrap().unwrap();

    assert!(matches!(
      serde_json::from_str::<PluginResponse>(&response).unwrap(),
      PluginResponse::StderrBytes { id, bytes } if id == "command" && bytes == b"warning"
    ));
    drop(source_writer);
    forwarder.await.unwrap().unwrap();
  }

  #[tokio::test]
  async fn stream_output_reports_read_errors() {
    let (sink_reader, sink_writer) = tokio::io::duplex(4096);

    stream_output(
      ErrorReader,
      "stdout",
      Arc::new(Mutex::new(sink_writer)),
      "command".to_owned(),
    )
    .await
    .unwrap();

    assert!(matches!(
      read_responses(sink_reader).await.as_slice(),
      [PluginResponse::Error { id, message }]
        if id == "command" && message == "Failed to read stdout: broken stream"
    ));
  }

  #[tokio::test]
  async fn stream_output_rejects_an_unknown_stream() {
    let (_, source_reader) = tokio::io::duplex(16);
    let (_, sink_writer) = tokio::io::duplex(16);

    let error = stream_output(
      source_reader,
      "log",
      Arc::new(Mutex::new(sink_writer)),
      "command".to_owned(),
    )
    .await
    .unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
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
      id: "command".to_owned(),
      params: "test".to_string(),
      args: vec!["arg1".to_string(), "arg2".to_string()],
      dir: PathBuf::from("/test/dir"),
      envs: {
        let mut map = HashMap::new();
        map.insert("ENV1".to_string(), "value1".to_string());
        map
      },
      vars: HashMap::new(),
      secret_vars: Vec::new(),
      redact_params: false,
      raw: false,
      dry: false,
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

    assert!(matches!(
      &responses[0],
      PluginResponse::Started { id } if id == "command"
    ));
    assert!(matches!(responses[1], PluginResponse::Stdout { .. }));
    assert!(matches!(responses[2], PluginResponse::Stdout { .. }));
    assert!(matches!(responses[3], PluginResponse::Stdout { .. }));
    assert!(matches!(responses[4], PluginResponse::ExitStatus { .. }));

    let mock_logger = logger.as_any().downcast_ref::<MockLogger>().unwrap();
    let log_messages = mock_logger.get_messages().await;
    assert!(!log_messages.is_empty());
  }

  #[tokio::test]
  async fn secret_producer_payload_is_redacted_from_plugin_logs() {
    let (reader, writer) = tokio::io::duplex(1024);
    let writer = Arc::new(Mutex::new(writer));
    let active_commands = Arc::new(Mutex::new(HashMap::new()));
    let plugin = Arc::new(MockPlugin {
      version: "1.0.0".to_string(),
      execution_delay: None,
      should_fail: false,
      output_lines: Vec::new(),
    });
    let logger = Arc::new(MockLogger::new());
    let secret = "secret-producing-payload";
    let command = OctaCommand::Execute {
      id: "command".to_owned(),
      params: secret.to_owned(),
      args: Vec::new(),
      dir: PathBuf::from("."),
      envs: HashMap::new(),
      vars: HashMap::new(),
      secret_vars: Vec::new(),
      redact_params: true,
      raw: false,
      dry: false,
    };

    let response_handle = tokio::spawn(async move { read_responses(reader).await });
    handle_command(
      command,
      writer,
      active_commands,
      plugin,
      logger.clone(),
      CancellationToken::new(),
    )
    .await
    .unwrap();
    response_handle.await.unwrap();

    let messages = logger
      .as_any()
      .downcast_ref::<MockLogger>()
      .unwrap()
      .get_messages()
      .await;
    assert!(messages.iter().all(|message| !message.contains(secret)));
    assert!(messages.iter().any(|message| message.contains("*****")));
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
  async fn test_handle_command_cancel() {
    let (_reader, writer) = tokio::io::duplex(1024);
    let writer = Arc::new(Mutex::new(writer));
    let active_commands = Arc::new(Mutex::new(HashMap::new()));
    let plugin = Arc::new(MockPlugin {
      version: "1.0.0".to_string(),
      execution_delay: Some(Duration::from_secs(1)),
      should_fail: false,
      output_lines: vec!["first".to_string(), "second".to_string()],
    });

    handle_command(
      OctaCommand::Execute {
        id: "command".to_owned(),
        params: "test".to_string(),
        args: vec![],
        dir: PathBuf::from("."),
        envs: HashMap::new(),
        vars: HashMap::new(),
        secret_vars: Vec::new(),
        redact_params: false,
        raw: false,
        dry: false,
      },
      writer.clone(),
      active_commands.clone(),
      plugin.clone(),
      Arc::new(MockLogger::new()),
      CancellationToken::new(),
    )
    .await
    .unwrap();

    let id = active_commands.lock().await.keys().next().unwrap().clone();
    assert_eq!(id, "command");
    handle_command(
      OctaCommand::Cancel { id: id.clone() },
      writer,
      active_commands.clone(),
      plugin,
      Arc::new(MockLogger::new()),
      CancellationToken::new(),
    )
    .await
    .unwrap();

    assert!(active_commands.lock().await[&id].cancel_token.is_cancelled());
  }

  #[tokio::test]
  async fn test_handle_command_routes_terminal_input_to_the_active_command() {
    let (_reader, writer) = tokio::io::duplex(1024);
    let writer = Arc::new(Mutex::new(writer));
    let active_commands = Arc::new(Mutex::new(HashMap::new()));
    let plugin = Arc::new(MockPlugin {
      version: "1.0.0".to_string(),
      execution_delay: None,
      should_fail: false,
      output_lines: Vec::new(),
    });
    let (input, mut input_rx) = mpsc::unbounded_channel();
    active_commands.lock().await.insert(
      "command".to_owned(),
      ActiveCommand {
        handle: tokio::spawn(std::future::pending()),
        cancel_token: CancellationToken::new(),
        input,
      },
    );

    for command in [
      OctaCommand::Stdin {
        id: "command".to_owned(),
        bytes: vec![1, 2, 3],
      },
      OctaCommand::Resize {
        id: "command".to_owned(),
        rows: 24,
        cols: 80,
      },
      OctaCommand::CloseStdin {
        id: "command".to_owned(),
      },
    ] {
      handle_command(
        command,
        writer.clone(),
        active_commands.clone(),
        plugin.clone(),
        Arc::new(MockLogger::new()),
        CancellationToken::new(),
      )
      .await
      .unwrap();
    }

    assert!(matches!(input_rx.recv().await, Some(PluginInput::Bytes(bytes)) if bytes == [1, 2, 3]));
    assert!(matches!(
      input_rx.recv().await,
      Some(PluginInput::Resize { rows: 24, cols: 80 })
    ));
    assert!(matches!(input_rx.recv().await, Some(PluginInput::Close)));
    active_commands.lock().await.remove("command").unwrap().handle.abort();
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
      id: "command".to_owned(),
      params: "failing_command".to_string(),
      args: vec![],
      dir: PathBuf::from("."),
      envs: HashMap::new(),
      vars: HashMap::new(),
      secret_vars: Vec::new(),
      redact_params: false,
      raw: false,
      dry: false,
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
      id: "command".to_owned(),
      params: "long_running".to_string(),
      args: vec![],
      dir: PathBuf::from("."),
      envs: HashMap::new(),
      vars: HashMap::new(),
      secret_vars: Vec::new(),
      redact_params: false,
      raw: false,
      dry: false,
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
      id: "command".to_owned(),
      params: "to_be_cancelled".to_string(),
      args: vec![],
      dir: PathBuf::from("."),
      envs: HashMap::new(),
      vars: HashMap::new(),
      secret_vars: Vec::new(),
      redact_params: false,
      raw: false,
      dry: false,
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
      id: "command".to_owned(),
      params: "empty_output".to_string(),
      args: vec![],
      dir: PathBuf::from("."),
      envs: HashMap::new(),
      vars: HashMap::new(),
      secret_vars: Vec::new(),
      redact_params: false,
      raw: false,
      dry: false,
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
      id: "command".to_owned(),
      params: "env_test".to_string(),
      args: vec![],
      dir: PathBuf::from("."),
      envs,
      vars: HashMap::new(),
      secret_vars: Vec::new(),
      redact_params: false,
      raw: false,
      dry: false,
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
      id: "command".to_owned(),
      params: "test".to_string(),
      args: vec![],
      dir: PathBuf::from("."),
      envs: HashMap::new(),
      vars: HashMap::new(),
      secret_vars: Vec::new(),
      redact_params: false,
      raw: false,
      dry: false,
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
      for handle in commands.values() {
        assert!(handle.handle.is_finished());
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
        id: format!("command-{i}"),
        params: format!("cmd{}", i),
        args: vec![],
        dir: PathBuf::from("."),
        envs: HashMap::new(),
        vars: HashMap::new(),
        secret_vars: Vec::new(),
        redact_params: false,
        raw: false,
        dry: false,
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

    let schema = PluginSchema {
      key: "key".to_owned(),
      supports_raw: false,
      capabilities: Vec::new(),
      validation_schema: serde_json::json!({ "type": "string" }).as_object().cloned(),
    };

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
      id: "command".to_owned(),
      params: "".to_owned(),
      args: vec!["arg1".to_string(), "arg2".to_string()],
      dir: PathBuf::from("/test/dir"),
      envs: HashMap::new(),
      vars: HashMap::new(),
      secret_vars: Vec::new(),
      redact_params: false,
      raw: false,
      dry: false,
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
        assert_eq!(
          schema.validation_schema,
          serde_json::json!({ "type": "string" }).as_object().cloned()
        );
      },
      _ => panic!("Expected Schema response"),
    }

    // Verify the response
    assert!(matches!(&responses[2], PluginResponse::Started { .. }));

    listener_handle.await.unwrap(); // Wait for the listener task to finish
  }
}
