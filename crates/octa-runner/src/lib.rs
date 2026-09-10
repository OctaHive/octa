//! Versioned JSONL contract for controlling a headless Octa process.
//!
//! Each process accepts one `start` command, optional matching `cancel`, and
//! emits ordered messages associated with the request identifier.

use std::{
  collections::BTreeMap,
  io::{self, BufWriter, Write},
  num::NonZeroUsize,
  path::PathBuf,
  sync::{Arc, Mutex},
};

use octa_executor::ExecutionResult;
use octa_octafile::Silence;
use octa_output::{ConsoleEntry, ConsoleRenderer, EVENT_SCHEMA_VERSION};
use octa_plugin::protocol::PLUGIN_PROTOCOL_VERSION;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};

pub const RUNNER_PROTOCOL_VERSION: u16 = 1;
pub const MAX_RUNNER_INPUT_FRAME_BYTES: usize = 1024 * 1024;
pub const RUNNER_INPUT_SCHEMA_V1: &str = include_str!("../schema/input-v1.schema.json");
pub const RUNNER_OUTPUT_SCHEMA_V1: &str = include_str!("../schema/output-v1.schema.json");

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunnerCommand {
  Start {
    protocol_version: u16,
    request_id: String,
    request: Box<RunRequest>,
  },
  Cancel {
    request_id: String,
  },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
  Succeeded,
  Failed,
  Cancelled,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRequest {
  pub workspace: PathBuf,
  #[serde(default)]
  pub octafile: Option<PathBuf>,
  #[serde(default = "default_data_dir")]
  pub data_dir: PathBuf,
  #[serde(default = "default_plugins_dir")]
  pub plugins_dir: PathBuf,
  #[serde(default)]
  pub plugin_lock: Option<PathBuf>,
  #[serde(default)]
  pub secrets_profile: Option<PathBuf>,
  #[serde(default)]
  pub plugins: Vec<String>,
  #[serde(default)]
  pub default_plugin: Option<String>,
  pub commands: Vec<String>,
  #[serde(default)]
  pub variables: BTreeMap<String, String>,
  #[serde(default)]
  pub arguments: Vec<String>,
  #[serde(default)]
  pub concurrency: Option<NonZeroUsize>,
  #[serde(default)]
  pub parallel: bool,
  #[serde(default)]
  pub failfast: bool,
  #[serde(default)]
  pub dry: bool,
  #[serde(default)]
  pub force: bool,
  #[serde(default)]
  pub quiet: bool,
  #[serde(default)]
  pub silence: Option<Silence>,
}

impl RunRequest {
  /// Checks invariants that are not expressible in the JSON schema.
  pub fn validate(&self) -> Result<(), String> {
    if !self.workspace.is_absolute() {
      return Err("workspace must be an absolute path".to_owned());
    }
    if !self.workspace.is_dir() {
      return Err(format!("workspace '{}' is not a directory", self.workspace.display()));
    }
    if self.commands.is_empty() {
      return Err("commands must contain at least one task".to_owned());
    }
    if self.commands.iter().any(|command| command.is_empty()) {
      return Err("commands must not contain empty task names".to_owned());
    }
    Ok(())
  }
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunnerMessage<'a> {
  Hello {
    protocol_version: u16,
    octa_version: &'static str,
    event_schema_version: u16,
    plugin_protocol_version: u16,
  },
  Capabilities {
    octa_version: &'static str,
    runner_protocols: &'static [u16],
    event_schemas: &'static [u16],
    plugin_protocols: &'static [u16],
    octafile_versions: &'static [u8],
    platform: String,
    features: &'static [&'static str],
    #[serde(skip_serializing_if = "Option::is_none")]
    build_commit: Option<&'static str>,
  },
  Accepted {
    request_id: &'a str,
  },
  Event {
    request_id: &'a str,
    event: &'a ConsoleEntry,
  },
  Finished {
    request_id: &'a str,
    status: RunStatus,
    results: &'a [ExecutionResult],
  },
  Error {
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<&'a str>,
    message: &'a str,
  },
}

pub fn hello() -> RunnerMessage<'static> {
  RunnerMessage::Hello {
    protocol_version: RUNNER_PROTOCOL_VERSION,
    octa_version: env!("CARGO_PKG_VERSION"),
    event_schema_version: EVENT_SCHEMA_VERSION,
    plugin_protocol_version: PLUGIN_PROTOCOL_VERSION,
  }
}

