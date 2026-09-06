use std::{
  collections::HashMap,
  io,
  path::PathBuf,
  sync::{Arc, Weak},
  time::Duration,
};

use interprocess::local_socket::{tokio::Stream as TokioStream, traits::tokio::Stream as StreamTrait, Name};
use semver::{Version as SemVersion, VersionReq};
use serde_json::Value;
use tokio::{
  io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf},
  sync::{mpsc, oneshot, watch, Mutex},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use octa_plugin::protocol::{OctaCommand, PluginResponse, ProgressUpdate, Schema, Version};

const CONTROL_RESPONSE_CAPACITY: usize = 16;
const COMMAND_RESPONSE_CAPACITY: usize = 32;
const MAX_PLUGIN_FRAME_BYTES: usize = 1024 * 1024;
const CANCELLED_ROUTE_TTL: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum PluginClientError {
  Io(io::Error),
  SerdeJson(serde_json::Error),
  Protocol(String),
  ConnectionClosed,
  VersionMismatch,
  WriterClosed,
  FrameTooLarge { bytes: usize, limit: usize },
  ResponseQueueOverflow { id: String, capacity: usize },
}

impl From<PluginClientError> for io::Error {
  fn from(err: PluginClientError) -> Self {
    match err {
      PluginClientError::Io(e) => e,
      PluginClientError::SerdeJson(e) => io::Error::new(io::ErrorKind::InvalidData, e),
      PluginClientError::Protocol(msg) => io::Error::other(msg),
      PluginClientError::ConnectionClosed => io::Error::new(io::ErrorKind::ConnectionAborted, "Connection closed"),
      PluginClientError::VersionMismatch => io::Error::other("Version mismatch"),
      PluginClientError::WriterClosed => io::Error::new(io::ErrorKind::ConnectionAborted, "Writer closed"),
      PluginClientError::FrameTooLarge { bytes, limit } => io::Error::new(
        io::ErrorKind::InvalidData,
        format!("Plugin protocol frame contains {bytes} bytes; limit is {limit}"),
      ),
      PluginClientError::ResponseQueueOverflow { id, capacity } => io::Error::other(format!(
        "Plugin command '{id}' produced output faster than Octa could consume it (queue capacity: {capacity})"
      )),
    }
  }
}

impl From<io::Error> for PluginClientError {
  fn from(err: io::Error) -> Self {
    PluginClientError::Io(err)
  }
}

impl From<serde_json::Error> for PluginClientError {
  fn from(err: serde_json::Error) -> Self {
    PluginClientError::SerdeJson(err)
  }
}

impl std::fmt::Display for PluginClientError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      PluginClientError::Io(e) => write!(f, "IO error: {}", e),
      PluginClientError::SerdeJson(e) => write!(f, "JSON error: {}", e),
      PluginClientError::Protocol(msg) => write!(f, "Protocol error: {}", msg),
      PluginClientError::ConnectionClosed => write!(f, "Connection closed"),
      PluginClientError::VersionMismatch => write!(f, "Version mismatch"),
      PluginClientError::WriterClosed => write!(f, "Writer closed"),
      PluginClientError::FrameTooLarge { bytes, limit } => {
        write!(f, "Plugin protocol frame contains {bytes} bytes; limit is {limit}")
      },
      PluginClientError::ResponseQueueOverflow { id, capacity } => write!(
        f,
        "Plugin command '{id}' produced output faster than Octa could consume it (queue capacity: {capacity})"
      ),
    }
  }
}

#[derive(Clone, Debug)]
pub struct PluginClient {
  inner: Arc<PluginClientInner>,
}

#[derive(Debug)]
struct PluginClientInner {
  writer: Mutex<Option<WriteHalf<TokioStream>>>,
  control_rx: Mutex<mpsc::Receiver<PluginResponse>>,
  commands: Mutex<HashMap<String, CommandRoute>>,
  shutdown_signal: CancellationToken,
  connection_closed: CancellationToken,
}

#[derive(Debug)]
struct CommandRoute {
  state: CommandRouteState,
  sender: Option<mpsc::Sender<PluginResponse>>,
  progress: watch::Sender<Option<ProgressUpdate>>,
  overflowed: CancellationToken,
  completed: CancellationToken,
}

#[derive(Debug)]
enum CommandRouteState {
  AwaitingStart(oneshot::Sender<Result<(), PluginClientError>>),
  Running,
  Cancelled,
}

impl Drop for PluginClient {
  fn drop(&mut self) {
    if Arc::strong_count(&self.inner) == 1 {
      self.inner.shutdown_signal.cancel();
    }
  }
}

#[derive(Debug)]
pub struct PluginExecution {
  id: String,
  response_rx: mpsc::Receiver<PluginResponse>,
  progress_rx: watch::Receiver<Option<ProgressUpdate>>,
  overflowed: CancellationToken,
  completed: CancellationToken,
  client: PluginClient,
}

#[derive(Clone, Debug)]
pub struct PluginTerminalInput {
  id: String,
  client: PluginClient,
}

impl PluginTerminalInput {
  pub async fn write(&self, bytes: Vec<u8>) -> Result<(), PluginClientError> {
    self
      .client
      .send(&OctaCommand::Stdin {
        id: self.id.clone(),
        bytes,
      })
      .await
  }

  pub async fn resize(&self, rows: u16, cols: u16) -> Result<(), PluginClientError> {
    self
      .client
      .send(&OctaCommand::Resize {
        id: self.id.clone(),
        rows,
        cols,
      })
      .await
  }

  pub async fn close(&self) -> Result<(), PluginClientError> {
    self.client.send(&OctaCommand::CloseStdin { id: self.id.clone() }).await
  }
}

impl PluginExecution {
  pub fn id(&self) -> &str {
    &self.id
  }

  pub fn terminal_input(&self) -> PluginTerminalInput {
    PluginTerminalInput {
      id: self.id.clone(),
      client: self.client.clone(),
    }
  }

  pub async fn receive_output(
    &mut self,
    cancel_token: &CancellationToken,
  ) -> Result<Option<PluginResponse>, PluginClientError> {
    tokio::select! {
      biased;
      _ = cancel_token.cancelled() => Err(PluginClientError::Protocol("Command cancelled".into())),
      _ = self.overflowed.cancelled() => Err(PluginClientError::ResponseQueueOverflow {
        id: self.id.clone(),
        capacity: COMMAND_RESPONSE_CAPACITY,
      }),
      response = self.response_rx.recv() => Ok(response),
      changed = self.progress_rx.changed() => {
        changed.map_err(|_| PluginClientError::ConnectionClosed)?;
        Ok(self.progress_rx.borrow_and_update().clone().map(|progress| PluginResponse::Progress {
          id: self.id.clone(),
          progress,
        }))
      },
    }
  }

  /// Sends cancellation and consumes this command's terminal response only.
  pub async fn cancel_and_wait(&mut self) -> Result<(), PluginClientError> {
    self.client.send(&OctaCommand::Cancel { id: self.id.clone() }).await?;

    let wait = async {
      tokio::select! {
        _ = self.completed.cancelled() => Ok(()),
        _ = self.client.inner.connection_closed.cancelled() => Err(PluginClientError::ConnectionClosed),
      }
    };

    tokio::time::timeout(Duration::from_secs(5), wait)
      .await
      .map_err(|_| PluginClientError::Protocol(format!("Timed out while cancelling command {}", self.id)))?
  }
}

/// Complete payload required to start one plugin command.
pub struct PluginExecutionRequest {
  pub params: String,
  pub dry: bool,
  pub args: Vec<String>,
  pub dir: PathBuf,
  pub vars: HashMap<String, Value>,
  pub envs: HashMap<String, String>,
  pub secret_vars: Vec<String>,
  pub redact_params: bool,
  pub raw: bool,
}

