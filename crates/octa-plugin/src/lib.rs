use std::{
  collections::HashMap,
  env, io,
  path::PathBuf,
  process,
  sync::atomic::{AtomicBool, Ordering},
  sync::Arc,
};

use async_trait::async_trait;
use futures::StreamExt;
use interprocess::local_socket::{
  tokio::{prelude::*, Stream},
  GenericFilePath, ListenerOptions,
};
use lazy_static::lazy_static;
use tokio::{
  io::{split, AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader, WriteHalf},
  sync::{broadcast, Mutex},
  task::JoinHandle,
};
use tokio_util::codec::{FramedRead, LinesCodec};
use uuid::Uuid;

use protocol::{ClientCommand, ServerResponse, Version};

pub mod protocol;
pub mod socket;

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
  println!("Initiating graceful shutdown...");
  SERVER_RUNNING.store(false, Ordering::SeqCst);
  let _ = SHUTDOWN_NOTIFY.0.send(());
}

#[async_trait]
pub trait Plugin: Send + Sync + 'static {
  fn version(&self) -> String;

  async fn execute_command(
    &self,
    command: String,
    args: Vec<String>,
    dir: PathBuf,
    envs: HashMap<String, String>,
    writer: Arc<Mutex<WriteHalf<Stream>>>,
    id: String,
  ) -> io::Result<()>;
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

async fn handle_command(
  command: ClientCommand,
  writer: Arc<Mutex<WriteHalf<Stream>>>,
  active_commands: ActiveCommands,
  plugin: Arc<impl Plugin + 'static>,
) -> io::Result<()> {
  match command {
    ClientCommand::Execute {
      command,
      args,
      dir,
      envs,
    } => {
      let id = Uuid::new_v4().to_string();

      {
        // Send started response
        let start_response = ServerResponse::Started { id: id.clone() };
        let start_json = serde_json::to_string(&start_response)? + "\n";
        writer.lock().await.write_all(start_json.as_bytes()).await?;
      }

      // Clone what we need to move into the spawn
      let writer_clone = Arc::clone(&writer);
      let command_id = id.clone();

      // Spawn the command execution
      let handle = tokio::spawn(async move {
        if let Err(e) = plugin
          .execute_command(command, args, dir, envs, writer_clone.clone(), command_id.clone())
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
    },
    ClientCommand::Shutdown => {
      stop_server();
    },
  }
  Ok(())
}

async fn handle_conn(conn: Stream, plugin: Arc<impl Plugin + 'static>) -> io::Result<()> {
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
      println!("Client connected with version: {}", client_version.version);

      // Send server Hello response with plugin version
      let response = ServerResponse::Hello(Version {
        version: plugin.version(),
        features: vec![],
      });
      let response_json = serde_json::to_string(&response)? + "\n";
      writer.lock().await.write_all(response_json.as_bytes()).await?;
    },
    _ => {
      let response = ServerResponse::Error {
        id: "protocol_error".to_string(),
        message: "Expected Hello command".to_string(),
      };
      writer
        .lock()
        .await
        .write_all(format!("{}\n", serde_json::to_string(&response)?).as_bytes())
        .await?;
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
                ).await {
                  eprintln!("Error handling command: {}", e);
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
            eprintln!("Error reading from connection: {}", e);
            break;
          }
        }
      }
      _ = shutdown_rx.recv() => {
        println!("Connection received shutdown signal");
        break;
      }
    }
  }

  // Graceful connection shutdown
  {
    let mut commands = active_commands.lock().await;
    for (_, handle) in commands.drain() {
      handle.abort();
    }
  }

  // Send shutdown notification to server
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

pub async fn serve_plugin(plugin: impl Plugin + 'static) -> Result<(), Box<dyn std::error::Error>> {
  let plugin = Arc::new(plugin);
  let plugin_name: String = std::env::current_exe()
    .ok()
    .as_ref()
    .and_then(|path| path.file_stem())
    .map(|stem| stem.to_string_lossy().into_owned())
    .map(|stem| stem.strip_prefix("octa_plugin_").map(|s| s.to_owned()).unwrap_or(stem))
    .unwrap_or_else(|| "(unknown)".into());

  let mut args = env::args();
  args.next();

  let socket_path = if let Some(socket_path) = args.next() {
    socket_path
  } else {
    eprintln!("Missing socket_path argument");
    process::exit(1);
  };

  let socket_name = socket_path.clone().to_fs_name::<GenericFilePath>()?;

  println!("Plugin starting with version: {:?}", plugin.version());
  println!("Connect to socket {}", socket_path);

  let opts = ListenerOptions::new().name(socket_name);
  let listener = match opts.create_tokio() {
    Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
      eprintln!(
        "Error: could not start {} plugin because the socket file is occupied.
        Please check if {} is in use by another process and try again.",
        plugin_name, socket_path
      );
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
            let handle = tokio::spawn(async move {
              if let Err(e) = handle_conn(conn, plugin_clone).await {
                eprintln!("Error while handling connection: {e}");
              }
            });

            // Store the handle
            active_connections.lock().await.push(handle);
          },
          Err(e) => {
            eprintln!("There was an error with an incoming connection: {e}");
            continue;
          }
        }
      }
      _ = shutdown_rx.recv() => {
        println!("Received shutdown signal");
        break;
      }
    }
  }

  println!("Shutting down plugin...");

  // Wait for all active connections to complete
  let mut handles = active_connections.lock().await;
  for handle in handles.drain(..) {
    if let Err(e) = handle.await {
      eprintln!("Error waiting for connection to close: {}", e);
    }
  }

  println!("Plugin shutdown complete");
  Ok(())
}
