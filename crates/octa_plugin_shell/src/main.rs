use std::collections::HashMap;
use std::time::Duration;
use std::{path::PathBuf, process::Stdio, sync::Arc};

use anyhow::Context;
use async_trait::async_trait;
use octa_plugin::logger::Logger;
use octa_plugin::{protocol::ServerResponse, serve_plugin, Plugin};
use serde_json::Value;
use tokio::io::AsyncWrite;
use tokio::{
  io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
  sync::{mpsc, Mutex},
};
use tokio_util::sync::CancellationToken;

struct ShellPlugin {}

/// Platform-specific command setup for Unix
#[cfg(not(windows))]
fn setup_unix_command(cmd: &str, dir: &PathBuf, envs: HashMap<String, String>) -> tokio::process::Command {
  let mut command = tokio::process::Command::new("sh");
  command
    .current_dir(dir)
    .arg("-c")
    .arg(cmd)
    .envs(envs)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true)
    .process_group(0);
  command
}

/// Platform-specific command terminate for Unix
#[cfg(not(windows))]
fn terminate_unix_process(child: &mut tokio::process::Child) {
  use nix::sys::signal::{kill, Signal};
  use nix::unistd::Pid;

  if let Some(pid) = child.id() {
    let _ = kill(Pid::from_raw(-(pid as i32)), Signal::SIGTERM);
  }
}

/// Platform-specific command setup for Windows
#[cfg(windows)]
fn setup_windows_command(cmd: &str, dir: &PathBuf, envs: HashMap<String, String>) -> tokio::process::Command {
  #[allow(unused_imports)]
  use std::os::windows::process::CommandExt;

  const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
  const CREATE_NO_WINDOW: u32 = 0x08000000;

  let mut command = tokio::process::Command::new("cmd");
  command
    .current_dir(dir)
    .args(["/C", cmd])
    .envs(envs)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true)
    .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
  command
}

