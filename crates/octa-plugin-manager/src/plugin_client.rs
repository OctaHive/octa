use std::{
  collections::{HashMap, VecDeque},
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

use octa_plugin::protocol::{OctaCommand, PluginResponse, ProgressUpdate, Schema, Version};

const CONTROL_RESPONSE_CAPACITY: usize = 16;
const COMMAND_RESPONSE_CAPACITY: usize = 32;
const MAX_PLUGIN_FRAME_BYTES: usize = 1024 * 1024;

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
  pending_starts: Mutex<VecDeque<oneshot::Sender<Result<PluginExecution, PluginClientError>>>>,
  commands: Mutex<HashMap<String, CommandRoute>>,
  shutdown_signal: CancellationToken,
  connection_closed: CancellationToken,
}

#[derive(Debug)]
struct CommandRoute {
  sender: Option<mpsc::Sender<PluginResponse>>,
  progress: watch::Sender<Option<ProgressUpdate>>,
  overflowed: CancellationToken,
  completed: CancellationToken,
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
      pending_starts: Mutex::new(VecDeque::new()),
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
        for pending in inner.pending_starts.lock().await.drain(..) {
          let _ = pending.send(Err(PluginClientError::ConnectionClosed));
        }
        inner.commands.lock().await.clear();
      }
    });
  }

  async fn dispatch_response(
    inner: &Arc<PluginClientInner>,
    control_tx: &mpsc::Sender<PluginResponse>,
    response: PluginResponse,
  ) -> bool {
    if let PluginResponse::Started { id } = &response {
      let Some(pending) = inner.pending_starts.lock().await.pop_front() else {
        return true;
      };
      let (response_tx, response_rx) = mpsc::channel(COMMAND_RESPONSE_CAPACITY);
      let (progress_tx, progress_rx) = watch::channel(None);
      let overflowed = CancellationToken::new();
      let completed = CancellationToken::new();
      inner.commands.lock().await.insert(
        id.clone(),
        CommandRoute {
          sender: Some(response_tx),
          progress: progress_tx,
          overflowed: overflowed.clone(),
          completed: completed.clone(),
        },
      );
      let execution = PluginExecution {
        id: id.clone(),
        response_rx,
        progress_rx,
        overflowed,
        completed,
        client: PluginClient { inner: inner.clone() },
      };
      if let Err(Ok(mut execution)) = pending.send(Ok(execution)) {
        tokio::spawn(async move {
          let _ = execution.cancel_and_wait().await;
        });
      }
      return true;
    }

    if let PluginResponse::Progress { id, progress } = response {
      if let Some(route) = inner.commands.lock().await.get(&id) {
        route.progress.send_replace(Some(progress));
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
          if let Some(sender) = route.sender.take() {
            if sender.try_send(response).is_err() {
              route.overflowed.cancel();
            }
          }
          return true;
        }
      } else if let Some(route) = commands.get_mut(&id) {
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
        return true;
      }
      drop(commands);

      if let PluginResponse::Error { message, .. } = &response {
        if let Some(pending) = inner.pending_starts.lock().await.pop_front() {
          let _ = pending.send(Err(PluginClientError::Protocol(message.clone())));
          return true;
        }
        return control_tx.try_send(response).is_ok();
      }
      // Output for an unknown or already completed command cannot be routed and
      // must not pollute the bounded control channel.
      return true;
    }

    control_tx.try_send(response).is_ok()
  }

  /// Starts a command and returns an independently routed response stream.
  pub async fn start_execution(
    &self,
    request: PluginExecutionRequest,
    cancel_token: CancellationToken,
  ) -> Result<PluginExecution, PluginClientError> {
    let cmd = OctaCommand::Execute {
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
    {
      // Queue and wire order must match because the protocol assigns the id in Started.
      let mut writer = self.inner.writer.lock().await;
      let writer = writer.as_mut().ok_or(PluginClientError::WriterClosed)?;
      self.inner.pending_starts.lock().await.push_back(started_tx);
      writer.write_all(command_json.as_bytes()).await?;
      writer.flush().await?;
    }

    tokio::select! {
      result = started_rx => result.unwrap_or(Err(PluginClientError::ConnectionClosed)),
      _ = cancel_token.cancelled() => Err(PluginClientError::Protocol("Command cancelled".into())),
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
          let id = if handle_type == "multiple" {
            match serde_json::from_str::<OctaCommand>(&buffer).unwrap() {
              OctaCommand::Execute { params, .. } => params,
              _ => unreachable!(),
            }
          } else {
            "test-id".to_owned()
          };
          Some(PluginResponse::Started { id })
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

    assert_eq!(execution.id(), "test-id");

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
      OctaCommand::Stdin { id, bytes } if id == "terminal" && bytes == &[1, 2, 3]
    )));
    assert!(commands.iter().any(|command| matches!(
      command,
      OctaCommand::Resize { id, rows: 24, cols: 80 } if id == "terminal"
    )));
    assert!(commands.iter().any(|command| matches!(
      command,
      OctaCommand::CloseStdin { id } if id == "terminal"
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
          OctaCommand::Execute { params, .. } => {
            write_response(&writer, PluginResponse::Started { id: params.clone() }).await;
            let writer = writer.clone();
            tokio::spawn(async move {
              let delay = if params == "first" { 80 } else { 10 };
              tokio::time::sleep(Duration::from_millis(delay)).await;
              for current in 0..COMMAND_RESPONSE_CAPACITY * 4 {
                write_response(
                  &writer,
                  PluginResponse::Progress {
                    id: params.clone(),
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
                  id: params.clone(),
                  line: format!("{params}-output"),
                },
              )
              .await;
              write_response(&writer, PluginResponse::ExitStatus { id: params, code: 0 }).await;
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
          OctaCommand::Execute { params, .. } if params == "noisy" => {
            write_response(&writer, PluginResponse::Started { id: params.clone() }).await;
            for index in 0..=COMMAND_RESPONSE_CAPACITY {
              write_response(
                &writer,
                PluginResponse::Stdout {
                  id: params.clone(),
                  line: index.to_string(),
                },
              )
              .await;
            }
            write_response(&writer, PluginResponse::ExitStatus { id: params, code: 0 }).await;
          },
          OctaCommand::Execute { params, .. } => {
            write_response(&writer, PluginResponse::Started { id: params.clone() }).await;
            write_response(
              &writer,
              PluginResponse::Stdout {
                id: params.clone(),
                line: "ready".to_owned(),
              },
            )
            .await;
            write_response(&writer, PluginResponse::ExitStatus { id: params, code: 0 }).await;
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
