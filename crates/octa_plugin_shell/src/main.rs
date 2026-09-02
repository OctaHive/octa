use std::collections::HashMap;
use std::env;
use std::process::ExitCode;
use std::time::Duration;
use std::{path::PathBuf, sync::Arc};

use anyhow::Context;
use async_trait::async_trait;
use octa_plugin::logger::Logger;
use octa_plugin::{protocol::PluginResponse, serve_plugin, Plugin};
use octa_plugin::{PluginSchema, SHELL_CAPABILITY};
use serde_json::Value;
use tera::{Context as TeraContext, Tera};
use tokio::io::AsyncWrite;
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
    id: String,
    dry: bool,
    command: String,
    _args: Vec<String>,
    dir: PathBuf,
    vars: HashMap<String, Value>,
    envs: HashMap<String, String>,
    writer: Arc<Mutex<impl AsyncWrite + Send + 'static + std::marker::Unpin>>,
    logger: Arc<impl Logger>,
    cancel_token: CancellationToken,
  ) -> anyhow::Result<()> {
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
        let mut reader = BufReader::new(stdout);
        let mut buffer = String::new();

        loop {
          tokio::select! {
            result = reader.read_line(&mut buffer) => {
              match result {
                Ok(0) => break,
                Ok(_) => {
                  if !buffer.is_empty() {
                    let response = PluginResponse::Stdout {
                      id: id.clone(),
                      line: buffer.clone(),
                    };
                    let response_json = serde_json::to_string(&response).unwrap() + "\n";
                    let _ = tx_stdout.send(response_json.clone()).await;
                    let _ = logger.log(&response_json.to_string());
                  }
                  buffer.clear();
                }
                Err(_) => break,
              }
            }
            _ = cancel_token.cancelled() => {
              break;
            }
          }
        }
        let _ = tx_done_stdout.send(()).await;
      })
    };

    let stderr_handle = {
      let id = id.clone();
      let logger = logger.clone();
      let cancel_token = cancel_token.clone();
      tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut buffer = String::new();

        loop {
          tokio::select! {
            result = reader.read_line(&mut buffer) => {
              match result {
                Ok(0) => break,
                Ok(_) => {
                  if !buffer.is_empty() {
                    let response = PluginResponse::Stderr {
                      id: id.clone(),
                      line: buffer.clone(),
                    };
                    let response_json = serde_json::to_string(&response).unwrap() + "\n";
                    let _ = tx_stderr.send(response_json.clone()).await;
                    let _ = logger.log(&response_json.to_string());
                  }
                  buffer.clear();
                }
                Err(_) => break,
              }
            }
            _ = cancel_token.cancelled() => {
              break;
            }
          }
        }
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

    let result = plugin
      .execute_command(
        "test-id".to_string(),
        true,
        "echo Hello, World!".to_owned(),
        vec![],
        dir,
        HashMap::new(),
        HashMap::new(),
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