pub async fn connect_to_plugin(socket_path: &Name<'_>) -> io::Result<TokioStream> {
  let mut attempts = 0;
  const MAX_ATTEMPTS: u32 = 50;

  loop {
    match <TokioStream as StreamTrait>::connect(socket_path.to_owned()).await {
      Ok(stream) => return Ok(stream),
      Err(e) => {
        if attempts >= MAX_ATTEMPTS {
          return Err(e);
        }
        attempts += 1;
        tokio::time::sleep(Duration::from_millis(100)).await;
      },
    }
  }
}

impl PluginClient {
  pub async fn connect(socket_name: &Name<'_>) -> Result<Self, PluginClientError> {
    let stream = connect_to_plugin(socket_name).await.map_err(PluginClientError::Io)?;
    let (reader, writer) = tokio::io::split(stream);

    let (control_tx, control_rx) = mpsc::channel(CONTROL_RESPONSE_CAPACITY);

    let inner = Arc::new(PluginClientInner {
      writer: Mutex::new(Some(writer)),
      control_rx: Mutex::new(control_rx),
      commands: Mutex::new(HashMap::new()),
      shutdown_signal: CancellationToken::new(),
      connection_closed: CancellationToken::new(),
    });

    Self::start_response_handler(
      reader,
      Arc::downgrade(&inner),
      inner.shutdown_signal.clone(),
      control_tx,
    );

    Ok(Self { inner })
  }

  async fn send(&self, command: &OctaCommand) -> Result<(), PluginClientError> {
    let json = encode_command(command)?;
    let mut writer = self.inner.writer.lock().await;
    let writer = writer.as_mut().ok_or(PluginClientError::WriterClosed)?;
    if let Err(error) = writer.write_all(json.as_bytes()).await {
      if error.kind() == io::ErrorKind::BrokenPipe {
        return Err(PluginClientError::ConnectionClosed);
      }
      return Err(error.into());
    }
    writer.flush().await?;
    Ok(())
  }

  async fn receive_control(&self) -> Result<PluginResponse, PluginClientError> {
    self
      .inner
      .control_rx
      .lock()
      .await
      .recv()
      .await
      .ok_or(PluginClientError::ConnectionClosed)
  }

  pub async fn handshake(&self) -> Result<(), PluginClientError> {
    let hello = OctaCommand::Hello(Version {
      version: env!("CARGO_PKG_VERSION").to_string(),
      features: vec![],
    });
    self.send(&hello).await?;

    match self.receive_control().await? {
      PluginResponse::Hello(version) => {
        let octa_version = SemVersion::parse(env!("CARGO_PKG_VERSION")).unwrap();
        let req_version = VersionReq::parse(&version.version).unwrap();

        if !req_version.matches(&octa_version) {
          return Err(PluginClientError::VersionMismatch);
        }

        Ok(())
      },
      PluginResponse::Error { message, .. } => Err(PluginClientError::Protocol(message)),
      _ => Err(PluginClientError::Protocol("Unexpected response to Hello".into())),
    }
  }

  pub async fn get_schema(&self) -> Result<Schema, PluginClientError> {
    self.send(&OctaCommand::Schema).await?;

    match self.receive_control().await? {
      PluginResponse::Schema(schema) => Ok(schema),
      PluginResponse::Error { message, .. } => Err(PluginClientError::Protocol(message)),
      _ => Err(PluginClientError::Protocol("Unexpected response to Schema".into())),
    }
  }

  fn start_response_handler(
    reader: ReadHalf<TokioStream>,
    inner: Weak<PluginClientInner>,
    shutdown_signal: CancellationToken,
    control_tx: mpsc::Sender<PluginResponse>,
  ) {
    tokio::spawn(async move {
      let mut reader = BufReader::new(reader);
      let mut buffer = String::new();

      loop {
        buffer.clear();
        let read = tokio::select! {
          _ = shutdown_signal.cancelled() => break,
          result = read_plugin_frame(&mut reader, &mut buffer) => result,
        };
        match read {
          Ok(0) => break,
          Ok(_) => {
            let response =
              serde_json::from_str::<PluginResponse>(buffer.trim()).unwrap_or_else(|error| PluginResponse::Error {
                id: "parse_error".to_string(),
                message: format!("Invalid JSON response: {error}"),
              });
            let Some(inner) = inner.upgrade() else {
              break;
            };
            if !Self::dispatch_response(&inner, &control_tx, response).await {
              break;
            }
          },
          Err(_) => break,
        }
      }

      if let Some(inner) = inner.upgrade() {
        inner.connection_closed.cancel();
        for (_, route) in inner.commands.lock().await.drain() {
          route.completed.cancel();
          if let CommandRouteState::AwaitingStart(pending) = route.state {
            let _ = pending.send(Err(PluginClientError::ConnectionClosed));
          }
        }
      }
    });
  }

  async fn dispatch_response(
    inner: &Arc<PluginClientInner>,
    control_tx: &mpsc::Sender<PluginResponse>,
    response: PluginResponse,
  ) -> bool {
    if let PluginResponse::Started { id } = &response {
      let pending = {
        let mut commands = inner.commands.lock().await;
        let Some(route) = commands.get_mut(id) else {
          drop(commands);
          return Self::fail_protocol(inner, format!("Plugin acknowledged unknown command '{id}'")).await;
        };
        match std::mem::replace(&mut route.state, CommandRouteState::Running) {
          CommandRouteState::AwaitingStart(pending) => Some(pending),
          CommandRouteState::Cancelled => {
            route.state = CommandRouteState::Cancelled;
            None
          },
          CommandRouteState::Running => {
            drop(commands);
            return Self::fail_protocol(inner, format!("Plugin acknowledged command '{id}' more than once")).await;
          },
        }
      };

      if pending.is_some_and(|pending| pending.send(Ok(())).is_err()) {
        let client = PluginClient { inner: inner.clone() };
        let id = id.clone();
        tokio::spawn(async move {
          client.mark_cancelled(&id).await;
          let _ = client.send(&OctaCommand::Cancel { id }).await;
        });
      }
      return true;
    }

    if let PluginResponse::Progress { id, progress } = response {
      let mut commands = inner.commands.lock().await;
      if let Some(route) = commands.get_mut(&id) {
        match route.state {
          CommandRouteState::Running => {
            route.progress.send_replace(Some(progress));
          },
          CommandRouteState::Cancelled => {},
          CommandRouteState::AwaitingStart(_) => {
            drop(commands);
            return Self::fail_protocol(
              inner,
              format!("Plugin sent progress before acknowledging command '{id}'"),
            )
            .await;
          },
        }
      }
      return true;
    }

    let (id, terminal) = match &response {
      PluginResponse::Stdout { id, .. }
      | PluginResponse::Stderr { id, .. }
      | PluginResponse::StdoutBytes { id, .. }
      | PluginResponse::StderrBytes { id, .. }
      | PluginResponse::Diagnostic { id, .. } => (Some(id.clone()), false),
      PluginResponse::ExitStatus { id, .. } | PluginResponse::Error { id, .. } => (Some(id.clone()), true),
      _ => (None, false),
    };
    if let Some(id) = id {
      let mut commands = inner.commands.lock().await;
      if terminal {
        if let Some(mut route) = commands.remove(&id) {
          route.completed.cancel();
          match route.state {
            CommandRouteState::AwaitingStart(pending) => {
              let error = match response {
                PluginResponse::Error { message, .. } => PluginClientError::Protocol(message),
                _ => PluginClientError::Protocol(format!("Plugin completed command '{id}' before acknowledging it")),
              };
              let _ = pending.send(Err(error));
            },
            CommandRouteState::Running => {
              if let Some(sender) = route.sender.take() {
                if sender.try_send(response).is_err() {
                  route.overflowed.cancel();
                }
              }
            },
            CommandRouteState::Cancelled => {},
          }
          return true;
        }
      } else if let Some(route) = commands.get_mut(&id) {
        match route.state {
          CommandRouteState::Running => {
            if let Some(sender) = &route.sender {
              match sender.try_send(response) {
                Ok(()) => {},
                Err(mpsc::error::TrySendError::Full(_)) => {
                  route.sender.take();
                  route.overflowed.cancel();
                },
                Err(mpsc::error::TrySendError::Closed(_)) => {
                  route.sender.take();
                },
              }
            }
          },
          CommandRouteState::Cancelled => {},
          CommandRouteState::AwaitingStart(_) => {
            drop(commands);
            return Self::fail_protocol(inner, format!("Plugin sent output before acknowledging command '{id}'")).await;
          },
        }
        return true;
      }
      drop(commands);

      if matches!(response, PluginResponse::Error { .. }) {
        return control_tx.try_send(response).is_ok();
      }
      // Output for an unknown or already completed command cannot be routed and
      // must not pollute the bounded control channel.
      return true;
    }

    control_tx.try_send(response).is_ok()
  }

