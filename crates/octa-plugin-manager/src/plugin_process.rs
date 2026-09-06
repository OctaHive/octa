use std::{collections::HashMap, ffi::OsString, io, path::PathBuf, process::Stdio};

use async_trait::async_trait;
use thiserror::Error;
use tokio::{
  io::AsyncRead,
  process::{Child, Command},
};

/// Complete host-side request for starting one plugin process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PluginLaunchRequest {
  /// Program to execute. Any script interpreter must already be resolved by the caller.
  pub executable: PathBuf,
  /// Complete program arguments except for the launcher-specific socket transport argument.
  pub arguments: Vec<OsString>,
  /// Environment entries added to the inherited process environment.
  pub environment: HashMap<String, String>,
  /// Working directory assigned to the plugin process.
  pub workspace: PathBuf,
  /// Local socket the plugin must use to connect back to the host.
  pub socket_path: PathBuf,
}

#[derive(Debug, Error)]
pub enum PluginLaunchError {
  #[error("failed to launch plugin process: {0}")]
  Io(#[from] io::Error),
  #[error("launched plugin did not expose its {0} stream")]
  MissingStream(&'static str),
}

pub type PluginOutput = Box<dyn AsyncRead + Send + Unpin>;

/// Running plugin process independent of the mechanism that created it.
pub struct PluginProcess {
  control: Box<dyn PluginProcessControl>,
  stdout: Option<PluginOutput>,
  stderr: Option<PluginOutput>,
}

impl PluginProcess {
  pub fn new(
    control: impl PluginProcessControl + 'static,
    stdout: impl AsyncRead + Send + Unpin + 'static,
    stderr: impl AsyncRead + Send + Unpin + 'static,
  ) -> Self {
    Self {
      control: Box::new(control),
      stdout: Some(Box::new(stdout)),
      stderr: Some(Box::new(stderr)),
    }
  }

  pub fn take_stdout(&mut self) -> Option<PluginOutput> {
    self.stdout.take()
  }

  pub fn take_stderr(&mut self) -> Option<PluginOutput> {
    self.stderr.take()
  }

  pub fn start_kill(&mut self) -> io::Result<()> {
    self.control.start_kill()
  }

  pub async fn wait(&mut self) -> io::Result<()> {
    self.control.wait().await
  }

  pub async fn kill(&mut self) -> io::Result<()> {
    self.control.kill().await
  }
}

/// Lifecycle operations required by plugin supervision.
#[async_trait]
pub trait PluginProcessControl: Send {
  fn start_kill(&mut self) -> io::Result<()>;
  async fn wait(&mut self) -> io::Result<()>;
  async fn kill(&mut self) -> io::Result<()>;
}

#[async_trait]
pub trait PluginLauncher: Send + Sync {
  /// Starts a supervised plugin and connects it to `request.socket_path`.
  ///
  /// Implementations own transport-specific argument construction, stdio capture, and process
  /// termination. `PluginLaunchRequest::executable` and `arguments` already describe the exact
  /// program to run; launchers must not reinterpret the plugin artifact type.
  async fn launch(&self, request: PluginLaunchRequest) -> Result<PluginProcess, PluginLaunchError>;
}

/// Starts plugins directly on the host operating system.
#[derive(Debug, Default)]
pub struct LocalPluginLauncher;

#[async_trait]
impl PluginLauncher for LocalPluginLauncher {
  async fn launch(&self, request: PluginLaunchRequest) -> Result<PluginProcess, PluginLaunchError> {
    let mut command = Command::new(request.executable);
    command
      .args(request.arguments)
      .arg("--socket-path")
      .arg(request.socket_path)
      .envs(request.environment)
      .current_dir(request.workspace)
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .kill_on_drop(true);
    configure_platform(&mut command);

    let mut child = command.spawn()?;
    let stdout = child.stdout.take().ok_or(PluginLaunchError::MissingStream("stdout"))?;
    let stderr = child.stderr.take().ok_or(PluginLaunchError::MissingStream("stderr"))?;
    Ok(PluginProcess::new(LocalProcessControl(child), stdout, stderr))
  }
}

struct LocalProcessControl(Child);

#[async_trait]
impl PluginProcessControl for LocalProcessControl {
  fn start_kill(&mut self) -> io::Result<()> {
    self.0.start_kill()
  }

  async fn wait(&mut self) -> io::Result<()> {
    self.0.wait().await.map(|_| ())
  }

  async fn kill(&mut self) -> io::Result<()> {
    self.0.kill().await
  }
}

#[cfg(not(windows))]
fn configure_platform(command: &mut Command) {
  command.process_group(0);
}

#[cfg(windows)]
fn configure_platform(command: &mut Command) {
  const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
  const CREATE_NO_WINDOW: u32 = 0x0800_0000;
  const DETACHED_PROCESS: u32 = 0x0000_0008;

  command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW | DETACHED_PROCESS);
}
