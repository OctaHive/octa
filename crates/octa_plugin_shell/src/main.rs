use std::collections::HashMap;
use std::env;
use std::io::{Read, Write};
use std::process::ExitCode;
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use std::{path::PathBuf, sync::Arc};

use anyhow::Context;
use async_trait::async_trait;
use octa_plugin::logger::Logger;
use octa_plugin::{protocol::PluginResponse, serve_plugin, Plugin, PluginCommand, PluginInput};
use octa_plugin::{PluginSchema, SHELL_CAPABILITY};
use portable_pty::{native_pty_system, PtySize};
#[cfg(test)]
use serde_json::Value;
use tera::{Context as TeraContext, Tera};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::{
  io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
  sync::{mpsc, Mutex},
};
use tokio_util::sync::CancellationToken;

mod brush;
mod coreutils;

struct ShellPlugin {
  coreutils: coreutils::Coreutils,
}

struct PtyChildGuard {
  child: Arc<StdMutex<Box<dyn portable_pty::Child + Send + Sync>>>,
  process_group: Option<i32>,
  running: bool,
}

impl PtyChildGuard {
  fn new(child: Box<dyn portable_pty::Child + Send + Sync>, process_group: Option<i32>) -> Self {
    Self {
      child: Arc::new(StdMutex::new(child)),
      process_group,
      running: true,
    }
  }

  fn finished(&mut self) {
    self.running = false;
  }

  fn terminate(&self) {
    #[cfg(unix)]
    if let Some(process_group) = self.process_group {
      use nix::{
        sys::signal::{kill, Signal},
        unistd::Pid,
      };
      let _ = kill(Pid::from_raw(-process_group), Signal::SIGTERM);
    }
    let _ = self.child.lock().unwrap().kill();
  }
}

impl Drop for PtyChildGuard {
  fn drop(&mut self) {
    if self.running {
      self.terminate();
    }
  }
}

async fn forward_output(
  stream: impl AsyncRead + Unpin,
  id: String,
  stdout: bool,
  tx: mpsc::Sender<String>,
  logger: Arc<impl Logger>,
  cancel_token: CancellationToken,
) {
  let mut reader = BufReader::new(stream);
  let mut buffer = String::new();
  loop {
    let read = tokio::select! {
      read = reader.read_line(&mut buffer) => read,
      _ = cancel_token.cancelled() => break,
    };
    match read {
      Ok(0) | Err(_) => break,
      Ok(_) => {
        let response = if stdout {
          PluginResponse::Stdout {
            id: id.clone(),
            line: buffer.clone(),
          }
        } else {
          PluginResponse::Stderr {
            id: id.clone(),
            line: buffer.clone(),
          }
        };
        let response_json = serde_json::to_string(&response).unwrap() + "\n";
        let _ = tx.send(response_json.clone()).await;
        let _ = logger.log(&response_json);
        buffer.clear();
      },
    }
  }
}

struct RawPtyCommand {
  id: String,
  command: String,
  dir: PathBuf,
  envs: HashMap<String, String>,
  input: mpsc::UnboundedReceiver<PluginInput>,
}

async fn execute_raw_pty(
  request: RawPtyCommand,
  coreutils_path: &std::path::Path,
  writer: Arc<Mutex<impl AsyncWrite + Send + 'static + Unpin>>,
  cancel_token: CancellationToken,
) -> anyhow::Result<()> {
  let RawPtyCommand {
    id,
    command,
    dir,
    envs,
    mut input,
  } = request;
  let size = PtySize::default();
  let pair = native_pty_system().openpty(size)?;
  #[cfg(unix)]
  let process_group = pair.master.process_group_leader();
  #[cfg(not(unix))]
  let process_group = None;
  let mut child = PtyChildGuard::new(
    pair
      .slave
      .spawn_command(brush::pty_command(&command, &dir, envs, coreutils_path)?)?,
    process_group,
  );
  drop(pair.slave);

  let mut reader = pair.master.try_clone_reader()?;
  let mut pty_writer = Some(Arc::new(StdMutex::new(pair.master.take_writer()?)));
  let (output_tx, mut output_rx) = mpsc::channel::<Vec<u8>>(32);
  let reader_task = tokio::task::spawn_blocking(move || {
    let mut buffer = vec![0; 8192];
    loop {
      match reader.read(&mut buffer) {
        Ok(0) | Err(_) => break,
        Ok(count) if output_tx.blocking_send(buffer[..count].to_vec()).is_err() => break,
        Ok(_) => {},
      }
    }
  });

  let mut code = None;
  let mut output_closed = false;
  while code.is_none() || !output_closed {
    tokio::select! {
      output = output_rx.recv(), if !output_closed => {
        match output {
          Some(bytes) => {
            let response = PluginResponse::StdoutBytes { id: id.clone(), bytes };
            let json = serde_json::to_string(&response)? + "\n";
            let mut writer = writer.lock().await;
            writer.write_all(json.as_bytes()).await?;
            writer.flush().await?;
          },
          None => output_closed = true,
        }
      },
      terminal_input = input.recv(), if code.is_none() => {
        match terminal_input {
          Some(PluginInput::Bytes(bytes)) => {
            if let Some(pty_writer) = &pty_writer {
              let pty_writer = pty_writer.clone();
              tokio::task::spawn_blocking(move || {
                let mut writer = pty_writer.lock().unwrap();
                writer.write_all(&bytes)?;
                writer.flush()
              }).await??;
            }
          },
          Some(PluginInput::Resize { rows, cols }) => pair.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
          })?,
          Some(PluginInput::Close) | None => pty_writer = None,
        }
      },
      _ = cancel_token.cancelled(), if code.is_none() => {
        child.terminate();
        code = Some(-1);
      },
      _ = tokio::time::sleep(Duration::from_millis(20)), if code.is_none() => {
        let status = child.child.lock().unwrap().try_wait()?;
        if let Some(status) = status {
          code = Some(status.exit_code() as i32);
          child.finished();
          pty_writer = None;
        }
      },
    }
  }

  let _ = reader_task.await;
  let response = PluginResponse::ExitStatus {
    id,
    code: code.unwrap_or(-1),
  };
  let json = serde_json::to_string(&response)? + "\n";
  let mut writer = writer.lock().await;
  writer.write_all(json.as_bytes()).await?;
  writer.flush().await?;
  Ok(())
}