  async fn fail_protocol(inner: &Arc<PluginClientInner>, message: String) -> bool {
    inner.connection_closed.cancel();
    for (_, route) in inner.commands.lock().await.drain() {
      route.completed.cancel();
      if let CommandRouteState::AwaitingStart(pending) = route.state {
        let _ = pending.send(Err(PluginClientError::Protocol(message.clone())));
      }
    }
    false
  }

  async fn mark_cancelled(&self, id: &str) {
    {
      let mut commands = self.inner.commands.lock().await;
      let Some(route) = commands.get_mut(id) else {
        return;
      };
      route.state = CommandRouteState::Cancelled;
    }
    let inner = self.inner.clone();
    let id = id.to_owned();
    tokio::spawn(async move {
      tokio::time::sleep(CANCELLED_ROUTE_TTL).await;
      let mut commands = inner.commands.lock().await;
      if matches!(
        commands.get(&id).map(|route| &route.state),
        Some(CommandRouteState::Cancelled)
      ) {
        if let Some(route) = commands.remove(&id) {
          route.completed.cancel();
        }
      }
    });
  }

  /// Starts a command and returns an independently routed response stream.
  pub async fn start_execution(
    &self,
    request: PluginExecutionRequest,
    cancel_token: CancellationToken,
  ) -> Result<PluginExecution, PluginClientError> {
    if cancel_token.is_cancelled() {
      return Err(PluginClientError::Protocol("Command cancelled".into()));
    }
    let id = Uuid::new_v4().to_string();
    let cmd = OctaCommand::Execute {
      id: id.clone(),
      params: request.params,
      args: request.args,
      dir: request.dir,
      envs: request.envs,
      vars: request.vars,
      secret_vars: request.secret_vars,
      redact_params: request.redact_params,
      raw: request.raw,
      dry: request.dry,
    };
    let command_json = encode_command(&cmd)?;
    let (started_tx, started_rx) = oneshot::channel();
    let (response_tx, response_rx) = mpsc::channel(COMMAND_RESPONSE_CAPACITY);
    let (progress_tx, progress_rx) = watch::channel(None);
    let overflowed = CancellationToken::new();
    let completed = CancellationToken::new();
    let execution = PluginExecution {
      id: id.clone(),
      response_rx,
      progress_rx,
      overflowed: overflowed.clone(),
      completed: completed.clone(),
      client: self.clone(),
    };
    {
      let mut writer = self.inner.writer.lock().await;
      let writer = writer.as_mut().ok_or(PluginClientError::WriterClosed)?;
      self.inner.commands.lock().await.insert(
        id.clone(),
        CommandRoute {
          state: CommandRouteState::AwaitingStart(started_tx),
          sender: Some(response_tx),
          progress: progress_tx,
          overflowed,
          completed,
        },
      );
      if let Err(error) = writer.write_all(command_json.as_bytes()).await {
        self.inner.commands.lock().await.remove(&id);
        return Err(error.into());
      }
      if let Err(error) = writer.flush().await {
        self.inner.commands.lock().await.remove(&id);
        return Err(error.into());
      }
    }

    tokio::select! {
      result = started_rx => {
        result.unwrap_or(Err(PluginClientError::ConnectionClosed))?;
        Ok(execution)
      },
      _ = cancel_token.cancelled() => {
        self.mark_cancelled(&id).await;
        self.send(&OctaCommand::Cancel { id }).await?;
        Err(PluginClientError::Protocol("Command cancelled".into()))
      },
    }
  }

  pub async fn shutdown(&self) -> Result<(), PluginClientError> {
    let connection_closed = self.inner.control_rx.lock().await.is_closed();
    if connection_closed {
      self.cleanup().await;
      return Ok(());
    }

    let cmd = OctaCommand::Shutdown;
    let cmd_json = serde_json::to_string(&cmd)? + "\n";

    let write_result = {
      let mut writer_guard = self.inner.writer.lock().await;
      if let Some(writer) = writer_guard.as_mut() {
        let res = writer.write_all(cmd_json.as_bytes()).await;
        if res.is_ok() {
          let _ = writer.flush().await;
        }
        res
      } else {
        return Ok(());
      }
    };

    if let Err(error) = write_result {
      if is_connection_closed(&error) {
        self.cleanup().await;
        return Ok(());
      }
      return Err(error.into());
    }

    match tokio::time::timeout(Duration::from_secs(5), self.receive_control()).await {
      Ok(Ok(PluginResponse::Shutdown { .. })) => {
        self.cleanup().await;
        Ok(())
      },
      Ok(Ok(PluginResponse::Error { message, .. })) => Err(PluginClientError::Protocol(message)),
      Ok(Ok(_)) => Err(PluginClientError::Protocol("Expected Shutdown response".into())),
      Ok(Err(PluginClientError::ConnectionClosed)) => {
        self.cleanup().await;
        Ok(())
      },
      Ok(Err(error)) => Err(error),
      Err(_) => {
        self.cleanup().await;
        Ok(())
      },
    }
  }

  async fn cleanup(&self) {
    self.inner.shutdown_signal.cancel();
    self.inner.connection_closed.cancel();

    let mut writer = self.inner.writer.lock().await;
    if let Some(writer) = writer.as_mut() {
      let _ = writer.shutdown().await;
    }
    *writer = None;
  }
}

async fn read_plugin_frame<R: AsyncRead + Unpin>(reader: &mut BufReader<R>, buffer: &mut String) -> io::Result<usize> {
  buffer.clear();
  let mut limited = (&mut *reader).take((MAX_PLUGIN_FRAME_BYTES + 1) as u64);
  let read = limited.read_line(buffer).await?;
  if read == MAX_PLUGIN_FRAME_BYTES + 1 && !buffer.ends_with('\n') {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      format!("Plugin response exceeds the {MAX_PLUGIN_FRAME_BYTES}-byte frame limit"),
    ));
  }
  Ok(read)
}

fn is_connection_closed(error: &io::Error) -> bool {
  matches!(
    error.kind(),
    io::ErrorKind::BrokenPipe
      | io::ErrorKind::ConnectionReset
      | io::ErrorKind::ConnectionAborted
      | io::ErrorKind::NotConnected
      | io::ErrorKind::UnexpectedEof
  ) || error.raw_os_error() == Some(233)
}

fn encode_command(command: &OctaCommand) -> Result<String, PluginClientError> {
  let mut json = serde_json::to_string(command)?;
  if json.len() > MAX_PLUGIN_FRAME_BYTES {
    return Err(PluginClientError::FrameTooLarge {
      bytes: json.len(),
      limit: MAX_PLUGIN_FRAME_BYTES,
    });
  }
  json.push('\n');
  Ok(json)
}

#[cfg(test)]
mod tests {
  use super::*;
  use interprocess::local_socket::{traits::tokio::Listener, ListenerOptions};
  use octa_plugin::socket::interpret_local_socket_name;
  use std::{
    ffi::{OsStr, OsString},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
  };
  use tempfile::TempDir;
  use tokio::io::{AsyncWriteExt, BufReader};

  const TIMEOUT: Duration = Duration::from_secs(5);

