use std::{collections::HashMap, io, path::PathBuf, sync::Arc, time::Duration};

use interprocess::local_socket::{tokio::Stream as TokioStream, traits::tokio::Stream as StreamTrait, Name};
use tokio::{
  io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf},
  sync::{mpsc, Mutex},
};
use tokio_util::sync::CancellationToken;

use octa_plugin::protocol::{ClientCommand, ServerResponse, Version};

#[derive(Debug)]
pub enum PluginClientError {
  Io(io::Error),
  SerdeJson(serde_json::Error),
  Protocol(String),
  ConnectionClosed,
}

impl From<PluginClientError> for io::Error {
  fn from(err: PluginClientError) -> Self {
    match err {
      PluginClientError::Io(e) => e,
      PluginClientError::SerdeJson(e) => io::Error::new(io::ErrorKind::InvalidData, e),
      PluginClientError::Protocol(msg) => io::Error::new(io::ErrorKind::Other, msg),
      PluginClientError::ConnectionClosed => io::Error::new(io::ErrorKind::ConnectionAborted, "Connection closed"),
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
    }
  }
}

#[derive(Debug)]
pub struct PluginClient {
  inner: Arc<PluginClientInner>,
  response_rx: mpsc::UnboundedReceiver<ServerResponse>,
}

#[derive(Debug)]
struct PluginClientInner {
  writer: Mutex<WriteHalf<TokioStream>>,
  response_tx: mpsc::UnboundedSender<ServerResponse>,
  child_senders: Mutex<Vec<mpsc::UnboundedSender<ServerResponse>>>,
}

impl Clone for PluginClient {
  fn clone(&self) -> Self {
    let (new_tx, new_rx) = mpsc::unbounded_channel();

    // Store the new sender in the list of child senders
    let mut child_senders = self.inner.child_senders.blocking_lock();
    child_senders.push(new_tx);

    Self {
      inner: Arc::clone(&self.inner),
      response_rx: new_rx,
    }
  }
}

pub async fn connect_to_plugin(socket_path: &Name<'_>) -> io::Result<TokioStream> {
  let mut attempts = 0;
  const MAX_ATTEMPTS: u32 = 10;

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

    let inner = Arc::new(PluginClientInner {
      writer: Mutex::new(writer),
      response_tx: response_tx.clone(),
      child_senders: Mutex::new(vec![]),
    });

    // Start response handling task with the inner Arc
    Self::start_response_handler(reader, Arc::clone(&inner));

    let mut client = Self { inner, response_rx };

    client.handshake().await?;

    Ok(client)
  }

  async fn handshake(&mut self) -> Result<(), PluginClientError> {
    let hello = ClientCommand::Hello(Version {
      version: env!("CARGO_PKG_VERSION").to_string(),
      features: vec![],
    });

    let hello_json = serde_json::to_string(&hello)? + "\n";
    self.inner.writer.lock().await.write_all(hello_json.as_bytes()).await?;

    match self.response_rx.recv().await {
      Some(ServerResponse::Hello(version)) => {
        println!("Server version: {}", version.version);
        Ok(())
      },
      Some(ServerResponse::Error { message, .. }) => Err(PluginClientError::Protocol(message)),
      Some(_) => Err(PluginClientError::Protocol("Unexpected response to Hello".into())),
      None => Err(PluginClientError::ConnectionClosed),
    }
  }

  fn start_response_handler(reader: ReadHalf<TokioStream>, inner: Arc<PluginClientInner>) {
    tokio::spawn(async move {
      let mut reader = BufReader::new(reader);
      let mut buffer = String::new();

      loop {
        buffer.clear();
        match reader.read_line(&mut buffer).await {
          Ok(0) => break, // Connection closed
          Ok(_) => {
            // Parse the response once
            if serde_json::from_str::<ServerResponse>(buffer.trim()).is_ok() {
              // Get all senders
              let mut senders = vec![inner.response_tx.clone()];
              let child_senders = inner.child_senders.lock().await;
              senders.extend(child_senders.iter().cloned());

              // Send to all channels by parsing the JSON for each sender
              for sender in senders {
                // Re-parse the JSON for each sender since we can't clone ServerResponse
                if let Ok(response) = serde_json::from_str::<ServerResponse>(buffer.trim()) {
                  let _ = sender.send(response);
                }
              }
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
    envs: HashMap<String, String>,
    cancel_token: CancellationToken,
  ) -> Result<String, PluginClientError> {
    let cmd = ClientCommand::Execute {
      command,
      args,
      dir,
      envs,
    };

    let cmd_json = serde_json::to_string(&cmd)? + "\n";
    self.inner.writer.lock().await.write_all(cmd_json.as_bytes()).await?;

    tokio::select! {
      response = self.response_rx.recv() => {
        match response {
          Some(ServerResponse::Started { id }) => Ok(id),
          Some(ServerResponse::Error { message, .. }) => {
            Err(PluginClientError::Protocol(message))
          }
          Some(_) => Err(PluginClientError::Protocol("Expected Started response".into())),
          None => Err(PluginClientError::ConnectionClosed),
        }
      }
      _ = cancel_token.cancelled() => {
        self.shutdown().await?;
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
        self.shutdown().await?;
        Err(PluginClientError::Protocol("Command cancelled".into()))
      }
    }
  }

  pub async fn shutdown(&mut self) -> Result<(), PluginClientError> {
    let cmd = ClientCommand::Shutdown;
    let cmd_json = serde_json::to_string(&cmd)? + "\n";
    self.inner.writer.lock().await.write_all(cmd_json.as_bytes()).await?;

    match self.response_rx.recv().await {
      Some(ServerResponse::Shutdown { .. }) => Ok(()),
      Some(ServerResponse::Error { message, .. }) => Err(PluginClientError::Protocol(message)),
      Some(_) => Err(PluginClientError::Protocol("Expected Shutdown response".into())),
      None => Err(PluginClientError::ConnectionClosed),
    }
  }
}