impl ShellPlugin {
  fn new() -> anyhow::Result<Self> {
    Ok(Self {
      coreutils: coreutils::Coreutils::new()?,
    })
  }
}

fn plugin_schema() -> PluginSchema {
  PluginSchema {
    key: "shell".to_owned(),
    supports_raw: true,
    capabilities: vec![SHELL_CAPABILITY.to_owned()],
    validation_schema: serde_json::json!({ "type": "string" }).as_object().cloned(),
  }
}

#[async_trait]
impl Plugin for ShellPlugin {
  /// Return plugin version
  fn version(&self) -> String {
    env!("CARGO_PKG_VERSION").to_owned()
  }

  async fn execute_command(
    &self,
    request: PluginCommand,
    writer: Arc<Mutex<impl AsyncWrite + Send + 'static + std::marker::Unpin>>,
    logger: Arc<impl Logger>,
    cancel_token: CancellationToken,
  ) -> anyhow::Result<()> {
    let PluginCommand {
      id,
      dry,
      command,
      args: _,
      dir,
      vars,
      envs,
      raw,
      input,
    } = request;
    let mut tera = Tera::default();
    let template_name = format!("template_{}", id);

    tera
      .add_raw_template(&template_name, &command)
      .context("Failed to parse template")?;

    let context = TeraContext::from_serialize(&vars).context("Failed to serialize variables to context")?;

    let result = tera
      .render(&template_name, &context)
      .context(format!("Failed to render template: {:?}", context))?;

    if dry {
      logger.log(&format!("Run command in dry mode: {}", result))?;

      let response = serde_json::to_string(&PluginResponse::ExitStatus {
        id: id.clone(),
        code: 0,
      })?
        + "\n";
      let mut writer = writer.lock().await;
      writer.write_all(response.as_bytes()).await?;
      writer.flush().await?;

      return Ok(());
    }

    if raw {
      return execute_raw_pty(
        RawPtyCommand {
          id,
          command: result,
          dir,
          envs,
          input,
        },
        self.coreutils.path(),
        writer,
        cancel_token,
      )
      .await;
    }

    let (tx, mut rx): (mpsc::Sender<String>, mpsc::Receiver<String>) = mpsc::channel(100);
    let writer_handle = tokio::spawn({
      let writer = Arc::clone(&writer);
      async move {
        while let Some(msg) = rx.recv().await {
          let mut lock = writer.lock().await;
          let _ = lock.write_all(msg.as_bytes()).await;
          let _ = lock.flush().await;
        }
      }
    });

    let mut command = brush::command(&result, &dir, envs, self.coreutils.path())?;
    let mut child = command.spawn()?;

    let stdout = child.stdout.take().context("Failed to capture stdout")?;
    let stderr = child.stderr.take().context("Failed to capture stderr")?;

    let tx_stdout = tx.clone();
    let tx_stderr = tx.clone();

    let (tx_done, mut rx_done) = mpsc::channel::<()>(2);
    let tx_done_stdout = tx_done.clone();
    let tx_done_stderr = tx_done.clone();

    let stdout_handle = {
      let id = id.clone();
      let logger = logger.clone();
      let cancel_token = cancel_token.clone();
      tokio::spawn(async move {
        forward_output(stdout, id, true, tx_stdout, logger, cancel_token).await;
        let _ = tx_done_stdout.send(()).await;
      })
    };

    let stderr_handle = {
      let id = id.clone();
      let logger = logger.clone();
      let cancel_token = cancel_token.clone();
      tokio::spawn(async move {
        forward_output(stderr, id, false, tx_stderr, logger, cancel_token).await;
        let _ = tx_done_stderr.send(()).await;
      })
    };

    let wait_handle = {
      let id = id.clone();
      let tx = tx.clone();
      let cancel_token = cancel_token.clone();
      tokio::spawn(async move {
        // Wait for both stdout and stderr to complete
        let mut completed = 0;
        while let Some(()) = rx_done.recv().await {
          completed += 1;
          if completed == 2 {
            break;
          }
        }

        tokio::select! {
          status = child.wait() => {
            if let Ok(status) = status {
              let response = PluginResponse::ExitStatus {
                id: id.clone(),
                code: status.code().unwrap_or(-1),
              };
              let response_json = serde_json::to_string(&response).unwrap() + "\n";
              let _ = tx.send(response_json.clone()).await;
              let _ = logger.log(&response_json.to_string());
            }
          }
          _ = cancel_token.cancelled() => {
            brush::terminate(&mut child);

            tokio::time::sleep(Duration::from_millis(100)).await;

            let _ = child.kill().await;

            let response = PluginResponse::ExitStatus {
              id: id.clone(),
              code: -1,
            };
            let response_json = serde_json::to_string(&response).unwrap() + "\n";
            let _ = tx.send(response_json.clone()).await;
            let _ = logger.log(&response_json.to_string());
          }
        }
      })
    };

    // Add type annotations for join
    let _: (Result<(), _>, Result<(), _>, Result<(), _>) = tokio::join!(stdout_handle, stderr_handle, wait_handle);

    drop(tx);
    let _ = writer_handle.await;

    Ok(())
  }
}