  fn execution_request(params: impl Into<String>) -> PluginExecutionRequest {
    PluginExecutionRequest {
      params: params.into(),
      dry: false,
      args: Vec::new(),
      dir: PathBuf::from("."),
      vars: HashMap::new(),
      envs: HashMap::new(),
      secret_vars: Vec::new(),
      redact_params: false,
      raw: false,
    }
  }

  #[test]
  fn client_errors_preserve_context_when_converted_to_io_errors() {
    fn assert_error(error: PluginClientError, message: &str, kind: io::ErrorKind) {
      assert!(error.to_string().contains(message));
      let error = io::Error::from(error);
      assert_eq!(error.kind(), kind);
      assert!(error.to_string().contains(message));
    }

    assert_error(
      PluginClientError::Io(io::Error::new(io::ErrorKind::PermissionDenied, "denied")),
      "denied",
      io::ErrorKind::PermissionDenied,
    );
    assert_error(
      PluginClientError::SerdeJson(serde_json::from_str::<Value>("{").unwrap_err()),
      "EOF",
      io::ErrorKind::InvalidData,
    );
    assert_error(
      PluginClientError::Protocol("bad response".to_owned()),
      "bad response",
      io::ErrorKind::Other,
    );
    assert_error(
      PluginClientError::ConnectionClosed,
      "Connection closed",
      io::ErrorKind::ConnectionAborted,
    );
    assert_error(
      PluginClientError::VersionMismatch,
      "Version mismatch",
      io::ErrorKind::Other,
    );
    assert_error(
      PluginClientError::WriterClosed,
      "Writer closed",
      io::ErrorKind::ConnectionAborted,
    );
    assert_error(
      PluginClientError::FrameTooLarge { bytes: 2, limit: 1 },
      "2 bytes; limit is 1",
      io::ErrorKind::InvalidData,
    );
    assert_error(
      PluginClientError::ResponseQueueOverflow {
        id: "command".to_owned(),
        capacity: 32,
      },
      "command",
      io::ErrorKind::Other,
    );
  }

  #[test]
  fn rejects_oversized_outbound_protocol_frames() {
    let command = OctaCommand::Execute {
      id: "command".to_owned(),
      params: "x".repeat(MAX_PLUGIN_FRAME_BYTES),
      args: Vec::new(),
      dir: PathBuf::from("."),
      envs: HashMap::new(),
      vars: HashMap::new(),
      secret_vars: Vec::new(),
      redact_params: false,
      raw: false,
      dry: false,
    };

    assert!(matches!(
      encode_command(&command),
      Err(PluginClientError::FrameTooLarge {
        limit: MAX_PLUGIN_FRAME_BYTES,
        ..
      })
    ));
  }

  #[tokio::test]
  async fn bounds_inbound_protocol_frames_while_reading() {
    let (mut writer, reader) = tokio::io::duplex(MAX_PLUGIN_FRAME_BYTES + 1);
    let write = tokio::spawn(async move {
      writer.write_all(&vec![b'x'; MAX_PLUGIN_FRAME_BYTES + 1]).await.unwrap();
    });
    let mut reader = BufReader::new(reader);
    let mut frame = String::new();

    let error = read_plugin_frame(&mut reader, &mut frame).await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(frame.len(), MAX_PLUGIN_FRAME_BYTES + 1);
    write.await.unwrap();
  }

  async fn write_response(writer: &Arc<Mutex<WriteHalf<TokioStream>>>, response: PluginResponse) {
    let response = serde_json::to_string(&response).unwrap() + "\n";
    let mut writer = writer.lock().await;
    writer.write_all(response.as_bytes()).await.unwrap();
    writer.flush().await.unwrap();
  }

  struct TestServer {
    listener: Arc<interprocess::local_socket::tokio::Listener>,
    _temp_dir: TempDir,
    stop_signal: Arc<AtomicBool>,
    server_handle: Option<tokio::task::JoinHandle<Vec<String>>>,
    socket_name: Name<'static>,
  }

  impl TestServer {
    async fn new() -> Self {
      let temp_dir = tempfile::tempdir().unwrap();
      let socket_path = temp_dir.path().join("test.sock");
      let socket_path_osstring: OsString = socket_path.into_os_string();
      let name = interpret_local_socket_name(Box::leak(socket_path_osstring.into_boxed_os_str())).unwrap();

      let listener = Arc::new(ListenerOptions::new().name(name.clone()).create_tokio().unwrap());

      Self {
        listener,
        _temp_dir: temp_dir,
        stop_signal: Arc::new(AtomicBool::new(false)),
        server_handle: None,
        socket_name: name,
      }
    }

    fn socket_name(&self) -> &Name<'static> {
      &self.socket_name
    }

    async fn start(&mut self, handle_type: String) {
      let stop_signal = Arc::clone(&self.stop_signal);
      let listener = Arc::clone(&self.listener);

      self.server_handle = Some(tokio::spawn(async move {
        let stream = listener.accept().await.unwrap();
        Self::handle_connection(stream, &handle_type, stop_signal).await
      }));
    }

    async fn start_invalid(&mut self) {
      let listener = Arc::clone(&self.listener);

      self.server_handle = Some(tokio::spawn(async move {
        let mut messages = Vec::new();

        if let Ok(stream) = listener.accept().await {
          let (reader, mut writer) = tokio::io::split(stream);
          let mut reader = BufReader::new(reader);
          let mut buffer = String::new();

          // Read Hello handshake
          if reader.read_line(&mut buffer).await.is_ok() {
            messages.push(buffer.clone());

            // Send Hello response
            let response = PluginResponse::Hello(Version {
              version: env!("CARGO_PKG_VERSION").to_string(),
              features: vec![],
            });
            let response_json = serde_json::to_string(&response).unwrap() + "\n";
            writer.write_all(response_json.as_bytes()).await.unwrap();
            writer.flush().await.unwrap();
            messages.push(response_json);

            // Wait for next client message and respond with invalid JSON
            buffer.clear();
            if reader.read_line(&mut buffer).await.is_ok() {
              messages.push(buffer);
              writer.write_all(b"invalid json\n").await.unwrap();
              writer.flush().await.unwrap();
              messages.push("invalid json\n".to_string());
            }
          }
        }

        messages
      }));
    }

    async fn stop(mut self) -> Vec<String> {
      self.stop_signal.store(true, Ordering::SeqCst);
      if let Some(handle) = self.server_handle.take() {
        match tokio::time::timeout(TIMEOUT, handle).await {
          Ok(result) => result.unwrap_or_default(),
          Err(_) => vec!["Server timeout".to_string()],
        }
      } else {
        vec![]
      }
    }

