use std::{collections::HashMap, io, path::PathBuf, sync::Arc, time::Duration};

use interprocess::local_socket::{tokio::Stream as TokioStream, traits::tokio::Stream as StreamTrait, Name};
use semver::{Version as SemVersion, VersionReq};
use serde_json::Value;
use tokio::{
  io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf},
  sync::{mpsc, Mutex},
};
use tokio_util::sync::CancellationToken;

use octa_plugin::protocol::{ClientCommand, Schema, ServerResponse, Version};

#[derive(Debug)]
pub enum PluginClientError {
  Io(io::Error),
  SerdeJson(serde_json::Error),
  Protocol(String),
  ConnectionClosed,
  VersionMismatch,
  WriterClosed,
}

impl From<PluginClientError> for io::Error {
  fn from(err: PluginClientError) -> Self {
    match err {
      PluginClientError::Io(e) => e,
      PluginClientError::SerdeJson(e) => io::Error::new(io::ErrorKind::InvalidData, e),
      PluginClientError::Protocol(msg) => io::Error::new(io::ErrorKind::Other, msg),
      PluginClientError::ConnectionClosed => io::Error::new(io::ErrorKind::ConnectionAborted, "Connection closed"),
      PluginClientError::VersionMismatch => io::Error::new(io::ErrorKind::Other, "Version mismatch"),
      PluginClientError::WriterClosed => io::Error::new(io::ErrorKind::ConnectionAborted, "Writer closed"),
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
      PluginClientError::WriterClosed => write!(f, "Writter closed"),
    }
  }
}

#[derive(Debug)]
pub struct PluginClient {
  inner: Arc<PluginClientInner>,
  response_rx: mpsc::UnboundedReceiver<ServerResponse>,
  shutdown_signal: Arc<CancellationToken>,
}

#[derive(Debug)]
struct PluginClientInner {
  writer: Mutex<Option<WriteHalf<TokioStream>>>,
  response_tx: mpsc::UnboundedSender<ServerResponse>,
}

impl Drop for PluginClient {
  fn drop(&mut self) {
    // Signal shutdown in drop
    self.shutdown_signal.cancel();
  }
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

    let (response_tx, response_rx) = mpsc::unbounded_channel();
    let shutdown_signal = Arc::new(CancellationToken::new());

    let inner = Arc::new(PluginClientInner {
      writer: Mutex::new(Some(writer)),
      response_tx: response_tx.clone(),
    });

    // Start response handling task with the inner Arc
    Self::start_response_handler(reader, Arc::clone(&inner), Arc::clone(&shutdown_signal));

    let client = Self {
      inner,
      response_rx,
      shutdown_signal,
    };

