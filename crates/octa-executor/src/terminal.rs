//! Host abstraction for raw/PTY terminal sessions.
//!
//! The executor owns protocol-neutral input and lifecycle traits. Concrete
//! terminal implementations live at the embedding boundary rather than in task
//! execution code.

use std::io;

use async_trait::async_trait;
use octa_plugin_manager::plugin_client::PluginTerminalInput;

/// Host-facing input channel for one raw command.
///
/// The wrapper keeps plugin-manager protocol types out of the embedding API.
#[derive(Clone, Debug)]
pub struct RawTerminalInput {
  inner: PluginTerminalInput,
}

impl RawTerminalInput {
  pub(crate) fn new(inner: PluginTerminalInput) -> Self {
    Self { inner }
  }

  /// Sends input bytes from the host terminal to the active plugin command.
  pub async fn write(&self, bytes: Vec<u8>) -> io::Result<()> {
    self.inner.write(bytes).await.map_err(io::Error::from)
  }

  /// Updates the plugin PTY dimensions.
  pub async fn resize(&self, rows: u16, cols: u16) -> io::Result<()> {
    self.inner.resize(rows, cols).await.map_err(io::Error::from)
  }

  /// Closes the command's input stream without cancelling the command.
  pub async fn close(&self) -> io::Result<()> {
    self.inner.close().await.map_err(io::Error::from)
  }
}

#[async_trait]
/// Host resources retained for the lifetime of one raw command.
pub trait RawTerminalSession: Send {
  /// Stops host-side forwarding and restores any terminal state it changed.
  async fn shutdown(&mut self);
}

#[async_trait]
/// Connects a plugin raw-input channel to an application-owned terminal.
pub trait RawTerminalConnector: Send + Sync {
  /// Starts forwarding host input and resize events to `input`.
  async fn connect(&self, input: RawTerminalInput) -> io::Result<Box<dyn RawTerminalSession>>;
}

/// Connector used by headless hosts that do not provide interactive terminals.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnsupportedRawTerminal;

#[async_trait]
impl RawTerminalConnector for UnsupportedRawTerminal {
  async fn connect(&self, _input: RawTerminalInput) -> io::Result<Box<dyn RawTerminalSession>> {
    Err(io::Error::new(
      io::ErrorKind::Unsupported,
      "raw execution requires a host terminal connector",
    ))
  }
}
