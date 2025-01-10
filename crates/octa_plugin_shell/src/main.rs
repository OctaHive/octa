use std::collections::HashMap;
use std::time::Duration;
use std::{io, path::PathBuf, process::Stdio, sync::Arc};

use async_trait::async_trait;
use interprocess::local_socket::tokio::Stream;
use octa_plugin::{protocol::ServerResponse, serve_plugin, stop_server, Plugin};
use tokio::io::WriteHalf;
use tokio::{
  io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
  sync::{mpsc, Mutex},
};

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

#[cfg(unix)]
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
  fn version(&self) -> String {
    "1.0.0".to_owned()
  }

  async fn execute_command(
    &self,
    command: String,
    _args: Vec<String>,
    dir: PathBuf,
    envs: HashMap<String, String>,
    writer: Arc<Mutex<WriteHalf<Stream>>>,
    id: String,
  ) -> io::Result<()> {
    #[cfg(windows)]
    let mut command = setup_windows_command(&command, &dir, envs);

    #[cfg(not(windows))]
    let mut command = setup_unix_command(&command, &dir, envs);

    let mut child = command.spawn()?;

    let stdout = child.stdout.take().expect("Failed to capture stdout");
    let stderr = child.stderr.take().expect("Failed to capture stderr");

    let (tx, mut rx): (mpsc::Sender<String>, mpsc::Receiver<String>) = mpsc::channel(100);
    let (cancel_tx, mut cancel_rx) = mpsc::channel::<()>(1);
    let tx_stdout = tx.clone();
    let tx_stderr = tx.clone();

    let writer_handle = tokio::spawn({
      let writer = Arc::clone(&writer);
      async move {
        while let Some(msg) = rx.recv().await {
          let mut lock = writer.lock().await;
          let _ = lock.write_all(msg.as_bytes()).await;
        }
      }
    });

    let stdout_handle = {
      let id = id.clone();
      let cancel_tx_stdout = cancel_tx.clone();
      tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut buffer = String::new();

        loop {
          tokio::select! {
            result = reader.read_line(&mut buffer) => {
              match result {
                Ok(0) => break,
                Ok(_) => {
                  let response = ServerResponse::Stdout {
                    id: id.clone(),
                    line: buffer.clone(),
                  };
                  let response_json = serde_json::to_string(&response).unwrap() + "\n";
                  let _ = tx_stdout.send(response_json).await;
                  buffer.clear();
                }
                Err(_) => break,
              }
            }
            _ = cancel_tx_stdout.closed() => {
              break;
            }
          }
        }
      })
    };

    let stderr_handle = {
      let id = id.clone();
      let cancel_tx_stderr = cancel_tx.clone();
      tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut buffer = String::new();

        loop {
          tokio::select! {
            result = reader.read_line(&mut buffer) => {
              match result {
                Ok(0) => break,
                Ok(_) => {
                  let response = ServerResponse::Stderr {
                    id: id.clone(),
                    line: buffer.clone(),
                  };
                  let response_json = serde_json::to_string(&response).unwrap() + "\n";
                  let _ = tx_stderr.send(response_json).await;
                  buffer.clear();
                }
                Err(_) => break,
              }
            }
            _ = cancel_tx_stderr.closed() => {
              break;
            }
          }
        }
      })
    };

    let wait_handle = {
      let cancel_tx_wait = cancel_tx.clone();
      let id = id.clone();
      let tx = tx.clone();
      tokio::spawn(async move {
        tokio::select! {
          status = child.wait() => {
            if let Ok(status) = status {
              let response = ServerResponse::ExitStatus {
                id,
                code: status.code().unwrap_or(-1),
              };
              let response_json = serde_json::to_string(&response).unwrap() + "\n";
              let _ = tx.send(response_json).await;
            }
          }
          _ = cancel_tx_wait.closed() => {
            #[cfg(windows)]
            terminate_windows_process(&mut child);
            #[cfg(unix)]
            terminate_unix_process(&mut child);

            // Give the process time to clean up
            tokio::time::sleep(Duration::from_millis(100)).await;

            // Then force kill if still running
            let _ = child.kill().await;

            let response = ServerResponse::ExitStatus {
              id,
              code: -1, // Indicate termination
            };
            let response_json = serde_json::to_string(&response).unwrap() + "\n";
            let _ = tx.send(response_json).await;
          }
        }
      })
    };

    let cancel_handle = tokio::spawn(async move {
      if let Some(()) = cancel_rx.recv().await {
        drop(cancel_tx);
      }
    });

    // Add type annotations for join
    let _: (Result<(), _>, Result<(), _>, Result<(), _>, Result<(), _>) =
      tokio::join!(stdout_handle, stderr_handle, wait_handle, cancel_handle);

    drop(tx);
    let _ = writer_handle.await;

    Ok(())
  }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
  ctrlc::set_handler(move || {
    println!("Shutting down server...");
    stop_server();
  })?;

  serve_plugin(ShellPlugin {}).await
}
