use std::{io, io::IsTerminal, time::Duration};

use async_trait::async_trait;
use octa_executor::{RawTerminalConnector, RawTerminalInput, RawTerminalSession};
use tokio::task::JoinHandle;

#[cfg(windows)]
const DEFAULT_CURSOR_POSITION_RESPONSE: &[u8] = b"\x1b[1;1R";

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LocalRawTerminal;

#[async_trait]
impl RawTerminalConnector for LocalRawTerminal {
  async fn connect(&self, input: RawTerminalInput) -> io::Result<Box<dyn RawTerminalSession>> {
    RawTerminalBridge::start(input)
      .await
      .map(|bridge| Box::new(bridge) as Box<dyn RawTerminalSession>)
  }
}

/// Bridges the host terminal to a command-scoped plugin protocol stream.
struct RawTerminalBridge {
  input: RawTerminalInput,
  tasks: Vec<JoinHandle<()>>,
  terminal_mode: bool,
}

impl RawTerminalBridge {
  async fn start(input: RawTerminalInput) -> io::Result<Self> {
    let terminal_mode = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if terminal_mode {
      crossterm::terminal::enable_raw_mode()?;
    }

    #[cfg(windows)]
    if !terminal_mode {
      // portable-pty creates ConPTY with PSEUDOCONSOLE_INHERIT_CURSOR. When
      // stdout is not a terminal, answer its cursor query on the host's behalf.
      input.write(DEFAULT_CURSOR_POSITION_RESPONSE.to_vec()).await?;
    }

    let mut tasks = Vec::new();
    let input_writer = input.clone();
    tasks.push(tokio::spawn(async move {
      let _ = forward_input(input_writer, terminal_mode).await;
    }));

    if terminal_mode {
      let resize_input = input.clone();
      tasks.push(tokio::spawn(async move {
        let mut previous = None;
        loop {
          if let Ok((cols, rows)) = crossterm::terminal::size() {
            let size = (rows, cols);
            if previous != Some(size) {
              if resize_input.resize(rows, cols).await.is_err() {
                break;
              }
              previous = Some(size);
            }
          }
          tokio::time::sleep(Duration::from_millis(200)).await;
        }
      }));
    }

    Ok(Self {
      input,
      tasks,
      terminal_mode,
    })
  }

  async fn stop(&mut self) {
    let tasks = std::mem::take(&mut self.tasks);
    for task in &tasks {
      task.abort();
    }
    for task in tasks {
      let _ = task.await;
    }
    let _ = self.input.close().await;
    self.restore_terminal();
  }

  fn restore_terminal(&mut self) {
    if self.terminal_mode {
      let _ = crossterm::terminal::disable_raw_mode();
      self.terminal_mode = false;
    }
  }
}

#[cfg(unix)]
async fn forward_input(input: RawTerminalInput, _terminal_mode: bool) -> io::Result<()> {
  use std::{fs::File, io::Read};

  use nix::fcntl::{fcntl, FcntlArg, OFlag};
  use nix::unistd::dup;
  use tokio::io::unix::AsyncFd;

  struct RestoreStdinFlags(OFlag);
  impl Drop for RestoreStdinFlags {
    fn drop(&mut self) {
      let _ = fcntl(std::io::stdin(), FcntlArg::F_SETFL(self.0));
    }
  }

  // Tokio implements stdin with an uncancellable blocking read. If piped
  // stdin remains open after a raw child exits, awaiting an aborted forwarding
  // task can therefore hang process shutdown. A duplicated nonblocking Unix
  // descriptor remains cancellable for terminals, pipes, and redirected files.
  let stdin = std::io::stdin();
  let flags = OFlag::from_bits_truncate(fcntl(&stdin, FcntlArg::F_GETFL).map_err(io::Error::other)?);
  fcntl(&stdin, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).map_err(io::Error::other)?;
  let _restore_flags = RestoreStdinFlags(flags);
  let mut descriptor = File::from(dup(&stdin).map_err(io::Error::other)?);
  let mut buffer = vec![0; 8192];

  // Regular files and `/dev/null` cannot be registered with epoll on Linux,
  // but a nonblocking read can drain them without a readiness subscription.
  if drain_ready_input(&mut descriptor, &input, &mut buffer).await? {
    return input.close().await;
  }

  let terminal = match AsyncFd::new(descriptor) {
    Ok(terminal) => terminal,
    Err(error) if error.raw_os_error() == Some(nix::libc::EPERM) => {
      let mut descriptor = File::from(dup(&stdin).map_err(io::Error::other)?);
      loop {
        if drain_ready_input(&mut descriptor, &input, &mut buffer).await? {
          return input.close().await;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
      }
    },
    Err(error) => return Err(error),
  };
  loop {
    let mut ready = terminal.readable().await?;
    match ready.try_io(|file| {
      let mut file = file.get_ref();
      file.read(&mut buffer)
    }) {
      Ok(Ok(0)) => break,
      Ok(Ok(count)) => input.write(buffer[..count].to_vec()).await?,
      Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
      Ok(Err(error)) => return Err(error),
      Err(_) => continue,
    }
  }
  input.close().await
}

#[cfg(unix)]
async fn drain_ready_input(file: &mut std::fs::File, input: &RawTerminalInput, buffer: &mut [u8]) -> io::Result<bool> {
  use std::io::Read as _;

  loop {
    match file.read(buffer) {
      Ok(0) => return Ok(true),
      Ok(count) => input.write(buffer[..count].to_vec()).await?,
      Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
      Err(error) if error.kind() == io::ErrorKind::Interrupted => {},
      Err(error) => return Err(error),
    }
  }
}

#[cfg(not(unix))]
async fn forward_input(input: RawTerminalInput, terminal_mode: bool) -> io::Result<()> {
  use tokio::io::AsyncReadExt;

  let mut stdin = tokio::io::stdin();
  let mut buffer = vec![0; 8192];
  loop {
    match stdin.read(&mut buffer).await {
      Ok(0) => break,
      Ok(count) => input.write(buffer[..count].to_vec()).await?,
      Err(error) => return Err(error),
    }
  }
  if terminal_mode {
    input.close().await
  } else {
    Ok(())
  }
}

impl Drop for RawTerminalBridge {
  fn drop(&mut self) {
    for task in &self.tasks {
      task.abort();
    }
    self.restore_terminal();
  }
}

#[async_trait]
impl RawTerminalSession for RawTerminalBridge {
  async fn shutdown(&mut self) {
    self.stop().await;
  }
}
