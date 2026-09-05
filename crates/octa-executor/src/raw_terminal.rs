use std::{io, io::IsTerminal, time::Duration};

use octa_plugin_manager::plugin_client::PluginTerminalInput;
use tokio::task::JoinHandle;

/// Bridges the host terminal to a command-scoped plugin protocol stream.
pub(crate) struct RawTerminalBridge {
  input: PluginTerminalInput,
  tasks: Vec<JoinHandle<()>>,
  terminal_mode: bool,
}

impl RawTerminalBridge {
  pub(crate) async fn start(input: PluginTerminalInput) -> io::Result<Self> {
    let terminal_mode = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if terminal_mode {
      crossterm::terminal::enable_raw_mode()?;
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

  pub(crate) async fn shutdown(&mut self) {
    let tasks = self.tasks.drain(..).collect::<Vec<_>>();
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
async fn forward_input(input: PluginTerminalInput, terminal_mode: bool) -> io::Result<()> {
  use std::{fs::File, io::Read};

  use nix::fcntl::{fcntl, FcntlArg, OFlag};
  use nix::unistd::dup;
  use tokio::io::{unix::AsyncFd, AsyncReadExt};

  struct RestoreStdinFlags(OFlag);
  impl Drop for RestoreStdinFlags {
    fn drop(&mut self) {
      let _ = fcntl(std::io::stdin(), FcntlArg::F_SETFL(self.0));
    }
  }

  if !terminal_mode {
    let mut stdin = tokio::io::stdin();
    let mut buffer = vec![0; 8192];
    loop {
      match stdin.read(&mut buffer).await {
        Ok(0) => break,
        Ok(count) => input.write(buffer[..count].to_vec()).await.map_err(io::Error::from)?,
        Err(error) => return Err(error),
      }
    }
    return input.close().await.map_err(io::Error::from);
  }

  let stdin = std::io::stdin();
  let flags = OFlag::from_bits_truncate(fcntl(&stdin, FcntlArg::F_GETFL).map_err(io::Error::other)?);
  fcntl(&stdin, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).map_err(io::Error::other)?;
  let _restore_flags = RestoreStdinFlags(flags);
  let terminal = AsyncFd::new(File::from(dup(&stdin).map_err(io::Error::other)?))?;
  let mut buffer = vec![0; 8192];
  loop {
    let mut ready = terminal.readable().await?;
    match ready.try_io(|file| {
      let mut file = file.get_ref();
      file.read(&mut buffer)
    }) {
      Ok(Ok(0)) => break,
      Ok(Ok(count)) => input.write(buffer[..count].to_vec()).await.map_err(io::Error::from)?,
      Ok(Err(error)) => return Err(error),
      Err(_) => continue,
    }
  }
  input.close().await.map_err(io::Error::from)
}

#[cfg(not(unix))]
async fn forward_input(input: PluginTerminalInput, _terminal_mode: bool) -> io::Result<()> {
  use tokio::io::AsyncReadExt;

  let mut stdin = tokio::io::stdin();
  let mut buffer = vec![0; 8192];
  loop {
    match stdin.read(&mut buffer).await {
      Ok(0) => break,
      Ok(count) => input.write(buffer[..count].to_vec()).await.map_err(io::Error::from)?,
      Err(error) => return Err(error),
    }
  }
  input.close().await.map_err(io::Error::from)
}

impl Drop for RawTerminalBridge {
  fn drop(&mut self) {
    for task in &self.tasks {
      task.abort();
    }
    self.restore_terminal();
  }
}
