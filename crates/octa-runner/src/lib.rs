//! Versioned JSONL contract for controlling a headless Octa process.
//!
//! Each process accepts one `start` command, optional matching `cancel`, and
//! emits ordered messages associated with the request identifier.

use std::{
  io::{self, BufWriter, Write},
  sync::{Arc, Mutex},
};

use octa_output::{ConsoleEntry, ConsoleRenderer};
pub use octa_runner_protocol::{
  RunRequest, RunStatus, RunnerCommand, RunnerMessage, Silence, MAX_RUNNER_INPUT_FRAME_BYTES,
  RUNNER_EVENT_SCHEMA_VERSION, RUNNER_INPUT_SCHEMA_V1, RUNNER_OUTPUT_SCHEMA_V1, RUNNER_PLUGIN_PROTOCOL_VERSION,
  RUNNER_PROTOCOL_VERSION,
};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};

pub fn hello() -> RunnerMessage<String, (), ()> {
  RunnerMessage::Hello {
    protocol_version: RUNNER_PROTOCOL_VERSION,
    octa_version: env!("CARGO_PKG_VERSION").to_owned(),
    event_schema_version: RUNNER_EVENT_SCHEMA_VERSION,
    plugin_protocol_version: RUNNER_PLUGIN_PROTOCOL_VERSION,
  }
}

pub fn capabilities() -> RunnerMessage<String, (), ()> {
  RunnerMessage::Capabilities {
    octa_version: env!("CARGO_PKG_VERSION").to_owned(),
    runner_protocols: vec![RUNNER_PROTOCOL_VERSION],
    event_schemas: vec![RUNNER_EVENT_SCHEMA_VERSION],
    plugin_protocols: vec![RUNNER_PLUGIN_PROTOCOL_VERSION],
    octafile_versions: vec![1],
    platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
    features: [
      "artifacts",
      "reports",
      "locked-plugins",
      "secret-providers",
      "vault-secrets",
      "graceful-cancellation",
      "versioned-events",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect(),
    build_commit: option_env!("OCTA_BUILD_COMMIT").map(str::to_owned),
  }
}

/// Reads one bounded JSONL command without allowing an unterminated frame to
/// grow beyond the runner protocol limit.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut BufReader<R>, frame: &mut String) -> io::Result<usize> {
  frame.clear();
  let mut limited = (&mut *reader).take((MAX_RUNNER_INPUT_FRAME_BYTES + 1) as u64);
  let read = limited.read_line(frame).await?;
  if read == MAX_RUNNER_INPUT_FRAME_BYTES + 1 && !frame.ends_with('\n') {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      format!("runner command exceeds the {MAX_RUNNER_INPUT_FRAME_BYTES}-byte frame limit"),
    ));
  }
  Ok(read)
}

#[derive(Clone)]
/// Serializes complete messages to stdout so concurrent event producers cannot interleave frames.
pub struct MessageWriter {
  writer: Arc<Mutex<BufWriter<io::Stdout>>>,
}

impl Default for MessageWriter {
  fn default() -> Self {
    Self {
      writer: Arc::new(Mutex::new(BufWriter::new(io::stdout()))),
    }
  }
}

impl MessageWriter {
  /// Writes and flushes one JSONL protocol frame.
  pub fn write<I, E, R>(&self, message: &RunnerMessage<I, E, R>) -> io::Result<()>
  where
    I: Serialize,
    E: Serialize,
    R: Serialize,
  {
    let mut writer = self
      .writer
      .lock()
      .map_err(|_| io::Error::other("runner output lock is poisoned"))?;
    serde_json::to_writer(&mut *writer, message).map_err(io::Error::other)?;
    writer.write_all(b"\n")?;
    writer.flush()
  }

  /// Creates a console renderer bound to one request identifier.
  pub fn event_renderer(&self, request_id: String) -> RunnerEventRenderer {
    RunnerEventRenderer {
      request_id,
      output: self.clone(),
    }
  }
}

pub struct RunnerEventRenderer {
  request_id: String,
  output: MessageWriter,
}

impl ConsoleRenderer for RunnerEventRenderer {
  fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
    self
      .output
      .write(&RunnerMessage::event(self.request_id.as_str(), entry))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn input_frames_are_bounded() {
    let oversized = vec![b'x'; MAX_RUNNER_INPUT_FRAME_BYTES + 1];
    let mut reader = BufReader::new(oversized.as_slice());
    let mut frame = String::new();

    let error = read_frame(&mut reader, &mut frame).await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
  }
}