    Ok(client)
  }

  pub async fn handshake(&mut self) -> Result<(), PluginClientError> {
    let hello = ClientCommand::Hello(Version {
      version: env!("CARGO_PKG_VERSION").to_string(),
      features: vec![],
    });

    let hello_json = serde_json::to_string(&hello)? + "\n";

    {
      let mut writer_guard = self.inner.writer.lock().await;
      if let Some(writer) = writer_guard.as_mut() {
        writer.write_all(hello_json.as_bytes()).await?;
        writer.flush().await?;
      } else {
        return Err(PluginClientError::WriterClosed);
      }
    }

    match self.response_rx.recv().await {
      Some(ServerResponse::Hello(version)) => {
        let octa_version = SemVersion::parse(env!("CARGO_PKG_VERSION")).unwrap();
        let req_version = VersionReq::parse(&version.version).unwrap();

        if !req_version.matches(&octa_version) {
          return Err(PluginClientError::VersionMismatch);
        }

        Ok(())
      },
      Some(ServerResponse::Error { message, .. }) => Err(PluginClientError::Protocol(message)),
      Some(_) => Err(PluginClientError::Protocol("Unexpected response to Hello".into())),
      None => Err(PluginClientError::ConnectionClosed),
    }
  }

  pub async fn get_schema(&mut self) -> Result<Schema, PluginClientError> {
    let schema = ClientCommand::Schema;

    let schema_json = serde_json::to_string(&schema)? + "\n";

    {
      let mut writer_guard = self.inner.writer.lock().await;
      if let Some(writer) = writer_guard.as_mut() {
        writer.write_all(schema_json.as_bytes()).await?;
        writer.flush().await?;
      } else {
        return Err(PluginClientError::WriterClosed);
      }
    }

    match self.response_rx.recv().await {
      Some(ServerResponse::Schema(schema)) => Ok(schema),
      Some(ServerResponse::Error { message, .. }) => Err(PluginClientError::Protocol(message)),
      Some(_) => Err(PluginClientError::Protocol("Unexpected response to Hello".into())),
      None => Err(PluginClientError::ConnectionClosed),
    }
  }

  fn start_response_handler(
    reader: ReadHalf<TokioStream>,
    inner: Arc<PluginClientInner>,
    shutdown_signal: Arc<CancellationToken>,
  ) {
    tokio::spawn(async move {
      let mut reader = BufReader::new(reader);
      let mut buffer = String::new();

      loop {
        // Check shutdown signal
        if shutdown_signal.is_cancelled() {
          break;
        }

        buffer.clear();
        match reader.read_line(&mut buffer).await {
          Ok(0) => break, // Connection closed
          Ok(_) => {
            // Parse the response once and handle any parsing errors
            match serde_json::from_str::<ServerResponse>(buffer.trim()) {
              Ok(response) => {
                let _ = inner.response_tx.send(response.clone());
              },
              Err(e) => {
                // Create error response for invalid JSON
                let error_response = ServerResponse::Error {
                  id: "parse_error".to_string(),
                  message: format!("Invalid JSON response: {}", e),
                };
                let _ = inner.response_tx.send(error_response.clone());
              },
            }
          },
          Err(_) => break,
        }
      }
    });
  }

  pub async fn execute(
    &mut self,
    command: String,
    args: Vec<String>,
    dir: PathBuf,
    vars: HashMap<String, Value>,
    envs: HashMap<String, String>,
    cancel_token: CancellationToken,
  ) -> Result<String, PluginClientError> {
    let cmd = ClientCommand::Execute {
      command,
      args,
      dir,
      envs,
      vars,
    };

    let cmd_json = serde_json::to_string(&cmd)? + "\n";
    {
      let mut writer_guard = self.inner.writer.lock().await;
      if let Some(writer) = writer_guard.as_mut() {
        // Handle write errors that might indicate connection closure
        if let Err(e) = writer.write_all(cmd_json.as_bytes()).await {
          if e.kind() == io::ErrorKind::BrokenPipe {
            return Err(PluginClientError::ConnectionClosed);
          }
          return Err(e.into());
        }
        writer.flush().await?;
      } else {
        return Err(PluginClientError::WriterClosed);
      }
    }

    tokio::select! {
      response = self.response_rx.recv() => {
        match response {
          Some(ServerResponse::Started { id }) => Ok(id),
          Some(ServerResponse::Error { message, .. }) => {
            Err(PluginClientError::Protocol(message))
          }
          Some(resp) => Err(PluginClientError::Protocol(format!("Expected Started response, received {:?}", resp))),
          None => Err(PluginClientError::ConnectionClosed),
        }
      }
      _ = cancel_token.cancelled() => {
        Err(PluginClientError::Protocol("Command cancelled".into()))
      }
    }
  }

  pub async fn receive_output(
    &mut self,
    cancel_token: &CancellationToken,
  ) -> Result<Option<ServerResponse>, PluginClientError> {
    tokio::select! {
      response = self.response_rx.recv() => Ok(response),
      _ = cancel_token.cancelled() => {
        Err(PluginClientError::Protocol("Command cancelled".into()))
      }
    }
  }

  pub async fn shutdown(&mut self) -> Result<(), PluginClientError> {
    if self.response_rx.is_closed() {
      // Connection was already closed from server side
      self.cleanup().await;

      return Ok(());
    }

    let cmd = ClientCommand::Shutdown;
    let cmd_json = serde_json::to_string(&cmd)? + "\n";

    // Try to write shutdown command
    let write_result = {
      let mut writer_guard = self.inner.writer.lock().await;
      if let Some(writer) = writer_guard.as_mut() {
        let res = writer.write_all(cmd_json.as_bytes()).await;
        if res.is_ok() {
          let _ = writer.flush().await;
        }

        res
      } else {
        return Err(PluginClientError::WriterClosed);
      }
    };

    if let Err(e) = write_result {
      if e.kind() == std::io::ErrorKind::BrokenPipe {
        // Connection was closed by server
        self.cleanup().await;

        return Ok(());
      }

      // Process windows specific error
      if let Some(error_code) = e.raw_os_error() {
        if error_code == 233 {
          return Ok(());
        }
      }

      return Err(e.into());
    }

    match tokio::time::timeout(std::time::Duration::from_secs(5), self.response_rx.recv()).await {
      Ok(Some(ServerResponse::Shutdown { .. })) => {
        // Normal shutdown
        self.cleanup().await;

        Ok(())
      },
      Ok(Some(ServerResponse::Error { message, .. })) => Err(PluginClientError::Protocol(message)),
      Ok(Some(_)) => Err(PluginClientError::Protocol("Expected Shutdown response".into())),
      Ok(None) => {
        // Channel closed without response
        self.cleanup().await;
        Err(PluginClientError::ConnectionClosed)
      },
      Err(_) => {
        // Timeout
        self.cleanup().await;
        Ok(())
      },
    }
  }

  async fn cleanup(&self) {
    self.shutdown_signal.cancel();

    // Wait while stop reader
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut writer_lock = self.inner.writer.lock().await;
    if writer_lock.is_some() {
      let _ = writer_lock.as_mut().unwrap().shutdown().await;
      *writer_lock = None;
    }
  }
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
          if let Ok(_) = reader.read_line(&mut buffer).await {
            messages.push(buffer.clone());

            // Send Hello response
            let response = ServerResponse::Hello(Version {
              version: env!("CARGO_PKG_VERSION").to_string(),
              features: vec![],
            });
            let response_json = serde_json::to_string(&response).unwrap() + "\n";
            writer.write_all(response_json.as_bytes()).await.unwrap();
            writer.flush().await.unwrap();
            messages.push(response_json);

            // Wait for next client message and respond with invalid JSON
            buffer.clear();
            if let Ok(_) = reader.read_line(&mut buffer).await {
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
          Some(ServerResponse::Hello(Version {
            version: env!("CARGO_PKG_VERSION").to_string(),
            features: vec![],
          }))
        } else if buffer.contains("Schema") {
          Some(ServerResponse::Schema(Schema { key: "key".to_owned() }))
        } else if buffer.contains("Execute") {
          Some(ServerResponse::Started {
            id: "test-id".to_string(),
          })
        } else if buffer.contains("Shutdown") {
          Some(ServerResponse::Shutdown {
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

    let mut client = tokio::time::timeout(TIMEOUT, PluginClient::connect(server.socket_name()))
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

    let mut client = tokio::time::timeout(TIMEOUT, PluginClient::connect(server.socket_name()))
      .await
      .expect("Connection timeout")
      .expect("Failed to connect client");

    client.handshake().await.expect("Handshake error");

    let result = tokio::time::timeout(
      TIMEOUT,
      client.execute(
        "test".to_string(),
        vec![],
        PathBuf::from("."),
        HashMap::new(),
        HashMap::new(),
        CancellationToken::new(),
      ),
    )
    .await
    .expect("Execute timeout")
    .expect("Failed to execute command");

    assert_eq!(result, "test-id");

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
  async fn test_shutdown() {
    let mut server = TestServer::new().await;
    server.start("shutdown".to_string()).await;

    let mut client = tokio::time::timeout(TIMEOUT, PluginClient::connect(server.socket_name()))
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

    let mut client = PluginClient::connect(server.socket_name()).await.unwrap();

    client.handshake().await.expect("Handshake error");

    // Try to execute a command which should receive invalid JSON response
    let result = client
      .execute(
        "test".to_string(),
        vec![],
        PathBuf::from("."),
        HashMap::new(),
        HashMap::new(),
        CancellationToken::new(),
      )
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

    let client = Arc::new(Mutex::new(PluginClient::connect(server.socket_name()).await.unwrap()));

    // Spawn multiple concurrent commands
    let handles: Vec<_> = (0..5)
      .map(|i| {
        let client = Arc::clone(&client);
        tokio::spawn(async move {
          let mut client = client.lock().await;
          client
            .execute(
              format!("test{}", i),
              vec![],
              PathBuf::from("."),
              HashMap::new(),
              HashMap::new(),
              CancellationToken::new(),
            )
            .await
        })
      })
      .collect();

    // All commands should complete successfully
    for handle in handles {
      let result = handle.await.unwrap();
      assert!(result.is_ok());
    }

    let mut client = client.lock().await;
    client.shutdown().await.expect("Failed to shutdown client");

    let messages = server.stop().await;
    assert!(messages.len() > 5); // Hello + multiple Execute commands
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
        if let Ok(_) = reader.read_line(&mut buffer).await {
          messages.push(buffer.clone());
          let response = ServerResponse::Hello(Version {
            version: env!("CARGO_PKG_VERSION").to_string(),
            features: vec![],
          });
          let response_json = serde_json::to_string(&response).unwrap() + "\n";
          writer.write_all(response_json.as_bytes()).await.unwrap();
          writer.flush().await.unwrap();

          // Wait for Schema command
          buffer.clear();
          if let Ok(_) = reader.read_line(&mut buffer).await {
            messages.push(buffer.clone());

            let response = ServerResponse::Schema(Schema { key: "key".to_owned() });
            let response_json = serde_json::to_string(&response).unwrap() + "\n";
            writer.write_all(response_json.as_bytes()).await.unwrap();
            writer.flush().await.unwrap();
          }

          // Wait for shutdown command but don't respond
          buffer.clear();
          if let Ok(_) = reader.read_line(&mut buffer).await {
            messages.push(buffer);
            // Just wait without responding
            tokio::time::sleep(Duration::from_secs(10)).await;
          }
        }
      }
      messages
    }));

    let mut client = PluginClient::connect(server.socket_name()).await.unwrap();
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
        if let Ok(_) = reader.read_line(&mut buffer).await {
          messages.push(buffer.clone());
          let response = ServerResponse::Hello(Version {
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

    let mut client = PluginClient::connect(server.socket_name()).await.unwrap();
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

    let mut client = PluginClient::connect(server.socket_name()).await.unwrap();
    client.handshake().await.expect("Handshake failed");

    // Execute multiple commands before shutdown
    for i in 0..3 {
      let result = client
        .execute(
          format!("test{}", i),
          vec![],
          PathBuf::from("."),
          HashMap::new(),
          HashMap::new(),
          CancellationToken::new(),
        )
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
        if let Ok(_) = reader.read_line(&mut buffer).await {
          messages.push(buffer.clone());
          let response = ServerResponse::Hello(Version {
            version: env!("CARGO_PKG_VERSION").to_string(),
            features: vec![],
          });
          let response_json = serde_json::to_string(&response).unwrap() + "\n";
          writer.write_all(response_json.as_bytes()).await.unwrap();
          writer.flush().await.unwrap();

          // Wait for shutdown command and respond with error
          buffer.clear();
          if let Ok(_) = reader.read_line(&mut buffer).await {
            messages.push(buffer);
            let error_response = ServerResponse::Error {
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

    let mut client = PluginClient::connect(server.socket_name()).await.unwrap();
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

    let mut client = PluginClient::connect(server.socket_name()).await.unwrap();
    client.handshake().await.expect("Handshake failed");

    // First shutdown
    let result1 = client.shutdown().await;
    assert!(result1.is_ok(), "First shutdown should succeed");

    // Second shutdown should still succeed
    let result2 = client.shutdown().await;
    assert!(result2.is_err(), "Writter closed");

    let messages = server.stop().await;
    assert!(messages.iter().any(|m| m.contains("Shutdown")));
  }
}