/// Platform-specific command terminate for Windows
#[cfg(windows)]
fn terminate_windows_process(child: &mut tokio::process::Child) {
  use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
  use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

  if let Some(pid) = child.id() {
    unsafe {
      let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
      if handle as HANDLE != INVALID_HANDLE_VALUE {
        let handle_ptr = handle as HANDLE;
        TerminateProcess(handle_ptr, 1);
        CloseHandle(handle_ptr);
      }
    }
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
    command: String,
    _args: Vec<String>,
    dir: PathBuf,
    _vars: HashMap<String, Value>,
    envs: HashMap<String, String>,
    writer: Arc<Mutex<impl AsyncWrite + Send + 'static + std::marker::Unpin>>,
    logger: Arc<impl Logger>,
    cancel_token: CancellationToken,
  ) -> anyhow::Result<()> {
    #[cfg(windows)]
    let mut command = setup_windows_command(&command, &dir, envs);

    #[cfg(not(windows))]
    let mut command = setup_unix_command(&command, &dir, envs);

    let mut child = command.spawn()?;

    let stdout = child.stdout.take().context("Failed to capture stdout")?;
    let stderr = child.stderr.take().context("Failed to capture stderr")?;

    let (tx, mut rx): (mpsc::Sender<String>, mpsc::Receiver<String>) = mpsc::channel(100);
    let tx_stdout = tx.clone();
    let tx_stderr = tx.clone();

    let (tx_done, mut rx_done) = mpsc::channel::<()>(2);
    let tx_done_stdout = tx_done.clone();
    let tx_done_stderr = tx_done.clone();

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
                    let response = ServerResponse::Stdout {
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
                    let response = ServerResponse::Stderr {
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
              let response = ServerResponse::ExitStatus {
                id: id.clone(),
                code: status.code().unwrap_or(-1),
              };
              let response_json = serde_json::to_string(&response).unwrap() + "\n";
              let _ = tx.send(response_json.clone()).await;
              let _ = logger.log(&response_json.to_string());
            }
          }
          _ = cancel_token.cancelled() => {
            #[cfg(windows)]
            terminate_windows_process(&mut child);
            #[cfg(unix)]
            terminate_unix_process(&mut child);

            tokio::time::sleep(Duration::from_millis(100)).await;

            let _ = child.kill().await;

            let response = ServerResponse::ExitStatus {
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
async fn main() -> anyhow::Result<()> {
  serve_plugin(ShellPlugin {}).await
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
      std::io::Write::write_all(&mut this.buffer, buf)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
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
    (writer, logger, temp_dir.into_path())
  }

  #[tokio::test]
  async fn test_shell_plugin_version() {
    let plugin = ShellPlugin {};
    assert_eq!(plugin.version(), env!("CARGO_PKG_VERSION").to_string());
  }

  #[tokio::test]
  async fn test_echo_command() {
    let (writer, logger, dir) = setup_test().await;
    let plugin = ShellPlugin {};
    let test_string = "Hello, World!";
    let cancel_token = CancellationToken::new();

    let result = plugin
      .execute_command(
        "test-id".to_string(),
        format!("echo {}", test_string),
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

    // Wait a bit for async logging to complete
    tokio::time::sleep(Duration::from_millis(100)).await;

    let output = writer.lock().await.get_output();
    let lines: Vec<&str> = output.lines().collect();

    // Find stdout message
    let stdout_line = lines
      .iter()
      .find(|line| line.contains("\"type\":\"Stdout\""))
      .expect("Should have stdout message");

    let response: ServerResponse = serde_json::from_str(stdout_line).unwrap();
    match response {
      ServerResponse::Stdout { id, line } => {
        assert_eq!(id, "test-id");
        assert!(line.contains(test_string));
      },
      _ => panic!("Expected Stdout response"),
    }

    // Check logger messages using as_any()
    let mock_logger = logger.as_any().downcast_ref::<MockLogger>().unwrap();
    let log_messages = mock_logger.get_messages().await;
    assert!(!log_messages.is_empty());
    assert!(log_messages.iter().any(|msg| msg.contains("Stdout")));
  }

  #[tokio::test]
  async fn test_command_cancellation() {
    let (writer, logger, dir) = setup_test().await;
    let plugin = ShellPlugin {};
    let cancel_token = CancellationToken::new();

    // Use a command that takes some time to complete
    #[cfg(windows)]
    let command = "ping -n 5 127.0.0.1";
    #[cfg(not(windows))]
    let command = "sleep 5";

    let handle = tokio::spawn({
      let cancel_token = cancel_token.clone();
      async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel_token.cancel();
      }
    });

    let result = plugin
      .execute_command(
        "test-id".to_string(),
        command.to_string(),
        vec![],
        dir,
        HashMap::new(),
        HashMap::new(),
        writer.clone(),
        logger.clone(),
        cancel_token,
      )
      .await;

    handle.await.unwrap();
    assert!(result.is_ok());

    // Wait a bit for async operations to complete
    tokio::time::sleep(Duration::from_millis(100)).await;

    let output = writer.lock().await.get_output();
    let lines: Vec<&str> = output.lines().collect();

    // Should find an exit status with non-zero code
    let exit_line = lines
      .iter()
      .find(|line| line.contains("\"type\":\"ExitStatus\""))
      .expect("Should have exit status message");

    let response: ServerResponse = serde_json::from_str(exit_line).unwrap();
    match response {
      ServerResponse::ExitStatus { id, code } => {
        assert_eq!(id, "test-id");
        assert_ne!(code, 0); // Should be non-zero for cancelled process
      },
      _ => panic!("Expected ExitStatus response"),
    }

    // Verify cancellation was logged
    let mock_logger = logger.as_any().downcast_ref::<MockLogger>().unwrap();
    let log_messages = mock_logger.get_messages().await;
    assert!(log_messages.iter().any(|msg| msg.contains("ExitStatus")));
  }
}