    async fn handle_connection(stream: TokioStream, handle_type: &str, stop_signal: Arc<AtomicBool>) -> Vec<String> {
      let mut received_messages = Vec::new();
      let (reader, mut writer) = tokio::io::split(stream);
      let mut reader = BufReader::new(reader);
      let mut buffer = String::new();

      while let Ok(n) = reader.read_line(&mut buffer).await {
        if n == 0 || stop_signal.load(Ordering::SeqCst) {
          break;
        }

        println!("Server received: {}", buffer);
        received_messages.push(buffer.clone());

        let response = if buffer.contains("Hello") {
          Some(PluginResponse::Hello(Version {
            version: env!("CARGO_PKG_VERSION").to_string(),
            features: vec![],
          }))
        } else if buffer.contains("Schema") {
          Some(PluginResponse::Schema(Schema {
            key: "key".to_owned(),
            supports_raw: false,
            capabilities: Vec::new(),
            validation_schema: None,
          }))
        } else if buffer.contains("Execute") {
          let mut id = match serde_json::from_str::<OctaCommand>(&buffer).unwrap() {
            OctaCommand::Execute { id, .. } => id,
            _ => unreachable!(),
          };
          if handle_type == "wrong-id" {
            id = "plugin-generated-id".to_owned();
          } else if handle_type == "delayed-start" {
            tokio::time::sleep(Duration::from_millis(50)).await;
          }
          if handle_type == "terminal-before-start" {
            Some(PluginResponse::ExitStatus { id, code: 0 })
          } else if handle_type == "progress-before-start" {
            Some(PluginResponse::Progress {
              id,
              progress: ProgressUpdate {
                message: "early".to_owned(),
                current: None,
                total: None,
                unit: None,
              },
            })
          } else if handle_type == "output-before-start" {
            Some(PluginResponse::Stdout {
              id,
              line: "early".to_owned(),
            })
          } else {
            Some(PluginResponse::Started { id })
          }
        } else if buffer.contains("Cancel") {
          let OctaCommand::Cancel { id } = serde_json::from_str::<OctaCommand>(&buffer).unwrap() else {
            unreachable!()
          };
          Some(PluginResponse::ExitStatus { id, code: -1 })
        } else if buffer.contains("Shutdown") {
          Some(PluginResponse::Shutdown {
            message: "Shutting down".to_string(),
          })
        } else {
          None
        };

        if let Some(response) = response {
          let response_json = serde_json::to_string(&response).unwrap() + "\n";
          println!("Server sending: {}", response_json);
          writer.write_all(response_json.as_bytes()).await.unwrap();
          writer.flush().await.unwrap();

          if handle_type == "duplicate-start" && matches!(response, PluginResponse::Started { .. }) {
            writer.write_all(response_json.as_bytes()).await.unwrap();
            writer.flush().await.unwrap();
          }

          if handle_type == "single" || buffer.contains("Shutdown") {
            break;
          }
        }

        buffer.clear();
      }

      received_messages
    }
  }

  #[tokio::test]
  async fn test_handshake() {
    let mut server = TestServer::new().await;
    server.start("single".to_string()).await;

    let client = tokio::time::timeout(TIMEOUT, PluginClient::connect(server.socket_name()))
      .await
      .expect("Connection timeout")
      .expect("Failed to connect client");

    client.handshake().await.expect("Handshake error");

    client.shutdown().await.expect("Failed to shutdown client");
    let messages = server.stop().await;

    assert!(
      messages.iter().any(|m| m.contains("Hello")),
      "No Hello message found in messages: {:?}",
      messages
    );
  }

  #[tokio::test]
  async fn test_execute_command() {
    let mut server = TestServer::new().await;
    server.start("execute".to_string()).await;

    let client = tokio::time::timeout(TIMEOUT, PluginClient::connect(server.socket_name()))
      .await
      .expect("Connection timeout")
      .expect("Failed to connect client");

    client.handshake().await.expect("Handshake error");

    let execution = tokio::time::timeout(
      TIMEOUT,
      client.start_execution(execution_request("test"), CancellationToken::new()),
    )
    .await
    .expect("Execute timeout")
    .expect("Failed to execute command");

    let execution_id = execution.id().to_owned();
    assert!(Uuid::parse_str(&execution_id).is_ok());

    client.shutdown().await.expect("Failed to shutdown client");
    drop(client);
    let messages = server.stop().await;

    assert!(
      messages.iter().any(|m| m.contains("Hello")),
      "No Hello message found in messages: {:?}",
      messages
    );
    assert!(
      messages.iter().any(|m| m.contains("Execute")),
      "No Execute message found in messages: {:?}",
      messages
    );
    assert!(messages.iter().any(|message| {
      matches!(
        serde_json::from_str::<OctaCommand>(message),
        Ok(OctaCommand::Execute { id, .. }) if id == execution_id
      )
    }));
  }

  #[tokio::test]
  async fn rejects_a_started_id_that_was_not_assigned_by_the_host() {
    let mut server = TestServer::new().await;
    server.start("wrong-id".to_owned()).await;

    let client = PluginClient::connect(server.socket_name()).await.unwrap();
    client.handshake().await.unwrap();
    let error = client
      .start_execution(execution_request("test"), CancellationToken::new())
      .await
      .unwrap_err();

    assert!(matches!(
      error,
      PluginClientError::Protocol(message) if message.contains("unknown command 'plugin-generated-id'")
    ));
    let _ = server.stop().await;
  }

  #[tokio::test]
  async fn terminal_response_before_started_cleans_up_the_route() {
    let mut server = TestServer::new().await;
    server.start("terminal-before-start".to_owned()).await;

    let client = PluginClient::connect(server.socket_name()).await.unwrap();
    client.handshake().await.unwrap();
    let error = client
      .start_execution(execution_request("test"), CancellationToken::new())
      .await
      .unwrap_err();

    assert!(matches!(
      error,
      PluginClientError::Protocol(message) if message.contains("before acknowledging it")
    ));
    assert!(client.inner.commands.lock().await.is_empty());
    client.shutdown().await.unwrap();
    let _ = server.stop().await;
  }

  #[tokio::test]
  async fn rejects_progress_and_output_before_started() {
    for (mode, expected) in [
      ("progress-before-start", "sent progress before acknowledging"),
      ("output-before-start", "sent output before acknowledging"),
    ] {
      let mut server = TestServer::new().await;
      server.start(mode.to_owned()).await;

      let client = PluginClient::connect(server.socket_name()).await.unwrap();
      client.handshake().await.unwrap();
      let error = client
        .start_execution(execution_request("test"), CancellationToken::new())
        .await
        .unwrap_err();
      assert!(matches!(error, PluginClientError::Protocol(message) if message.contains(expected)));
      client.shutdown().await.unwrap();
      let _ = server.stop().await;
    }
  }

  #[tokio::test]
  async fn rejects_duplicate_started_responses() {
    let mut server = TestServer::new().await;
    server.start("duplicate-start".to_owned()).await;

    let client = PluginClient::connect(server.socket_name()).await.unwrap();
    client.handshake().await.unwrap();
    let _execution = client
      .start_execution(execution_request("test"), CancellationToken::new())
      .await
      .unwrap();
    client.inner.connection_closed.cancelled().await;

    client.shutdown().await.unwrap();
    let _ = server.stop().await;
  }

  #[tokio::test]
  async fn routes_out_of_order_started_responses_by_host_id() {
    let mut server = TestServer::new().await;
    let listener = server.listener.clone();
    let (requests_tx, requests_rx) = oneshot::channel();
    server.server_handle = Some(tokio::spawn(async move {
      let stream = listener.accept().await.unwrap();
      let (reader, mut writer) = tokio::io::split(stream);
      let mut reader = BufReader::new(reader);
      let mut line = String::new();
      let mut requests = HashMap::new();
      while requests.len() < 2 {
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        let OctaCommand::Execute { id, params, .. } = serde_json::from_str(line.trim()).unwrap() else {
          panic!("expected execute request");
        };
        requests.insert(params, id);
      }
      for params in ["second", "first"] {
        let response = PluginResponse::Started {
          id: requests[params].clone(),
        };
        writer
          .write_all((serde_json::to_string(&response).unwrap() + "\n").as_bytes())
          .await
          .unwrap();
      }
      writer.flush().await.unwrap();
      requests_tx.send(requests.clone()).unwrap();

      line.clear();
      reader.read_line(&mut line).await.unwrap();
      assert!(matches!(
        serde_json::from_str::<OctaCommand>(line.trim()).unwrap(),
        OctaCommand::Shutdown
      ));
      write_response(
        &Arc::new(Mutex::new(writer)),
        PluginResponse::Shutdown {
          message: "done".to_owned(),
        },
      )
      .await;
      Vec::new()
    }));

    let client = PluginClient::connect(server.socket_name()).await.unwrap();
    let (first, second) = tokio::join!(
      client.start_execution(execution_request("first"), CancellationToken::new()),
      client.start_execution(execution_request("second"), CancellationToken::new())
    );
    let first = first.unwrap();
    let second = second.unwrap();
    let requests = requests_rx.await.unwrap();

    assert_eq!(first.id(), requests["first"]);
    assert_eq!(second.id(), requests["second"]);
    client.shutdown().await.unwrap();
    let _ = server.stop().await;
  }

  #[tokio::test]
  async fn cancellation_before_started_is_sent_with_the_host_id() {
    let mut server = TestServer::new().await;
    server.start("delayed-start".to_owned()).await;

    let client = PluginClient::connect(server.socket_name()).await.unwrap();
    client.handshake().await.unwrap();
    let already_cancelled = CancellationToken::new();
    already_cancelled.cancel();
    assert!(matches!(
      client.start_execution(execution_request("not-sent"), already_cancelled).await,
      Err(PluginClientError::Protocol(message)) if message == "Command cancelled"
    ));

    let cancellation = CancellationToken::new();
    let start = tokio::spawn({
      let client = client.clone();
      let cancellation = cancellation.clone();
      async move { client.start_execution(execution_request("test"), cancellation).await }
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    cancellation.cancel();
    let error = start.await.unwrap().unwrap_err();
    assert!(matches!(
      error,
      PluginClientError::Protocol(message) if message == "Command cancelled"
    ));

    tokio::time::sleep(Duration::from_millis(100)).await;
    client.shutdown().await.unwrap();
    let messages = server.stop().await;
    let execute_id = messages.iter().find_map(|message| {
      let Ok(OctaCommand::Execute { id, .. }) = serde_json::from_str(message) else {
        return None;
      };
      Some(id)
    });
    assert!(messages.iter().any(|message| {
      matches!(
        serde_json::from_str::<OctaCommand>(message),
        Ok(OctaCommand::Cancel { id }) if Some(id.as_str()) == execute_id.as_deref()
      )
    }));
  }

  #[tokio::test]
  async fn dropping_a_start_waiter_cancels_the_acknowledged_command() {
    let mut server = TestServer::new().await;
    server.start("delayed-start".to_owned()).await;

    let client = PluginClient::connect(server.socket_name()).await.unwrap();
    client.handshake().await.unwrap();
    let start = tokio::spawn({
      let client = client.clone();
      async move {
        client
          .start_execution(execution_request("test"), CancellationToken::new())
          .await
      }
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    start.abort();
    assert!(start.await.unwrap_err().is_cancelled());

    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(client.inner.commands.lock().await.is_empty());
    client.shutdown().await.unwrap();
    let messages = server.stop().await;
    assert!(messages.iter().any(|message| {
      matches!(
        serde_json::from_str::<OctaCommand>(message),
        Ok(OctaCommand::Cancel { .. })
      )
    }));
  }

  #[tokio::test]
  async fn cancelled_start_route_expires_when_plugin_never_responds() {
    let mut server = TestServer::new().await;
    let listener = server.listener.clone();
    let (execute_tx, execute_rx) = oneshot::channel();
    server.server_handle = Some(tokio::spawn(async move {
      let stream = listener.accept().await.unwrap();
      let (reader, mut writer) = tokio::io::split(stream);
      let mut reader = BufReader::new(reader);
      let mut line = String::new();

      reader.read_line(&mut line).await.unwrap();
      assert!(matches!(
        serde_json::from_str::<OctaCommand>(line.trim()).unwrap(),
        OctaCommand::Hello(_)
      ));
      let hello = PluginResponse::Hello(Version {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        features: Vec::new(),
      });
      writer
        .write_all((serde_json::to_string(&hello).unwrap() + "\n").as_bytes())
        .await
        .unwrap();
      writer.flush().await.unwrap();

      line.clear();
      reader.read_line(&mut line).await.unwrap();
      let OctaCommand::Execute { id, .. } = serde_json::from_str(line.trim()).unwrap() else {
        panic!("expected execute request");
      };
      execute_tx.send(id.clone()).unwrap();

      line.clear();
      reader.read_line(&mut line).await.unwrap();
      assert!(matches!(
        serde_json::from_str::<OctaCommand>(line.trim()).unwrap(),
        OctaCommand::Cancel { id: cancelled } if cancelled == id
      ));

      line.clear();
      reader.read_line(&mut line).await.unwrap();
      assert!(matches!(
        serde_json::from_str::<OctaCommand>(line.trim()).unwrap(),
        OctaCommand::Shutdown
      ));
      let shutdown = PluginResponse::Shutdown {
        message: "done".to_owned(),
      };
      writer
        .write_all((serde_json::to_string(&shutdown).unwrap() + "\n").as_bytes())
        .await
        .unwrap();
      Vec::new()
    }));

    let client = PluginClient::connect(server.socket_name()).await.unwrap();
    client.handshake().await.unwrap();
    tokio::time::pause();
    let cancellation = CancellationToken::new();
    let start = tokio::spawn({
      let client = client.clone();
      let cancellation = cancellation.clone();
      async move { client.start_execution(execution_request("test"), cancellation).await }
    });
    let id = execute_rx.await.unwrap();
    cancellation.cancel();
    assert!(matches!(
      start.await.unwrap(),
      Err(PluginClientError::Protocol(message)) if message == "Command cancelled"
    ));

    tokio::time::advance(CANCELLED_ROUTE_TTL + Duration::from_millis(1)).await;
    tokio::task::yield_now().await;
    assert!(!client.inner.commands.lock().await.contains_key(&id));

    client.shutdown().await.unwrap();
    let _ = server.stop().await;
  }

  #[tokio::test]
  async fn terminal_input_sends_data_resize_and_close_commands() {
    let mut server = TestServer::new().await;
    server.start("multiple".to_string()).await;

    let client = PluginClient::connect(server.socket_name()).await.unwrap();
    client.handshake().await.unwrap();
    let execution = client
      .start_execution(execution_request("terminal"), CancellationToken::new())
      .await
      .unwrap();
    let execution_id = execution.id().to_owned();
    let input = execution.terminal_input();

    input.write(vec![1, 2, 3]).await.unwrap();
    input.resize(24, 80).await.unwrap();
    input.close().await.unwrap();

    client.shutdown().await.unwrap();
    let messages = server.stop().await;
    let commands: Vec<OctaCommand> = messages
      .iter()
      .filter_map(|message| serde_json::from_str(message).ok())
      .collect();
    assert!(commands.iter().any(|command| matches!(
      command,
      OctaCommand::Stdin { id, bytes } if id == &execution_id && bytes == &[1, 2, 3]
    )));
    assert!(commands.iter().any(|command| matches!(
      command,
      OctaCommand::Resize { id, rows: 24, cols: 80 } if id == &execution_id
    )));
    assert!(commands.iter().any(|command| matches!(
      command,
      OctaCommand::CloseStdin { id } if id == &execution_id
    )));
  }

  #[tokio::test]
  async fn test_cancel_command() {
    let mut server = TestServer::new().await;
    server.start("execute".to_string()).await;

    let client = PluginClient::connect(server.socket_name()).await.unwrap();
    client.handshake().await.unwrap();
    let mut execution = client
      .start_execution(execution_request("test"), CancellationToken::new())
      .await
      .unwrap();

    execution.cancel_and_wait().await.unwrap();
    client.shutdown().await.unwrap();
    drop(client);
    let messages = server.stop().await;

    assert!(messages.iter().any(|message| message.contains("Cancel")));
  }

  #[tokio::test]
  async fn test_shutdown() {
    let mut server = TestServer::new().await;
    server.start("shutdown".to_string()).await;

    let client = tokio::time::timeout(TIMEOUT, PluginClient::connect(server.socket_name()))
      .await
      .expect("Connection timeout")
      .expect("Failed to connect client");

    client.handshake().await.expect("Handshake error");
    client.get_schema().await.expect("Get schema error");

    // Send shutdown command
    tokio::time::timeout(TIMEOUT, client.shutdown())
      .await
      .expect("Shutdown timeout")
      .expect("Failed to shutdown");

    drop(client);
    let messages = server.stop().await;

    assert!(
      messages.iter().any(|m| m.contains("Hello")),
      "No Hello message found in messages: {:?}",
      messages
    );
    assert!(
      messages.iter().any(|m| m.contains("Shutdown")),
      "No Shutdown message found in messages: {:?}",
      messages
    );
  }

  #[tokio::test]
  async fn test_protocol_error_handling() {
    let mut server = TestServer::new().await;
    server.start_invalid().await;

    let client = PluginClient::connect(server.socket_name()).await.unwrap();

    client.handshake().await.expect("Handshake error");

    // Try to execute a command which should receive invalid JSON response
    let result = client
      .start_execution(execution_request("test"), CancellationToken::new())
      .await;

    assert!(result.is_err(), "Execute should fail due to invalid response");

    let messages = server.stop().await;
    assert!(messages.len() >= 4);
    assert!(messages[0].contains("Hello"), "First message should be client Hello");
    assert!(messages[1].contains("Hello"), "Second message should be server Hello");
    assert!(messages[2].contains("Execute"), "Third message should be Execute");
    assert!(
      messages[3].contains("invalid json"),
      "Fourth message should be invalid json"
    );
  }

  #[tokio::test]
  async fn test_connection_timeout() {
    let temp_dir = tempfile::tempdir().unwrap();
    let socket_path = temp_dir.path().join("nonexistent.sock");
    let name = interpret_local_socket_name(OsStr::new(&socket_path)).unwrap();

    let result = tokio::time::timeout(Duration::from_secs(1), PluginClient::connect(&name)).await;

    assert!(result.is_err() || result.unwrap().is_err());
  }

  #[tokio::test]
  async fn test_concurrent_commands() {
    let mut server = TestServer::new().await;
    server.start("multiple".to_string()).await;

    let client = PluginClient::connect(server.socket_name()).await.unwrap();

    // Spawn multiple concurrent commands
    let handles: Vec<_> = (0..5)
      .map(|i| {
        let client = client.clone();
        tokio::spawn(async move {
          client
            .start_execution(execution_request(format!("test{i}")), CancellationToken::new())
            .await
        })
      })
      .collect();

    // All commands should complete successfully
    for handle in handles {
      let result = handle.await.unwrap();
      assert!(result.is_ok());
    }

    client.shutdown().await.expect("Failed to shutdown client");

    let messages = server.stop().await;
    assert!(messages.len() > 5); // Hello + multiple Execute commands
  }

  #[tokio::test]
  async fn routes_outputs_and_coalesces_progress_per_command() {
    let server = TestServer::new().await;
    let listener = Arc::clone(&server.listener);
    let server_handle = tokio::spawn(async move {
      let stream = listener.accept().await.unwrap();
      let (reader, writer) = tokio::io::split(stream);
      let mut reader = BufReader::new(reader);
      let writer = Arc::new(Mutex::new(writer));
      let mut line = String::new();

      loop {
        line.clear();
        if reader.read_line(&mut line).await.unwrap() == 0 {
          break;
        }
        match serde_json::from_str::<OctaCommand>(line.trim()).unwrap() {
          OctaCommand::Hello(_) => {
            write_response(
              &writer,
              PluginResponse::Hello(Version {
                version: env!("CARGO_PKG_VERSION").to_owned(),
                features: Vec::new(),
              }),
            )
            .await;
          },
          OctaCommand::Execute { id, params, .. } => {
            write_response(&writer, PluginResponse::Started { id: id.clone() }).await;
            let writer = writer.clone();
            tokio::spawn(async move {
              let delay = if params == "first" { 80 } else { 10 };
              tokio::time::sleep(Duration::from_millis(delay)).await;
              for current in 0..COMMAND_RESPONSE_CAPACITY * 4 {
                write_response(
                  &writer,
                  PluginResponse::Progress {
                    id: id.clone(),
                    progress: ProgressUpdate {
                      message: "working".to_owned(),
                      current: Some(current as u64),
                      total: Some((COMMAND_RESPONSE_CAPACITY * 4) as u64),
                      unit: None,
                    },
                  },
                )
                .await;
              }
              tokio::time::sleep(Duration::from_millis(20)).await;
              write_response(
                &writer,
                PluginResponse::Stdout {
                  id: id.clone(),
                  line: format!("{params}-output"),
                },
              )
              .await;
              write_response(&writer, PluginResponse::ExitStatus { id, code: 0 }).await;
            });
          },
          OctaCommand::Shutdown => {
            write_response(
              &writer,
              PluginResponse::Shutdown {
                message: "done".to_owned(),
              },
            )
            .await;
            break;
          },
          _ => {},
        }
      }
    });

    let client = PluginClient::connect(server.socket_name()).await.unwrap();
    client.handshake().await.unwrap();
    let (first, second) = tokio::join!(
      client.start_execution(execution_request("first"), CancellationToken::new()),
      client.start_execution(execution_request("second"), CancellationToken::new())
    );

    async fn output(mut execution: PluginExecution) -> (Option<u64>, String) {
      let mut latest_progress = None;
      loop {
        match execution.receive_output(&CancellationToken::new()).await.unwrap() {
          Some(PluginResponse::Progress { progress, .. }) => latest_progress = progress.current,
          Some(PluginResponse::Stdout { line, .. }) => return (latest_progress, line),
          Some(_) => {},
          None => panic!("command response stream closed"),
        }
      }
    }

    let (first, second) = tokio::join!(output(first.unwrap()), output(second.unwrap()));
    let expected_progress = Some((COMMAND_RESPONSE_CAPACITY * 4 - 1) as u64);
    assert_eq!(first, (expected_progress, "first-output".to_owned()));
    assert_eq!(second, (expected_progress, "second-output".to_owned()));

    client.shutdown().await.unwrap();
    server_handle.await.unwrap();
  }

  #[tokio::test]
  async fn overflowing_one_command_does_not_block_other_commands() {
    let server = TestServer::new().await;
    let listener = Arc::clone(&server.listener);
    let server_handle = tokio::spawn(async move {
      let stream = listener.accept().await.unwrap();
      let (reader, writer) = tokio::io::split(stream);
      let mut reader = BufReader::new(reader);
      let writer = Arc::new(Mutex::new(writer));
      let mut line = String::new();

      loop {
        line.clear();
        if reader.read_line(&mut line).await.unwrap() == 0 {
          break;
        }
        match serde_json::from_str::<OctaCommand>(line.trim()).unwrap() {
          OctaCommand::Hello(_) => {
            write_response(
              &writer,
              PluginResponse::Hello(Version {
                version: env!("CARGO_PKG_VERSION").to_owned(),
                features: Vec::new(),
              }),
            )
            .await;
          },
          OctaCommand::Execute { id, params, .. } if params == "noisy" => {
            write_response(&writer, PluginResponse::Started { id: id.clone() }).await;
            for index in 0..=COMMAND_RESPONSE_CAPACITY {
              write_response(
                &writer,
                PluginResponse::Stdout {
                  id: id.clone(),
                  line: index.to_string(),
                },
              )
              .await;
            }
            write_response(&writer, PluginResponse::ExitStatus { id, code: 0 }).await;
          },
          OctaCommand::Execute { id, .. } => {
            write_response(&writer, PluginResponse::Started { id: id.clone() }).await;
            write_response(
              &writer,
              PluginResponse::Stdout {
                id: id.clone(),
                line: "ready".to_owned(),
              },
            )
            .await;
            write_response(&writer, PluginResponse::ExitStatus { id, code: 0 }).await;
          },
          OctaCommand::Cancel { id } => {
            write_response(&writer, PluginResponse::ExitStatus { id, code: -1 }).await;
          },
          OctaCommand::Shutdown => {
            write_response(
              &writer,
              PluginResponse::Shutdown {
                message: "done".to_owned(),
              },
            )
            .await;
            break;
          },
          _ => {},
        }
      }
    });

    let client = PluginClient::connect(server.socket_name()).await.unwrap();
    client.handshake().await.unwrap();
    let mut noisy = client
      .start_execution(execution_request("noisy"), CancellationToken::new())
      .await
      .unwrap();
    tokio::time::timeout(TIMEOUT, noisy.overflowed.cancelled())
      .await
      .expect("noisy command queue did not overflow");
    assert!(matches!(
      noisy.receive_output(&CancellationToken::new()).await,
      Err(PluginClientError::ResponseQueueOverflow { .. })
    ));
    noisy.cancel_and_wait().await.unwrap();

    let mut quiet = client
      .start_execution(execution_request("quiet"), CancellationToken::new())
      .await
      .unwrap();
    assert!(matches!(
      quiet.receive_output(&CancellationToken::new()).await.unwrap(),
      Some(PluginResponse::Stdout { line, .. }) if line == "ready"
    ));
    assert!(matches!(
      quiet.receive_output(&CancellationToken::new()).await.unwrap(),
      Some(PluginResponse::ExitStatus { code: 0, .. })
    ));

    client.shutdown().await.unwrap();
    server_handle.await.unwrap();
  }

  #[tokio::test]
  async fn test_shutdown_with_timeout() {
    let mut server = TestServer::new().await;

    // Setup server that never responds to shutdown
    let listener = Arc::clone(&server.listener);
    server.server_handle = Some(tokio::spawn(async move {
      let mut messages = Vec::new();
      if let Ok(stream) = listener.accept().await {
        let (reader, mut writer) = tokio::io::split(stream);
        let mut reader = BufReader::new(reader);
        let mut buffer = String::new();

        // Handle handshake
        if reader.read_line(&mut buffer).await.is_ok() {
          messages.push(buffer.clone());
          let response = PluginResponse::Hello(Version {
            version: env!("CARGO_PKG_VERSION").to_string(),
            features: vec![],
          });
          let response_json = serde_json::to_string(&response).unwrap() + "\n";
          writer.write_all(response_json.as_bytes()).await.unwrap();
          writer.flush().await.unwrap();

          // Wait for Schema command
          buffer.clear();
          if reader.read_line(&mut buffer).await.is_ok() {
            messages.push(buffer.clone());

            let response = PluginResponse::Schema(Schema {
              key: "key".to_owned(),
              supports_raw: false,
              capabilities: Vec::new(),
              validation_schema: None,
            });
            let response_json = serde_json::to_string(&response).unwrap() + "\n";
            writer.write_all(response_json.as_bytes()).await.unwrap();
            writer.flush().await.unwrap();
          }

          // Wait for shutdown command but don't respond
          buffer.clear();
          if reader.read_line(&mut buffer).await.is_ok() {
            messages.push(buffer);
            // Just wait without responding
            tokio::time::sleep(Duration::from_secs(6)).await;
          }
        }
      }
      messages
    }));

    let client = PluginClient::connect(server.socket_name()).await.unwrap();
    client.handshake().await.expect("Handshake failed");
    client.get_schema().await.expect("Get schema failed");

    // Shutdown should succeed with timeout
    let result = client.shutdown().await;
    assert!(result.is_ok(), "Shutdown should succeed with timeout");

    let messages = server.stop().await;
    assert!(messages.iter().any(|m| m.contains("Shutdown")));
  }

  #[tokio::test]
  async fn test_shutdown_with_connection_closed() {
    let mut server = TestServer::new().await;

    // Setup server that closes connection after handshake
    let listener = Arc::clone(&server.listener);
    server.server_handle = Some(tokio::spawn(async move {
      let mut messages = Vec::new();
      if let Ok(stream) = listener.accept().await {
        let (reader, mut writer) = tokio::io::split(stream);
        let mut reader = BufReader::new(reader);
        let mut buffer = String::new();

        // Handle handshake
        if reader.read_line(&mut buffer).await.is_ok() {
          messages.push(buffer.clone());
          let response = PluginResponse::Hello(Version {
            version: env!("CARGO_PKG_VERSION").to_string(),
            features: vec![],
          });
          let response_json = serde_json::to_string(&response).unwrap() + "\n";
          writer.write_all(response_json.as_bytes()).await.unwrap();
          writer.flush().await.unwrap();

          // Close connection immediately
          drop(writer);
        }
      }
      messages
    }));

    let client = PluginClient::connect(server.socket_name()).await.unwrap();
    client.handshake().await.expect("Handshake failed");

    // Give some time for server to close connection
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Shutdown should succeed when connection is already closed
    let result = client.shutdown().await;
    assert!(result.is_ok(), "Shutdown should succeed when connection is closed");

    let messages = server.stop().await;
    assert!(!messages.is_empty());
  }

  #[tokio::test]
  async fn test_shutdown_after_multiple_commands() {
    let mut server = TestServer::new().await;
    server.start("multiple".to_string()).await;

    let client = PluginClient::connect(server.socket_name()).await.unwrap();
    client.handshake().await.expect("Handshake failed");

    // Execute multiple commands before shutdown
    for i in 0..3 {
      let result = client
        .start_execution(execution_request(format!("test{i}")), CancellationToken::new())
        .await;
      assert!(result.is_ok());
    }

    // Shutdown should succeed
    let result = client.shutdown().await;
    assert!(result.is_ok(), "Shutdown should succeed after multiple commands");

    let messages = server.stop().await;
    assert!(messages.iter().any(|m| m.contains("Hello")));
    assert!(messages.iter().any(|m| m.contains("Execute")));
    assert!(messages.iter().any(|m| m.contains("Shutdown")));
  }

  #[tokio::test]
  async fn test_shutdown_with_error_response() {
    let mut server = TestServer::new().await;

    // Setup server that responds with error to shutdown
    let listener = Arc::clone(&server.listener);
    server.server_handle = Some(tokio::spawn(async move {
      let mut messages = Vec::new();
      if let Ok(stream) = listener.accept().await {
        let (reader, mut writer) = tokio::io::split(stream);
        let mut reader = BufReader::new(reader);
        let mut buffer = String::new();

        // Handle handshake
        if reader.read_line(&mut buffer).await.is_ok() {
          messages.push(buffer.clone());
          let response = PluginResponse::Hello(Version {
            version: env!("CARGO_PKG_VERSION").to_string(),
            features: vec![],
          });
          let response_json = serde_json::to_string(&response).unwrap() + "\n";
          writer.write_all(response_json.as_bytes()).await.unwrap();
          writer.flush().await.unwrap();

          // Wait for shutdown command and respond with error
          buffer.clear();
          if reader.read_line(&mut buffer).await.is_ok() {
            messages.push(buffer);
            let error_response = PluginResponse::Error {
              id: "shutdown_error".to_string(),
              message: "Failed to shutdown".to_string(),
            };
            let error_json = serde_json::to_string(&error_response).unwrap() + "\n";
            writer.write_all(error_json.as_bytes()).await.unwrap();
            writer.flush().await.unwrap();
          }
        }
      }
      messages
    }));

    let client = PluginClient::connect(server.socket_name()).await.unwrap();
    client.handshake().await.expect("Handshake failed");

    // Shutdown should return error
    let result = client.shutdown().await;
    assert!(matches!(result, Err(PluginClientError::Protocol(_))));

    let messages = server.stop().await;
    assert!(messages.iter().any(|m| m.contains("Shutdown")));
  }

  #[tokio::test]
  async fn test_double_shutdown() {
    let mut server = TestServer::new().await;
    server.start("shutdown".to_string()).await;

    let client = PluginClient::connect(server.socket_name()).await.unwrap();
    client.handshake().await.expect("Handshake failed");

    // First shutdown
    let result1 = client.shutdown().await;
    assert!(result1.is_ok(), "First shutdown should succeed");

    // Shutdown is idempotent after the response dispatcher has stopped.
    let result2 = client.shutdown().await;
    assert!(result2.is_ok());

    let messages = server.stop().await;
    assert!(messages.iter().any(|m| m.contains("Shutdown")));
  }
}