pub fn capabilities() -> RunnerMessage<'static> {
  RunnerMessage::Capabilities {
    octa_version: env!("CARGO_PKG_VERSION"),
    runner_protocols: &[RUNNER_PROTOCOL_VERSION],
    event_schemas: &[EVENT_SCHEMA_VERSION],
    plugin_protocols: &[PLUGIN_PROTOCOL_VERSION],
    octafile_versions: &[1],
    platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
    features: &[
      "artifacts",
      "reports",
      "locked-plugins",
      "secret-providers",
      "vault-secrets",
      "graceful-cancellation",
      "versioned-events",
    ],
    build_commit: option_env!("OCTA_BUILD_COMMIT"),
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
  pub fn write(&self, message: &RunnerMessage<'_>) -> io::Result<()> {
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
    self.output.write(&RunnerMessage::Event {
      request_id: &self.request_id,
      event: entry,
    })
  }
}

fn default_data_dir() -> PathBuf {
  PathBuf::from(".octa")
}

fn default_plugins_dir() -> PathBuf {
  PathBuf::from("plugins")
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn rejects_relative_workspaces_and_empty_commands() {
    let mut request = RunRequest {
      workspace: PathBuf::from("relative"),
      octafile: None,
      data_dir: default_data_dir(),
      plugins_dir: default_plugins_dir(),
      plugin_lock: None,
      secrets_profile: None,
      plugins: Vec::new(),
      default_plugin: None,
      commands: vec!["build".to_owned()],
      variables: BTreeMap::new(),
      arguments: Vec::new(),
      concurrency: None,
      parallel: false,
      failfast: false,
      dry: false,
      force: false,
      quiet: false,
      silence: None,
    };
    assert!(request.validate().unwrap_err().contains("absolute"));

    let file = tempfile::NamedTempFile::new().unwrap();
    request.workspace = file.path().to_path_buf();
    assert!(request.validate().unwrap_err().contains("not a directory"));
    let workspace = tempfile::tempdir().unwrap();
    request.workspace = workspace.path().to_path_buf();
    request.commands.clear();
    assert!(request.validate().unwrap_err().contains("at least one"));
    request.commands.push(String::new());
    assert!(request.validate().unwrap_err().contains("empty task"));
    request.commands[0] = "build".to_owned();
    assert!(request.validate().is_ok());
  }

  #[test]
  fn protocol_rejects_unknown_fields() {
    let command = serde_json::from_str::<RunnerCommand>(r#"{"type":"cancel","request_id":"one","unexpected":true}"#);
    assert!(command.is_err());
  }

  #[tokio::test]
  async fn input_frames_are_bounded() {
    let oversized = vec![b'x'; MAX_RUNNER_INPUT_FRAME_BYTES + 1];
    let mut reader = BufReader::new(oversized.as_slice());
    let mut frame = String::new();

    let error = read_frame(&mut reader, &mut frame).await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
  }

  #[test]
  fn schemas_validate_protocol_examples() {
    let input_schema: serde_json::Value = serde_json::from_str(RUNNER_INPUT_SCHEMA_V1).unwrap();
    let input = json!({
      "type": "start",
      "protocol_version": RUNNER_PROTOCOL_VERSION,
      "request_id": "job-1",
      "request": {
        "workspace": "/workspace",
        "commands": ["ci"]
      }
    });
    assert!(jsonschema::validator_for(&input_schema).unwrap().is_valid(&input));

    let output_schema: serde_json::Value = serde_json::from_str(RUNNER_OUTPUT_SCHEMA_V1).unwrap();
    let validator = jsonschema::validator_for(&output_schema).unwrap();
    assert!(validator.is_valid(&serde_json::to_value(hello()).unwrap()));
    assert!(validator.is_valid(&serde_json::to_value(capabilities()).unwrap()));

    let report_validator = jsonschema::validator_for(&output_schema["$defs"]["report"]).unwrap();
    assert!(report_validator.is_valid(&json!({
      "name": "benchmark",
      "path": "reports/result.json",
      "format": "acme/benchmark-v2"
    })));
    assert!(!report_validator.is_valid(&json!({
      "name": "benchmark",
      "path": "reports/result.json",
      "format": "invalid format"
    })));
  }
}