#[tokio::main]
async fn main() -> anyhow::Result<ExitCode> {
  if let Some(exit_code) = coreutils::dispatch() {
    return Ok(ExitCode::from(exit_code));
  }
  if let Some(exit_code) = brush::run_child().await? {
    return Ok(ExitCode::from(exit_code));
  }

  serve_plugin(ShellPlugin::new()?, plugin_schema()).await?;
  Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
  use super::*;
  use octa_plugin::logger::{Logger, MockLogger};
  use std::io;
  use tempfile::tempdir;
  use tokio::sync::Mutex;

  struct TestWriter {
    buffer: Vec<u8>,
  }

  impl TestWriter {
    fn new() -> Self {
      Self { buffer: Vec::new() }
    }

    fn get_output(&self) -> String {
      String::from_utf8_lossy(&self.buffer).to_string()
    }
  }

  impl AsyncWrite for TestWriter {
    fn poll_write(
      self: std::pin::Pin<&mut Self>,
      _cx: &mut std::task::Context<'_>,
      buf: &[u8],
    ) -> std::task::Poll<Result<usize, io::Error>> {
      let this = self.get_mut();
      // Use explicit Write trait implementation
      std::io::Write::write_all(&mut this.buffer, buf).map_err(std::io::Error::other)?;
      std::task::Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(
      self: std::pin::Pin<&mut Self>,
      _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
      std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
      self: std::pin::Pin<&mut Self>,
      _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
      std::task::Poll::Ready(Ok(()))
    }
  }

  async fn setup_test() -> (Arc<Mutex<TestWriter>>, Arc<impl Logger>, PathBuf) {
    let writer = Arc::new(Mutex::new(TestWriter::new()));
    let logger = Arc::new(MockLogger::new());
    let temp_dir = tempdir().unwrap();
    (writer, logger, temp_dir.keep())
  }

  #[tokio::test]
  async fn test_shell_plugin_version() {
    let plugin = ShellPlugin::new().unwrap();
    assert_eq!(plugin.version(), env!("CARGO_PKG_VERSION").to_string());
  }

  #[test]
  fn test_shell_plugin_schema() {
    let schema = plugin_schema();

    assert_eq!(schema.key, "shell");
    assert_eq!(schema.capabilities, [SHELL_CAPABILITY]);
    assert_eq!(
      schema.validation_schema.unwrap().get("type"),
      Some(&Value::String("string".to_owned()))
    );
  }

  #[tokio::test]
  async fn test_dry_command() {
    let (writer, logger, dir) = setup_test().await;
    let plugin = ShellPlugin::new().unwrap();
    let cancel_token = CancellationToken::new();
    let (_input, input) = mpsc::unbounded_channel();

    let result = plugin
      .execute_command(
        PluginCommand {
          id: "test-id".to_string(),
          dry: true,
          command: "echo Hello, World!".to_owned(),
          args: vec![],
          dir,
          vars: HashMap::new(),
          envs: HashMap::new(),
          raw: false,
          input,
        },
        writer.clone(),
        logger.clone(),
        cancel_token,
      )
      .await;

    assert!(result.is_ok());

    let output = writer.lock().await.get_output();
    let lines: Vec<&str> = output.lines().collect();
    let exit_status = lines
      .iter()
      .find(|line| line.contains("\"type\":\"ExitStatus\""))
      .expect("Should have exit status message");

    let response: PluginResponse = serde_json::from_str(exit_status).unwrap();
    match response {
      PluginResponse::ExitStatus { id, code } => {
        assert_eq!(id, "test-id");
        assert_eq!(code, 0);
      },
      _ => panic!("Expected ExitStatus response"),
    }
  }
}
